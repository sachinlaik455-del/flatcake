//! fckv — FlatCake Level-B prototype (bin). Library code lives in src/lib.rs.
//! Subcommands: gen-workload (M3), selftest; demo/bench land at M7/M9.

use fckv::config;
use fckv::types::{Gva, SSD_FILE_MAX};
use fckv::workload::GenParams;

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

fn usage() -> i32 {
    eprintln!(
        "fckv — FlatCake Level-B prototype\nusage: fckv selftest | fckv gen-workload --regime agentic|oneshot|mixed [options]"
    );
    2
}

fn main() {
    let code = match std::env::args().nth(1).as_deref() {
        Some("selftest") => selftest(),
        Some("gen-workload") => gen_workload(),
        _ => usage(),
    };
    std::process::exit(code);
}
