//! Track A (final validation) — storage & data-integrity probes.
//!
//! Deterministic by construction: fixed seeds, single-threaded test bodies, no
//! sleeps (ordering is enforced by the engine's own request-completion
//! condvars). Every assertion verifies a REAL property:
//!   * byte equality against independently regenerated SplitMix64 reference
//!     payloads (`util::payload_fill`, the frozen formula of ARCHITECTURE §11),
//!   * exact `io::ErrorKind` of guard errors,
//!   * exact counter deltas / invariant bounds (`hit+miss == requested`,
//!     `physical_write_bytes` unchanged across failed puts, ...).
//!
//! Probes (none duplicated from engine_tests / p4_cio_tests / p2_matrix_tests):
//!   1. `valA_ssd_scope_boundary_fill_then_overflow_clean_err_no_partial_publish`
//!      — SSD scope filled to the exact gpu_file_bytes guard boundary; the
//!      exceeding put must be a clean Err, write zero payload bytes, leave the
//!      published scope byte-exact, and never deliver garbage for the failed key.
//!   2. `valA_pool_file_consumed_new_session_beyond_pool_put_integrity`
//!      — gpu_file_pool = 1 fully consumed; a NEW session's first SSD put probes
//!      the actual pool semantics (see the comment inside: pool is a
//!      pre-allocation count, NOT a hard cap — reported as a finding), the
//!      resulting budget excess reclaims the oldest scope, and integrity of BOTH
//!      sessions holds (recompute allowed, byte-exactness required).
//!   3. `valA_recovery_drop_rebuild_truncated_log_recompute_integrity`
//!      — recovery_log = 2: puts, engine DROPPED without shutdown (crash), log
//!      tail truncated to the pre-second-put length (models records that never
//!      reached disk), rebuild: recovered blocks byte-exact via SSD hits with
//!      zero misses; never-flushed blocks recompute with seed-derived bytes
//!      (misses allowed); accounting invariant holds on the rebuilt engine.
//!   4. `valA_padding_boundary_segment_fit_then_oversize_guard`
//!      — block_bytes = 3200 (not 4 KiB-multiple) so the layer-major segment
//!      carries REAL align_up padding: a segment whose padded end lands exactly
//!      on gpu_file_bytes must succeed byte-exact; one block more must be a
//!      clean session-size-guard Err both as an append and as a fresh scope
//!      (with allocation rollback verified by a successful retry).
//!   5. `valA_byte_exact_stress_3_sessions_6_rounds_forced_reclaim`
//!      — 6 rounds of fixed-seed randomized interleaved puts/gets across 3
//!      sessions (one DRAM-scoped, two SSD-scoped, hybrid mixed gets), forced
//!      reclaims between rounds (drain-to-low watermark overshoot frees every
//!      established scope, which also bounds per-scope byte volume), every
//!      delivered block verified against regenerated reference bytes, accounting
//!      invariant asserted each round.
//!
//! Plus one `#[ignore]`d defect pin (reported, NOT fixed — this track is
//! test-only): `valA_defect_pin_replay_alloc_reuses_adopted_file_seq`
//! demonstrates that the P4 recovery replay's `adopt_session` does not reserve
//! the adopted file_seq, so the next new-scope allocation re-issues that seq and
//! physically overwrites the recovered scope's bytes. Run explicitly with
//! `cargo test --test validation_storage -- --ignored` (fails while the defect
//! exists, passes once fixed).
//!
//! NOT covered here (with reason): OS power-loss crash (only process-level drop
//! is simulable in-process); multi-file chaining (deferred by design, OQ-11);
//! baseline engine storage paths (parallel track); mid-segment unaligned
//! merged-range reads (with block_bytes not a 4 KiB multiple, a get whose
//! coalesced range starts mid-segment hits the hard `assert!` in
//! cio/stream.rs read_one_range — reported as a finding, deliberately not
//! triggered inside a passing suite).

#![allow(non_snake_case)]

use fckv::config::Config;
use fckv::engine::FlatCakeEngine;
use fckv::engine_api::{GetSpec, KvEngine, PutSpec};
use fckv::types::{BlockKey, PlacementSignals};
use fckv::util::{payload_fill, splitmix64};
use std::collections::HashMap;
use std::io::ErrorKind;
use std::fs::OpenOptions;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static IDS: AtomicU64 = AtomicU64::new(9_000);
fn next_id() -> u64 {
    IDS.fetch_add(1, Ordering::SeqCst)
}

/// Base config for the probes. `hbm_window_bytes = 2048 < block_bytes` disables
/// the hbm window entirely (record_delivery skips oversized payloads), so every
/// get is attributable to DRAM/SSD/recompute deterministically. Session 1 is
/// avoided: the engine special-cases it (`refcount = 2` in recompute ownership),
/// which would make placement non-uniform.
fn base_cfg(tag: &str) -> Config {
    let mut c = Config::default();
    c.dram_pool_bytes = 256 * 1024;
    c.hbm_window_bytes = 2048;
    c.gpu_file_bytes = 64 * 1024; // 8 blocks of the default 8 KiB per file
    c.gpu_file_pool = 2;
    // Reclaim disabled while within budget: usage > 1.0 * cap never fires.
    c.ssd_high_watermark = 1.0;
    c.ssd_low_watermark = 0.5;
    c.dram_high_watermark = 1.0;
    c.dram_low_watermark = 0.5;
    c.ssd_dir = format!("data/test/valA-{tag}-{}", std::process::id());
    c.baseline_ssd_dir = format!("data/test/valA-b-{tag}-{}", std::process::id());
    c.queue_cap = 64;
    c.recovery_log = 1;
    c.write_coalesce_bytes = 65536;
    c.dedup_enabled = 0;
    c
}

/// PutSpec with deterministic placement control via `turn`:
/// turn == 1 (unshared, short reply) -> SSD (Table-2 row 4);
/// turn > 1  (reply < tau)           -> DRAM (row 5) at scope creation.
fn put(session: u64, turn: u64, layers: u32, start: u64, n: u32, seed: u64) -> PutSpec {
    PutSpec {
        session,
        turn,
        layers,
        start_blk: start,
        n_blk: n,
        shared: 0,
        reply_tokens: 100,
        ctx_tokens: 512,
        seed,
    }
}

/// Keys use the BLOCK-TOKENS multiplier (engine convention: token_start =
/// start_blk * block_tokens), never block_bytes.
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

fn get_spec(blocks: Vec<BlockKey>, seeds: Vec<u64>, requester: u64) -> GetSpec {
    GetSpec {
        id: next_id(),
        arrival_ts_us: 0,
        blocks,
        seed_by_block: seeds,
        signals: PlacementSignals {
            session: requester,
            refcount: 1,
            turn: 1,
            reply_tokens: 100,
            ctx_tokens: 512,
        },
    }
}

/// Run one get and verify EVERY delivered block against the independently
/// regenerated reference payload (frozen SplitMix64 formula, seed = the
/// creating put's seed). Asserts the delivered byte length and that every
/// requested block was delivered. Returns the delivered count.
fn checked_get(
    e: &FlatCakeEngine,
    blocks: &[BlockKey],
    seeds: &[u64],
    requester: u64,
    bb: u64,
    ctx: &str,
) -> usize {
    assert_eq!(blocks.len(), seeds.len(), "{ctx}: seeds must pair with blocks");
    let gspec = get_spec(blocks.to_vec(), seeds.to_vec(), requester);
    let mut delivered = 0usize;
    let blocks = gspec.blocks.clone();
    e.get_request(&gspec, &mut |idx, bytes| {
        let k = blocks[idx];
        assert_eq!(
            bytes.len(),
            bb as usize,
            "{ctx}: delivered byte length for {k:?}"
        );
        let mut expect = vec![0u8; bb as usize];
        payload_fill(k.session, k.layer, k.token_start, seeds[idx], &mut expect);
        assert_eq!(bytes, expect, "{ctx}: byte mismatch for {k:?}");
        delivered += 1;
    })
    .unwrap_or_else(|e| panic!("{ctx}: get_request returned Err: {e}"));
    assert_eq!(
        delivered,
        blocks.len(),
        "{ctx}: every requested block must be delivered"
    );
    delivered
}

fn accounting_ok(m: &fckv::types::Metrics, ctx: &str) {
    assert_eq!(
        m.hit_hbm + m.hit_dram + m.hit_ssd + m.misses,
        m.requested_blocks,
        "{ctx}: accounting invariant hit+miss == requested (hbm={} dram={} ssd={} miss={} req={})",
        m.hit_hbm,
        m.hit_dram,
        m.hit_ssd,
        m.misses,
        m.requested_blocks
    );
}

/// Deterministic Fisher-Yates shuffle driven by SplitMix64.
fn shuffle<T>(v: &mut [T], state: &mut u64) {
    for i in (1..v.len()).rev() {
        let r = splitmix64(state);
        let j = (r % (i as u64 + 1)) as usize;
        v.swap(i, j);
    }
}

type Reg = HashMap<u64, Vec<(BlockKey, u64)>>;

/// Deterministically pick up to `k` (key, seed) pairs of one session.
fn pick(reg: &Reg, session: u64, k: usize, state: &mut u64) -> Vec<(BlockKey, u64)> {
    let mut items = reg.get(&session).cloned().unwrap_or_default();
    shuffle(&mut items, state);
    items.truncate(k);
    items
}

fn register(reg: &mut Reg, spec: &PutSpec, bt: u64) {
    let e = reg.entry(spec.session).or_default();
    for k in keys_of(spec, bt) {
        e.push((k, spec.seed));
    }
}

// ===========================================================================
// Probe 1 — SSD-scope exhaustion at the exact guard boundary.
// ===========================================================================
#[test]
fn valA_ssd_scope_boundary_fill_then_overflow_clean_err_no_partial_publish() {
    let cfg = base_cfg("scope-full");
    let _ = std::fs::remove_dir_all(&cfg.ssd_dir);
    let e = Arc::new(FlatCakeEngine::new(&cfg).unwrap());
    let bb = cfg.block_bytes();
    let fb = cfg.gpu_file_bytes;
    assert_eq!(bb, 8192, "default block_bytes = 256 tokens * 32 B");
    assert_eq!(fb / bb, 8, "one 1-layer batch of 8 blocks fills the file exactly");

    // (1) Fill session 2's scope to EXACTLY the guard boundary: seg_len =
    //     8 * 8192 = 65536 = gpu_file_bytes; the guard checks
    //     `physical_end > file_bytes`, so equality must SUCCEED.
    let p1 = put(2, 1, 1, 0, 8, 0xA1);
    e.put_batch(&p1).expect("boundary-fit put (physical_end == gpu_file_bytes) must succeed");
    let ks1 = keys_of(&p1, cfg.block_tokens);
    let seeds1 = vec![0xA1u64; ks1.len()];

    // (2) All 8 blocks served from SSD, byte-exact (hbm window disabled).
    let before = e.snapshot();
    let got = checked_get(&e, &ks1, &seeds1, 2, bb, "probe1 boundary get");
    assert_eq!(got, 8);
    let after = e.snapshot();
    assert_eq!(after.hit_ssd - before.hit_ssd, 8, "boundary blocks must be SSD hits");
    assert_eq!(after.misses - before.misses, 0, "no recompute for published blocks");
    accounting_ok(&after, "probe1 after boundary get");

    // (3) One block more (append inherits SSD scope stickiness) -> the
    //     session-size guard must reject with a CLEAN error and zero payload
    //     writes (no partial publish), and must not trigger reclamation.
    let m_before = e.snapshot();
    let p2 = put(2, 2, 1, 8, 1, 0xA2);
    let err = e
        .put_batch(&p2)
        .expect_err("append past gpu_file_bytes must fail cleanly");
    assert_eq!(
        err.kind(),
        ErrorKind::InvalidData,
        "session-size guard error kind (cio::layout append_scope_batch)"
    );
    let m_after = e.snapshot();
    assert_eq!(
        m_after.physical_write_bytes, m_before.physical_write_bytes,
        "failed put must write ZERO payload bytes (no partial publish)"
    );
    assert_eq!(
        m_after.reclaim_events, m_before.reclaim_events,
        "guard error must not trigger reclamation"
    );

    // (4) The previously published scope is intact: re-read all 8 blocks
    //     byte-exact after the failed put.
    let got = checked_get(&e, &ks1, &seeds1, 2, bb, "probe1 post-failure re-get");
    assert_eq!(got, 8);

    // (5) The overflow key was NOT published (slots rolled back): a get of it
    //     must not deliver garbage. Two clean outcomes are acceptable: Err from
    //     the guard (recompute re-persist hits the same full-scope guard), or Ok
    //     with byte-exact recomputed delivery. A silent wrong-byte delivery is
    //     not. NOTE: the Err path currently leaks a reader pin (defect D-A2 in
    //     the report) — harmless for the remainder of this test (no reclaim).
    let ks2 = keys_of(&p2, cfg.block_tokens);
    let seeds2 = vec![0xA2u64; ks2.len()];
    let gspec = get_spec(ks2.clone(), seeds2.clone(), 2);
    let mut delivered = 0usize;
    let blocks = gspec.blocks.clone();
    let res = e.get_request(&gspec, &mut |idx, bytes| {
        let k = blocks[idx];
        let mut expect = vec![0u8; bb as usize];
        payload_fill(k.session, k.layer, k.token_start, seeds2[idx], &mut expect);
        assert_eq!(bytes, expect, "probe1: overflow key delivered wrong bytes");
        delivered += 1;
    });
    match res {
        Ok(()) => {
            assert_eq!(delivered, 1, "Ok outcome must deliver byte-exact recompute");
        }
        Err(err) => {
            assert_eq!(
                err.kind(),
                ErrorKind::InvalidData,
                "Err outcome must be the clean session-size guard"
            );
            assert_eq!(delivered, 0, "Err outcome must deliver nothing");
        }
    }

    // Cumulative accounting invariant (the failed get counts its miss before
    // failing, so requested_blocks is 17 here).
    let m = e.snapshot();
    accounting_ok(&m, "probe1 final");
    e.shutdown();
}

// ===========================================================================
// Probe 2 — pool exhaustion: gpu_file_pool = 1 fully consumed, then a NEW
// session's first SSD put.
// ===========================================================================
#[test]
fn valA_pool_file_consumed_new_session_beyond_pool_put_integrity() {
    let mut cfg = base_cfg("pool-1");
    cfg.gpu_file_pool = 1;
    let _ = std::fs::remove_dir_all(&cfg.ssd_dir);
    let e = Arc::new(FlatCakeEngine::new(&cfg).unwrap());
    let bb = cfg.block_bytes();

    // Session 2 consumes the ONLY pool file exactly (8 blocks = 64 KiB).
    let p1 = put(2, 1, 1, 0, 8, 0xB1);
    e.put_batch(&p1).expect("fill the single pool file to its boundary");
    let ks1 = keys_of(&p1, cfg.block_tokens);
    let seeds1 = vec![0xB1u64; ks1.len()];
    let before = e.snapshot();
    checked_get(&e, &ks1, &seeds1, 2, bb, "probe2 baseline get");
    let after = e.snapshot();
    assert_eq!(after.hit_ssd - before.hit_ssd, 8, "baseline: 8 SSD hits");

    // NEW session's first SSD put while the pool file is fully consumed.
    //
    // PINNED ACTUAL SEMANTICS (finding D-A1 in the report): gpu_file_pool is a
    // PRE-ALLOCATION count, not a hard cap — `SsdLayoutImpl::alloc_file`
    // (src/cio/layout.rs) grows the file set beyond the pool on demand. The
    // budget is enforced only by the ADVISORY watermark + reclaim
    // (ARCHITECTURE §5), so this put SUCCEEDS and the resulting usage excess
    // (72 KiB > 1.0 * 64 KiB cap) deterministically reclaims the OLDEST
    // unreferenced scope — session 2's (creation-timestamp order, session-id
    // tiebreak).
    let m_before = e.snapshot();
    let p2 = put(3, 1, 1, 0, 1, 0xB2);
    e.put_batch(&p2).expect(
        "pinned semantics: beyond-pool allocation succeeds (pool is advisory; see report D-A1)",
    );
    assert!(
        Path::new(&format!("{}/gf_000001.bin", cfg.ssd_dir)).exists(),
        "a beyond-pool scope file must have been allocated on demand"
    );
    let m_after = e.snapshot();
    assert!(
        m_after.reclaim_events >= 1,
        "the budget excess must have reclaimed at least one scope at the put's watermark pass"
    );
    assert_eq!(
        m_after.physical_write_bytes - m_before.physical_write_bytes,
        bb,
        "exactly one block of payload bytes written for the new session"
    );

    // Session 2's SSD data was reclaimed (tombstoned before extent free):
    // a get must MISS and recompute with the seed-derived bytes — misses
    // allowed, byte-exactness REQUIRED. (The mission's "old session's data
    // remains intact" holds in the integrity sense, not the persistence
    // sense: persistence was lost to reclaim, integrity is preserved.)
    let pre = e.snapshot();
    let got = checked_get(&e, &ks1, &seeds1, 2, bb, "probe2 post-reclaim recompute get");
    assert_eq!(got, 8);
    let post = e.snapshot();
    assert_eq!(post.misses - pre.misses, 8, "reclaimed blocks recompute (misses)");
    assert_eq!(post.hit_ssd - pre.hit_ssd, 0, "reclaimed blocks cannot be SSD hits");

    // The recompute re-persist is byte-exact ON SSD: the second read is pure
    // SSD hits with zero new misses (hbm window is disabled).
    let pre = e.snapshot();
    let got = checked_get(&e, &ks1, &seeds1, 2, bb, "probe2 re-persisted SSD hits");
    assert_eq!(got, 8);
    let post = e.snapshot();
    assert_eq!(post.hit_ssd - pre.hit_ssd, 8, "re-persisted blocks served from SSD");
    assert_eq!(post.misses - pre.misses, 0, "no recompute after re-persist");

    // Session 3's block (the beyond-pool put) survived the reclaim: SSD hit,
    // byte-exact.
    let ks3 = keys_of(&p2, cfg.block_tokens);
    let seeds3 = vec![0xB2u64; ks3.len()];
    let pre = e.snapshot();
    let got = checked_get(&e, &ks3, &seeds3, 3, bb, "probe2 survivor get");
    assert_eq!(got, 1);
    let post = e.snapshot();
    assert_eq!(post.hit_ssd - pre.hit_ssd, 1, "survivor block served from SSD");

    let m = e.snapshot();
    accounting_ok(&m, "probe2 final");
    assert_eq!(
        m.logical_block_gets, m.requested_blocks,
        "no failed gets: every requested block is a counted logical get"
    );
    e.shutdown();
}

// ===========================================================================
// Probe 3 — interrupted-operation recovery: drop without shutdown, truncated
// log tail, rebuild, verify recovered + recomputed integrity.
// ===========================================================================
#[test]
fn valA_recovery_drop_rebuild_truncated_log_recompute_integrity() {
    let mut cfg = base_cfg("recovery");
    cfg.recovery_log = 2; // write + replay at startup
    cfg.gpu_file_pool = 4;
    let _ = std::fs::remove_dir_all(&cfg.ssd_dir);
    let bb = cfg.block_bytes();
    let bt = cfg.block_tokens;

    let e1 = FlatCakeEngine::new(&cfg).unwrap();
    // Session 2: 2 layers x 4 blocks = 64 KiB (one full file, file_seq 0).
    let pa = put(2, 1, 2, 0, 4, 0xC1);
    e1.put_batch(&pa).expect("session 2 put");
    let ks_a = keys_of(&pa, bt);
    let seeds_a = vec![0xC1u64; ks_a.len()];
    // Log length after put A's batch-end flush: exactly its 8 records.
    let log_path = format!("{}/index_log.jsonl", cfg.ssd_dir);
    let l1 = std::fs::metadata(&log_path).expect("recovery log exists").len();

    // Session 3: 4 blocks = 32 KiB (file_seq 1); 4 more log records.
    let pb = put(3, 1, 1, 0, 4, 0xC2);
    e1.put_batch(&pb).expect("session 3 put");
    let ks_b = keys_of(&pb, bt);
    let seeds_b = vec![0xC2u64; ks_b.len()];
    let l2 = std::fs::metadata(&log_path).unwrap().len();
    assert!(l2 > l1, "put B must have appended 4 log records");

    // Pre-crash baseline: both sessions byte-exact (pure hits; no log writes).
    let mut all_blocks = ks_a.clone();
    all_blocks.extend(ks_b.iter().copied());
    let mut all_seeds = seeds_a.clone();
    all_seeds.extend(seeds_b.iter().copied());
    checked_get(&e1, &all_blocks, &all_seeds, 2, bb, "probe3 pre-crash baseline");

    // CRASH: drop WITHOUT shutdown (no clean teardown; worker threads leak by
    // design of this simulation — same pattern as p4_recovery_replay_round_trip).
    drop(e1);

    // Simulate records that never reached the disk: truncate the log to the
    // post-put-A length. Session 3's publish records are gone; session 2's
    // survive (truncation lands on a record boundary: no partial lines).
    {
        let f = OpenOptions::new()
            .write(true)
            .open(&log_path)
            .expect("reopen recovery log for truncation");
        f.set_len(l1).expect("truncate log tail (simulated lost records)");
    }

    // REBUILD with the same workload: replay must recover exactly session 2's
    // 8 records.
    let e2 = FlatCakeEngine::new(&cfg).unwrap();
    let m0 = e2.snapshot();
    assert_eq!(
        m0.recovery_replayed, 8,
        "exactly session 2's 8 records must replay"
    );
    assert_eq!(
        m0.recovery_truncated, 0,
        "record-boundary truncation leaves no partial lines"
    );

    // (a) Recovered blocks: byte-exact via SSD hits with ZERO misses. This get
    //     runs BEFORE any new scope allocation on the rebuilt engine (see the
    //     defect pin at the bottom of this file: a new allocation re-issues the
    //     adopted file_seq and would overwrite this scope's bytes).
    let pre = e2.snapshot();
    let got = checked_get(&e2, &ks_a, &seeds_a, 2, bb, "probe3 recovered get");
    assert_eq!(got, 8);
    let post = e2.snapshot();
    assert_eq!(post.hit_ssd - pre.hit_ssd, 8, "recovered blocks must be SSD hits");
    assert_eq!(post.misses - pre.misses, 0, "recovered blocks must not miss");

    // (b) Blocks whose log records were never flushed: keys unknown on the
    //     rebuilt engine -> recompute with the seed-derived bytes (misses
    //     allowed, byte-exactness required), then re-persisted via the normal
    //     put path.
    let pre = e2.snapshot();
    let got = checked_get(&e2, &ks_b, &seeds_b, 3, bb, "probe3 never-flushed recompute");
    assert_eq!(got, 4);
    let post = e2.snapshot();
    assert_eq!(post.misses - pre.misses, 4, "never-flushed keys must miss then recompute");
    assert_eq!(post.hit_ssd - pre.hit_ssd, 0, "first access cannot be an SSD hit");

    // Re-persist durability: the second read is pure SSD hits, byte-exact.
    let pre = e2.snapshot();
    let got = checked_get(&e2, &ks_b, &seeds_b, 3, bb, "probe3 re-persisted hits");
    assert_eq!(got, 4);
    let post = e2.snapshot();
    assert_eq!(post.hit_ssd - pre.hit_ssd, 4, "re-persisted blocks served from SSD");
    assert_eq!(post.misses - pre.misses, 0);

    // (c) Accounting invariant on the rebuilt engine: 8 + 4 + 4 = 16 requested,
    //     12 SSD hits + 4 recompute misses.
    let m = e2.snapshot();
    accounting_ok(&m, "probe3 final");
    assert_eq!(m.requested_blocks, 16, "8 recovered + 4 recomputed + 4 re-hits");
    assert_eq!(
        m.logical_block_gets, m.requested_blocks,
        "no failed gets on the rebuilt engine"
    );
    e2.shutdown();
}

// ===========================================================================
// Probe 4 — extreme sizes: a segment whose PADDED end lands exactly on
// gpu_file_bytes (real padding: block_bytes = 3200 is not a 4 KiB multiple),
// then one block more in two shapes (append / fresh scope).
// ===========================================================================
#[test]
fn valA_padding_boundary_segment_fit_then_oversize_guard() {
    let mut cfg = base_cfg("pad-boundary");
    cfg.block_tokens = 100; // block_bytes = 3200 — NOT 4 KiB-aligned:
    cfg.bytes_per_token = 32; // exercises the align_up padding in the guard.
    let _ = std::fs::remove_dir_all(&cfg.ssd_dir);
    let e = Arc::new(FlatCakeEngine::new(&cfg).unwrap());
    let bb = cfg.block_bytes();
    let fb = cfg.gpu_file_bytes;
    assert_eq!(bb, 3200);
    // 20 blocks: seg_len = 64000; padded end = align_up(64000, 4096) = 65536
    // == gpu_file_bytes exactly (the segment is gpu_file_bytes minus its
    // 1536 bytes of padding).
    let n_fit = (fb / bb) as u32; // 20
    assert_eq!(
        (n_fit as u64 * bb).div_ceil(4096) * 4096,
        fb,
        "20 segments pad exactly onto the file boundary"
    );

    // Boundary fit: fresh scope, one layer, padded end == file_bytes -> success.
    let p1 = put(2, 1, 1, 0, n_fit, 0xD1);
    e.put_batch(&p1).expect("padded boundary fit must succeed");
    let ks1 = keys_of(&p1, cfg.block_tokens);
    let seeds1 = vec![0xD1u64; ks1.len()];
    // Byte-exact read of the whole batch. Only FULL-batch gets are issued in
    // this probe: a get whose coalesced range starts mid-segment would trip the
    // 4 KiB-alignment assert in cio::stream (finding D-A3 — reported).
    let before = e.snapshot();
    let got = checked_get(&e, &ks1, &seeds1, 2, bb, "probe4 boundary-fit get");
    assert_eq!(got, n_fit as usize);
    let after = e.snapshot();
    assert_eq!(
        after.hit_ssd - before.hit_ssd,
        n_fit as u64,
        "boundary-fit blocks served from SSD"
    );

    // One block more as an APPEND (scope stickiness): seg_start =
    // align_up(64000, 4096) = 65536, padded end 69632 > file -> clean guard Err.
    let m_before = e.snapshot();
    let p2 = put(2, 2, 1, n_fit as u64, 1, 0xD2);
    let err = e
        .put_batch(&p2)
        .expect_err("one block more must exceed the session-size guard");
    assert_eq!(err.kind(), ErrorKind::InvalidData, "append-overflow guard kind");
    let m_after = e.snapshot();
    assert_eq!(
        m_after.physical_write_bytes, m_before.physical_write_bytes,
        "failed append writes zero payload bytes"
    );

    // One block more as a FRESH scope (21 blocks in one put): padded end
    // 69632 > file -> clean guard Err with ALLOCATION ROLLBACK (the just-made
    // scope allocation is returned to the free list, session 4 stays retryable).
    let p3 = put(4, 1, 1, 0, n_fit + 1, 0xD3);
    let err = e
        .put_batch(&p3)
        .expect_err("fresh-scope oversize batch must fail cleanly");
    assert_eq!(err.kind(), ErrorKind::InvalidData, "fresh-scope guard kind");
    // Rollback verified behaviorally: a small put for session 4 succeeds.
    let p4 = put(4, 1, 1, 100, 1, 0xD4);
    e.put_batch(&p4).expect("rolled-back scope must be retryable");
    let ks4 = keys_of(&p4, cfg.block_tokens);
    let seeds4 = vec![0xD4u64; ks4.len()];
    let pre = e.snapshot();
    let got = checked_get(&e, &ks4, &seeds4, 4, bb, "probe4 retry get");
    assert_eq!(got, 1);
    let post = e.snapshot();
    assert_eq!(post.hit_ssd - pre.hit_ssd, 1, "retry block served from SSD");

    // Session 2's boundary-fit scope is still intact after both failed puts.
    let got = checked_get(&e, &ks1, &seeds1, 2, bb, "probe4 post-failure re-get");
    assert_eq!(got, n_fit as usize);

    let m = e.snapshot();
    accounting_ok(&m, "probe4 final");
    assert_eq!(
        m.logical_block_gets, m.requested_blocks,
        "no failed gets: 20 + 1 + 20 requested, all delivered"
    );
    e.shutdown();
}

// ===========================================================================
// Probe 5 — byte-for-byte stress: 6 rounds of fixed-seed randomized
// interleaved puts/gets across 3 sessions (10/30 SSD-scoped, 20 DRAM-scoped),
// forced reclaims between rounds, every delivered block verified against
// independently regenerated reference bytes, accounting invariant each round.
// ===========================================================================
#[derive(Clone, Copy)]
enum Op {
    Put10,
    Put20,
    Put30,
    Get10,
    Get20,
    GetMix,
}

#[test]
fn valA_byte_exact_stress_3_sessions_6_rounds_forced_reclaim() {
    let mut cfg = base_cfg("stress");
    cfg.gpu_file_pool = 3;
    cfg.dram_pool_bytes = 128 * 1024; // 16 blocks
    cfg.dram_high_watermark = 0.9;
    cfg.dram_low_watermark = 0.05; // drain-to-low frees every established scope
    cfg.ssd_high_watermark = 0.9;
    cfg.ssd_low_watermark = 0.05;
    let _ = std::fs::remove_dir_all(&cfg.ssd_dir);
    let e = Arc::new(FlatCakeEngine::new(&cfg).unwrap());
    let bb = cfg.block_bytes();
    let bt = cfg.block_tokens;

    // key -> creating-seed registry per session (reference bytes are ALWAYS
    // regenerated from (session, layer, token_start, seed) inside checked_get).
    let mut reg: Reg = HashMap::new();
    let mut rng: u64 = 0x5EED_C0DE_00C0_FFEE; // fixed seed
    let mut total_requested = 0u64;

    for round in 0..6u64 {
        // Per-round op plan, deterministically shuffled.
        let mut ops: Vec<Op> = vec![
            Op::Put10,
            Op::Get10,
            Op::Put20,
            Op::Get20,
            Op::Put30,
            Op::GetMix,
        ];
        shuffle(&mut ops, &mut rng);
        for op in ops {
            match op {
                Op::Put10 => {
                    // turn = round+1 (turn 1 at round 0) -> SSD scope.
                    let p = put(10, round + 1, 1, round * 2, 2, 0xE100 + round * 16);
                    e.put_batch(&p).expect("put10");
                    register(&mut reg, &p, bt);
                }
                Op::Put20 => {
                    // turn = round+2 >= 2, short reply -> DRAM scope (row 5).
                    let p = put(20, round + 2, 1, round * 3, 3, 0xE200 + round * 16);
                    e.put_batch(&p).expect("put20");
                    register(&mut reg, &p, bt);
                }
                Op::Put30 => {
                    let p = put(30, round + 1, 2, round * 2, 1, 0xE300 + round * 16);
                    e.put_batch(&p).expect("put30");
                    register(&mut reg, &p, bt);
                }
                Op::Get10 => {
                    let picks = pick(&reg, 10, 2, &mut rng);
                    if picks.is_empty() {
                        continue;
                    }
                    let (blocks, seeds): (Vec<_>, Vec<_>) = picks.into_iter().unzip();
                    total_requested += blocks.len() as u64;
                    checked_get(&e, &blocks, &seeds, 10, bb, &format!("round {round} get10"));
                }
                Op::Get20 => {
                    let picks = pick(&reg, 20, 2, &mut rng);
                    if picks.is_empty() {
                        continue;
                    }
                    let (blocks, seeds): (Vec<_>, Vec<_>) = picks.into_iter().unzip();
                    total_requested += blocks.len() as u64;
                    checked_get(&e, &blocks, &seeds, 20, bb, &format!("round {round} get20"));
                }
                Op::GetMix => {
                    // One block per session -> hybrid mixed get when all three
                    // sessions have published blocks.
                    let mut blocks = Vec::new();
                    let mut seeds = Vec::new();
                    for s in [10u64, 20, 30] {
                        for (k, sd) in pick(&reg, s, 1, &mut rng) {
                            blocks.push(k);
                            seeds.push(sd);
                        }
                    }
                    if blocks.is_empty() {
                        continue;
                    }
                    total_requested += blocks.len() as u64;
                    checked_get(&e, &blocks, &seeds, 10, bb, &format!("round {round} getmix"));
                }
            }
        }

        // ---- forced reclaims between rounds ----
        // Throwaway sessions overrun the SSD budget (5 x 8 blocks = 320 KiB >
        // 0.9 * 3 * 64 KiB) and the DRAM pool (12 blocks + session 20's blocks
        // > 0.9 * 128 KiB). With low = 0.05 the drain frees EVERY established
        // scope down to < 10 KiB — which also bounds each session-scope's byte
        // volume to one round (<< gpu_file_bytes, so recompute re-persists can
        // never hit the session-size guard).
        //
        // Ordering matters: the SSD fillers run FIRST (SSD drain frees every
        // scope's SSD extents), then the DRAM filler (its watermark pass frees
        // a mixed DRAM+SSD scope's remaining half and REMOVES it), and then two
        // more SSD overruns force another reclaim + layout detach NOW. Without
        // that tail, a scope removed by the DRAM pass keeps a STALE layout
        // session entry (with its append cursor) until the NEXT over-watermark
        // SSD put — i.e. the next round's fillers — and a re-created session
        // appends at the stale cursor (observed while authoring this probe;
        // report D-A5). The tail guarantees every round starts fully clean.
        let before_fill = e.snapshot();
        for i in 0..3u64 {
            let f = put(1000 + round * 10 + i, 1, 1, 0, 8, 0xF000 + round * 64 + i);
            e.put_batch(&f)
                .expect("ssd filler (exact file-boundary batch, fresh session)");
        }
        let f = put(2000 + round, 2, 1, 0, 12, 0xF100 + round);
        e.put_batch(&f).expect("dram filler (spill-on-full is a legal path)");
        for i in 3..5u64 {
            let f = put(1000 + round * 10 + i, 1, 1, 0, 8, 0xF000 + round * 64 + i);
            e.put_batch(&f).expect("ssd detach-forcing filler (fresh session)");
        }
        let after_fill = e.snapshot();
        assert!(
            after_fill.reclaim_events > before_fill.reclaim_events,
            "round {round}: forced budget overrun must trigger at least one reclaim"
        );

        // Accounting invariant after each round (cumulative).
        let m = e.snapshot();
        accounting_ok(&m, &format!("round {round} accounting"));
    }

    // Final cross-checks: cumulative counters consistent after 6 rounds.
    let m = e.snapshot();
    accounting_ok(&m, "stress final");
    assert_eq!(
        m.logical_block_gets, m.requested_blocks,
        "no failed gets: every requested block is a counted logical get"
    );
    assert_eq!(
        m.requested_blocks, total_requested,
        "requested_blocks must equal the sum of issued get block counts"
    );
    assert!(
        m.hit_ssd + m.hit_dram > 0 || m.misses > 0,
        "stress must have exercised real delivery paths"
    );
    e.shutdown();
}

// ===========================================================================
// Defect pin (#[ignore]) — REPORTED, NOT FIXED (Track A is test-only).
// ===========================================================================
//
// DEFECT D-A4: recovery replay does not reserve adopted file_seqs.
//   * `FlatCakeEngine::new` replay loop (src/engine.rs, recovery_log == 2)
//     re-publishes recovered SSD GVAs and calls
//     `core.layout.adopt_session(session, file_seq, cursor)`.
//   * `SsdLayoutImpl::adopt_session` (src/cio/layout.rs) registers the adopted
//     file in `inner.files` but leaves `inner.next_seq` at 0.
//   * The next NEW-scope allocation (`alloc_file`, src/cio/layout.rs) pops an
//     empty free list and re-issues seq 0, REPLACING the adopted FileState and
//     appending at in-file offset 0 — physically overwriting the recovered
//     scope's bytes. The recovered session's index still points there, so its
//     layer-0 blocks silently deliver the new tenant's payload.
//   * Trigger: recovery_log = 2 restart + ANY new SSD scope (a new session's
//     put, or a recompute re-persist of a non-recovered session).
// This test pins the CORRECT behavior (byte-exactness of recovered data after
// a post-replay allocation): it FAILS while the defect exists and PASSES once
// `adopt_session` advances `next_seq` past adopted file_seqs.
#[test]
#[ignore]
fn valA_defect_pin_replay_alloc_reuses_adopted_file_seq() {
    let mut cfg = base_cfg("defect-pin");
    cfg.recovery_log = 2;
    cfg.gpu_file_pool = 4;
    let _ = std::fs::remove_dir_all(&cfg.ssd_dir);
    let bb = cfg.block_bytes();
    let bt = cfg.block_tokens;

    let e1 = FlatCakeEngine::new(&cfg).unwrap();
    let pa = put(2, 1, 2, 0, 4, 0xC1); // 8 blocks -> file 0, layer 0 at 0..32K
    e1.put_batch(&pa).unwrap();
    drop(e1); // crash

    let e2 = FlatCakeEngine::new(&cfg).unwrap();
    let ks_a = keys_of(&pa, bt);
    let seeds_a = vec![0xC1u64; ks_a.len()];
    checked_get(&e2, &ks_a, &seeds_a, 2, bb, "defect-pin recovered get");

    // A NEW SSD scope is allocated right after replay: with the defect, it
    // re-uses file_seq 0 at offset 0 and clobbers session 2's layer-0 bytes.
    let p3 = put(3, 1, 1, 0, 4, 0xC2);
    e2.put_batch(&p3).unwrap();

    // CORRECT behavior: the recovered scope must still read byte-exact.
    let e2b = &e2;
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        checked_get(e2b, &ks_a, &seeds_a, 2, bb, "defect-pin post-alloc get");
    }));
    e2.shutdown();
    if res.is_err() {
        panic!(
            "CONFIRMED D-A4 (replay file_seq reuse): after a post-replay SSD allocation, \
             the recovered session's bytes are physically overwritten — \
             fix: adopt_session must reserve adopted file_seqs (advance next_seq) \
             in src/cio/layout.rs"
        );
    }
}
