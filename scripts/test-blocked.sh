#!/usr/bin/env bash
set -uo pipefail

# Check that the tests blocked on a known bug still fail (an "xfail" check).
#
# A test that pins down a known, unfixed library bug keeps its assertions,
# is ignored with the finding it waits for, and has `blocked_on_` in its name
# (so scripts/test-couchdb.sh can skip it with a plain libtest filter):
#
#   #[tokio::test]
#   #[ignore = "blocked on F03"]
#   async fn blocked_on_f03_inline_attachments() { ... }
#
# This runs all of them and fails if any passes: the bug is fixed (or the test
# is wrong), so drop the marker and the name prefix in the fixing PR. Blocked
# CouchDB tests need CouchDB, as for scripts/test-couchdb.sh.
#
# Usage: bash scripts/test-blocked.sh [cargo test args...]

cd "$(dirname "$0")/.." || exit

export COUCHDB_URL="${COUCHDB_URL:-http://admin:password@localhost:15984}"

# The whole workspace, unless the arguments select packages.
scope=(--workspace)
for arg in "$@"; do
    case $arg in -p | -p* | --package | --package=*) scope=() ;; esac
done

log=$(mktemp)
trap 'rm -f "$log"' EXIT

cargo test ${scope[@]+"${scope[@]}"} --tests --no-fail-fast "$@" -- --ignored blocked_on_ 2>&1 | tee "$log"
status=${PIPESTATUS[0]}

total=$(grep -cE '^test .*blocked_on_.* \.\.\. ' "$log")
passing=$(sed -nE 's/^test (.*blocked_on_.*) \.\.\. ok$/\1/p' "$log")
failing=$(grep -cE '^test .*blocked_on_.* \.\.\. FAILED$' "$log")

if [ -n "$passing" ]; then
    echo
    echo "error: these tests are marked as blocked on a bug but pass now." >&2
    echo "If the bug is fixed, remove their #[ignore = \"blocked on ...\"] and" \
        "the blocked_on_ name prefix:" >&2
    while read -r name; do echo "  $name"; done <<<"$passing" >&2
    exit 1
fi
if [ "$status" -ne 0 ] && [ "$failing" -eq 0 ]; then
    echo "error: cargo test failed before running the blocked tests" >&2
    exit "$status"
fi
if [ "$total" -eq 0 ]; then
    echo "No blocked tests"
else
    echo "$total blocked test(s), all still failing"
fi
