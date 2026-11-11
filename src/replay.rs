//! Open-loop replay driver (T18, ARCHITECTURE §9): paces records at
//! `start + ts_us × time_scale`, executes puts+gets on a bounded worker pool
//! (backpressure at queue_cap), verifies payload integrity, collects metrics.
//!
//! Latency semantics: request-level latency (issue → completion, including work-queue
//! wait) is measured here and REPLACES `metrics.lat_samples` in the final snapshot.
//! Engine-side lat_samples are service time; queue_wait_us / prep_us stay engine-side.

use crate::config::Config;
use crate::engine_api::{GetSpec, KvEngine, PutSpec};
use crate::types::{BlockKey, Metrics};
use crate::util::payload_fill;
use crate::workload::{Op, TraceRecord};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const REPLAY_WORKERS: usize = 32;

enum Job {
    Put(PutSpec),
    Get(GetSpec, Instant), // Instant = issue time (request-level latency)
    Shutdown,
}

pub struct ReplayOutput {
    pub metrics: Metrics,
    pub elapsed_s: f64,
    pub integrity_failures: u64,
    pub requests_issued: u64,
}

struct Shared {
    queue: Mutex<VecDeque<Job>>,
    cv: Condvar,
    depth: AtomicU64, // enqueued but not yet completed
}

pub fn run(
    engine: Arc<dyn KvEngine>,
    records: &[TraceRecord],
    cfg: &Config,
    time_scale: f64,
    csv_window_us: u64,
    csv_path: Option<&str>,
) -> ReplayOutput {
    // Startup guard (OQ-16 / ARCHITECTURE §3): unique blocks must fit the
    // fixed-capacity Level-1 index at its documented 0.7 load factor.
    {
        let mut unique: std::collections::HashSet<(u64, u32, u64)> = Default::default();
        for r in records {
            if r.op == Op::Put {
                for l in 0..r.layers {
                    for i in 0..r.n_blk {
                        unique.insert((
                            r.session,
                            l,
                            (r.start_blk + i as u64) * cfg.block_tokens,
                        ));
                    }
                }
            }
        }
        let cap = cfg.index_capacity;
        if (unique.len() as f64) > 0.7 * cap as f64 {
            panic!(
                "trace needs {} unique blocks > 0.7 x index_capacity ({}); raise index_capacity or shrink the workload",
                unique.len(),
                cap
            );
        }
    }

    // Per-block seed lookup: block (session, token_start = idx*bt) was created by the
    // put whose [start_blk, start_blk+n_blk) range contains idx (integrity + recompute).
    let mut session_seeds: HashMap<u64, Vec<(u64, u64, u64)>> = HashMap::new(); // session -> (start_blk, n_blk_per_layer, seed)
    for r in records {
        if r.op == Op::Put {
            session_seeds
                .entry(r.session)
                .or_default()
                .push((r.start_blk, r.n_blk as u64, r.seed));
        }
    }
    let seed_for = |ss: &HashMap<u64, Vec<(u64, u64, u64)>>, session: u64, blk_idx: u64| -> u64 {
        ss.get(&session)
            .and_then(|v| {
                v.iter()
                    .find(|(sb, nb, _)| blk_idx >= *sb && blk_idx < sb + nb)
            })
            .map(|(_, _, s)| *s)
            .unwrap_or(0)
    };
    let session_seeds = Arc::new(session_seeds);

    let bb = cfg.block_bytes();
    let bt = cfg.block_tokens;
    let shared = Arc::new(Shared {
        queue: Mutex::new(VecDeque::with_capacity(cfg.queue_cap)),
        cv: Condvar::new(),
        depth: AtomicU64::new(0),
    });
    let integrity = Arc::new(AtomicU64::new(0));
    let issued = Arc::new(AtomicU64::new(0));
    let lat: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));

    let mut handles = Vec::with_capacity(REPLAY_WORKERS);
    for _ in 0..REPLAY_WORKERS {
        let shared = shared.clone();
        let engine = engine.clone();
        let integrity = integrity.clone();
        let lat = lat.clone();
        handles.push(std::thread::spawn(move || loop {
            let job = {
                let mut q = shared.queue.lock().unwrap();
                loop {
                    match q.pop_front() {
                        Some(j) => break j,
                        None => q = shared.cv.wait(q).unwrap(),
                    }
                }
            };
            match job {
                Job::Shutdown => break,
                Job::Put(spec) => {
                    engine.put_batch(&spec).expect("put_batch failed");
                    shared.depth.fetch_sub(1, Ordering::SeqCst);
                    // Wake the issue thread if it is parked on queue_cap backpressure.
                    shared.cv.notify_all();
                }
                Job::Get(spec, t_issue) => {
                    let mut payload = vec![0u8; bb as usize];
                    engine
                        .get_request(&spec, &mut |idx, bytes| {
                            let b = &spec.blocks[idx];
                            let seed = spec.seed_by_block[idx];
                            payload_fill(b.session, b.layer, b.token_start, seed, &mut payload);
                            if bytes != payload.as_slice() {
                                integrity.fetch_add(1, Ordering::SeqCst);
                            }
                        })
                        .expect("get_request failed");
                    lat.lock().unwrap().push(t_issue.elapsed().as_micros() as u64);
                    shared.depth.fetch_sub(1, Ordering::SeqCst);
                    shared.cv.notify_all();
                }
            }
        }));
    }

    let t0 = Instant::now();
    let mut csv = csv_path.map(CsvLog::new);
    let mut window = 0u64;

    fn enqueue(shared: &Shared, cfg: &Config, job: Job) {
        let mut q = shared.queue.lock().unwrap();
        while shared.depth.load(Ordering::SeqCst) >= cfg.queue_cap as u64 {
            q = shared.cv.wait(q).unwrap();
        }
        shared.depth.fetch_add(1, Ordering::SeqCst);
        q.push_back(job);
        shared.cv.notify_one();
    }

    for (i, r) in records.iter().enumerate() {
        // Open-loop pacing: absolute schedule from record position.
        let due = t0 + Duration::from_micros((r.ts_us as f64 * time_scale) as u64);
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }
        match r.op {
            Op::Put => {
                enqueue(
                    &shared,
                    cfg,
                    Job::Put(PutSpec {
                        session: r.session,
                        turn: r.turn,
                        layers: r.layers,
                        start_blk: r.start_blk,
                        n_blk: r.n_blk,
                        shared: r.shared,
                        reply_tokens: r.reply_tokens,
                        ctx_tokens: r.ctx_tokens,
                        seed: r.seed,
                    }),
                );
            }
            Op::Get => {
                let mut blocks: Vec<BlockKey> = Vec::new();
                let mut seeds: Vec<u64> = Vec::new();
                if r.shared_with > 0 {
                    // Shared prompt: session's first put defines its prefix (start 0).
                    if let Some(ext) = session_seeds
                        .get(&r.shared_with)
                        .and_then(|v| v.iter().find(|(s, _, _)| *s == 0))
                    {
                        let (_, sw_n, _) = *ext;
                        for l in 0..r.layers {
                            for i in 0..sw_n {
                                blocks.push(BlockKey {
                                    session: r.shared_with,
                                    layer: l,
                                    token_start: i * bt,
                                });
                                seeds.push(seed_for(&session_seeds, r.shared_with, i));
                            }
                        }
                    }
                }
                for l in 0..r.layers {
                    for i in 0..r.n_blk {
                        let blk_idx = r.start_blk + i as u64;
                        blocks.push(BlockKey {
                            session: r.session,
                            layer: l,
                            token_start: blk_idx * bt,
                        });
                        seeds.push(seed_for(&session_seeds, r.session, blk_idx));
                    }
                }
                let spec = GetSpec {
                    id: i as u64,
                    arrival_ts_us: r.ts_us,
                    blocks,
                    seed_by_block: seeds,
                    signals: fckv_signals(r),
                };
                issued.fetch_add(1, Ordering::SeqCst);
                enqueue(&shared, cfg, Job::Get(spec, Instant::now()));
            }
        }
        if let Some(w) = csv.as_mut() {
            let t_us = t0.elapsed().as_micros() as u64;
            if t_us >= (window + 1) * csv_window_us {
                window += 1;
                let m = engine.snapshot();
                w.row(window, t_us, &m);
            }
        }
    }

    // Drain: wait until every enqueued job has completed, then stop workers.
    loop {
        if shared.depth.load(Ordering::SeqCst) == 0 && shared.queue.lock().unwrap().is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    {
        let mut q = shared.queue.lock().unwrap();
        for _ in 0..REPLAY_WORKERS {
            q.push_back(Job::Shutdown);
        }
        shared.cv.notify_all();
    }
    for h in handles {
        h.join().unwrap();
    }

    let mut m = engine.snapshot();
    // Request-level latency (issue → completion incl. work-queue wait) is authoritative.
    m.lat_samples = std::mem::take(&mut *lat.lock().unwrap());
    m.delivered_requests = issued.load(Ordering::SeqCst);
    let integrity_failures = integrity.load(Ordering::SeqCst);
    if let Some(w) = csv.as_mut() {
        w.finish();
    }
    ReplayOutput {
        metrics: m,
        elapsed_s: t0.elapsed().as_secs_f64(),
        integrity_failures,
        requests_issued: issued.load(Ordering::SeqCst),
    }
}

/// Write-time signals of the requesting session (from the trace record) for
/// recompute re-persist placement (engine_api::GetSpec.signals).
fn fckv_signals(r: &TraceRecord) -> crate::types::PlacementSignals {
    crate::types::PlacementSignals {
        session: r.session,
        refcount: if r.shared == 1 { 2 } else { 1 },
        turn: r.turn,
        reply_tokens: r.reply_tokens,
        ctx_tokens: r.ctx_tokens,
    }
}

struct CsvLog {
    f: std::fs::File,
}

impl CsvLog {
    fn new(path: &str) -> CsvLog {
        if let Some(d) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(d).ok();
        }
        let mut f = std::fs::File::create(path).expect("csv create");
        use std::io::Write;
        writeln!(
            f,
            "window_idx,t_us,delivered,requested,hit_rate,phys_reads,read_amp"
        )
        .ok();
        CsvLog { f }
    }
    fn row(&mut self, w: u64, t_us: u64, m: &Metrics) {
        use std::io::Write;
        let hits = m.hit_hbm + m.hit_dram + m.hit_ssd;
        writeln!(
            self.f,
            "{},{},{},{},{:.4},{},{:.4}",
            w,
            t_us,
            hits,
            m.requested_blocks,
            m.delivered_hit_rate(),
            m.physical_read_ops,
            m.read_amp()
        )
        .ok();
    }
    fn finish(&mut self) {
        use std::io::Write;
        self.f.flush().ok();
    }
}
