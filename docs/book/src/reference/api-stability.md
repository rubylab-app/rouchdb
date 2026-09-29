# API Stability

RouchDB is pre-1.0, so a minor release (0.5 → 0.6) may still break the API. **0.5 is the last release in which adding a field to a struct or a variant to an enum is a breaking change**: its public types are shaped so that the library can grow without breaking code that follows the rules below. The next goal is stabilizing the API and the on-disk (redb) format toward 1.0.

The rules apply to every published crate: `rouchdb`, `rouchdb-core`, `rouchdb-adapter-memory`, `rouchdb-adapter-redb`, `rouchdb-adapter-http`, `rouchdb-changes`, `rouchdb-query`, `rouchdb-views` and `rouchdb-replication`. The server and the CLI are applications, not libraries.

## Enums that may grow: add a catch-all arm

These enums are `#[non_exhaustive]`: new variants may be added in a minor release, so a `match` on them needs a `_` arm.

| Enum | Why it may grow |
|------|-----------------|
| `RouchError` | New error conditions (for example a storage format that needs an upgrade). |
| `ChangesEvent` | New lifecycle events of a changes stream. `Complete { last_seq, .. }` may gain fields. |
| `ReplicationEvent` | New progress events. `Change { docs_read, .. }` may gain fields. |
| `ReplicationFilter` | CouchDB design-document filters, `_view` filters, ... |
| `ReduceFn` | More CouchDB built-ins, such as `_approx_count_distinct`. |
| `StaleOption` | CouchDB's newer `update=true/false/lazy` parameter. |
| `SortField` | A typed `{field, direction}` form. Use `SortField::try_field_and_direction` instead of matching. |

```rust
match db.get("doc").await {
    Ok(doc) => println!("{}", doc.data),
    Err(RouchError::NotFound(_)) => println!("no such document"),
    Err(e) => return Err(e), // every other error, including future ones
}
```

These enums are exhaustive on purpose, because their set of cases is closed and code must be able to handle every case without a catch-all:

| Enum | Why it is closed |
|------|------------------|
| `Seq` | A sequence is a JSON number (local adapters) or a JSON string (CouchDB) on the wire; code that compares or passes sequences back must handle both. |
| `OpenRevs` | CouchDB's `open_revs` is `"all"` or a list of revisions. |
| `ChangesStyle` | CouchDB's `style` is `main_only` or `all_docs`. |
| `SortDirection` | Ascending or descending. |
| `RevStatus` | A revision body is stored or it is not. |
| `MergeResult`, `ReplicatedWrite`, `LocalWrite` | Outcomes of the shared write planner (`rouchdb_core::merge` / `write`). An adapter must handle each one to store a write correctly, so a new outcome must be a compile error in the adapter rather than a silently ignored `_` arm. |

## Result types: read them, build them with constructors

Types the library returns are `#[non_exhaustive]`. Their fields stay public, so you read them as usual, but they cannot be built with a struct literal outside their crate. Code that has to build them (a custom `Adapter`, a plugin, a test double) uses their constructors:

| Type | Constructors |
|------|--------------|
| `DocResult` | `DocResult::ok(id, rev)`, `DocResult::error(id, error, reason)` |
| `DbInfo` | `DbInfo::new(db_name, doc_count, doc_del_count, update_seq)` |
| `AllDocsResponse` | `AllDocsResponse::new(total_rows, offset, rows)`, `.with_update_seq(seq)` |
| `AllDocsRow` | `AllDocsRow::document(id, value)`, `.with_doc(doc)`, `AllDocsRow::not_found(key)` |
| `AllDocsRowValue` | `AllDocsRowValue::new(rev, deleted)` |
| `ChangesResponse` | `ChangesResponse::new(results, last_seq)` |
| `ChangeEvent` | `ChangeEvent::new(seq, id, revs)`, `.with_deleted(..)`, `.with_doc(..)`, `.with_conflicts(..)` |
| `ChangeRev` | `ChangeRev::new(rev)` |
| `RevsDiffResponse`, `RevsDiffResult` | `RevsDiffResponse::new(map)`, `RevsDiffResult::new(missing, possible_ancestors)` |
| `BulkGetResponse`, `BulkGetResult` | `BulkGetResponse::new(results)`, `BulkGetResult::new(id, docs)` |
| `BulkGetDoc`, `BulkGetError` | `BulkGetDoc::ok(json)`, `BulkGetDoc::error(err)`, `BulkGetError::new(id, rev, error, reason)` |
| `PurgeResponse` | `PurgeResponse::new(purge_seq, purged)` |
| `ChangeNotice` | `ChangeNotice::new(seq, doc_id)` |
| `DocMetadata` | `DocMetadata::new(id, rev_tree, seq)` |
| `FindResponse` | `FindResponse::new(docs)` |
| `IndexInfo`, `IndexFields` | `IndexInfo::new(name, ddoc, fields)`, `IndexFields::new(fields)` |
| `CreateIndexResponse` | `CreateIndexResponse::new(result, name)` |
| `ExplainResponse`, `ExplainIndex` | `ExplainResponse::new(dbname, index, selector, fields)`, `ExplainIndex::new(ddoc, name, type, fields)` |
| `BuiltIndex` | `build_index(..)`, `BuiltIndex::new(def)` |
| `EmittedRow` | `EmittedRow::new(id, key, value)` |

These are `#[non_exhaustive]` without a public constructor because only the library builds them: `PutResponse`, `RevInfo`, `ViewResult`, `ViewRow`, `ReplicationResult`, `PersistentViewIndex`, `Session`, `UserContext`, `PlannedWrite`, `LeafInfo`.

See [Building Results](../guides/adapters.md#building-results) for a custom adapter.

## Options and data: use `..Default::default()`

Option structs are not `#[non_exhaustive]` (that would forbid the `..Default::default()` syntax outside their crate). They implement `Default`; set the fields you need and fill the rest from the default:

```rust
let rows = db.all_docs(AllDocsOptions {
    include_docs: true,
    limit: Some(10),
    ..Default::default()
}).await?;
```

A literal that lists every field stops compiling when a field is added; **adding a field with a default value is not considered a breaking change**, so always end the literal with `..Default::default()` (or `..X::new()`, where `new()` exists and differs from `default()`, as for `ViewQueryOptions`). This covers `GetOptions`, `BulkDocsOptions`, `AllDocsOptions`, `ChangesOptions`, `GetAttachmentOptions`, `FindOptions`, `ViewQueryOptions`, `ChangesStreamOptions`, `ReplicationOptions`, `HttpAdapterOptions` and `IndexDefinition`.

Data you both build and read follows the same rule, and has constructors where they read better:

| Type | Build it with |
|------|---------------|
| `Document` | `Document::new(id, data)`, `Document::from_json(json)`, or `..Default::default()` |
| `AttachmentMeta` | `AttachmentMeta::new(content_type, bytes)` or `..Default::default()` |
| `DesignDocument`, `ViewDef` | `DesignDocument::new(name).with_view(..)`, `ViewDef::new(map).with_reduce(..)`, or `..Default::default()` |
| `SecurityDocument`, `SecurityGroup` | `..Default::default()` (unknown members round-trip through `extra`) |
| `BulkGetItem` | `BulkGetItem::new(id).with_rev(rev)` or `..Default::default()` |
| `Revision` | `Revision::new(pos, hash)` or `"1-abc".parse()`; a revision id is `{pos}-{hash}` by definition, so this type will not grow. |

## Low-level modules

`rouchdb_core::rev_tree`, `merge`, `write`, `collation` and `json` are the building blocks of the bundled adapters. The revision tree types (`RevPath`, `RevNode`, `NodeOpts`) are plain data that adapters build and persist; they mirror the stored format and will be stabilized together with the on-disk format on the way to 1.0.

## Traits

New `Adapter` and `Plugin` methods are added with a default implementation, so existing implementations keep compiling (as `Adapter::subscribe` and `Adapter::id` were in 0.5).
