# EXECUTION_LEDGER.md — authoritative record of milestones, evidence, and open issues

Format: one row per milestone. Evidence = command output / test log / file paths. Nothing is marked complete without evidence.

| Milestone | Status | Files changed | Evidence | Failures / unresolved | Next |
|---|---|---|---|---|---|
| M0 Paper reading + research docs | ✅ COMPLETE | research/paper/flatcake-paper.pdf (copy of source), docs/RESEARCH_READINESS.md, docs/PAPER_ANALYSIS.md, docs/PAPER_TO_IMPLEMENTATION.md, docs/OPEN_QUESTIONS.md | 14/14 pages, 16/16 sections, 10/10 figures, 3/3 tables, 1/1 equation inspected; GVA layout + Eq.1 verified verbatim in PDF (pp. 5–6); 18 open questions logged | None concealed; OQ-1..OQ-18 recorded | M1 |
| M1 Environment audit + stack selection | ✅ COMPLETE | docs/ENVIRONMENT_AUDIT.md, DECISIONS.md (D10) | Hardware: i5-14450HX 10C/16T, 15.7 GB RAM (3.9 GB free), RTX 3050 6GB (unused at Level B — GDS is Linux-only), KIOXIA 512GB NVMe, 275 GB free on E:. Toolchains: Python 3.14.7, Git 2.55, gcc 6.3 MinGW.org (rejected), no CMake/WSL/CUDA. **Rust 1.99.1 stable-x86_64-pc-windows-gnu installed via rustup; verified with atomic/threading smoke test (`THREADS-OK 8000`)** | Only ~3.9 GB RAM free → DRAM pool 512 MiB default | M2 |
| M2 Architecture & interfaces + EGM review | ✅ COMPLETE | docs/ARCHITECTURE.md (rev 2), docs/OPEN_QUESTIONS.md (OQ-19..21), DECISIONS.md (D11–D20) | EGM design critic + EGM implementation-readiness dispatched (2 concurrent). Findings: 3 CRITICAL (reclamation race windows; page-cache contamination → adopted FILE_FLAG_NO_BUFFERING; closed-loop replay → adopted open-loop driver), 4 blockers (placement precedence, frozen types.rs, pinned results schema, layout formula) — **all incorporated into ARCHITECTURE.md rev 2**; 12 additional findings triaged | Recovery log cut (OQ-9); multi-file chaining deferred (OQ-11); index fixed-capacity (OQ-16) | M3 |
| M3 Workload generator + baseline | ✅ COMPLETE | src/workload.rs, src/baseline.rs, src/engine_api.rs, src/hbm.rs, src/replay.rs, src/main.rs (gen-workload), tests/baseline_tests.rs, tests/replay_baseline_tests.rs, docs/WORKLOAD_MODEL.md, docs/BENCHMARK_METHODOLOGY.md, docs/FUTURE_GDS_INTEGRATION.md | Unit: 4 lib tests (payload determinism, FNV vector, percentiles, hbm LRU) + 4 baseline tests (round-trip integrity, LRU spill→SSD read-back, staging-exhaustion→recompute, FIFO-bucket reclaim invalidation) + 2 replay integration tests (pipeline integrity, open-loop pacing). CLI: `gen-workload` produces valid JSONL for both regimes with sizing guards firing correctly (session-size + SSD-budget) | 2 bugs found & fixed during dev: session-size arithmetic (layers double-count), pacing closure borrow errors. Metrics given additive `Clone` derive (ledger-noted amendment of frozen types.rs) | M4/M5/M6 |
| M4 FC-GVA | ✅ COMPLETE | src/gva/{mod,arena,index,placement,reclaim}.rs (851 lines), tests/gva_tests.rs (992 lines) | **33/33 tests pass** (+10x flake check, debug-mode debug_asserts). Covers T1 round-trip/boundaries/invalid, T2 index (duplicates idempotent, probe chains, tombstone reuse, 8-thread stress at 0.68 load), T3 publish-after-write + racing-get accounting, T4 per-row placement + spill flip (OQ-6), T5 SSD-sticky/DRAM-reeval appends, T6 reclaim (never frees referenced; tombstone-before-free spy-asserted; oldest-first; advisory when nothing reclaimable; reclaim_payload_ops==0 everywhere) | Deviations accepted (D21/D22/D23): hash stored after winning CAS (pre-CAS store can clobber winner); reclaim drains to LOW watermark (literal while-high reading would stop in [low,high) band); mixed-medium scopes retire across two passes | M5 |
| M5 FC-MAS | 🔜 IN PROGRESS (agent) | src/mas/{mod,scheduler}.rs, tests/mas_tests.rs | (pending) | — | M6 |
| M6 FC-CIO | ✅ COMPLETE | src/cio/{mod,layout,coalesce,stream}.rs (883 lines), tests/cio_tests.rs (596 lines) | **17/17 tests pass**: T11 layer-major round-trip + alignment + session-size guard, T12 coalescing suite (adjacent/overlap/gap-bridge/no-cross-file/shuffled/duplicates/empty/reconstruction-with-sentinels), T13 read-ops bounded (1 op for full restore), T14 tiny-window chunked correctness + padded-tail discard. All NO_BUFFERING real I/O | Deviations accepted (D24): MergedRange.part_lens parallel vec (needed for exact per-desc reconstruction); chunk step rounds window to 4 KiB; alloc_zeroed for pad bytes; per-file recycle when freed==appended; release_session inherent method (engine calls it) | M7 |
| M7 Integrated engine | ⬜ | — | — | — | M8 |
| M8 Correctness & stress | ⬜ | — | — | — | M9 |
| M9 Benchmarking | ⬜ | — | — | — | M10 |
| M10 Performance optimization | ⬜ | — | — | — | M11 |
| M11 Adaptive placement extension | ⬜ | — | — | — | M12 |
| M12 Independent audit + final report | ⬜ | — | — | — | — |

## Elapsed-time records
- Research phase start: 1794349320 (2026-11-11 03:52 IST)
- Research docs completion: recorded in CHANGELOG entry below with epoch timestamp.

## Incident log
- **2026-11-11 (P2)**: Agent dispatches A7/A6 failed with "exceed quota limit" (after earlier 4x "user concurrency limit exceeded"). Account quota for sub-agents is exhausted. Resolution per directive §21: orchestrator absorbs A6 (workload doc), A7 (verification matrix), A8 (audit) assignments directly; specialist review coverage preserved via A1/A2 completed reports + EGM personas at freeze. No work lost — all assignments were fully specified.

## PHASE 2 LEDGER (implementation intensification)

| Milestone | Status | Evidence | Notes |
|---|---|---|---|
| P2-M0 State recovery | ✅ COMPLETE | git log (7 commits intact), full test suite pre-MAS: 66/66; PHASE_2_STATUS.md written from actual repo inspection | No work overwritten; MAS module breaks lib build temporarily (its own file, agent mid-write) |
| P2-M1 Agent activation | ✅ COMPLETE (with documented limitation) | Platform rejected 4/5 specialists: **hard concurrency limit = 2 agents** (error: user concurrency limit exceeded). Active: FC-MAS + A2. QUEUED with full assignments: A1, A6, A7, A8 — resumed serially as slots free (directive §3 explicitly permits queued agents; §21 blocker policy applied) | Incident recorded; queue managed by orchestrator |
| P2-M2 Interface contracts | 🔜 | contracts frozen at Phase-0 (types.rs commit 92da2a6); A2 verifies as-built consistency | — |
| P2-M3 Component tests | ✅ COMPLETE | FC-GVA 33/33, FC-CIO 17/17, **FC-MAS 15/15** (agent fixed its backpressure off-by-one after orchestrator defect report; boundary decision documented in module docs) | 83 tests total |
| P2-M4 Baseline/workloads | ✅ code complete | baseline 4/4 + replay 2/2 + A6 doc pending | — |
| P2-M5 Vertical slice | ✅ COMPLETE | Full build green; engine.rs integrates gva/cio/mas; A2 findings (M1-M4, R5) + A1 findings (unpublished spill, cross-scope reader pins, metric parity) all fixed;  runs both systems end-to-end with byte-exact integrity: **flatcake 82.52% hit vs baseline 35.32%** at identical budgets; bench CLI writes pinned results schema | Modeled-recompute caveat for latency comparisons recorded for M9 |
| P2-M6 Reliability | 🔜 | A7 verification matrix (write now, run when green) | — |
| P2-M7 Integration review | 🔜 | A1 + A8 + EGM persona | — |
| P2-M8 Freeze | ⬜ | — | — |

## Phase 3 ledger (M9/M11/M12)

| Milestone | Status | Evidence | Notes |
|---|---|---|---|
| M9 Benchmarks | ✅ COMPLETE | 8 cold-start runs (results/*.json + CSVs + plots); docs/RESULTS.md; EXPERIMENT_LOG rows; degenerate first sweep caught by run inspection and re-run with corrected ratios | FlatCake 100% hits vs baseline 86-99.6%; 109x fewer physical ops; write-amp 0.995x vs 3.269x; integrity 0 everywhere |
| M10 Optimization | ✅ COMPLETE (scope: measure-first audit) | docs/PHASE_2_AUDIT.md U1/U2 identify M10 candidates with measured evidence; no premature optimization applied (directive Part 15: simplest implementation meeting measured requirements) | Profiling artifacts: results/io_probe.json |
| M11 Adaptive extension | ✅ COMPLETE (honest negative) | 24 runs x 2 generator versions; docs/ADAPTIVE_EXTENSION.md results; T19 shape tests caught a generator bug first, re-run confirmed verdict | adaptive == static parity; fixed_dram collapses; H0 confirmed |
| M12 Final audit + report | ✅ COMPLETE | docs/AUDIT_REPORT.md (self-audit; EGM agent quota-blocked, evidence from A1/A2 independent agent reports + command outputs); docs/FINAL_REPORT.md; 91/91 tests | — |

## P3 ledger (medium-aware scheduling phase)

| Item | Status | Evidence | Notes |
|---|---|---|---|
| P3 architecture (additive registry APIs, config knobs, migration core) | ✅ | 101/101 tests; docs/P3_REPORT.md | Default config = paper-faithful write-once (migration off); regression-guard test pins it |
| P3 tests (10 deterministic) | ✅ | tests/p3_migration_tests.rs — incl. adversarial: hot/cold flips, RAM saturation, SSD-heavy, concurrent migration, thrash/cooldown, loop bound, starvation, headroom | Found + fixed 2 engine-design gaps during testing: hbm-resident promotion source; recency-aware hot-protection expiry |
| P3 benchmark | ⏳ DEFERRED (I/O hygiene) | benchmarks/run_p3.sh ready; blocked until M9 sweep releases the disk (unbuffered-timing contamination) | Will be reported honestly — migration may lose to write-once (the paper predicts little headroom); nothing claimed before measurement |
| Incident | git checkout reverted UNCOMMITTED P3 engine work mid-phase (orchestrator error); recovered by re-running the patch script; immediate-commit policy adopted afterwards | ledger note; commit "P3 WIP" | — |

## P4 ledger (final phase)

| Item | Status | Evidence |
|---|---|---|
| Track A integration audit | DONE | docs/P4_INTEGRATION_AUDIT.md (15 findings, agent-authored) |
| Track B recovery log + batching + dedup + metrics | DONE impl / dedup disabled | 7 P4 tests; audit items 1-14 addressed; D-P4-1 dedup defect found by stress test -> disabled by default |
| Track C tests | DONE | 7 deterministic P4 tests; 111 total, 1 known-flaky CRITICAL (D-P4-2 concurrent integrity, ~50% repro, engine-level, pre-dates P4 additions) |
| Track D benchmarks | DONE | recovery overhead ~0%; batching mechanism verified; no improvement claims beyond measurement |
| Track E review + verdict | DONE | warnings-as-errors clean; diff reviewed; docs/P4_REPORT.md verdict: FAIL(conditional) on concurrent-integrity criterion, all else pass |
