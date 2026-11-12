# PAPER_TO_IMPLEMENTATION.md — Requirements → modules → tests → validation

## 1. Fidelity level declaration (per directive Part 2)

| Level | Definition | Status |
|---|---|---|
| **A — functional simulator** | In-memory algorithm validation, no physical I/O | Allowed for early algorithm tests only; **never** reported as I/O measurements |
| **B — CPU prototype with real local SSD I/O** | Real Windows filesystem I/O, real NVMe SSD, bounded RAM | **Immediate target. All benchmarks are Level B.** |
| **C — full hardware reproduction** | Linux + NVIDIA CUDA + GPUDirect Storage (cuFile) + SGLang + B200/A100 + PM9A3 RAID0 + DeepSeek-V4-Flash + claude-code-weka-0621 trace | **Future objective.** Design only (FUTURE_GDS_INTEGRATION.md). Never claimed as done. |

**The paper's reported numbers (94% hit rate, 1.6× TTFT, etc.) are the authors' measurements on different hardware with a production trace. They are never presented as ours. Our results are compared qualitatively (mechanism behavior: hit-rate stability, amplification, coalescing) — not numerically.**

## 2. Environment deltas (paper → prototype)

| Paper | Prototype (Windows 11 laptop, 16 GB RAM) |
|---|---|
| GPU HBM + GDS + CUDA streams | CPU "consumer" buffer; GDS boundary documented, not faked |
| Pinned DRAM pool (1 TiB KV budget) | Bounded DRAM pool (default 512 MiB, configurable) |
| 4 TiB NVMe RAID0, XFS, O_DIRECT, cuFile | Local SSD (NTFS), sequential file I/O; no O_DIRECT (unavailable via standard Win32 without unbuffered handles — documented) |
| SGLang engine, radix-tree prefix matching | Synthetic request stream with explicit prefix/session model (no LLM) |
| DeepSeek-V4-Flash / Llama-3.1-70B KV tensors | Deterministic byte-pattern payloads sized like KV blocks |
| Production Claude Code trace | Seeded synthetic workloads reproducing the *paper's stated patterns* (bimodal reuse) |
| Multi-node coordinator, RDMA, NVMe-oF | Single node, NodeID=0 |

## 3. Module map

| Module | Paper source | Owns |
|---|---|---|
| `workload` | §2.1, §2.2, Table 1, §5.1 | Deterministic trace generation (sessions, turns, reuse patterns), trace export |
| `baseline` | §2.1, §5.1 baselines | Tiered DRAM+SSD LRU cache w/ staging; Mooncake-like bucket reclaim |
| `gva` | §3.2, Table 2 | 64-bit GVA encode/decode, Level-1 index, placement policy, session reclamation |
| `mas` | §3.3, Eq. 1, Fig. 4 | Dual queues, HRRN priority, arrival_ts re-admission, backpressure |
| `cio` | §3.4, Fig. 5 | GPU-File layout (layer-major), sort-merge coalescing (Δgap=64 KiB), bounded reader windows |
| `engine` | §3.1, Fig. 3, §4 | Integration: exists/get/put API, lifecycle, instrumentation |
| `bench` | §5.1–5.4 (methodology) | Harness, metric definitions, CSV/JSON output, plots |
| `adaptive` (M11) | Directive Part 13 extension | Placement-policy comparison (NOT from the paper) |

## 4. Traceability matrix

Status: ✅ done · ✅ planned · ❌ out of scope (documented). Sections reference the paper.

| # | Paper requirement | Source | Interpretation | Module | Test / validation | Status |
|---|---|---|---|---|---|---|
| T1 | GVA = MediaType[63:60] \| NodeID[59:48] \| Offset[47:0]; DRAM GVA<2^60, SSD ≥2^60 | §3.2 | Exact bit layout, encode/decode via bit masks | `gva::addr` | round-trip tests, boundary offsets (0, 2^48−1), invalid media (≥2), max NodeID, DRAM/SSD classification | ✅ |
| T2 | Block key (session ID, layer, token range) → GVA, O(1) | §3.2 | Hash map keyed by (session, layer, token_start) | `gva::index` | lookup/collision/duplicate-key tests, concurrent read/write | ✅ |
| T3 | Publish key→GVA only after write completion | §3.2 | Entry inserted on write-completion callback, not at allocation | `gva::index` + `engine` | read-after-write hazard test: lookup during in-flight write misses; after completion hits | ✅ |
| T4 | Write-time placement per Table 2 (refcount, turn, reply len, context, DRAM-full) | §3.2, Table 2 | Decision function with τ/L_agent config (OQ-1/2); one-shot/turn=1 → SSD; DRAM-full → SSD spill | `gva::placement` | per-signal unit tests; each Table-2 row mapped to a test case | ✅ |
| T5 | Same-session appends bind to session's medium (no migration) | §3.2 | Session-scope sticky placement; write-once invariant | `gva::placement` | append-after-placement test; immutability-after-publish test | ✅ |
| T6 | Session-granularity reclamation: refcount, high watermark, oldest-unreferenced extents, O(1) free | §3.2 | Extent free-list per GPU File; atomic reader refcount; watermark trigger | `gva::reclaim` | reclaim never frees referenced scope; oldest-first order; watermark trigger; O(1) = metadata-only assertion | ✅ |
| T7 | Dual queues: max_j GVA_j < 2^60 → DRAM-Ready (immediate); ≥2^60 → SSD-Preparing | §3.3 | Exact classification incl. hybrid requests | `mas::scheduler` | mixed-request classification tests (Fig. 4 scenario: R1/R3/R5 SSD, R2/R4/R6 DRAM) | ✅ |
| T8 | HRRN Priority_i = 1 + wait_time_i / KV_size_i | §3.3 Eq.1 | Lazy recompute at dispatch decisions (OQ-4); units per OQ-15; KV_size>0 guard | `mas::ssd_queue` | formula unit tests; small-before-large; aging guarantees eventual dispatch of giant restore | ✅ |
| T9 | Re-admission ordered by original arrival_ts (not tail) | §3.3 | Binary-insert into Ready Queue by arrival_ts | `mas::readmit` | promoted-request ordering vs newly arrived DRAM hits | ✅ |
| T10 | No DRAM pre-staging on SSD hit path | §3.3 | SSD reads go directly to consumer buffer; DRAM holds only placed-at-creation data | `engine` | staging-memory-doesn't-scale-with-context test (contrast w/ baseline) | ✅ |
| T11 | Write-time layer-major scope layout in contiguous GPU Files; appends segment-wise | §3.4 | Scope → file mapping, layer-major byte layout, append pointer | `cio::layout` | layout round-trip: write N turns × L layers, verify contiguous per-layer extents | ✅ |
| T12 | Read-time sort-merge by (file, offset); bridge gaps < Δgap=64 KiB | §3.4 | Sort+linear merge; gap bridging configurable | `cio::coalesce` | adjacent/overlapping/scattered/gap-bridge/across-file/shuffled/empty/duplicate descriptor tests; **reconstructed bytes exactly equal requested bytes**; unrelated-block data never included | ✅ |
| T13 | O(layers) physical reads for full-prefix restore | §3.4 | Metric: physical reads per logical restore | `cio` + `bench` | 100-block × 8-layer restore ⇒ ≤ 8 (+bridged) physical reads; recorded in metrics | ✅ |
| T14 | Bounded streaming window (O(1) per consumer), double-buffered, no revoke-on-saturation (wait) | §3.4, §4 | Fixed-size ring of layer buffers; backpressure by waiting | `cio::stream` | memory-bound test under concurrency; saturation ⇒ wait not abort | ✅ |
| T15 | Engine API exists/get/put; immutable published payloads | §4 | Same interface shape at Level B | `engine` | lifecycle tests; double-put rejected; concurrent readers consistent | ✅ |
| T16 | Zero writeback; write amplification 1.000× for SSD-admitted blocks | §3.2, Fig. 9 | Bytes-written = bytes-placed (no rewrite/migration) | `bench` | write-amp metric == 1.0 on stable workload | ✅ |
| T17 | Baseline: tiered HBM→DRAM→SSD LRU + host staging + FIFO-bucket reclaim | §2.1, §5.1 | Fair baseline, same budgets/workload; documented deviations | `baseline` | integrity vs FlatCake identical logical contents; LRU order tests | ✅ |
| T18 | Delivered cache hit rate, read/write amplification, physical-vs-logical reads, latencies | §5.1 metrics | Definitions fixed in BENCHMARK_METHODOLOGY.md (read amp = physical bytes / logical bytes) | `bench` | metric self-consistency tests on synthetic runs | ✅ |
| T19 | Bimodal workload regimes (one-shot heavy; agentic reuse) | §2.2, Table 1 | Synthetic generators w/ seeds; patterns documented in WORKLOAD_MODEL.md | `workload` | statistical shape tests (once-access %, reuse-distance distribution, regime contrast) in tests/workload_shape_tests.rs | ✅ |
| T20 | 4 KiB alignment discipline (libcufile DMA) | §4 | Alignment kept for offsets/reads (future-GDS compatibility) | `cio::layout` | alignment invariant test | ✅ |
| T21 | Lock-free Level-1 index reads | §4 | Concurrent map w/ wait-free reads at Level B (Rust lock-free or RwLock documented as deviation) | `gva::index` | concurrency stress test | ✅ |
| T22 | GDS/cuFile direct-to-GPU streaming, CUDA scatter, pinned hugepages | §4 | ❌ Level C boundary → FUTURE_GDS_INTEGRATION.md | — | — | ❌ |
| T23 | Multi-node coordinator, RDMA, NVMe-oF transports | §3.2 | ❌ single node | — | — | ❌ |
| T24 | SGLang integration, real model KV, production trace replay | §5 | ❌ no LLM on target machine → synthetic payload equivalence documented | — | — | ❌ |

## 5. Metric definitions (normative for all benchmarks)

- **Delivered hit rate** = blocks delivered to consumer before prefill-equivalent compute ÷ blocks requested (OQ-17).
- **Read amplification** = physical bytes read from SSD ÷ logical bytes requested.
- **Write amplification** = physical bytes written ÷ logical bytes placed.
- **Physical read ops** = OS-level read calls on storage files; **logical reads** = block-level get() calls.
- All latency percentiles computed over completed requests in the measurement window only (warm-up excluded, protocol in BENCHMARK_METHODOLOGY.md).
- Simulated/analytic values are labeled `simulated:`; OS-level timings labeled `measured:`. Never mixed.
