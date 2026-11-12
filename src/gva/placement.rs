//! gva::placement — frozen write-time placement precedence (T4/T5, D14).
//!
//! **This is the OQ-19 reference interpretation of paper Table 2.** The
//! paper's Table-2 rows carry no priority order and genuinely conflict under
//! append stickiness ("short-turn chat → DRAM" vs "turn = 1 → one-shot →
//! SSD"). The frozen precedence (ARCHITECTURE §4) resolves the tension by
//! evaluating at scope creation and leaning on the paper's own
//! asymmetric-cost argument (§3.2): a reusable block on SSD is served by
//! coalesced GDS reads, not a miss — SSD-stuck short chats are acceptable
//! behavior, not a bug (OQ-20).
//!
//! Evaluated per **scope creation** (first put of a session scope). Appends
//! inherit the scope medium **if the scope is SSD** (SSD on-disk contiguity
//! is a paper invariant, §3.4); appends to DRAM scopes re-evaluate the rows
//! fresh (DRAM grouping is irrelevant; no migration either way). The engine
//! passes `sticky = registry.sticky_medium(session)`; `Some(Ssd)`
//! short-circuits, `Some(Dram)` / `None` evaluate all rows in order.
//!
//! The DRAM-full spill (OQ-6, EGM #5) is not a signal of this function: when
//! the arena reserve fails, the engine places that put on SSD and flips the
//! scope (`reclaim::ScopeRegistry::flip_to_ssd`), after which stickiness
//! keeps every subsequent append on SSD.

use crate::config::Config;
use crate::types::{Medium, PlacementSignals};

/// Frozen precedence decision (ARCHITECTURE §4 table, verbatim).
/// M11 policy dispatch (ADAPTIVE_EXTENSION.md). `gets_served` = observed served
/// gets for the session (evidence input); None disables the evidence rule.
pub fn decide_policy(
    sig: &PlacementSignals,
    sticky: Option<Medium>,
    cfg: &Config,
    gets_served: Option<u64>,
    dram_headroom: f64,
) -> Medium {
    match cfg.placement_policy.as_str() {
        "fixed_dram" => Medium::Dram,
        "fixed_ssd" => Medium::Ssd,
        "adaptive" => {
            if let Some(n) = gets_served {
                if n >= cfg.adaptive_k && dram_headroom >= cfg.adaptive_h_min {
                    return Medium::Dram;
                }
            }
            decide(sig, sticky, cfg)
        }
        _ => decide(sig, sticky, cfg),
    }
}

pub fn decide(sig: &PlacementSignals, sticky: Option<Medium>, cfg: &Config) -> Medium {
    // SSD scope stickiness preserves on-disk contiguity (§3.4).
    if sticky == Some(Medium::Ssd) {
        return Medium::Ssd;
    }
    // Row 1: shared prefix (refcount > 1) → DRAM (hot shared reuse).
    if sig.refcount > 1 {
        return Medium::Dram;
    }
    // Row 2: agentic long-context session (ctx ≥ L_agent) → SSD.
    if sig.ctx_tokens >= cfg.l_agent_tokens {
        return Medium::Ssd;
    }
    // Row 3: long reply (≥ τ) → SSD.
    if sig.reply_tokens >= cfg.tau_tokens {
        return Medium::Ssd;
    }
    // Row 4: turn-1 unshared one-shot → SSD.
    if sig.turn == 1 {
        return Medium::Ssd;
    }
    // Row 5: short-turn chat (turn > 1 ∧ reply < τ) → DRAM.
    if sig.turn > 1 && sig.reply_tokens < cfg.tau_tokens {
        return Medium::Dram;
    }
    // Fallback → DRAM.
    Medium::Dram
}
