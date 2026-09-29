# Changelog

All notable changes to RouchDB are documented here.

This project follows [Semantic Versioning](https://semver.org/). Since we are pre-1.0, minor version bumps may include breaking changes.

---

## [0.5.0] - 2026-09-29

> **Upgrading from 0.4 with redb (`Database::open`) files? Read this first.**
> 0.5 does **not** open a `.redb` file written by 0.4: it returns `RouchError::UpgradeRequired` and leaves the file untouched. Upgrade each file **once**, explicitly:
>
> ```sh
> rouchdb migrate --dry-run app.redb  # only reads the file: reports what the upgrade would change
> rouchdb migrate app.redb            # writes a verified backup to app.redb.rouchdb-0.4.bak first
> ```
>
> (or `RedbAdapter::upgrade(path, UpgradePolicy::WithBackup(None))`, or `Database::open_with(path, name, OpenOptions::new().upgrade(UpgradePolicy::WithBackup(None)))`). **Have about three times the file size of free disk space** when backing up (about twice with `--no-backup`). After the upgrade, and for every file 0.5 creates, **0.4 refuses to open the file** (with an error saying it requires rouchdb >= 0.5) instead of misreading it (a 0.4 build writing to a 0.5 file would silently replace document histories). **The first `compact()` after the upgrade permanently deletes old revision bodies** and attachment bytes only old revisions reference (0.4 never compacted); keep the backup until you have checked the upgrade report. See [Upgrading redb files](#upgrading-redb-files).

CouchDB-fidelity release. Two audits (a bug audit of every crate and a test-quality audit with mutation testing) drove fixes that were each checked against **CouchDB 3.5.1** as the reference. Many of them change observable behavior: single-document writes return errors, several option and result types changed shape, the server is locked down by default, Mango and views follow CouchDB's semantics, and the redb file format changed. **Read the [migration guide](docs/book/src/upgrading/0.4-to-0.5.md) before upgrading.**

Thanks to **@stn (Akira Ishino)** for the first external contribution (#7).

### Breaking changes

#### API

- **`Database::put`, `update`, `remove`, `post` and `put_design` return `Err` on failure** (`Err(RouchError::Conflict)`, `NotFound`, `BadRequest`, …) instead of `Ok(DocResult { ok: false, .. })`, as `docs/book/src/reference/error-handling.md` always described. `bulk_docs` still returns one result per document. (#12, #18)
- **`update` with a `_rev` on a document that does not exist is `Err(Conflict)`** (was a `not_found` failure), on every adapter. **`remove` of a missing or already-deleted document is `Err(NotFound)`** (CouchDB `DELETE`). (#18)
- **`get` with a malformed `rev` is `Err(InvalidRev)`** on every adapter (was `NotFound` locally and `BadRequest` over HTTP). Any CouchDB 400 "Invalid rev format" maps to `InvalidRev`. (#18)
- **`ViewQueryOptions::default()` equals `ViewQueryOptions::new()`** (CouchDB's defaults: `reduce: true`, `inclusive_end: true`). It used to leave both off, so `..Default::default()` literals silently returned map rows and excluded `end_key`; set `reduce: false` / `inclusive_end: false` explicitly to keep that behavior.
- **`BulkDocsOptions::default()` and `AllDocsOptions::default()` equal `new()`.** `BulkDocsOptions::default()` used to mean replication mode (`new_edits: false`) and `AllDocsOptions::default()` an exclusive `end_key`. Ask for replication explicitly with `BulkDocsOptions::replication()`. (#21)
- **`AllDocsRow` has optional `id` / `value` and a new `error`**, and `all_docs` with `keys` returns one row per requested key, in request order, on every adapter and in the server: unknown ids give a `{"key": …, "error": "not_found"}` row (they used to be skipped locally and dropped over HTTP) and deleted documents a row with `value.deleted: true` (and `"doc": null` with `include_docs`). `skip`/`limit` count error rows. Read the id with `row.key` (or `row.id.as_deref()`) and the revision with `row.rev()`. (#21)
- **`DesignDocument` is lossless**: any design document CouchDB accepts reads and writes back unchanged. New members `other_views` (`lib`, Mango views) and `extra` (`options`, `autoupdate`, `rewrites`, custom fields), `ViewDef::extra`; `filters`, `shows`, `lists` and `updates` hold `serde_json::Value`s. Struct literals need the new fields or `..Default::default()`. (#21)
- **`put_design` writes exactly the struct it is given** and no longer merges members of the stored revision into it. Start from `get_design` to edit a design document; removing a member from the struct now removes it from the document. (#21)
- **`AttachmentMeta` gains `revpos`, `encoding` and `encoded_length`.** Struct literals need the new fields, `AttachmentMeta::new` or `..Default::default()`. (#21)
- **`ServerConfig` has new fields** `cors_origins`, `admin`, `max_request_size` and `session_timeout`. Struct literals need `..Default::default()`. (#11, #15)
- **The default `Adapter::put_security` returns `Err(BadRequest)`** instead of reporting success and dropping the document. Custom adapters that store security documents must implement it; all bundled adapters do. (#12)
- **`before_write` plugins run on `put_attachment` / `remove_attachment`** (they may accept or reject the revision the attachment write creates, but their modifications are not stored; dropping the document is `Forbidden`). (#22)
- **`ViewQueryOptions::new()` sets `reduce: true`** (CouchDB's default). Passing a reduce function now reduces unless you set `reduce: false`. (#14)
- **`SortField` deserialization is strict**: only a field name or a single `{"field": "asc" | "desc"}` is accepted (`{}`, multi-key maps and other directions are rejected). New `SortField::try_field_and_direction`; `field_and_direction()` no longer panics. (#14)
- **`rouchdb_core::collation::to_indexable_string` has a new output format** whose byte order matches `collate`. Rebuild anything you persisted with it. (#14)
- **`rouchdb_core::merge::latest_rev` is removed** (it was unused). Use `merge::latest_leaf`. (#20)
- **`ChangeEvent` serializes `deleted` only when it is `true`** (CouchDB omits it). (#15)
- **`rouchdb_changes::ChangeNotification` is an alias of `rouchdb_core::adapter::ChangeNotice`** (same fields). (#21)
- **Types that may grow are `#[non_exhaustive]`**, so that later releases can add fields and variants without breaking your code (see the book's [API Stability](docs/book/src/reference/api-stability.md) page). A `match` on `RouchError`, `ChangesEvent`, `ReplicationEvent`, `ReplicationFilter`, `ReduceFn`, `StaleOption` or `SortField` needs a catch-all arm, and `ChangesEvent::Complete { last_seq, .. }` / `ReplicationEvent::Change { docs_read, .. }` need `..`. Result types (`DocResult`, `DbInfo`, `AllDocsResponse`, `AllDocsRow`, `AllDocsRowValue`, `ChangesResponse`, `ChangeEvent`, `ChangeRev`, `RevsDiffResponse`, `RevsDiffResult`, `BulkGetResponse`, `BulkGetResult`, `BulkGetDoc`, `BulkGetError`, `PurgeResponse`, `ChangeNotice`, `FindResponse`, `IndexInfo`, `IndexFields`, `CreateIndexResponse`, `ExplainResponse`, `ExplainIndex`, `BuiltIndex`, `EmittedRow`, `PutResponse`, `RevInfo`, `DocMetadata`, `ViewResult`, `ViewRow`, `ReplicationResult`, `PersistentViewIndex`, `Session`, `UserContext`, `PlannedWrite`, `LeafInfo`) can no longer be built with struct literals outside their crate: custom adapters, plugins and tests use the new constructors (`DocResult::ok` / `DocResult::error`, `DbInfo::new`, `ChangesResponse::new`, `ChangeEvent::new`, ...). Option and data structs stay open to `..Default::default()`, which literals should now always use; `Document::new`, `BulkGetItem::new` / `with_rev` and `Default` for `Document`, `IndexDefinition` and `BulkGetItem` are new, and `SortDirection` is re-exported by `rouchdb`.
- **0.5 is the last release in which adding a field or a variant is a breaking change.** The next goal is stabilizing the API and the on-disk format toward 1.0.

#### Documents and storage

- **redb files written by 0.4 must be upgraded explicitly** (`rouchdb migrate <path>`, `RedbAdapter::upgrade`, or `open_with` with an `UpgradePolicy`); `open` refuses them with the new `RouchError::UpgradeRequired` without modifying them, and **0.4 cannot open upgraded files or files created by 0.5**. `RouchError::UpgradeRequired { path }` is a new variant (`RouchError` is `#[non_exhaustive]`). See [Upgrading redb files](#upgrading-redb-files). (#12, #18)
- **Writes are validated like CouchDB** (`new_edits` mode): unknown `_`-prefixed members, non-object bodies, reserved `_` ids and wrongly typed `_id`, `_rev`, `_deleted` or `_attachments` are `BadRequest`. Read-only metadata (`_conflicts`, `_revs_info`, `_revisions`) is dropped, so writing back a `get()` result is safe. Local adapters report an invalid document's bare reason in `bulk_docs` results (no `bad request: ` prefix), like the HTTP adapter and CouchDB. (#12, #22)
- **Generated ids** (`Database::post`, local `bulk_docs` without `_id`, server `POST /{db}`) are 32 lower-case hex digits (still UUID v4) instead of hyphenated UUIDs; over HTTP a document without an id is sent without `_id`, so CouchDB generates it. (#22)
- **`_local/` ids are local documents** on every adapter: `put`, `update`, `remove`, `bulk_docs` and `get` with a `_local/…` id use `0-N` revisions without MVCC, the documents are not listed by `all_docs`, not in the changes feed and not replicated. Replicated `_local/` documents are ignored, as in CouchDB. (#18)
- **Documents may be nested up to `MAX_NESTING_DEPTH` (1000) levels** (re-exported by `rouchdb`) on every input path: the adapters, the server and the CLI. Deeper writes are `BadRequest`; `HttpAdapter::bulk_docs` rejects a deeper document on its own instead of sending it. Documents up to that depth, which broke every read path above serde_json's 128 levels, are now readable on all adapters and replicate. (#18, #22)
- **Revision stemming follows CouchDB.** `stem` cuts every root-to-leaf path (it used to stop at the first branch point); stemmed revisions are unreadable (`NotFound`) and `revs_diff` reports them missing. The limit is configurable with `MemoryAdapter::with_rev_limit` / `RedbAdapter::with_rev_limit` (`DEFAULT_REV_LIMIT` = 1000, `0` = unlimited). (#12, #18)
- **Deleting a document drops its attachments**, and an explicit `_attachments` map is exact: stubs keep an attachment, omitted entries are removed (an *empty* map still inherits from the parent). (#12)
- **Attachment stubs are matched by name** and the stored metadata wins; `{"stub": true}` without a digest is accepted; a stub whose name has no stored attachment (including a "renamed" stub) is `missing_stub`. Stubs (`Document::to_json`, `get`, `bulk_get`) now include `revpos`, which follows CouchDB's rules through writes and replication both ways; attachments stored by 0.4 read back with `revpos` 0. (#12, #18, #21)
- **Re-creating a deleted document extends its tombstone** (`3-…` after `1-…`/`2-…` deleted), as CouchDB does. Re-sending an old edit of a deleted document is a `Conflict` (it used to return the existing revision and rewrite it). (#7, #12, #18)
- **Revision ids are normalized to lower-case hex**; ids containing NUL are rejected; malformed replicated `_revisions` are `doc_validation` errors per document. (#18)
- **`get` of an unknown, compacted or body-less revision is `NotFound("missing")`**; local adapters reject `open_revs` with `BadRequest` instead of silently returning the winner. (#12)
- **Purge follows CouchDB**: only leaves are purged, older revisions are never resurrected, `purge_seq` is tracked, and ids with nothing purged are reported with `[]`. (#12)
- **`destroy` leaves a usable, empty database** on every adapter (documents, local documents/checkpoints, attachments, security and Mango indexes are gone). The HTTP adapter re-creates the remote database on its next use, and destroying an already-deleted remote database is `Ok`. (#14, #18)
- **`changes` with `limit: Some(0)` returns no change** (was one). (#18)
- **`revs_diff`** lists `missing` sorted by generation then id, and `possible_ancestors` once each, in winner order. (#18)

#### Mango (`find`, selector `changes`, replication selector filters)

- **CouchDB semantics for missing fields**: a document without the field only matches `{"$exists": false}`; `$ne`, `$nin`, `$not` and `$nor` no longer match it (PouchDB differs here). (#14)
- **Nested object selectors are paths** (`{"a": {"b": 1}}` is `{"a.b": 1}`), `$and`/`$or`/`$nor`/`$not` inside a field apply to that field, `$in`/`$nin` look inside arrays, and `items.0.name` / `a\.b` paths work. (#14)
- **`find` never returns design documents.** (#14)
- **`fields` returns only the requested fields** (no implicit `_id`), and nested paths keep their structure (`address.city` → `{"address": {"city": …}}`). (#14)
- **Invalid selectors, operators, regexes and sort fields are `Err(BadRequest)`** with CouchDB's reasons (`Invalid operator: $foo`, `Bad argument for operator $not: 5`, …) instead of an empty result. A `null` or missing selector is an error too, and so is `$elemMatch` with a non-object argument. (#14, #15, #22)
- **Empty selectors and combinators**: a selector that is only `{"$and": []}` / `{"$or": []}` / `{"$nor": []}` returns nothing; nested empty combinators are true; `{}` below the top level is an equality test with `{}` (`{"$and": [{}]}` matches nothing); operators without a field inside `$and`/`$or`/`$nor` apply to the whole document. `$all` uses exact equality, `$not` needs an object, and a sorted `find` skips documents that lack a sort field. (#15, #22)
- **Descending sort ties come in reverse index order**, as in CouchDB. (#22)
- **`$regex` is PCRE-like** (lookaround and backreferences via `fancy-regex`); a pattern that exhausts the backtracking limit does not match. (#15)

#### Views and collation

- **Reduce**: `_sum` and `_stats` keep integers (`90`, not `90.0`) and sum arrays element-wise and objects per field. `_sum` over values it cannot add returns CouchDB's `{"error": "builtin_reduce_error", …, "caused_by": <value>}` object as the row's value (a 200 over the server); `_stats` over other values is `Err(BadRequest)`. Custom reduce functions receive `[key, doc_id]` keys, and all values of a group are reduced in one call (`rereduce` is always `false`). (#14, #22)
- **Query options**: several `keys` with a reduce need `group: true` without `group_level`; a single key behaves like `key`; `include_docs` with a reduce is an error; `include_docs` now fills `ViewRow.doc` (including linked `{"_id": …}` docs). A key range that cannot match (`start_key` after `end_key` without `descending`), several `keys` together with `key`/`start_key`/`end_key`, and `group`/`group_level` without a reduce are `BadRequest`; `start_key`/`end_key` are honored next to `key`; `group` + `keys` keeps the keys' order when descending. (#14, #15, #22)
- **Results**: `total_rows` counts the whole view and `offset` is the position of the first returned row; design documents are not mapped. (#14)
- **Collation**: `-0.0 == 0`, integers and floats compare exactly (no precision loss above 2⁵³), and strings compare by UTF-16 code units, exactly like PouchDB (this only differs from 0.4 for characters above U+FFFF against U+E000..U+FFFF). CouchDB's ICU order and object key order are not emulated (see "Differences from CouchDB" in the book). (#14, #21)

#### Replication and the HTTP adapter

- **`Database::http` / `HttpAdapter::new` create a missing remote database on first use** (PouchDB's default). Set `HttpAdapterOptions::skip_setup` to keep failing with `NotFound`. (#13)
- **Replications with an HTTP peer get a new replication id** (it now includes the server uuid), so the first run after upgrading re-reads the changes feed once. `revs_diff` means no document is transferred again; the old `_local` checkpoints are left unused. (#13)
- **Replications with a memory or redb database also get a new replication id, once**: a local database is identified by a uuid of its own (stored in the file for redb, per instance for memory, renewed by `destroy()`) instead of its name, so the first run after upgrading re-reads the changes feed of each local replication (including local↔local and `sync`). `revs_diff` means no document is transferred again, and a redb file keeps its identity (and so its checkpoints) when reopened, whatever name it is opened with. (#29)
- **A replication selector is validated before the replication starts**: an invalid one is `Err(BadRequest)` from `replicate`, and an `Error` event that ends `replicate_live` (even with `retry`), before any checkpoint is read or written. (#29)
- **Denied documents** (`forbidden` / `unauthorized` from a validation function) are reported in `errors` (so `ok` is `false`) and the checkpoint moves past them instead of stalling forever. (#13)
- **`ReplicationFilter::Custom` closures no longer use checkpoints**: each run rescans from `since` (documents already on the target are not re-sent). (#13)
- **`before_write` plugins act as `validate_doc_update` for replicated documents**: they run per document, their modifications are ignored, and a rejection is reported as denied without blocking the batch. `after_write` also sees attachment writes and documents replicated to CouchDB. (#13)
- **Live feeds and live replication from memory and redb react to writes immediately** instead of polling (`poll_interval` then only applies to HTTP sources and retry delays), and live replication from them emits `Paused` as soon as it is caught up. (#21)
- **Mango on `Database::http` runs on the server**: `find`, `create_index`, `get_indexes`, `delete_index` and `explain` use `_find`/`_index`/`_explain`, so results follow CouchDB and indexes are real CouchDB indexes. (#14)
- **HTTP errors are mapped from the status and `{error, reason}`**: 400/413/415 → `BadRequest`, 412 `file_exists` → `DatabaseExists`, 401 → `Unauthorized`. (#13)
- **HTTP requests time out**: 30 s to connect and 60 s of inactivity by default (`HttpAdapterOptions`). (#13)

#### Server (`rouchdb-server`)

- **CORS is off by default** and **optional admin authentication** is available; see [Security](#security). (#11)
- **`DELETE /{db}` really deletes the database**: every database route answers 404 until `PUT /{db}` re-creates it. (#15)
- **Status codes and error bodies follow CouchDB**: errors are always JSON with CouchDB's names and reasons (`Document update conflict.`, `Database does not exist.`, `missing` / `deleted`, `Invalid rev format`, `illegal_docid`, `doc_validation`, `query_parse_error`, Mango error names); a wrong-shape JSON body is 400 (was 422) and malformed JSON is CouchDB's "invalid UTF-8 JSON"; `DELETE` without a rev and `PUT` with a `_rev` the document does not have are 409; design-document conflicts are 409 (were 201 with `"ok": false`); 405 carries a sorted `Allow`. (#11, #15, #22)
- **Anonymous `POST`/`PUT`/`DELETE` on `/` and `/_active_tasks` are 405** (`Only GET,HEAD allowed`); non-GET requests on `/_utils` need credentials. (#22)
- **`POST /{db}/_all_docs` with `keys`** answers exactly like CouchDB: one row per key including `not_found` rows (also for non-string keys, which used to be dropped), `"offset": null`, and `"doc": null` for deleted documents. (#21)
- **`GET /{db}/_design/{ddoc}` serves the stored document as is**, with the same options and headers as any document (`rev`, `revs`, `conflicts`, `attachments`, `ETag`/`If-None-Match`). (#21)
- **`_bulk_docs` rejects the whole request** on an invalid document (nothing is written); with `new_edits: false` it returns only failures (`[]` normally). (#15)
- **`_find` returns at most 25 documents unless `limit` is set**, and bookmarks are real (an invalid one is 400 `invalid_bookmark`). (#11)
- **`POST /{db}/_index` writes a `_design/<ddoc>` document** (`language: "query"`, as CouchDB): it appears in `_all_docs` and `_changes`, replicates, and the response `id` is the design-document id (was the index name). (#11)
- **`_all_docs` query keys must be JSON-encoded** (`?key="b"`; `?key=b` is 400). (#11)
- **`_changes` rejects unknown feeds and unsupported filters** (400/404) instead of ignoring them. (#11)
- **`POST /{db}/_compact` requires `Content-Type: application/json`** (415 otherwise). (#15)
- **The request body limit is 64 MiB** (was axum's implicit 2 MB), with a JSON 413. (#11)
- **`/_uuids` and `POST /{db}` generate 32 lower-case hex digits** (were hyphenated). (#15, #22)

#### CLI (`rouchdb-cli`)

- Read-only commands (`info`, `get`, `all-docs`, `find`, `changes`, `dump`, `compact`, `delete`, a redb `replicate` source) fail with exit 1 on a missing file instead of creating it. (#11)
- `replicate` exits 1 when `ok` is `false`, and its output gains `errors` (passwords masked) and `last_seq`. (#11)
- `dump` inlines attachments as base64 and `import` restores them. `dump` still exports winning revisions only and names conflicted documents on stderr; use `replicate` to copy conflicts and history. (#11)

#### Build

- **HTTPS uses rustls by default instead of native-tls/OpenSSL.** `rouchdb` and `rouchdb-adapter-http` expose the TLS backend as features: `rustls-tls` (default, bundled Mozilla roots), `rustls-tls-native-roots` (rustls with the OS certificate store) and `native-tls` (the 0.4 behavior: OpenSSL on Linux). Building no longer needs OpenSSL headers on Linux. If your CouchDB certificate is signed by a private CA in the OS trust store, enable `rustls-tls-native-roots` or `native-tls`:
  ```toml
  rouchdb = { version = "0.5", default-features = false, features = ["native-tls"] }
  ```
  (#10)
- **The minimum supported Rust version is declared: 1.88** (`rust-version`) and checked in CI. (#10)

### Added

- **The server is a replication peer**: `POST _revs_diff`, `POST _bulk_get` (with `revs` and `attachments`), `GET/PUT/DELETE _local/{id}`, `POST _purge` and `?open_revs=all|[…]` (JSON form). Replication to and from a memory- or redb-backed server works both ways and resumes from checkpoints. (#11)
- **Server `_changes` feeds**: `feed=longpoll|continuous|live|eventsource`, `heartbeat`, inactivity `timeout`, `pending`, and the `_doc_ids`, `_selector` and `_design` filters. (#11, #15)
- **Server HTTP semantics**: `ETag` / `If-None-Match` (304) / `If-Match`, `Location` headers, attachment `ETag` / `Accept-Ranges: none` / `Content-Security-Policy: sandbox`, attachment names containing `/`, and attachment GET honoring `?rev`. Mango indexes persist as design documents and are restored at startup (`restore_indexes`). (#11, #15)
- **Server options**: `--cors-origin` / `ROUCHDB_CORS_ORIGINS`, `--admin` / `ROUCHDB_ADMIN`, `--max-request-size`, `--session-timeout`; `ServerConfig::{cors_origins, admin, max_request_size, session_timeout}`, `AdminCredentials`, `parse_cors_origin`, `DEFAULT_MAX_REQUEST_SIZE`, `DEFAULT_SESSION_TIMEOUT`, `Auth::with_timeout`. (#11, #15)
- **CLI**: `ROUCHDB_USER` / `ROUCHDB_PASSWORD` supply credentials for http(s) URLs without userinfo. (#11)
- **HTTP adapter**: `HttpAdapterOptions` (`skip_setup`, `connect_timeout`, `read_timeout`), `HttpAdapter::with_options`, `DEFAULT_CONNECT_TIMEOUT`, `DEFAULT_READ_TIMEOUT`, `HttpAdapter::request_json`. (#13, #14)
- **`Adapter::id()`** (default method: the database name; the HTTP adapter returns the server uuid plus the database name, the memory and redb adapters a uuid of their own that `destroy()` renews), used to derive replication ids. Custom adapters should override it: see its documentation. (#13, #29)
- **`Adapter::subscribe()`** (default method returning `None`, so custom adapters compile unchanged and keep being polled): the memory and redb adapters announce committed changes (`ChangeNotice { seq, doc_id }`) to live changes feeds and live replication, and `destroy()` with a `ChangeNotice::reset()` (`reset: true`). (#21, #30)
- **Replication warnings**: `ReplicationResult::warnings` and `ReplicationEvent::Warning`, reported when both peers have the same `Adapter::id()` (the replication then runs without checkpoints). (#30)
- **Opt-in `arbitrary-precision` feature** on `rouchdb` for exact JSON numbers (enables serde_json's `arbitrary_precision` for the whole build; revision ids of documents with such numbers differ from those computed without it). (#21)
- **Live changes**: `ChangesEvent::Paused` / `Active` / `Error` are now emitted (catch-up, resume, failed fetch with retry and backoff), heartbeats and the inactivity `timeout` work, and `LiveChangesStream::next_event()` / `last_seq()` are new. (#13)
- **Views**: `ViewEngine::query` with `StaleOption` (`False`, `Ok`, `UpdateAfter`); `ViewEngine` rebuilds after purges or a recreated database, and `register_map` invalidates an existing view. (#14)
- **Query API**: `CompiledSelector`, `find_in_docs`, `BuiltIndex::apply_changes`, `query_emitted`, `query_sorted`, `sort_emitted`, `attach_docs`. (#14)
- **Core API**: the `rouchdb_core::write` module (storage-independent edit rules shared by the memory and redb adapters), `Document::prepare_for_write`, `rev_tree::path_from_revisions`, `rouchdb_core::json` (`MAX_NESTING_DEPTH`, `from_slice`, `from_input`, `value_depth`, `text_depth`, `check_document_depth`), `merge::{merge_and_stem, stem_revs, revs_diff_one}`, `write::{local_doc_id, plan_local_write, local_document, LocalWrite}`, `Revision::normalized`, `document::normalize_rev_hash`, `PlannedWrite::stemmed`. (#12, #18, #22)
- **Type helpers**: `AllDocsRow::{rev, is_deleted, is_error}` and the `AllDocsRow::{document, not_found}` constructors; `DesignDocument::new`, `with_view`, `with_filter`, `ViewDef::new`, `with_reduce` and `Default` for both; `AttachmentMeta::new` and `to_json`. (#21)
- **redb**: purge, persistent security documents, and `with_rev_limit`. (#12, #18)
- **redb file upgrades**: `RedbAdapter::open_with` / `Database::open_with` with `OpenOptions` and `UpgradePolicy` (`Refuse` (default), `WithBackup(Option<PathBuf>)`, `InPlaceNoBackup`), `RedbAdapter::upgrade`, `RedbAdapter::inspect_upgrade` (dry run), `RedbAdapter::upgrade_report`, `UpgradeReport` and `DiscardedRevision`; `rouchdb migrate <path> [--backup <path>] [--no-backup] [--dry-run]` in the CLI and `rouchdb-server --upgrade`. See [Upgrading redb files](#upgrading-redb-files).
- **Docs**: a "Differences from CouchDB" book page (object key order, local `all_docs` `offset`, numbers, attachment digests) and a [0.4 → 0.5 migration guide](docs/book/src/upgrading/0.4-to-0.5.md). (#21)

### Fixed

**Storage (core, memory, redb)**
- Re-creating a deleted document with the same body as its first revision was a silent no-op: it returned `ok: true` but the document stayed deleted. Thanks @stn. (#7)
- redb stored the revision tree as nested records that could not be read back for long histories; it is now a flat list, deep legacy records load, and decode errors are reported instead of the document looking missing. (#12)
- redb dropped attachments on body-only updates and ignored inline attachments from `bulk_docs` and replication; `bulk_get` now returns attachment data. (#12)
- redb compaction and purge of `a` deleted the bodies of `a\0b`. (#18)
- Merging a replicated revision could overwrite an existing revision in place, drop ancestors of a path that starts earlier, or skip overlapping roots. (#12)
- Merging a path that bridges two roots of a stemmed (multi-root) tree kept both copies of the joined branch, so revisions (and the winner) were listed twice and the tree depended on merge order; this could happen when replicating from sources with different `revs_limit`s. Roots are now sorted by `(pos, hash)`, as in CouchDB. (#25)
- Edits could not extend a losing branch, so conflicts could not be resolved by updating or deleting the loser. (#12)
- The changes feed and `include_docs` reported whether the *edit* was a deletion instead of whether the *winner* is deleted, which also confused `ViewEngine` and live changes. (#12)
- Stemmed revisions stayed readable; memory `revs_diff` returned duplicate `possible_ancestors`; non-string `_revisions.ids` were dropped and invented revisions. (#18)
- The revision hash now covers the final attachment set (hashes of documents without attachments are unchanged). (#12)
- redb ignored `latest`, `attachments`, `revs_info` and `conflicts` in `get`; `revs_info` listed other branches with wrong statuses; `revs` (`_revisions`) is implemented on both local adapters. (#12)
- redb `all_docs` with `keys` lost request order and duplicates, and `descending` did not reverse the keys. (#12)
- redb `compact` kept every revision body; it now drops non-leaf bodies and unreferenced attachment bytes. The memory adapter stored attachment bytes before the write was accepted and never freed them. (#12)
- `remove_attachment` of a missing attachment wrote a new revision; it is now `NotFound`. (#12)
- redb work blocked the async runtime; it now runs on `spawn_blocking`. (#12)
- `Document::from_json` rejected the `{content_type, data}` inline attachment form clients send and CouchDB's inline form without `length`. (#12)

**Replication, changes and HTTP**
- `docs_written` was always 0 when replicating to CouchDB, and an indexed `find` on a remote database failed to decode `_all_docs` (`offset: null`). (#9)
- Losing branches did not replicate (the changes feed was read without `style=all_docs`), and attachment bytes pulled from CouchDB were missing. (#13)
- Attachments pushed to CouchDB arrived with `revpos: 0`. (#21)
- `replicate_to_with_events` deadlocked past 64 events. (#13)
- Checkpoints could skip documents after a `_bulk_get` item error, stall forever on a denied document, fail on a read-only source, or not advance past filtered or already-synced batches; checkpoint write errors are now reported. (#13)
- Live replication ignored `since` and rescanned on every poll; selector filters did not see `_id`, `_rev` and `_deleted`. (#13)
- Two local databases with the same name shared their replication checkpoints, in both directions: `sync()` between them pushed, then skipped the other side's documents on the pull and reported success. Local databases are now identified by their own uuid (see *Replication and the HTTP adapter* above). (#29)
- A live replication whose source (or target) was destroyed and reused kept its cursor: the new database's changes, numbered from 1 again, were skipped while the replication reported `Paused`. Live replication now checks the identity of both databases on every pass and, when one changed, starts over from the checkpoint of the new pair (none for a destroyed database, so from the start). (#29)
- A redb file and a copy of it shared their identity (the uuid stored in the file), so `sync()` between them used one checkpoint for both directions and skipped the other side's documents while reporting success, even when opened under different names. The redb identity now combines that uuid with the file's canonical path: reopening a file at the same path (or through a symlink) resumes its checkpoints, while a copied or moved file is replicated with one full rescan (no document is transferred again). Independently, replication no longer reads or writes checkpoints between two peers that report the same `Adapter::id()` (a custom adapter that keeps the default id, for instance): it scans the whole feed and reports a warning. (#30)
- A live replication did not notice its target being destroyed and reused while the source was idle: it waited for the source's next change, so the target stayed without the source's documents. `destroy()` on the memory and redb adapters now announces a reset through `Adapter::subscribe`; live replication listens for resets from both peers, compares the id of a target that cannot announce them every `poll_interval`, and then copies everything again to the new target. (#30)
- `ReplicationFilter::DocIds` selections that only differed in how their ids joined with NUL (`["a", "b"]` and `["a\0b"]`) shared a checkpoint, so the second silently skipped its documents. The ids are now fingerprinted as a JSON array; checkpoints written with the old encoding are not reused (one rescan of `DocIds` replications). (#29)
- An invalid replication selector (`{"x": {"$typo": 1}}`) matched nothing and the replication reported success while copying no document, and could advance its checkpoint past them. It is now a `BadRequest` before anything is read or written; the compiled selector is reused for every document. (#29)
- The live changes feed dropped polls when the heartbeat was shorter than the poll interval, reported a wrong `last_seq`, ignored `limit` with a filter, and kept running after the receiver was dropped. (#13)
- `AuthClient::login` failed against CouchDB (`POST /_session` has no `userCtx`); `sign_up` did not percent-encode the user id. (#13)
- `changes(conflicts)` and `all_docs(update_seq)` were ignored over HTTP. (#13)
- The CLI's `replicate` to CouchDB failed because the target database was not created. (#13)

**Query and views**
- `$mod` could panic (`i64::MIN % -1`) and accepted a zero or non-integer divisor; `SortField::field_and_direction` could panic; `put_design` panicked when a plugin dropped the document. (#14)
- Mango indexes were rebuilt under a write lock on every `find`; they are now updated incrementally from the changes feed, and an indexed query narrows candidates by binary search. (#14)
- `Partition::all_docs` returned documents outside the partition with `descending` or out-of-range keys, missed ids after `p:\u{10FFFF}`, and applied `inclusive_end` to the partition's own bound; it now lists exactly the partition and applies `inclusive_end` only to the caller's end key. (#14, #22)
- `changes` with a selector counted `limit` before filtering. (#14)
- `put_design` / `get_design` dropped design-document members `DesignDocument` did not model (`views.lib`, Mango index views, `options`, custom fields); the struct is now lossless. (#14, #21)

**Server and CLI**
- Creating a document through an attachment `PUT` produced a `2-` revision (CouchDB: `1-`). (#15)
- `PUT`/`DELETE /{db}/{doc}/{att}` with a malformed `?rev` or `If-Match` answered 404/409 when the document was missing; it is now 400 `Invalid rev format` before the lookup, as in CouchDB. (#26)
- `_changes.pending` was always 0, and 404 reasons were the document id instead of `missing` / `deleted`. (#15)
- Axum rejections (bad JSON, bad query parameters, missing `Content-Type`, 404/405) returned non-JSON bodies. (#11)
- The server and the CLI rejected JSON bodies, selectors and `import` files nested deeper than serde_json's 128 levels. (#22)
- `import` of 20 000 documents took 97 s; it now writes in batches of 500 and takes 0.7 s (release build). (#11)
- A broken pipe (`rouchdb … | head`) panicked with exit 101. (#11)

### Security

- **Server CORS is disabled by default.** Browser apps on another origin must be allowed with `--cors-origin` / `ROUCHDB_CORS_ORIGINS` (repeatable). Credentials are allowed only for listed origins; `*` allows any origin without credentials. (#11)
- **Optional admin authentication**: with `--admin user:password` / `ROUCHDB_ADMIN`, every endpoint except `/`, `/_session`, `/_uuids` and `/_utils` requires HTTP Basic auth or a `_session` cookie (random token, `HttpOnly`, `SameSite=Strict`). The server warns at startup when it listens on a non-loopback address without authentication. (#11)
- **Wrong Basic credentials are rejected with 401 on every route**, including `/`, `/_uuids`, `/_session` and `/_utils` (they used to be accepted on public routes). (#15)
- **Session cookies follow CouchDB**: only the first `AuthSession` cookie counts, a malformed one is a 400 on every route (even with valid Basic credentials), and tokens have CouchDB's shape. (#22)
- **Session cookies expire after 10 minutes without use** (was 1 hour), carry `Max-Age`, and are refreshed on cookie-authenticated requests. (#15)
- **Request bodies are limited** (64 MiB by default, `--max-request-size`). (#11)
- **The CLI redacts `user:password@`** from every error it prints. (#11)
- Not covered yet: per-database `_security` members/admins are not enforced, and there is no Host-header (DNS rebinding) check, so run the server with `--admin` whenever other local software could reach it.

### Changed

- redb keeps document counts in its metadata, answers `all_docs` with range scans and early `skip`/`limit`, and iterates `changes` lazily. (#12)
- `rouchdb-changes` and `rouchdb-views` now depend on `rouchdb-query`, so `rouchdb-query` is published before them. (#13, #14)
- `rouchdb-query` uses `fancy-regex` instead of `regex`. (#15)
- `rouchdb-core` owns the `md-5` dependency (moved from the adapters). (#12)
- async-trait 0.1.92 (fixes `clippy::double_must_use` on Rust 1.99). (#19)

### CI and tooling

- The toolchain is pinned in `rust-toolchain.toml`; CI checks the MSRV, the TLS feature combinations and minimal dependency versions, and runs non-blocking clippy on stable and beta. (#10, #16)
- The CouchDB integration suite (`#[ignore = "requires CouchDB"]`) runs in CI against `couchdb:3.5.1`, in parallel, with RAII-cleaned `rouchdb_test_*` databases and a leftover-database check; blocked tests use `#[ignore = "blocked on …"]` plus a `blocked_on_` name and run as a non-blocking xfail step; a lint rejects bare `#[ignore]`. The two tests blocked at the start of the cycle are fixed and unblocked. (#10, #16, #22)
- New `rouchdb-bench` crate with criterion benchmarks, smoke-run in CI; a manual `Benchmarks` workflow runs them. (#10, #16)
- A cross-adapter conformance suite (`crates/rouchdb/tests/adapter_conformance.rs`) runs every storage scenario against memory and redb (with reopen). (#12, #18, #20, #21)
- Differential tests against CouchDB 3.5.1 for the server, Mango and views (the CouchDB parity tables re-check every divergence fix), and exact-oracle rewrites of the replication, HTTP, server, CLI, core, facade and query tests, measured with cargo-mutants. (#15, #17, #20, #22)
- Property-based tests (proptest) for the revision tree: merge order independence, idempotence, winner/conflicts against a brute-force oracle, stemming, `merge_and_stem`, `remove_leaves`, and the revision/document codecs. (#25)
- Tests that kill residual mutants in security documents, replicated writes, server write results, rev-format checks and document read options. (#26)
- CI runs the test suite with the `arbitrary-precision` feature. (#21)
- Coverage (cargo-llvm-cov), `cargo mutants --in-diff` on PRs and a nightly flaky-test workflow (non-blocking). The README examples compile as doctests. (#16)
- The "Mutants in the diff" job runs the whole suite per mutant against a CouchDB 3.5.1 service (so mutants caught by the CouchDB-backed tests are no longer reported as missed), reports survivors as notices with a job summary instead of failing, and stops within a 75-minute budget with partial results. (#24)
- CI fails if README/book install snippets drift from the workspace version. (#10)

### Upgrading redb files

0.4 and 0.5 cannot share a file: 0.4 does not understand the flat document records, digest-keyed attachments and local-document layout 0.5 writes, and before this release it would treat such documents as missing and **replace their whole history on its next write**. So:

- **0.5 refuses 0.4 files unless told to upgrade them.** `RedbAdapter::open` / `Database::open` return `RouchError::UpgradeRequired` for a file written by rouchdb ≤ 0.4, and the file is not modified. This applies to every open, including `rouchdb info` and `rouchdb-server` (which print how to upgrade).
- **Upgrade once, with a backup:** `rouchdb migrate app.redb` (or `RedbAdapter::upgrade(path, UpgradePolicy::WithBackup(None))`, or `open_with` with that policy). The backup is written first to `app.redb.rouchdb-0.4.bak` (or `--backup <path>`): every table is copied from one read transaction, committed, reopened and compared entry by entry with the original, synced, then renamed into place; it opens in 0.4 exactly like the original. The upgrade is refused if the backup path already exists; a backup found there after an interrupted attempt is complete (a backup gets its final name only once verified), and the error says to move it elsewhere to keep it. Any failure while backing up removes the partial copy; a `<backup>.partial` left by a killed process is not a backup, and the error says it can be deleted (the file itself was not changed). `--no-backup` / `UpgradePolicy::InPlaceNoBackup` skip the backup.
- **`--dry-run` / `RedbAdapter::inspect_upgrade` only read the file** (read transactions): not a byte of it changes and it uses no disk space. They run the upgrade's analysis: they validate every record and compute the plan, so they refuse a file the upgrade would refuse for its content, with the same error, and report the same counts. They do not exercise the rest of the upgrade: the backup (its path, its creation and verification), the free disk space, and the write and commit of the upgrade are not tried, so the upgrade can still fail there.
- **Disk space and memory:** the upgrade needs about twice the file size of free disk space (redb copies every page it changes and commits in two phases), and the backup about the file size again: **about three times the file size free when backing up**. No free-space check is made beforehand: running out of space fails the backup or the upgrade, which then leaves the file as it was. Memory stays small whatever the file size (records and attachments are handled one at a time with a 32 MiB page cache): a 2.2 GB file holding 1 GB of attachments upgrades with a backup in about 110 MB of memory.
- **The upgrade is one atomic transaction** (two-phase commit): the file is upgraded completely or not at all. An error **before the commit** (while backing up, or while applying the plan) leaves the file exactly as it was and removes the backup made for that attempt, which is then redundant. If **the commit itself** fails, the backup is kept, and the error says so: if rouchdb still reports that the file must be upgraded, the file was not changed (move the backup elsewhere, or choose another backup path, before retrying); otherwise the upgrade was committed and the backup is the copy of the old file. It re-keys attachment bytes by digest, counts the documents, **moves `_local/…` documents that 0.4 stored as ordinary documents** (written through `put`/`bulk_docs`) to the local document store where 0.5 looks for them (latest body kept, revision `0-N` with `N` its generation; deleted ones dropped), **rewrites revision ids stored in upper-case hex** (accepted by 0.4 through `new_edits: false`) in lower case so 0.5 can address them, moves the metadata to a new `rouchdb_meta` table, and installs the format guard. Revision trees stay in the old nested format until each document is next written; both formats are readable.
- **Revision ids that differ only in case.** 0.4 could store the same revision under two spellings (`2-ABC…` replicated first, then `2-abc…`, which 0.4 treated as a different revision). 0.5 treats them as one, so the upgrade merges them into one lower-case revision (keeping the children of both). If both spellings have a stored body and the bodies are identical, one copy is kept. If they differ, **the body kept is that of the spelling 0.4 ranked first**: a leaf before an inner revision, a live leaf before a deleted one, then the greater id in byte order (lower case sorts after upper case), which is how 0.4 picked the winning revision, so 0.4's winner keeps its body. The discarded body is never dropped silently: `UpgradeReport::case_duplicate_bodies_discarded` lists every one (document, discarded spelling, kept spelling), and it remains in the backup.
- **The winning revision can change.** 0.4 compared upper-case ids as written (`B` sorts before `a`); 0.5 compares lower-case ids, so among conflicting revisions a different one can win. The report counts these documents and lists them (`docs_with_changed_winner`).
- **It prints (and `UpgradeReport` holds) what it found**: live/deleted documents, attachment entries re-keyed, `_local/` documents moved, revisions normalized, case-duplicate revisions merged and bodies discarded, documents whose winner changes, attachment references whose bytes 0.4 had already lost (0.4 kept one copy per document and name, so re-attaching a name overwrote the bytes of older revisions), old revision bodies, and documents whose attachment bytes only old revisions reference. Its closing advice depends on what was done (dry run, backup or not, 0.4 or development-build file).
- **A record the upgrade cannot decode stops it** with an error naming the document or revision (nothing is skipped and nothing is changed): repair or remove that record with 0.4, then retry. Likewise a `_local/x` document that collides with a local document `x` written by `put_local`, and a live document with the id `_local/` itself (a local document with an empty name, which 0.5 cannot address). These refusals happen before any backup is written.
- **The format guard: 0.4 refuses 0.5 files.** The `metadata` table of every file written by 0.5 (new, upgraded, or destroyed and reused) has a value type named `rouchdb-format-2 (this file requires rouchdb >= 0.5)`. rouchdb 0.1–0.4 fail in `open` with `metadata is of type Table<&str, rouchdb-format-2 (this file requires rouchdb >= 0.5)>` and write nothing. **Downgrade is not supported**: keep the backup if you may need 0.4.
- **The first `compact()` after upgrading deletes old data.** 0.4's `compact` did nothing; 0.5 keeps only leaf revision bodies and the attachment bytes they reference. The first compaction therefore permanently deletes every old revision body and the attachment bytes only old revisions reference (often attachments 0.4 dropped from a document whose body was updated without them). Check the upgrade report (and keep the backup) before compacting.
- Files written by unreleased 0.5 development builds (metadata `schema` 1 or 2 without the guard, which 0.4 cannot open) are upgraded automatically on open, keeping their security document, **after a verified backup to `<file>.rouchdb-0.5-pre.bak`** (a copy of the development-build file; 0.4 cannot open it). `UpgradePolicy::InPlaceNoBackup` skips that backup. Errors about such files name the development build, not 0.4. Files written by a newer rouchdb are refused with a clear error and left untouched, as are redb files rouchdb did not create.
- Attachments stored by 0.4 read back with `revpos` 0. (#21)
- Bodies of stemmed revisions left by older versions are ignored and removed by the next `compact()`. Bodies deeper than 1000 levels written by older versions (unreadable before) give a clear error. (#18)
- `rouchdb-adapter-redb` now requires redb ≥ 2.3 (two-phase commit).

---

## [0.4.0] — 2026-06-09

Correctness release. A deep audit fixed **44 bugs** across every crate. There are no new features, but several fixes change observable behavior (HTTP status codes, replication checkpoints) or public type shapes — **read the migration guide before upgrading.**

### Migration Guide (0.3.2 → 0.4.0)

#### Breaking Changes — API

1. **`ChangesEvent` has a new `Heartbeat` variant.** An exhaustive `match` without a wildcard arm will no longer compile. Add an arm:
   ```rust
   ChangesEvent::Heartbeat => { /* keep-alive tick; ignore or reset a timeout */ }
   ```
   Heartbeats are emitted by `live_changes_events` only when `ChangesStreamOptions.heartbeat` is set (the option was previously inert).

2. **`DbInfo` has a new field `doc_del_count: u64`.** If you build it with struct-literal syntax, add the field or use `..Default::default()`. It now reports the real deleted-document count (was hardcoded to `0` in the server response).

3. **`SecurityDocument` has a new field `extra: serde_json::Map<String, Value>`** — a `#[serde(flatten)]` catch-all so CouchDB's arbitrary `_security` fields round-trip instead of being dropped. Add `..Default::default()` to struct literals.

4. **`CheckpointDoc` gained a `_rev` field and `Checkpointer::new` takes a third argument** (a filter fingerprint): `Checkpointer::new(source_id, target_id, filter_fingerprint)`. Only affects code that constructs these `rouchdb-replication` types directly.

#### Breaking Changes — Behavior

5. **Replication checkpoints are invalidated once.** The replication ID now incorporates the filter (and uses a revised hashing scheme), so the first replication after upgrading re-reads from sequence 0. This is a one-time, **idempotent** re-scan (no data loss or duplicates, thanks to `revs_diff`); subsequent runs resume normally from the new checkpoint.

6. **HTTP server status codes now match CouchDB:**
   - `PUT /{db}/{docid}` returns **409 Conflict** on a stale/missing `_rev` (was 201 Created).
   - `DELETE /{db}/{docid}` returns **409 / 404** on conflict / not-found (was 200 OK).
   - `PUT /{db}` returns **412 file_exists** when the database already exists (was 201).

   Clients that treated the old (incorrect) codes as success must update their handling.

7. **`PUT /{db}/{docid}` with `{"_deleted": true}` now deletes the document** (previously it silently created a new live revision).

8. **`get(rev = X, latest = true)`** now returns the leaf of *X's own branch*, not the global winning leaf. Only affects conflicted documents.

9. **An empty map/reduce now returns `{"rows": []}`** instead of one `{"key": null, "value": 0}` row (matches CouchDB). Code that indexes `rows[0]` unconditionally must guard for emptiness.

10. **`Revision` parsing rejects an empty hash** (e.g. `"3-"`) with `InvalidRev`; such strings previously parsed successfully.

11. **`put` / `update` / `remove` return `Err(DatabaseError)`** instead of panicking when a `before_write` plugin removes the document from the batch.

### Bug Fixes

**Core (revision tree, collation, model):**
- Stemming stopped at the first conflict branch point, so `revs_limit` was never enforced on conflicted documents (unbounded rev-tree growth). It now prunes past branch points by re-rooting subtrees.
- Collation lost precision on integers above 2⁵³, collapsing distinct values to `Equal` and breaking Mango `$eq`/range queries. Integers are now compared exactly.
- Inline attachment data serialized as a JSON byte array instead of a base64 string (malformed CouchDB output).
- `Revision::from_str` accepted a missing hash (`"{pos}-"`).

**Memory adapter:**
- `put_attachment` reported success but discarded the attachment — bytes were unrecoverable. Attachments are now persisted per revision and round-trip through `get_attachment` and replication.
- `get(latest = true)` returned the global winner instead of the requested branch's leaf.
- `style=all_docs` emitted an empty `changes` array for fully-deleted documents.
- `changes` reported `last_seq = since` when every change was filtered out (endless re-scans).
- `all_docs` `total_rows` reflected the filtered count instead of the database total.

**Redb adapter:**
- `all_docs` descending key-range used ascending comparisons, returning the wrong rows.
- `all_docs` ignored `conflicts`, omitting `_conflicts` from included docs.
- `changes` panicked on a single corrupt/undeserializable record (now propagates an error).
- `info()` / `all_docs` read `update_seq` in a separate transaction from the document snapshot (TOCTOU); now a single read transaction.

**HTTP adapter:**
- `all_docs` ignored `key`, `keys`, and `inclusive_end`.
- `startkey` / `endkey` / `key` / `open_revs` were spliced into the URL unencoded (broke on special characters and unicode).
- Design-document ids were encoded as `_design%2Fx`, breaking routing; the prefix slash is now preserved.

**Query (Mango + map/reduce):**
- `$elemMatch` with an operator expression failed on arrays of scalars.
- `skip` / `limit` were ignored on reduced/grouped output.
- `group_level=0` did not collapse into a single global group.
- Reduce over an empty result set returned a spurious zero row.

**Views:**
- Design documents containing a `views.lib` (CommonJS shared library) entry failed to parse.

**Replication:**
- Replication ID omitted the filter, so different filters shared a checkpoint (silent data loss).
- Checkpoint `_local` doc was written without `_rev`, so every CouchDB checkpoint update after the first failed with 409.
- Checkpoint history was reset to a single entry on every write, breaking cross-session resume.
- `compare_checkpoints` compared opaque CouchDB sequences by numeric prefix; transient checkpoint read errors were swallowed as "no checkpoint."
- The checkpoint advanced past documents that failed to parse or write (permanent data loss with no resume); `docs_written` counted failed writes.
- Attachments were dropped during replication (now carried end-to-end between memory adapters).
- Live replication emitted a `Complete` event on every poll iteration instead of once at the end.

**Changes feed:**
- `limit` counted pre-filter changes, so a filtered live feed delivered fewer than `limit` matches.
- The `heartbeat` option was declared but never honored.

**Umbrella (`Database`):**
- Index-accelerated `find()` ignored dotted/nested sort fields (e.g. `address.city`).
- `put` / `update` / `remove` panicked when a plugin dropped the document.

**HTTP server:**
- Wrong status codes on conflict/not-found and on existing-database `PUT` (see migration notes).
- `since=now` mapped to `u64::MAX`, causing an integer overflow.
- `PUT` ignored `_deleted` in the body.
- Attachment downloads guessed the content type from the filename, discarding the stored `content_type`.
- `doc_del_count` was hardcoded to `0`.
- `PUT /{db}/_security` dropped any fields outside `admins`/`members`.

**CLI:**
- `import` exited `0` even when documents failed.
- `put --force` swallowed non-`NotFound` errors from `get` and silently created instead of updating.

### Tests

- 366 unit tests passing (≈25 new regression tests); `clippy -D warnings` and `cargo fmt --check` clean.

---

## [0.3.2] — 2026-02-13

### Changes

- Update repository URL from `github.com/RubyLabApp/rouchdb` to `github.com/rubylab-app/rouchdb` following organization rename

---

## [0.3.1] — 2026-02-12

### Fixes

- Add fauxton `.gitkeep` so the `fauxton/` directory exists in CI (rust-embed requires it)
- Fix `cargo fmt` formatting in CLI and server session routes

---

## [0.3.0] — 2026-02-12

### New Crates

| Crate | Description |
|-------|-------------|
| `rouchdb-server` | CouchDB-compatible HTTP server with Fauxton web dashboard |
| `rouchdb-cli` | Command-line tool for inspecting and querying redb databases |

### New Features

#### HTTP Server (`rouchdb-server`)

A standalone CouchDB-compatible HTTP server built on Axum. Wraps any `.redb` database file and exposes it as a REST API that Fauxton, PouchDB, curl, or any CouchDB client can connect to.

**Endpoints implemented:**

| Endpoint | Description |
|----------|-------------|
| `GET /` | CouchDB welcome message (reports version 3.3.3 for Fauxton compatibility) |
| `GET/POST/DELETE /_session` | Session management (no-auth mode: always admin) |
| `GET /_all_dbs` | List databases (single-db mode) |
| `GET /_uuids` | Generate UUIDs |
| `GET /_active_tasks` | List running tasks |
| `GET /_membership` | Cluster membership (single node) |
| `GET /_utils/*` | Fauxton web dashboard (embedded static files via rust-embed) |
| `GET/PUT/DELETE /{db}` | Database info, creation, and deletion |
| `POST /{db}` | Create document with auto-generated ID |
| `GET/PUT/DELETE /{db}/{docid}` | Document CRUD |
| `GET/PUT/DELETE /{db}/{docid}/{attname}` | Attachment CRUD |
| `GET/POST /{db}/_all_docs` | Query all documents |
| `POST /{db}/_bulk_docs` | Bulk document writes |
| `GET/POST /{db}/_changes` | Changes feed |
| `POST /{db}/_find` | Mango queries |
| `GET/POST /{db}/_index` | Create and list Mango indexes |
| `DELETE /{db}/_index/{ddoc}/{type}/{name}` | Delete a Mango index |
| `POST /{db}/_index/_bulk_delete` | Bulk delete indexes |
| `POST /{db}/_explain` | Query execution plan |
| `POST /{db}/_compact` | Database compaction |
| `GET/PUT /{db}/_security` | Database permissions |
| `GET/PUT/DELETE /{db}/_design/{ddoc}` | Design document CRUD |
| `GET /{db}/_design/{ddoc}/_info` | Design document metadata |
| `GET/POST /{db}/_design/{ddoc}/_view/{view}` | View queries (returns error — JS views not supported) |

**Usage:**

```bash
# Install
cargo install --path crates/rouchdb-server

# Download Fauxton (optional)
bash scripts/download-fauxton.sh

# Start the server
rouchdb-server mydb.redb --port 5984

# Open Fauxton
open http://localhost:5984/_utils/
```

#### CLI Tool (`rouchdb-cli`)

A command-line tool for inspecting and querying redb database files without starting a server.

**Read commands:** `info`, `get`, `all-docs`, `find`, `changes`, `dump`.
**Write commands:** `put`, `post`, `delete`, `import`.
**Operations:** `replicate`, `compact`.

```bash
cargo install --path crates/rouchdb-cli
rouchdb info mydb.redb
rouchdb get mydb.redb user:alice
rouchdb find mydb.redb --selector '{"age": {"$gte": 30}}'
rouchdb put mydb.redb user:alice '{"name":"Alice","age":30}'
rouchdb post mydb.redb '{"name":"Bob"}'
rouchdb delete mydb.redb user:alice --rev 1-abc
rouchdb import mydb.redb docs.json
```

#### Redb Adapter — Attachment Support (`rouchdb-adapter-redb`)

The redb adapter now supports `put_attachment`, `get_attachment`, and `remove_attachment`. Attachments are stored in a dedicated redb table with content-addressable keys and tracked per-revision in the document metadata.

### Documentation

- Added HTTP server section to README, installation guide, and Spanish docs
- Updated crate guide with `rouchdb-server` and `rouchdb-cli` descriptions
- Updated architecture overview with new crates
- Updated CLAUDE.md dependency graph
- Added `scripts/download-fauxton.sh` for building Fauxton from the official Apache couchdb-fauxton source

---

## [0.2.1] — 2026-02-08

Patch release: add README to all crates for crates.io.

---

## [0.2.0] — 2026-02-07

Full PouchDB API parity release. This version adds every remaining PouchDB feature that was missing in 0.1.x, fixes several correctness bugs, and includes a new crate (`rouchdb-views`).

### Migration Guide (0.1.1 → 0.2.0)

#### Breaking Changes

1. **`Database` struct now has a `plugins` field.** If you destructure `Database`, update your code. Most users won't be affected since the fields are private.

2. **`ReplicationFilter::Custom` changed from `Box` to `Arc`:**
   ```rust
   // Before (0.1.1)
   ReplicationFilter::Custom(Box::new(|event| event.id.starts_with("user:")))

   // After (0.2.0)
   ReplicationFilter::Custom(Arc::new(|event| event.id.starts_with("user:")))
   ```

3. **`ReplicationOptions` has new fields.** If you construct it with struct literal syntax, add the new fields or use `..Default::default()`:
   ```rust
   // Recommended
   ReplicationOptions {
       batch_size: 50,
       ..Default::default()
   }
   ```
   New fields: `live`, `retry`, `poll_interval`, `back_off_function`, `since`, `checkpoint`.

4. **`AllDocsResponse` has a new optional `update_seq` field.** If you destructure it, add `update_seq`.

5. **`ChangeEvent` has a new optional `conflicts` field.** If you destructure it, add `conflicts`.

6. **`ChangesOptions` has new fields:** `conflicts`, `style` (`ChangesStyle` enum).

7. **`AllDocsOptions` has new fields:** `conflicts`, `update_seq`.

8. **`GetOptions` has new fields:** `revs_info`, `latest`, `attachments`.

9. **New `rouchdb-views` crate** is re-exported from the umbrella crate. No action needed unless you import crates individually.

10. **`build_path_from_revs` no longer panics on empty revs.** It now returns a degenerate single-node path. This is a behavior change that improves robustness.

#### New Dependencies

- `rouchdb-views` (new crate)
- `rouchdb-changes` now depends on `tokio-util` (for `CancellationToken`)
- `rouchdb-replication` now depends on `rouchdb-query`, `tokio-util`
- `rouchdb-core` now depends on `base64`
- `rouchdb-adapter-http` now depends on `percent-encoding`
- `rouchdb` (umbrella) now depends on `uuid`, `async-trait`

### New Features

#### Database API — New Methods

| Method | Description |
|--------|-------------|
| `post(data)` | Create a document with auto-generated UUID. Equivalent to PouchDB's `db.post()`. |
| `put_attachment(doc_id, att_id, rev, data, content_type)` | Store an attachment on a document. |
| `get_attachment(doc_id, att_id)` | Retrieve raw attachment bytes. |
| `get_attachment_with_opts(doc_id, att_id, opts)` | Retrieve attachment bytes with options (e.g., specific rev). |
| `remove_attachment(doc_id, att_id, rev)` | Remove an attachment from a document. |
| `create_index(def)` | Create a Mango index for faster queries. Returns `"created"` or `"exists"`. |
| `get_indexes()` | List all Mango indexes on this database. |
| `delete_index(name)` | Delete a Mango index by name. |
| `explain(opts)` | Analyze a query plan without executing it. |
| `live_changes(opts)` | Start a live changes stream → `(Receiver<ChangeEvent>, ChangesHandle)`. |
| `live_changes_events(opts)` | Live changes with lifecycle events → `(Receiver<ChangesEvent>, ChangesHandle)`. |
| `replicate_to_with_events(target, opts)` | One-shot replication with event streaming. |
| `replicate_to_live(target, opts)` | Continuous (live) replication → `(Receiver<ReplicationEvent>, ReplicationHandle)`. |
| `put_design(ddoc)` | Create or update a design document. |
| `get_design(name)` | Retrieve a design document by name. |
| `delete_design(name, rev)` | Delete a design document. |
| `view_cleanup()` | Remove orphaned view indexes. |
| `get_security()` | Get the database security document. |
| `put_security(doc)` | Set the database security document. |
| `close()` | Close the database and release resources. |
| `purge(doc_id, revs)` | Permanently remove document revisions (non-replicating). |
| `with_plugin(plugin)` | Register a plugin (builder pattern). |
| `partition(name)` | Get a partitioned view scoped to `"{name}:"` prefix. |
| `http_with_auth(url, auth)` | Connect to CouchDB with cookie authentication. |

#### Plugin System

New `Plugin` trait for extending database behavior with lifecycle hooks:

```rust
#[async_trait]
pub trait Plugin: Send + Sync {
    fn name(&self) -> &str;
    async fn before_write(&self, docs: &mut Vec<Document>) -> Result<()> { Ok(()) }
    async fn after_write(&self, results: &[DocResult]) -> Result<()> { Ok(()) }
    async fn on_destroy(&self) -> Result<()> { Ok(()) }
}

let db = Database::memory("mydb").with_plugin(my_plugin);
```

#### Partitioned Queries

```rust
let partition = db.partition("users");
partition.all_docs(AllDocsOptions::new()).await?;
partition.find(FindOptions { selector: json!({"age": {"$gt": 21}}), ..Default::default() }).await?;
partition.get("alice").await?;  // fetches "users:alice"
partition.put("bob", json!({})).await?;  // stores "users:bob"
```

#### Live Replication

Continuous replication that polls for changes and syncs automatically:

```rust
let (mut rx, handle) = local.replicate_to_live(&remote, ReplicationOptions {
    poll_interval: Duration::from_secs(5),
    retry: true,
    ..Default::default()
});

while let Some(event) = rx.recv().await {
    match event {
        ReplicationEvent::Change { docs_read } => println!("synced {docs_read} docs"),
        ReplicationEvent::Paused => println!("up to date, waiting..."),
        ReplicationEvent::Error(msg) => eprintln!("error: {msg}"),
        _ => {}
    }
}

handle.cancel(); // or just drop it
```

#### Live Changes Feed

Two flavors — raw events or lifecycle-aware:

```rust
// Raw change events
let (mut rx, handle) = db.live_changes(ChangesStreamOptions::default());
while let Some(event) = rx.recv().await {
    println!("{}: {}", event.id, event.seq);
}

// Lifecycle events
let (mut rx, handle) = db.live_changes_events(ChangesStreamOptions::default());
while let Some(event) = rx.recv().await {
    match event {
        ChangesEvent::Change(ce) => println!("changed: {}", ce.id),
        ChangesEvent::Paused => println!("waiting..."),
        ChangesEvent::Active => println!("processing..."),
        ChangesEvent::Complete { last_seq } => println!("done at {last_seq}"),
        ChangesEvent::Error(msg) => eprintln!("{msg}"),
    }
}
```

Both support Mango selector filtering — only matching documents are forwarded through the channel.

#### Mango Indexes

Persistent in-memory indexes that speed up `find()` queries:

```rust
db.create_index(IndexDefinition {
    name: String::new(), // auto-generated as "idx-age"
    fields: vec![SortField::Simple("age".into())],
    ddoc: None,
}).await?;

// Queries on "age" now use the index
let result = db.find(FindOptions {
    selector: json!({"age": {"$gte": 21}}),
    ..Default::default()
}).await?;

// Inspect query plan
let plan = db.explain(FindOptions {
    selector: json!({"age": {"$gt": 20}}),
    ..Default::default()
}).await;
println!("Using index: {} ({})", plan.index.name, plan.index.index_type);
```

#### Cookie Authentication (HTTP Adapter)

```rust
use rouchdb::{AuthClient, Database};

let auth = AuthClient::new("http://localhost:5984");
auth.login("admin", "password").await?;

let db = Database::http_with_auth("http://localhost:5984/mydb", &auth);
```

#### Changes Feed Selector Filtering

```rust
// One-shot with selector
let changes = db.changes(ChangesOptions {
    selector: Some(json!({"type": "user"})),
    include_docs: true,
    ..Default::default()
}).await?;

// Live changes with selector
let (mut rx, handle) = db.live_changes(ChangesStreamOptions {
    selector: Some(json!({"type": "user"})),
    ..Default::default()
});
```

#### New Core Types

| Type | Description |
|------|-------------|
| `PurgeResponse` | Result of a purge operation. |
| `SecurityDocument` / `SecurityGroup` | Database security configuration. |
| `RevInfo` | Revision info entry (rev + status). |
| `ChangesStyle` | Enum: `MainOnly` (default) or `AllDocs` (all leaf revisions). |
| `ChangesEvent` | Enum: `Change`, `Complete`, `Error`, `Paused`, `Active`. |
| `ChangesHandle` | Handle to cancel a live changes stream. |
| `ReplicationHandle` | Handle to cancel a live replication. |
| `IndexDefinition` / `IndexInfo` / `IndexFields` | Mango index types. |
| `CreateIndexResponse` / `BuiltIndex` | Index creation result and internal index state. |
| `ExplainResponse` / `ExplainIndex` | Query plan explanation types. |
| `DesignDocument` / `ViewDef` | Design document and view definition types. |
| `Plugin` trait | Lifecycle hooks for database operations. |
| `Partition` | Partitioned database view. |
| `AuthClient` / `Session` / `UserContext` | Cookie authentication types. |

#### New Adapter Trait Methods

| Method | Description |
|--------|-------------|
| `close()` | Release resources (default: no-op). |
| `purge(req)` | Permanently remove revisions. |
| `get_security()` | Get security document (default: empty). |
| `put_security(doc)` | Set security document (default: no-op). |

All new trait methods have default implementations, so existing custom adapters won't break.

#### New `rouchdb-views` Crate

Design documents and persistent view engine:
- `DesignDocument` struct with `views`, `filters`, `validate_doc_update`
- `ViewEngine` for persistent map/reduce indexes
- `PersistentViewIndex` for materialized view results

### Bug Fixes

- **Collation: `partial_cmp` → `total_cmp` for floats.** `f64::partial_cmp` returns `None` for NaN comparisons, which collapsed to `Equal`. Now uses `total_cmp` for well-defined ordering of all values including edge cases.
- **Collation: handle `NaN`, `Infinity`, `-Infinity` in number encoding.** The `encode_number` function panicked or produced incorrect sort keys for non-finite values. Now handles all IEEE 754 special values.
- **`build_path_from_revs` no longer panics on empty revs.** Returns a degenerate single-node path instead of hitting `assert!`.
- **`build_path_from_revs` uses saturating arithmetic.** Prevents underflow when `pos < len - 1`.
- **Attachment `from_json` parsing handles Base64 strings.** CouchDB sends inline attachment data as Base64 strings, but serde expected `Vec<u8>` (a JSON array). Now properly strips and decodes Base64 `data` fields.
- **`to_json` uses safe serialization for attachments.** Replaced `unwrap()` with `if let Ok(...)` to avoid panics on serialization edge cases.
- **ReDB `destroy()` uses O(1) table deletion.** Replaced `pop_last()` loops (O(n) per table) with `delete_table()` + `open_table()` for instant destruction regardless of database size.
- **Memory adapter: collapsible if-let chain.** Fixed Edition 2024 clippy lint.
- **`put()`, `update()`, `remove()` validate empty IDs.** Now return `RouchError::MissingId` instead of creating documents with empty IDs.
- **`put()`, `update()`, `remove()` now route through `bulk_docs()`.** This ensures plugins receive `before_write` / `after_write` hooks for all write operations.
- **`find()` rebuilds index on each query.** Indexes are lazily rebuilt before query execution to pick up document changes since the index was created.
- **HTTP adapter: URL-encode attachment IDs.** Attachment names with special characters (spaces, unicode) are now percent-encoded in HTTP requests.
- **HTTP adapter: `revs_info`, `latest`, `attachments` query parameters.** These `GetOptions` flags are now forwarded to CouchDB.

### Dependency Cleanup

- Removed 7 unused dependencies across 5 crates:
  - `rouchdb-core`: removed `md-5`, `uuid`
  - `rouchdb-changes`: removed `tokio-stream`, `async-trait`
  - `rouchdb-replication`: removed `rouchdb-changes`
  - `rouchdb-adapter-redb`: removed `base64`
  - `rouchdb-views`: removed `async-trait`

### Documentation

- Corrected 20+ inaccuracies across the mdBook documentation
- Fixed API signatures, type names, and code examples to match actual implementation
- Added `rouchdb-views` crate to installation guide and crate guide
- Updated dependency graph in CLAUDE.md and crate-guide.md
- Fixed Spanish translation of replication guide (missing fields, type errors)
- Added new chapters: live replication, live changes, design documents
- All mdBook builds cleanly with no warnings

### Tests

- 287 unit tests passing across all 9 crates
- New test suites: `post_and_attachments.rs`, expanded `changes_feed.rs`, expanded `mango_queries.rs`, expanded `replication.rs`
- Zero clippy warnings

---

## [0.1.1] — 2026-02-06

Version bump for crates.io publishing with updated metadata and repository URLs.

### Changes

- Bump version to 0.1.1 across all workspace crates
- Add MIT license file and crates.io publish metadata (description, keywords, categories)
- Update repository URL to `github.com/RubyLabApp/rouchdb`
- Add CI workflow (build, test, clippy, fmt) and docs deployment workflow (GitHub Actions)
- Fix all clippy warnings for clean CI
- Add CLAUDE.md and README.md
- Track `docker-compose.yml` for CouchDB integration tests
- Add filtered replication (doc_ids, selector, custom closure filters)

---

## [0.1.0] — 2026-02-05

Initial release. Core document database with CouchDB replication.

### Crates

| Crate | Description |
|-------|-------------|
| `rouchdb-core` | Types, traits, revision tree, merge algorithm, CouchDB-compatible collation, errors |
| `rouchdb-adapter-memory` | In-memory adapter for testing and ephemeral data |
| `rouchdb-adapter-redb` | Persistent local storage via redb (pure Rust, no C deps) |
| `rouchdb-adapter-http` | CouchDB HTTP client adapter via reqwest |
| `rouchdb-changes` | Changes feed with one-shot and live streaming modes |
| `rouchdb-replication` | CouchDB replication protocol with checkpoint-based incremental sync |
| `rouchdb-query` | Mango selectors (`$eq`, `$gt`, `$in`, `$regex`, etc.) and map/reduce views |
| `rouchdb` | Umbrella crate with `Database` API and re-exports |

### Features

- **Document CRUD:** `put`, `get`, `get_with_opts`, `update`, `remove`, `bulk_docs`, `all_docs`
- **Revision Tree:** Full CouchDB-compatible revision tree with merge algorithm and deterministic winning revision selection
- **Collation:** CouchDB-compatible ordering (null < bool < number < string < array < object)
- **Changes Feed:** One-shot and live streaming with doc_ids filtering
- **Replication:** Complete CouchDB replication protocol — checkpoints, revs_diff, bulk_get, new_edits=false
- **Mango Queries:** `find()` with selectors, field projection, sorting, skip/limit
- **Map/Reduce:** `query_view()` with custom map functions and built-in reduce (`_count`, `_sum`, `_stats`)
- **Attachments:** Binary attachment storage and retrieval via adapter trait
- **Multiple Backends:** Memory (testing), redb (persistent), HTTP (CouchDB remote)
- **Bidirectional Sync:** `sync()` method for push + pull in one call
- **Seq Type:** Handles both numeric (local) and opaque string (CouchDB 3.x) sequences
- **mdBook Documentation:** Guides, reference, architecture docs, and Spanish translations
- **77 integration tests** against real CouchDB
- **85%+ unit test coverage** across all crates
