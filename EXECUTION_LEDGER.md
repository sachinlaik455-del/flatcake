# EXECUTION_LEDGER.md — authoritative record of milestones, evidence, and open issues

Format: one row per milestone. Evidence = command output / test log / file paths. Nothing is marked complete without evidence.

| Milestone | Status | Files changed | Evidence | Failures / unresolved | Next |
|---|---|---|---|---|---|
| M0 Paper reading + research docs | ✅ COMPLETE | research/paper/flatcake-paper.pdf (copy of source), docs/RESEARCH_READINESS.md, docs/PAPER_ANALYSIS.md, docs/PAPER_TO_IMPLEMENTATION.md, docs/OPEN_QUESTIONS.md | 14/14 pages, 16/16 sections, 10/10 figures, 3/3 tables, 1/1 equation inspected; GVA layout + Eq.1 verified verbatim in PDF (pp. 5–6); 18 open questions logged | None concealed; OQ-1..OQ-18 recorded | M1 |
| M1 Environment audit + stack selection | ✅ COMPLETE | docs/ENVIRONMENT_AUDIT.md, DECISIONS.md (D10) | Hardware: i5-14450HX 10C/16T, 15.7 GB RAM (3.9 GB free), RTX 3050 6GB (unused at Level B — GDS is Linux-only), KIOXIA 512GB NVMe, 275 GB free on E:. Toolchains: Python 3.14.7, Git 2.55, gcc 6.3 MinGW.org (rejected), no CMake/WSL/CUDA. **Rust 1.99.1 stable-x86_64-pc-windows-gnu installed via rustup; verified with atomic/threading smoke test (`THREADS-OK 8000`)** | Only ~3.9 GB RAM free → DRAM pool 512 MiB default | M2 |
| M2 Architecture & interfaces + EGM review | ✅ COMPLETE | docs/ARCHITECTURE.md (rev 2), docs/OPEN_QUESTIONS.md (OQ-19..21), DECISIONS.md (D11–D20) | EGM design critic + EGM implementation-readiness dispatched (2 concurrent). Findings: 3 CRITICAL (reclamation race windows; page-cache contamination → adopted FILE_FLAG_NO_BUFFERING; closed-loop replay → adopted open-loop driver), 4 blockers (placement precedence, frozen types.rs, pinned results schema, layout formula) — **all incorporated into ARCHITECTURE.md rev 2**; 12 additional findings triaged | Recovery log cut (OQ-9); multi-file chaining deferred (OQ-11); index fixed-capacity (OQ-16) | M3 |
| M3 Workload generator + baseline | ✅ COMPLETE | src/workload.rs, src/baseline.rs, src/engine_api.rs, src/hbm.rs, src/replay.rs, src/main.rs (gen-workload), tests/baseline_tests.rs, tests/replay_baseline_tests.rs, docs/WORKLOAD_MODEL.md, docs/BENCHMARK_METHODOLOGY.md, docs/FUTURE_GDS_INTEGRATION.md | Unit: 4 lib tests (payload determinism, FNV vector, percentiles, hbm LRU) + 4 baseline tests (round-trip integrity, LRU spill→SSD read-back, staging-exhaustion→recompute, FIFO-bucket reclaim invalidation) + 2 replay integration tests (pipeline integrity, open-loop pacing). CLI: `gen-workload` produces valid JSONL for both regimes with sizing guards firing correctly (session-size + SSD-budget) | 2 bugs found & fixed during dev: session-size arithmetic (layers double-count), pacing closure borrow errors. Metrics given additive `Clone` derive (ledger-noted amendment of frozen types.rs) | M4/M5/M6 |
| M4 FC-GVA | ⬜ | — | — | — | M5 |
| M5 FC-MAS | ⬜ | — | — | — | M6 |
| M6 FC-CIO | ⬜ | — | — | — | M7 |
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
(none yet)
