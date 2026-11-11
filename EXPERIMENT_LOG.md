# EXPERIMENT_LOG.md — benchmark configurations and results (append-only)

Every row = one executed run with its exact reproduction command. Results are never written before the experiment runs (Directive Part 17). Raw records live in `results/*.json` (pinned schema) + `results/plots/summary.csv`.

| Run ID | Date/epoch | System | Trace (regime, sessions, qps, seed) | Config deltas | Result summary (hit rate / read amp / p50) | Artifacts | Repro command |
|---|---|---|---|---|---|---|---|

## Environment note (applies to all runs)

Windows 11, i5-14450HX (10C/16T), 15.7 GB RAM, KIOXIA 512 GB NVMe (single drive — NOT the paper's 8× PM9A3 RAID0; absolute throughput numbers are not comparable to the paper, D2). FILE_FLAG_NO_BUFFERING on both systems (D12). Cold start, no warm-up (paper §5.1 protocol). Multiple seeds; variance reported in RESULTS.md.
