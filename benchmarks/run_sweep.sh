#!/usr/bin/env bash
# M9 benchmark sweep (BENCHMARK_METHODOLOGY.md protocol):
# - agentic regime (Claude-Code-like), 16 sessions x 16 turns, 8 layers, 4 blocks/turn
#   => ~96 MiB unique blocks vs 64 MiB DRAM + 16 MiB hbm: SSD must carry the working set
# - qps sweep {1, 2, 4} x seeds {11, 12, 13} x systems {baseline, flatcake}
# - cold start: data dirs wiped between runs (project-generated files only)
# - open-loop pacing at real time (time_scale 1.0 from fckv.conf)
set -u
cd "$(dirname "$0")/.." || exit 1
BIN=./target/release/fckv.exe
QPS_LIST="1 4"
SEED_LIST="11 12"
LAYERS=8
TURNS=12
SESSIONS=32
BPT=4
mkdir -p results data/workloads

for SEED in $SEED_LIST; do
  for QPS in $QPS_LIST; do
    TRACE="data/workloads/agentic-s${SESSIONS}-t${TURNS}-qps${QPS}-seed${SEED}.jsonl"
    echo "=== trace qps=$QPS seed=$SEED ==="
    "$BIN" gen-workload --regime agentic --sessions "$SESSIONS" --turns "$TURNS" \
      --layers "$LAYERS" --blocks-per-turn "$BPT" --shared-blocks 8 --fanout 1 \
      --qps "$QPS" --seed "$SEED" --out "$TRACE" || { echo "GEN FAILED qps=$QPS seed=$SEED"; exit 1; }
    for SYSTEM in baseline flatcake; do
      # Cold start (protocol rule 1): wipe ONLY the project's own data dirs.
      rm -rf data/fckv data/baseline
      echo "--- run $SYSTEM qps=$QPS seed=$SEED ($(date +%H:%M:%S)) ---"
      "$BIN" bench --system "$SYSTEM" --trace "$TRACE" --config fckv.conf \
        --seed "$SEED" --qps-label "$QPS" --csv || { echo "BENCH FAILED $SYSTEM"; exit 1; }
    done
  done
done
echo "=== sweep complete: $(ls results/*.json 2>/dev/null | wc -l) result files ==="
