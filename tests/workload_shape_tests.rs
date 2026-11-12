//! T19 statistical-shape self-checks (targets declared in WORKLOAD_MODEL.md;
//! implemented after the A1 fidelity review flagged their absence).
//! Accounting follows the paper's Table-1 convention: a put is the block's first
//! access; reuse distance = intervening REQUESTS (puts+gets, replay-order index)
//! between consecutive accesses of the same block.
//! These tests caught a real generator bug: oneshot/shift/mixed "shared prompt
//! re-gets" previously referenced session-s keys that were never put — the reuse
//! read non-existent blocks (fixed to target session 1).
//! Hybrid (shared_with) gets are resolved exactly as the engine resolves them:
//! shared prefix = the shared session's own put extent at start 0; own prefix =
//! the requesting session's blocks.

use fckv::workload::{generate, GenParams, Op};
use std::collections::HashMap;

fn params(regime: &str, seed: u64, out: &str, turns: u64) -> GenParams {
    GenParams {
        regime: regime.into(),
        sessions: 40,
        turns,
        layers: 4,
        blocks_per_turn: 4,
        shared_blocks: 8,
        fanout: 1,
        reuse_rate: 0.2,
        qps: 8.0,
        seed,
        out: out.into(),
        block_tokens: 256,
        bytes_per_token: 32,
        gpu_file_bytes: 1 << 28,
        // Shape tests only emit JSONL; the SSD-budget guard is for bench sizing.
        ssd_budget_bytes: 256 << 20,
    }
}

#[derive(Default, Debug)]
struct Shape {
    put_blocks: u64,
    gets: u64,
    max_ctx: u64,
    accessed_unique: usize,
    once_accessed: usize,
    reuse_median: u64,
}

/// Access stream = puts (first access) + gets (reuse); request index = record position.
/// Hybrid gets are resolved like the engine: shared session's put extent + own prefix.
fn shape(records: &[fckv::workload::TraceRecord], bt: u64) -> Shape {
    let mut s = Shape::default();
    let mut counts: HashMap<(u64, u32, u64), u32> = HashMap::new();
    let mut last_req: HashMap<(u64, u32, u64), usize> = HashMap::new();
    let mut dists: Vec<u64> = Vec::new();
    // session -> (start_blk, n_blk) of its puts (own-prefix extents)
    let mut extents: HashMap<u64, Vec<(u64, u32)>> = HashMap::new();
    let mut shared_n: u32 = 0;
    for (ridx, r) in records.iter().enumerate() {
        match r.op {
            Op::Put => {
                extents.entry(r.session).or_default().push((r.start_blk, r.n_blk));
                if r.session == 1 {
                    shared_n = shared_n.max(r.n_blk);
                }
                for l in 0..r.layers {
                    for i in 0..r.n_blk {
                        let k = (r.session, l, (r.start_blk + i as u64) * bt);
                        let c = counts.entry(k).or_insert(0);
                        if let Some(prev) = last_req.insert(k, ridx) {
                            dists.push((ridx - prev) as u64);
                        }
                        *c += 1;
                        s.put_blocks += 1;
                    }
                }
                if r.session != 1 {
                    s.max_ctx = s.max_ctx.max(r.ctx_tokens);
                }
            }
            Op::Get => {
                s.gets += 1;
                let mut blocks: Vec<(u64, u32, u64)> = Vec::new();
                if r.shared_with > 0 {
                    // Shared prefix: the shared session's actual put extent at start 0.
                    for l in 0..r.layers {
                        for i in 0..shared_n {
                            blocks.push((r.shared_with, l, i as u64 * bt));
                        }
                    }
                }
                // Own prefix: this session's blocks [0, n_blk) bounded by its puts
                // (generator always emits own-prefix gets at start_blk 0).
                let own_extent = extents
                    .get(&r.session)
                    .map(|v| v.iter().map(|(st, n)| st + *n as u64).max().unwrap_or(0))
                    .unwrap_or(0);
                let own_n = (r.n_blk as u64).min(own_extent);
                for l in 0..r.layers {
                    for i in 0..own_n {
                        blocks.push((r.session, l, i as u64 * bt));
                    }
                }
                for k in blocks {
                    let c = counts.entry(k).or_insert(0);
                    if let Some(prev) = last_req.insert(k, ridx) {
                        dists.push((ridx - prev) as u64);
                    }
                    *c += 1;
                }
            }
        }
    }
    s.accessed_unique = counts.len();
    s.once_accessed = counts.values().filter(|&&c| c == 1).count();
    dists.sort_unstable();
    s.reuse_median = dists.get(dists.len() / 2).copied().unwrap_or(0);
    s
}

#[test]
fn t19_regime_shapes_match_paper_motivations() {
    // Agentic turns sized so session ctx crosses l_agent (65536 default):
    // 18 turns x 4 blocks x 4 layers x 256 tokens = 73,728.
    let ag = format!("data/test/t19-ag-{}.jsonl", std::process::id());
    let (n, _) = generate(&params("agentic", 31, &ag, 18)).expect("gen agentic");
    assert!(n > 20);
    let agentic = shape(&fckv::workload::parse_trace(&ag).expect("parse"), 256);

    let os = format!("data/test/t19-os-{}.jsonl", std::process::id());
    let (n2, _) = generate(&params("oneshot", 32, &os, 8)).expect("gen oneshot");
    assert!(n2 > 20);
    let oneshot = shape(&fckv::workload::parse_trace(&os).expect("parse"), 256);

    // Regime B (agentic; paper: Claude Code once-access 16.6%, mean reuse dist 0.85):
    let ag_once = agentic.once_accessed as f64 / agentic.accessed_unique.max(1) as f64;
    assert!(
        ag_once < 0.5,
        "agentic must be reuse-dominated: once_frac={ag_once:.2} shape={agentic:?}"
    );
    assert!(
        agentic.reuse_median <= 3,
        "agentic reuse distance median {} must be near-immediate (paper mean 0.85 requests)",
        agentic.reuse_median
    );
    assert!(
        agentic.max_ctx >= 65536,
        "agentic ctx must cross l_agent (65536): max={}",
        agentic.max_ctx
    );

    // Regime A (oneshot; paper: 53-94% once-access, heavy-tailed reuse distances):
    let os_once = oneshot.once_accessed as f64 / oneshot.accessed_unique.max(1) as f64;
    assert!(
        os_once > 0.8,
        "oneshot must be once-access-dominated: once_frac={os_once:.2} shape={oneshot:?}"
    );
    assert!(
        oneshot.max_ctx < 65536,
        "oneshot ctx must stay under l_agent: max={}",
        oneshot.max_ctx
    );

    // Bimodality contrast (paper Table 1: 0.85 vs 728 mean reuse distance).
    assert!(
        oneshot.reuse_median > agentic.reuse_median * 3,
        "regime contrast: oneshot median {} vs agentic median {}",
        oneshot.reuse_median,
        agentic.reuse_median
    );
}
