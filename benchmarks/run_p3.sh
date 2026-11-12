#!/usr/bin/env bash
# P3 benchmark: medium-aware migration vs paper-faithful write-once, identical
# workload. Migration-favorable shape: agentic sessions classified SSD at turn 1
# (one-shot rule) but heavily re-accessed — static serves every reuse from SSD;
# migration promotes hot blocks to DRAM. Same budgets both runs.
# NOTE: run only AFTER the M9 sweep finishes (raw-I/O timing hygiene).
set -u
cd "$(dirname "$0")/.." || exit 1
BIN=./target/release/fckv.exe
mkdir -p results/p3 data/workloads

TRACE="data/workloads/p3-migration.jsonl"
"$BIN" gen-workload --regime agentic --sessions 8 --turns 12 --layers 4 \
  --blocks-per-turn 4 --shared-blocks 8 --fanout 1 --qps 8 --seed 41 --out "$TRACE" \
  || { echo "GEN FAILED"; exit 1; }

for MODE in static migrated; do
  CFG="configs/p3-run.conf"
  if [ "$MODE" = "migrated" ]; then
    sed -e 's/^migration_enabled = .*/migration_enabled = 1/' \
        -e 's/^eviction_policy = .*/eviction_policy = medium_aware/' \
        configs/p3.conf > "$CFG"
  else
    cp configs/p3.conf "$CFG"
  fi
  rm -rf data/p3/fckv data/p3/baseline   # cold start
  "$BIN" bench --system flatcake --trace "$TRACE" --config "$CFG" \
    --seed 41 --qps-label "p3-$MODE" \
    --out "results/p3/flatcake-$MODE.json" --csv \
    || { echo "BENCH FAILED $MODE"; exit 1; }
done
echo "P3 benchmark complete: results/p3/"
