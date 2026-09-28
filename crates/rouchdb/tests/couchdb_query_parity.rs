//! Parity tables: run the same Mango queries and view queries against a
//! real CouchDB and against RouchDB, and compare the results in order.
//!
//! The Mango table holds the results CouchDB 3.5.1 returns. The
//! `#[ignore]`d tests (run with `-- --ignored`, see `common`) check the
//! table against CouchDB; `mango_table_matches_couchdb_results_locally` runs
//! it on the memory and redb adapters in every `cargo test`.

mod common;

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::task::{Context, Poll};

use common::{delete_remote_db, fresh_remote_db};
use rouchdb::{
    BulkDocsOptions, Database, Document, FindOptions, IndexDefinition, ReduceFn, SortField,
    ViewQueryOptions, query_view,
};
use serde_json::{Value, json};

// =========================================================================
// CouchDB helpers
// =========================================================================

/// A future that turns a panic while polling `F` into an `Err`.
struct CatchUnwind<F>(Pin<Box<F>>);

impl<F: Future> Future for CatchUnwind<F> {
    type Output = std::thread::Result<F::Output>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = self.0.as_mut();
        match std::panic::catch_unwind(AssertUnwindSafe(|| inner.poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(value)) => Poll::Ready(Ok(value)),
            Err(panic) => Poll::Ready(Err(panic)),
        }
    }
}

/// Run `body` against a fresh single-shard CouchDB database, deleting the
/// database afterwards even if `body` panics.
///
/// With several shards CouchDB merges per-shard results, so duplicate
/// `keys` and the `offset` of multi-key queries depend on the shard layout;
/// one shard gives the plain semantics that PouchDB and RouchDB implement.
async fn with_couch_db<F, Fut>(prefix: &str, body: F)
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = ()>,
{
    let url = fresh_remote_db(prefix).await;
    let client = reqwest::Client::new();
    client.delete(&url).send().await.unwrap();
    let created = client.put(format!("{url}?q=1")).send().await.unwrap();
    assert!(created.status().is_success(), "{}", created.status());
    let result = CatchUnwind(Box::pin(body(url.clone()))).await;
    delete_remote_db(&url).await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn couch_bulk_docs(url: &str, docs: &[Value]) {
    let resp = reqwest::Client::new()
        .post(format!("{url}/_bulk_docs"))
        .json(&json!({ "docs": docs }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
}

async fn local_bulk_docs(db: &Database, docs: &[Value]) {
    let docs = docs
        .iter()
        .map(|doc| Document::from_json(doc.clone()).unwrap())
        .collect();
    for result in db.bulk_docs(docs, BulkDocsOptions::new()).await.unwrap() {
        assert!(result.ok, "{result:?}");
    }
}

/// Revision hashes differ between CouchDB and RouchDB, so compare bodies.
fn without_rev(doc: &Value) -> Value {
    let mut doc = doc.clone();
    if let Some(obj) = doc.as_object_mut() {
        obj.remove("_rev");
    }
    doc
}

// =========================================================================
// Mango
// =========================================================================

/// Documents of the Mango table. Strings compared by range or sorted are
/// ASCII lowercase, where CouchDB's ICU collation agrees with RouchDB's
/// code unit order; the non-ASCII strings in `u` are only matched by
/// equality, `$in`, `$regex` and `$beginsWith`.
fn mango_corpus() -> Vec<Value> {
    vec![
        json!({"_id": "d1", "address": {"city": "nyc", "zip": "10001"}, "age": 3,
               "tags": ["rust", "db"], "items": [{"subject": "math", "name": "x"}, {"subject": "art"}],
               "scores": [50, 85, 60], "x": 3, "n": 10}),
        json!({"_id": "d2", "address": {"city": "la"}, "age": 20, "tags": ["js"],
               "items": [{"subject": "bio"}], "scores": [10], "x": 7, "n": -7}),
        json!({"_id": "d3", "name": "noage"}),
        json!({"_id": "d4", "age": -0.0, "big": 9_007_199_254_740_993_u64, "n": i64::MIN}),
        json!({"_id": "d5", "a.b": 1, "a": {"b": 2}}),
        json!({"_id": "d6", "address": {}, "m": {"b": 1, "c": 2}, "v": 5.0}),
        json!({"_id": "d7", "age": null, "flag": false, "tags": [], "obj": {}, "s": "", "f": 1.5,
               "u": "ñandú", "opt": null}),
        json!({"_id": "d8", "age": true, "flag": true, "tags": ["db"], "s": "b", "f": -2.25,
               "u": "émile", "opt": "x"}),
        json!({"_id": "d9", "age": "20", "tags": ["js", "rust"], "s": "abc", "f": 0.1, "u": "日本"}),
        json!({"_id": "d10", "age": [20], "flag": false, "s": "ab", "f": -0.5, "u": "😀",
               "tags": ["rust"]}),
        json!({"_id": "d11", "age": {"v": 1}, "s": "a", "f": 2, "flag": true}),
        json!({"_id": "_design/foo", "views": {}}),
    ]
}

/// `(query, expected)` for queries run before any index exists, where
/// CouchDB returns documents in `_id` order. `expected` lists the ids of
/// the corpus documents returned whole, or the documents themselves when
/// the query has `fields`. A query without `limit` is sent to CouchDB with
/// a large one, since CouchDB defaults to 25.
fn unindexed_queries() -> Vec<(Value, Value)> {
    vec![
        // F14: nested sub-document selectors
        (
            json!({"selector": {"address": {"city": "nyc"}}}),
            json!(["d1"]),
        ),
        (
            json!({"selector": {"address": {"$gt": null, "city": "nyc"}}}),
            json!(["d1"]),
        ),
        (json!({"selector": {"address": {}}}), json!(["d6"])),
        (json!({"selector": {"m": {"b": 1}}}), json!(["d6"])),
        (json!({"selector": {"age": {"v": 1}}}), json!(["d11"])),
        // F15: combinators inside a field or $elemMatch
        (
            json!({"selector": {"age": {"$or": [{"$lt": 5}, {"$gt": 10}]}}}),
            json!(["d1", "d10", "d11", "d2", "d4", "d7", "d8", "d9"]),
        ),
        (
            json!({"selector": {"age": {"$and": [{"$gt": 1}, {"$lt": 5}]}}}),
            json!(["d1"]),
        ),
        (
            json!({"selector": {"items": {"$elemMatch": {"$or": [{"subject": "math"}, {"subject": "bio"}]}}}}),
            json!(["d1", "d2"]),
        ),
        (
            json!({"selector": {"scores": {"$elemMatch": {"$gt": 40, "$lt": 55}}}}),
            json!(["d1"]),
        ),
        // F43: $not over several operators
        (
            json!({"selector": {"x": {"$not": {"$gt": 5, "$lt": 10}}}}),
            json!(["d1"]),
        ),
        (
            json!({"selector": {"x": {"$not": {"$in": [3]}}}}),
            json!(["d2"]),
        ),
        (
            json!({"selector": {"flag": {"$not": {"$eq": true}}}}),
            json!(["d10", "d7"]),
        ),
        (
            json!({"selector": {"$and": [{"flag": {"$exists": true}}, {"$not": {"flag": true}}]}}),
            json!(["d10", "d7"]),
        ),
        // F44: $in / $nin on arrays
        (
            json!({"selector": {"tags": {"$in": ["rust"]}}}),
            json!(["d1", "d10", "d9"]),
        ),
        (
            json!({"selector": {"tags": {"$nin": ["rust"]}}}),
            json!(["d2", "d7", "d8"]),
        ),
        (json!({"selector": {"tags": {"$in": [["js"]]}}}), json!([])),
        (
            json!({"selector": {"age": {"$in": [20]}}}),
            json!(["d10", "d2"]),
        ),
        (json!({"selector": {"age": {"$in": [[20]]}}}), json!([])),
        // $in / $nin with null and false (CouchDB 500s on these with an index)
        (
            json!({"selector": {"age": {"$in": [null, false]}}}),
            json!(["d7"]),
        ),
        (
            json!({"selector": {"age": {"$in": [null, true]}}}),
            json!(["d7", "d8"]),
        ),
        (
            json!({"selector": {"flag": {"$in": [false]}}}),
            json!(["d10", "d7"]),
        ),
        (
            json!({"selector": {"flag": {"$nin": [null, false]}}}),
            json!(["d11", "d8"]),
        ),
        (
            json!({"selector": {"age": {"$nin": [null, true, 20]}}}),
            json!(["d1", "d11", "d4", "d9"]),
        ),
        // F100: missing fields and negations
        (
            json!({"selector": {"age": {"$ne": 20}}}),
            json!(["d1", "d10", "d11", "d4", "d7", "d8", "d9"]),
        ),
        (
            json!({"selector": {"age": {"$nin": [20]}}}),
            json!(["d1", "d11", "d4", "d7", "d8", "d9"]),
        ),
        (
            json!({"selector": {"$not": {"age": 20}}}),
            json!(["d1", "d10", "d11", "d4", "d7", "d8", "d9"]),
        ),
        (
            json!({"selector": {"$nor": [{"age": 20}]}}),
            json!(["d1", "d10", "d11", "d4", "d7", "d8", "d9"]),
        ),
        (
            json!({"selector": {"age": {"$not": {"$regex": "x"}}}}),
            json!(["d1", "d10", "d11", "d2", "d4", "d7", "d8", "d9"]),
        ),
        (
            json!({"selector": {"$not": {"age": {"$exists": true}}}}),
            json!(["d3", "d5", "d6"]),
        ),
        (
            json!({"selector": {"address.city.x": {"$exists": false}}}),
            json!(["d10", "d11", "d3", "d4", "d5", "d6", "d7", "d8", "d9"]),
        ),
        // F101: paths through arrays, escaped dots
        (json!({"selector": {"items.0.name": "x"}}), json!(["d1"])),
        (
            json!({"selector": {"items.1.subject": "art"}}),
            json!(["d1"]),
        ),
        (
            json!({"selector": {"scores.1": {"$gt": 80}}}),
            json!(["d1"]),
        ),
        (
            json!({"selector": {"tags.0": "rust"}}),
            json!(["d1", "d10"]),
        ),
        (
            json!({"selector": {"tags.1": {"$exists": true}}}),
            json!(["d1", "d9"]),
        ),
        (json!({"selector": {"age.0": 20}}), json!(["d10"])),
        (json!({"selector": {"age.v": 1}}), json!(["d11"])),
        (json!({"selector": {"a\\.b": 1}}), json!(["d5"])),
        (json!({"selector": {"a.b": 2}}), json!(["d5"])),
        // F99: $mod
        (
            json!({"selector": {"n": {"$mod": [-1, 0]}}}),
            json!(["d1", "d2", "d4"]),
        ),
        (json!({"selector": {"n": {"$mod": [3, 1]}}}), json!(["d1"])),
        (json!({"selector": {"v": {"$mod": [5, 0]}}}), json!([])),
        (json!({"selector": {"f": {"$mod": [2, 0]}}}), json!(["d11"])),
        // F46: design documents are never returned
        (
            json!({"selector": {}}),
            json!([
                "d1", "d10", "d11", "d2", "d3", "d4", "d5", "d6", "d7", "d8", "d9"
            ]),
        ),
        (
            json!({"selector": {"_id": {"$gt": null}}}),
            json!([
                "d1", "d10", "d11", "d2", "d3", "d4", "d5", "d6", "d7", "d8", "d9"
            ]),
        ),
        (
            json!({"selector": {"views": {"$exists": false}}}),
            json!([
                "d1", "d10", "d11", "d2", "d3", "d4", "d5", "d6", "d7", "d8", "d9"
            ]),
        ),
        // F79: -0.0 and exact integer/float comparison
        (json!({"selector": {"age": 0}}), json!(["d4"])),
        (
            json!({"selector": {"big": {"$eq": 9007199254740992.0}}}),
            json!([]),
        ),
        (
            json!({"selector": {"big": {"$gt": 9007199254740992.0}}}),
            json!(["d4"]),
        ),
        // Ranges across types: null < false < true < numbers < strings < arrays < objects
        (
            json!({"selector": {"age": {"$lt": 0}}}),
            json!(["d7", "d8"]),
        ),
        (
            json!({"selector": {"age": {"$lte": 0}}}),
            json!(["d4", "d7", "d8"]),
        ),
        (
            json!({"selector": {"age": {"$gt": 3, "$lte": 20}}}),
            json!(["d2"]),
        ),
        (
            json!({"selector": {"age": {"$gte": "1"}}}),
            json!(["d10", "d11", "d9"]),
        ),
        (
            json!({"selector": {"age": {"$gte": "a"}}}),
            json!(["d10", "d11"]),
        ),
        (json!({"selector": {"age": {"$lte": false}}}), json!(["d7"])),
        (
            json!({"selector": {"age": {"$gt": null}}}),
            json!(["d1", "d10", "d11", "d2", "d4", "d8", "d9"]),
        ),
        (
            json!({"selector": {"age": {"$gte": null}}}),
            json!(["d1", "d10", "d11", "d2", "d4", "d7", "d8", "d9"]),
        ),
        (
            json!({"selector": {"age": {"$gte": [], "$lt": {}}}}),
            json!(["d10"]),
        ),
        (json!({"selector": {"age": {"$gte": {}}}}), json!(["d11"])),
        // Non-integer floats
        (
            json!({"selector": {"f": {"$gt": 0.1}}}),
            json!(["d11", "d7"]),
        ),
        (json!({"selector": {"f": {"$lt": 0}}}), json!(["d10", "d8"])),
        (json!({"selector": {"f": 0.1}}), json!(["d9"])),
        (
            json!({"selector": {"f": {"$gte": -0.5, "$lte": 1.5}}}),
            json!(["d10", "d7", "d9"]),
        ),
        // Equality with null, booleans and empty values
        (json!({"selector": {"age": null}}), json!(["d7"])),
        (json!({"selector": {"age": {"$eq": null}}}), json!(["d7"])),
        (
            json!({"selector": {"age": {"$ne": null}}}),
            json!(["d1", "d10", "d11", "d2", "d4", "d8", "d9"]),
        ),
        (json!({"selector": {"age": true}}), json!(["d8"])),
        (json!({"selector": {"age": [20]}}), json!(["d10"])),
        (
            json!({"selector": {"age": {"$eq": {"v": 1}}}}),
            json!(["d11"]),
        ),
        (json!({"selector": {"s": ""}}), json!(["d7"])),
        (json!({"selector": {"obj": {}}}), json!(["d7"])),
        (json!({"selector": {"tags": []}}), json!(["d7"])),
        // Non-ASCII strings (equality only: CouchDB orders them with ICU)
        (json!({"selector": {"u": "ñandú"}}), json!(["d7"])),
        (
            json!({"selector": {"u": {"$in": ["日本", "😀"]}}}),
            json!(["d10", "d9"]),
        ),
        // $exists on a null field
        (
            json!({"selector": {"opt": {"$exists": true}}}),
            json!(["d7", "d8"]),
        ),
        (
            json!({"selector": {"opt": {"$exists": false}}}),
            json!(["d1", "d10", "d11", "d2", "d3", "d4", "d5", "d6", "d9"]),
        ),
        // $type
        (
            json!({"selector": {"age": {"$type": "number"}}}),
            json!(["d1", "d2", "d4"]),
        ),
        (
            json!({"selector": {"age": {"$type": "null"}}}),
            json!(["d7"]),
        ),
        (
            json!({"selector": {"flag": {"$type": "boolean"}}}),
            json!(["d10", "d11", "d7", "d8"]),
        ),
        (
            json!({"selector": {"tags": {"$type": "array"}}}),
            json!(["d1", "d10", "d2", "d7", "d8", "d9"]),
        ),
        (
            json!({"selector": {"age": {"$type": "object"}}}),
            json!(["d11"]),
        ),
        (
            json!({"selector": {"obj": {"$type": "object"}}}),
            json!(["d7"]),
        ),
        (
            json!({"selector": {"age": {"$type": "string"}}}),
            json!(["d9"]),
        ),
        // $regex and $beginsWith
        (
            json!({"selector": {"s": {"$regex": "^a"}}}),
            json!(["d10", "d11", "d9"]),
        ),
        (
            json!({"selector": {"s": {"$regex": "b$"}}}),
            json!(["d10", "d8"]),
        ),
        (json!({"selector": {"u": {"$regex": "ñ"}}}), json!(["d7"])),
        (json!({"selector": {"age": {"$regex": "2"}}}), json!(["d9"])),
        (
            json!({"selector": {"name": {"$beginsWith": "no"}}}),
            json!(["d3"]),
        ),
        (
            json!({"selector": {"s": {"$beginsWith": ""}}}),
            json!(["d10", "d11", "d7", "d8", "d9"]),
        ),
        (
            json!({"selector": {"s": {"$beginsWith": "a"}}}),
            json!(["d10", "d11", "d9"]),
        ),
        (
            json!({"selector": {"u": {"$beginsWith": "é"}}}),
            json!(["d8"]),
        ),
        // $size, $all, $allMatch, $elemMatch, $keyMapMatch
        (
            json!({"selector": {"tags": {"$size": 2}}}),
            json!(["d1", "d9"]),
        ),
        (
            json!({"selector": {"tags": {"$size": 1}}}),
            json!(["d10", "d2", "d8"]),
        ),
        (json!({"selector": {"tags": {"$size": 0}}}), json!(["d7"])),
        (json!({"selector": {"tags": {"$all": []}}}), json!([])),
        (
            json!({"selector": {"tags": {"$all": ["rust"]}}}),
            json!(["d1", "d10", "d9"]),
        ),
        (
            json!({"selector": {"tags": {"$all": [["rust"]]}}}),
            json!(["d10"]),
        ),
        (
            json!({"selector": {"tags": {"$all": [["rust", "db"]]}}}),
            json!(["d1"]),
        ),
        (json!({"selector": {"age": {"$all": [20]}}}), json!(["d10"])),
        (
            json!({"selector": {"age": {"$all": [[20]]}}}),
            json!(["d10"]),
        ),
        (
            json!({"selector": {"scores": {"$allMatch": {"$gt": 40}}}}),
            json!(["d1"]),
        ),
        (
            json!({"selector": {"tags": {"$allMatch": {"$eq": "rust"}}}}),
            json!(["d10"]),
        ),
        (
            json!({"selector": {"tags": {"$allMatch": {"$gt": "a"}}}}),
            json!(["d1", "d10", "d2", "d8", "d9"]),
        ),
        (
            json!({"selector": {"tags": {"$allMatch": {"$or": ["rust", "db"]}}}}),
            json!(["d1", "d10", "d8"]),
        ),
        (json!({"selector": {"tags": {"$elemMatch": {}}}}), json!([])),
        (
            json!({"selector": {"items": {"$elemMatch": {}}}}),
            json!([]),
        ),
        (
            json!({"selector": {"tags": {"$elemMatch": {"$eq": "db"}}}}),
            json!(["d1", "d8"]),
        ),
        (
            json!({"selector": {"tags": {"$elemMatch": {"$or": ["js", "db"]}}}}),
            json!(["d1", "d2", "d8", "d9"]),
        ),
        (
            json!({"selector": {"address": {"$keyMapMatch": {"$eq": "zip"}}}}),
            json!(["d1"]),
        ),
        (
            json!({"selector": {"address": {"$keyMapMatch": {"$beginsWith": "z"}}}}),
            json!(["d1"]),
        ),
        (
            json!({"selector": {"m": {"$keyMapMatch": {"$eq": "c"}}}}),
            json!(["d6"]),
        ),
        (
            json!({"selector": {"m": {"$keyMapMatch": {"$or": ["b", "z"]}}}}),
            json!(["d6"]),
        ),
        (
            json!({"selector": {"obj": {"$keyMapMatch": {"$eq": "c"}}}}),
            json!([]),
        ),
        // Combinators with bare values
        (
            json!({"selector": {"s": {"$or": ["a", "b"]}}}),
            json!(["d11", "d8"]),
        ),
        (
            json!({"selector": {"s": {"$or": ["a", {"$eq": "b"}]}}}),
            json!(["d11", "d8"]),
        ),
        (json!({"selector": {"s": {"$and": ["a"]}}}), json!(["d11"])),
        (
            json!({"selector": {"s": {"$nor": ["a", "b"]}}}),
            json!(["d10", "d7", "d9"]),
        ),
        (json!({"selector": {"$or": [1]}}), json!([])),
        (
            json!({"selector": {"$or": [{"s": "a"}, 1]}}),
            json!(["d11"]),
        ),
        (json!({"selector": {"$and": [{"s": "a"}, 1]}}), json!([])),
        (
            json!({"selector": {"$nor": [1]}}),
            json!([
                "d1", "d10", "d11", "d2", "d3", "d4", "d5", "d6", "d7", "d8", "d9"
            ]),
        ),
        // fields, skip and limit (in _id order without an index)
        (
            json!({"selector": {"_id": {"$gt": "d5"}}, "fields": ["_id", "s"]}),
            json!([{"_id": "d6"}, {"_id": "d7", "s": ""}, {"_id": "d8", "s": "b"}, {"_id": "d9", "s": "abc"}]),
        ),
        (
            json!({"selector": {"address.city": {"$exists": true}}, "fields": ["address.city", "address.zip"]}),
            json!([{"address": {"city": "nyc", "zip": "10001"}}, {"address": {"city": "la"}}]),
        ),
        (
            json!({"selector": {"address.city": {"$exists": true}}, "fields": ["address.zip", "address.city", "_id"]}),
            json!([{"address": {"zip": "10001", "city": "nyc"}, "_id": "d1"}, {"address": {"city": "la"}, "_id": "d2"}]),
        ),
        (
            json!({"selector": {"tags": {"$size": 2}}, "fields": ["tags.0", "_id"]}),
            json!([{"tags": {"0": "rust"}, "_id": "d1"}, {"tags": {"0": "js"}, "_id": "d9"}]),
        ),
        (
            json!({"selector": {"a.b": 2}, "fields": ["a\\.b", "a.b"]}),
            json!([{"a.b": 1, "a": {"b": 2}}]),
        ),
        (
            json!({"selector": {"s": {"$gt": null}}, "fields": ["_id", "nope"]}),
            json!([{"_id": "d10"}, {"_id": "d11"}, {"_id": "d7"}, {"_id": "d8"}, {"_id": "d9"}]),
        ),
        (
            json!({"selector": {"age": {"$gte": null}}, "skip": 2, "limit": 3}),
            json!(["d11", "d2", "d4"]),
        ),
        (
            json!({"selector": {"age": {"$gte": null}}, "limit": 0}),
            json!([]),
        ),
        (
            json!({"selector": {"age": {"$gte": null}}, "skip": 100}),
            json!([]),
        ),
    ]
}

/// Queries CouchDB rejects with a 400, which RouchDB must reject too.
fn rejected_queries() -> Vec<Value> {
    vec![
        json!({"selector": {"$gt": 1}}),
        json!({"selector": {"age": {"$foo": 1}}}),
        json!({"selector": {"s": {"$regex": "[a"}}}),
        json!({"selector": {"age": {"$in": 20}}}),
        json!({"selector": {"age": {"$size": -1}}}),
        json!({"selector": {"age": {"$exists": "yes"}}}),
        json!({"selector": {"$or": {"age": 3}}}),
        json!({"selector": {"a..b": 1}}),
        json!({"selector": {"age": {"$mod": [2.5, 1]}}}),
        json!({"selector": {"tags": {"$allMatch": 5}}}),
        json!({"selector": {"m": {"$keyMapMatch": "b"}}}),
        json!({"selector": {"s": {"$type": 1}}}),
        json!({"selector": {"s": {"$beginsWith": 1}}}),
        json!({"selector": {"s": {"$gt": null}}, "sort": [{"s": "up"}]}),
        json!({"selector": {"s": {"$gt": null}}, "sort": [{"s": "asc", "f": "asc"}]}),
    ]
}

/// JSON indexes created, in CouchDB and in RouchDB, before
/// `indexed_queries` run: CouchDB needs one for every sort.
fn mango_indexes() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        ("by-age", vec!["age"]),
        ("by-s", vec!["s"]),
        ("by-f", vec!["f"]),
        ("by-tag0", vec!["tags.0"]),
        ("by-flag-s", vec!["flag", "s"]),
    ]
}

/// Like `unindexed_queries`, run once the `mango_indexes` exist. Without a
/// sort, documents come in the order of the index used.
fn indexed_queries() -> Vec<(Value, Value)> {
    vec![
        // Unsorted: index order (key, then _id)
        (
            json!({"selector": {"age": {"$gte": null}}}),
            json!(["d7", "d8", "d4", "d1", "d2", "d9", "d10", "d11"]),
        ),
        (
            json!({"selector": {"age": {"$lt": 20}}}),
            json!(["d7", "d8", "d4", "d1"]),
        ),
        (
            json!({"selector": {"age": {"$lte": 3}}}),
            json!(["d7", "d8", "d4", "d1"]),
        ),
        (
            json!({"selector": {"age": {"$gt": 3}}}),
            json!(["d2", "d9", "d10", "d11"]),
        ),
        (
            json!({"selector": {"age": {"$gte": 3, "$lt": "z"}}}),
            json!(["d1", "d2", "d9"]),
        ),
        (json!({"selector": {"age": 20}}), json!(["d2"])),
        (json!({"selector": {"age": {"$eq": 0}}}), json!(["d4"])),
        (
            json!({"selector": {"age": {"$in": [20, 3]}}}),
            json!(["d1", "d2", "d10"]),
        ),
        (
            json!({"selector": {"tags.0": {"$gt": null}}}),
            json!(["d8", "d2", "d9", "d1", "d10"]),
        ),
        (
            json!({"selector": {"tags.0": "rust"}}),
            json!(["d1", "d10"]),
        ),
        (
            json!({"selector": {"age": {"$gte": null}}, "sort": ["age"]}),
            json!(["d7", "d8", "d4", "d1", "d2", "d9", "d10", "d11"]),
        ),
        (
            json!({"selector": {"age": {"$gte": null}}, "sort": [{"age": "asc"}]}),
            json!(["d7", "d8", "d4", "d1", "d2", "d9", "d10", "d11"]),
        ),
        (
            json!({"selector": {"age": {"$gte": null}}, "sort": [{"age": "desc"}]}),
            json!(["d11", "d10", "d9", "d2", "d1", "d4", "d8", "d7"]),
        ),
        (
            json!({"selector": {"age": {"$gt": null}}, "sort": [{"age": "desc"}], "skip": 1, "limit": 3}),
            json!(["d10", "d9", "d2"]),
        ),
        (
            json!({"selector": {"age": {"$gt": null}}, "sort": ["age"], "skip": 6, "limit": 5}),
            json!(["d11"]),
        ),
        (
            json!({"selector": {"s": {"$gt": null}}, "sort": ["s"], "fields": ["_id", "s"]}),
            json!([{"_id": "d7", "s": ""}, {"_id": "d11", "s": "a"}, {"_id": "d10", "s": "ab"}, {"_id": "d9", "s": "abc"}, {"_id": "d8", "s": "b"}]),
        ),
        (
            json!({"selector": {"s": {"$gt": "a"}}, "sort": [{"s": "desc"}], "fields": ["s"]}),
            json!([{"s": "b"}, {"s": "abc"}, {"s": "ab"}]),
        ),
        (
            json!({"selector": {"s": {"$gte": ""}}, "sort": ["s"], "limit": 2, "skip": 1}),
            json!(["d11", "d10"]),
        ),
        (
            json!({"selector": {"f": {"$gt": -1}}, "sort": ["f"], "fields": ["_id", "f"]}),
            json!([{"_id": "d10", "f": -0.5}, {"_id": "d9", "f": 0.1}, {"_id": "d7", "f": 1.5}, {"_id": "d11", "f": 2}]),
        ),
        (
            json!({"selector": {"f": {"$exists": true}}, "sort": [{"f": "desc"}], "fields": ["f"]}),
            json!([{"f": 2}, {"f": 1.5}, {"f": 0.1}, {"f": -0.5}, {"f": -2.25}]),
        ),
        (
            json!({"selector": {"tags.0": {"$gt": null}}, "sort": ["tags.0"], "fields": ["_id", "tags"]}),
            json!([{"_id": "d8", "tags": ["db"]}, {"_id": "d2", "tags": ["js"]}, {"_id": "d9", "tags": ["js", "rust"]}, {"_id": "d1", "tags": ["rust", "db"]}, {"_id": "d10", "tags": ["rust"]}]),
        ),
        // Ties in a descending sort are left out: CouchDB walks the index
        // backwards (ties in reverse _id order), RouchDB keeps them in _id
        // order.
        (
            json!({"selector": {"flag": {"$exists": true}, "s": {"$exists": true}}, "sort": ["flag", "s"], "fields": ["_id", "flag", "s"]}),
            json!([{"_id": "d7", "flag": false, "s": ""}, {"_id": "d10", "flag": false, "s": "ab"}, {"_id": "d11", "flag": true, "s": "a"}, {"_id": "d8", "flag": true, "s": "b"}]),
        ),
        (
            json!({"selector": {"flag": {"$exists": true}, "s": {"$exists": true}}, "sort": [{"flag": "desc"}, {"s": "desc"}], "fields": ["_id"]}),
            json!([{"_id": "d8"}, {"_id": "d11"}, {"_id": "d10"}, {"_id": "d7"}]),
        ),
        (
            json!({"selector": {"flag": false}, "sort": ["flag", "s"], "fields": ["_id"]}),
            json!([{"_id": "d7"}, {"_id": "d10"}]),
        ),
    ]
}

/// The documents `expected` stands for (see `unindexed_queries`).
fn expected_docs(expected: &Value) -> Vec<Value> {
    let corpus = mango_corpus();
    expected
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| match entry {
            Value::String(id) => corpus.iter().find(|d| d["_id"] == *id).unwrap().clone(),
            doc => doc.clone(),
        })
        .collect()
}

async fn couch_find(url: &str, query: &Value) -> Result<Vec<Value>, String> {
    let mut body = query.clone();
    if body.get("limit").is_none() {
        body["limit"] = json!(1000);
    }
    let resp = reqwest::Client::new()
        .post(format!("{url}/_find"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body: Value = resp.json().await.unwrap();
    if !status.is_success() {
        return Err(format!("{status} {body}"));
    }
    Ok(body["docs"]
        .as_array()
        .unwrap()
        .iter()
        .map(without_rev)
        .collect())
}

async fn local_find(db: &Database, query: &Value) -> Result<Vec<Value>, String> {
    let opts: FindOptions = serde_json::from_value(query.clone()).map_err(|e| e.to_string())?;
    let res = db.find(opts).await.map_err(|e| e.to_string())?;
    Ok(res.docs.iter().map(without_rev).collect())
}

async fn create_local_indexes(db: &Database) {
    for (name, fields) in mango_indexes() {
        let created = db
            .create_index(IndexDefinition {
                name: name.into(),
                fields: fields
                    .into_iter()
                    .map(|f| SortField::Simple(f.into()))
                    .collect(),
                ddoc: None,
            })
            .await
            .unwrap();
        assert_eq!(created.result, "created");
    }
}

async fn create_couch_indexes(url: &str) {
    for (name, fields) in mango_indexes() {
        let resp = reqwest::Client::new()
            .post(format!("{url}/_index"))
            .json(&json!({"index": {"fields": fields}, "name": name, "type": "json"}))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "{}", resp.status());
    }
}

/// Run `cases` on a RouchDB database and describe every result that
/// differs from the expected one.
async fn local_mismatches(db: &Database, cases: &[(Value, Value)]) -> Vec<String> {
    let mut mismatches = Vec::new();
    for (query, expected) in cases {
        let expected = Ok(expected_docs(expected));
        let ours = local_find(db, query).await;
        if ours != expected {
            mismatches.push(format!(
                "{query}:\n  expected={expected:?}\n  rouchdb={ours:?}"
            ));
        }
    }
    mismatches
}

async fn run_mango_table_locally(db: &Database) -> Vec<String> {
    local_bulk_docs(db, &mango_corpus()).await;
    let mut mismatches = local_mismatches(db, &unindexed_queries()).await;
    for query in rejected_queries() {
        if let Ok(docs) = local_find(db, &query).await {
            mismatches.push(format!("{query}: accepted by rouchdb: {docs:?}"));
        }
    }
    create_local_indexes(db).await;
    mismatches.extend(local_mismatches(db, &indexed_queries()).await);
    mismatches
}

#[tokio::test]
async fn mango_table_matches_couchdb_results_locally() {
    let memory = Database::memory("mango_table");
    let mismatches = run_mango_table_locally(&memory).await;
    assert!(mismatches.is_empty(), "memory:\n{}", mismatches.join("\n"));

    let dir = tempfile::tempdir().unwrap();
    let redb = Database::open(dir.path().join("mango.redb"), "mango_table").unwrap();
    let mismatches = run_mango_table_locally(&redb).await;
    assert!(mismatches.is_empty(), "redb:\n{}", mismatches.join("\n"));
}

/// Run `cases` on CouchDB and describe every result that differs from the
/// expected one.
async fn couch_mismatches(url: &str, cases: &[(Value, Value)]) -> Vec<String> {
    let mut mismatches = Vec::new();
    for (query, expected) in cases {
        let expected = Ok(expected_docs(expected));
        let couch = couch_find(url, query).await;
        if couch != expected {
            mismatches.push(format!(
                "{query}:\n  expected={expected:?}\n  couchdb={couch:?}"
            ));
        }
    }
    mismatches
}

#[tokio::test]
#[ignore]
async fn mango_selectors_match_couchdb() {
    with_couch_db("b1q_parity_mango", |url| async move {
        couch_bulk_docs(&url, &mango_corpus()).await;
        let mut mismatches = couch_mismatches(&url, &unindexed_queries()).await;
        for query in rejected_queries() {
            let couch = couch_find(&url, &query).await;
            if !couch.as_ref().is_err_and(|e| e.starts_with("400")) {
                mismatches.push(format!("{query}: not rejected by couchdb: {couch:?}"));
            }
        }
        // Several of the unindexed queries make CouchDB fail (500) once
        // there is an index on their field, so the indexes come last.
        create_couch_indexes(&url).await;
        mismatches.extend(couch_mismatches(&url, &indexed_queries()).await);
        assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
    })
    .await;
}

// =========================================================================
// Views
// =========================================================================

fn view_docs() -> Vec<Value> {
    vec![
        json!({"_id": "a", "dept": "eng", "n": 30, "arr": [1, 2], "obj": {"x": 1, "y": 2}, "ref": "b"}),
        json!({"_id": "b", "dept": "sales", "n": 25, "arr": [3, 4], "obj": {"x": 2}}),
        json!({"_id": "c", "dept": "eng", "n": 35, "arr": [1, 0], "obj": {"z": 1}, "ref": "a"}),
        json!({"_id": "d", "dept": "hr", "n": 5, "arr": [0, 1], "obj": {"x": 0}, "ref": "zzz"}),
        // Keys of every shape, for group_level
        json!({"_id": "k1", "k": "str", "v": 1}),
        json!({"_id": "k2", "k": [], "v": 2}),
        json!({"_id": "k3", "k": {"a": 1}, "v": 3}),
        json!({"_id": "k4", "k": ["x", 1], "v": 4}),
        json!({"_id": "k5", "k": ["x", 2, 3], "v": 5}),
        json!({"_id": "k6", "k": ["x", 2, 4], "v": 6}),
        json!({"_id": "k7", "k": ["y"], "v": 7}),
        json!({"_id": "k8", "k": null, "v": 8}),
        json!({"_id": "k9", "k": ["x"], "v": 9}),
        json!({"_id": "k10", "k": 5, "v": 1.5}),
        json!({"_id": "k11", "k": 5, "v": 2.25}),
        // Arrays of different lengths (and a number) for _sum
        json!({"_id": "r1", "rag": [1, 2]}),
        json!({"_id": "r2", "rag": [3]}),
        json!({"_id": "r3", "rag": [4, 5, 6]}),
        json!({"_id": "r4", "rag": 7}),
    ]
}

fn view_design_docs() -> Vec<Value> {
    vec![
        json!({"_id": "_design/parity", "views": {
            "by_dept": {"map": "function(doc){ if (doc.dept) emit(doc.dept, doc.n); }", "reduce": "_sum"},
            "count": {"map": "function(doc){ emit(doc._id, 1); }", "reduce": "_count"},
            "stats": {"map": "function(doc){ if (doc.dept) emit(doc.dept, doc.n); }", "reduce": "_stats"},
            "arrs": {"map": "function(doc){ if (doc.arr) emit(doc.dept, doc.arr); }", "reduce": "_sum"},
            "objs": {"map": "function(doc){ if (doc.obj) emit(doc.dept, doc.obj); }", "reduce": "_sum"},
            "linked": {"map": "function(doc){ if (doc.ref) emit(doc._id, {_id: doc.ref}); }"},
            "linked_rev": {"map": "function(doc){ if (doc.ref_rev) emit(doc._id, {_id: doc.ref_id, _rev: doc.ref_rev}); }"},
            "custom": {"map": "function(doc){ if (doc.dept) emit([doc.dept, doc.n], 1); }",
                       "reduce": "function(keys, values, rereduce){ if (rereduce) return sum(values); return keys.length; }"},
            "mixed": {"map": "function(doc){ if (doc.v !== undefined) emit(doc.k, doc.v); }", "reduce": "_sum"},
            "mixed_stats": {"map": "function(doc){ if (doc.v !== undefined) emit(doc.k, doc.v); }", "reduce": "_stats"},
            "mixed_count": {"map": "function(doc){ if (doc.v !== undefined) emit(doc.k, doc.v); }", "reduce": "_count"}
        }}),
        // A reduce error can break every view of a design document in
        // CouchDB, so views with unusual reduce input get their own.
        json!({"_id": "_design/ragged", "views": {
            "ragged": {"map": "function(doc){ if (doc.rag !== undefined) emit(doc._id, doc.rag); }", "reduce": "_sum"}
        }}),
    ]
}

/// The view options for the CouchDB query parameters `params`.
fn view_options(params: &Value) -> ViewQueryOptions {
    let mut opts = ViewQueryOptions::new();
    for (name, value) in params.as_object().unwrap() {
        let flag = || value.as_bool().unwrap();
        match name.as_str() {
            "key" => opts.key = Some(value.clone()),
            "keys" => opts.keys = Some(value.as_array().unwrap().clone()),
            "startkey" => opts.start_key = Some(value.clone()),
            "endkey" => opts.end_key = Some(value.clone()),
            "inclusive_end" => opts.inclusive_end = flag(),
            "descending" => opts.descending = flag(),
            "skip" => opts.skip = value.as_u64().unwrap(),
            "limit" => opts.limit = Some(value.as_u64().unwrap()),
            "include_docs" => opts.include_docs = flag(),
            "reduce" => opts.reduce = flag(),
            "group" => opts.group = flag(),
            "group_level" => opts.group_level = Some(value.as_u64().unwrap()),
            other => panic!("unsupported view parameter {other}"),
        }
    }
    opts
}

/// Query a CouchDB view, JSON-encoding every parameter.
async fn couch_view(url: &str, ddoc: &str, view: &str, params: &Value) -> Value {
    let query: Vec<(String, String)> = params
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.to_string()))
        .collect();
    let view_url =
        reqwest::Url::parse_with_params(&format!("{url}/_design/{ddoc}/_view/{view}"), &query)
            .unwrap();
    reqwest::get(view_url).await.unwrap().json().await.unwrap()
}

/// A view row as CouchDB returns it. Revisions differ between CouchDB and
/// RouchDB, so `_rev` is dropped from documents and linked-document values.
fn row_json(id: Option<&str>, key: &Value, value: &Value, doc: Option<&Value>) -> Value {
    let mut row = json!({"key": key, "value": without_rev(value)});
    if let Some(id) = id {
        row["id"] = json!(id);
    }
    if let Some(doc) = doc.filter(|d| !d.is_null()) {
        row["doc"] = without_rev(doc);
    }
    row
}

fn local_rows(result: &rouchdb::ViewResult) -> Value {
    result
        .rows
        .iter()
        .map(|r| row_json(r.id.as_deref(), &r.key, &r.value, r.doc.as_ref()))
        .collect()
}

fn couch_rows(response: &Value) -> Value {
    response["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| row_json(r["id"].as_str(), &r["key"], &r["value"], r.get("doc")))
        .collect()
}

type MapFn = Box<dyn Fn(&Value) -> Vec<(Value, Value)>>;

/// Emit `(doc[key], doc[value])` for the documents that have `field`, like
/// the `if (doc.<field>) emit(...)` maps of `view_design_docs`.
fn map_if(field: &'static str, key: &'static str, value: &'static str) -> MapFn {
    Box::new(move |doc| match doc.get(field) {
        Some(_) => vec![(doc[key].clone(), doc[value].clone())],
        None => vec![],
    })
}

/// The Rust version of a view of `view_design_docs`: its design document,
/// map function and reduce function.
fn local_view(view: &str) -> (&'static str, MapFn, Option<ReduceFn>) {
    match view {
        "by_dept" => ("parity", map_if("dept", "dept", "n"), Some(ReduceFn::Sum)),
        "stats" => ("parity", map_if("dept", "dept", "n"), Some(ReduceFn::Stats)),
        "count" => (
            "parity",
            Box::new(|doc| vec![(doc["_id"].clone(), json!(1))]),
            Some(ReduceFn::Count),
        ),
        "arrs" => ("parity", map_if("arr", "dept", "arr"), Some(ReduceFn::Sum)),
        "objs" => ("parity", map_if("obj", "dept", "obj"), Some(ReduceFn::Sum)),
        "linked" => (
            "parity",
            Box::new(|doc| match doc.get("ref") {
                Some(r) => vec![(doc["_id"].clone(), json!({"_id": r}))],
                None => vec![],
            }),
            None,
        ),
        "linked_rev" => (
            "parity",
            Box::new(|doc| match doc.get("ref_rev") {
                Some(rev) => vec![(
                    doc["_id"].clone(),
                    json!({"_id": doc["ref_id"], "_rev": rev}),
                )],
                None => vec![],
            }),
            None,
        ),
        "custom" => (
            "parity",
            Box::new(|doc| match doc.get("dept") {
                Some(d) => vec![(json!([d, doc["n"]]), json!(1))],
                None => vec![],
            }),
            // CouchDB passes [key, docid] pairs to a custom reduce.
            Some(ReduceFn::Custom(Box::new(|keys, values, rereduce| {
                if rereduce {
                    json!(values.iter().filter_map(Value::as_u64).sum::<u64>())
                } else {
                    assert!(
                        keys.iter()
                            .all(|k| k.as_array().is_some_and(|p| p.len() == 2))
                    );
                    json!(keys.len())
                }
            }))),
        ),
        "mixed" => ("parity", map_if("v", "k", "v"), Some(ReduceFn::Sum)),
        "mixed_stats" => ("parity", map_if("v", "k", "v"), Some(ReduceFn::Stats)),
        "mixed_count" => ("parity", map_if("v", "k", "v"), Some(ReduceFn::Count)),
        "ragged" => ("ragged", map_if("rag", "_id", "rag"), Some(ReduceFn::Sum)),
        other => panic!("unknown view {other}"),
    }
}

/// `(view, CouchDB query parameters)`.
fn view_queries() -> Vec<(&'static str, Value)> {
    vec![
        // F53: total_rows / offset
        ("by_dept", json!({"reduce": false, "key": "eng"})),
        ("by_dept", json!({"reduce": false, "startkey": "hr"})),
        (
            "by_dept",
            json!({"reduce": false, "startkey": "hr", "descending": true}),
        ),
        ("by_dept", json!({"reduce": false, "skip": 1, "limit": 2})),
        (
            "by_dept",
            json!({"reduce": false, "startkey": "hr", "skip": 1}),
        ),
        (
            "by_dept",
            json!({"reduce": false, "startkey": "hr", "skip": 1, "descending": true}),
        ),
        ("by_dept", json!({"reduce": false, "limit": 0})),
        // endkey and inclusive_end, ascending and descending
        ("by_dept", json!({"reduce": false, "endkey": "hr"})),
        (
            "by_dept",
            json!({"reduce": false, "endkey": "hr", "inclusive_end": false}),
        ),
        (
            "by_dept",
            json!({"reduce": false, "startkey": "sales", "endkey": "eng",
                           "descending": true, "inclusive_end": false}),
        ),
        (
            "by_dept",
            json!({"reduce": false, "endkey": "hr", "descending": true,
                           "inclusive_end": false}),
        ),
        // keys: rows in the order of the keys (reversed when descending),
        // offset of the first returned row
        ("by_dept", json!({"reduce": false, "keys": ["hr", "eng"]})),
        (
            "by_dept",
            json!({"reduce": false, "keys": ["hr", "eng"], "descending": true}),
        ),
        (
            "by_dept",
            json!({"reduce": false, "keys": ["sales", "eng", "zzz"], "descending": true}),
        ),
        (
            "by_dept",
            json!({"reduce": false, "keys": ["sales"], "descending": true}),
        ),
        ("by_dept", json!({"reduce": false, "keys": ["zzz"]})),
        (
            "by_dept",
            json!({"reduce": false, "keys": ["hr", "eng"], "skip": 1, "limit": 1}),
        ),
        (
            "by_dept",
            json!({"reduce": false, "keys": ["eng", "hr", "eng"]}),
        ),
        (
            "by_dept",
            json!({"reduce": false, "keys": ["eng", "hr", "eng"], "descending": true}),
        ),
        (
            "by_dept",
            json!({"group": true, "keys": ["hr", "eng", "hr"]}),
        ),
        // F104: reduce is on by default when a reduce function is given
        ("by_dept", json!({})),
        ("by_dept", json!({"descending": true})),
        ("by_dept", json!({"limit": 0})),
        ("by_dept", json!({"startkey": "f", "endkey": "z"})),
        ("by_dept", json!({"group": true})),
        ("by_dept", json!({"group": true, "descending": true})),
        ("by_dept", json!({"group": true, "skip": 1, "limit": 1})),
        (
            "by_dept",
            json!({"group": true, "descending": true, "skip": 1}),
        ),
        ("by_dept", json!({"group": true, "startkey": "hr"})),
        (
            "by_dept",
            json!({"group": true, "startkey": "hr", "descending": true}),
        ),
        // F50: design documents are not mapped
        ("count", json!({})),
        ("count", json!({"reduce": false})),
        // F54: _sum / _stats keep integers and handle arrays and objects
        ("stats", json!({})),
        ("stats", json!({"group": true})),
        ("arrs", json!({})),
        ("objs", json!({})),
        ("ragged", json!({})),
        ("ragged", json!({"startkey": "r2"})),
        ("ragged", json!({"endkey": "r2"})),
        ("ragged", json!({"group": true, "keys": ["r1", "r2", "r3"]})),
        // group_level with keys that are not arrays, [] and objects
        ("mixed", json!({"reduce": false})),
        ("mixed", json!({"group_level": 0})),
        ("mixed", json!({"group_level": 1})),
        ("mixed", json!({"group_level": 2})),
        ("mixed", json!({"group_level": 1, "descending": true})),
        (
            "mixed",
            json!({"group_level": 1, "startkey": ["x"], "endkey": ["x", {}]}),
        ),
        ("mixed", json!({"group": true})),
        ("mixed", json!({"group": false})),
        ("mixed", json!({"group": true, "descending": true})),
        ("mixed", json!({"group": true, "skip": 2, "limit": 3})),
        ("mixed_stats", json!({"group_level": 1})),
        ("mixed_stats", json!({"key": 5})),
        ("mixed_count", json!({"group_level": 1})),
        // F16: include_docs, including linked documents and revisions
        (
            "by_dept",
            json!({"reduce": false, "include_docs": true, "limit": 2}),
        ),
        ("linked", json!({"include_docs": true})),
        ("linked_rev", json!({"include_docs": true})),
        // F105: custom reduce receives [key, id] pairs
        ("custom", json!({"group_level": 1})),
        // Rejected by CouchDB with a 400
        ("by_dept", json!({"include_docs": true})),
        ("by_dept", json!({"keys": ["hr", "eng"]})),
        ("by_dept", json!({"keys": ["hr", "eng"], "group_level": 0})),
    ]
}

/// Write `t` twice, then documents whose `linked_rev` rows point at the
/// first revision of `t` and at a revision that does not exist.
async fn couch_linked_revisions(url: &str) {
    let client = reqwest::Client::new();
    let put = |id: &str, body: Value| {
        let request = client.put(format!("{url}/{id}")).json(&body);
        async move {
            let resp: Value = request.send().await.unwrap().json().await.unwrap();
            resp["rev"].as_str().unwrap().to_string()
        }
    };
    let first = put("t", json!({"gen": 1})).await;
    put("t", json!({"_rev": first, "gen": 2})).await;
    put("lr1", json!({"ref_id": "t", "ref_rev": first})).await;
    put(
        "lr2",
        json!({"ref_id": "t", "ref_rev": "1-00000000000000000000000000000000"}),
    )
    .await;
}

async fn local_linked_revisions(db: &Database) {
    let first = db.put("t", json!({"gen": 1})).await.unwrap().rev.unwrap();
    db.update("t", &first, json!({"gen": 2})).await.unwrap();
    db.put("lr1", json!({"ref_id": "t", "ref_rev": first}))
        .await
        .unwrap();
    db.put(
        "lr2",
        json!({"ref_id": "t", "ref_rev": "1-00000000000000000000000000000000"}),
    )
    .await
    .unwrap();
}

#[tokio::test]
#[ignore]
async fn views_match_couchdb() {
    with_couch_db("b1q_parity_views", |url| async move {
        let local = Database::memory("local");
        let mut docs = view_docs();
        docs.extend(view_design_docs());
        couch_bulk_docs(&url, &docs).await;
        local_bulk_docs(&local, &docs).await;
        couch_linked_revisions(&url).await;
        local_linked_revisions(&local).await;

        let mut mismatches = Vec::new();
        for (view, params) in view_queries() {
            let (ddoc, map_fn, reduce) = local_view(view);
            let couch = couch_view(&url, ddoc, view, &params).await;
            let ours = query_view(
                local.adapter(),
                &map_fn,
                reduce.as_ref(),
                view_options(&params),
            )
            .await;
            // Option combinations CouchDB rejects must be rejected too.
            let same = match (&ours, couch.get("error")) {
                (Ok(ours), None) => {
                    local_rows(ours) == couch_rows(&couch)
                        && (couch.get("total_rows").is_none()
                            || (couch["total_rows"] == json!(ours.total_rows)
                                && couch["offset"] == json!(ours.offset)))
                }
                (Err(_), Some(_)) => true,
                _ => false,
            };
            if !same {
                let ours = ours.map(|r| {
                    format!(
                        "total_rows {} offset {} rows {}",
                        r.total_rows,
                        r.offset,
                        local_rows(&r)
                    )
                });
                mismatches.push(format!(
                    "{view} {params}:\n  couchdb={couch}\n  rouchdb={ours:?}"
                ));
            }
        }

        assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn http_database_runs_mango_on_couchdb() {
    // F48: find/create_index on Database::http must use CouchDB's _find and
    // _index instead of downloading every document.
    with_couch_db("b1q_remote_mango", |url| async move {
        let remote = Database::http(&url);
        for name in ["apple", "Banana", "cherry"] {
            remote.put(name, json!({"name": name})).await.unwrap();
        }
        for i in 0..30 {
            remote
                .put(&format!("n{i:02}"), json!({"n": i}))
                .await
                .unwrap();
        }

        let created = remote
            .create_index(rouchdb::IndexDefinition {
                name: String::new(),
                fields: vec![rouchdb::SortField::Simple("name".into())],
                ddoc: None,
            })
            .await
            .unwrap();
        assert_eq!(created.result, "created");
        assert_eq!(created.name, "idx-name");
        // The index exists on the server.
        let server: Value = reqwest::get(format!("{url}/_index"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            server["indexes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["name"] == "idx-name")
        );
        let indexes = remote.get_indexes().await;
        assert_eq!(indexes.len(), 1);
        assert_eq!(indexes[0].name, "idx-name");
        let plan = remote
            .explain(FindOptions {
                selector: json!({"name": {"$gt": null}}),
                ..Default::default()
            })
            .await;
        assert_eq!(plan.index.name, "idx-name");

        // CouchDB sorts with ICU collation ("apple" < "Banana"), which only
        // happens if the query ran on the server.
        let sorted = remote
            .find(FindOptions {
                selector: json!({"name": {"$gt": null}}),
                sort: Some(vec![rouchdb::SortField::Simple("name".into())]),
                fields: Some(vec!["name".into()]),
                ..Default::default()
            })
            .await
            .unwrap();
        let names: Vec<_> = sorted.docs.iter().map(|d| d["name"].clone()).collect();
        assert_eq!(names, [json!("apple"), json!("Banana"), json!("cherry")]);

        // No limit means every match, not CouchDB's default of 25.
        let all = remote
            .find(FindOptions {
                selector: json!({"n": {"$gte": 0}}),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(all.docs.len(), 30);
        let page = remote
            .find(FindOptions {
                selector: json!({"n": {"$gte": 0}}),
                skip: Some(5),
                limit: Some(3),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page.docs.len(), 3);

        // A sort CouchDB cannot serve without an index still works.
        let by_n = remote
            .find(FindOptions {
                selector: json!({"n": {"$gte": 28}}),
                sort: Some(vec![rouchdb::SortField::WithDirection(
                    [("n".to_string(), "desc".to_string())].into(),
                )]),
                ..Default::default()
            })
            .await
            .unwrap();
        let ns: Vec<_> = by_n.docs.iter().map(|d| d["n"].clone()).collect();
        assert_eq!(ns, [json!(29), json!(28)]);

        // Invalid selectors are reported.
        assert!(
            remote
                .find(FindOptions {
                    selector: json!({"n": {"$foo": 1}}),
                    ..Default::default()
                })
                .await
                .is_err()
        );

        remote.delete_index("idx-name").await.unwrap();
        assert!(remote.get_indexes().await.is_empty());
        assert!(remote.delete_index("idx-name").await.is_err());
    })
    .await;
}
