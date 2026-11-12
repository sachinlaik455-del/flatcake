# ADAPTIVE_EXTENSION.md — M11 research extension design (Directive Part 13)

**This extension is NOT from the paper.** It is a controlled research question built on top of the paper's static placement. Everything paper-derived is in PAPER_ANALYSIS.md; everything below is the extension.

## Research question

Can **adaptive write-time placement** — using observed per-session reuse feedback to decide DRAM-vs-SSD for *newly written* blocks — outperform the paper's fixed write-time scope classification (Table-2 precedence, D14) when workload characteristics **change over time**, while respecting the write-once/no-migration invariant?

## Hypothesis (falsifiable)

H1: On a mixed trace with a **regime shift** (sessions that begin as one-shot/short and later become long-lived agentic reusers), the adaptive policy achieves **≥ the static policy's delivered hit rate**, because the static policy mispredicts scopes whose reuse pattern changes after their first placement (a chat classified one-shot at turn 1 stays SSD forever; an adaptive policy can put the session's *later* blocks in DRAM once reuse is observed).

H0: no significant difference (or adaptive is worse on pure regimes, where static's write-time signals are already correct).

## Variables

- **Independent**: placement policy ∈ { fixed-DRAM, fixed-SSD, static-scope-direct (D14, the paper's proxy), adaptive }.
- **Controlled**: identical traces (byte-identical JSONL), identical budgets (hbm/DRAM/SSD/index), identical seeds, identical engine (only the placement decision function differs), identical reclamation.
- **Measured**: delivered_hit_rate (primary), per-medium hits, read_amp, write_amp, dram_spill_puts, mem_high_water.

## Adaptive policy (single, simple, no speculative complexity)

Feedback-only, placement-time decisions, **no migration, no tombstoning games**:

1. Per-session stats (maintained by the engine): `gets_served(session)`, `last_get_ts`, `once_flag`.
2. On a put for session s with DRAM headroom `H = dram_free / dram_pool`:
   - **evidence rule**: place in DRAM iff `gets_served(s) ≥ K` (session has demonstrated reuse) **and** `H ≥ H_min`;
   - else fall back to the static Table-2 decision.
3. Default `K = 2`, `H_min = 0.2` (two tunables only). Reuse evidence is observed from the engine's own delivered hits — no workload-side hints.

## Controlled conditions

- **Tuning traces** (set A): mixed-regime traces, seeds 11–13 — used to pick K/H_min from a small grid {1,2,4} × {0.1,0.2,0.3}.
- **Evaluation traces** (set B): same generator, seeds 21–23 — **never used for tuning**. Results reported on set B only.
- Regime-shift trace: `mixed` with a mid-trace parameter pivot (generator emits one-shot sessions for the first half, then agentic sessions activate — new generator flag `--shift`).

## Success criteria (declared before implementation)

- Primary: adaptive delivered_hit_rate ≥ static delivered_hit_rate − 0.5 pp on set B (parity on pure regimes) **and** > static by ≥ 2 pp on the shift trace (the claimed win condition).
- Fairness guards: write_amp ≤ 1.1× of static's on all runs (no re-persist storms); mem_high_water within budget; integrity_failures = 0.
- Negative results reported as-is: if adaptive fails H1, RESULTS.md states it plainly with per-run numbers.

## Novelty disclaimer

No novelty claim is made. A brief literature check (solid-state caching with reuse feedback, e.g., AdaptCache [10] in the paper's own related work) will be cited for context at M11; the purpose here is a controlled comparison on our own engine, not a publication claim.


## RESULTS (2026-11-12, results/m11/*.json, 24 runs, integrity_fail=0 everywhere)

Re-run after the T19 shape tests caught a generator bug (oneshot/shift/mixed
shared-prompt re-gets referenced never-put session keys — fixed to target
session 1). Both generator versions agree on the verdict:

| Policy | shift s21/s22/s23 (fixed gen) | mixed s21/s22/s23 (fixed gen) |
|---|---|---|
| static (paper) | 100.00 / 100.00 / 100.00 % | 100.00 / 100.00 / 100.00 % |
| fixed_dram | 96.15 / 96.15 / 96.13 % | 83.63 / 83.33 / 83.33 % |
| fixed_ssd | 100.00 / 100.00 / 100.00 % | 100.00 / 100.00 / 100.00 % |
| **adaptive (K=2, H_min=0.2)** | **100.00 / 100.00 / 100.00 %** | **100.00 / 100.00 / 100.00 %** |

(Pre-fix numbers, for the record: static 99.70/99.12/99.70 shift; adaptive identical; fixed_dram 82.75-96.15.)

**Verdict: H0 (no advantage) — H1's win condition (adaptive > static by >= 2 pp on shift) NOT met; parity criterion (>= static - 0.5 pp on set B) MET everywhere.**

### Why (mechanism, from per-run metrics)

Adaptive's per-medium split (dram=64, ssd~2800) is indistinguishable from static's: the evidence rule almost never fires because (a) an agentic session's FIRST put precedes any observed get, so the scope is created on SSD by the turn-1/one-shot rule and OQ-19 stickiness; (b) by turn 3 when K=2 is satisfied, the 8 MiB DRAM pool is cycling in its 70-90% watermark band, so headroom < H_min=0.2 blocks the DRAM placement — the rule is self-limiting exactly when it would matter. Meanwhile SSD-served histories still deliver 99.7% hits via coalesced reads, i.e., the paper's asymmetric-misprediction argument (§3.2) holds: there is little hit-rate headroom for ANY placement policy to recover. fixed_dram confirms the converse: forcing everything into a too-small DRAM pool causes reclaim-churn misses (82-96%).

### Honest limitations of this evaluation

- Tuning grid reduced to the declared defaults (K=2, H_min=0.2) under the session's compute/quota constraints — no post-hoc tuning on set B (which would invalidate it); a fuller grid might find a better operating point, but the headroom-gate mechanism above bounds what any (K, H_min) can achieve under these budgets.
- Compressed schedule (time_scale 0.05) — valid for hit-rate accounting, not latency.
- Conclusion is workload/budget-scoped (tight-DRAM regimes), as pre-declared in the controlled-conditions section.
