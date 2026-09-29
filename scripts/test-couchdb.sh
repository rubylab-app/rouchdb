#!/usr/bin/env bash
set -euo pipefail

# Run the tests that need a real CouchDB: every #[ignore = "requires CouchDB"]
# test of the workspace, in parallel.
#
# Prerequisites: CouchDB 3 reachable at $COUCHDB_URL
#   (default http://admin:password@localhost:15984, see docker-compose.yml)
#
# Tests that pin down a known, unfixed library bug are
# #[ignore = "blocked on Fxx"] and have `blocked_on_` in their name (e.g.
# `fn blocked_on_f03_inline_attachments`), so libtest's --skip leaves them
# out. scripts/test-blocked.sh checks that they still fail.
#
# Every database the tests create is named rouchdb_test_* and deleted by its
# own test, also when it fails. The run fails if one of its databases is left
# behind. --sweep deletes all rouchdb_test_* databases (the leftovers of a
# killed run); do not use it while another suite runs on the same server.
#
# Extra arguments go to `cargo test` (e.g. --locked, -p rouchdb).
#
# Usage: bash scripts/test-couchdb.sh [cargo test args...]
#        bash scripts/test-couchdb.sh --sweep

cd "$(dirname "$0")/.."

export COUCHDB_URL="${COUCHDB_URL:-http://admin:password@localhost:15984}"
prefix=rouchdb_test_

# Names of the databases that start with $1.
list_dbs() {
    local all
    if ! all=$(curl -fsS "$COUCHDB_URL/_all_dbs"); then
        echo "warning: could not list the CouchDB databases" >&2
        return 0
    fi
    tr -d '[]"' <<<"$all" | tr ',' '\n' | grep "^$1" || true
}

delete_dbs() {
    local db
    for db in "$@"; do
        echo "  $db"
        curl -fsS -o /dev/null -X DELETE "$COUCHDB_URL/$db" ||
            echo "warning: could not delete $db" >&2
    done
}

if [ "${1:-}" = --sweep ]; then
    # shellcheck disable=SC2046 # database names contain no whitespace
    set -- $(list_dbs "$prefix")
    echo "Deleting $# test database(s)"
    delete_dbs "$@"
    exit 0
fi

if ! curl -fs -o /dev/null "$COUCHDB_URL/_up"; then
    echo "error: CouchDB is not reachable at $(sed -E 's#//[^/@]*@#//#' <<<"$COUCHDB_URL")" \
        "(start it with: docker compose up -d)" >&2
    exit 1
fi

# The tests put this in their database names, so the check below only looks
# at this run's databases, not at those of a suite running concurrently.
ROUCHDB_TEST_RUN="r$(date +%s)$$"
export ROUCHDB_TEST_RUN

check_leftovers() {
    local status=$? leftovers
    leftovers=$(list_dbs "$prefix${ROUCHDB_TEST_RUN}_")
    if [ -n "$leftovers" ]; then
        echo "error: the tests left these CouchDB databases behind (now deleted);" \
            "create test databases with common::fresh_remote_db/unique_remote_db:" >&2
        # shellcheck disable=SC2086 # one name per line, no whitespace
        delete_dbs $leftovers >&2
        [ "$status" -ne 0 ] || status=1
    fi
    exit "$status"
}
trap check_leftovers EXIT

# The whole workspace, unless the arguments select packages.
scope=(--workspace)
for arg in "$@"; do
    case $arg in -p | -p* | --package | --package=*) scope=() ;; esac
done

# --tests leaves out the doctests: with --ignored, libtest would also try to
# run the ```ignore ones.
cargo test ${scope[@]+"${scope[@]}"} --tests --no-fail-fast "$@" -- --ignored --skip blocked_on_
