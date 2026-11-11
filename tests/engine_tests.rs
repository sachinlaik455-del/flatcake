//! P2-M5 vertical-slice integration tests: the complete FlatCake lifecycle through
//! the integrated engine (paper Fig. 3 end-to-end at Level B):
//! session → scope-direct placement → GVA → medium write → publish → lookup →
//! MAS dual-queue dispatch → coalesced bounded-window SSD read → byte-verified
//! delivery → safe reclamation → metrics.
//! Directive §12: integration must share ACTUAL state across components — these
//! tests drive the real engine, not module-by-module calls.

use fckv::config::Config;
use fckv::engine::FlatCakeEngine;
use fckv::engine_api::{GetSpec, KvEngine, PutSpec};
use fckv::types::PlacementSignals;
use fckv::util::payload_fill;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn engine_cfg(tag: &str) -> Config {
    let mut c = Config::default();
    // Tight budgets so every mechanism fires without self-defeating reclamation:
    // DRAM holds 16 blocks (shared prompt + recompute space stays under watermark),
    // SSD = 4 scope files of 256 KiB, hbm window 128 KiB.
    c.dram_pool_bytes = 128 * 1024;
    c.hbm_window_bytes = 128 * 1024;
    c.gpu_file_bytes = 256 * 1024;
    c.gpu_file_pool = 4;
    c.ssd_high_watermark = 0.8;
    c.ssd_low_watermark = 0.5;
    c.dram_high_watermark = 0.8;
    c.dram_low_watermark = 0.5;
    c.ssd_dir = format!("data/test/engine-{tag}-{}", std::process::id());
    c.queue_cap = 64;
    c.ssd_workers = 2;
    c.dram_workers = 2;
    c
}

/// PutSpec for `layers` x `n` blocks starting at start_blk.
fn put(session: u64, turn: u64, layers: u32, start_blk: u64, n: u32, seed: u64) -> PutSpec {
    PutSpec {
        session,
        turn,
        layers,
        start_blk,
        n_blk: n,
        shared: 0,
        reply_tokens: 500,
        ctx_tokens: layers as u64 * (start_blk + n as u64) * 256,
        seed,
    }
}

fn shared_put(layers: u32, n: u32, seed: u64) -> PutSpec {
    let mut p = put(1, 1, layers, 0, n, seed);
    p.shared = 1;
    p.reply_tokens = 0;
    p.ctx_tokens = layers as u64 * n as u64 * 256;
    p
}

fn keys_of(spec: &PutSpec, block_tokens: u64) -> Vec<fckv::types::BlockKey> {
    let mut v = Vec::new();
    for l in 0..spec.layers {
        for i in 0..spec.n_blk {
            v.push(fckv::types::BlockKey {
                session: spec.session,
                layer: l,
                token_start: (spec.start_blk + i as u64) * block_tokens,
            });
        }
    }
    v
}

#[test]
fn vertical_slice_end_to_end_dram_ssd_hybrid_and_reclaim() {
    let cfg = engine_cfg("e2e");
    let e = Arc::new(FlatCakeEngine::new(&cfg).unwrap());
    let bb = cfg.block_bytes();

    // 1. Shared prompt (session 1, shared=1 → refcount 2 → DRAM per Table 2 row 1).
    let sp = shared_put(2, 2, 100);
    e.put_batch(&sp).unwrap();

    // 2. Agentic session 2: ctx grows across turns → SSD scope (OQ-19 row 2 at scope
    //    creation via ctx, or turn-1 one-shot; either way SSD-bound once ctx ≥ L_agent).
    let s2t1 = put(2, 1, 2, 0, 2, 200);
    e.put_batch(&s2t1).unwrap();
    let s2t2 = put(2, 2, 2, 2, 2, 201);
    e.put_batch(&s2t2).unwrap();

    // 3. Turn-2 get: hybrid request — shared prompt (DRAM) + own history (SSD).
    //    Directive §12: DRAM hits must NOT perform SSD reads and vice versa; the
    //    request resolves both media in one call.
    let mut blocks = keys_of(&sp, cfg.block_tokens);
    blocks.extend(keys_of(&s2t1, cfg.block_tokens));
    blocks.extend(keys_of(&s2t2, cfg.block_tokens));
    let mut seeds = Vec::new();
    for _ in 0..(sp.layers * sp.n_blk) as usize {
        seeds.push(sp.seed);
    }
    for _ in 0..(s2t1.layers * s2t1.n_blk) as usize {
        seeds.push(s2t1.seed);
    }
    for _ in 0..(s2t2.layers * s2t2.n_blk) as usize {
        seeds.push(s2t2.seed);
    }
    let sig = PlacementSignals {
        session: 2,
        refcount: 1,
        turn: 2,
        reply_tokens: 500,
        ctx_tokens: 8 * 256,
    };
    let got = AtomicUsize::new(0);
    let gspec = GetSpec {
        id: 1,
        arrival_ts_us: 1000,
        blocks,
        seed_by_block: seeds,
        signals: sig,
    };
    e.get_request(&gspec, &mut |idx, bytes| {
        assert_eq!(bytes.len(), bb as usize);
        // Integrity: delivered bytes == deterministic pattern of the creating put.
        let key = gspec.blocks[idx];
        let mut expect = vec![0u8; bb as usize];
        payload_fill(key.session, key.layer, key.token_start, gspec.seed_by_block[idx], &mut expect);
        assert_eq!(bytes, expect, "vertical-slice integrity for block idx {idx}");
        got.fetch_add(1, Ordering::SeqCst);
    })
    .unwrap();
    assert_eq!(got.load(Ordering::SeqCst), gspec.blocks.len());

    let m = e.snapshot();
    // Integrity is asserted byte-exact inside the sink callbacks above.
    assert!(
        m.hit_dram >= 1,
        "shared prompt must be a DRAM hit; metrics: hbm={} dram={} ssd={} miss={} racing={}",
        m.hit_hbm,
        m.hit_dram,
        m.hit_ssd,
        m.misses,
        m.pending_racing_gets
    );
    assert!(m.hit_ssd >= 1, "session history must be served from SSD");
    // Coalescing observable (directive §12): the SSD restore of the 2-layer
    // prefix must be physically small, not per-block.
    assert!(
        m.physical_read_ops <= 4,
        "coalesced restore should need few physical reads, got {}",
        m.physical_read_ops
    );

    // 4. Second get of the same prefix → hbm window hits (delivered blocks resident).
    e.get_request(&gspec, &mut |_idx, _b| {}).unwrap();
    let m2 = e.snapshot();
    assert!(m2.hit_hbm > 0, "delivered blocks must be hbm-resident on re-read");

    // 5. Safe reclamation: fill SSD beyond the watermark, reclaim fires, blocks
    //    freed are unreferenced ones; a get of a reclaimed block recomputes with
    //    correct bytes (miss accounting, integrity preserved).
    for s in 3..12u64 {
        e.put_batch(&put(s, 1, 2, 0, 2, s * 77)).unwrap();
    }
    let m3 = e.snapshot();
    if m3.reclaim_events > 0 {
        assert_eq!(m3.reclaim_payload_ops, 0, "reclaim must be metadata-only");
    }
    // Session 2's scope may or may not have been reclaimed (oldest-first);
    // either way a get must return correct bytes via hit or recompute.
    e.get_request(&gspec, &mut |idx, bytes| {
        let key = gspec.blocks[idx];
        let mut expect = vec![0u8; bb as usize];
        payload_fill(key.session, key.layer, key.token_start, gspec.seed_by_block[idx], &mut expect);
        assert_eq!(bytes, expect, "post-reclaim integrity");
    })
    .unwrap();

    e.shutdown();
}

#[test]
fn engine_concurrent_mixed_sessions_no_loss_no_corruption() {
    let cfg = engine_cfg("conc");
    let e = Arc::new(FlatCakeEngine::new(&cfg).unwrap());
    let bb = cfg.block_bytes();
    let e2 = e.clone();
    // 8 threads: 4 writer sessions (agentic, SSD-bound), 4 reader loops hammering
    // the shared prompt + own prefixes. Integrity verified per delivery.
    e.put_batch(&shared_put(2, 2, 900)).unwrap();
    let mut handles = Vec::new();
    for s in 2..6u64 {
        let e = e2.clone();
        handles.push(std::thread::spawn(move || {
            for t in 1..4u64 {
                e.put_batch(&put(s, t, 2, (t - 1) * 2, 2, s * 1000 + t)).unwrap();
            }
        }));
    }
    for _ in 0..4 {
        let e = e2.clone();
        handles.push(std::thread::spawn(move || {
            for _ in 0..6 {
                let mut blocks = keys_of(&shared_put(2, 2, 900), bb);
                let mut seeds = vec![900u64; blocks.len()];
                blocks.extend(keys_of(&put(2, 2, 2, 2, 2, 2002), bb));
                seeds.extend(vec![2002u64; 4]);
                let gspec = GetSpec {
                    id: rand_id(),
                    arrival_ts_us: 0,
                    blocks,
                    seed_by_block: seeds,
                    signals: PlacementSignals {
                        session: 2,
                        refcount: 1,
                        turn: 2,
                        reply_tokens: 500,
                        ctx_tokens: 8192,
                    },
                };
                e.get_request(&gspec, &mut |idx, bytes| {
                    let key = gspec.blocks[idx];
                    let mut expect = vec![0u8; bb as usize];
                    payload_fill(key.session, key.layer, key.token_start, gspec.seed_by_block[idx], &mut expect);
                    assert_eq!(bytes, expect);
                })
                .unwrap();
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
        "accounting mismatch: hbm={} dram={} ssd={} miss={} racing={} requested={}",
        m.hit_hbm,
        m.hit_dram,
        m.hit_ssd,
        m.misses,
        m.pending_racing_gets,
        m.requested_blocks
    );
    e.shutdown();
}

fn rand_id() -> u64 {
    use std::sync::atomic::AtomicU64;
    static N: AtomicU64 = AtomicU64::new(1);
    N.fetch_add(1, Ordering::SeqCst)
}
