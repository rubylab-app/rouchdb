# Development Setup

This guide walks you through setting up a local development environment for RouchDB.

## Prerequisites

### Rust Toolchain

RouchDB requires a recent stable Rust toolchain. Install it via [rustup](https://rustup.rs/):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Verify your installation:

```bash
rustc --version
cargo --version
```

The minimum supported Rust version (MSRV) is 1.88, declared as `rust-version` in the root `Cargo.toml`. The toolchain used for development and CI is pinned in `rust-toolchain.toml`; rustup installs it automatically the first time you run `cargo` inside the repository.

### Docker (for integration tests)

Integration tests run against a real CouchDB instance. You need Docker and Docker Compose installed:

- [Docker Desktop](https://www.docker.com/products/docker-desktop/) (macOS / Windows)
- [Docker Engine](https://docs.docker.com/engine/install/) (Linux)

Docker is **not** required for building the project or running unit tests.

## Clone and Build

```bash
git clone https://github.com/rubylab-app/rouchdb.git
cd rouchdb
cargo build
```

The workspace contains these crates:

| Crate | Purpose |
|---|---|
| `rouchdb-core` | Types, traits, revision tree, merge, collation, errors |
| `rouchdb-adapter-memory` | In-memory adapter (testing and ephemeral use) |
| `rouchdb-adapter-redb` | Persistent local storage via redb |
| `rouchdb-adapter-http` | CouchDB HTTP client adapter |
| `rouchdb-changes` | Streaming changes feed |
| `rouchdb-replication` | CouchDB replication protocol |
| `rouchdb-query` | Mango selectors and map/reduce views |
| `rouchdb-views` | Design documents and persistent view engine |
| `rouchdb` | Umbrella crate that re-exports everything |
| `rouchdb-server` | CouchDB-compatible HTTP server (not published) |
| `rouchdb-cli` | Command-line tool (not published) |
| `rouchdb-bench` | Criterion benchmarks (not published) |

## Running Unit Tests

Unit tests live inside each crate as `#[cfg(test)]` modules. They do not require any external services:

```bash
cargo test
```

This runs all unit tests across every crate in the workspace. To test a single crate:

```bash
cargo test -p rouchdb-core
cargo test -p rouchdb-adapter-memory
```

## Docker Compose Setup for CouchDB

The project includes a `docker-compose.yml` at the repository root that starts a CouchDB 3 instance:

```yaml
services:
  couchdb:
    image: couchdb:3
    ports:
      - "15984:5984"
    environment:
      COUCHDB_USER: admin
      COUCHDB_PASSWORD: password
    healthcheck:
      test: ["CMD", "curl", "-f", "http://localhost:5984/_up"]
      interval: 3s
      timeout: 5s
      retries: 10
```

Start CouchDB:

```bash
docker compose up -d
```

Wait for it to be healthy:

```bash
docker compose ps
```

You should see the `couchdb` service listed as `healthy`. CouchDB is now accessible at `http://localhost:15984` with credentials `admin:password`.

To stop CouchDB when you are done:

```bash
docker compose down
```

## Running Integration Tests

Integration tests verify RouchDB against a real CouchDB server. They are marked `#[ignore]` so they do not run during normal `cargo test`. Run them explicitly:

```bash
docker compose up -d
bash scripts/test-couchdb.sh
```

## Environment Variables

### `COUCHDB_URL`

Override the default CouchDB connection URL used by integration tests. The default is:

```
http://admin:password@localhost:15984
```

To use a different CouchDB instance:

```bash
export COUCHDB_URL="http://myuser:mypass@couchdb.example.com:5984"
bash scripts/test-couchdb.sh
```

## Editor Setup

RouchDB is a standard Cargo workspace. Any editor with rust-analyzer support works well:

- **VS Code** with the rust-analyzer extension
- **RustRover** (JetBrains)
- **Neovim / Helix** with rust-analyzer LSP

No special configuration files are needed beyond what Cargo provides.
