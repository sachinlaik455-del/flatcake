#!/usr/bin/env bash
# P4 benchmark: recompute-heavy workload (2 MiB DRAM vs ~12 MiB working set =>
# reclaim pressure => recompute => small SSD writes). Compares recovery_log
# 0/1/2 (same workload; 2 replays from the previous run's log) + dedup under
# concurrent duplicate gets (the replay uses 8 client threads via replay
# workers; dedup fires on identical concurrent SSD requests).
set -u
cd "$(dirname "$0")/.." || exit 1
BIN=./target/release/fckv.exe
mkdir -p results/p4 data/workloads
TRACE="data/workloads/p4-recompute.jsonl"
"$BIN" gen-workload --regime agentic --sessions 8 --turns 12 --layers 4 \
  --blocks-per-turn 4 --shared-blocks 8 --fanout 1 --qps 16 --seed 91 --out "$TRACE" \
  || { echo "GEN FAILED"; exit 1; }
for MODE in off on replay; do
  rm -rf data/p4/fckv data/p4/baseline
  "$BIN" bench --system flatcake --trace "$TRACE" --config "configs/p4-$MODE.conf" \
    --seed 91 --qps-label "p4-$MODE" \
    --out "results/p4/flatcake-$MODE.json" --csv \
    || { echo "BENCH FAILED $MODE"; exit 1; }
done
echo "P4 benchmark complete: results/p4/"
