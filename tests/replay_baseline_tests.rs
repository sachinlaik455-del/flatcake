//! M3 pipeline validation: generated trace -> open-loop replay -> baseline engine.
//! Asserts integrity, hit-rate sanity, and that the tiered mechanisms fire.

use fckv::baseline::BaselineEngine;
use fckv::config::Config;
use fckv::replay;
use fckv::workload::{generate, GenParams, Op};
use std::sync::Arc;

fn tiny_cfg(tag: &str) -> Config {
    let mut c = Config::default();
    c.dram_pool_bytes = 512 * 1024; // 64 blocks of 8 KiB — heavy pressure
    c.hbm_window_bytes = 256 * 1024;
    c.bucket_bytes = 256 * 1024;
    c.bucket_count = 8; // 2 MiB SSD budget
    c.staging_pool_bytes = 256 * 1024;
    c.baseline_ssd_dir = format!("data/test/replay-{tag}-{}", std::process::id());
    c.queue_cap = 64;
    c
}

fn gen_trace(path: &str, qps: f64) -> usize {
    let p = GenParams {
        regime: "agentic".into(),
        sessions: 4,
        turns: 8,
        layers: 4,
        blocks_per_turn: 4,
        shared_blocks: 8,
        fanout: 0,
        reuse_rate: 0.1,
        qps,
        seed: 11,
        out: path.into(),
        block_tokens: 256,
        bytes_per_token: 32,
        gpu_file_bytes: 1 << 28,
        ssd_budget_bytes: 8 << 20,
    };
    let (n, _) = generate(&p).expect("generate");
    n as usize
}

#[test]
fn replay_over_baseline_is_integrated_and_correct() {
    let cfg = tiny_cfg("pipe");
    let trace_path = format!("data/test/trace-{}.jsonl", std::process::id());
    let n = gen_trace(&trace_path, 20.0); // fast: pace compressed
    assert!(n > 10);

    let records = fckv::workload::parse_trace(&trace_path).expect("parse");
    let puts = records.iter().filter(|r| r.op == Op::Put).count();
    let gets = records.iter().filter(|r| r.op == Op::Get).count();
    assert!(puts > 0 && gets > 0);

    let engine = Arc::new(BaselineEngine::new(&cfg).unwrap());
    let out = replay::run(engine.clone(), &records, &cfg, 0.0, cfg.csv_window_us, None);
    // time_scale 0 = no pacing (compressed test run)

    // Integrity: every delivered block byte-matched its deterministic pattern.
    assert_eq!(out.integrity_failures, 0, "payload integrity must hold");

    // The shared prompt + immediate-turn reuse must produce hits.
    let m = &out.metrics;
    assert!(m.delivered_hit_rate() > 0.0, "expected some delivered hits");
    assert!(m.hit_hbm + m.hit_dram + m.hit_ssd > 0);
    assert_eq!(
        (m.hit_hbm + m.hit_dram + m.hit_ssd + m.misses) as usize,
        m.requested_blocks as usize
    );
    // Tiny DRAM (64 blocks) vs many more unique blocks => SSD involvement.
    assert!(
        m.hit_ssd > 0 || m.misses > 0,
        "small DRAM forces SSD reads or reclaim-induced misses"
    );
    assert!(out.requests_issued == gets as u64);
    assert!(m.logical_block_gets > 0);
    assert!(!m.lat_samples.is_empty());
}

#[test]
fn replay_pacing_is_open_loop() {
    // At time_scale 1.0 with qps encoded in ts_us, a 20-record trace at 5 qps takes
    // >= (records/qps) seconds wall clock — proving the driver paces, not slams.
    let cfg = tiny_cfg("pace");
    let trace_path = format!("data/test/trace-pace-{}.jsonl", std::process::id());
    gen_trace(&trace_path, 5.0);
    let records = fckv::workload::parse_trace(&trace_path).unwrap();
    let engine = Arc::new(BaselineEngine::new(&cfg).unwrap());
    let t0 = std::time::Instant::now();
    let _ = replay::run(engine, &records, &cfg, 1.0, cfg.csv_window_us, None);
    let elapsed = t0.elapsed().as_secs_f64();
    let expected = records.last().unwrap().ts_us as f64 / 1e6;
    assert!(
        elapsed >= expected * 0.9,
        "open-loop pacing must honor ts_us (elapsed {elapsed:.2}s vs schedule {expected:.2}s)"
    );
}
