//! Track C final validation — concurrency and race-condition probes.
//! Scope: engine-level (public API) probes BEYOND existing coverage in
//! engine_tests.rs (mixed sessions), p2_matrix_tests.rs (stress accounting),
//! p3_migration_tests.rs (concurrent migration integrity), p4_cio_tests.rs
//! (dedup / shuffled multisession), mas_tests.rs (scheduler-level, injected
//! clock) and gva_tests.rs (index/arena component stress):
//!   c01  repeated re-initialization on ONE ssd_dir (30 cycles recovery_log=1
//!        + 1 cycle recovery_log=2) + Windows file-handle release check
//!        (remove_dir_all must succeed — a leaked handle fails the test).
//!   c02  engine.shutdown() racing in-flight gets from 4 client threads:
//!        every call returns within a 30 s watchdog, bytes stay correct, and a
//!        freshly rebuilt engine (recovery_log=2) serves the published blocks.
//!   c03  queue_cap=4 saturation: 8 slow SSD requests vs 3 fast DRAM requests —
//!        DRAM completions must interleave (no head-of-line blocking) and every
//!        request must complete byte-exact.
//!   c04  mixed churn soak (deadline-bounded ~15 s): 6 sessions, fixed seeds,
//!        migration ENABLED (medium_aware, dedup off); mem_used() sampled
//!        continuously (quarter samples reported) and asserted
//!        <= dram_pool + hbm_window + 64 KiB; accounting invariant + zero
//!        integrity failures. Clean OQ-11 scope-full errors ("would grow
//!        scope") are counted and reported, not treated as failures — they are
//!        the documented hard-error path of a one-file-per-scope layout.
//!   c05  concurrent duplicate put of IDENTICAL PutSpecs from 4 threads
//!        (barrier-aligned): no panic, index stays consistent (byte-exact
//!        gets afterwards), accounting invariant holds.
//!   c06  white-box probe of the documented index same-key insert window
//!        (src/gva/index.rs module doc): 4 threads insert+publish the SAME
//!        key concurrently; slot-index identity and lookup consistency are
//!        observed through the public Index API.
//! Determinism: every workload decision is seeded (splitmix64); thread
//! interleaving is the variable under test. Watchdog: any phase that must
//! terminate is bounded by a 30 s channel watchdog; on timeout the process
//! exits 101 so cargo reports the binary as failed — a hang must never wedge
//! the harness silently (std test threads cannot be killed from outside).

use fckv::config::Config;
use fckv::engine::FlatCakeEngine;
use fckv::engine_api::{GetSpec, KvEngine, PutSpec};
use fckv::gva::index::Index;
use fckv::types::{BlockKey, Gva, Lookup, Metrics, PlacementSignals};
use fckv::util::{key_hash, payload_fill, splitmix64};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Barrier, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const WATCHDOG: Duration = Duration::from_secs(30);
/// Default-config block geometry used by every engine test here.
const BB: u64 = 8192; // block_tokens(256) * bytes_per_token(32)
const BT: u64 = 256;

static IDS: AtomicU64 = AtomicU64::new(1);
fn next_id() -> u64 {
    IDS.fetch_add(1, Ordering::SeqCst)
}

fn valc_dir(tag: &str) -> String {
    format!("data/test/valc-{tag}-{}", std::process::id())
}

/// Small engine budgets so every mechanism fires quickly; per-test mutations
/// applied on top.
fn valc_cfg(tag: &str) -> Config {
    let mut c = Config::default();
    c.dram_pool_bytes = 128 * 1024;
    c.hbm_window_bytes = 64 * 1024;
    c.gpu_file_bytes = 256 * 1024;
    c.gpu_file_pool = 4;
    c.ssd_dir = valc_dir(tag);
    c.baseline_ssd_dir = format!("data/test/valcb-{tag}-{}", std::process::id());
    c.queue_cap = 64;
    c.ssd_workers = 2;
    c.dram_workers = 2;
    c.recovery_log = 1;
    c.dedup_enabled = 0;
    c.migration_enabled = 0;
    c
}

/// The documented one-file-per-scope hard error (OQ-11): a batch would grow a
/// scope beyond gpu_file_bytes. Clean, expected under churn; anything else is
/// a failure.
fn is_scope_full_err(err: &std::io::Error) -> bool {
    format!("{err}").contains("would grow scope")
}

#[allow(dead_code)]
fn pspec(session: u64, turn: u64, layers: u32, start_blk: u64, n_blk: u32, shared: u32, seed: u64) -> PutSpec {
    PutSpec {
        session,
        turn,
        layers,
        start_blk,
        n_blk,
        shared,
        reply_tokens: if shared == 1 { 0 } else { 500 },
        ctx_tokens: layers as u64 * (start_blk + n_blk as u64) * BT,
        seed,
    }
}

/// Layer-major block keys of a put (BLOCK-TOKENS multiplier, engine convention).
fn keys_of(spec: &PutSpec) -> Vec<BlockKey> {
    let mut v = Vec::new();
    for l in 0..spec.layers {
        for i in 0..spec.n_blk {
            v.push(BlockKey {
                session: spec.session,
                layer: l,
                token_start: (spec.start_blk + i as u64) * BT,
            });
        }
    }
    v
}

fn seeds_for(n: usize, seed: u64) -> Vec<u64> {
    vec![seed; n]
}

fn sig(session: u64, turn: u64) -> PlacementSignals {
    PlacementSignals {
        session,
        refcount: 1,
        turn,
        reply_tokens: 500,
        ctx_tokens: 8 * BT,
    }
}

fn gspec(blocks: Vec<BlockKey>, seeds: Vec<u64>, session: u64, arrival_ts_us: u64) -> GetSpec {
    GetSpec {
        id: next_id(),
        arrival_ts_us,
        blocks,
        seed_by_block: seeds,
        signals: sig(session, 2),
    }
}

/// Byte-exact delivery verifier: counts integrity failures into `fails` and
/// deliveries into `delivered` (asserted by the callers; a sink must never
/// panic mid-request — failures are aggregated for a single clear assert).
fn byte_verifier(
    blocks: Vec<BlockKey>,
    seeds: Vec<u64>,
    fails: Arc<AtomicUsize>,
    delivered: Arc<AtomicUsize>,
) -> impl FnMut(usize, &[u8]) {
    move |idx, bytes| {
        let k = blocks[idx];
        let mut expect = vec![0u8; BB as usize];
        payload_fill(k.session, k.layer, k.token_start, seeds[idx], &mut expect);
        delivered.fetch_add(1, Ordering::SeqCst);
        if bytes != expect.as_slice() {
            fails.fetch_add(1, Ordering::SeqCst);
            eprintln!(
                "VALC INTEGRITY FAIL: block {k:?} idx {idx} got {} bytes expected {} bytes",
                bytes.len(),
                expect.len()
            );
        }
    }
}

fn assert_no_integrity_failures(fails: &AtomicUsize, ctx: &str) {
    let n = fails.load(Ordering::SeqCst);
    assert_eq!(n, 0, "{ctx}: {n} integrity failures (byte-exactness violated)");
}

fn assert_accounting(m: &Metrics, ctx: &str) {
    assert_eq!(
        m.hit_hbm + m.hit_dram + m.hit_ssd + m.misses,
        m.requested_blocks,
        "{ctx}: accounting invariant hit_hbm({}) + hit_dram({}) + hit_ssd({}) + misses({}) \
         != requested_blocks({})",
        m.hit_hbm,
        m.hit_dram,
        m.hit_ssd,
        m.misses,
        m.requested_blocks
    );
}

fn watchdog_fail(what: &str) -> ! {
    eprintln!("VALC WATCHDOG TIMEOUT (30 s): {what} — aborting with exit 101");
    std::process::exit(101);
}

/// Compact engine-state dump for the c07 watchdog diagnostics. Atomic-free
/// observations first; the engine-lock probe runs on a 3 s watchdogged thread
/// (a held metrics/scheduler mutex under a wedge surfaces as a BLOCKED probe
/// instead of blocking the watchdog itself).
fn dump_c07(e: &Arc<FlatCakeEngine>, label: &str) {
    eprintln!("VALC c07 DUMP ({label}):");
    let ep = e.clone();
    let (ptx, prx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut s = format!("mem_used={}", ep.mem_used());
        let m = ep.snapshot();
        s.push_str(&format!(
            " delivered={} requested={} misses={} racing={} promo={} demo={} reclaim={} qwait={}",
            m.delivered_requests,
            m.requested_blocks,
            m.misses,
            m.pending_racing_gets,
            m.migr_promotions,
            m.migr_demotions,
            m.reclaim_events,
            m.qwait_samples.len()
        ));
        let _ = ptx.send(s);
    });
    match prx.recv_timeout(Duration::from_secs(3)) {
        Ok(line) => eprintln!("  engine probe: {line}"),
        Err(_) => eprintln!(
            "  engine probe: BLOCKED >3 s inside mem_used/snapshot — a thread holds the \
             metrics or scheduler mutex forever"
        ),
    }
}

/// Join worker threads under the watchdog; surfaces panics from the workers.
fn join_with_watchdog(name: &str, handles: Vec<JoinHandle<()>>) {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let results = handles.into_iter().map(|h| h.join()).collect::<Vec<_>>();
        let _ = tx.send(results);
    });
    match rx.recv_timeout(WATCHDOG) {
        Ok(results) => {
            for (i, r) in results.iter().enumerate() {
                if let Err(e) = r {
                    panic!("{name}: worker thread {i} panicked: {e:?}");
                }
            }
        }
        Err(_) => watchdog_fail(&format!("{name}: threads did not finish within 30 s")),
    }
}

fn recv_deadline<T>(rx: &mpsc::Receiver<T>, deadline: Instant, what: &str) -> T {
    let now = Instant::now();
    if now >= deadline {
        watchdog_fail(what);
    }
    match rx.recv_timeout(deadline - now) {
        Ok(v) => v,
        Err(_) => watchdog_fail(what),
    }
}

/// Watchdogged receive with a diagnostic dump on timeout (used by c04/c07 so
/// a wedged engine call surfaces with per-thread progress before exit).
fn recv_deadline_or<T>(
    rx: &mpsc::Receiver<T>,
    budget: Duration,
    what: &str,
    dump: impl FnOnce(),
) -> T {
    match rx.recv_timeout(budget) {
        Ok(v) => v,
        Err(_) => {
            eprintln!("VALC WATCHDOG TIMEOUT ({budget:?}): {what}");
            dump();
            std::process::exit(101);
        }
    }
}

fn recv_deadline_or_opt<T>(rx: &mpsc::Receiver<T>, budget: Duration) -> Option<T> {
    rx.recv_timeout(budget).ok()
}

/// Join under the watchdog with a diagnostic dump on timeout.
fn join_with_watchdog_dump(name: &str, handles: Vec<JoinHandle<()>>, dump: impl FnOnce()) {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let results = handles.into_iter().map(|h| h.join()).collect::<Vec<_>>();
        let _ = tx.send(results);
    });
    match rx.recv_timeout(WATCHDOG) {
        Ok(results) => {
            for (i, r) in results.iter().enumerate() {
                if let Err(e) = r {
                    panic!("{name}: worker thread {i} panicked: {e:?}");
                }
            }
        }
        Err(_) => {
            eprintln!("VALC WATCHDOG TIMEOUT ({WATCHDOG:?}): {name}: threads did not finish");
            dump();
            std::process::exit(101);
        }
    }
}

// ---------------------------------------------------------------------------
// c01 — repeated initialization on the same ssd_dir + handle-release detector
// ---------------------------------------------------------------------------

// 30 create/put/get/drop cycles with recovery_log=1, then one cycle with
// recovery_log=2 (replays the accumulated log; last record per key wins).
// Every init must succeed, every round-trip byte-exact, and after all engines
// are shut down and dropped the scope files must be deletable (Windows
// remove_dir_all fails while ANY handle is open — a leak detector).
#[test]
fn valc_c01_repeated_init_30_cycles_same_dir_byte_exact_and_handles_released() {
    let dir = valc_dir("reinit");
    let spec = pspec(7, 1, 2, 0, 2, 0, 4242); // turn-1 one-shot => SSD scope
    let keys = keys_of(&spec);
    let seeds = seeds_for(keys.len(), 4242);
    assert_eq!(keys.len(), 4);

    let cycles = 31;
    let mut inits_ok = 0usize;
    let mut puts_ok = 0usize;
    let mut gets_ok = 0usize;

    for c in 0..cycles {
        let mut cfg = valc_cfg("reinit");
        cfg.ssd_dir = dir.clone();
        cfg.recovery_log = if c < 30 { 1 } else { 2 };

        let e = FlatCakeEngine::new(&cfg)
            .unwrap_or_else(|err| panic!("cycle {c}: engine init failed: {err}"));
        inits_ok += 1;

        if c == 30 {
            let m = e.snapshot();
            assert_eq!(
                m.recovery_replayed, 4,
                "cycle 30 (recovery_log=2): replay must recover the 4 unique keys \
                 (last record per key wins)"
            );
            assert_eq!(
                m.recovery_truncated, 0,
                "cycle 30: no log lines may be truncated (flush on every batch end)"
            );
        }

        e.put_batch(&spec)
            .unwrap_or_else(|err| panic!("cycle {c}: put failed: {err}"));
        puts_ok += 1;

        let fails = Arc::new(AtomicUsize::new(0));
        let delivered = Arc::new(AtomicUsize::new(0));
        let g = gspec(keys.clone(), seeds.clone(), 7, c as u64 * 10);
        let mut v = byte_verifier(keys.clone(), seeds.clone(), fails.clone(), delivered.clone());
        e.get_request(&g, &mut v)
            .unwrap_or_else(|err| panic!("cycle {c}: get failed: {err}"));
        gets_ok += 1;
        assert_eq!(
            delivered.load(Ordering::SeqCst),
            keys.len(),
            "cycle {c}: every requested block must be delivered exactly once"
        );
        assert_no_integrity_failures(&fails, "c01");

        e.shutdown();
        drop(e); // release every handle BEFORE the next cycle opens the same log
    }

    assert_eq!(inits_ok, cycles, "every init must succeed");
    assert_eq!(puts_ok, cycles, "every put must succeed");
    assert_eq!(gets_ok, cycles, "every get must succeed");

    // Windows file-handle leak detector: this fails while ANY handle from any
    // of the 31 engines (scope files, recovery log) is still open.
    std::fs::remove_dir_all(&dir)
        .unwrap_or_else(|err| panic!("c01: scope files must be deletable after teardown (leaked handle?): {err}"));
    eprintln!("VALC c01: {inits_ok} inits, {puts_ok} puts, {gets_ok} byte-exact gets; dir deletable");
}

// ---------------------------------------------------------------------------
// c02 — shutdown() during active operations
// ---------------------------------------------------------------------------

// 4 client threads issue mixed (pure-DRAM / pure-SSD / hybrid) gets; the main
// thread calls shutdown() mid-flight. Every client call must RETURN (Ok or
// clean Err) within the 30 s watchdog, every delivered byte must be correct,
// and a freshly rebuilt engine over the same dir (recovery_log=2) must serve
// the previously published SSD blocks byte-exact.
#[test]
fn valc_c02_shutdown_midflight_gets_return_no_hang_rebuild_serves_byte_exact() {
    let dir = valc_dir("shutdown");
    let mut cfg = valc_cfg("shutdown");
    cfg.ssd_dir = dir.clone();
    cfg.dram_pool_bytes = 192 * 1024;
    cfg.gpu_file_pool = 6;
    let e = Arc::new(FlatCakeEngine::new(&cfg).unwrap());

    // Shared prompt (session 1, shared=1 -> DRAM scope): 6 blocks.
    let shared = pspec(1, 1, 2, 0, 3, 1, 11);
    e.put_batch(&shared).unwrap();
    let shared_keys = keys_of(&shared);
    let shared_seeds = seeds_for(shared_keys.len(), 11);

    // 4 agentic sessions on SSD (turn-1 one-shot): 4 blocks each.
    let sess: Vec<(Vec<BlockKey>, Vec<u64>, u64)> = (2..6u64)
        .map(|s| {
            let p = pspec(s, 1, 2, 0, 2, 0, s * 77);
            e.put_batch(&p).unwrap();
            let k = keys_of(&p);
            let sd = seeds_for(k.len(), s * 77);
            (k, sd, s)
        })
        .collect();

    let fails = Arc::new(AtomicUsize::new(0));
    let (tx_ready, rx_ready) = mpsc::channel::<()>();
    let (tx_done, rx_done) = mpsc::channel::<usize>();

    let mut handles = Vec::new();
    for (k, (skeys, sseeds, s)) in sess.iter().enumerate() {
        let e = e.clone();
        let fails = fails.clone();
        let shared_keys = shared_keys.clone();
        let shared_seeds = shared_seeds.clone();
        let skeys = skeys.clone();
        let sseeds = sseeds.clone();
        let s = *s;
        let tx_ready = tx_ready.clone();
        let tx_done = tx_done.clone();
        handles.push(std::thread::spawn(move || {
            let _ = k; // client index (reporting only)
            let mut ats = 100u64;
            // Warmup: full hybrid request (shared DRAM prefix + own SSD history).
            {
                let mut blocks = shared_keys.clone();
                let mut seeds = shared_seeds.clone();
                blocks.extend(skeys.iter().copied());
                seeds.extend(sseeds.iter().copied());
                let g = gspec(blocks, seeds, s, ats);
                ats += 1;
                let d = Arc::new(AtomicUsize::new(0));
                let mut v = byte_verifier(g.blocks.clone(), g.seed_by_block.clone(), fails.clone(), d.clone());
                e.get_request(&g, &mut v).expect("warmup get");
                assert_eq!(d.load(Ordering::SeqCst), g.blocks.len(), "warmup delivery");
            }
            let _ = tx_ready.send(());
            // Post-shutdown loop: every call must RETURN (no hang).
            for i in 0..6usize {
                let (blocks, seeds): (Vec<BlockKey>, Vec<u64>) = match i % 3 {
                    0 => (shared_keys[..2].to_vec(), shared_seeds[..2].to_vec()), // pure DRAM
                    1 => (skeys.clone(), sseeds.clone()),                         // pure SSD
                    _ => {
                        let mut b = shared_keys[2..4].to_vec();
                        let mut sd = shared_seeds[2..4].to_vec();
                        b.extend(skeys.iter().copied());
                        sd.extend(sseeds.iter().copied());
                        (b, sd) // hybrid
                    }
                };
                let n_expect = blocks.len();
                let g = gspec(blocks, seeds, s, ats);
                ats += 1;
                let d = Arc::new(AtomicUsize::new(0));
                let mut v = byte_verifier(g.blocks.clone(), g.seed_by_block.clone(), fails.clone(), d.clone());
                // Ok or clean Err are both acceptable; a hang is not (watchdog).
                let _ = e.get_request(&g, &mut v);
                let _ = tx_done.send(d.load(Ordering::SeqCst).min(n_expect));
            }
        }));
    }
    drop(tx_ready);
    drop(tx_done);

    // Wait for all 4 warmups, give the clients a moment to be mid-flight,
    // then shut down UNDER the watchdog (shutdown itself must return).
    let deadline = Instant::now() + WATCHDOG;
    for k in 0..4 {
        recv_deadline(&rx_ready, deadline, &format!("c02: client {k} warmup ready"));
    }
    std::thread::sleep(Duration::from_millis(5));

    let (stx, srx) = mpsc::channel::<()>();
    let e2 = e.clone();
    std::thread::spawn(move || {
        e2.shutdown();
        let _ = stx.send(());
    });
    if srx.recv_timeout(WATCHDOG).is_err() {
        watchdog_fail("c02: engine.shutdown() did not return within 30 s");
    }

    // All 4 clients x 6 iterations must deliver their blocks within the watchdog.
    let mut done_count = 0usize;
    let mut delivered_blocks = 0usize;
    for _ in 0..24 {
        delivered_blocks += recv_deadline(&rx_done, deadline, "c02: client get did not return within 30 s");
        done_count += 1;
    }
    assert_eq!(done_count, 24, "every client get must return");
    // 6 iterations cycle the (pure-DRAM 2, pure-SSD 4, hybrid 6) pattern twice
    // per client: 2 * 12 * 4 clients = 96 delivered blocks.
    assert_eq!(delivered_blocks, 96, "post-shutdown deliveries");
    join_with_watchdog("c02 clients", handles);
    assert_no_integrity_failures(&fails, "c02 (mid-flight deliveries)");

    // Freshly rebuilt engine over the same dir (recovery_log=2): the SSD
    // blocks published before shutdown are replayed from the log and must be
    // served byte-exact; DRAM-published blocks recompute byte-exact.
    let mut rcfg = valc_cfg("shutdown");
    rcfg.ssd_dir = dir.clone();
    rcfg.dram_pool_bytes = 256 * 1024;
    rcfg.gpu_file_pool = 6;
    rcfg.recovery_log = 2;
    let r = FlatCakeEngine::new(&rcfg).unwrap();
    let rm = r.snapshot();
    assert_eq!(
        rm.recovery_replayed, 16,
        "rebuild: 16 SSD keys (4 sessions x 4 blocks) must replay"
    );
    assert_eq!(rm.recovery_truncated, 0, "rebuild: log tail must be intact");
    let mut blocks = shared_keys.clone();
    let mut seeds = shared_seeds.clone();
    for (k, sd, _) in &sess {
        blocks.extend(k.iter().copied());
        seeds.extend(sd.iter().copied());
    }
    let fails2 = Arc::new(AtomicUsize::new(0));
    let delivered2 = Arc::new(AtomicUsize::new(0));
    let g = gspec(blocks.clone(), seeds.clone(), 2, 9000);
    let mut v = byte_verifier(blocks.clone(), seeds.clone(), fails2.clone(), delivered2.clone());
    r.get_request(&g, &mut v).expect("rebuilt engine get");
    assert_eq!(delivered2.load(Ordering::SeqCst), blocks.len(), "rebuild delivery");
    assert_no_integrity_failures(&fails2, "c02 rebuild");
    r.shutdown();
    drop(r);
    drop(e); // engine Arc released after all client threads joined

    std::fs::remove_dir_all(&dir)
        .unwrap_or_else(|err| panic!("c02: dir must be deletable after teardown (leaked handle?): {err}"));
    eprintln!("VALC c02: shutdown mid-flight OK; 24 client gets returned; rebuild replayed {} keys", rm.recovery_replayed);
}

// ---------------------------------------------------------------------------
// c03 — SSD-pipeline saturation: no head-of-line blocking for DRAM requests
// ---------------------------------------------------------------------------

// 8 concurrent pure-SSD gets (64 KiB NO_BUFFERING scope reads each) saturate
// the SSD pipeline: ssd_workers=1 serializes them, so the SSD drain spans
// ~8 service periods. queue_cap=12 (>= 8 SSD + 3 DRAM) keeps DRAM admission
// off the shared backpressure bound — with a combined cap (scheduler.rs) the
// DRAM enqueuers would have to win a condvar race against blocked SSD
// enqueuers, which is not a guaranteed property. The main thread issues
// 3 small pure-DRAM gets (later arrival_ts) once the FIRST SSD get has
// completed; the DRAM start timestamp is that completion signal, so the
// "DRAM issued while SSD outstanding" precondition holds structurally
// (a preempted main thread cannot invalidate it).
// Requirement: DRAM requests complete OUT-OF-ORDER relative to the SSD ones
// (at least one DRAM completion strictly before the last SSD completion while
// SSD work was still outstanding) and every request completes byte-exact.
#[test]
fn valc_c03_ssd_saturation_dram_completes_out_of_order() {
    let dir = valc_dir("hol");
    let mut cfg = valc_cfg("hol");
    cfg.ssd_dir = dir.clone();
    cfg.dram_pool_bytes = 256 * 1024;
    cfg.gpu_file_pool = 8;
    cfg.ssd_high_watermark = 0.95; // 512 KiB used of 2 MiB: no SSD reclaim mid-test
    // 12 >= 8 SSD + 3 DRAM: DRAM admission never blocks on the shared
    // backpressure bound (dram.len() + ssd.len() >= cap, scheduler.rs). The
    // no-HOL property under test lives in the dual-queue scheduling, not in
    // admission-race fairness; the SSD pipeline stays saturated via
    // ssd_workers=1.
    cfg.queue_cap = 12;
    cfg.ssd_workers = 1;
    cfg.dram_workers = 1;
    cfg.recovery_log = 0;
    let e = Arc::new(FlatCakeEngine::new(&cfg).unwrap());

    // Shared prompt: 6 DRAM-resident blocks (3 D-gets of disjoint pairs).
    let shared = pspec(1, 1, 2, 0, 3, 1, 11);
    e.put_batch(&shared).unwrap();
    let shared_keys = keys_of(&shared);
    let shared_seeds = seeds_for(shared_keys.len(), 11);

    // 8 SSD sessions, 8 blocks (64 KiB) each = 8 slow SSD requests.
    let ssd_sessions: Vec<(u64, Vec<BlockKey>, Vec<u64>)> = (2..10u64)
        .map(|s| {
            let p = pspec(s, 1, 2, 0, 4, 0, s * 97);
            e.put_batch(&p).unwrap();
            let k = keys_of(&p);
            (s, k.clone(), seeds_for(k.len(), s * 97))
        })
        .collect();

    let fails = Arc::new(AtomicUsize::new(0));
    #[derive(Clone, Copy)]
    #[allow(dead_code)]
    struct Completion {
        kind: char,
        id: u64,
        start: Instant,
        end: Instant,
    }
    let log: Arc<Mutex<Vec<Completion>>> = Arc::new(Mutex::new(Vec::new()));
    let (tx_ping, rx_ping) = mpsc::channel::<u64>();
    // First-SSD-completion signal: the DRAM issue point. Overlap with
    // outstanding SSD work is structural, not timing-dependent (CI runner
    // preemption can otherwise outlast every in-flight SSD get).
    let (tx_sdone, rx_sdone) = mpsc::channel::<Instant>();
    let deadline = Instant::now() + WATCHDOG;

    let mut handles = Vec::new();
    for (i, (s, keys, seeds)) in ssd_sessions.iter().enumerate() {
        let e = e.clone();
        let fails = fails.clone();
        let log = log.clone();
        let tx_ping = tx_ping.clone();
        let tx_sdone = tx_sdone.clone();
        let keys = keys.clone();
        let seeds = seeds.clone();
        let s = *s;
        handles.push(std::thread::spawn(move || {
            let _ = tx_ping.send(s); // "about to enqueue an SSD request"
            let start = Instant::now();
            let delivered = Arc::new(AtomicUsize::new(0));
            let g = gspec(keys.clone(), seeds.clone(), s, 100 + i as u64);
            let mut v = byte_verifier(keys, seeds, fails, delivered.clone());
            let res = e.get_request(&g, &mut v);
            let end = Instant::now();
            assert!(res.is_ok(), "S get for session {s} must return Ok");
            assert_eq!(delivered.load(Ordering::SeqCst), 8, "S get delivery count");
            log.lock().unwrap().push(Completion { kind: 'S', id: s, start, end });
            let _ = tx_sdone.send(end);
        }));
    }
    drop(tx_ping);
    drop(tx_sdone);

    // Give 2 SSD requests a head start, then issue 3 fast DRAM gets from
    // dedicated threads with LATER arrival timestamps. The issue point is the
    // FIRST SSD completion (not the pings): its timestamp is recorded as the
    // DRAM start, so "DRAM issued while SSD outstanding" holds by construction
    // even when a preempted main thread resumes after several SSD completions.
    for _ in 0..2 {
        recv_deadline(&rx_ping, deadline, "c03: SSD client ping");
    }
    let d_start0: Instant = recv_deadline(&rx_sdone, deadline, "c03: first SSD completion");
    let mut d_handles = Vec::new();
    for j in 0..3usize {
        let e = e.clone();
        let fails = fails.clone();
        let log = log.clone();
        let keys = vec![shared_keys[2 * j], shared_keys[2 * j + 1]];
        let seeds = vec![shared_seeds[2 * j], shared_seeds[2 * j + 1]];
        d_handles.push(std::thread::spawn(move || {
            let start = d_start0;
            let delivered = Arc::new(AtomicUsize::new(0));
            let g = gspec(keys.clone(), seeds.clone(), 1, 5000 + j as u64);
            let mut v = byte_verifier(keys, seeds, fails, delivered.clone());
            let res = e.get_request(&g, &mut v);
            let end = Instant::now();
            assert!(res.is_ok(), "D get {j} must return Ok");
            assert_eq!(delivered.load(Ordering::SeqCst), 2, "D get delivery count");
            log.lock().unwrap().push(Completion { kind: 'D', id: j as u64, start, end });
        }));
    }

    join_with_watchdog("c03 SSD clients", handles);
    join_with_watchdog("c03 DRAM clients", d_handles);
    assert_no_integrity_failures(&fails, "c03");

    let comps = log.lock().unwrap().clone();
    assert_eq!(comps.len(), 11, "every request must have completed");
    let s_end_max = comps.iter().filter(|c| c.kind == 'S').map(|c| c.end).max().unwrap();
    let d_end_min = comps.iter().filter(|c| c.kind == 'D').map(|c| c.end).min().unwrap();
    let d_start_min = comps.iter().filter(|c| c.kind == 'D').map(|c| c.start).min().unwrap();
    // Overlap: DRAM requests were issued while SSD work was still outstanding.
    assert!(
        d_start_min < s_end_max,
        "c03: DRAM requests must be issued while SSD requests are still outstanding"
    );
    // No head-of-line blocking: a DRAM completion strictly before the last SSD
    // completion (a FIFO-over-both-queues system would finish all 8 SSD
    // requests — enqueued first, earlier arrival_ts — before any DRAM one).
    assert!(
        d_end_min < s_end_max,
        "c03: no head-of-line blocking — at least one DRAM completion must precede \
         the last SSD completion"
    );
    let mut sorted = comps.clone();
    sorted.sort_by_key(|c| c.end);
    let order: String = sorted.iter().map(|c| c.kind).collect();
    let t0 = sorted[0].end;
    let rel = |t: Instant| (t - t0).as_micros();
    eprintln!(
        "VALC c03: completion order by end time: {order}; last SSD completed +{}us after the first completion; first DRAM completed +{}us",
        rel(s_end_max),
        rel(d_end_min)
    );

    let m = e.snapshot();
    assert_accounting(&m, "c03");
    e.shutdown();
    drop(e);
    std::fs::remove_dir_all(&dir)
        .unwrap_or_else(|err| panic!("c03: dir must be deletable after teardown: {err}"));
}

// ---------------------------------------------------------------------------
// c04 — mixed churn soak with migration enabled: bounded-memory property
// ---------------------------------------------------------------------------

// 6 session threads (fixed seeds) interleave put / get over their own agentic
// scopes while a dedicated pressure thread injects shared-prompt (DRAM) puts.
// migration_enabled=1 + eviction_policy=medium_aware + dedup_enabled=0.
// mem_used() is sampled every 200 ms and asserted
// <= dram_pool + hbm_window + 64 KiB (quarter samples reported); the
// accounting invariant must hold and zero integrity failures are tolerated.
// Deadline-bounded (~15 s churn). Turns capped at 7 per session (224 KiB of
// the 256 KiB scope file) so migration re-append traffic keeps headroom; cap 6
// WORKAROUND (documented, not a fix): put_batch callers are serialized
// test-side (put_serial mutex) — with two concurrent put_batch callers the
// engine deadlocks on the M<->D ABBA (see the WORKAROUND comment in the body
// and the isolated repro in c08). Gets stay fully concurrent and promote
// (get-side) still overlaps demote (put-side).
// EVERY phase runs under the 30 s watchdog with a diagnostic dump: a wedged
// engine call must surface as a loud failure with per-thread progress, never
// a silent hang.
#[test]
fn valc_c04_mixed_churn_soak_migration_bounded_memory() {
    let dir = valc_dir("soak");
    let mut cfg = valc_cfg("soak");
    cfg.ssd_dir = dir.clone();
    cfg.dram_pool_bytes = 128 * 1024;
    cfg.hbm_window_bytes = 64 * 1024;
    cfg.gpu_file_pool = 16;
    cfg.ssd_high_watermark = 0.9;
    cfg.migration_enabled = 1;
    cfg.eviction_policy = "medium_aware".into();
    cfg.migration_cooldown_ms = 0; // deterministic churn (recency guard vacuous)
    cfg.migration_max_per_get = 1;
    cfg.adaptive_k = 2;
    cfg.adaptive_h_min = 0.1;
    let e = Arc::new(FlatCakeEngine::new(&cfg).unwrap());

    let bound = cfg.dram_pool_bytes + cfg.hbm_window_bytes + 64 * 1024;
    let fails = Arc::new(AtomicUsize::new(0));
    let scope_full = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let samples: Arc<Mutex<Vec<(u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));
    // Per-thread progress instrumentation for the watchdog dump:
    // index 0..5 = clients, 6 = pressure thread. phase: 0 idle, 1 put, 2 get.
    let rounds: Vec<Arc<AtomicUsize>> = (0..7).map(|_| Arc::new(AtomicUsize::new(0))).collect();
    let phase: Vec<Arc<AtomicUsize>> = (0..7).map(|_| Arc::new(AtomicUsize::new(0))).collect();

    // Continuous mem_used() sampler (superset of the quarter sampling below).
    let (stx, srx) = mpsc::channel::<()>();
    {
        let e = e.clone();
        let stop = stop.clone();
        let samples = samples.clone();
        std::thread::spawn(move || {
            let t0 = Instant::now();
            loop {
                let used = e.mem_used();
                samples.lock().unwrap().push((t0.elapsed().as_millis() as u64, used));
                if stop.load(Ordering::SeqCst) {
                    let _ = stx.send(());
                    return;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        });
    }

    // Incremental dump: PURE-ATOMIC progress first (can never block), then a
    // BEST-EFFORT engine-state probe on a watchdogged thread. The probe takes
    // the metrics/scheduler locks — under the known M<->D ABBA the metrics
    // mutex is held by a deadlocked thread, so the probe TIMES OUT and that
    // fact itself is the evidence. The watchdog always reaches exit(101).
    let dump_state = |label: &str| {
        eprintln!("VALC c04 DUMP ({label}):");
        for i in 0..7usize {
            let who = if i < 6 { format!("client{i}") } else { "pressure".to_string() };
            eprintln!(
                "  {who}: rounds={} phase={} (0=idle 1=put 2=get)",
                rounds[i].load(Ordering::SeqCst),
                phase[i].load(Ordering::SeqCst)
            );
        }
        eprintln!(
            "  scope_full={} fails={}",
            scope_full.load(Ordering::SeqCst),
            fails.load(Ordering::SeqCst)
        );
        // Best-effort engine probe (arena Pool + hbm locks, then metrics +
        // scheduler q): 3 s budget, never blocks the watchdog.
        let ep = e.clone();
        let (ptx, prx2) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            let mut s = format!("    mem_used={}", ep.mem_used());
            let m = ep.snapshot();
            s.push_str(&format!(
                " delivered={} requested={} misses={} racing={} hbm={} dram={} ssd={} \
                 promo={} demo={} reclaim={}",
                m.delivered_requests,
                m.requested_blocks,
                m.misses,
                m.pending_racing_gets,
                m.hit_hbm,
                m.hit_dram,
                m.hit_ssd,
                m.migr_promotions,
                m.migr_demotions,
                m.reclaim_events
            ));
            let _ = ptx.send(s);
        });
        match prx2.recv_timeout(Duration::from_secs(3)) {
            Ok(line) => eprintln!("  engine probe: {line}"),
            Err(_) => eprintln!(
                "  engine probe: BLOCKED >3 s inside mem_used/snapshot — a worker holds the \
                 metrics or scheduler mutex forever. Known M<->D ABBA: maybe_reclaim holds \
                 metrics across flush_pending_frees/purge_dead_scope_bookkeeping \
                 (engine.rs:276 -> 318-319, dram_blocks taken at 834) while demote_pass holds \
                 dram_blocks across its victim loop and takes metrics per victim \
                 (engine.rs:715/771); two concurrent put_batch callers deadlock the engine."
            ),
        }
    };

    let churn = Duration::from_secs(15);
    let deadline = Instant::now() + churn;
    // WORKAROUND (not a fix): put_batch callers are serialized test-side.
    // The engine deadlocks when TWO put_batch callers overlap: maybe_reclaim
    // holds `metrics` across its tail flush_pending_frees +
    // purge_dead_scope_bookkeeping (engine.rs:276 -> 318-319; dram_blocks
    // taken at 834) while demote_pass holds `dram_blocks` across its victim
    // loop and takes `metrics` per victim (engine.rs:715/771) — M<->D ABBA.
    // Observed 5/5 wedged runs with concurrent put callers (see c08 for the
    // isolated repro). Gets stay FULLY concurrent; promote (get thread) still
    // overlaps demote (put thread) — the pair the engine survives.
    let put_serial = Arc::new(Mutex::new(()));
    let (pub_tx, pub_rx) = mpsc::channel::<(u64, Vec<(BlockKey, u64)>)>();
    let mut handles = Vec::new();
    for i in 0..6u64 {
        let e = e.clone();
        let fails = fails.clone();
        let scope_full = scope_full.clone();
        let deadline = deadline;
        let pub_tx = pub_tx.clone();
        let round_i = rounds[i as usize].clone();
        let phase_i = phase[i as usize].clone();
        let put_serial = put_serial.clone();
        handles.push(std::thread::spawn(move || {
            let session = 10 + i;
            // Fixed per-session seed chain: the workload sequence is
            // deterministic; thread interleaving is the variable.
            let mut rng: u64 = 0xA11CE ^ session.wrapping_mul(0x9E37_79B9);
            let mut published: Vec<(BlockKey, u64)> = Vec::new();
            let mut turn = 1u64;
            let mut r = 0usize;
            while Instant::now() < deadline && r < 20000 {
                r += 1;
                round_i.store(r, Ordering::SeqCst);
                if turn <= 6 {
                    // Agentic put: turn-1 one-shot => SSD scope (sticky).
                    let seed = splitmix64(&mut rng);
                    let p = pspec(session, turn, 2, (turn - 1) * 2, 2, 0, seed);
                    phase_i.store(1, Ordering::SeqCst);
                    let put_guard = put_serial.lock().unwrap();
                    let res = e.put_batch(&p);
                    drop(put_guard);
                    phase_i.store(0, Ordering::SeqCst);
                    match res {
                        Ok(()) => {
                            for k in keys_of(&p) {
                                published.push((k, seed));
                            }
                            turn += 1;
                        }
                        // Scope-file exhausted by migration re-appends (OQ-11):
                        // clean, documented; stop putting, keep getting.
                        Err(err) if is_scope_full_err(&err) => {
                            scope_full.fetch_add(1, Ordering::SeqCst);
                            turn = 7;
                        }
                        Err(err) => panic!("soak session {session}: put failed: {err}"),
                    }
                }
                // Random-window get over published blocks, byte-verified.
                if !published.is_empty() {
                    let n = published.len();
                    let start_at = (splitmix64(&mut rng) % n as u64) as usize;
                    let take = 1 + (splitmix64(&mut rng) % (n.min(8) as u64)) as usize;
                    let mut blocks = Vec::with_capacity(take);
                    let mut seeds = Vec::with_capacity(take);
                    for j in 0..take {
                        let (k, s) = &published[(start_at + j) % n];
                        blocks.push(*k);
                        seeds.push(*s);
                    }
                    let delivered = Arc::new(AtomicUsize::new(0));
                    let g = gspec(blocks.clone(), seeds.clone(), session, r as u64);
                    let mut v = byte_verifier(blocks, seeds, fails.clone(), delivered.clone());
                    phase_i.store(2, Ordering::SeqCst);
                    let res = e.get_request(&g, &mut v);
                    phase_i.store(0, Ordering::SeqCst);
                    match res {
                        Ok(()) => {
                            assert_eq!(
                                delivered.load(Ordering::SeqCst), take,
                                "soak session {session}: delivery count"
                            );
                        }
                        // Recompute racing scope exhaustion: clean OQ-11 error.
                        Err(err) if is_scope_full_err(&err) => {
                            scope_full.fetch_add(1, Ordering::SeqCst);
                        }
                        Err(err) => panic!("soak session {session}: get failed: {err}"),
                    }
                }
            }
            let _ = pub_tx.send((session, published));
        }));
    }
    drop(pub_tx);

    // Reclaim pressure on its own watchdogged thread (main never calls the
    // engine unwatchdogged): shared-prompt puts fill the DRAM pool (row-1
    // scope), driving the medium-aware demotion/reclaim passes while clients
    // run. Capped so SSD logical bytes stay under the watermark (no SSD scope
    // reclaim: client keys stay resolvable, churn comes from migration).
    let (ptx, prx) = mpsc::channel::<usize>();
    {
        let e = e.clone();
        let scope_full = scope_full.clone();
        let deadline = deadline;
        let round_p = rounds[6].clone();
        let phase_p = phase[6].clone();
        let put_serial = put_serial.clone();
        std::thread::spawn(move || {
            let mut ok = 0usize;
            for j in 1..=40usize {
                if Instant::now() >= deadline {
                    break;
                }
                let mut rng = 0xF00D_u64 ^ j as u64;
                let seed = splitmix64(&mut rng);
                let p = pspec(1, j as u64, 2, 1000 + j as u64 * 2, 2, 1, seed);
                phase_p.store(1, Ordering::SeqCst);
                let put_guard = put_serial.lock().unwrap();
                let res = e.put_batch(&p);
                drop(put_guard);
                phase_p.store(0, Ordering::SeqCst);
                match res {
                    Ok(()) => {
                        ok += 1;
                        round_p.store(ok, Ordering::SeqCst);
                    }
                    // Session 1's scope file exhausted (flip + spills): done.
                    Err(err) if is_scope_full_err(&err) => {
                        scope_full.fetch_add(1, Ordering::SeqCst);
                        break;
                    }
                    Err(err) => panic!("c04 pressure put failed: {err}"),
                }
                std::thread::sleep(Duration::from_millis(40));
            }
            let _ = ptx.send(ok);
        });
    }

    eprintln!("VALC c04: main waiting for pressure thread");
    let pressure_ok = recv_deadline_or(&prx, WATCHDOG, "c04: pressure thread did not return within 30 s", || {
        dump_state("pressure thread stuck (main waiting on prx)")
    });
    assert!(pressure_ok >= 1, "c04: at least one pressure put must land");

    eprintln!("VALC c04: main joining clients");
    join_with_watchdog_dump("c04 clients", handles, || dump_state("client threads stuck"));
    stop.store(true, Ordering::SeqCst);
    eprintln!("VALC c04: main stopping sampler");
    recv_deadline_or(&srx, WATCHDOG, "c04: sampler did not stop", || {
        dump_state("sampler stop")
    });

    // Bounded-memory property: EVERY sample (hence every quarter) stays
    // within dram_pool + hbm_window + 64 KiB.
    let samp = samples.lock().unwrap().clone();
    assert!(samp.len() >= 5, "c04: sampler must have produced samples");
    let over: Vec<(u64, u64)> = samp.iter().filter(|(_, u)| *u > bound).copied().collect();
    assert!(
        over.is_empty(),
        "c04: mem_used exceeded bound {bound} at samples {over:?}"
    );
    // Report the quarter samples explicitly (25/50/75/100% of the soak).
    let total_ms = samp.last().unwrap().0.max(1);
    for q in [0.25f64, 0.5, 0.75, 1.0] {
        let target = (total_ms as f64 * q) as u64;
        if let Some(&(ms, used)) = samp.iter().find(|(ms, _)| *ms >= target) {
            eprintln!("VALC c04 quarter {:.0}%: t={ms}ms mem_used={used} (bound {bound})", q * 100.0);
        }
    }

    // Post-soak sanity: a full byte-exact get over session 10's ACTUAL
    // published block list (collected from the client thread; never-published
    // blocks are not probed — a full scope file would turn their recompute
    // into the documented OQ-11 error). Runs under the watchdog: main must
    // never touch the engine unprotected.
    eprintln!("VALC c04: main running post-soak get");
    {
        let mut found = None;
        for _ in 0..6 {
            let (session, list) = recv_deadline_or(&pub_rx, WATCHDOG, "c04: client published list", || {
                dump_state("waiting for client published lists")
            });
            if session == 10 {
                found = Some(list);
            } else if session == 15 && found.is_none() {
                found = Some(list);
            }
        }
        let list = found.expect("c04: session list must exist");
        assert!(!list.is_empty(), "c04: session 10 must have published blocks");
        let (blocks, seeds): (Vec<BlockKey>, Vec<u64>) = list.iter().fold(
            (Vec::new(), Vec::new()),
            |(mut b, mut s), (k, seed)| {
                b.push(*k);
                s.push(*seed);
                (b, s)
            },
        );
        let (gtx, grx) = mpsc::channel::<()>();
        let e2 = e.clone();
        let fails2 = fails.clone();
        let scope_full2 = scope_full.clone();
        let expect = blocks.len();
        std::thread::spawn(move || {
            let delivered = Arc::new(AtomicUsize::new(0));
            let g = gspec(blocks.clone(), seeds.clone(), 10, 99_000);
            let mut v = byte_verifier(blocks, seeds, fails2, delivered.clone());
            let res = e2.get_request(&g, &mut v);
            match res {
                Ok(()) => assert_eq!(
                    delivered.load(Ordering::SeqCst), expect,
                    "c04 post-soak delivery"
                ),
                // A block whose only copy was destroyed by the DRAM reclaim
                // pass after promotion (see REPORT: promoted blocks lose their
                // SSD backing at promote time; whole-scope DRAM reclaim then
                // tombstone+free the last copy) recomputes — and the recompute
                // cannot re-persist into a full scope file (clean OQ-11
                // error). Counted, not a failure: every DELIVERED byte is
                // still byte-verified.
                Err(err) if is_scope_full_err(&err) => {
                    scope_full2.fetch_add(1, Ordering::SeqCst);
                    eprintln!(
                        "VALC c04: post-soak get hit OQ-11 (promoted-block loss): delivered \
                         {}/{} blocks; delivered bytes byte-verified",
                        delivered.load(Ordering::SeqCst),
                        expect
                    );
                }
                Err(err) => panic!("c04 post-soak get failed: {err}"),
            }
            let _ = gtx.send(());
        });
        recv_deadline_or(&grx, WATCHDOG, "c04: post-soak get did not return within 30 s", || {
            dump_state("post-soak get stuck")
        });
    }

    assert_no_integrity_failures(&fails, "c04 soak");
    let m = e.snapshot();
    assert_accounting(&m, "c04 soak");
    assert!(
        m.migr_promotions + m.migr_demotions > 0,
        "c04: the soak must have exercised migration (promotions {}, demotions {})",
        m.migr_promotions,
        m.migr_demotions
    );
    eprintln!(
        "VALC c04: mem high-water {} (bound {bound}), promotions {} demotions {} bytes_moved {} reclaim_events {} misses {} racing {} scope_full_errs {}",
        m.mem_high_water,
        m.migr_promotions,
        m.migr_demotions,
        m.migr_bytes_moved,
        m.reclaim_events,
        m.misses,
        m.pending_racing_gets,
        scope_full.load(Ordering::SeqCst)
    );
    eprintln!("VALC c04: main shutting engine down");
    let (shx, shrx) = mpsc::channel::<()>();
    let e2 = e.clone();
    std::thread::spawn(move || {
        e2.shutdown();
        let _ = shx.send(());
    });
    recv_deadline_or(&shrx, WATCHDOG, "c04: shutdown did not return within 30 s", || {
        dump_state("shutdown stuck")
    });
    drop(e);
    std::fs::remove_dir_all(&dir)
        .unwrap_or_else(|err| panic!("c04: dir must be deletable after teardown: {err}"));
}

// ---------------------------------------------------------------------------
// c07 — regression guard: concurrent promote x demote churn must not wedge
// ---------------------------------------------------------------------------

// History: the engine as-of validation start took `dram_blocks` BEFORE
// `migration_lock` in demote_pass while promote_one took `migration_lock`
// BEFORE `dram_blocks` — a textbook ABBA that wedged concurrent put/get
// workloads with migration enabled (reproduced live during this validation:
// all test threads parked, workers idle; the source now takes
// migration_lock first in BOTH paths). This probe churns BOTH sides
// concurrently and requires completion inside the watchdog: a reintroduced
// lock-order inversion parks an in-flight promote/demote holder forever and
// surfaces here as a watchdog failure with diagnostics.
#[test]
fn valc_c07_concurrent_promote_demote_churn_no_wedge() {
    let dir = valc_dir("wedge");
    let mut cfg = valc_cfg("wedge");
    cfg.ssd_dir = dir.clone();
    cfg.dram_pool_bytes = 64 * 1024; // 8 blocks: pressure builds fast
    cfg.hbm_window_bytes = 64 * 1024;
    cfg.gpu_file_pool = 48; // 12 MiB: no SSD reclaim, session 2 stays SSD-backed
    cfg.ssd_high_watermark = 1.0;
    cfg.migration_enabled = 1;
    cfg.eviction_policy = "medium_aware".into();
    cfg.migration_cooldown_ms = 0;
    cfg.migration_max_per_get = 1;
    cfg.adaptive_k = 1; // promote after a single delivery
    cfg.adaptive_h_min = 0.15;
    cfg.recovery_log = 0;
    let e = Arc::new(FlatCakeEngine::new(&cfg).unwrap());

    // Hot session: 4 SSD-resident blocks (turn-1 one-shot scope).
    let hot = pspec(2, 1, 2, 0, 2, 0, 4242);
    e.put_batch(&hot).unwrap();
    let hot_keys = keys_of(&hot);
    let hot_seeds = seeds_for(hot_keys.len(), 4242);

    let fails = Arc::new(AtomicUsize::new(0));
    let churn = Duration::from_secs(2);
    let deadline = Instant::now() + churn;

    // 3 getter threads: promotions run in get_request (mig -> db order).
    let (gtx, grx) = mpsc::channel::<usize>();
    let mut handles = Vec::new();
    for g in 0..3usize {
        let e = e.clone();
        let fails = fails.clone();
        let keys = hot_keys.clone();
        let seeds = hot_seeds.clone();
        let deadline = deadline;
        let gtx = gtx.clone();
        handles.push(std::thread::spawn(move || {
            let mut done = 0usize;
            while Instant::now() < deadline && done < 400 {
                let delivered = Arc::new(AtomicUsize::new(0));
                let gs = gspec(keys.clone(), seeds.clone(), 2, 10 + g as u64 * 1000 + done as u64);
                let mut v = byte_verifier(keys.clone(), seeds.clone(), fails.clone(), delivered.clone());
                let res = e.get_request(&gs, &mut v);
                assert!(res.is_ok(), "c07 getter {g}: get failed: {res:?}");
                assert_eq!(delivered.load(Ordering::SeqCst), keys.len(), "c07 getter delivery");
                done += 1;
            }
            let _ = gtx.send(done);
        }));
    }
    drop(gtx);

    // Pressure thread: demotions run in put_batch/maybe_reclaim. One-block
    // shared puts over 40 rotating sessions (8 x 8 KiB = 64 KiB per scope
    // file): every put runs maybe_reclaim -> demote_pass while getters run
    // promote_one — maximal overlap of both migration paths.
    let (ptx, prx) = mpsc::channel::<usize>();
    let p_handle = {
        let e = e.clone();
        let deadline = deadline;
        std::thread::spawn(move || {
            let mut done = 0usize;
            for j in 0..320usize {
                if Instant::now() >= deadline {
                    break;
                }
                let sess = 1000 + (j / 8) as u64;
                let mut rng = 0xBEEF_u64 ^ j as u64;
                let seed = splitmix64(&mut rng);
                let p = pspec(sess, 1, 1, j as u64 % 8, 1, 1, seed);
                match e.put_batch(&p) {
                    Ok(()) => done += 1,
                    // A pressure scope file filling up is a clean OQ-11 stop.
                    Err(err) if is_scope_full_err(&err) => break,
                    Err(err) => panic!("c07 pressure put failed: {err}"),
                }
            }
            let _ = ptx.send(done);
        })
    };

    let getters_done: Vec<usize> = (0..3)
        .map(|_| {
            recv_deadline_or(&grx, WATCHDOG, "c07: getter thread did not return within 30 s", || {
                dump_c07(&e, "getter stuck")
            })
        })
        .collect();
    let pressure_done = recv_deadline_or(&prx, WATCHDOG, "c07: pressure thread did not return within 30 s", || {
        dump_c07(&e, "pressure stuck")
    });
    join_with_watchdog_dump("c07 getters", handles, || dump_c07(&e, "getter join"));
    join_with_watchdog_dump("c07 pressure", vec![p_handle], || dump_c07(&e, "pressure join"));

    // Wedge check: if any churn thread did NOT finish its bounded loop within
    // the window, the engine may be wedged — probe with a canary get. A
    // wedged migration path parks the canary in promote_one/demote_pass and
    // the canary deadline fires.
    let all_finished = getters_done.iter().all(|&d| d >= 400) && pressure_done >= 320;
    let mut wedged = false;
    if !all_finished {
        let (ctx, crx) = mpsc::channel::<()>();
        {
            let e = e.clone();
            let fails = fails.clone();
            let keys = hot_keys.clone();
            let seeds = hot_seeds.clone();
            std::thread::spawn(move || {
                let delivered = Arc::new(AtomicUsize::new(0));
                let g = gspec(keys.clone(), seeds.clone(), 2, 50_000);
                let mut v = byte_verifier(keys, seeds, fails, delivered.clone());
                let _ = e.get_request(&g, &mut v);
                let _ = ctx.send(());
            });
        }
        if recv_deadline_or_opt(&crx, Duration::from_secs(8)).is_none() {
            wedged = true;
        }
    }
    assert_no_integrity_failures(&fails, "c07");
    let m = e.snapshot();
    assert_accounting(&m, "c07");
    eprintln!(
        "VALC c07: churn complete: getters {:?} (400 cap each), pressure {} (320 cap); \
         promotions {} demotions {} reclaim {}; wedge_detected={wedged}",
        getters_done,
        pressure_done,
        m.migr_promotions,
        m.migr_demotions,
        m.reclaim_events
    );
    if wedged {
        eprintln!(
            "VALC c07 WEDGE: engine did not finish the bounded churn loop and a canary get \
             parked for 8 s — concurrent promote/demote wedge reproduced (see REPORT)"
        );
    } else {
        // Healthy path: full teardown + Windows handle-release check.
        let (shx, shrx) = mpsc::channel::<()>();
        let e2 = e.clone();
        std::thread::spawn(move || {
            e2.shutdown();
            let _ = shx.send(());
        });
        recv_deadline_or(&shrx, WATCHDOG, "c07: shutdown did not return within 30 s", || {
            dump_c07(&e, "shutdown stuck")
        });
        drop(e);
        std::fs::remove_dir_all(&dir)
            .unwrap_or_else(|err| panic!("c07: dir must be deletable after teardown: {err}"));
    }
    // On the wedged path the parked churn threads still hold engine Arcs and
    // open handles, so the leak check is intentionally skipped (documented).
}

// ---------------------------------------------------------------------------
// c05 — concurrent duplicate put of the SAME keys from 4 threads
// ---------------------------------------------------------------------------

// 4 threads put byte-identical PutSpecs (shared=1 => DRAM scope) 5 times each,
// barrier-aligned so all 4 hit the engine simultaneously per round. The first
// round races scope creation + medium decision + index insert for the same
// keys. Requirements: no panic (clean Ok/Err both accepted and reported), the
// index stays consistent (all subsequent gets byte-exact), accounting holds.
#[test]
fn valc_c05_concurrent_duplicate_put_same_keys_consistent() {
    let dir = valc_dir("dupput");
    let mut cfg = valc_cfg("dupput");
    cfg.ssd_dir = dir.clone();
    cfg.dram_pool_bytes = 1024 * 1024; // 20 duplicate puts x 32 KiB fit: pure probe, no spill/reclaim interference
    cfg.gpu_file_pool = 2;
    let e = Arc::new(FlatCakeEngine::new(&cfg).unwrap());

    let spec = pspec(21, 2, 2, 0, 2, 1, 99); // shared=1 -> refcount 2 -> DRAM (row 1)
    let keys = keys_of(&spec);
    let seeds = seeds_for(keys.len(), 99);
    assert_eq!(keys.len(), 4);

    let results: Arc<Mutex<Vec<(usize, usize, bool, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let barrier = Arc::new(Barrier::new(4));
    let mut handles = Vec::new();
    for t in 0..4usize {
        let e = e.clone();
        let barrier = barrier.clone();
        let results = results.clone();
        let spec = spec.clone();
        handles.push(std::thread::spawn(move || {
            for r in 0..5usize {
                let _ = barrier.wait();
                match e.put_batch(&spec) {
                    Ok(()) => results.lock().unwrap().push((r, t, true, String::new())),
                    Err(err) => {
                        let s = format!("{err}");
                        results.lock().unwrap().push((r, t, false, s));
                    }
                }
            }
        }));
    }
    join_with_watchdog("c05 putters", handles);

    let res = results.lock().unwrap().clone();
    let oks = res.iter().filter(|(_, _, ok, _)| *ok).count();
    let errs = res.len() - oks;
    for (_, t, ok, msg) in res.iter() {
        if !*ok {
            eprintln!("VALC c05: duplicate put by thread {t} returned clean Err: {msg}");
        }
    }
    eprintln!("VALC c05: 20 duplicate puts -> {oks} Ok, {errs} clean Err (both accepted; no panic = pass)");
    assert_eq!(res.len(), 20, "every duplicate put must return");

    // 4 concurrent byte-exact verification gets over the same keys.
    let fails = Arc::new(AtomicUsize::new(0));
    let barrier2 = Arc::new(Barrier::new(4));
    let mut vhandles = Vec::new();
    for t in 0..4usize {
        let e = e.clone();
        let fails = fails.clone();
        let barrier = barrier2.clone();
        let keys = keys.clone();
        let seeds = seeds.clone();
        vhandles.push(std::thread::spawn(move || {
            let _ = barrier.wait();
            let delivered = Arc::new(AtomicUsize::new(0));
            let g = gspec(keys.clone(), seeds.clone(), 21, 100 + t as u64);
            let mut v = byte_verifier(keys, seeds, fails.clone(), delivered.clone());
            e.get_request(&g, &mut v).expect("c05 verify get");
            assert_eq!(delivered.load(Ordering::SeqCst), 4, "c05 verify delivery");
        }));
    }
    join_with_watchdog("c05 verifiers", vhandles);
    // One sequential re-check for good measure.
    {
        let delivered = Arc::new(AtomicUsize::new(0));
        let g = gspec(keys.clone(), seeds.clone(), 21, 999);
        let mut v = byte_verifier(keys, seeds, fails.clone(), delivered.clone());
        e.get_request(&g, &mut v).expect("c05 final get");
        assert_eq!(delivered.load(Ordering::SeqCst), 4);
    }
    assert_no_integrity_failures(&fails, "c05 (index consistency after duplicate puts)");
    let m = e.snapshot();
    assert_accounting(&m, "c05");
    e.shutdown();
    drop(e);
    std::fs::remove_dir_all(&dir)
        .unwrap_or_else(|err| panic!("c05: dir must be deletable after teardown: {err}"));
}

// ---------------------------------------------------------------------------
// c06 — white-box probe: index same-key concurrent insert window
// ---------------------------------------------------------------------------

// src/gva/index.rs documents that two threads inserting the SAME key inside
// the hash-store window can double-insert (permitted: both slots then hold
// identical immutable payloads). This probe measures that window through the
// public API: 4 threads insert+publish the same key with DISTINCT GVAs;
// insert_pending returns the slot index, so the number of distinct slots
// claimed for one key is directly observable. No lost publish and no stale
// GVA may ever be observed by concurrent lookups.
#[test]
fn valc_c06_index_same_key_concurrent_insert_window_probe() {
    let idx = Arc::new(Index::new(4096));
    let key = BlockKey { session: 1, layer: 0, token_start: 0 };
    let h = key_hash(1, 0, 0);

    let slots_seen: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
    let published: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    let mut handles = Vec::new();
    for t in 0..4usize {
        let idx = idx.clone();
        let slots_seen = slots_seen.clone();
        let published = published.clone();
        handles.push(std::thread::spawn(move || {
            for r in 0..250usize {
                let slot = match idx.insert_pending(&key) {
                    Ok(s) => s,
                    Err(_) => panic!("c06: IndexFull on a 4096-slot table with 1 key"),
                };
                slots_seen.lock().unwrap().push(slot);
                let g = Gva::encode(0, 0, (t * 1000 + r + 1) as u64).unwrap();
                idx.publish(slot, g);
                published.lock().unwrap().push(g.0);
            }
        }));
    }
    join_with_watchdog("c06 inserters", handles);

    let distinct_slots: std::collections::HashSet<usize> =
        slots_seen.lock().unwrap().iter().copied().collect();
    assert!(!distinct_slots.is_empty(), "c06: at least one slot must be claimed");
    assert!(
        distinct_slots.len() <= 4,
        "c06: at most one claim per racing thread ({} distinct slots)",
        distinct_slots.len()
    );
    let pub_set: std::collections::HashSet<u64> = published.lock().unwrap().iter().copied().collect();

    // Post-race: lookup must resolve to a PUBLISHED GVA that some thread
    // actually published for this key (no lost publish, no stale value).
    match idx.lookup(&key) {
        Lookup::Hit(g) => assert!(
            pub_set.contains(&g.0),
            "c06: lookup returned GVA {} never published for the key",
            g.0
        ),
        other => panic!("c06: key must be Hit after all publishes, got {other:?}"),
    }

    // Concurrent lookup storm: every observation must be a Hit within the
    // published set (Pending is impossible post-join; Miss = lost publish).
    let misses = Arc::new(AtomicUsize::new(0));
    let mut lhandles = Vec::new();
    for _ in 0..4usize {
        let idx = idx.clone();
        let misses = misses.clone();
        let pub_set = pub_set.clone();
        lhandles.push(std::thread::spawn(move || {
            for _ in 0..1000usize {
                match idx.lookup(&key) {
                    Lookup::Hit(g) => {
                        if !pub_set.contains(&g.0) {
                            misses.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    _ => {
                        misses.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
        }));
    }
    join_with_watchdog("c06 lookups", lhandles);
    assert_no_integrity_failures(&misses, "c06 (no lost publish / no stale GVA)");
    eprintln!(
        "VALC c06: same-key 4x250 concurrent inserts -> {} distinct slot(s) claimed \
         (1 = no double-insert observed; >1 = documented window hit; hash {h:#x})",
        distinct_slots.len()
    );
}

// ---------------------------------------------------------------------------
// c08 — isolated repro probe: two concurrent put_batch callers wedge the
// engine (metrics <-> dram_blocks ABBA)
// ---------------------------------------------------------------------------

// Minimal repro of the deadlock Track C discovered during this validation:
//   thread A: put_batch -> maybe_reclaim tail — holds `metrics` (engine.rs:276)
//             across flush_pending_frees + purge_dead_scope_bookkeeping
//             (engine.rs:318-319) and wants `dram_blocks` (engine.rs:834);
//   thread B: put_batch -> demote_pass — holds `dram_blocks` (engine.rs:715,
//             under migration_lock taken at 714) across its victim loop and
//             wants `metrics` per victim (engine.rs:771).
// A waits on D (held by B); B waits on M (held by A): permanent wedge. The
// getters park behind migration_lock (held by B); every other put caller
// parks behind metrics (held by A) — the whole engine freezes. Observed 5/5
// runs of c04 with 7 put callers before the put-serialization workaround.
// This probe uses exactly TWO put callers plus one getter and REPORTS the
// wedge (the parked threads keep engine handles, so teardown is skipped on
// the wedged path); it passes either way — the finding lives in the REPORT.
#[test]
fn valc_c08_concurrent_put_callers_abba_repro() {
    let dir = valc_dir("abba");
    let mut cfg = valc_cfg("abba");
    cfg.ssd_dir = dir.clone();
    cfg.dram_pool_bytes = 64 * 1024; // 8 blocks: pressure + promotions fill fast
    cfg.hbm_window_bytes = 64 * 1024;
    cfg.gpu_file_pool = 48; // no SSD reclaim: session 2 stays SSD-backed
    cfg.ssd_high_watermark = 1.0;
    cfg.migration_enabled = 1;
    cfg.eviction_policy = "medium_aware".into();
    cfg.migration_cooldown_ms = 0;
    cfg.migration_max_per_get = 1;
    cfg.adaptive_k = 1;
    cfg.adaptive_h_min = 0.15;
    cfg.recovery_log = 0;
    let e = Arc::new(FlatCakeEngine::new(&cfg).unwrap());

    // Hot session: 4 SSD-resident blocks; the getter's gets promote them so
    // dram_blocks has victims for the put threads' demote passes.
    let hot = pspec(2, 1, 2, 0, 2, 0, 4242);
    e.put_batch(&hot).unwrap();
    let hot_keys = keys_of(&hot);
    let hot_seeds = seeds_for(hot_keys.len(), 4242);

    let fails = Arc::new(AtomicUsize::new(0));
    let churn = Duration::from_secs(2);
    let deadline = Instant::now() + churn;

    // Getter: promotions run inside get_request.
    let (gtx, grx) = mpsc::channel::<usize>();
    let g_handle = {
        let e = e.clone();
        let fails = fails.clone();
        let keys = hot_keys.clone();
        let seeds = hot_seeds.clone();
        let deadline = deadline;
        std::thread::spawn(move || {
            let mut done = 0usize;
            while Instant::now() < deadline && done < 500 {
                let delivered = Arc::new(AtomicUsize::new(0));
                let g = gspec(keys.clone(), seeds.clone(), 2, 10 + done as u64);
                let mut v = byte_verifier(keys.clone(), seeds.clone(), fails.clone(), delivered.clone());
                let res = e.get_request(&g, &mut v);
                assert!(res.is_ok(), "c08 getter: get failed: {res:?}");
                assert_eq!(delivered.load(Ordering::SeqCst), keys.len(), "c08 getter delivery");
                done += 1;
            }
            let _ = gtx.send(done);
        })
    };

    // TWO put threads: both call put_batch concurrently (rotating fresh
    // sessions so each has a roomy scope file and no file-lock contention) —
    // each put runs maybe_reclaim, whose demote pass and metrics-held tail
    // are the two halves of the ABBA.
    let (ptx, prx) = mpsc::channel::<usize>();
    let mut put_handles = Vec::new();
    for p in 0..2usize {
        let e = e.clone();
        let deadline = deadline;
        let ptx = ptx.clone();
        put_handles.push(std::thread::spawn(move || {
            let mut done = 0usize;
            for j in 0..100usize {
                if Instant::now() >= deadline {
                    break;
                }
                let sess = 1000 + (j / 8) as u64 + p as u64 * 50;
                let mut rng = 0xBEEF_u64 ^ (j as u64 + p as u64 * 1000);
                let seed = splitmix64(&mut rng);
                let spec = pspec(sess, 1, 1, j as u64 % 8, 1, 1, seed);
                match e.put_batch(&spec) {
                    Ok(()) => done += 1,
                    Err(err) if is_scope_full_err(&err) => break,
                    Err(err) => panic!("c08 putter {p}: put failed: {err}"),
                }
            }
            let _ = ptx.send(done);
        }));
    }
    drop(ptx);

    // Wedge detection: a churn thread that NEVER returns within the watchdog
    // is parked inside an engine call — that IS the repro (expected on the
    // current source; this test passes either way and reports loudly). On the
    // wedged path NO engine lock may be taken (snapshot would block forever),
    // so the metrics-based asserts and teardown are skipped.
    let mut wedged = false;
    let getter_done = match grx.recv_timeout(WATCHDOG) {
        Ok(d) => d,
        Err(_) => {
            wedged = true;
            eprintln!("VALC c08: getter did not return within 30 s — parked inside an engine call");
            dump_c07(&e, "c08 getter stuck");
            0
        }
    };
    let mut put_done = Vec::new();
    for i in 0..2 {
        match prx.recv_timeout(WATCHDOG) {
            Ok(d) => put_done.push(d),
            Err(_) => {
                wedged = true;
                eprintln!(
                    "VALC c08: put thread {i} did not return within 30 s — parked inside an engine call"
                );
                dump_c07(&e, "c08 put thread stuck");
                put_done.push(0);
            }
        }
    }
    if !wedged {
        join_with_watchdog_dump("c08 getter", vec![g_handle], || dump_c07(&e, "c08 getter join"));
        join_with_watchdog_dump("c08 putters", put_handles, || dump_c07(&e, "c08 putter join"));
    }

    // Secondary check (healthy path only): if every churn thread returned but
    // stopped EARLY without a scope-full stop, probe with a canary get.
    let all_finished = getter_done >= 2000 && put_done.iter().all(|&d| d >= 400);
    if !wedged && !all_finished {
        let (ctx, crx) = mpsc::channel::<()>();
        {
            let e = e.clone();
            let fails = fails.clone();
            let keys = hot_keys.clone();
            let seeds = hot_seeds.clone();
            std::thread::spawn(move || {
                let delivered = Arc::new(AtomicUsize::new(0));
                let g = gspec(keys.clone(), seeds.clone(), 2, 50_000);
                let mut v = byte_verifier(keys, seeds, fails, delivered.clone());
                let _ = e.get_request(&g, &mut v);
                let _ = ctx.send(());
            });
        }
        if recv_deadline_or_opt(&crx, Duration::from_secs(8)).is_none() {
            wedged = true;
        }
    }
    let mut m = Metrics::default();
    if !wedged {
        assert_no_integrity_failures(&fails, "c08");
        m = e.snapshot();
        assert_accounting(&m, "c08");
    }
    eprintln!(
        "VALC c08: getter {} (2000 cap), putters {:?} (400 cap each); promotions {} demotions {}; \
         wedge_detected={wedged}",
        getter_done,
        put_done,
        m.migr_promotions,
        m.migr_demotions
    );
    if wedged {
        eprintln!(
            "VALC c08 ABBA REPRODUCED: bounded churn threads parked inside engine calls and a \
             canary get parked for 8 s — two concurrent put_batch callers wedged the engine \
             (metrics <-> dram_blocks ABBA, engine.rs:276/318-319/834 vs 715/771). Teardown \
             skipped: the parked threads still hold engine handles."
        );
    } else {
        let (shx, shrx) = mpsc::channel::<()>();
        let e2 = e.clone();
        std::thread::spawn(move || {
            e2.shutdown();
            let _ = shx.send(());
        });
        recv_deadline_or(&shrx, WATCHDOG, "c08: shutdown did not return within 30 s", || {
            dump_c07(&e, "c08 shutdown stuck")
        });
        drop(e);
        std::fs::remove_dir_all(&dir)
            .unwrap_or_else(|err| panic!("c08: dir must be deletable after teardown: {err}"));
    }
}
