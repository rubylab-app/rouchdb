#!/usr/bin/env bash
set -uo pipefail

# Flaky-test detection: run a test command several times and report every
# test that failed in any of the runs, with how often. Fails if any run
# failed. Used by the nightly workflow; works locally too, e.g.
#
#   bash scripts/repeat-tests.sh 5 cargo test --workspace
#   bash scripts/repeat-tests.sh 5 bash scripts/test-couchdb.sh
#
# Usage: bash scripts/repeat-tests.sh RUNS COMMAND [ARGS...]

if [ "$#" -lt 2 ] || ! [ "$1" -gt 0 ] 2>/dev/null; then
    echo "usage: $0 RUNS COMMAND [ARGS...]" >&2
    exit 2
fi
runs=$1
shift

log=$(mktemp)
failures=$(mktemp)
trap 'rm -f "$log" "$failures"' EXIT

failed_runs=0
for i in $(seq "$runs"); do
    echo "=== Run $i/$runs: $*"
    "$@" 2>&1 | tee "$log"
    status=${PIPESTATUS[0]}
    sed -nE 's/^test (.*) \.\.\. FAILED$/\1/p' "$log" >>"$failures"
    if [ "$status" -ne 0 ]; then
        failed_runs=$((failed_runs + 1))
        if ! grep -qE '^test .* \.\.\. FAILED$' "$log"; then
            echo "(run $i failed without a failing test, exit status $status)" >>"$failures"
        fi
    fi
done

report() {
    if [ "$failed_runs" -eq 0 ]; then
        echo "All $runs runs of \`$*\` passed."
        return
    fi
    echo "$failed_runs of $runs runs of \`$*\` failed. Failures (count, test):"
    echo
    echo '```'
    sort "$failures" | uniq -c | sort -rn
    echo '```'
}

echo
report "$@"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    report "$@" >>"$GITHUB_STEP_SUMMARY"
fi
[ "$failed_runs" -eq 0 ]
