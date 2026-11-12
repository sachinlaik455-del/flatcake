# P4_REPORT.md — FC-CIO integration phase (P4 engineering report + verdict)

## Implementation summary

P4 closed the audited integration gaps (docs/P4_INTEGRATION_AUDIT.md, Track A agent) with four additions, all additive to the frozen architecture:

1. **Recovery log** (WAL): every SSD publish logs a publish record; every migration swap logs an invalidate record (audit item 10). Payload-durable-before-log ordering enforced at call sites. Startup replay (opt-in `recovery_log = 2`, same-workload restart — audit item 5) rebuilds the Level-1 index last-wins-per-key, drops corrupt/partial tail lines (crash-mid-log tolerance), re-adopts session append cursors via `layout.adopt_session`, and skips non-SSD records (DRAM bytes are not crash-survivable). `recovery_log = 1` (default) writes the log without replaying (protects cross-trace session-id reuse).
2. **Recompute write coalescing**: missing blocks are batched per (owning session, layer) into single threshold-split SSD appends (`write_coalesce_bytes`, default 64 KiB) instead of one 8 KiB write per block. **Ownership fix**: recomputed blocks persist under the KEY's owning session (a recomputed shared-prompt block belongs to session 1, never to the requester).
3. **Duplicate-request coalescing** (dedup): identical in-flight SSD requests (sorted-GVA fingerprint, audit item 12) share one completion slot. **DISABLED BY DEFAULT** (`dedup_enabled = 0`): see defect D-P4-1 below.
4. **Metrics**: `dedup_requests`, `recovery_replayed`, `recovery_truncated`, `avg_read_io_bytes` (computed).

FC-CIO's read-side coalescing (P2) is unchanged and untouched (per audit item 15); P1 baseline untouched.

## Agent contributions

- **Track A agent** (integration audit): 15 findings with file:line evidence; drove the recovery-log design (SSD-only records, ordering, run-scoping), the dedup attach-point decision, and the metrics contract.
- **Tracks B/C (orchestrator)**: implementation + 7 deterministic tests.
- **Track D (orchestrator)**: benchmarks (below).
- **Track E (orchestrator)**: diff review — 0 debug markers, 2 test-file unused imports fixed, warnings-as-errors clean.

## Files changed (P4)

`src/engine.rs` (+~430), `src/cio/layout.rs` (+26 adopt_session), `src/config.rs` (+12), `src/types.rs` (+4), `src/results.rs` (+24), `tests/p4_cio_tests.rs` (+397). Test-file import cleanups in 2 files.

## Tests

**111 tests total: 110 pass, 1 known-flaky CRITICAL (D-P4-1, below).** P4 suite: 7/7 deterministic pass (batching, recovery round-trip, truncated-tail, dedup-disabled default, ownership, alignment, duplicates). Warnings-as-errors: clean. `fckv selftest`: PASS.

## Benchmark (measured; identical workloads)

- **Recovery-log overhead: ~0%** — recompute-heavy trace, `recovery_log` 0/1/2: elapsed 17.0 s all three runs; hit rate 100.00% all three; integrity 0 all three (results/p4/*.json).
- **Write batching**: mechanism verified by `p4_recompute_write_batching` (one batched append per layer-group); workload-level delta not measurable in the benchmark workloads because healthy runs produce zero recompute (100% hit) — stated as a limitation, not an improvement claim.
- **Dedup**: correctness proven at unit level; disabled by default (defect below); no benchmark claim.
- M9 sweep (fresh, corrected ratios): FlatCake 100% hit at every load vs baseline 88.8–99.7%; 44–105× fewer physical read ops; write-amp 0.995× vs 3.165–3.269×; integrity 0 in all 8 runs (docs/RESULTS.md).

## Defects found and fixed during P4

1. Dedup follower self-deadlock (re-locked the slot mutex while holding it) — fixed (drop before merge).
2. `parse_log_line` off-by-one (dropped the leading digit of every GVA — recovered entries pointed outside the arena) — caught by the recovery tests via arena-bounds panic; fixed.
3. Recomputed shared blocks persisted under the requester's scope (ownership violation) — fixed (owner-session batching).
4. Two stale-trace `eprintln!` blocks and 3 unused imports — removed.

## CRITICAL known defect (unresolved, honestly reported)

**D-P4-1 — intermittent concurrent-integrity failure in `engine_concurrent_mixed_sessions_no_loss_no_corruption` (~50% repro).** Under 8-thread mixed put/get/reclaim/recompute load, a reader can receive another block's bytes (byte-level assert failure). Isolation results: P2-layer concurrent test (gva/reclaim/arena/index) passes 10/10 — the defect is engine-level; migration OFF still fails (~37%) — not P3 promotion; recovery log OFF still fails (~50%) — not the log; dedup OFF still fails (~50%) — not dedup. Root cause not identified within the phase (suspect: hbm-window/arena-extent lifecycle interaction under reclaim pressure). **Impact: correctness under concurrent mixed load. Not a P4 regression — a latent defect quantified by the P4 stress suite.** The test remains enabled (the honest signal); the defect is the top handoff item for the security audit.

## Verdict

**P4: FAIL (conditional on one criterion)** — implementation complete, 110/111 tests green, benchmarks honest, but the criterion "no unresolved critical correctness defects" fails on D-P4-1. All other P4 criteria pass: batching/alignment/partial-batches deterministic; recovery round-trip + truncation tolerance proven; FC-GVA mappings and block ownership verified through migration and recovery; P1/P2 behavior intact (regression-guarded); P1–P3 baseline re-established and benchmarked; reproducible commands in README. The repository is prepared for the independent audit with D-P4-1 as the headline item.
