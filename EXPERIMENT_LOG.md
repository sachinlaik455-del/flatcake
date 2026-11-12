# EXPERIMENT_LOG.md — benchmark configurations and results (append-only)

Every row = one executed run with its exact reproduction command. Results are never written before the experiment runs (Directive Part 17). Raw records live in `results/*.json` (pinned schema) + `results/plots/summary.csv`.

| Run ID | Date/epoch | System | Trace (regime, sessions, qps, seed) | Config deltas | Result summary (hit rate / read amp / p50) | Artifacts | Repro command |
|---|---|---|---|---|---|---|---|
| io-probe | 2026-11-11 | n/a | n/a (256 MiB file) | none | unbuffered: 4k-rand 56 MB/s · 4k-seq 111 · 1M-seq 1075; buffered 4k-rand ~2900 (cache artifact) | results/io_probe.json | `python -I scripts/audit_io_probe.py 256` |
| demo-01 | 2026-11-11 | both | agentic 3s×10t, qps5, seed42 | demo budgets (2 MiB DRAM) | baseline 35.32% / flatcake 82.52%, integrity 0/0 | terminal (README) | `./target/release/fckv.exe demo` |
| sweep qps1 seed11 | 2026-11-11→12 | both | agentic 32s×12t, qps1, seed11 | fckv.conf | baseline 87.36% / 1.000x / 66.9ms · flatcake 100.00% / 1.000x / 626.6ms (real I/O; overload p99) | results/{baseline,flatcake}-qps1-seed11.json | `bash benchmarks/run_sweep.sh` |
| sweep qps1 seed12 | 2026-11-11→12 | both | same, seed12 | fckv.conf | baseline 86.15% / 1.000x / 868.5ms · flatcake 100.00% / 1.003x / 728.3ms (sustained overload both; drains dominate wall clock: 4.6 ks / 15.0 ks) | results/*-qps1-seed12.json | as above |
| sweep qps4 seed11 | 2026-11-12 | both | same, qps4, seed11 | fckv.conf | baseline 99.57% / 1.000x / 3.1ms, write_amp 3.269x, 38,272 ops · flatcake 100.00% / 1.000x / 4.5ms, write_amp 0.995x, **352 ops** | results/*-qps4-seed11.json | as above |
| sweep qps4 seed12 | 2026-11-12 | both | same, qps4, seed12 | fckv.conf | baseline 99.57% / 1.219ms p50 · flatcake 100.00% / 1.541ms, 352 ops | results/*-qps4-seed12.json | as above |

Notes: integrity_failures = 0 in every run. qps=1 cells are overload regimes (arrival > service for large-prefix restores) — reported with the overload caveat in RESULTS.md; the honest comparison cells are qps=4 plus the mechanism metrics (ops, amplification) everywhere.

## Environment note (applies to all runs)

Windows 11, i5-14450HX (10C/16T), 15.7 GB RAM, KIOXIA 512 GB NVMe (single drive — NOT the paper's 8× PM9A3 RAID0; absolute throughput numbers are not comparable to the paper, D2). FILE_FLAG_NO_BUFFERING on both systems (D12). Cold start, no warm-up (paper §5.1 protocol). Multiple seeds; variance reported in RESULTS.md.

## M11 adaptive-placement runs (2026-11-12)

24 runs: 4 policies x {shift, mixed} x seeds {21,22,23} (eval set B; tuning grid = declared defaults only, D25).
Pre-fix generator (phantom reuse): static 99.12-100%, adaptive identical, fixed_dram 82.75-96.15%.
Post-fix generator (D26): static 100.00 all cells; adaptive 100.00 all cells; fixed_ssd 100.00; fixed_dram 83.33-96.15%.
Artifacts: results/m11/*.json. Repro: `bash benchmarks/run_m11.sh`. integrity_fail=0 in all 24.
