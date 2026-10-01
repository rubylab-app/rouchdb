# rouchdb-cli

Command-line tool for [RouchDB](https://github.com/rubylab-app/rouchdb) database files: inspect, query and edit `.redb` files, replicate them with CouchDB, compact them, and upgrade files written by rouchdb 0.4.

```bash
cargo install rouchdb-cli
```

This installs the `rouchdb` binary. It requires Rust 1.88 or later.

## Reading

```bash
rouchdb info mydb.redb                    # Database info
rouchdb get mydb.redb user:alice          # Get document by ID
rouchdb all-docs mydb.redb --include-docs # List all documents
rouchdb find mydb.redb --selector '{"age": {"$gte": 30}}'  # Mango query
rouchdb changes mydb.redb --include-docs  # Changes feed
rouchdb dump mydb.redb --pretty           # Export all docs as JSON
```

## Writing

```bash
rouchdb put mydb.redb user:alice '{"name":"Alice","age":30}'              # Create
rouchdb put mydb.redb user:alice '{"name":"Alice","age":31}' --rev 1-abc  # Update
rouchdb put mydb.redb user:alice '{"name":"Alice","age":32}' --force      # Upsert (auto-fetches rev)
rouchdb post mydb.redb '{"name":"Bob","age":25}'                       # Auto-ID
rouchdb delete mydb.redb user:alice --rev 2-def                        # Delete
rouchdb import mydb.redb docs.json                                     # Bulk import
```

`dump` exports each document's winning revision with its attachments inlined as
base64, and `import` restores them. Revision history and conflicting revisions
are not exported (`dump` warns about documents with conflicts); use `replicate`
to copy a database with its full history. Read-only commands fail instead of
creating a missing `.redb` file, and every command exits with status 1 on
failure (including a `replicate` or `import` that only partly succeeded).

## Operations

```bash
rouchdb replicate mydb.redb http://admin:password@localhost:5984/mydb  # Sync
rouchdb compact mydb.redb                                              # Compact
rouchdb migrate mydb.redb                  # Upgrade a file written by rouchdb 0.4 (backup first)
```

To keep the CouchDB password out of shell history and `ps`, `replicate` reads
credentials from `ROUCHDB_USER` and `ROUCHDB_PASSWORD` for any http(s) URL that
has none of its own:

```bash
ROUCHDB_USER=admin ROUCHDB_PASSWORD=password \
  rouchdb replicate mydb.redb http://localhost:5984/mydb
```

Add `--pretty` (or `-p`) to any command for formatted JSON output.

See the [RouchDB book](https://rubylab-app.github.io/rouchdb/) for the library and the database format. Licensed under MIT.
