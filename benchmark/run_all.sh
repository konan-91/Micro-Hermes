#!/usr/bin/env bash
# Full benchmark matrix: 3 policies x 5 cases x (3 load levels for cases
# 1-4, 1 for case 5) x TRIALS, driven through run_case.sh — the phase-2
# analogue of phase 1's analysis/run_benchmarks.sh, same output naming
# convention so the existing notebook needs minimal changes to read either.
#
# Each run_case.sh invocation starts and stops a real `hermes` process
# under sudo, so this is much slower per-point than phase 1's in-process
# runs (expect real process startup/eBPF-attach/shutdown overhead on every
# point, not just once). Run `sudo -v` first so sudo's credential cache
# covers the whole matrix instead of prompting repeatedly.
#
# Usage:  ./run_all.sh            # 3 trials
#         TRIALS=5 ./run_all.sh   # more trials for tighter error bars
#
# Idempotent: existing result files are skipped, so a partial/interrupted
# run can be resumed; delete benchmark/results/ to force a full re-run.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"
RESULTS_DIR="$SCRIPT_DIR/results"
TRIALS="${TRIALS:-3}"

# Prime sudo's credential cache so the matrix doesn't prompt per-point. This
# is a best-effort latency optimisation only: `sudo -v` needs a controlling
# terminal, which it doesn't have when this script runs non-interactively
# (e.g. backgrounded, or under sudo-rs on Ubuntu where `-v` is terminal-only).
# In those setups a passwordless sudoers rule for the `hermes` binary is what
# actually authorises each start, so a failure here is non-fatal — the real
# `sudo -E ... hermes` calls in run_case.sh still succeed. Don't let it trip
# `set -e`.
echo "[bench] priming sudo credential cache (best-effort; needed for every 'hermes' start)"
sudo -v 2>/dev/null || echo "[bench] sudo -v unavailable (non-interactive / sudo-rs); relying on NOPASSWD sudoers rule"

echo "[bench] building release binaries..."
cargo build --release --quiet --manifest-path "$REPO_ROOT/Cargo.toml" -p hermes -p hermes-bench

mkdir -p "$RESULTS_DIR"

total=0
skipped=0
run_one() {
    local policy="$1" case="$2" load="$3" trial="$4"
    local tag="${policy}_case${case}_${load}_trial${trial}"
    total=$((total + 1))
    if [[ -s "$RESULTS_DIR/${tag}_conns.csv" ]]; then
        skipped=$((skipped + 1))
        return
    fi
    echo "[bench] trial $trial/$TRIALS  case $case  $load  $policy"
    "$SCRIPT_DIR/run_case.sh" "$policy" "$case" "$load" "$trial"
}

for trial in $(seq 1 "$TRIALS"); do
    for case in 1 2 3 4; do
        for load in light medium heavy; do
            for policy in hermes lifo reuseport; do
                run_one "$policy" "$case" "$load" "$trial"
            done
        done
    done
    for policy in hermes lifo reuseport; do
        run_one "$policy" 5 medium "$trial"
    done
done

echo "[bench] done: $((total - skipped)) runs executed, $skipped skipped (already present)"
echo "[bench] results in $RESULTS_DIR"
