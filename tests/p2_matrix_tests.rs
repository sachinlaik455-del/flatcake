//! P2-M6 verification matrix (directive §13) — cross-component tests NOT already
//! covered by gva_tests / cio_tests / baseline_tests / replay_baseline_tests /
//! mas_tests / engine_tests (their coverage is cited in docs/PHASE_2_TEST_REPORT.md).
//! Each test names the requirement it verifies (MATRIX row).

use fckv::baseline::BaselineEngine;
use fckv::config::Config;
use fckv::engine::FlatCakeEngine;
use fckv::engine_api::{GetSpec, KvEngine, PutSpec};
use fckv::types::{BlockKey, PlacementSignals};
use fckv::util::payload_fill;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static IDS: AtomicU64 = AtomicU64::new(1);
fn next_id() -> u64 {
    IDS.fetch_add(1, Ordering::SeqCst)
}

fn cfg_for(tag: &str) -> Config {
    let mut c = Config::default();
    c.dram_pool_bytes = 128 * 1024;
    c.hbm_window_bytes = 64 * 1024;
    c.gpu_file_bytes = 256 * 1024;
    c.gpu_file_pool = 4;
    c.ssd_dir = format!("data/test/p2-{tag}-{}", std::process::id());
    c.baseline_ssd_dir = format!("data/test/p2b-{tag}-{}", std::process::id());
    c.queue_cap = 64;
    c
}

fn put_spec(session: u64, turn: u64, layers: u32, start: u64, n: u32, seed: u64) -> PutSpec {
    PutSpec {
        session,
        turn,
        layers,
        start_blk: start,
        n_blk: n,
        shared: 0,
        reply_tokens: 500,
        ctx_tokens: layers as u64 * (start + n as u64) * 256,
        seed,
    }
}

fn keys(session: u64, layers: u32, n: u32, bt: u64, seed: u64) -> (Vec<BlockKey>, Vec<u64>) {
    let mut ks = Vec::new();
    for l in 0..layers {
        for i in 0..n {
            ks.push(BlockKey { session, layer: l, token_start: i as u64 * bt });
        }
    }
    (ks.clone(), vec![seed; ks.len()])
}

// MATRIX: workload determinism — same seed => byte-identical trace.
#[test]
fn matrix_trace_determinism() {
    let dir = format!("data/test/p2-det-{}", std::process::id());
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mk = |out: &str| fckv::workload::GenParams {
        regime: "mixed".into(),
        sessions: 6,
        turns: 4,
        layers: 2,
        blocks_per_turn: 2,
        shared_blocks: 4,
        fanout: 1,
        reuse_rate: 0.3,
        qps: 3.0,
        seed: 77,
        out: out.into(),
        block_tokens: 256,
        bytes_per_token: 32,
        gpu_file_bytes: 1 << 28,
        ssd_budget_bytes: 8 << 20,
    };
    let a = format!("{dir}/a.jsonl");
    let b = format!("{dir}/b.jsonl");
    fckv::workload::generate(&mk(&a)).expect("gen a");
    fckv::workload::generate(&mk(&b)).expect("gen b");
    let sa = std::fs::read_to_string(&a).unwrap();
    let sb = std::fs::read_to_string(&b).unwrap();
    assert_eq!(sa, sb, "same seed must produce byte-identical traces");
    // And a different seed differs.
    let mut other = mk(&format!("{dir}/c.jsonl"));
    other.seed = 78;
    fckv::workload::generate(&other).expect("gen c");
    let sc = std::fs::read_to_string(format!("{dir}/c.jsonl")).unwrap();
    assert_ne!(sa, sc);
}

// MATRIX: block immutability after publish (double-put cannot alter payload).
#[test]
fn matrix_published_blocks_immutable() {
    let cfg = cfg_for("imm");
    let e = FlatCakeEngine::new(&cfg).unwrap();
    let bt = cfg.block_tokens;
    e.put_batch(&put_spec(2, 1, 2, 0, 2, 10)).unwrap();
    // Same key, different seed: write-once — the second batch must NOT overwrite
    // the first (duplicate identity => original bytes persist).
    e.put_batch(&put_spec(2, 1, 2, 0, 2, 999)).unwrap();
    let (ks, _) = keys(2, 2, 2, bt, 10);
    let g = GetSpec {
        id: next_id(),
        arrival_ts_us: 1,
        blocks: ks.clone(),
        seed_by_block: vec![10; ks.len()],
        signals: PlacementSignals { session: 2, refcount: 1, turn: 1, reply_tokens: 500, ctx_tokens: 1024 },
    };
    e.get_request(&g, &mut |idx, bytes| {
        let k = g.blocks[idx];
        let mut expect = vec![0u8; cfg.block_bytes() as usize];
        payload_fill(k.session, k.layer, k.token_start, 10, &mut expect);
        assert_eq!(bytes, expect, "original payload must be immutable");
    })
    .unwrap();
    e.shutdown();
}

// MATRIX: concurrent retrieval + simultaneous reclamation => zero corruption.
#[test]
fn matrix_concurrent_reads_with_reclaim_pressure() {
    let cfg = cfg_for("cr");
    let e = Arc::new(FlatCakeEngine::new(&cfg).unwrap());
    let bt = cfg.block_tokens;
    // Seed several sessions (SSD-bound via ctx_tokens).
    for s in 2..10u64 {
        e.put_batch(&put_spec(s, 1, 2, 0, 3, s * 7)).unwrap();
    }
    let (ks, seeds) = keys(2, 2, 3, bt, 14);
    let g = GetSpec {
        id: next_id(),
        arrival_ts_us: 0,
        blocks: ks.clone(),
        seed_by_block: seeds,
        signals: PlacementSignals { session: 2, refcount: 1, turn: 1, reply_tokens: 500, ctx_tokens: 1536 },
    };
    let mut handles = Vec::new();
    for _ in 0..4 {
        let e = e.clone();
        let g = g.clone();
        handles.push(std::thread::spawn(move || {
            for _ in 0..25 {
                e.get_request(&g, &mut |idx, bytes| {
                    let k = g.blocks[idx];
                    let mut expect = vec![0u8; 8192];
                    payload_fill(k.session, k.layer, k.token_start, g.seed_by_block[idx], &mut expect);
                    assert_eq!(bytes, expect, "corruption under reclaim pressure");
                })
                .unwrap();
            }
        }));
    }
    // Reclamation driver: more sessions push usage past watermarks repeatedly.
    for s in 20..40u64 {
        e.put_batch(&put_spec(s, 1, 2, 0, 3, s * 13)).unwrap();
    }
    for h in handles {
        h.join().unwrap();
    }
    e.shutdown();
}

// MATRIX: engine accounting invariant under stress: hits + misses == requested.
#[test]
fn matrix_accounting_invariant_under_stress() {
    let cfg = cfg_for("acc");
    let e = Arc::new(FlatCakeEngine::new(&cfg).unwrap());
    let bt = cfg.block_tokens;
    let mut handles = Vec::new();
    for s in 2..6u64 {
        let e = e.clone();
        handles.push(std::thread::spawn(move || {
            for t in 1..6u64 {
                e.put_batch(&put_spec(s, t, 2, (t - 1) * 2, 2, s * 100 + t)).unwrap();
                let (mut ks, mut seeds) = keys(s, 2, (t * 2) as u32, bt, s * 100 + 1);
                // seeds must match per-block creating put (first put's seed covers blk 0-1, etc.)
                seeds.clear();
                for k in &ks {
                    let blk = k.token_start / bt;
                    seeds.push(s * 100 + 1 + (blk / 2));
                }
                ks.truncate(6);
                seeds.truncate(6);
                let g = GetSpec {
                    id: next_id(),
                    arrival_ts_us: 0,
                    blocks: ks,
                    seed_by_block: seeds,
                    signals: PlacementSignals { session: s, refcount: 1, turn: t, reply_tokens: 500, ctx_tokens: 4096 },
                };
                e.get_request(&g, &mut |_, _| {}).unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let m = e.snapshot();
    assert_eq!(
        m.hit_hbm + m.hit_dram + m.hit_ssd + m.misses,
        m.requested_blocks,
        "hits + misses must equal requested blocks"
    );
    e.shutdown();
}

// MATRIX: queue_cap boundedness — replay completes with a tiny cap (backpressure
// engages, issue thread blocks, no unbounded growth, clean drain).
#[test]
fn matrix_replay_bounded_queue_cap() {
    let mut cfg = cfg_for("qc");
    cfg.queue_cap = 8;
    let dir = format!("data/test/p2-qc-{}", std::process::id());
    let _ = std::fs::remove_dir_all(&dir);
    let p = fckv::workload::GenParams {
        regime: "agentic".into(),
        sessions: 4,
        turns: 5,
        layers: 2,
        blocks_per_turn: 2,
        shared_blocks: 4,
        fanout: 0,
        reuse_rate: 0.1,
        qps: 50.0,
        seed: 5,
        out: format!("{dir}/t.jsonl"),
        block_tokens: 256,
        bytes_per_token: 32,
        gpu_file_bytes: 1 << 28,
        ssd_budget_bytes: 8 << 20,
    };
    fckv::workload::generate(&p).expect("gen");
    let records = fckv::workload::parse_trace(&p.out).expect("parse");
    let engine = Arc::new(BaselineEngine::new(&cfg).unwrap());
    let out = fckv::replay::run(engine.clone(), &records, &cfg, 0.0, cfg.csv_window_us, None);
    engine.shutdown();
    assert_eq!(out.integrity_failures, 0);
    assert!(out.requests_issued > 0);
}

// MATRIX: baseline session-expiration semantics — bucket reclaim invalidates hbm
// window entries; recomputed blocks return correct bytes (no stale hbm delivery).
#[test]
fn matrix_baseline_reclaim_invalidates_hbm() {
    let mut cfg = cfg_for("exp");
    cfg.dram_pool_bytes = 64 * 1024;
    cfg.bucket_bytes = 64 * 1024;
    cfg.bucket_count = 2;
    cfg.hbm_window_bytes = 1 << 20; // big enough to hold stale entries
    let e = BaselineEngine::new(&cfg).unwrap();
    let bt = cfg.block_tokens;
    // Session 2 blocks written then read (populating hbm window).
    e.put_batch(&put_spec(2, 1, 2, 0, 2, 21)).unwrap();
    let (ks, seeds) = keys(2, 2, 2, bt, 21);
    let g = GetSpec {
        id: next_id(),
        arrival_ts_us: 0,
        blocks: ks,
        seed_by_block: seeds,
        signals: PlacementSignals { session: 2, refcount: 1, turn: 1, reply_tokens: 100, ctx_tokens: 1024 },
    };
    e.get_request(&g, &mut |_, _| {}).unwrap();
    let m0 = e.snapshot();
    assert!(m0.hit_hbm + m0.hit_dram > 0);
    // Force bucket recycling with many more blocks (16-block SSD budget).
    for s in 3..10u64 {
        e.put_batch(&put_spec(s, 1, 2, 0, 2, s)).unwrap();
    }
    // Session 2's early blocks may be reclaimed; a re-read must deliver correct
    // bytes via dram/ssd/recompute — never stale hbm bytes from the old entry.
    e.get_request(&g, &mut |idx, bytes| {
        let k = g.blocks[idx];
        let mut expect = vec![0u8; cfg.block_bytes() as usize];
        payload_fill(k.session, k.layer, k.token_start, 21, &mut expect);
        assert_eq!(bytes, expect, "stale hbm/invalidated block delivered");
    })
    .unwrap();
    e.shutdown();
}

// MATRIX: engine memory boundedness — mem_used plateaus under sustained load.
#[test]
fn matrix_engine_memory_plateau() {
    let cfg = cfg_for("mem");
    let e = FlatCakeEngine::new(&cfg).unwrap();
    let bt = cfg.block_tokens;
    let mut peak_after_warm = 0u64;
    for s in 2..40u64 {
        e.put_batch(&put_spec(s, 1, 2, 0, 3, s)).unwrap();
        let (ks, seeds) = keys(s, 2, 3, bt, s);
        let g = GetSpec {
            id: next_id(),
            arrival_ts_us: 0,
            blocks: ks,
            seed_by_block: seeds,
            signals: PlacementSignals { session: s, refcount: 1, turn: 1, reply_tokens: 500, ctx_tokens: 1536 },
        };
        e.get_request(&g, &mut |_, _| {}).unwrap();
        if s > 10 {
            peak_after_warm = peak_after_warm.max(e.mem_used());
        }
    }
    // DRAM arena 128 KiB + hbm 64 KiB: hard bound regardless of 38 sessions.
    assert!(
        peak_after_warm <= cfg.dram_pool_bytes + cfg.hbm_window_bytes + 8 * 1024,
        "memory must stay bounded: used {peak_after_warm} vs pools {}",
        cfg.dram_pool_bytes + cfg.hbm_window_bytes
    );
    e.shutdown();
}
