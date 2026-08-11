#!/usr/bin/env bash
# Full benchmark matrix, 3 policies x 5 cases x load levels x TRIALS,
# driven through run_case.sh. Each point starts and stops a real hermes
# process under sudo, so run `sudo -v` first to avoid repeated prompts.
#
# Usage  ./run_all.sh            (3 trials)
#        TRIALS=5 ./run_all.sh   (more trials for tighter error bars)
#
# Existing result files are skipped so an interrupted run can be resumed.
# Delete benchmark/results/ to force a full re-run
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"
RESULTS_DIR="$SCRIPT_DIR/results"
TRIALS="${TRIALS:-3}"

# Prime sudo's credential cache, best effort only. Non-interactive runs
# rely on a NOPASSWD sudoers rule instead, so don't let a failure here
# trip set -e
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
