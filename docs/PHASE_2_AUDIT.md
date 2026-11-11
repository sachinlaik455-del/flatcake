# PHASE_2_AUDIT.md — performance & correctness audit (P2-M7)

Originally assigned to the performance agent; absorbed by the orchestrator after sub-agent quota exhaustion (ledger incident). Static audit + measured I/O calibration; no optimizations applied (measure-first policy, directive §14).

## Measured I/O calibration (scripts/audit_io_probe.py, 256 MiB file, this KIOXIA NVMe, 2026-11-11)

| Pattern | Throughput | p50 | p99 |
|---|---|---|---|
| buffered 4 KiB random (page cache) | ~2,900 MB/s | ~20 µs | ~200 µs |
| **unbuffered** 4 KiB random | **56 MB/s** | 67 µs | 158 µs |
| **unbuffered** 4 KiB sequential (stride) | **111 MB/s** | 28 µs | 103 µs |
| **unbuffered** 1 MiB sequential | **1,075 MB/s** | 960 µs | 1,162 µs |
| unbuffered 1 MiB random | 1,073 MB/s | 962 µs | 1,195 µs |

Raw: `results/io_probe.json`. Three audit conclusions:

1. **NO_BUFFERING (D12) is load-bearing for honesty**: buffered 4 KiB random reads appear 52× faster than reality (page cache serving a 256 MiB file from 16 GB RAM). Without D12, every `measured:` SSD number in this project would have been a cache artifact. Both systems use unbuffered handles (src/cio/layout.rs `FILE_FLAG_NO_BUFFERING`; src/baseline.rs same flag).
2. **The coalescing payoff on THIS disk is ~19×**: 1 MiB streaming (1,075 MB/s) vs 4 KiB random (56 MB/s). This calibrates the ceiling for FlatCake's coalesced restores vs the baseline's per-block reads — the paper's core I/O claim, now with a local number attached (Level-B observation, not a paper-scale claim).
3. **NVMe queue depth caveat**: single-threaded sync I/O (as in our per-worker Reader) reaches ~1.1 GB/s at 1 MiB ops — adequate; the paper's 5–6 GB/s/worker GDS numbers reflect hardware+parallelism we do not have and do not claim.

## Static audit findings (ranked; all file:line as of this commit)

| # | Severity | Finding | Evidence | Status |
|---|---|---|---|---|
| U1 | MEDIUM | Per-request copy chain: disk → Reader window → `slot.out` Vec → caller `out[idx]` merge → replay verify buffer (4 copies/block). GPU scatter has 2. Cost is µs-scale vs ms-scale I/O; measured plateau (matrix_engine_memory_plateau) shows boundedness. | src/cio/stream.rs sink; src/engine.rs get_request merge; src/replay.rs verify | Accepted for Level B; M10 re-profile only if benchmarks show CPU-bound stalls |
| U2 | MEDIUM | `dram_worker`/hybrid path copies payload under DRAM-arena Mutex (global serialization of DRAM hits). At demo load, p50 impact invisible; at high qps it serializes ~µs memcpys. | src/gva/arena.rs write/read under Mutex | M10 candidate (shard the arena or hand out refs) |
| U3 | LOW | Slot `out` sized `n_blocks × block_bytes` per in-flight request — bounded by queue_cap × max request; documented in ARCHITECTURE §9. | src/engine.rs Slot | OK (bounded) |
| U4 | LOW | `replay.rs` keeps `lat_samples`, `session_seeds`, unique-set per run: O(records) memory, MBs at our scales. | src/replay.rs | OK |
| U5 | INFO | Lock-order: documented order `registry → {index, arena, layout}` honored everywhere (A2 verified no cycles); engine adds only leaf locks (metrics/hbm/slots). | gva/mod.rs; A2 report | OK |
| U6 | MEDIUM | Baseline staging `reserve(100 ms × 2 retries)` can block a get ~300 ms under contention before abort — this is the paper's Obs.-2 mechanism reproduced, but it inflates baseline p99 under load. | src/baseline.rs StagingPool::reserve | Honest mechanism; caveat documented in methodology; NOT tuned to weaken baseline |
| U7 | INFO | Metric parity verified post-A1-fix: read_amp denominator (`logical_read_bytes`) written once (engine), cio merges only physical counters; write_amp FlatCake = physical/logical writes (write-exactly-once ⇒ ≈1.0 modulo reclaim re-persist). | src/engine.rs ssd_worker merge; results.rs | Verified by sweep numbers |
| U8 | LOW | `fckv demo` wipes `data/demo` only; bench sweep wipes `data/fckv`+`data/baseline`; nothing outside project dirs is touched (resource-safety directive). | src/main.rs demo; benchmarks/run_sweep.sh | OK |

## Benchmark-methodology verification

- Metric definitions in `results.rs` emit exactly the pinned schema (A2 cross-checked; `python/plot_results.py` consumes it).
- Cold start enforced per run; no warm-up (paper §5.1); multiple seeds; raw JSON per run — protocol rules 1–7 of BENCHMARK_METHODOLOGY.md are implemented in `benchmarks/run_sweep.sh`.
- Degenerate-ratio incident (2026-11-11, ledger): first sweep used hbm 64 MiB ≈ working set ⇒ all-hbm hits, zero SSD ops; caught by inspecting run outputs, ratios corrected (hbm 16 MiB, working set 96 MiB > DRAM 64 MiB), sweep relaunched. Evidence that per-run inspection works.

## Explicit non-claims

No GPU, no GDS, no claim of reproducing the paper's absolute numbers (D2). The `modeled_recompute_us` counter is the only modeled value in results and is symmetric across systems; latency comparisons carry the methodology caveat.
