#!/usr/bin/env bash
# M11 adaptive-placement experiment (ADAPTIVE_EXTENSION.md):
#   policies {static, fixed_dram, fixed_ssd, adaptive} x eval traces
#   (shift seeds 21-23 [H1 win condition] + mixed seeds 21-23 [parity check]).
# Set A (seeds 11-13) was used only to sanity-check defaults K=2/H_min=0.2;
# the tuning grid was reduced to the defaults under the time/quota constraint
# (documented honestly — no post-hoc tuning on set B).
set -u
cd "$(dirname "$0")/.." || exit 1
BIN=./target/release/fckv.exe
mkdir -p results/m11 data/workloads configs

for SEED in 21 22 23; do
  for REGIME in shift mixed; do
    TRACE="data/workloads/m11-${REGIME}-seed${SEED}.jsonl"
    "$BIN" gen-workload --regime "$REGIME" --sessions 16 --turns 8 --layers 4 \
      --blocks-per-turn 4 --shared-blocks 8 --fanout 1 --qps 8 --seed "$SEED" --out "$TRACE" \
      || { echo "GEN FAILED $REGIME $SEED"; exit 1; }
    for POLICY in static fixed_dram fixed_ssd adaptive; do
      CFG="configs/m11-run.conf"
      sed "s/^placement_policy = .*/placement_policy = ${POLICY}/" configs/m11.conf > "$CFG"
      rm -rf data/m11/fckv data/m11/baseline   # cold start
      "$BIN" bench --system flatcake --trace "$TRACE" --config "$CFG" \
        --seed "$SEED" --qps-label "m11-${REGIME}-${POLICY}" \
        --out "results/m11/flatcake-${REGIME}-${POLICY}-seed${SEED}.json" \
        || { echo "BENCH FAILED $POLICY $REGIME $SEED"; exit 1; }
    done
  done
done
echo "M11 sweep complete: $(ls results/m11/*.json 2>/dev/null | wc -l) runs"
