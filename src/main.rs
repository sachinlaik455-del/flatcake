//! fckv — FlatCake Level-B prototype (Phase 0 skeleton).
//! Subcommands land per milestone: gen-workload (M3), demo/bench (M7/M9).

mod config;
mod types;
mod util;

use types::{Gva, SSD_FILE_MAX};

fn selftest() -> i32 {
    // GVA round-trip + boundaries + invalid values (T1 semantics).
    let cases = [
        (types::MEDIA_DRAM, 0u64, 0u64),
        (types::MEDIA_DRAM, 4095, 0xFFFF_FFFF_FFFF),
        (types::MEDIA_SSD, 0, SSD_FILE_MAX - 1),
        (types::MEDIA_SSD, 4095, 0xFFFF_FFFF_FFFF),
    ];
    for (m, n, o) in cases {
        let g = Gva::encode(m, n, o).expect("valid encode");
        assert_eq!(g.media(), m, "media round-trip");
        assert_eq!(g.node(), n, "node round-trip");
        assert_eq!(g.offset(), o, "offset round-trip");
        assert_eq!(g.is_ssd(), m == types::MEDIA_SSD, "is_ssd classification");
    }
    assert!(Gva::encode(2, 0, 0).is_none(), "media >= 2 reserved");
    assert!(Gva::encode(0, 0x1000, 0).is_none(), "node > 12 bits");
    assert!(Gva::encode(0, 0, 1 << 48).is_none(), "offset > 48 bits");

    // Config validation.
    let cfg = config::Config::default();
    assert_eq!(cfg.block_bytes(), 8192);
    assert_eq!(cfg.gpu_file_bytes, SSD_FILE_MAX);
    assert!(config::load("does-not-exist.conf").is_err());

    println!("SELFTEST PASS");
    0
}

fn usage() -> i32 {
    eprintln!("fckv — FlatCake Level-B prototype\nusage: fckv selftest");
    2
}

fn main() {
    let code = match std::env::args().nth(1).as_deref() {
        Some("selftest") => selftest(),
        _ => usage(),
    };
    std::process::exit(code);
}
