# final-security-audit.md — FlatCake Level-B prototype (final independent security audit)

Auditor: lead security engineer (this session), with Track A (integration audit) and Track C (concurrency hunt) subagent contributions. Date: 2026-11-13. Repository: 31 commits, clean tree at audit close.

## Executive summary

FlatCake (Level-B CPU prototype, std-only Rust, zero dependencies) was audited across P1–P4: attack surface, memory safety, concurrency, storage integrity, scheduler/queue robustness, supply chain, and adversarial testing. **108 tests pass (0 failures), warnings-as-errors clean, selftest PASS.**

The audit's most significant activity was the investigation of **D-P4-1** — an intermittent (~50–70%) concurrent byte-integrity failure flagged by the P4 stress suite and carried into this audit as the headline defect. **Root-caused during this audit: a TEST-HARNESS bug, not an engine defect.** The test constructed block keys with a bytes-vs-tokens multiplier, creating phantom keys that collided with real published keys under race (a reader expecting seed-2002 bytes at a token the writer had filled with seed-2001 bytes). The forensic probe (delivered-bytes identification across all candidate seeds/sessions/tokens) proved the engine delivered exactly what the index mapped; after the test fix, **10/10 consecutive runs pass**. No engine code change was required.

**Verdict: PASS WITH FINDINGS** (two Low/Informational findings + documented hardening items; no Critical/High findings remain open).

## Threat model

Attacker/upstream-fault capabilities considered: malformed or hostile KV-cache requests (invalid media, oversized offsets/lengths, zero-length blocks), corrupted metadata or recovery log, extreme block sizes, storage failures and partial writes, concurrent access races, resource exhaustion (RAM/SSD/queues), interrupted operations. Trust boundary: everything entering the engine via `KvEngine` (put_batch/get_request), the config file, and trace files is potentially hostile.

## Architecture and attack-surface map

| Boundary | Defense | Evidence |
|---|---|---|
| GVA encode | `Gva::encode` rejects media ≥ 2, node > 12 bits, offset > 48 bits (no silent truncation) | types.rs:23-31; tests t1_invalid_values_rejected, selftest |
| Index publish | PENDING invisible until publish-after-write; CAS state machine; TOMBSTONE invalidation before extent free | gva/index.rs; t2/t3/t6 tests |
| Placement | Pure function of (signals, sticky, cfg) — deterministic | gva/placement.rs; t4_* + p3 determinism test |
| Reclamation | Refcount-gated (readers+writers), tombstone-before-free, oldest-first, O(1) metadata-only (payload-ops asserted 0) | gva/reclaim.rs; t6_* incl. spy assertions |
| Session scope | Size guard (batch ≤ gpu_file_bytes), one-file-per-scope, recycling gated on freed==appended with buffer-state reset | cio/layout.rs:291-309, 184-195 |
| NO_BUFFERING I/O | 4 KiB-aligned offsets/lengths/AlignedBuf (zero-filled); padded tails discarded; short reads rejected | cio/layout.rs, stream.rs; t11/t14 tests |
| Recovery log | Payload-durable-before-log ordering; SSD-only records (DRAM never logged — audit item 2); last-wins replay; corrupt-tail dropped | engine.rs log_publish/parse_log_line; p4 recovery tests |
| Config | Bounds-checked every key; unknown keys rejected; power-of-two file size enforced | config.rs; selftest |

## Confirmed findings (with remediation state)

| ID | Severity | Component | Finding | Status |
|---|---|---|---|---|
| F-1 | **Critical → RESOLVED (test bug)** | tests/engine_tests.rs | D-P4-1: intermittent concurrent byte-integrity failure. Root cause: test's `keys_of(..., bb)` used a bytes multiplier creating phantom keys (tokens 8192/16384/24576) that collided with real published keys (token 0) under the writer/reader/recompute race. Forensic probe identified delivered bytes as recompute-of-phantom-key content; engine index mapping verified correct throughout. Fix: test key construction corrected to block_tokens; **10/10 consecutive passes after fix**. | ✅ RESOLVED + regression-pinned |
| F-2 | Low | src/engine.rs (dedup) | Duplicate-request coalescing delivered wrong bytes under multi-round mixed-medium interleaving. **Remediated: disabled by default** (`dedup_enabled = 0`), implementation retained behind the flag with the pure-SSD gate and positional-merge safety argument; re-enable requires fixing the interleaving (documented in P4_REPORT). | ✅ Mitigated (off by default) |
| F-3 | Low | src/gva/reclaim.rs | Recovery-log/registry additive APIs (active_readers/writers, remove_*_extent) are new public surface without dedicated adversarial tests beyond the P3/P4 suites' indirect coverage. | Informational (covered indirectly by 33 gva tests + engine suites) |
| F-4 | Informational | src/replay.rs | `panic!` on traces exceeding 0.7 × index_capacity — loud failure instead of graceful error return. Acceptable for a prototype; documented (OQ-16). | Open (hardening) |
| F-5 | Informational | cio/layout.rs | Multi-file chaining deferred (OQ-11): sessions > gpu_file_bytes fail loudly with a clear error rather than silently corrupting. | Open (by design, asserted) |

No out-of-bounds access, use-after-free, double-free, stale-mapping, partial-block exposure, deadlock, or unbounded-allocation finding survived testing. The dedup self-deadlock and the parse off-by-one found during development were fixed before this audit's final runs (P4_REPORT defects 1–4).

## Hardening recommendations (non-blocking)

1. F-4: return `Result` instead of panicking on capacity overrun.
2. F-2: fix the dedup interleaving (suspect: follower attaching to a leader whose block set diverged after mixed hbm/DRAM prefixes — needs a formal interleaving proof or a leader-epoch check) before re-enabling.
3. Property-based fuzzing of `parse_log_line` and `Gva::encode` (both are pure functions with tight invariants — cheap fuzz targets).
4. Consider `FlushFileBuffers` checkpoints on the recovery log for power-loss durability (current bar: seek_write completion — process-crash safe, not power-loss safe; documented).

## Dependency and supply-chain assessment

- **Zero external crates** (Cargo.toml has no `[dependencies]`); std-only Rust 1.99.1 stable-gnu. No transitive supply chain.
- Python tooling (matplotlib) used for plotting only — not in the build/test path of the shipped library.
- No CI workflows exist in the repo (no workflow-permission or action-pinning surface). No network operations at build/test time (verified: no `reqwest`/`ureq`/fetch in code). No credentials or secrets in the tree (grep clean). Git history: 32 commits, no force-pushes, no resets; large generated artifacts (target dirs, data/, results/) gitignored.
- Reproducibility: `cargo build --release` from a clean clone + the README commands; toolchain pinned by instruction (README) rather than a lockfile (no deps to lock).

## Adversarial and stress test results (executed)

- Boundary/overflow: GVA encode edge matrix (media/node/offset boundaries, invalid values) — selftest + t1.
- Zero/oversized: provider-length mismatch rejected (layout.rs provider validation); session-size guard fires with explicit error (t11_session_size_guard).
- Corrupted metadata: truncated/partial recovery-log tail dropped, earlier records recover (p4_recovery_truncated_tail).
- RAM saturation: arena full → spill; headroom floor blocks promotion (p3_promotion_respects_headroom_floor).
- Concurrent: 8-thread mixed put/get/reclaim/recompute — integrity held after F-1 fix (10/10); matrix accounting invariant under stress; concurrent index stress at 0.68 load; concurrent migration integrity.
- Queue saturation/backpressure/starvation: mas_tests (15) — backpressure blocks bounded, aging prevents starvation, cancellation, duplicate dispatch.
- Recovery: crash-simulated drop without shutdown → same-workload replay restores all blocks byte-exact (p4_recovery_replay_round_trip).
- Tooling honesty: no sanitizers/ASan/Loom/miri runs — the GNU Windows toolchain + std-only code; race detection relied on deterministic stress suites (documented as a platform limitation).

## Stress-test and benchmark results

M9 sweep (8 cold-start runs, corrected ratios): FlatCake **100% delivered hit rate at every load** vs baseline 88.8–99.7%; **44–105× fewer physical read ops** (477 vs 20,958 @qps1; 352–366 vs 36,349–38,272 @qps4); write-amp **0.995×** vs 3.165–3.269×; read-amp 1.000× both; integrity 0 in all runs; recovery-log overhead ~0% (17.0 s identical across off/on/replay). Full data: docs/RESULTS.md, results/*.json, results/plots/.

## Platform limitations

Windows 11: NO_BUFFERING via `FILE_FLAG_NO_BUFFERING` (std OpenOptionsExt) — the honest analog of O_DIRECT; GPUDirect Storage/cuFile is Linux-only and explicitly not implemented or claimed (docs/FUTURE_GDS_INTEGRATION.md). WSL2 unavailable on this machine.

## Residual risks

1. F-2 (dedup off-default) — re-enablement requires the documented interleaving fix.
2. F-4/F-5 hardening items.
3. Benchmarks measure mechanism behavior on one laptop with synthetic traces — no paper-scale comparability (D2).
4. Single-node scope: coordinator/RDMA/NVMe-oF unimplemented (OQ-10).

## Final build and test results

- Clean build with `RUSTFLAGS="-D warnings"`: 0 errors, 0 warnings.
- Full suite: **108 tests, 0 failures** across 12 targets (10 lib + 4 baseline + 17 cio + 2 engine + 33 gva + 15 mas + 7 matrix + 10 p3 + 7 p4 + 2 replay + 1 shape).
- `fckv selftest`: PASS. `fckv demo`: PASS (byte-exact integrity, both systems).
- Working tree: clean at audit close.

## Release-readiness verdict

**PASS WITH FINDINGS.** The system is safe and correct for its declared Level-B scope: no memory-unsafety (std-only, bounds-checked), no stale-mapping or partial-write exposure found under adversarial and concurrent probing, recovery integrity proven, and the one critical-flagged issue root-caused to a test bug with the engine exonerated by forensic evidence. The findings above are hardening items, not release blockers for a research prototype. Handoff to any future hardware (Level-C) effort proceeds via docs/FUTURE_GDS_INTEGRATION.md.
