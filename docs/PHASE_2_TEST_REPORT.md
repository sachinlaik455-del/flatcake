# PHASE_2_TEST_REPORT.md — verification matrix & results (P2-M6)

Originally assigned to the Testing agent; absorbed by the orchestrator after the account's sub-agent quota was exhausted (incident in EXECUTION_LEDGER.md). All commands run from the repo root on the target Windows 11 machine.

## Reproduction commands

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release
cargo test --release                       # all suites (when no bench is running)
cargo test --release --test p2_matrix_tests   # targeted (bench-safe)
./target/release/fckv.exe selftest
./target/release/fckv.exe demo
```

Note: while a benchmark sweep is running (holds `target/release/fckv.exe`), use per-suite `cargo test --release --test <name>` invocations; a full `cargo test` fails to link the bin target with "Permission denied" (Windows file lock) — that is a lock artifact, not a defect.

## Verification matrix (directive §13 items → evidence)

| Requirement | Test (suite) | Result |
|---|---|---|
| GVA encode/decode, field boundaries, invalid values | `t1_*` (gva_tests, 3) + `fckv selftest` | PASS |
| Key→address lookup, collisions, duplicates, concurrency | `t2_*` (gva_tests, 6) | PASS |
| Publish-after-write / racing gets / rollback | `t3_*` (gva_tests, 2) | PASS |
| Placement decisions (Table-2 rows + spill flip) | `t4_*` (gva_tests, 7) | PASS |
| Session ownership, stickiness | `t5_*` (gva_tests, 2) | PASS |
| Reference release & safe reclamation (incl. spy-asserted tombstone-before-free) | `t6_*` (gva_tests, 8) | PASS |
| DRAM dispatch / SSD preparation classification (Fig. 4) | `t7_classify_fig4_dual_queue_routing` (mas_tests) | PASS |
| HRRN priority, size-order, aging/starvation | `t8_*`, starvation tests (mas_tests) | PASS |
| Original-arrival-timestamp preservation (re-admission seniority) | `t9_readmit_wait_covers_prefetch_seniority` (mas_tests) | PASS |
| Request cancellation, duplicate-dispatch prevention | cancel/idempotent tests (mas_tests) | PASS |
| Queue backpressure (bounded, no deadlock) | `backpressure_enqueue_blocks_until_pops_free_capacity` (mas_tests — off-by-one found by orchestrator, root-caused & fixed by FC-MAS agent) | PASS |
| File layout, layer ordering, 4 KiB alignment | `t11_*` (cio_tests, 3) | PASS |
| Descriptor sorting, adjacent merge, gap bridging, no cross-file merge, duplicates, reconstruction | `t12_*` (cio_tests, 9) | PASS |
| Physical-op bounding (O(layers) reads), chunked windows, padded tails | `t13/t14_*` (cio_tests, 3) | PASS |
| Baseline correctness (round-trip, LRU spill, staging abort→recompute, FIFO reclaim) | baseline_tests (4) | PASS |
| End-to-end: DRAM+SSD+hybrid delivery, hbm reuse, safe reclaim, integrity | engine_tests `vertical_slice_end_to_end_dram_ssd_hybrid_and_reclaim` | PASS |
| Concurrent mixed sessions, no loss/corruption | engine_tests `engine_concurrent_mixed_sessions_no_loss_no_corruption` | PASS |
| Open-loop pacing honored | replay_baseline_tests `replay_pacing_is_open_loop` | PASS |
| Trace determinism (same seed ⇒ byte-identical) | p2_matrix `matrix_trace_determinism` | PASS |
| Block immutability after publish (duplicate put) | p2_matrix `matrix_published_blocks_immutable` | PASS |
| Concurrent retrieval + simultaneous reclamation ⇒ zero corruption | p2_matrix `matrix_concurrent_reads_with_reclaim_pressure` | PASS |
| Accounting invariant hits+misses==requested under stress | p2_matrix `matrix_accounting_invariant_under_stress` | PASS |
| queue_cap boundedness through replay | p2_matrix `matrix_replay_bounded_queue_cap` | PASS |
| Session-expiration semantics (hbm invalidated on reclaim; no stale bytes) | p2_matrix `matrix_baseline_reclaim_invalidates_hbm` | PASS |
| Bounded memory (plateau ≤ pools) | p2_matrix `matrix_engine_memory_plateau` | PASS |

## Totals

- **90 tests, 0 failures** (10 lib + 4 baseline + 17 cio + 2 engine + 33 gva + 15 mas + 7 matrix + 2 replay).
- Suites run 3× consecutively to screen for flakes: no flaky tests observed (the historical MAS backpressure failure and the replay deadlock were deterministic defects, both fixed at root cause; the fixes are in the ledger).

## Defects found during P2 (with resolution)

1. `mas_tests::backpressure_…` off-by-one (cap boundary) — reported to FC-MAS agent; root cause fixed in src/mas/scheduler.rs; test now asserts the documented boundary. RESOLVED.
2. Replay-driver worker wakeup (missing notify on depth decrement) — orchestrator fixed (notify_all after completion). RESOLVED.
3. Baseline ABBA lock ordering (metrics guard held across spill→ssd→hbm) — orchestrator fixed (spill owns its locks, one-at-a-time). RESOLVED.
4. Replay per-block seed derivation (turn-N blocks verified against turn-1 seed) — caught by integrity check; fixed with range-lookup. RESOLVED.
5. Engine accounting gaps (hybrid/hbm/miss paths not incrementing requested_blocks) — orchestrator fixed; invariant now asserted under stress. RESOLVED.
6. A1 fidelity findings (unpublished DRAM-spill blocks; single-scope reader pins; metric parity; missing load-factor guard) — all fixed; engine tests + matrix re-verified. RESOLVED.

No unresolved defects as of this report.
