#!/usr/bin/env bash
set -euo pipefail

# Run the #[ignore]d test suite: the tests that need a real CouchDB.
#
# Prerequisites: CouchDB 3 reachable at $COUCHDB_URL
#   (default http://admin:password@localhost:15984, see docker-compose.yml)
#
# Tests marked #[ignore = "blocked on Fxx"] pin down known library bugs and
# fail until the fix lands, so they are skipped. Extra arguments go to
# `cargo test` (e.g. --locked, -p rouchdb).
#
# Usage: bash scripts/test-couchdb.sh [cargo test args...]

cd "$(dirname "$0")/.."

# shellcheck disable=SC2016 # $0 belongs to the awk program
blocked=$(git ls-files '*.rs' | xargs awk '
    /#\[ignore = "blocked on/ { blocked = 1; next }
    blocked && match($0, /fn [A-Za-z0-9_]+/) {
        print substr($0, RSTART + 3, RLENGTH - 3); blocked = 0
    }')

skips=()
for name in $blocked; do
    skips+=(--skip "$name")
done
echo "Skipping blocked tests: ${blocked//$'\n'/ }"

exec cargo test --workspace --no-fail-fast "$@" -- --ignored --test-threads=1 ${skips[@]+"${skips[@]}"}
