# BENCHMARK_METHODOLOGY.md — normative benchmark rules (M9)

## Metric definitions (identical to PAPER_TO_IMPLEMENTATION.md §5; single source of truth for code)

| Metric | Definition | Label |
|---|---|---|
| delivered_hit_rate | (hit_hbm + hit_dram + hit_ssd) / (hits + misses), per block | measured (counts) |
| read_amp | physical_read_bytes / logical_read_bytes | measured |
| write_amp | physical_write_bytes / logical_write_bytes | measured |
| physical_read_ops | OS-level positional reads on storage files | measured |
| logical_block_gets | block-level get() calls served | measured |
| latency_us | request issue (intended ts_us) → all blocks delivered; includes queue wait | measured |
| queue_wait_us | flatcake: time in MAS queues before dispatch; baseline: staging-pool wait | measured |
| prep_us | flatcake: SSD prefetch (coalesced read) time; baseline: per-block SSD read time | measured |
| modeled_recompute_us | recompute cost charged on misses (identical constant both systems) | **modeled** |
| mem_high_water_bytes | max engine-owned RAM over the run | measured |
| reclaim_events / reclaim_payload_ops | reclamation count / payload-byte ops during reclaim (must be 0) | measured |

`modeled_recompute_us` is the ONLY modeled number in results. It represents the GPU prefill compute the paper's systems avoid/spend; both systems use the same constant (`recompute_us_per_token`), so comparisons stay fair. No SSD latency is ever simulated — every SSD number is a real positional `seek_read/seek_write` with `FILE_FLAG_NO_BUFFERING` (D12).

## Parity table (EGM #8 — identical budgets unless the mechanism itself differs)

| Resource | FlatCake | Baseline |
|---|---|---|
| hbm window | 64 MiB LRU-by-delivery | 64 MiB LRU-by-delivery (identical code) |
| DRAM cache | 512 MiB write-once scope pool | 512 MiB LRU cache |
| SSD budget | 16 × 256 MiB scope files = 4 GiB | 128 × 32 MiB buckets = 4 GiB |
| Read path | coalesced ranges through per-worker windows | one physical read per block |
| Staging pool | **none** (no pre-staging — the design point) | 128 MiB (tiered mechanism) |
| Miss handling | recompute (modeled) + re-persist | identical |

Staging-pool bytes are counted in `mem_used` for both; FlatCake simply never allocates any. This asymmetry IS the paper's claim (removing staging removes the bottleneck), so it is part of the mechanism under test, not an unfairness.

## Protocol

1. **Cold start, no cache warm-up** (paper §5.1): fresh engine + empty `data/` dirs per run; measurement window = the whole run.
2. One system runs per process invocation; systems never share data dirs (`ssd_dir` vs `baseline_ssd_dir`).
3. **Seeds**: ≥3 deterministic seeds per configuration; mean ± spread reported.
4. **Load sweep**: qps values encoded in the trace (`gen-workload --qps`); traces are regenerated per qps with the same seed.
5. Every run writes `results/<system>-<regime>-qps<Q>-seed<S>.json` (schema ARCHITECTURE §11) + `<...>.csv` (per-window time series, `csv_window_us`).
6. Reproduce: exact commands recorded in `EXPERIMENT_LOG.md` per run. Raw JSON never edited by hand.
7. Disk space check before each run (needs ~2× trace SSD bytes free).
8. Stop conditions: run aborts on engine error; partial results are discarded, never reported.

## Modeled-recompute caveat for LATENCY comparisons (observed in the vertical-slice demo)

Baseline misses are handled as recompute with a **bookkeeping-only modeled cost** (`modeled_recompute_us` counter — no wall-clock sleep), while FlatCake SSD hits pay REAL disk I/O. Therefore p50/p99 latency comparisons are **not meaningful in FlatCake's favor or against it** when miss rates differ hugely: a baseline run with 39% misses shows artificially low latency because its recompute is instant bookkeeping, whereas FlatCake's real SSD hits take real milliseconds. The honest comparison axes at Level B are: **delivered hit rate, per-medium hit distribution, read/write amplification, physical-vs-logical op counts, and integrity** — exactly the mechanism behaviors the paper claims. Latency distributions are reported for completeness with this caveat attached (paper-fidelity note: the paper compares real GPU prefill on both sides; Level B has no GPU compute to model both sides equally).

## What we compare — and what we refuse to compare

- Compared: hit-rate stability under load; read/write amplification; physical-vs-logical read counts (coalescing effect); latency distribution shift with load; staging-abort behavior (baseline) vs bounded-window waiting (FlatCake).
- **Never** compared numerically to the paper's numbers (D2): different hardware, different trace, no GPU. Paper numbers appear only as literature context in RESULTS.md.
