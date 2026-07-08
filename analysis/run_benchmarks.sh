#!/usr/bin/env bash
# Benchmark matrix runner for micro-hermes.
#
# Runs every POLICY × WORKLOAD_CASE combination TRIALS times (each trial with
# a distinct SEED so the synthetic connection stream differs reproducibly)
# and writes per-run CSVs into analysis/results/:
#
#   {policy}_case{case}_trial{t}_ticks.csv   — per-iteration WST snapshots
#   {policy}_case{case}_trial{t}_conns.csv   — per-connection latency records
#
# Usage:  ./run_benchmarks.sh            # 3 trials (≈ 2.5 min total)
#         TRIALS=5 ./run_benchmarks.sh   # more trials for tighter error bars
#
# Idempotence: existing result files are skipped, so a partial run can be
# resumed; delete analysis/results/ to force a full re-run.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"
RESULTS_DIR="$SCRIPT_DIR/results"
TRIALS="${TRIALS:-3}"

mkdir -p "$RESULTS_DIR"

echo "[bench] building release binary..."
cargo build --release --quiet --manifest-path "$REPO_ROOT/Cargo.toml"
BIN="$REPO_ROOT/target/release/micro-hermes"

total=0
skipped=0
for trial in $(seq 1 "$TRIALS"); do
  for case in 1 2 3 4; do
    for policy in hermes lifo reuseport; do
      ticks="$RESULTS_DIR/${policy}_case${case}_trial${trial}_ticks.csv"
      conns="$RESULTS_DIR/${policy}_case${case}_trial${trial}_conns.csv"
      total=$((total + 1))
      if [[ -s "$ticks" && -s "$conns" ]]; then
        skipped=$((skipped + 1))
        continue
      fi
      echo "[bench] trial $trial/$TRIALS  case $case  $policy"
      POLICY="$policy" WORKLOAD_CASE="$case" SEED="$trial" \
        METRICS_PATH="$ticks" CONNS_PATH="$conns" \
        "$BIN" > /dev/null
    done
  done
done

echo "[bench] done: $((total - skipped)) runs executed, $skipped skipped (already present)"
echo "[bench] results in $RESULTS_DIR"
