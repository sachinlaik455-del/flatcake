# final-security-audit.md — FlatCake KV-cache system (final independent security assessment)

Lead auditor: principal security engineer (this session). Date: 2026-11-13. Target: `E:\project ml\KVCache` @ `1d768314bc55d6651168c4448898816d6f537e18` (clean tree).

## 1. Executive verdict

**PASS WITH FINDINGS.** One High-severity vulnerability (SEC-1, index poisoning via batched recompute — a genuine cross-block data-exposure defect introduced by the P4 write-coalescing feature) was **confirmed with a deterministic repro, fixed with a minimal surgical change, and regression-pinned** during this audit. Two Medium findings at the same site (extent-ownership leak, missing writer pin) were fixed in the same patch. One Medium finding (SEC-2, dedup wrong-bytes) was **mitigated by disabling the feature by default** with its defect documented. Four disproved candidates are recorded as rejected with evidence. The final suite is **111 tests, 0 failures, warnings-as-errors clean**, and the previously-flaky concurrent stress test now passes deterministically (its root cause was a test-harness key bug, fixed and proven by forensic probe).

## 2. Skill provenance and procedures applied

- Skill: Cloudflare `security-audit-skill`, cloned from `https://github.com/cloudflare/security-audit-skill.git` (not installed locally — verified by directory search of `.agents/skills` and `.zcode/skills`), HEAD `c1c8a8c1471069fb0e188eeaff69b8e8db6564a8` (2026-09-14). License file present; contents inspected before use; no skill scripts were executed (the two `.cjs` validators require POSIX no-follow open protection unavailable on Windows — documented limitation; equivalent structural checks were applied via Python).
- **Mode: full audit** (explicit user request), profile `standard`, budget 8 agent invocations, run id `flatcake-kvcache-run-1`, output directory `~/security-audit-skill/flatcake-kvcache/run-1/` (outside the target, per skill rules).
- **Procedures executed** (SKILL.md six-phase workflow):
  1. Phase 1 Reconnaissance: `run-metadata.json`, `architecture.md` (architecture + data-flow + trust boundaries), `coverage-ledger.json` (12 units seeded).
  2. Phase 2 Coverage-led hunting: 2 delegated hunters (memory-safety units u02–u04/u06; concurrency/DoS units u07–u09/u12) + parent-led hunts for u01/u05/u10/u11 (documented deviation: subagent concurrency quota = 2; third hunter quota-blocked).
  3. Phase 3 Candidate validation: every candidate independently re-derived or repro-tested (hunter repro executed by the lead; SEC-1 repro run 10/10 pre-fix and post-fix).
  4. Phase 4 Structured output: `findings.json` (4 confirmed / 2 needs_validation / 4 rejected) written and structurally validated against `report-schema.json` requirements in Python (official `validate-findings.cjs` unavailable on Windows — recorded).
  5. Phase 5 Independent record verification: lead reproduced SEC-1 (pre-fix failure observed), verified the fix (post-fix pass), and re-verified D-P4-1's prior resolution (10/10 post-test-fix runs).
  6. Phase 6 Reporting: skill-standard `REPORT.md`, `FINDINGS-DETAIL.md`, `NEEDS-VALIDATION.md` written; this file is the integrated release-readiness deliverable.
- **Deviations (honestly recorded)**: (a) Node validators could not execute on Windows (no-follow unavailable) — equivalent structural validation applied; (b) the third hunter and one verifier slot were absorbed by the lead after a subagent concurrency-limit failure (the second documented quota event this session); (c) ASan/Loom/miri/fuzzing unavailable in the GNU Windows toolchain — compensated by deterministic adversarial suites (documented in the prior audit and unchanged).

## 3. Repository inventory and architecture/data-flow map

Complete map in the run's `architecture.md` (skill Phase 1 artifact, mirrored in `docs/ARCHITECTURE.md` rev 2). Summary: std-only Rust lib+bin (`fckv`), zero external crates; KV blocks enter via JSONL trace → open-loop replay driver → `KvEngine` (put/get) → FC-GVA (placement, 64-bit GVA `MediaType[63:60]|NodeID[59:48]|Offset[47:0]`, Level-1 lock-free index, DRAM arena) → FC-CIO (layer-major scope files, NO_BUFFERING 4 KiB-aligned I/O, sort-merge coalesced reads, bounded windows) → FC-MAS (dual queues, HRRN, arrival-ts re-admission) → P3 migration (promote/demote, deferred frees) → P4 (recovery WAL, recompute batching, dedup). Concurrency: 2+2 workers + replay clients; lock order `registry → {index, arena, layout}`; every trust boundary (config file, trace file, recovery log) is a local hostile-input surface.

## 4. Threat model

Attacker capabilities: hostile local `fckv.conf` (extreme allocation requests), hostile/corrupted trace JSONL (malformed records, unbounded size), crash-corrupted recovery log (poisoned metadata), concurrent hostile callers (races, duplicates, saturation), interrupted operations at any point. Not in scope: remote/network attackers (the system has no network surface — verified), multi-tenant isolation (single-process prototype). Assets: KV block bytes, index mappings, allocation metadata, storage capacity, process availability.

## 5. Confirmed findings (all remediated + regression-pinned)

| ID | Severity | Finding | Location | Trigger | Impact | Remediation | Regression test |
|---|---|---|---|---|---|---|---|
| SEC-1 | **High** | Index poisoning: batched recompute persisted phantom-token bytes under real keys | `src/engine.rs` `flush_recompute_chunk` (P4 addition) | Miss group with a token gap (normal re-get case) | Later readers of affected keys receive another block's bytes (cross-block data exposure) | Provider uses each entry's real `token_start` | `p4_engine_recompute_nonconsecutive_group_poisons_index` (failed 10/10 pre-fix, passes post-fix) |
| SEC-1a | Medium | Batched-recompute SSD extents owned by no scope (reclamation leak) | same site | Any batched recompute | SSD watermark leak; recycle pressure | Per-block `add_ssd_extent` registration | covered by SEC-1 test + reclaim tests |
| SEC-1b | Medium | Batched flush lacked writer-pin discipline (stale-mapping chain) | same site | Concurrent reclaim vs flush | Stale mappings under concurrency | `pin_writer_retry` around the batched flush | covered by engine tests |
| SEC-2 | Medium | Dedup coalescing delivered wrong bytes under mixed-medium interleaving | `src/engine.rs` dedup block | Multi-round workload; divergent caller prefixes | Follower receives another block's bytes | **Disabled by default** (`dedup_enabled = 0`); pure-SSD gate + safety argument documented for re-enable | `p4_duplicate_requests_integrity_and_dedup` (opt-in) |

Root-cause note: SEC-1 was introduced by the P4 write-coalescing feature itself; the concurrent stress suite added in P4 (and the audit's forensic probe) is what exposed it — the test process worked as designed.

## 6. Disproved candidates (rejected, with evidence)

- **rej-1** coalesce index-scatter mislabeling (hunter's prime suspect): `enumerate()` runs before the sort; parts reference caller positions; deterministic shuffled test passes 10/10 (`p4_cio_scatter_shuffled_multiseq_byte_exact`, added by Track C).
- **rej-2** D-P4-1 as an engine defect: forensic probe + fixed test → 10/10; the flake was phantom test keys (see P4_REPORT).
- **rej-3** GVA offset-math overflow: `Gva::encode` returns `None` on overflow → loud per-append error; file_seq is u32 with encode-time validation.
- **rej-4** index probe-chain break under concurrency: 33-test gva suite + pin protocol; concurrent stress passes.

## 7. Needs-validation records (no severity, per skill discipline)

- NV-1: recovery-log rotation horizon (long-uptime disk growth unmeasured).
- NV-2: allocation-failure behavior at max config bounds under committed-memory pressure (probe succeeded on the 16 GB host).

## 8. Supply-chain assessment

Zero external crates (no transitive risk); no CI workflows (no trigger/pinning/permission surface); no network operations in build/test (verified by grep); git history scanned for secrets — 2 grep hits were audit prose containing the word "secrets", no credentials; generated artifacts gitignored. Build reproducibility: clean `cargo build --release` + README commands; no lockfile needed (no deps).

## 9. Platform limitations

Windows 11 + GNU toolchain: NO_BUFFERING via `FILE_FLAG_NO_BUFFERING` (honest O_DIRECT analog; GDS is Linux-only and unclaimed); official Cloudflare Node validators and sanitizers/Loom/miri unavailable — compensated by structural schema validation and deterministic stress suites (recorded honestly per skill rules).

## 10. Final build/test results and regression check

- Clean `RUSTFLAGS="-D warnings"` build: 0 errors, 0 warnings.
- Full suite after all security fixes: **111 tests, 0 failures across 13 targets** (includes the new SEC-1 regression pin and Track C's scatter/churn regression tests).
- Benchmarks: recovery-log overhead ~0% (measured, results/p4/); M9 sweep numbers unchanged (docs/RESULTS.md) — security fixes introduced no measurable performance regression at these scales (the fixed paths execute only on recompute/miss, which the sweep's healthy workloads do not hit; the regression tests cover correctness).
- Working tree: clean at audit close; run artifacts live outside the target (`~/security-audit-skill/flatcake-kvcache/run-1/`).

## 11. Residual risks

1. SEC-2 mitigation is disabling, not fixing — re-enabling dedup requires the documented interleaving fix.
2. NV-1/NV-2 items above.
3. Recovery replay is opt-in and assumes same-workload restart (cross-trace session reuse protected by default).
4. Level-C hardware (GPU/GDS) remains unimplemented by design — boundary documented, not faked.

## 12. Release-readiness

**PASS WITH FINDINGS.** The complete P1–P4 system is memory-safe (std-only, runtime bounds enforcement), concurrency-safe to the tested interleavings (one real race found and disabled-by-default; one real poisoning bug fixed and pinned), data-integrity-safe (byte-exact verification across all adversarial suites; recovery proven), and operationally bounded (every queue, map, and buffer has an enforced cap). The repository is ready for handoff; per the strict boundary, development stops here and no release actions were taken.
