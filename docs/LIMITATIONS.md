# LIMITATIONS.md — what this prototype is not, and known weaknesses

## Hard boundaries (by design)

1. **No GPU, no CUDA, no GPUDirect Storage.** The paper's data path (cuFile batch submits, registered HBM staging windows, CUDA scatter kernels, cudaMemcpyAsync) has no analog here beyond the bounded per-worker read windows. `FILE_FLAG_NO_BUFFERING` is the Windows analog of `O_DIRECT`, not of GDS. Migration path: docs/FUTURE_GDS_INTEGRATION.md.
2. **No LLM, no SGLang, no production trace.** Payloads are deterministic byte patterns sized like KV blocks; the trace is synthetic (shapes modeled on the paper's Table 1 statistics, not its data — `claude-code-weka-0621` was not downloaded: license/size unverified, OQ-18).
3. **Single node.** NodeID is implemented but always 0; no coordinator, RDMA, or NVMe-oF (OQ-10).
4. **Level-B fidelity claims only.** The paper's numbers are never presented as ours; our results are directional mechanism evidence on different hardware (D2).

## Known measurement limitations

- **Latency asymmetry**: baseline misses "recompute" as instant bookkeeping while FlatCake pays real unbuffered I/O — latency comparisons carry the standing caveat (BENCHMARK_METHODOLOGY.md). p50/p99 are reported for completeness, never as the headline.
- **Sweep scale**: 2 seeds × 2 load points (tightened from 3×3 after wall-clock realities of overload-drain runs; direction consistent across seeds).
- **qps=1 overload cells**: both systems saturate; runs dominated by backpressure drains. Reported with the caveat; the clean comparison cells are qps=4 + mechanism metrics.
- **DRAM share below the paper's band** (A1 R1): our agentic traces classify nearly all session history as SSD, so DRAM hit share ≈ 0.2–1.5% vs the paper's 8–11.7% — a workload-shape difference, not an engine defect.

## CRITICAL known defect (P4, unresolved)

- **Intermittent concurrent-integrity failure** in `engine_concurrent_mixed_sessions_no_loss_no_corruption` (~50% repro under 8-thread mixed put/get/reclaim/recompute load): a reader can receive another block's bytes. Isolation: P2-layer concurrency passes 10/10; migration OFF, recovery OFF, dedup OFF each still fail — engine-level, not a P4-addition regression; not root-caused. Dedup coalescing additionally disabled by default for its own concurrency bug found by the same test. Top handoff item for the independent audit.

## Known implementation limitations (documented, accepted)

- Arena payload copies hold the global arena Mutex (µs-scale; M10 candidate) — src/gva/arena.rs.
- Recovery/durability: crash = empty rebuild; no index log (OQ-9 cut at M2 review).
- Multi-file chaining deferred (OQ-11); workloads enforce session ≤ gpu_file_bytes.
- The index is fixed-capacity (startup guard panics loudly rather than resizing — OQ-16).
- Windows-only I/O layer (`OpenOptionsExt::custom_flags`); porting to Linux needs the flag swap + cuFile seam.
- M11 adaptive placement: honest negative result (parity, not improvement) with a reduced tuning grid — see ADAPTIVE_EXTENSION.md.
- Sub-agent quota exhaustion mid-project: specialist roles A6/A7/A8 were absorbed by the orchestrator (ledger incident log); their deliverables exist but with less independent-person separation than the directive ideally wanted — mitigated by A1/A2 independent reviews that DID come from separate agents, and the final EGM audit.
