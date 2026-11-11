# AGENT_OWNERSHIP.md — exclusive file ownership (Phase 2, directive §6/§18)

No two agents may write the same file. Defects found in another agent's module are reported to the orchestrator, not self-fixed. Shared manifests (Cargo.toml, Cargo.lock, lib.rs, fckv.conf) are owned by the orchestrator only.

| Owner | Files (exclusive write) |
|---|---|
| orchestrator | src/main.rs, src/lib.rs, src/types.rs, src/config.rs, src/util.rs, src/engine.rs, src/engine_api.rs, src/hbm.rs, src/replay.rs, src/baseline.rs, src/results.rs, src/workload.rs, fckv.conf, docs/ARCHITECTURE.md, docs/*_STATUS.md, EXECUTION_LEDGER.md, DECISIONS.md, CHANGELOG.md, README.md, .gitignore, git history |
| FC-MAS agent | src/mas/**, tests/mas_tests.rs |
| A1 paper-fidelity | docs/PHASE_2_PAPER_FIDELITY.md (read-only elsewhere) |
| A2 architect | docs/PHASE_2_ARCHITECTURE.md (read-only elsewhere) |
| A6 workload-doc | docs/PHASE_2_WORKLOADS.md (read-only elsewhere) |
| A7 testing | tests/p2_matrix_tests.rs, docs/PHASE_2_TEST_REPORT.md |
| A8 audit | docs/PHASE_2_AUDIT.md, scripts/** |
| (frozen — Phase-0) | src/types.rs, src/config.rs, src/util.rs: amendments only by orchestrator with ledger note (agents propose, never edit) |

Phase-1 historical ownership (work complete, evidence in ledger): FC-GVA agent → src/gva/** + tests/gva_tests.rs; FC-CIO agent → src/cio/** + tests/cio_tests.rs; benchmark analysis → python/plot_results.py (orchestrator).
