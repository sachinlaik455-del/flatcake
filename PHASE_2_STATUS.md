# PHASE_2_STATUS.md — recovered state, assignments, and acceptance criteria

Written at Phase 2 start (P2-M0), after inspecting the actual repository (git log, file tree, test run). No Phase-1 work was overwritten or reset.

## Recovered state (evidence-based)

| Area | State | Evidence |
|---|---|---|
| Research docs (M0) | ✅ complete | docs/RESEARCH_READINESS.md (14/14 pages), PAPER_ANALYSIS.md, PAPER_TO_IMPLEMENTATION.md (T1–T24), OPEN_QUESTIONS.md (OQ-1..21) |
| Environment (M1) | ✅ complete | docs/ENVIRONMENT_AUDIT.md; Rust 1.99.1 GNU, Python 3.14.7 + matplotlib 3.11.2 |
| Architecture (M2) | ✅ complete rev 2 | docs/ARCHITECTURE.md after EGM design review; frozen contracts in src/types.rs, config.rs, util.rs (Phase-0 freeze commit 92da2a6) |
| FC-GVA (M4) | ✅ complete | src/gva/** (851 lines) + tests/gva_tests.rs: **33/33 pass**, 10× flake check |
| FC-CIO (M6) | ✅ complete | src/cio/** (883 lines) + tests/cio_tests.rs: **17/17 pass**, real NO_BUFFERING SSD I/O |
| Baseline + workloads (M3) | ✅ complete | src/baseline.rs, src/workload.rs, src/replay.rs, src/hbm.rs, src/engine_api.rs, src/results.rs; tests/baseline_tests.rs (4), replay_baseline_tests.rs (2 incl. integrity + open-loop pacing) |
| FC-MAS (M5) | 🔜 agent in flight | src/mas/scheduler.rs (373 lines) actively being edited by the FC-MAS agent (mtime < 5 s old at P2-M0); its module declaration currently breaks the lib build for all targets until it compiles — **known, temporary, single blocker** |
| Engine integration (M7) | 🔜 written, undeclared | src/engine.rs complete against frozen contracts + agent APIs; `pub mod engine;` pending MAS landing |
| Bench CLI + demo | ⬜ pending engine | — |
| Benchmarks / adaptive / final audit | ⬜ later phase | methodology + design docs already in place (BENCHMARK_METHODOLOGY.md, ADAPTIVE_EXTENSION.md) |

Stack (Phase 1 selection, unchanged): **std-only Rust** lib+bin crate `fckv` + Python for plots. No external crates.

## Technical blockers

1. **Lib build red until FC-MAS agent's module compiles** (its `pub mod mas;` is declared; errors are inside its own file, being fixed by its own iteration loop). All cargo activity that links the lib is gated on this — testing/audit agents are instructed to write now and run after the orchestrator announces green.
2. Nothing else. No hardware blockers at Level B (GPU/GDS boundary documented in FUTURE_GDS_INTEGRATION.md).

## Agent assignments (Phase 2)

| Agent | Role | Owns (exclusive) | Depends on | Status |
|---|---|---|---|---|
| FC-MAS (in flight) | Scheduler module | src/mas/**, tests/mas_tests.rs | frozen types | IMPLEMENTING |
| A1 fidelity | Paper-fidelity review | docs/PHASE_2_PAPER_FIDELITY.md | read-only code access | DISPATCHED |
| A2 architect | Contract verification + integration doc | docs/PHASE_2_ARCHITECTURE.md | read-only code access | DISPATCHED |
| A6 workload-doc | Workload/baseline doc | docs/PHASE_2_WORKLOADS.md | read-only | DISPATCHED |
| A7 testing | Verification matrix + cross-component integration tests | tests/p2_matrix_tests.rs, docs/PHASE_2_TEST_REPORT.md | build green (MAS) to RUN; write now | DISPATCHED |
| A8 audit | Performance + correctness audit, profiles | docs/PHASE_2_AUDIT.md, scripts/** | build green to profile | DISPATCHED |
| orchestrator | Integration, engine, bench CLI, demo, vertical slice, ledger | src/engine.rs, src/main.rs, lib.rs, docs, git | MAS landing | ACTIVE |

Conflict rule: no two agents share a writable file. Cross-module defects found by A1/A7/A8 are REPORTED to the orchestrator, never self-fixed in another agent's file.

## Phase 2 acceptance criteria (from the directive §22, mapped)

1–2. State recovered; ≥6 agents activated (5 dispatched + MAS in flight + orchestrator). 3. Ownership/interfaces documented (this file + AGENT_OWNERSHIP.md + A2's doc). 4–5. FC-GVA/FC-CIO/FC-MAS real implementations with passing tests (2 done, 1 in flight). 6. FC-CIO does real local SSD I/O with payload-reconstruction tests ✓. 7–8. Baseline retrieval correct ✓, traces deterministic ✓. 9–10. Engine end-to-end lifecycle + safe reclamation → vertical slice (P2-M5) + A7 tests. 11. Bounded memory/queues verified by A7. 12–13. Builds on Windows 11 ✓; reproducible commands in every report. 14. Deviations documented (ledger D1–D24 + A1's report). 15–16. Independent audit (A8 + EGM persona at P2-M7) + ledger accuracy.
