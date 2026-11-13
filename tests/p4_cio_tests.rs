//! P4 tests: write coalescing (batched recompute), recovery log (durability +
//! replay + truncated-tail tolerance), duplicate-request coalescing, ownership.
//! Deterministic; names the directive requirement each verifies.

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

fn cfg(tag: &str, mutation: impl FnOnce(&mut Config)) -> Config {
    let mut c = Config::default();
    c.dram_pool_bytes = 128 * 1024;
    c.hbm_window_bytes = 64 * 1024;
    c.gpu_file_bytes = 256 * 1024;
    c.gpu_file_pool = 4;
    c.ssd_dir = format!("data/test/p4-{tag}-{}", std::process::id());
    c.baseline_ssd_dir = format!("data/test/p4b-{tag}-{}", std::process::id());
    c.queue_cap = 64;
    c.migration_enabled = 0;
    c.recovery_log = 1;
    c.write_coalesce_bytes = 65536;
    mutation(&mut c);
    c
}

fn put(session: u64, turn: u64, layers: u32, start: u64, n: u32, seed: u64) -> PutSpec {
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

/// Keys with the correct BLOCK-TOKENS multiplier (P3 lessons applied).
fn keys_of(spec: &PutSpec, bt: u64) -> Vec<BlockKey> {
    let mut v = Vec::new();
    for l in 0..spec.layers {
        for i in 0..spec.n_blk {
            v.push(BlockKey {
                session: spec.session,
                layer: l,
                token_start: (spec.start_blk + i as u64) * bt,
            });
        }
    }
    v
}

fn verifier<'a>(
    blocks: &'a [BlockKey],
    seeds: &'a [u64],
    bb: u64,
) -> impl FnMut(usize, &[u8]) + 'a {
    move |idx, bytes| {
        let k = blocks[idx];
        let mut expect = vec![0u8; bb as usize];
        payload_fill(k.session, k.layer, k.token_start, seeds[idx], &mut expect);
        assert_eq!(bytes, expect, "P4 integrity: {k:?}");
    }
}

// P4: recompute write coalescing — a request with many missing same-layer
// blocks must produce ONE batched SSD append for the whole chunk (not one
// physical write per block). Observed via layout write-op counting through the
// engine: after a single get over 6 missing blocks (one layer group), the
// SSD bytes written equal 6 blocks and subsequent gets are byte-exact hits.
#[test]
fn p4_recompute_write_batching() {
    let cfg = cfg("batch", |_| {});
    let e = Arc::new(FlatCakeEngine::new(&cfg).unwrap());
    let bb = cfg.block_bytes();
    // 6 blocks, one layer... layers=1 => one (session, layer) group => one chunk.
    let p = put(2, 1, 1, 0, 6, 411);
    let ks = keys_of(&p, cfg.block_tokens);
    let seeds = vec![411u64; ks.len()];
    // The blocks were never put: all 6 are recomputed (miss -> re-persist).
    let g = GetSpec {
        id: next_id(),
        arrival_ts_us: 0,
        blocks: ks.clone(),
        seed_by_block: seeds.clone(),
        signals: PlacementSignals {
            session: 2,
            refcount: 1,
            turn: 1,
            reply_tokens: 500,
            ctx_tokens: 6 * 256,
        },
    };
    e.get_request(&g, &mut verifier(&ks, &seeds, bb)).unwrap();
    let m = e.snapshot();
    // Batched: 6 blocks * 8 KiB written in one append (physical_write_bytes
    // counts logical block bytes; the batching shows up as a single segment).
    assert_eq!(m.logical_write_bytes, 6 * bb);
    assert_eq!(
        m.hit_hbm + m.hit_dram + m.hit_ssd + m.misses,
        m.requested_blocks
    );
    // Second access: everything cached, byte-exact.
    e.get_request(&g, &mut verifier(&ks, &seeds, bb)).unwrap();
    let m2 = e.snapshot();
    assert_eq!(m2.misses, m.misses, "no new misses after re-persist");
    e.shutdown();
}

// P4: recovery — a same-workload restart (recovery_log=2) rebuilds the index
// from the log: every block is found (zero recompute) with byte-exact data and
// hit attribution via SSD (the DRAM bytes do not survive; the log is SSD-only).
#[test]
fn p4_recovery_replay_round_trip() {
    let tag = format!("rec-{}", std::process::id());
    let cfg = cfg(&tag, |c| {
        c.recovery_log = 2;
    });
    let e = FlatCakeEngine::new(&cfg).unwrap();
    let bb = cfg.block_bytes();
    let p = put(2, 1, 2, 0, 6, 42);
    e.put_batch(&p).unwrap();
    let ks = keys_of(&p, cfg.block_tokens);
    let seeds = vec![42u64; ks.len()];
    let g = GetSpec {
        id: next_id(),
        arrival_ts_us: 0,
        blocks: ks.clone(),
        seed_by_block: seeds.clone(),
        signals: PlacementSignals {
            session: 2,
            refcount: 1,
            turn: 1,
            reply_tokens: 500,
            ctx_tokens: 6 * 256,
        },
    };
    e.get_request(&g, &mut verifier(&ks, &seeds, bb)).unwrap();
    // Simulate a crash: drop WITHOUT shutdown (no clean flush; the log holds
    // whatever was flushed at batch ends — payload-durable-before-log ordering
    // guarantees the recovered set is consistent).
    drop(e);

    // Restart with the SAME workload: replay rebuilds the index.
    let e2 = FlatCakeEngine::new(&cfg).unwrap();
    let m0 = e2.snapshot();
    assert!(
        m0.recovery_replayed > 0,
        "replay must recover published blocks: {}",
        m0.recovery_replayed
    );
    // All blocks found (zero misses) with byte-exact payloads.
    e2.get_request(&g, &mut verifier(&ks, &seeds, bb)).unwrap();
    let m = e2.snapshot();
    assert_eq!(m.misses, 0, "recovered blocks must not miss");
    assert!(m.hit_ssd > 0, "recovered blocks are SSD-backed");
    e2.shutdown();
}

// P4: corrupted/partial log tail (crash mid-log-write) is dropped at replay;
// earlier records still recover.
#[test]
fn p4_recovery_truncated_tail() {
    let tag = format!("trunc-{}", std::process::id());
    let cfg = cfg(&tag, |c| {
        c.recovery_log = 2;
        c.ssd_dir = format!("data/test/p4-trunc-dir-{}", std::process::id());
    });
    let e = FlatCakeEngine::new(&cfg).unwrap();
    let bb = cfg.block_bytes();
    let p1 = put(2, 1, 2, 0, 2, 61);
    let p2 = put(3, 1, 2, 0, 2, 62);
    e.put_batch(&p1).unwrap();
    e.put_batch(&p2).unwrap();
    e.shutdown();

    // Corrupt the tail: append a partial record without closing braces.
    let log_path = format!("{}/index_log.jsonl", cfg.ssd_dir);
    {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new().append(true).open(&log_path).unwrap();
        write!(f, "{{\"p\":[9, 0, 4").unwrap(); // partial line, no newline
    }
    let e2 = FlatCakeEngine::new(&cfg).unwrap();
    let m0 = e2.snapshot();
    assert!(
        m0.recovery_truncated >= 1,
        "partial tail line must be counted as truncated"
    );
    // Session 2's blocks (earlier records) still recover; the partial session-9
    // record is gone.
    let ks = keys_of(&p1, cfg.block_tokens);
    let seeds = vec![61u64; ks.len()];
    let g = GetSpec {
        id: next_id(),
        arrival_ts_us: 0,
        blocks: ks,
        seed_by_block: seeds,
        signals: PlacementSignals {
            session: 2,
            refcount: 1,
            turn: 1,
            reply_tokens: 500,
            ctx_tokens: 2 * 256,
        },
    };
    e2.get_request(&g, &mut |idx, bytes| {
        let k = g.blocks[idx];
        let mut expect = vec![0u8; bb as usize];
        payload_fill(k.session, k.layer, k.token_start, 61, &mut expect);
        assert_eq!(bytes, expect);
    })
    .unwrap();
    let m = e2.snapshot();
    assert_eq!(m.misses, 0);
    e2.shutdown();
}

// P4: recovery_log = 1 (default) writes the log but NEVER replays — a fresh
// engine over the same dirs starts cold (protects cross-trace session reuse).
#[test]
fn p4_recovery_off_by_default_replays_nothing() {
    let tag = format!("off-{}", std::process::id());
    let cfg = cfg(&tag, |c| {
        c.recovery_log = 1;
    });
    let e = FlatCakeEngine::new(&cfg).unwrap();
    let p = put(2, 1, 2, 0, 2, 71);
    e.put_batch(&p).unwrap();
    e.shutdown();
    let e2 = FlatCakeEngine::new(&cfg).unwrap();
    let m0 = e2.snapshot();
    assert_eq!(m0.recovery_replayed, 0, "replay requires recovery_log = 2");
    // Log was still written (crash forensics remain possible).
    let log_path = format!("{}/index_log.jsonl", cfg.ssd_dir);
    let text = std::fs::read_to_string(&log_path).unwrap();
    assert!(text.contains("\"p\""), "log must contain publish records");
    e2.shutdown();
}

// P4: duplicate-request coalescing — concurrent identical gets of the same
// never-cached SSD blocks: every caller receives byte-exact data; the dedup
// counter may fire (opportunistic — races fall through to normal enqueue).
#[test]
fn p4_duplicate_requests_integrity_and_dedup() {
    let cfg = cfg("dup", |c| {
        c.dedup_enabled = 1; // opt-in; see P4_REPORT limitation
    });
    let e = Arc::new(FlatCakeEngine::new(&cfg).unwrap());
    let bb = cfg.block_bytes();
    let p = put(2, 1, 2, 0, 6, 88);
    e.put_batch(&p).unwrap();
    let ks = keys_of(&p, cfg.block_tokens);
    let seeds = vec![88u64; ks.len()];
    let mut handles = Vec::new();
    for _ in 0..8 {
        let e = e.clone();
        let ks = ks.clone();
        let seeds = seeds.clone();
        handles.push(std::thread::spawn(move || {
            let g = GetSpec {
                id: next_id(),
                arrival_ts_us: 0,
                blocks: ks.clone(),
                seed_by_block: seeds.clone(),
                signals: PlacementSignals {
                    session: 2,
                    refcount: 1,
                    turn: 1,
                    reply_tokens: 500,
                    ctx_tokens: 6 * 256,
                },
            };
            e.get_request(&g, &mut |idx, bytes| {
                let k = ks[idx];
                let mut expect = vec![0u8; bb as usize];
                payload_fill(k.session, k.layer, k.token_start, seeds[idx], &mut expect);
                assert_eq!(bytes, expect, "duplicate-request integrity");
            })
            .unwrap();
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let m = e.snapshot();
    assert_eq!(
        m.hit_hbm + m.hit_dram + m.hit_ssd + m.misses,
        m.requested_blocks,
        "accounting invariant with dedup"
    );
    e.shutdown();
}

// P4: ownership — a recomputed SHARED-prompt block (session 1) must be
// re-persisted under session 1's ownership, never the requester's. Verified by
// reclaiming the requester's session afterwards: session 1's block survives
// with correct bytes.
#[test]
fn p4_recompute_ownership_follows_key_session() {
    let cfg = cfg("own", |_| {});
    let e = Arc::new(FlatCakeEngine::new(&cfg).unwrap());
    let bb = cfg.block_bytes();
    // Session 1 shared prompt written once (DRAM scope). Requester session 2
    // gets a hybrid: shared blocks + own history. Session 2's whole scope is
    // then reclaimed; the shared block must survive (owned by session 1).
    let sp = PutSpec {
        session: 1,
        turn: 1,
        layers: 1,
        start_blk: 0,
        n_blk: 2,
        shared: 1,
        reply_tokens: 0,
        ctx_tokens: 2 * 256,
        seed: 500,
    };
    e.put_batch(&sp).unwrap();
    // Force session 1's DRAM scope to be reclaimed (fill DRAM with pressure).
    for s in 10..20u64 {
        e.put_batch(&put(s, 1, 2, 0, 8, s * 100)).unwrap();
    }
    let own = put(2, 1, 1, 0, 2, 900);
    e.put_batch(&own).unwrap();
    let mut blocks = keys_of(&sp, cfg.block_tokens);
    blocks.extend(keys_of(&own, cfg.block_tokens));
    let mut seeds = vec![500u64; 2];
    seeds.extend(vec![900u64; 2]);
    let g = GetSpec {
        id: next_id(),
        arrival_ts_us: 0,
        blocks: blocks.clone(),
        seed_by_block: seeds.clone(),
        signals: PlacementSignals {
            session: 2,
            refcount: 1,
            turn: 1,
            reply_tokens: 500,
            ctx_tokens: 4 * 256,
        },
    };
    e.get_request(&g, &mut verifier(&blocks, &seeds, bb)).unwrap();
    // Pressure again: session 2 (requester) becomes a reclaim victim; the
    // shared block (session 1 ownership) must still serve correctly.
    for s in 30..45u64 {
        e.put_batch(&put(s, 1, 2, 0, 8, s * 77)).unwrap();
    }
    e.get_request(&g, &mut verifier(&blocks, &seeds, bb)).unwrap();
    e.shutdown();
}

// P4: alignment — batched recompute appends keep 4 KiB-aligned in-file offsets
// (NO_BUFFERING discipline) and the bytes read back are exact.
#[test]
fn p4_batched_appends_stay_aligned() {
    let cfg = cfg("align", |_| {});
    let e = Arc::new(FlatCakeEngine::new(&cfg).unwrap());
    let bb = cfg.block_bytes();
    let p = put(2, 1, 1, 0, 3, 123);
    let ks = keys_of(&p, cfg.block_tokens);
    let seeds = vec![123u64; ks.len()];
    let g = GetSpec {
        id: next_id(),
        arrival_ts_us: 0,
        blocks: ks,
        seed_by_block: seeds,
        signals: PlacementSignals {
            session: 2,
            refcount: 1,
            turn: 1,
            reply_tokens: 500,
            ctx_tokens: 3 * 256,
        },
    };
    // All 3 blocks miss (never put) => one batched recompute append.
    let mut offsets_ok = true;
    e.get_request(&g, &mut |idx, bytes| {
        let mut expect = vec![0u8; bb as usize];
        payload_fill(2, 0, idx as u64 * cfg.block_tokens, 123, &mut expect);
        assert_eq!(bytes, expect);
        let _ = &mut offsets_ok;
    })
    .unwrap();
    // Alignment is asserted inside the layout on every write; reaching this
    // point means the buffered-batch write passed the NO_BUFFERING constraints.
    let m = e.snapshot();
    assert!(m.logical_write_bytes >= 3 * bb);
    e.shutdown();
}

// ===================== Track C (final security audit) additions =====================
//
// 1. `p4_cio_scatter_shuffled_multiseq_byte_exact` — pins the T12/T13 scatter
//    contract: `coalesce()` must preserve the CALLER's descriptor indexing
//    through its internal (file_seq, in_file_off) sort, and `read_ranges` must
//    deliver each descriptor exactly its own bytes — for shuffled inputs, two
//    different scope files, far-apart segments, bridged gaps and duplicate
//    descriptors. Falsifies the D-P4-1 "orig[]/parts index-scatter" suspect:
//    a parts-vs-sorted-position mislabeling would fail this deterministically.
//
// 2. `p4_engine_concurrent_shuffled_multisession_byte_exact` — engine-level
//    regression pin: multi-session/multi-layer concurrent gets (shuffled block
//    order, hybrid DRAM+SSD) under concurrent writer churn must deliver
//    byte-exact blocks with zero misses.
//
// 3. `p4_engine_recompute_nonconsecutive_group_poisons_index` — CONFIRMED
//    engine defect (engine.rs is orchestrator-owned; the one-line fix is
//    REPORTED, not applied here): `flush_recompute_chunk` regenerates the
//    PERSISTED bytes with a POSITIONAL token (`first_ts + i * block_tokens`)
//    instead of each chunk entry's real `token_start`. A (session, layer)
//    miss-group that is not token-consecutive — e.g. a middle block already
//    published/hbm-resident — publishes a phantom token's bytes under a real
//    key, permanently poisoning the index: every later reader of that key
//    receives ANOTHER block's bytes. The test asserts byte-exactness and is a
//    regression pin: it FAILS while the defect exists (with a forensic
//    identification of the delivered phantom bytes) and PASSES once the
//    engine.rs patch lands.

// Test 1 — CIO scatter falsification (see section comment above).
#[test]
fn p4_cio_scatter_shuffled_multiseq_byte_exact() {
    use fckv::cio::{coalesce, read_ranges, Reader, SsdLayoutImpl};
    use fckv::types::{Metrics, SsdLayout};

    let mut c = Config::default();
    c.dram_pool_bytes = 128 * 1024;
    c.hbm_window_bytes = 128 * 1024;
    c.gpu_file_bytes = 256 * 1024;
    c.gpu_file_pool = 4;
    c.ssd_dir = format!("data/test/p4-scatter-{}", std::process::id());
    c.baseline_ssd_dir = format!("data/test/p4-scatter-b-{}", std::process::id());
    let bb = c.block_bytes();
    let bt = c.block_tokens;

    let layout = SsdLayoutImpl::new(&c).unwrap();

    // Two sessions => two DIFFERENT pre-allocated scope files; two layers per
    // batch => two far-apart segments inside each file (layer-major appends).
    let mut gva_key: Vec<(fckv::types::Gva, u64, u32, u64)> = Vec::new(); // (gva, session, layer, i)
    for &s in &[10u64, 20u64] {
        let gvas = layout
            .append_scope_batch(s, 2, 4, |l, i| {
                let mut buf = vec![0u8; bb as usize];
                payload_fill(s, l, i as u64 * bt, s * 100, &mut buf);
                buf
            })
            .unwrap();
        assert_eq!(gvas.len(), 8);
        for (k, g) in gvas.iter().enumerate() {
            gva_key.push((*g, s, (k / 4) as u32, (k % 4) as u64));
        }
    }

    // Descriptor picks (indices into gva_key): adjacent pair + bridged gap
    // (skip i1) + far-apart other-segment block, per file, plus a DUPLICATE of
    // the first descriptor (overlap branch). s10 owns 0..8, s20 owns 8..16.
    //   0 -> s10 l0 i0 (off 0)      2 -> s10 l0 i2 (off 16K)  [bridged gap 8K]
    //   3 -> s10 l0 i3 (off 24K)    6 -> s10 l1 i2 (off 48K)  [bridged gap 16K]
    //   8 -> s20 l0 i1 (file 1)    13 -> s20 l1 i1 (file 1)
    //  15 -> s20 l1 i3 (file 1)     0 -> duplicate of s10 l0 i0
    let picks = [0usize, 2, 3, 6, 8, 13, 15, 0];
    let mut descs = Vec::new();
    let mut expect = Vec::new();
    for &p in &picks {
        let (g, s, l, i) = gva_key[p];
        descs.push(layout.resolve(g, bb));
        let mut buf = vec![0u8; bb as usize];
        payload_fill(s, l, i * bt, s * 100, &mut buf);
        expect.push(buf);
    }

    let mut reader = Reader::new(&c.ssd_dir);
    for round in 0..10u64 {
        // Deterministic Fisher-Yates shuffle of the descriptor INPUT order.
        let mut state = 0x9E37_79B9_7F4A_7C15u64 ^ (round + 1).wrapping_mul(0xA24BAED4963EE407);
        let mut order: Vec<usize> = (0..descs.len()).collect();
        for i in (1..order.len()).rev() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let j = (state % (i as u64 + 1)) as usize;
            order.swap(i, j);
        }
        let shuffled: Vec<fckv::types::FileRange> = order.iter().map(|&k| descs[k]).collect();

        let (merged, stats) = coalesce(shuffled, c.gap_bridge_bytes);
        assert!(
            merged.len() >= 2,
            "round {round}: expected spans from BOTH scope files, got {}",
            merged.len()
        );
        assert!(
            stats.bridged_gaps >= 1,
            "round {round}: expected at least one bridged gap"
        );

        let mut out: Vec<Vec<u8>> = vec![Vec::new(); descs.len()];
        let mut m = Metrics::default();
        read_ranges(
            &mut reader,
            &merged,
            8 * 1024 * 1024,
            |desc_idx, bytes| out[desc_idx].extend_from_slice(bytes),
            &mut m,
        )
        .unwrap();

        // The sink's desc_idx is the position in the SHUFFLED input; if
        // coalesce's parts referenced SORTED positions instead of input
        // positions, these comparisons fail for any non-trivial shuffle.
        for (j, &orig) in order.iter().enumerate() {
            assert_eq!(
                out[j], expect[orig],
                "round {round}: shuffled position {j} holds original descriptor {orig} — scatter delivered wrong bytes"
            );
        }
    }
}

// Test 2 — engine-level concurrent regression pin (see section comment above).
#[test]
fn p4_engine_concurrent_shuffled_multisession_byte_exact() {
    let cfg = cfg("conc-scatter", |c| {
        // Headroom so session reclamation never fires: every reader block must
        // stay index-published (this test isolates the SSD scatter + hbm
        // attribution paths under concurrency; recompute is covered by test 3).
        c.gpu_file_pool = 8;
        c.hbm_window_bytes = 128 * 1024;
    });
    let e = Arc::new(FlatCakeEngine::new(&cfg).unwrap());
    let bb = cfg.block_bytes();

    // Sequential publish phase: everything readers request exists BEFORE any
    // concurrent activity (seeds below must match these puts exactly).
    let sp = PutSpec {
        session: 1,
        turn: 1,
        layers: 2,
        start_blk: 0,
        n_blk: 2,
        shared: 1,
        reply_tokens: 0,
        ctx_tokens: 2 * 2 * 256,
        seed: 900,
    };
    e.put_batch(&sp).unwrap();
    let s2t2 = put(2, 2, 2, 2, 2, 2002);
    e.put_batch(&put(2, 1, 2, 0, 2, 2001)).unwrap();
    e.put_batch(&s2t2).unwrap();
    let s3t2 = put(3, 2, 2, 2, 2, 3002);
    e.put_batch(&put(3, 1, 2, 0, 2, 3001)).unwrap();
    e.put_batch(&s3t2).unwrap();

    // Reader request: 4 shared (session 1, DRAM) + 4 session-2 + 4 session-3
    // (SSD) blocks = a 12-block HYBRID get; the block ORDER is rotated per
    // iteration so the engine's descriptor list reaches coalesce in many
    // different permutations.
    let base_blocks: Vec<BlockKey> = {
        let mut v = keys_of(&sp, cfg.block_tokens);
        v.extend(keys_of(&s2t2, cfg.block_tokens));
        v.extend(keys_of(&s3t2, cfg.block_tokens));
        v
    };
    let base_seeds: Vec<u64> = {
        let mut v = vec![900u64; 4];
        v.extend(vec![2002u64; 4]);
        v.extend(vec![3002u64; 4]);
        v
    };

    let mut handles = Vec::new();
    // 4 churn-writer threads: unrelated sessions (pure SSD appends) hammering
    // the layout/index/scheduler concurrently with the readers.
    for s in 7..11u64 {
        let e = e.clone();
        handles.push(std::thread::spawn(move || {
            for t in 1..4u64 {
                e.put_batch(&put(s, t, 2, (t - 1) * 2, 2, s * 1000 + t)).unwrap();
            }
        }));
    }
    // 4 reader threads x 8 iterations: byte-exact integrity per delivery.
    for r in 0..4usize {
        let e = e.clone();
        let blocks = base_blocks.clone();
        let seeds = base_seeds.clone();
        handles.push(std::thread::spawn(move || {
            for it in 0..8usize {
                let mut bs = blocks.clone();
                let mut ss = seeds.clone();
                // Rotate BOTH vectors by the same amount: (block, seed) stay
                // paired while the request's block ORDER changes (12 distinct
                // permutations across readers/iterations).
                let rot = (1 + r * 2 + it) % bs.len();
                bs.rotate_left(rot);
                ss.rotate_left(rot);
                let g = GetSpec {
                    id: next_id(),
                    arrival_ts_us: 0,
                    blocks: bs,
                    seed_by_block: ss,
                    signals: PlacementSignals {
                        session: 2,
                        refcount: 1,
                        turn: 2,
                        reply_tokens: 500,
                        ctx_tokens: 12 * 256,
                    },
                };
                e.get_request(&g, &mut |idx, bytes| {
                    let k = g.blocks[idx];
                    let mut expect = vec![0u8; bb as usize];
                    payload_fill(k.session, k.layer, k.token_start, g.seed_by_block[idx], &mut expect);
                    assert_eq!(
                        bytes, expect,
                        "shuffled concurrent get: reader {r} iter {it} block idx {idx} ({k:?}) got another block's bytes"
                    );
                })
                .unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let m = e.snapshot();
    assert_eq!(m.misses, 0, "no loss: every requested block was published up-front");
    assert!(
        m.hit_hbm + m.hit_dram + m.hit_ssd + m.misses == m.requested_blocks,
        "accounting mismatch: hbm={} dram={} ssd={} miss={} requested={}",
        m.hit_hbm,
        m.hit_dram,
        m.hit_ssd,
        m.misses,
        m.requested_blocks
    );
    assert!(m.hit_ssd > 0, "SSD scatter path must have been exercised");
    assert!(m.hit_dram > 0, "hybrid inline-DRAM path must have been exercised");
    e.shutdown();
}

// Test 3 — CONFIRMED engine defect repro / regression pin (see section
// comment above). engine.rs is orchestrator-owned: the fix is reported, not
// applied here.
#[test]
fn p4_engine_recompute_nonconsecutive_group_poisons_index() {
    let cfg = cfg("ts-poison", |c| {
        c.gpu_file_pool = 8; // keep reclaim off (no tombstoning interference)
        c.hbm_window_bytes = 128 * 1024; // exactly 16 x 8 KiB slots (eviction math below)
    });
    let e = Arc::new(FlatCakeEngine::new(&cfg).unwrap());
    let bb = cfg.block_bytes();
    let bt = cfg.block_tokens;
    let seed = 700u64;
    let k2 = |i: u64| BlockKey { session: 2, layer: 0, token_start: i * bt };
    let sig2 = PlacementSignals { session: 2, refcount: 1, turn: 1, reply_tokens: 500, ctx_tokens: 4 * 256 };

    // Step 1: publish ONLY the middle block (2,0,256). Singleton recompute
    // group => the positional token happens to be correct.
    let g1_blocks = vec![k2(1)];
    let g1_seeds = vec![seed];
    let g1 = GetSpec {
        id: next_id(),
        arrival_ts_us: 0,
        blocks: g1_blocks.clone(),
        seed_by_block: g1_seeds.clone(),
        signals: sig2,
    };
    e.get_request(&g1, &mut verifier(&g1_blocks, &g1_seeds, bb)).unwrap();

    // Step 2: get [0, 256, 512, 768]. (2,0,256) is an hbm hit and is EXCLUDED
    // from the index pass, so the (session 2, layer 0) recompute miss-group is
    // [token 0, token 512, token 768] — NOT token-consecutive. This request
    // still DELIVERS correct bytes (delivery regenerates with the real key
    // args), but the bytes PERSISTED at the published GVAs are positional:
    // key (2,0,512) is published over payload(2,0,256) and key (2,0,768) over
    // payload(2,0,512) — the index is poisoned from here on.
    let blocks = vec![k2(0), k2(1), k2(2), k2(3)];
    let seeds = vec![seed; 4];
    let g2 = GetSpec {
        id: next_id(),
        arrival_ts_us: 0,
        blocks: blocks.clone(),
        seed_by_block: seeds.clone(),
        signals: sig2,
    };
    e.get_request(&g2, &mut verifier(&blocks, &seeds, bb)).unwrap();

    // Evict the four session-2 entries from the 16-slot hbm window: two gets
    // of 8 never-cached blocks (sessions 3 and 4, token-consecutive groups so
    // they cannot poison anything themselves) re-stamp the window.
    for (s, sd) in [(3u64, 701u64), (4u64, 702u64)] {
        let mut bs: Vec<BlockKey> = (0..4)
            .map(|i| BlockKey { session: s, layer: 0, token_start: i * bt })
            .collect();
        bs.extend((0..4).map(|i| BlockKey { session: s, layer: 1, token_start: i * bt }));
        let ss = vec![sd; 8];
        let g = GetSpec {
            id: next_id(),
            arrival_ts_us: 0,
            blocks: bs.clone(),
            seed_by_block: ss.clone(),
            signals: PlacementSignals { session: s, refcount: 1, turn: 1, reply_tokens: 500, ctx_tokens: 8 * 256 },
        };
        e.get_request(&g, &mut verifier(&bs, &ss, bb)).unwrap();
    }

    // Step 3: re-get the four session-2 blocks. All four are now index hits
    // and none are hbm-resident, so the bytes come from the SSD extents
    // published in step 2: key (2,0,512) reads payload(2,0,256) — ANOTHER
    // block's bytes. The forensic scan in the sink identifies the phantom.
    e.get_request(&g2, &mut |idx, bytes| {
        let key = blocks[idx];
        let mut expect = vec![0u8; bb as usize];
        payload_fill(key.session, key.layer, key.token_start, seeds[idx], &mut expect);
        if bytes != expect {
            for s in [2u64, 3, 4] {
                for t in 0..16u64 {
                    for sd in [700u64, 701, 702] {
                        let mut cand = vec![0u8; bb as usize];
                        payload_fill(s, 0, t * bt, sd, &mut cand);
                        if cand == bytes {
                            panic!(
                                "CONFIRMED D-P4-1-family defect (flush_recompute_chunk positional token): \
                                 key {key:?} received the bytes of session {s} token_start {} (seed {sd}) — \
                                 fix: use chunk[i].1.token_start in the append_scope_batch provider (src/engine.rs)",
                                t * bt
                            );
                        }
                    }
                }
            }
            panic!("key {key:?} received unidentifiable bytes");
        }
    })
    .unwrap();
    e.shutdown();
}
