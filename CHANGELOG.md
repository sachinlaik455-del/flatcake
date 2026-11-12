# CHANGELOG.md

## 2026-11-13 — P4: FC-CIO integration finalization
- Recovery WAL (SSD publishes + invalidates; payload-durable-first; opt-in same-workload replay; truncated-tail tolerance); recompute write-batching per (owner-session, layer) + shared-block ownership fix; duplicate-request coalescing (DISABLED by default: concurrency defect found by stress test); avg-read-io metrics; warnings-as-errors clean; 111 tests (1 known-flaky CRITICAL documented — D-P4-2). Verdict: FAIL(conditional) on the concurrency-integrity criterion; full report docs/P4_REPORT.md.

## 2026-11-13 — P3: medium-aware scheduling (FC-MAS extension)
- Opt-in migration layer: promotion (zero-extra-SSD-read, publish-swap protocol) + demotion (recency/frequency-aware, budget-capped, SSD-pressure-guarded), deferred frees (race-free), cooldown anti-thrash; default remains paper-faithful write-once.
- 10 deterministic migration tests (adversarial set complete); 101/101 total.
- docs/P3_REPORT.md; benchmark deferred until the M9 sweep releases the disk.

## 2026-11-12 — Phase 3: benchmarks, extension, freeze
- M9 benchmark sweep (8 cold-start runs): FlatCake 100% delivered hits vs baseline 86–99.6%; **109× fewer physical read ops** at qps4; write-amp 0.995× vs 3.269×; integrity 0 everywhere (docs/RESULTS.md, results/).
- First sweep's degenerate ratios (hbm == working set) caught by run inspection, corrected, rerun (ledger).
- I/O calibration probe: 19× coalescing ceiling; 52× page-cache artifact proof (results/io_probe.json).
- M11 adaptive-placement extension: 4 policies × 6 eval traces; honest negative result — parity, not improvement; mechanism explained (docs/ADAPTIVE_EXTENSION.md).
- plot_results.py hardened (skips non-run JSONs).

## 2026-11-11 — Phase 2: implementation
- P2-M0..M8 complete: state recovery, agent activation under measured platform limits (concurrency=2, later quota exhaustion — absorbed), A2 contract verification (17 engine errors found & fixed), FC-MAS landed (15/15 after defect report), engine integrated with A1 fidelity fixes (publish-after-spill, multi-scope reader pins, metric parity, load-factor guard), vertical slice + demo (82.5% vs 35.3%), 90/90 tests, PHASE_2_* reports, README freeze.

## 2026-11-11 — Phase 1: research → architecture
- M0: full paper coverage (14/14 pages; GVA layout + Eq. 1 verified verbatim); 4 research docs; 21 open questions.
- M1: environment audit; Rust 1.99.1 GNU toolchain installed & verified; stack decision D10.
- M2: architecture rev 2 after EGM design + readiness reviews (3 critical findings fixed pre-code: reclamation protocol, NO_BUFFERING, open-loop replay); frozen contracts (types/config/util).
- M3: workload generator (agentic/oneshot/mixed), tiered baseline, open-loop replay driver; integrity verification caught 3 real bugs (seed derivation, ABBA deadlock, worker wakeup).
- M4/M6: FC-GVA (33 tests) and FC-CIO (17 tests) by specialist agents with accepted deviations D21–D24.
