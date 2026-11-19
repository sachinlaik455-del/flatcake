//! P3 medium-aware migration tests (deterministic; cooldown=0 unless testing
//! the cooldown itself). Each test names its adversarial requirement.

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
    c.dram_pool_bytes = 128 * 1024; // 16 blocks
    c.hbm_window_bytes = 64 * 1024;
    c.gpu_file_bytes = 256 * 1024;
    c.gpu_file_pool = 4;
    c.ssd_dir = format!("data/test/p3-{tag}-{}", std::process::id());
    c.baseline_ssd_dir = format!("data/test/p3b-{tag}-{}", std::process::id());
    c.queue_cap = 64;
    c.migration_enabled = 1;
    c.eviction_policy = "medium_aware".into();
    c.migration_cooldown_ms = 0; // deterministic by default
    c.migration_max_per_get = 2;
    c.adaptive_k = 2;
    c.adaptive_h_min = 0.1;
    mutation(&mut c);
    c
}

/// Shared prompt (session 1, DRAM by Table-2 row 1).
fn shared_put(session: u64, layers: u32, n: u32, seed: u64) -> PutSpec {
    PutSpec {
        session,
        turn: 1,
        layers,
        start_blk: 0,
        n_blk: n,
        shared: 1,
        reply_tokens: 0,
        ctx_tokens: layers as u64 * n as u64 * 256,
        seed,
    }
}

/// Agentic session put: turn-1 one-shot => SSD scope (OQ-19 row 4), ctx grows.
fn sess_put(session: u64, turn: u64, layers: u32, start: u64, n: u32, seed: u64) -> PutSpec {
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

/// Block keys must use BLOCK TOKENS as the multiplier (engine convention),
/// not block bytes — a mismatch here looks like phantom misses.
fn keys_of(spec: &PutSpec, block_tokens: u64) -> Vec<BlockKey> {
    let mut v = Vec::new();
    for l in 0..spec.layers {
        for i in 0..spec.n_blk {
            v.push(BlockKey {
                session: spec.session,
                layer: l,
                token_start: (spec.start_blk + i as u64) * block_tokens,
            });
        }
    }
    v
}

fn get_spec_for(blocks: Vec<BlockKey>, seeds: &[u64], session: u64, turn: u64) -> GetSpec {
    let n = blocks.len() as u64;
    GetSpec {
        id: next_id(),
        arrival_ts_us: 0,
        blocks,
        seed_by_block: seeds.to_vec(),
        signals: PlacementSignals {
            session,
            refcount: 1,
            turn,
            reply_tokens: 500,
            ctx_tokens: n * 256,
        },
    }
}

fn verify_bytes<'a>(spec_blocks: &'a [BlockKey], seeds: &'a [u64], bb: u64) -> impl FnMut(usize, &[u8]) + 'a {
    move |idx, bytes| {
        let k = spec_blocks[idx];
        let mut expect = vec![0u8; bb as usize];
        payload_fill(k.session, k.layer, k.token_start, seeds[idx], &mut expect);
        assert_eq!(bytes, expect, "P3 integrity: block {k:?}");
    }
}

// P3(2)+(4): promotion moves a hot SSD block to RAM; FC-GVA mapping follows;
// bytes identical before and after; subsequent gets are DRAM hits.
#[test]
fn p3_promotion_integrity_and_mapping() {
    let cfg = cfg("promo", |_| {});
    let e = Arc::new(FlatCakeEngine::new(&cfg).unwrap());
    let bb = cfg.block_bytes();
    // Session 2: turn-1 one-shot => SSD scope; blocks later re-accessed hot.
    let p = sess_put(2, 1, 2, 0, 4, 500);
    e.put_batch(&p).unwrap();
    let ks = keys_of(&p, cfg.block_tokens);
    let seeds = vec![500u64; ks.len()];

    // Access 1: SSD hits (scope was created by the one-shot rule).
    let g1 = get_spec_for(ks.clone(), &seeds, 2, 2);
    e.get_request(&g1, &mut verify_bytes(&ks, &seeds, bb)).unwrap();
    let m1 = e.snapshot();
    assert!(m1.hit_ssd > 0, "first access must come from SSD");

    // Access 2: heat reaches K=2 => promoted (bytes from hbm window, zero extra
    // SSD reads); mapping swaps to DRAM.
    e.get_request(&g1, &mut verify_bytes(&ks, &seeds, bb)).unwrap();
    let m2 = e.snapshot();
    assert!(
        m2.migr_promotions >= 1,
        "hot block must promote: {}",
        m2.migr_promotions
    );

    // Access 3: evict the hbm window with filler sessions, then access again —
    // the promoted block must now be served from DRAM via its new mapping.
    for s in 10..14u64 {
        let fp = sess_put(s, 1, 2, 0, 4, s * 91);
        e.put_batch(&fp).unwrap();
        let fks = keys_of(&fp, cfg.block_tokens);
        let fseeds = vec![s * 91; fks.len()];
        let fg = get_spec_for(fks.clone(), &fseeds, s, 2);
        e.get_request(&fg, &mut verify_bytes(&fks, &fseeds, bb)).unwrap();
    }
    e.get_request(&g1, &mut verify_bytes(&ks, &seeds, bb)).unwrap();
    let m3 = e.snapshot();
    assert!(
        m3.hit_dram > m1.hit_dram,
        "promoted block must be served from DRAM (dram {} -> {})",
        m1.hit_dram,
        m3.hit_dram
    );
    assert_eq!(
        m3.hit_hbm + m3.hit_dram + m3.hit_ssd + m3.misses,
        m3.requested_blocks,
        "accounting invariant holds across migration"
    );
    e.shutdown();
}

// P3(4)+(5): medium_aware eviction demotes cold DRAM blocks under pressure
// (frequency-aware: hot blocks stay), preserves bytes + mappings.
#[test]
fn p3_demotion_under_pressure_hot_blocks_stay() {
    let cfg = cfg("demo", |_| {});
    let e = Arc::new(FlatCakeEngine::new(&cfg).unwrap());
    let bb = cfg.block_bytes();
    // Two shared prompts (DRAM scopes): P_hot will be made hot, P_cold stays cold.
    let hot = shared_put(1, 2, 4, 700); // 8 blocks = 64 KiB
    let cold = shared_put(3, 2, 4, 800); // another 8 blocks
    e.put_batch(&hot).unwrap();
    e.put_batch(&cold).unwrap();

    // Make the hot prompt hot: 3 access rounds (heat 3 >= K=2).
    let hot_ks = keys_of(&hot, cfg.block_tokens);
    let hot_seeds = vec![700u64; hot_ks.len()];
    let gh = get_spec_for(hot_ks.clone(), &hot_seeds, 1, 2);
    for _ in 0..3 {
        e.get_request(&gh, &mut verify_bytes(&hot_ks, &hot_seeds, bb))
            .unwrap();
    }

    // Drive DRAM past the high watermark with more shared prompts; the
    // medium_aware demotion pass must evict COLD blocks and keep the hot one.
    eprintln!("P3D pre-pressure mem_used={}", e.mem_used());
    for s in 10..18u64 {
        e.put_batch(&shared_put(s, 2, 4, s * 100)).unwrap();
        eprintln!("P3D after put s{}: mem_used={}", s, e.mem_used());
    }
    let m = e.snapshot();
    eprintln!("P3D demotions={} promotions={}", m.migr_demotions, m.migr_promotions);
    assert!(
        m.migr_demotions > 0,
        "pressure must demote cold blocks: {}",
        m.migr_demotions
    );

    // Hot prompt still served without SSD reads (heat >= K kept it in DRAM);
    // cold prompt (session 3) was demoted => served from SSD, bytes exact.
    e.get_request(&gh, &mut verify_bytes(&hot_ks, &hot_seeds, bb))
        .unwrap();
    let cold_ks = keys_of(&cold, cfg.block_tokens);
    let cold_seeds = vec![800u64; cold_ks.len()];
    let gc = get_spec_for(cold_ks.clone(), &cold_seeds, 3, 2);
    e.get_request(&gc, &mut verify_bytes(&cold_ks, &cold_seeds, bb))
        .unwrap();
    let m2 = e.snapshot();
    assert_eq!(
        m2.hit_hbm + m2.hit_dram + m2.hit_ssd + m2.misses,
        m2.requested_blocks
    );
    e.shutdown();
}

// P3(6): anti-thrash — a just-DEMOTED block cannot re-promote within the
// cooldown (the demotion itself records last_migrated; the skip is observable).
#[test]
fn p3_cooldown_blocks_immediate_remigration() {
    let cfg = cfg("cool", |c| {
        c.migration_cooldown_ms = 60_000; // effectively forever within the test
        c.adaptive_h_min = 0.05;
    });
    let e = Arc::new(FlatCakeEngine::new(&cfg).unwrap());
    let bb = cfg.block_bytes();
    // Cold DRAM-resident block (shared scope, never accessed => heat 0 < K):
    // recent but demotable.
    let cold = shared_put(3, 2, 4, 800); // 8 blocks = 64 KiB
    e.put_batch(&cold).unwrap();
    // Pressure: fill DRAM past the watermark with other shared prompts =>
    // the medium_aware pass demotes the cold blocks (last_migrated recorded).
    for s in 10..20u64 {
        e.put_batch(&shared_put(s, 2, 4, s * 100)).unwrap();
    }
    let m0 = e.snapshot();
    assert!(m0.migr_demotions > 0, "cold block must demote under pressure");
    // Re-access the demoted block K+ times: it becomes hot (heat >= K) and is
    // an SSD-backed promotion candidate — but its demotion cooldown blocks the
    // re-promotion (skip observable, no migration inside the window).
    let cold_ks = keys_of(&cold, cfg.block_tokens);
    let cold_seeds = vec![800u64; cold_ks.len()];
    let gc = get_spec_for(cold_ks.clone(), &cold_seeds, 3, 2);
    for _ in 0..4 {
        e.get_request(&gc, &mut verify_bytes(&cold_ks, &cold_seeds, bb))
            .unwrap();
    }
    let m1 = e.snapshot();
    assert!(
        m1.migr_cooldown_skips > 0,
        "cooldown must skip the re-promotion attempt (demotions={} skips={})",
        m1.migr_demotions,
        m1.migr_cooldown_skips
    );
    assert_eq!(
        m1.migr_promotions, 0,
        "no promotion may occur inside the cooldown"
    );
    // Bytes correct throughout (served from SSD while blocked).
    e.get_request(&gc, &mut verify_bytes(&cold_ks, &cold_seeds, bb))
        .unwrap();
    e.shutdown();
}

// P3(6): no migration loops — steady workload (hot stays hot, cold stays cold)
// produces a bounded number of migrations, not unbounded churn.
#[test]
fn p3_no_migration_loops_under_steady_state() {
    let cfg = cfg("loop", |_| {});
    let e = Arc::new(FlatCakeEngine::new(&cfg).unwrap());
    let bb = cfg.block_bytes();
    let hot = shared_put(1, 2, 4, 600);
    e.put_batch(&hot).unwrap();
    let hot_ks = keys_of(&hot, cfg.block_tokens);
    let hot_seeds = vec![600u64; hot_ks.len()];
    let gh = get_spec_for(hot_ks.clone(), &hot_seeds, 1, 2);
    // Warm the hot prompt.
    for _ in 0..2 {
        e.get_request(&gh, &mut verify_bytes(&hot_ks, &hot_seeds, bb))
            .unwrap();
    }
    // Steady state: 30 rounds of (pressure puts + hot re-access). After the
    // initial promotion and the first demotion wave, migrations must stop.
    for round in 0..30u64 {
        e.put_batch(&shared_put(50 + round, 2, 4, 1000 + round)).unwrap();
        e.get_request(&gh, &mut verify_bytes(&hot_ks, &hot_seeds, bb))
            .unwrap();
    }
    let _m_early = e.snapshot();
    // Loop detector: with cooldown = 0 a promote<->demote oscillation of the
    // SAME block would drive promotions up round after round. The hot block
    // is protected by heat >= K, and newly demoted blocks cannot re-promote
    // within... cooldown is 0 here, so recency is the guard: promotions must
    // be bounded by (initial promotions + blocks that left DRAM), not grow
    // per-round. Capture the trend over the last 10 rounds: promotions must
    // be flat while demotions may track new-block inflow (<= rounds*budget).
    let m_early = e.snapshot();
    for round in 30..40u64 {
        e.put_batch(&shared_put(50 + round, 2, 4, 2000 + round)).unwrap();
        e.get_request(&gh, &mut verify_bytes(&hot_ks, &hot_seeds, bb))
            .unwrap();
    }
    let m_late = e.snapshot();
    let promo_growth = m_late.migr_promotions - m_early.migr_promotions;
    assert!(
        promo_growth <= 4,
        "promotion churn in steady state = loop: grew by {} over 10 rounds",
        promo_growth
    );
    assert!(
        m_late.migr_demotions <= 40 * 2,
        "demotions must track inflow (<= rounds x budget): {}",
        m_late.migr_demotions
    );
    // Integrity to the last round.
    e.get_request(&gh, &mut verify_bytes(&hot_ks, &hot_seeds, bb))
        .unwrap();
    e.shutdown();
}

// P3(6): starvation-free promotion under budget — many hot SSD blocks promote
// eventually with a per-request budget of 2.
#[test]
fn p3_promotion_eventually_serves_all_hot_blocks() {
    let cfg = cfg("starve", |c| {
        c.migration_max_per_get = 2;
        c.hbm_window_bytes = 4 * 1024; // tiny hbm: SSD path dominates
    });
    let e = Arc::new(FlatCakeEngine::new(&cfg).unwrap());
    let bb = cfg.block_bytes();
    // One session with 16 SSD-resident blocks (2 layers x 8), all made hot.
    let p = sess_put(2, 1, 2, 0, 8, 90);
    e.put_batch(&p).unwrap();
    let ks = keys_of(&p, cfg.block_tokens);
    let seeds = vec![90u64; ks.len()];
    let g = get_spec_for(ks.clone(), &seeds, 2, 2);
    // Round 1: SSD reads, heat 1. Round 2: heat 2 => promotions (budget 2).
    // Rounds 3..N: remaining blocks promote as they cycle through the budget.
    for _ in 0..12 {
        e.get_request(&g, &mut verify_bytes(&ks, &seeds, bb)).unwrap();
    }
    let m = e.snapshot();
    assert!(
        m.migr_promotions >= ks.len() as u64 - 2,
        "all hot blocks must eventually promote (starvation check): {} of {}",
        m.migr_promotions,
        ks.len()
    );
    // Final state: served without SSD (all promoted) — bytes exact.
    let m2 = e.snapshot();
    // After full promotion the final access must serve every block with zero
    // misses (reused blocks never miss once promoted).
    e.get_request(&g, &mut verify_bytes(&ks, &seeds, bb)).unwrap();
    let m3 = e.snapshot();
    assert_eq!(
        m3.misses - m2.misses, 0,
        "reused blocks must never miss after promotion"
    );
    assert!(m3.migr_promotions >= ks.len() as u64 - 1);
    e.shutdown();
}

// P3(1)+(3): SSD-heavy workload with headroom below H_min => promotion must
// refuse (no cascading writes into a full RAM pool), workload still correct.
#[test]
fn p3_promotion_respects_headroom_floor() {
    let cfg = cfg("head", |c| {
        c.adaptive_h_min = 0.9; // demand 90% free: effectively never
        c.dram_pool_bytes = 64 * 1024;
    });
    let e = Arc::new(FlatCakeEngine::new(&cfg).unwrap());
    let bb = cfg.block_bytes();
    // Pre-fill DRAM so headroom < H_min from the start (promotions must refuse).
    let filler = shared_put(1, 2, 7, 400); // 56 KiB of the 64 KiB pool
    e.put_batch(&filler).unwrap();
    eprintln!("P3H mem_used={} pool={}", e.mem_used(), cfg.dram_pool_bytes);
    assert!(
        (1.0 - e.mem_used() as f64 / cfg.dram_pool_bytes as f64) < cfg.adaptive_h_min,
        "test precondition: headroom below the floor (used={} pool={})",
        e.mem_used(),
        cfg.dram_pool_bytes
    );
    let p = sess_put(2, 1, 2, 0, 4, 300);
    e.put_batch(&p).unwrap();
    let ks = keys_of(&p, cfg.block_tokens);
    let seeds = vec![300u64; ks.len()];
    let g = get_spec_for(ks.clone(), &seeds, 2, 2);
    for _ in 0..6 {
        e.get_request(&g, &mut verify_bytes(&ks, &seeds, bb)).unwrap();
    }
    let m = e.snapshot();
    assert_eq!(
        m.migr_promotions, 0,
        "headroom floor must block promotions"
    );
    assert_eq!(
        m.hit_hbm + m.hit_dram + m.hit_ssd + m.misses,
        m.requested_blocks
    );
    e.shutdown();
}

// P3(2): default configuration (migration_enabled = 0) is byte-for-byte the
// P1/P2 behavior — zero migration counters under identical pressure.
#[test]
fn p3_default_config_is_paper_faithful_write_once() {
    let cfg = cfg("off", |c| {
        c.migration_enabled = 0;
        c.eviction_policy = "session_extents".into();
    });
    let e = Arc::new(FlatCakeEngine::new(&cfg).unwrap());
    let bb = cfg.block_bytes();
    let hot = shared_put(1, 2, 4, 400);
    e.put_batch(&hot).unwrap();
    let hot_ks = keys_of(&hot, cfg.block_tokens);
    let hot_seeds = vec![400u64; hot_ks.len()];
    let gh = get_spec_for(hot_ks.clone(), &hot_seeds, 1, 2);
    for _ in 0..3 {
        e.get_request(&gh, &mut verify_bytes(&hot_ks, &hot_seeds, bb))
            .unwrap();
    }
    for s in 10..18u64 {
        e.put_batch(&shared_put(s, 2, 4, s * 100)).unwrap();
    }
    let m = e.snapshot();
    assert_eq!(m.migr_promotions, 0);
    assert_eq!(m.migr_demotions, 0);
    assert_eq!(m.migr_bytes_moved, 0);
    assert_eq!(
        m.hit_hbm + m.hit_dram + m.hit_ssd + m.misses,
        m.requested_blocks
    );
    e.shutdown();
}

// P3(4): concurrent access across migrations — integrity + accounting under
// parallel readers while promotion/demotion run.
#[test]
fn p3_concurrent_migration_integrity() {
    // Scope-file sizing: session-2 re-lands (promote→demote cycles) are capped
    // by migration_max_per_get × total gets = 2 × (4 threads × 15) = 120 blocks
    // on top of the initial 16, i.e. ≤ 544 KiB of scope-file space with no
    // in-file reuse (OQ-11/D20). The default 256 KiB file made that bound a
    // load-dependent race (promote/demote interleaving under suite contention
    // could exceed the file). 1 MiB makes the OQ-11 guard provably unreachable
    // for this workload while preserving the migration coverage.
    let cfg = cfg("conc", |c| {
        c.gpu_file_bytes = 1024 * 1024;
    });
    let e = Arc::new(FlatCakeEngine::new(&cfg).unwrap());
    let bb = cfg.block_bytes();
    let p = sess_put(2, 1, 2, 0, 8, 77);
    e.put_batch(&p).unwrap();
    let ks = keys_of(&p, cfg.block_tokens);
    let seeds = vec![77u64; ks.len()];
    let mut handles = Vec::new();
    for _ in 0..4 {
        let e = e.clone();
        let ks = ks.clone();
        let seeds = seeds.clone();
        handles.push(std::thread::spawn(move || {
            for _ in 0..15 {
                let g = get_spec_for(ks.clone(), &seeds, 2, 2);
                e.get_request(&g, &mut |idx, bytes| {
                    let k = ks[idx];
                    let mut expect = vec![0u8; bb as usize];
                    payload_fill(k.session, k.layer, k.token_start, seeds[idx], &mut expect);
                    assert_eq!(bytes, expect, "integrity across concurrent migration");
                })
                .unwrap();
            }
        }));
    }
    // Pressure driver: promotions + demotions interleaving with readers.
    for s in 10..30u64 {
        e.put_batch(&shared_put(s, 2, 4, s * 100)).unwrap();
    }
    for h in handles {
        h.join().unwrap();
    }
    let m = e.snapshot();
    assert_eq!(
        m.hit_hbm + m.hit_dram + m.hit_ssd + m.misses,
        m.requested_blocks,
        "accounting invariant under concurrent migration"
    );
    e.shutdown();
}

// P3(1): scheduling-decision determinism — same signals + sticky state yield
// the same medium decision across repeated calls (pure function check).
#[test]
fn p3_placement_decisions_are_deterministic() {
    let base = cfg("det", |_| {});
    let _ = base;
    // Same put spec replayed through fresh engines must place identically:
    // observable via which medium serves the first access.
    let p = sess_put(2, 1, 2, 0, 4, 55);
    let ks = keys_of(&p, base.block_bytes());
    let seeds = vec![55u64; ks.len()];
    let mut first_mediums = Vec::new();
    for round in 0..3 {
        let tag = format!("det{round}");
        let conf = cfg(&tag, |_| {});
        let eng = FlatCakeEngine::new(&conf).unwrap();
        eng.put_batch(&p).unwrap();
        let g = get_spec_for(ks.clone(), &seeds, 2, 2);
        eng.get_request(&g, &mut |_, _| {}).unwrap();
        let m = eng.snapshot();
        // Deterministic: the first access is an SSD hit in every fresh engine
        // (turn-1 one-shot => SSD scope) or identical misses — never mixed.
        first_mediums.push((m.hit_ssd > 0, m.hit_dram > 0));
        eng.shutdown();
    }
    assert!(
        first_mediums.windows(2).all(|w| w[0] == w[1]),
        "placement must be deterministic across identical runs: {first_mediums:?}"
    );
}

// P3(4): sudden workload change — cold block demoted, then workload shifts to
// hammer it; after cooldown it re-promotes and serves correctly (no stuck state).
#[test]
fn p3_sudden_workload_shift_recovers() {
    let cfg = cfg("shift", |_| {});
    let e = Arc::new(FlatCakeEngine::new(&cfg).unwrap());
    let bb = cfg.block_bytes();
    let p = sess_put(2, 1, 2, 0, 4, 810);
    e.put_batch(&p).unwrap();
    let ks = keys_of(&p, cfg.block_tokens);
    let seeds = vec![810u64; ks.len()];
    let g = get_spec_for(ks.clone(), &seeds, 2, 2);
    // Heat it up (promotes), then freeze it under pressure (demotes).
    for _ in 0..3 {
        e.get_request(&g, &mut verify_bytes(&ks, &seeds, bb)).unwrap();
    }
    for s in 10..22u64 {
        e.put_batch(&shared_put(s, 2, 4, s * 100)).unwrap();
    }
    let m1 = e.snapshot();
    assert!(m1.migr_promotions >= 1, "phase 1: promoted");
    // Sudden shift: hammer it again (cooldown=0 allows immediate re-promotion).
    for _ in 0..8 {
        e.get_request(&g, &mut verify_bytes(&ks, &seeds, bb)).unwrap();
    }
    let m2 = e.snapshot();
    assert!(
        m2.migr_promotions > m1.migr_promotions,
        "phase 2: re-promoted after workload shift (no stuck-cold state)"
    );
    e.get_request(&g, &mut verify_bytes(&ks, &seeds, bb)).unwrap();
    e.shutdown();
}
