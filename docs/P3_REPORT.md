# P3_REPORT.md — FC-MAS medium-aware scheduling (Phase 3 engineering report)

Builds on P1 (tiered-storage foundation: `src/baseline.rs`, `src/workload.rs`, `src/replay.rs`) and P2 (FC-GVA direct placement: `src/gva/**`, engine integration). **No existing component was rewritten**; P3 adds migration/eviction intelligence as opt-in policies on top of the paper-faithful write-once default.

## Architecture changes (minimal, additive)

1. **Registry observability + extent removal** (`src/gva/reclaim.rs`, additive): `active_readers(session)`, `active_writers(session)`, `remove_dram_extent`, `remove_ssd_extent` — migration needs refcount visibility (race-free extent moves) and ownership updates (prevents double-free at reclaim).
2. **Config** (`src/config.rs`): `migration_enabled` (0/1, **default 0 = paper-faithful write-once**), `eviction_policy` (`session_extents` paper default | `medium_aware`), `migration_cooldown_ms` (anti-thrash + recency window), `migration_max_per_get` (per-pass budget). Reuses `adaptive_k`/`adaptive_h_min` as promotion thresholds (no knob proliferation).
3. **Engine migration core** (`src/engine.rs`):
   - **Promotion** (SSD→RAM): after a request delivers SSD-resident blocks with heat ≥ K and DRAM headroom ≥ H_min, the hottest blocks move into the arena **using bytes already in hand (zero extra SSD reads)**. GVA swap protocol: `invalidate(old) → insert_pending(new) → publish(new)` — readers inside the window see Miss and recompute (immutable bytes stay correct); after the swap every lookup resolves to the new location.
   - **Demotion** (RAM→SSD, `eviction_policy = medium_aware`): when DRAM crosses its high watermark, cold blocks (heat < K **or** recency expired) move to their session's SSD scope, oldest-served first, budget-capped — **before** falling back to whole-session reclamation. Hot blocks (heat ≥ K, recency fresh) are protected.
   - **Deferred frees**: migration never frees an extent in place — it lands on a `pending_frees` list returned to its pool only when the owning scope has zero in-flight readers/writers (closes the read-vs-free race; same discipline as D15). Bounded list with a documented pathological fallback.
   - **Anti-thrash**: per-block migration cooldown (promotions AND demotions record `last_migrated`), recency-based hot-protection, per-pass budget, SSD-pressure guard (demotion refuses to run when SSD is itself over watermark — no pressure trading).
4. **Metrics** (`src/types.rs`, `src/results.rs`): `migr_promotions`, `migr_demotions`, `migr_bytes_moved`, `migr_cooldown_skips` — all zero when migration is disabled.

The scheduler's existing dual-queue/HRRN retrieval scheduling (P2) is unchanged; P3's placement/eviction intelligence rides on the same measurement substrate.

## Files modified

`src/gva/reclaim.rs` (+51), `src/config.rs` (+34), `src/types.rs` (+6), `src/results.rs` (+4), `src/engine.rs` (+~260 migration core + hooks), `tests/p3_migration_tests.rs` (+544), `configs/p3.conf`, `benchmarks/run_p3.sh`. No other module touched; no public interface removed.

## Tests (10 new, all deterministic — `tests/p3_migration_tests.rs`)

| Test | Adversarial requirement |
|---|---|
| `p3_promotion_integrity_and_mapping` | hot SSD block promotes; mapping swaps; bytes exact pre/post; DRAM serving after hbm eviction (fillers) |
| `p3_demotion_under_pressure_hot_blocks_stay` | RAM saturation: cold blocks demote, hot blocks survive (frequency+recency) |
| `p3_cooldown_blocks_immediate_remigration` | just-demoted block cannot re-promote inside the cooldown; skip observable |
| `p3_no_migration_loops_under_steady_state` | 40 rounds of pressure: promotion churn flat in steady state (no loops); demotions ≤ inflow×budget |
| `p3_promotion_eventually_serves_all_hot_blocks` | starvation: budget 2/request still promotes every hot block; zero misses at end |
| `p3_promotion_respects_headroom_floor` | H_min blocks promotion into a nearly-full RAM pool |
| `p3_default_config_is_paper_faithful_write_once` | **regression guard**: default config ⇒ all migration counters zero under identical pressure (P1/P2 behavior intact) |
| `p3_concurrent_migration_integrity` | 4 reader threads + interleaved migration pressure: integrity + accounting invariant |
| `p3_placement_decisions_are_deterministic` | identical signals ⇒ identical placement across fresh engines |
| `p3_sudden_workload_shift_recovers` | hot→cold→hot transitions recover (no stuck-cold state) |

**Full suite: 101/101 passing** (91 pre-P3 + 10 new) — P1/P2 functionality intact.

## Benchmark

`benchmarks/run_p3.sh`: migration-favorable workload (agentic sessions classified SSD at turn 1 by the one-shot rule but heavily re-accessed — the exact shape where static placement serves every reuse from SSD and migration can win) vs the write-once default, identical budgets, cold start. **Deferred until the background M9 sweep releases the disk** (unbuffered-I/O timing hygiene; sweep is mid-run at report time) — results will be appended below when measured; nothing is claimed before then.

### Results (pending measurement — placeholder, per the no-fabrication rule)

| Metric | write-once (static) | medium-aware migration |
|---|---|---|
| delivered hit rate | pending | pending |
| physical read ops | pending | pending |
| p50 / p95 / p99 latency | pending | pending |
| migr_promotions / demotions / bytes | 0 / 0 / 0 | pending |
| SSD bytes written (write amp) | pending | pending |

## Regressions / unresolved issues

- None open: 91 pre-P3 tests pass unchanged (the regression guard test pins the default path).
- Documented limitation: promotion draws bytes from in-hand sources only (request delivery or hbm window); a hot SSD block never re-requested is never promoted (by design — no background scanner per the no-background-services constraint).
- Documented limitation: heat has no decay timer; recency protection expires via the cooldown window instead (single-knob choice, `migration_cooldown_ms`).

## Verdict

**P3: PASS on implementation and correctness criteria** (deterministic decisions, migration integrity + address consistency, no thrash/starvation/loops in tested workloads, bounded queues/memory, 101/101 tests, P1/P2 intact). **Benchmark verdict: pending the deferred measurement above** — it will be reported honestly either way.
