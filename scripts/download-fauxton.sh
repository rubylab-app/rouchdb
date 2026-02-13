#!/usr/bin/env bash
set -euo pipefail

# Build Fauxton (CouchDB's official dashboard UI) from source.
#
# Prerequisites: Node.js (>=10) and npm
# Source: https://github.com/apache/couchdb-fauxton
#
# Usage: bash scripts/download-fauxton.sh

MIN_NODE_VERSION=10

# --- Check prerequisites ---
if ! command -v node &>/dev/null; then
    echo "ERROR: Node.js is not installed. Install Node.js >= $MIN_NODE_VERSION and try again."
    exit 1
fi

if ! command -v npm &>/dev/null; then
    echo "ERROR: npm is not installed. Install Node.js >= $MIN_NODE_VERSION (includes npm) and try again."
    exit 1
fi

NODE_VERSION=$(node -v | sed 's/^v//' | cut -d. -f1)
if [ "$NODE_VERSION" -lt "$MIN_NODE_VERSION" ]; then
    echo "ERROR: Node.js >= $MIN_NODE_VERSION required, but found v$(node -v | sed 's/^v//')."
    exit 1
fi

echo "Using Node.js $(node -v), npm $(npm -v)"

DEST="crates/rouchdb-server/fauxton"
REPO="https://github.com/apache/couchdb-fauxton.git"
BRANCH="main"
TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

echo "Cloning apache/couchdb-fauxton..."
git clone --depth 1 --branch "$BRANCH" "$REPO" "$TMPDIR/fauxton"

echo "Installing dependencies..."
cd "$TMPDIR/fauxton"
npm install --production=false 2>&1 | tail -1

echo "Building Fauxton for production..."
npx grunt release 2>&1 | tail -3

cd - >/dev/null

echo "Copying Fauxton files to $DEST..."
rm -rf "$DEST"
mkdir -p "$DEST"
cp -R "$TMPDIR/fauxton/dist/release/"* "$DEST/"

echo ""
echo "Done! Fauxton installed to $DEST"
echo "Files: $(find "$DEST" -type f | wc -l | tr -d ' ')"
echo "Rebuild with: cargo build -p rouchdb-server"
