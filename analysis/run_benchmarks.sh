#!/usr/bin/env bash
# Benchmark matrix runner for micro-hermes.
#
# Runs every POLICY × WORKLOAD_CASE × LOAD combination TRIALS times (each
# trial with a distinct SEED so the synthetic connection stream differs
# reproducibly) and writes per-run CSVs into analysis/results/:
#
#   {policy}_case{case}_{load}_trial{t}_ticks.csv — per-iteration WST snapshots
#   {policy}_case{case}_{load}_trial{t}_conns.csv — per-connection latency rows
#
# Cases 1-4 sweep three offered-load levels (light/medium/heavy), mirroring
# the paper's Table 3. Case 5 (the burst-evidence scenario) runs at a single
# level, tagged "medium" for filename uniformity.
#
# Usage:  ./run_benchmarks.sh            # 3 trials (≈ 8 min total)
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
run_one() {
  local trial="$1" case="$2" load="$3" policy="$4"
  local ticks="$RESULTS_DIR/${policy}_case${case}_${load}_trial${trial}_ticks.csv"
  local conns="$RESULTS_DIR/${policy}_case${case}_${load}_trial${trial}_conns.csv"
  total=$((total + 1))
  if [[ -s "$ticks" && -s "$conns" ]]; then
    skipped=$((skipped + 1))
    return
  fi
  echo "[bench] trial $trial/$TRIALS  case $case  $load  $policy"
  POLICY="$policy" WORKLOAD_CASE="$case" LOAD="$load" SEED="$trial" \
    METRICS_PATH="$ticks" CONNS_PATH="$conns" \
    "$BIN" > /dev/null
}

for trial in $(seq 1 "$TRIALS"); do
  for case in 1 2 3 4; do
    for load in light medium heavy; do
      for policy in hermes lifo reuseport; do
        run_one "$trial" "$case" "$load" "$policy"
      done
    done
  done
  # Case 5: single level (our extension, outside the paper's sweep).
  for policy in hermes lifo reuseport; do
    run_one "$trial" 5 medium "$policy"
  done
done

echo "[bench] done: $((total - skipped)) runs executed, $skipped skipped (already present)"
echo "[bench] results in $RESULTS_DIR"
