//! FC-MAS integration tests (T7–T10 semantics + starvation/fairness,
//! backpressure, cancel, shutdown, concurrency stress).
//!
//! Determinism: all ordering assertions use an injected monotonic test clock
//! (`Scheduler::with_clock` with an `Arc<AtomicU64>`); time is advanced
//! explicitly — no sleeps drive ordering. Only blocking-BEHAVIOR assertions
//! (backpressure parked workers, drain deadlines in the stress test) use
//! bounded waits, which cannot race with the ordering being asserted.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering as AOrd};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use fckv::mas::{classify, PoppedReq, Scheduler};
use fckv::types::{Gva, Request};

const MIB: u64 = 1024 * 1024;

// ---------------------------------------------------------------- helpers --

/// Scheduler + controllable monotonic µs clock (starts at `start_us`).
fn clock_sched(start_us: u64) -> (Arc<Scheduler>, Arc<AtomicU64>) {
    let t = Arc::new(AtomicU64::new(start_us));
    let t2 = t.clone();
    let s = Scheduler::with_clock(4096, Box::new(move || t2.load(AOrd::Relaxed)));
    (s, t)
}

fn set_us(t: &Arc<AtomicU64>, us: u64) {
    t.store(us, AOrd::Relaxed);
}

fn req_with(id: u64, arrival_ts_us: u64, size_bytes: u64, blocks: Vec<Gva>) -> Request {
    Request { id, session: 1, arrival_ts_us, blocks, size_bytes }
}

/// All-DRAM request (max GVA < 2^60).
fn dreq(id: u64, arrival_ts_us: u64, size_bytes: u64) -> Request {
    req_with(id, arrival_ts_us, size_bytes, vec![Gva(0), Gva(0xFFFF_FFFF)])
}

/// SSD request (contains a GVA >= 2^60).
fn sreq(id: u64, arrival_ts_us: u64, size_bytes: u64) -> Request {
    req_with(id, arrival_ts_us, size_bytes, vec![Gva(0), Gva(1 << 60)])
}

/// Hybrid request: mixed DRAM + SSD GVAs → SSD-bound (OQ-14).
fn hreq(id: u64, arrival_ts_us: u64, size_bytes: u64) -> Request {
    req_with(id, arrival_ts_us, size_bytes, vec![Gva(5), Gva((1 << 60) | 4096)])
}

// ------------------------------------------------------- T7 classification --

#[test]
fn t7_classify_fig4_dual_queue_routing() {
    // Paper Fig. 4 scenario: R1/R3/R5 SSD-resident, R2/R4/R6 DRAM-resident.
    let r1 = sreq(1, 100, 8 * MIB);
    let r2 = dreq(2, 110, 2 * MIB);
    let r3 = sreq(3, 120, 16 * MIB);
    let r4 = dreq(4, 130, 1 * MIB);
    let r5 = sreq(5, 140, 64 * MIB);
    let r6 = dreq(6, 150, 4 * MIB);
    assert!(classify(&r1) && classify(&r3) && classify(&r5));
    assert!(!(classify(&r2) || classify(&r4) || classify(&r6)));
    // Hybrid (mixed GVAs: DRAM-pinned prefix + SSD history) → SSD (OQ-14).
    assert!(classify(&hreq(7, 160, 32 * MIB)));
    // Single integer comparison at the boundary: GVA == 2^60 is SSD,
    // 2^60 - 1 is DRAM; vacuously all-DRAM when no blocks.
    assert!(!classify(&req_with(11, 0, MIB, vec![Gva((1 << 60) - 1)])));
    assert!(classify(&req_with(12, 0, MIB, vec![Gva(1 << 60)])));
    assert!(classify(&req_with(13, 0, MIB, vec![Gva(0), Gva(1 << 60)])));
    assert!(!classify(&req_with(14, 0, MIB, vec![])));

    // The scheduler routes by the caller's classification flag into the two
    // queues (pure control plane: it obeys the flag; the engine classifies).
    let (s, _t) = clock_sched(0);
    for (r, ssd) in [
        (&r1, true),
        (&r2, false),
        (&r3, true),
        (&r4, false),
        (&r5, true),
        (&r6, false),
        (&hreq(7, 160, 32 * MIB), true),
    ] {
        s.enqueue(r.clone(), ssd);
    }
    let st = s.stats();
    assert_eq!(st.dram_queue_len, 3); // R2/R4/R6
    assert_eq!(st.ssd_queue_len, 4); // R1/R3/R5 + hybrid R7
    assert_eq!(st.wait_samples.len(), 0);
    assert_eq!(st.hrrn_scans, 0);

    // A deliberately mismatched flag is routed as instructed (documented
    // control-plane obedience — classification is the engine's job).
    s.enqueue(dreq(8, 0, MIB), true);
    assert_eq!(s.stats().ssd_queue_len, 5);
}

// ------------------------------------------- T9 arrival-ordered re-admission --

#[test]
fn t9_readmit_arrival_ordered_before_later_dram_hits() {
    let (s, _t) = clock_sched(0);
    s.enqueue(dreq(100, 100, MIB), false); // DRAM hit, arrival ts 100
    s.enqueue(dreq(200, 200, MIB), false); // DRAM hit, arrival ts 200
    s.enqueue(sreq(50, 50, MIB), true); // SSD req, arrival ts 50 (earliest)
    let st = s.stats();
    assert_eq!((st.dram_queue_len, st.ssd_queue_len), (2, 1));

    let p = s.pick_ssd().unwrap(); // FC-CIO prefetch "completes"
    assert_eq!(p.req.id, 50);
    s.readmit(p.req); // re-admitted keyed by ORIGINAL arrival_ts 50 (T9)
    assert_eq!(s.stats().readmitted_count, 1);

    // Fig. 4(b) "Earlier completion": the re-admitted request pops before the
    // newly arrived DRAM hits despite being enqueued last.
    let ids: Vec<u64> = (0..3).filter_map(|_| s.pop_dram()).map(|p| p.req.id).collect();
    assert_eq!(ids, vec![50, 100, 200]);

    let st = s.stats();
    assert_eq!(st.dram_popped, 3);
    assert_eq!(st.ssd_popped, 1);
    assert_eq!(st.hrrn_scans, 1);
    assert_eq!(st.dram_queue_len + st.ssd_queue_len, 0);
    assert_eq!(st.wait_samples.len(), 4); // 3 dram pops + 1 ssd pick
}

#[test]
fn t9_readmit_wait_covers_prefetch_seniority() {
    let (s, t) = clock_sched(0);
    set_us(&t, 1_000); // clock now = 1 ms (arrival_ts_us is trace time, not clock)
    s.enqueue(sreq(1, 500, MIB), true);
    set_us(&t, 2_000);
    let p = s.pick_ssd().unwrap();
    assert_eq!(p.wait_us, 1_000); // SSD-segment wait: 2 ms - 1 ms enqueue
    s.readmit(p.req);
    set_us(&t, 5_000);
    let d = s.pop_dram().unwrap();
    assert_eq!(d.wait_us, 4_000); // total stay incl. prefetch (paper §3.3)
    let st = s.stats();
    assert_eq!(st.wait_samples, vec![1_000, 4_000]);
    assert_eq!(st.max_ssd_wait_us, 1_000);
    assert_eq!(st.max_dram_wait_us, 4_000);
    assert_eq!(st.readmitted_count, 1);
    assert_eq!((st.dram_popped, st.ssd_popped), (1, 1));
}

// --------------------------------------------------------- T8 HRRN Eq. 1 --

#[test]
fn t8_hrrn_small_first_then_giant_aging_overtakes() {
    let (s, t) = clock_sched(0);
    s.enqueue(sreq(1, 0, MIB), true); // 1 MiB restore
    s.enqueue(sreq(2, 0, 100 * MIB), true); // 100 MiB restore, same enqueue time
    set_us(&t, 1_000_000); // +1 s of aging for both
    // Eq. 1: small pri = 1 + 1/1 = 2.0; large pri = 1 + 1/100 = 1.01 → small
    let p = s.pick_ssd().unwrap();
    assert_eq!(p.req.id, 1);
    assert_eq!(p.wait_us, 1_000_000);
    // Aging: at t = 200 s the giant's ratio is 1 + 200/100 = 3.0 — after the
    // small restore is served, the giant is guaranteed dispatch (no starvation).
    set_us(&t, 200_000_000);
    let p = s.pick_ssd().unwrap();
    assert_eq!(p.req.id, 2);
    assert_eq!(p.wait_us, 200_000_000); // waited since its t=0 enqueue
    let st = s.stats();
    assert_eq!(st.ssd_popped, 2);
    assert_eq!(st.hrrn_scans, 2);
    assert_eq!(st.max_ssd_wait_us, 200_000_000);
    assert_eq!(st.wait_samples, vec![1_000_000, 200_000_000]);
}

#[test]
fn t8_hrrn_zero_size_guard_is_one_mib() {
    let (s, t) = clock_sched(0);
    s.enqueue(sreq(7, 0, 0), true); // size 0 → KV_size guarded to 1.0 MiB
    s.enqueue(sreq(8, 0, 10 * MIB), true);
    set_us(&t, 1_000_000);
    // guarded: 1 + 1/1 = 2.0  vs  1 + 1/10 = 1.1 → zero-size behaves like 1 MiB
    let p = s.pick_ssd().unwrap();
    assert_eq!(p.req.id, 7);
    assert_eq!(p.wait_us, 1_000_000);
}

// -------------------------------------------- sequencing: FIFO tie-breaks --

#[test]
fn dram_ready_queue_min_orders_by_arrival_ts() {
    let (s, _t) = clock_sched(0);
    s.enqueue(dreq(1, 300, MIB), false);
    s.enqueue(dreq(2, 100, MIB), false);
    s.enqueue(dreq(3, 200, MIB), false);
    let ids: Vec<u64> = (0..3).map(|_| s.pop_dram().unwrap().req.id).collect();
    assert_eq!(ids, vec![2, 3, 1]); // min-ordered by arrival_ts_us
}

#[test]
fn dram_fifo_tiebreak_on_equal_arrival_ts() {
    let (s, _t) = clock_sched(0);
    for id in 0..5 {
        s.enqueue(dreq(id, 42, MIB), false); // identical arrival_ts → seq FIFO
    }
    let ids: Vec<u64> = (0..5).map(|_| s.pop_dram().unwrap().req.id).collect();
    assert_eq!(ids, vec![0, 1, 2, 3, 4]);
}

// ------------------------------------ starvation / fairness (paper §3.3) --

#[test]
fn starvation_dram_traffic_never_blocks_ssd_queue() {
    let (s, _t) = clock_sched(0);
    s.enqueue(sreq(10, 0, 8 * MIB), true);
    for i in 0..16u64 {
        s.enqueue(dreq(1000 + i, i * 10, MIB), false);
    }
    // Drain wave 1 while another DRAM wave keeps arriving; the SSD pick is
    // available the whole time (dual queues — no head-of-line blocking).
    for i in 0..16u64 {
        let p = s.pop_dram().unwrap();
        assert_eq!(p.req.id, 1000 + i);
        s.enqueue(dreq(2000 + i, 1000 + i * 10, MIB), false);
    }
    let wave2: Vec<u64> = (0..16).map(|_| s.pop_dram().unwrap().req.id).collect();
    assert_eq!(wave2, (2000..2016).collect::<Vec<u64>>());
    let p = s.pick_ssd().unwrap();
    assert_eq!(p.req.id, 10);
    let st = s.stats();
    assert_eq!(st.dram_popped, 32);
    assert_eq!(st.ssd_popped, 1);
    assert_eq!(st.dram_queue_len + st.ssd_queue_len, 0);
    assert_eq!(st.wait_samples.len(), 33);
}

#[test]
fn starvation_giant_ssd_restore_eventually_dispatched_by_aging() {
    let (s, t) = clock_sched(0);
    s.enqueue(sreq(1, 0, MIB), true); // 1 MiB small restore
    s.enqueue(sreq(2, 0, 1024 * MIB), true); // 1 GiB giant restore
    // DRAM traffic interleaved throughout — never touches the SSD queue.
    s.enqueue(dreq(9, 10, 4 * MIB), false);
    set_us(&t, 1_000_000);
    let p = s.pick_ssd().unwrap(); // small pri 2.0 vs giant pri ≈ 1.0009
    assert_eq!(p.req.id, 1);
    assert_eq!(s.pop_dram().unwrap().req.id, 9);
    set_us(&t, 2_000_000);
    s.enqueue(sreq(3, 2_000_000, 16 * MIB), true); // fresh medium: pri 1.0
    // Giant accumulated 2 s of wait: pri = 1 + 2/1024 ≈ 1.00195 > 1.0 —
    // aging lets the senior giant dispatch ahead of the just-arrived medium.
    let p = s.pick_ssd().unwrap();
    assert_eq!(p.req.id, 2);
    assert_eq!(p.wait_us, 2_000_000);
    set_us(&t, 3_000_000);
    let p = s.pick_ssd().unwrap();
    assert_eq!(p.req.id, 3);
    assert_eq!(p.wait_us, 1_000_000);
    let st = s.stats();
    assert_eq!(st.ssd_popped, 3);
    assert_eq!(st.hrrn_scans, 3);
    assert_eq!(st.dram_queue_len, 0);
}

// ------------------------------------------------------------ backpressure --

#[test]
fn backpressure_enqueue_blocks_until_pops_free_capacity() {
    let s = Scheduler::with_clock(4, Box::new(|| 0u64)); // constant test clock
    for id in 0..4 {
        s.enqueue(dreq(id, id, MIB), false); // fill to cap synchronously
    }
    let done = mpsc::channel::<u64>();
    let mut workers = Vec::new();
    for id in 4..6 {
        let s = s.clone();
        let tx = done.0.clone();
        workers.push(std::thread::spawn(move || {
            s.enqueue(dreq(id, id, MIB), false); // blocks: total == cap
            tx.send(id).unwrap();
        }));
    }
    drop(done.0); // only worker sends remain

    // Both workers are parked on a full queue; nothing frees capacity on its
    // own, so no completion can arrive (bounded wait — blocking-behavior
    // assertion, not an ordering assertion).
    assert!(done.1.recv_timeout(Duration::from_millis(200)).is_err());

    // Exact boundary invariant while parked: total queued == cap == 4 exactly,
    // never 5. The depth check + insert are one atomic step under the lock and
    // popped (in-flight) requests do not count toward the bound.
    assert_eq!(s.queue_cap(), 4);
    let st = s.stats();
    assert_eq!(st.dram_queue_len + st.ssd_queue_len, 4);
    assert_eq!(st.dram_queue_len, 4);

    // Pop one → exactly one parked worker unblocks, enqueues and completes.
    // Which of the two wins the freed slot is a genuine race (both wake), so
    // only the fact that SOME worker completes is asserted here.
    let p = s.pop_dram().unwrap();
    assert_eq!(p.req.id, 0);
    let first = done.1.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(first == 4 || first == 5);
    // The woken worker refilled the freed slot: total is back to exactly cap
    // (the other worker stays parked — only one insert fits).
    let st = s.stats();
    assert_eq!(st.dram_queue_len + st.ssd_queue_len, 4);

    // Drain the rest; the other worker unblocks during the drain and all 6
    // requests end up queued + delivered.
    let mut ids = vec![p.req.id];
    while ids.len() < 6 {
        let p = s.pop_dram().expect("all 6 enqueues must complete");
        ids.push(p.req.id);
    }
    for w in workers {
        w.join().unwrap(); // both enqueues completed → senders dropped
    }
    // Collect any completion token still buffered; the loop ends with
    // Disconnected once the channel is empty, so this cannot hang.
    let mut completions = vec![first];
    while let Ok(id) = done.1.recv_timeout(Duration::from_millis(100)) {
        completions.push(id);
    }
    completions.sort_unstable();
    assert_eq!(completions, vec![4, 5], "both blocked enqueues completed");
    ids.sort_unstable();
    assert_eq!(ids, (0..6).collect::<Vec<u64>>());
    let st = s.stats();
    assert_eq!(st.dram_popped, 6);
    assert_eq!((st.dram_queue_len, st.ssd_queue_len), (0, 0));
    assert_eq!(st.wait_samples.len(), 6);
}

// ------------------------------------------------------------------ cancel --

#[test]
fn cancel_removes_from_either_queue_and_awaiting() {
    let (s, _t) = clock_sched(0);
    s.enqueue(dreq(1, 10, MIB), false);
    s.enqueue(dreq(2, 20, MIB), false);
    s.enqueue(sreq(3, 30, MIB), true);
    s.enqueue(sreq(4, 40, MIB), true);
    assert!(s.cancel(2)); // DRAM-Ready
    assert!(!s.cancel(2)); // already gone (idempotent)
    assert!(s.cancel(4)); // SSD-Preparing
    assert!(!s.cancel(999)); // unknown id
    let d = s.pop_dram().unwrap();
    assert_eq!(d.req.id, 1); // cancelled ids never pop
    let p = s.pick_ssd().unwrap();
    assert_eq!(p.req.id, 3);
    assert!(s.cancel(3)); // cancel an in-prefetch request (awaiting table)
    let st = s.stats();
    assert_eq!((st.dram_popped, st.ssd_popped), (1, 1));
    assert_eq!(st.dram_queue_len + st.ssd_queue_len, 0);
    // The cancelled prefetch is never re-admitted; nothing else is pending.
    s.shutdown();
    assert!(s.pop_dram().is_none());
    assert!(s.pick_ssd().is_none());
}

// ---------------------------------------------------------------- shutdown --

#[test]
fn shutdown_wakes_all_waiters_idempotent_and_rejects_new_work() {
    let (s, _t) = clock_sched(0); // queues empty → both poppers park

    let (txd, rxd) = mpsc::channel::<Option<PoppedReq>>();
    let sd = s.clone();
    std::thread::spawn(move || txd.send(sd.pop_dram()).unwrap());
    let (txs, rxs) = mpsc::channel::<Option<PoppedReq>>();
    let ss = s.clone();
    std::thread::spawn(move || txs.send(ss.pick_ssd()).unwrap());

    // Blocked enqueuer on a separate scheduler: cap 1, queue already full.
    let s2 = Scheduler::with_clock(1, Box::new(|| 0u64));
    s2.enqueue(dreq(1, 0, MIB), false);
    let (txe, rxe) = mpsc::channel::<u8>();
    let s2c = s2.clone();
    std::thread::spawn(move || {
        s2c.enqueue(dreq(2, 0, MIB), false); // parks until shutdown
        txe.send(1).unwrap();
    });

    s.shutdown(); // wakes the parked poppers
    s2.shutdown(); // wakes the parked enqueuer (its request is dropped)
    assert!(rxd.recv_timeout(Duration::from_secs(5)).unwrap().is_none());
    assert!(rxs.recv_timeout(Duration::from_secs(5)).unwrap().is_none());
    assert_eq!(rxe.recv_timeout(Duration::from_secs(5)).unwrap(), 1);
    assert_eq!(s2.stats().dram_queue_len, 1); // dropped enqueue never inserted

    // Enqueue after shutdown returns immediately as a documented no-op.
    s.enqueue(dreq(7, 0, MIB), false);
    s.enqueue(sreq(8, 0, MIB), true);
    assert_eq!((s.stats().dram_queue_len, s.stats().ssd_queue_len), (0, 0));
    // Re-admission after shutdown is likewise a no-op.
    s.readmit(dreq(9, 0, MIB));
    assert_eq!((s.stats().dram_queue_len, s.stats().readmitted_count), (0, 0));

    s.shutdown(); // idempotent: second call is a no-op
}

#[test]
fn shutdown_returns_none_even_with_items_queued_drain_first_contract() {
    let (s, _t) = clock_sched(0);
    s.enqueue(dreq(1, 0, MIB), false);
    s.enqueue(dreq(2, 0, MIB), false);
    s.shutdown();
    // Documented contract: poppers return None immediately on shutdown even
    // with items queued — callers must drain BEFORE shutdown.
    assert!(s.pop_dram().is_none());
    assert!(s.pick_ssd().is_none());
    assert_eq!(s.stats().dram_queue_len, 2); // remain visible in stats
    assert_eq!(s.stats().dram_popped, 0);
}

// --------------------------------------------- production constructor smoke --

#[test]
fn new_real_clock_basic_flow_and_stats() {
    let s = Scheduler::new(64); // real monotonic Instant-based clock
    assert_eq!(s.queue_cap(), 64);
    s.enqueue(dreq(1, 0, MIB), false);
    s.enqueue(sreq(2, 0, 2 * MIB), true);
    let d = s.pop_dram().unwrap();
    assert_eq!(d.req.id, 1);
    assert!(d.wait_us <= 1_000_000); // sanity: sub-second wait in this flow
    let p = s.pick_ssd().unwrap();
    assert_eq!(p.req.id, 2);
    s.readmit(p.req);
    let d2 = s.pop_dram().unwrap();
    assert_eq!(d2.req.id, 2);
    let st = s.stats();
    assert_eq!(st.dram_popped, 2);
    assert_eq!(st.ssd_popped, 1);
    assert_eq!(st.readmitted_count, 1);
    assert_eq!(st.hrrn_scans, 1);
    assert_eq!(st.dram_queue_len + st.ssd_queue_len, 0);
    assert_eq!(st.wait_samples, vec![d.wait_us, p.wait_us, d2.wait_us]);
    assert_eq!(st.max_dram_wait_us, d.wait_us.max(d2.wait_us));
    assert_eq!(st.max_ssd_wait_us, p.wait_us);
}

// ------------------------------------------------------- concurrency stress --

#[test]
fn concurrent_stress_no_lost_requests_and_consistent_stats() {
    let s = Scheduler::new(256); // real clock; cap 256 exercises backpressure
    const THREADS: usize = 8;
    const PER: usize = 100;
    const TOTAL: u64 = (THREADS * PER) as u64; // 800

    struct Log {
        ids: HashSet<u64>,
        dram_waits: Vec<u64>,
        ssd_waits: Vec<u64>,
    }
    let log = Arc::new(Mutex::new(Log {
        ids: HashSet::new(),
        dram_waits: Vec::new(),
        ssd_waits: Vec::new(),
    }));

    // 4 popper threads (2 DRAM workers, 2 SSD workers), looping until the
    // shutdown-None exit signal.
    let mut poppers = Vec::new();
    for kind in 0..4u8 {
        let s = s.clone();
        let log = log.clone();
        poppers.push(std::thread::spawn(move || {
            loop {
                let popped =
                    if kind < 2 { s.pop_dram() } else { s.pick_ssd() };
                match popped {
                    Some(p) => {
                        let mut l = log.lock().unwrap();
                        if !l.ids.insert(p.req.id) {
                            panic!("request {} popped twice", p.req.id);
                        }
                        if kind < 2 {
                            l.dram_waits.push(p.wait_us);
                        } else {
                            l.ssd_waits.push(p.wait_us);
                        }
                    }
                    None => break,
                }
            }
        }));
    }

    // 8 enqueuer threads × 100 mixed requests (even id → DRAM, odd → SSD).
    let mut enqueuers = Vec::new();
    for th in 0..THREADS {
        let s = s.clone();
        enqueuers.push(std::thread::spawn(move || {
            for i in 0..PER {
                let id = (th * PER + i) as u64;
                let (req, ssd) = if id % 2 == 0 {
                    (dreq(id, id, MIB + id), false)
                } else {
                    (sreq(id, id, MIB * (1 + id % 7)), true)
                };
                s.enqueue(req, ssd);
            }
        }));
    }
    for h in enqueuers {
        h.join().unwrap();
    }

    // Drain: wait until both queues are empty (all enqueuers are joined, so an
    // empty queue means every request was popped and counted under the lock).
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let st = s.stats();
        if st.dram_queue_len + st.ssd_queue_len == 0 {
            break;
        }
        assert!(Instant::now() < deadline, "drain deadline exceeded");
        std::thread::sleep(Duration::from_millis(1));
    }
    s.shutdown(); // poppers get their None exit signal
    for h in poppers {
        h.join().unwrap();
    }

    let st = s.stats();
    let l = log.lock().unwrap();
    assert_eq!(l.ids.len() as u64, TOTAL, "no lost or duplicated requests");
    for id in 0..TOTAL {
        assert!(l.ids.contains(&id), "missing request id {id}");
    }
    assert_eq!(st.dram_popped + st.ssd_popped, TOTAL);
    assert_eq!(st.dram_popped as usize, l.dram_waits.len());
    assert_eq!(st.ssd_popped as usize, l.ssd_waits.len());
    assert_eq!(st.wait_samples.len() as u64, TOTAL);
    assert_eq!(st.readmitted_count, 0);
    assert_eq!(st.hrrn_scans, st.ssd_popped); // one HRRN scan per successful pick
    assert_eq!(st.max_dram_wait_us, l.dram_waits.iter().max().copied().unwrap_or(0));
    assert_eq!(st.max_ssd_wait_us, l.ssd_waits.iter().max().copied().unwrap_or(0));
    assert_eq!((st.dram_queue_len, st.ssd_queue_len), (0, 0));
}
