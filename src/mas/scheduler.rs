//! FC-MAS scheduler core (paper §3.3; ARCHITECTURE §3 "Queues" row, §9 open-loop).
//!
//! Two queues behind one `Mutex` + `Condvar` (ARCHITECTURE §3 lock discipline):
//!
//! - **DRAM-Ready Queue**: min-ordered by `(arrival_ts_us, seq_no)` — `seq_no`
//!   breaks ties FIFO. Holds requests classified all-DRAM (`max_j GVA_j < 2^60`)
//!   at arrival AND re-admitted SSD requests, which keep their ORIGINAL
//!   `arrival_ts_us` (arrival-ordered re-admission, T9; paper Fig. 4(b)
//!   "Earlier completion" — a re-admitted request that already waited through
//!   SSD prefetch pops ahead of newly arrived DRAM hits with later timestamps).
//! - **SSD-Preparing Queue**: unordered Vec; selection = argmax HRRN priority,
//!   recomputed LAZILY at each dispatch decision (OQ-4) with an O(n) scan
//!   (n ≤ queue_cap). Not maintained sorted.
//!
//! HRRN Eq. 1 (paper §3.3, verbatim): `Priority_i = 1 + wait_time_i / KV_size_i`
//! with `wait_time` in seconds and `KV_size` in MiB (dimensionless, OQ-15).
//! `KV_size <= 0` is guarded to 1.0 MiB. Smaller restores get higher initial
//! priority (lower mean queuing delay); `wait_time` grows linearly, so
//! multi-GiB restores are guaranteed eventual dispatch (starvation prevention).
//!
//! Wait accounting: `enqueued_us` is stamped once at first enqueue (monotonic
//! µs clock) and preserved across SSD prefetch → re-admission, so a popped
//! request's `wait_us` covers its full queue stay (paper §3.3 seniority rule;
//! ARCHITECTURE §9 honest TTFT analog: measured latency includes queue wait).
//! A re-admitted request therefore contributes two wait samples: its SSD-segment
//! wait (at `pick_ssd`) and its total-stay wait (at `pop_dram`).
//!
//! Shutdown contract (documented): `shutdown` wakes ALL waiters, is idempotent,
//! and makes `pop_dram`/`pick_ssd` return `None` immediately even if items
//! remain queued — callers must drain BEFORE shutdown. `enqueue` calls that are
//! blocked or made after shutdown are no-ops (request dropped; no error path —
//! the engine only enqueues while running). `readmit` after shutdown is
//! likewise a no-op. Items still queued stay visible in `stats()` but are
//! never dispatched.
//!
//! Backpressure boundary (documented, exact): `enqueue` blocks while
//! `dram.len() + ssd.len() >= queue_cap`. The depth check and the insert are
//! one atomic step under the lock (a woken enqueuer re-checks the predicate
//! before inserting; a pop removes its item under the same lock before
//! notifying), so the TOTAL queued across both queues NEVER exceeds
//! `queue_cap` — no off-by-one. The bound counts requests sitting in the
//! queues only: popped (in-flight, worker-owned) requests are excluded, which
//! matches ARCHITECTURE §9 — the open-loop issue thread blocks exactly when
//! the queue holds `queue_cap` requests.

use crate::types::Request;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

/// Monotonic microsecond clock: µs since an arbitrary epoch, non-decreasing.
/// Production uses an `Instant`-based per-scheduler epoch ([`Scheduler::new`]);
/// tests inject a controllable clock ([`Scheduler::with_clock`]).
pub type Clock = Box<dyn Fn() -> u64 + Send + Sync>;

const MIB: f64 = 1024.0 * 1024.0;

/// HRRN Eq. 1 (paper §3.3, verbatim formula): `1 + wait_time / KV_size`.
/// Units (OQ-15): `wait_s` in seconds, `size_bytes` in bytes (divided by 1 MiB,
/// so the ratio is dimensionless and scale-stable on prototype hardware).
/// Guard: `size_bytes == 0` (KV_size <= 0) is treated as 1.0 MiB — never
/// divides by zero.
pub fn hrrn_priority(wait_s: f64, size_bytes: u64) -> f64 {
    let size_mib = if size_bytes == 0 { 1.0 } else { size_bytes as f64 / MIB };
    1.0 + wait_s / size_mib
}

/// A request handed to a worker by `pop_dram` (DRAM dispatch) or `pick_ssd`
/// (SSD prefetch pick). `wait_us` is the request's queue wait up to this pop,
/// measured from its original `enqueue` (for re-admitted requests it includes
/// the SSD-prefetch stay, per the paper §3.3 seniority rule).
#[derive(Debug)]
pub struct PoppedReq {
    pub req: Request,
    pub wait_us: u64,
}

/// Snapshot of scheduler counters and queue depths. Field names are
/// additive-friendly for `Metrics` (the engine copies `wait_samples` into
/// `Metrics.qwait_samples`).
#[derive(Default, Clone, Debug)]
pub struct SchedulerStats {
    pub dram_queue_len: usize,
    pub ssd_queue_len: usize,
    pub dram_popped: u64,
    pub ssd_popped: u64,
    pub readmitted_count: u64,
    pub max_dram_wait_us: u64,
    pub max_ssd_wait_us: u64,
    /// Queue-wait sample in µs per successful pop (both queues, pop order).
    pub wait_samples: Vec<u64>,
    /// HRRN argmax scans performed (one per successful `pick_ssd`).
    pub hrrn_scans: u64,
}

#[derive(Debug)]
struct PendingReq {
    req: Request,
    /// Enqueue-order FIFO tie-break (unique, monotonic per scheduler).
    seq_no: u64,
    /// Stamped once at first enqueue; preserved through re-admission.
    enqueued_us: u64,
}

/// DRAM-Ready heap entry, ordered by `key = (arrival_ts_us, seq_no)` ONLY.
/// `seq_no` is unique, so the payload never participates in comparisons
/// (`Request` has no `Ord`; comparison cost stays O(1) on the key pair).
#[derive(Debug)]
struct DramEntry {
    key: (u64, u64),
    p: PendingReq,
}

impl PartialEq for DramEntry {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}
impl Eq for DramEntry {}
impl PartialOrd for DramEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for DramEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key.cmp(&other.key)
    }
}

#[derive(Default, Debug)]
struct Inner {
    /// Min-heap on (arrival_ts_us, seq_no) via `Reverse` (T7/T9).
    dram: BinaryHeap<Reverse<DramEntry>>,
    /// SSD-Preparing, kept in insertion (seq) order; HRRN picked lazily (OQ-4).
    ssd: Vec<PendingReq>,
    /// Popped-from-SSD requests awaiting re-admission: req.id -> (seq_no,
    /// enqueued_us). Preserves queue seniority (seq_no) and the original wait
    /// start across the SSD-prefetch stay (paper §3.3). Bounded by the
    /// in-flight cap (= queue_cap, ARCHITECTURE §9).
    awaiting: HashMap<u64, (u64, u64)>,
    seq: u64,
    shutdown: bool,
    dram_popped: u64,
    ssd_popped: u64,
    readmitted: u64,
    max_dram_wait_us: u64,
    max_ssd_wait_us: u64,
    wait_samples: Vec<u64>,
    hrrn_scans: u64,
}

/// Medium-Aware Request Scheduler (paper §3.3). Pure control plane: no file
/// I/O, no payload handling — the engine (orchestrator) spawns worker threads
/// that consume what [`Scheduler::pop_dram`] / [`Scheduler::pick_ssd`] return.
/// Thread-safe: std `Mutex` + one `Condvar`; every state change notifies all
/// waiters (poppers, pickers and blocked enqueuers), so no wakeup is lost.
pub struct Scheduler {
    q: Mutex<Inner>,
    cv: Condvar,
    /// Max total queued (DRAM + SSD combined); backpressure bound.
    cap: usize,
    clock: Clock,
}

impl Scheduler {
    /// Production constructor: monotonic `Instant`-based µs clock with a small
    /// per-scheduler epoch.
    pub fn new(queue_cap: usize) -> Arc<Self> {
        let epoch = Instant::now();
        Self::with_clock(queue_cap, Box::new(move || epoch.elapsed().as_micros() as u64))
    }

    /// Constructor with an injected monotonic clock (test/determinism hook;
    /// production keeps using real time via [`Scheduler::new`]).
    /// `queue_cap < 1` is clamped to 1.
    pub fn with_clock(queue_cap: usize, clock: Clock) -> Arc<Self> {
        Arc::new(Scheduler {
            q: Mutex::new(Inner::default()),
            cv: Condvar::new(),
            cap: queue_cap.max(1),
            clock,
        })
    }

    /// Configured total-queue capacity (DRAM + SSD combined backpressure bound).
    pub fn queue_cap(&self) -> usize {
        self.cap
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // Poison-recovery is deliberate: a panic in one worker must not wedge the
        // engine's queues at teardown. State stays consistent because every
        // mutation happens under the lock as a unit.
        self.q.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Enqueue a pending request. `ssd` selects the queue: `true` →
    /// SSD-Preparing (HRRN-managed prefetch), `false` → DRAM-Ready (immediate
    /// dispatch). The caller classifies with [`crate::mas::classify`] (max-GVA
    /// rule, OQ-14); the scheduler is a pure control plane and routes by the
    /// flag as instructed.
    ///
    /// BLOCKS on the condvar while total queued (`dram.len() + ssd.len()`)
    /// >= queue_cap (backpressure, ARCHITECTURE §3/§9); wakes when pops/cancels
    /// free capacity. The predicate check and insert are one atomic step under
    /// the lock, so total queued never exceeds queue_cap (in-flight, popped
    /// requests do not count toward the bound). After [`Scheduler::shutdown`]
    /// this is a documented no-op (request dropped).
    pub fn enqueue(&self, req: Request, ssd: bool) {
        let mut g = self.lock();
        loop {
            if g.shutdown {
                return; // documented: post-shutdown enqueues are dropped
            }
            if g.dram.len() + g.ssd.len() < self.cap {
                break;
            }
            g = self.cv.wait(g).unwrap_or_else(PoisonError::into_inner);
        }
        let now = (self.clock)();
        g.seq += 1;
        let p = PendingReq { req, seq_no: g.seq, enqueued_us: now };
        if ssd {
            g.ssd.push(p);
        } else {
            let ats = p.req.arrival_ts_us;
            g.dram.push(Reverse(DramEntry { key: (ats, p.seq_no), p }));
        }
        drop(g);
        self.cv.notify_all();
    }

    /// Pop the head of the DRAM-Ready queue (min `(arrival_ts_us, seq_no)`).
    /// BLOCKS until a request is available; returns `None` once shut down.
    /// Re-admitted requests keep their original `arrival_ts_us`, so a
    /// re-admitted request pops ahead of newly arrived DRAM hits with later
    /// timestamps (T9, paper Fig. 4(b)).
    pub fn pop_dram(&self) -> Option<PoppedReq> {
        let mut g = self.lock();
        loop {
            if g.shutdown {
                return None; // documented: drain BEFORE shutdown
            }
            if let Some(Reverse(e)) = g.dram.pop() {
                let now = (self.clock)();
                let wait = now.saturating_sub(e.p.enqueued_us);
                g.dram_popped += 1;
                g.max_dram_wait_us = g.max_dram_wait_us.max(wait);
                g.wait_samples.push(wait);
                drop(g);
                self.cv.notify_all(); // freed capacity → wake blocked enqueuers
                return Some(PoppedReq { req: e.p.req, wait_us: wait });
            }
            g = self.cv.wait(g).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Pick the next SSD-Preparing request by HRRN priority (Eq. 1), recomputed
    /// lazily at this dispatch decision (OQ-4) via an O(n) scan (n ≤ queue_cap).
    /// Ties resolve FIFO: strict `>` keeps the earliest-seq maximum and the Vec
    /// is kept in insertion order. BLOCKS until a request is available; returns
    /// `None` once shut down. The picked request moves to the awaiting-re-
    /// admission table so its seniority and original wait start survive the
    /// prefetch stay; the caller (SSD worker) hands it back via
    /// [`Scheduler::readmit`] once FC-CIO prefetch completed.
    pub fn pick_ssd(&self) -> Option<PoppedReq> {
        let mut g = self.lock();
        loop {
            if g.shutdown {
                return None; // documented: drain BEFORE shutdown
            }
            if !g.ssd.is_empty() {
                let now = (self.clock)();
                let mut best = 0usize;
                let mut best_pri = f64::NEG_INFINITY;
                for (i, p) in g.ssd.iter().enumerate() {
                    let wait_s = now.saturating_sub(p.enqueued_us) as f64 / 1e6;
                    let pri = hrrn_priority(wait_s, p.req.size_bytes);
                    if pri > best_pri {
                        best_pri = pri;
                        best = i;
                    }
                }
                g.hrrn_scans += 1;
                let p = g.ssd.remove(best); // order-preserving → stable FIFO tie-break
                let wait = now.saturating_sub(p.enqueued_us);
                g.awaiting.insert(p.req.id, (p.seq_no, p.enqueued_us));
                g.ssd_popped += 1;
                g.max_ssd_wait_us = g.max_ssd_wait_us.max(wait);
                g.wait_samples.push(wait);
                drop(g);
                self.cv.notify_all(); // freed capacity → wake blocked enqueuers
                return Some(PoppedReq { req: p.req, wait_us: wait });
            }
            g = self.cv.wait(g).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Re-admit a prefetched SSD request into the DRAM-Ready queue keyed by its
    /// ORIGINAL `arrival_ts_us` (T9, paper §3.3 arrival-ordered re-admission:
    /// time spent in SSD prefetching counts toward queue seniority). Its
    /// `seq_no` and `enqueued_us` are restored from the awaiting table, so FIFO
    /// tie-break seniority and total-wait accounting are preserved. Calling it
    /// with an id the scheduler never picked grants fresh seniority (documented
    /// leniency); after [`Scheduler::shutdown`] it is a no-op.
    pub fn readmit(&self, req: Request) {
        let mut g = self.lock();
        if g.shutdown {
            return; // documented no-op
        }
        let now = (self.clock)();
        let (seq_no, enqueued_us) = match g.awaiting.remove(&req.id) {
            Some(pair) => pair, // seniority preserved across the prefetch stay
            None => {
                g.seq += 1;
                (g.seq, now)
            }
        };
        let ats = req.arrival_ts_us;
        g.dram.push(Reverse(DramEntry {
            key: (ats, seq_no),
            p: PendingReq { req, seq_no, enqueued_us },
        }));
        g.readmitted += 1;
        drop(g);
        self.cv.notify_all();
    }

    /// Remove a pending request from wherever it sits: DRAM-Ready heap,
    /// SSD-Preparing Vec, or the awaiting-re-admission table (in-prefetch).
    /// Returns whether anything was removed. Wakes blocked enqueuers (freed
    /// capacity). Idempotent: cancelling an absent id returns `false`.
    pub fn cancel(&self, req_id: u64) -> bool {
        let mut g = self.lock();
        let mut found = g.awaiting.remove(&req_id).is_some();
        let before = g.ssd.len();
        g.ssd.retain(|p| p.req.id != req_id);
        found |= g.ssd.len() != before;
        if !found && g.dram.iter().any(|e| e.0.p.req.id == req_id) {
            // BinaryHeap has no ordered removal: rebuild from its internal Vec.
            // The heap invariant is preserved by Vec -> BinaryHeap::from.
            let heap = std::mem::take(&mut g.dram);
            let kept: Vec<_> =
                heap.into_iter().filter(|e| e.0.p.req.id != req_id).collect();
            g.dram = kept.into();
            found = true;
        }
        drop(g);
        self.cv.notify_all();
        found
    }

    /// Idempotently shut the scheduler down: wakes ALL waiters — blocked
    /// `pop_dram`/`pick_ssd` return `None`, blocked (or subsequent) `enqueue`
    /// calls return without queueing — and freezes all further work. Items
    /// still queued remain visible in `stats()` but are never dispatched:
    /// callers must drain BEFORE calling this.
    pub fn shutdown(&self) {
        let mut g = self.lock();
        if g.shutdown {
            return; // idempotent
        }
        g.shutdown = true;
        g.awaiting.clear(); // private bookkeeping only; queues stay as-is in stats
        drop(g);
        self.cv.notify_all();
    }

    /// Consistent snapshot of counters and queue depths (taken under the lock).
    pub fn stats(&self) -> SchedulerStats {
        let g = self.lock();
        SchedulerStats {
            dram_queue_len: g.dram.len(),
            ssd_queue_len: g.ssd.len(),
            dram_popped: g.dram_popped,
            ssd_popped: g.ssd_popped,
            readmitted_count: g.readmitted,
            max_dram_wait_us: g.max_dram_wait_us,
            max_ssd_wait_us: g.max_ssd_wait_us,
            wait_samples: g.wait_samples.clone(),
            hrrn_scans: g.hrrn_scans,
        }
    }
}
