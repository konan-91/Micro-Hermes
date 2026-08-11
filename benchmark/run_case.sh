#!/usr/bin/env bash
# Run one (policy, case, load, trial) benchmark point against the real LB.
# Starts hermes (root, for eBPF), waits until it is listening, runs
# hermes-bench against it, stops the LB and collects both CSVs into
# benchmark/results/ with phase-1-compatible naming.
#
# Usage  run_case.sh <policy> <case> <load> <trial> [seed]
set -euo pipefail

POLICY="${1:?policy: hermes|reuseport|lifo}"
CASE="${2:?case: 1|2|3|4|5}"
LOAD="${3:?load: light|medium|heavy}"
TRIAL="${4:?trial number}"
SEED="${5:-$TRIAL}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_DIR="$ROOT/target/release"
RESULTS_DIR="$ROOT/benchmark/results"
PORT="${HERMES_PORT:-7878}"
RUN_TAG="${POLICY}_case${CASE}_${LOAD}_trial${TRIAL}"
METRICS_DIR="$(mktemp -d "${TMPDIR:-/tmp}/hermes_metrics_${RUN_TAG}_XXXX")"
CONNS_CSV="$RESULTS_DIR/${RUN_TAG}_conns.csv"

mkdir -p "$RESULTS_DIR"

if [[ ! -x "$BIN_DIR/hermes" || ! -x "$BIN_DIR/hermes-bench" ]]; then
    echo "error: $BIN_DIR/{hermes,hermes-bench} not built. Run:" >&2
    echo "  cargo build --release -p hermes -p hermes-bench" >&2
    exit 1
fi

# Case 2 injects a worker hang to exercise Stage 1. Harmless under the
# baselines, worker 0 just stalls regardless of policy
HANG_ENV=()
if [[ "$CASE" == "2" ]]; then
    HANG_ENV=(HERMES_HANG_INJECT=0:1500:400)
fi

echo "[run_case] starting hermes LB: policy=$POLICY port=$PORT metrics=$METRICS_DIR"
sudo -E env "${HANG_ENV[@]}" "$BIN_DIR/hermes" --policy "$POLICY" --port "$PORT" --metrics-dir "$METRICS_DIR" \
    > "$METRICS_DIR/lb.stdout.log" 2> "$METRICS_DIR/lb.stderr.log" &
LB_PID=$!

cleanup() {
    if kill -0 "$LB_PID" 2>/dev/null; then
        sudo kill -TERM "$LB_PID" 2>/dev/null || true
        wait "$LB_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT

# wait until the LB is actually listening rather than a fixed sleep
echo "[run_case] waiting for port $PORT..."
for _ in $(seq 1 100); do
    if (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then
        exec 3>&- 3<&-
        break
    fi
    if ! kill -0 "$LB_PID" 2>/dev/null; then
        echo "error: hermes LB exited before listening; see $METRICS_DIR/lb.stderr.log" >&2
        cat "$METRICS_DIR/lb.stderr.log" >&2
        exit 1
    fi
    sleep 0.1
done

echo "[run_case] running hermes-bench: case=$CASE load=$LOAD seed=$SEED -> $CONNS_CSV"
"$BIN_DIR/hermes-bench" --port "$PORT" --case "$CASE" --load "$LOAD" --seed "$SEED" \
    --label "$POLICY" --out "$CONNS_CSV"

echo "[run_case] stopping hermes LB"
sudo kill -TERM "$LB_PID"
wait "$LB_PID" 2>/dev/null || true
trap - EXIT

for f in "$METRICS_DIR"/w*_ticks.csv; do
    [[ -e "$f" ]] || continue
    base="$(basename "$f" .csv)" # w{N}_ticks
    worker="${base%%_ticks}"
    cp "$f" "$RESULTS_DIR/${RUN_TAG}_${worker}_ticks.csv"
done
rm -rf "$METRICS_DIR"

echo "[run_case] done: $RESULTS_DIR/${RUN_TAG}_{conns,w*_ticks}.csv"
