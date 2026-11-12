# RESULTS.md — Level-B benchmark results (M9)

All numbers below are **our measurements on this laptop** (Windows 11, i5-14450HX, 16 GB RAM, KIOXIA 512 GB NVMe; unbuffered I/O on both systems). They are **not** the paper's numbers and are not numerically comparable to them (different hardware, no GPU, synthetic traces; DECISIONS D2). Paper values appear only as directional context.

## Configuration (identical for both systems; `fckv.conf` + sweep)

- Trace: `agentic` — 32 sessions × 12 turns × 8 layers × 4 blocks/turn (8 KiB blocks) + shared prompt (8 blocks/layer), fanout 1 → **96 MiB unique blocks**; 1,089 records; 180,224 logical block gets per run.
- Budgets: hbm window 16 MiB · DRAM 64 MiB · SSD 4 GiB · queue 256 · 2+2 workers.
- Sweep: qps ∈ {1, 4} × seeds {11, 12} × systems {baseline, flatcake}, **cold start each run** (paper §5.1 protocol: no warm-up). Raw records: `results/*.json`; per-window CSVs; `results/plots/`.

## Headline results

| Metric (mean over seeds) | Tiered baseline | FlatCake | Delta |
|---|---|---|---|
| Delivered hit rate @ qps4 | 99.57% | **100.00%** | +0.4 pp |
| Delivered hit rate @ qps1 | 86.15–87.36% (seed spread) | **100.00%** (both seeds) | +13–14 pp |
| Physical read ops @ qps4 | 38,272 | **352** | **109× fewer** |
| Physical read ops @ qps1 | 26,771–29,277 | **592–804** | **36–45× fewer** |
| Write amplification @ qps4 | 3.269× (promotion churn) | **0.995×** (write-exactly-once) | 3.3× less SSD wear |
| Read amplification | 1.000× | 1.000–1.003× | parity (no overread waste; Δgap bridging pays for itself) |
| Integrity failures | 0 / 0 | 0 / 0 | every delivered block byte-exact |
| Recompute (modeled) @ qps1 | 46.6–51.1 s worth | **0** | — |

Per-run records: `results/plots/summary.csv`; charts `results/plots/*.png` (hit rate vs load, amplification, latency, per-medium split).

## Interpretation against the paper's claims (directional comparison only)

1. **Hit-rate stability under load (paper Fig. 6, claim: FlatCake stays near ceiling while tiered collapses)** — reproduced directionally: FlatCake holds 100.00% delivered hits at every load/seed; the baseline loses 13–14 pp at qps=1 (staging exhaustion → prefetch aborts → recompute, plus FIFO bucket churn). Same shape as the paper's Mooncake 87.9%→64.3% collapse at load.
2. **Coalescing effect (paper §3.4, Fig. 8)** — the strongest mechanism evidence at Level B: 109× fewer physical read ops at qps=4 for identical logical reads. Consistent with the raw-disk calibration (`results/io_probe.json`): 1 MiB streaming at 1,075 MB/s vs 4 KiB random at 56 MB/s ⇒ ~19× per-op cost gap; O(layers) vs O(layers×blocks) op counts compound it.
3. **Write-exactly-once (paper Fig. 9, claim 1.000× vs tiered 1.007–1.018×)** — FlatCake 0.995× (the small deficit is reclaim re-persist after watermark frees); baseline 3.269× at qps=4 (promotion + recompute re-persist churn). Our baseline's amplification is much larger than the paper's because our 64 MiB DRAM vs 96 MiB working set forces constant churn — at the paper's 1 TB DRAM, little evicts. Honest scale note, not a defect.
4. **Per-medium split (paper Fig. 7)** — FlatCake serves SSD-heavy mixes (ssd ≈ 66k–125k hits, hbm ≈ 54k–114k, dram ≈ 0.1–1.5k): the write-once policy sends session history to SSD and only shared/short scopes to DRAM — the DRAM share is small, matching A1's R1 prediction (below the paper's 8–11.7% band; a workload-shape property of our traces, logged in OPEN_QUESTIONS context).

## Caveats (carried from BENCHMARK_METHODOLOGY.md)

- **Latency is not a fair axis here**: baseline misses "recompute" as instant bookkeeping (`modeled_recompute_us`) while FlatCake's hits pay real unbuffered I/O. At qps=4 (no sustained overload) latencies are comparable (p50 1.2–3.1 ms vs 1.5–4.5 ms; FlatCake p99 21–24 ms with queueing). At qps=1 both systems saturate (arrival > service); FlatCake's honest real-I/O queueing shows up as large p99 (queue-full backpressure). The paper compares real GPU prefill on both sides; Level B cannot.
- The qps=1 seed-12 pair shows sustained overload behavior (both runs exceed trace duration; FlatCake's 14,983 s drain reflects 125k real SSD block-hits through 2 workers). Overload dynamics are honest but not a paper-comparable measurement; flagged in EXPERIMENT_LOG.
- Two seeds per cell, not 3 (sweep tightened for wall-clock; seed spread visible in the table; consistent direction).

## Reproduction

```bash
bash benchmarks/run_sweep.sh          # ~5-7 h wall clock on this laptop (dominated by qps=1 overload drains)
python python/plot_results.py results/ results/plots
python -I scripts/audit_io_probe.py   # raw-disk calibration
```
