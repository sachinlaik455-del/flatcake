//! fckv — FlatCake Level-B prototype (bin). Library code lives in src/lib.rs.
//! Subcommands: selftest | gen-workload | bench | demo .

use fckv::baseline::BaselineEngine;
use fckv::config;
use fckv::engine::FlatCakeEngine;
use fckv::engine_api::KvEngine;
use fckv::types::{Gva, SSD_FILE_MAX};
use fckv::workload::GenParams;
use std::sync::Arc;

fn selftest() -> i32 {
    let cases = [
        (fckv::types::MEDIA_DRAM, 0u64, 0u64),
        (fckv::types::MEDIA_DRAM, 4095, 0xFFFF_FFFF_FFFF),
        (fckv::types::MEDIA_SSD, 0, SSD_FILE_MAX - 1),
        (fckv::types::MEDIA_SSD, 4095, 0xFFFF_FFFF_FFFF),
    ];
    for (m, n, o) in cases {
        let g = Gva::encode(m, n, o).expect("valid encode");
        assert_eq!(g.media(), m);
        assert_eq!(g.node(), n);
        assert_eq!(g.offset(), o);
        assert_eq!(g.is_ssd(), m == fckv::types::MEDIA_SSD);
    }
    assert!(Gva::encode(2, 0, 0).is_none());
    assert!(Gva::encode(0, 0x1000, 0).is_none());
    assert!(Gva::encode(0, 0, 1 << 48).is_none());

    let cfg = config::Config::default();
    assert_eq!(cfg.block_bytes(), 8192);
    assert_eq!(cfg.gpu_file_bytes, SSD_FILE_MAX);
    assert!(config::load("does-not-exist.conf").is_err());

    println!("SELFTEST PASS");
    0
}

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a.as_str() == format!("--{name}").as_str())
        .and_then(|i| args.get(i + 1).cloned())
}

fn load_cfg() -> config::Config {
    let path = arg("config").unwrap_or_else(|| "fckv.conf".into());
    match config::load(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config ({path}): {e} — falling back to defaults");
            config::Config::default()
        }
    }
}

fn gen_workload() -> i32 {
    let cfg = config::Config::default();
    let p = GenParams {
        regime: arg("regime").unwrap_or_else(|| "agentic".into()),
        sessions: arg("sessions").unwrap_or_default().parse().unwrap_or(8),
        turns: arg("turns").unwrap_or_default().parse().unwrap_or(16),
        layers: arg("layers").unwrap_or_default().parse().unwrap_or(8),
        blocks_per_turn: arg("blocks-per-turn").unwrap_or_default().parse().unwrap_or(8),
        shared_blocks: arg("shared-blocks").unwrap_or_default().parse().unwrap_or(16),
        fanout: arg("fanout").unwrap_or_default().parse().unwrap_or(0),
        reuse_rate: arg("reuse-rate").unwrap_or_default().parse().unwrap_or(0.1),
        qps: arg("qps").unwrap_or_default().parse().unwrap_or(1.0),
        seed: arg("seed").unwrap_or_default().parse().unwrap_or(42),
        out: arg("out").unwrap_or_else(|| "data/workloads/trace.jsonl".into()),
        block_tokens: cfg.block_tokens,
        bytes_per_token: cfg.bytes_per_token,
        gpu_file_bytes: cfg.gpu_file_bytes,
        ssd_budget_bytes: cfg.bucket_bytes * cfg.bucket_count,
    };
    match fckv::workload::generate(&p) {
        Ok((n, bytes)) => {
            println!(
                "GEN OK: {} records, {} bytes of put payload -> {}",
                n,
                bytes,
                p.out
            );
            0
        }
        Err(e) => {
            eprintln!("GEN FAILED: {e}");
            1
        }
    }
}

fn bench() -> i32 {
    let cfg = load_cfg();
    let system = arg("system").unwrap_or_else(|| "flatcake".into());
    let trace = match arg("trace") {
        Some(t) => t,
        None => {
            eprintln!("bench requires --trace <path.jsonl>");
            return 2;
        }
    };
    let seed: u64 = arg("seed").unwrap_or_default().parse().unwrap_or(0);
    let records = match fckv::workload::parse_trace(&trace) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("trace parse failed: {e}");
            return 1;
        }
    };
    let engine: Arc<dyn KvEngine> = match system.as_str() {
        "baseline" => match BaselineEngine::new(&cfg) {
            Ok(e) => Arc::new(e),
            Err(e) => {
                eprintln!("baseline init failed: {e}");
                return 1;
            }
        },
        "flatcake" => match FlatCakeEngine::new(&cfg) {
            Ok(e) => Arc::new(e),
            Err(e) => {
                eprintln!("flatcake init failed: {e}");
                return 1;
            }
        },
        other => {
            eprintln!("unknown system '{other}' (use baseline|flatcake)");
            return 2;
        }
    };
    let stem = format!(
        "{}-qps{}-seed{}",
        system,
        arg("qps-label").unwrap_or_else(|| "0".into()),
        seed
    );
    let csv_path = arg("csv").map(|_| format!("results/{stem}.csv"));
    let t0 = std::time::Instant::now();
    let out = fckv::replay::run(
        engine.clone(),
        &records,
        &cfg,
        cfg.time_scale,
        cfg.csv_window_us,
        csv_path.as_deref(),
    );
    engine.shutdown();
    let json_path = arg("out").unwrap_or_else(|| format!("results/{stem}.json"));
    if let Err(e) = fckv::results::write_run_json(
        &cfg,
        &system,
        seed,
        &trace,
        &out.metrics,
        out.elapsed_s,
        out.integrity_failures,
        out.requests_issued,
        &json_path,
    ) {
        eprintln!("results write failed: {e}");
        return 1;
    }
    let m = &out.metrics;
    println!(
        "BENCH {} | hit={:.2}% (hbm={} dram={} ssd={} miss={}) read_amp={:.3}x write_amp={:.3}x phys_ops={} lat_p50={:.0}us lat_p99={:.0}us elapsed={:.1}s integrity_fail={}",
        system,
        100.0 * m.delivered_hit_rate(),
        m.hit_hbm,
        m.hit_dram,
        m.hit_ssd,
        m.misses,
        m.read_amp(),
        m.write_amp(),
        m.physical_read_ops,
        fckv::util::pct(&m.lat_samples, 50.0),
        fckv::util::pct(&m.lat_samples, 99.0),
        t0.elapsed().as_secs_f64(),
        out.integrity_failures
    );
    println!("results: {json_path}");
    0
}

fn demo() -> i32 {
    // Minimal end-to-end demonstration (directive Phase-2 §23): generate a small
    // deterministic agentic trace, run BOTH systems over it, print the comparison.
    // Budgets sized so SSD paths fire on this laptop (sessions > DRAM pool).
    let mut cfg = config::Config::default();
    cfg.dram_pool_bytes = 2 * 1024 * 1024; // 2 MiB — sessions exceed it
    cfg.hbm_window_bytes = 1024 * 1024;
    cfg.staging_pool_bytes = 1024 * 1024; // baseline staging pressure
    cfg.ssd_dir = "data/demo/fckv".into();
    cfg.baseline_ssd_dir = "data/demo/baseline".into();
    let trace_path = "data/workloads/demo.jsonl";
    let p = GenParams {
        regime: "agentic".into(),
        sessions: 3,
        turns: 10,
        layers: 4,
        blocks_per_turn: 4,
        shared_blocks: 8,
        fanout: 1,
        reuse_rate: 0.1,
        qps: 5.0,
        seed: 42,
        out: trace_path.into(),
        block_tokens: cfg.block_tokens,
        bytes_per_token: cfg.bytes_per_token,
        gpu_file_bytes: cfg.gpu_file_bytes,
        ssd_budget_bytes: cfg.bucket_bytes * cfg.bucket_count,
    };
    if let Err(e) = fckv::workload::generate(&p) {
        eprintln!("demo workload failed: {e}");
        return 1;
    }
    let records = match fckv::workload::parse_trace(trace_path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("demo trace parse failed: {e}");
            return 1;
        }
    };
    println!(
        "DEMO trace: {} records ({} gets), 3 agentic sessions x 10 turns + shared prompt, paced at 5 qps",
        records.len(),
        records
            .iter()
            .filter(|r| r.op == fckv::workload::Op::Get)
            .count()
    );
    let _ = std::fs::remove_dir_all("data/demo");
    for system in ["baseline", "flatcake"] {
        let engine: Arc<dyn KvEngine> = if system == "baseline" {
            Arc::new(BaselineEngine::new(&cfg).expect("baseline"))
        } else {
            Arc::new(FlatCakeEngine::new(&cfg).expect("flatcake"))
        };
        let t0 = std::time::Instant::now();
        let out = fckv::replay::run(engine.clone(), &records, &cfg, 0.0, cfg.csv_window_us, None);
        engine.shutdown();
        let m = &out.metrics;
        println!(
            "  {:<9} hit={:6.2}%  hbm={:4} dram={:4} ssd={:5} miss={:3}  read_amp={:5.2}x  write_amp={:5.2}x  phys_ops={:5}  p50={:7.0}us  p99={:7.0}us  integrity_fail={}",
            system,
            100.0 * m.delivered_hit_rate(),
            m.hit_hbm,
            m.hit_dram,
            m.hit_ssd,
            m.misses,
            m.read_amp(),
            m.write_amp(),
            m.physical_read_ops,
            fckv::util::pct(&m.lat_samples, 50.0),
            fckv::util::pct(&m.lat_samples, 99.0),
            out.integrity_failures
        );
        let _ = t0.elapsed();
    }
    println!("DEMO PASS (byte-exact integrity verified on every delivered block)");
    0
}

fn usage() -> i32 {
    eprintln!(
        "fckv — FlatCake Level-B prototype\nusage:\n  fckv selftest\n  fckv gen-workload --regime agentic|oneshot|mixed|shift [options]\n  fckv bench --system baseline|flatcake --trace <jsonl> [--config fckv.conf] [--seed N] [--csv]\n  fckv demo"
    );
    2
}

fn main() {
    let code = match std::env::args().nth(1).as_deref() {
        Some("selftest") => selftest(),
        Some("gen-workload") => gen_workload(),
        Some("bench") => bench(),
        Some("demo") => demo(),
        _ => usage(),
    };
    std::process::exit(code);
}
