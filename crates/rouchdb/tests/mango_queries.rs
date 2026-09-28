//! Mango query operator coverage (equality, comparison, logical, arrays,
//! sort/skip/limit/fields) and Mango indexes, on the memory adapter.
//!
//! Without an index, `find` returns documents in `_id` order, like CouchDB
//! scanning `_all_docs`; with an index, in index order.

use std::collections::HashMap;

use rouchdb::{BulkDocsOptions, Database, Document, FindOptions, IndexDefinition, SortField};
use serde_json::{Value, json};

async fn db_with(docs: Vec<Value>) -> Database {
    let db = Database::memory("mango");
    for doc in docs {
        let id = doc["_id"].as_str().unwrap().to_string();
        db.put(&id, doc).await.unwrap();
    }
    db
}

fn ids(docs: &[Value]) -> Vec<&str> {
    docs.iter().map(|d| d["_id"].as_str().unwrap()).collect()
}

async fn find(db: &Database, opts: FindOptions) -> Vec<Value> {
    db.find(opts).await.unwrap().docs
}

/// Ids of the documents matching `selector`.
async fn find_ids(db: &Database, selector: Value) -> Vec<String> {
    let docs = find(
        db,
        FindOptions {
            selector,
            ..Default::default()
        },
    )
    .await;
    ids(&docs).into_iter().map(String::from).collect()
}

fn desc(field: &str) -> SortField {
    SortField::WithDirection(HashMap::from([(field.to_string(), "desc".to_string())]))
}

fn index_on(name: &str, fields: &[&str]) -> IndexDefinition {
    IndexDefinition {
        name: name.into(),
        fields: fields
            .iter()
            .map(|f| SortField::Simple(f.to_string()))
            .collect(),
        ddoc: None,
    }
}

#[tokio::test]
async fn mango_equality_and_inequality() {
    let db = db_with(vec![
        json!({"_id": "a", "name": "Alice", "age": 30}),
        json!({"_id": "b", "name": "Bob", "age": 25}),
        json!({"_id": "c", "name": "Charlie", "age": 30}),
    ])
    .await;

    assert_eq!(find_ids(&db, json!({"age": {"$eq": 30}})).await, ["a", "c"]);
    assert_eq!(find_ids(&db, json!({"name": "Bob"})).await, ["b"]);
    assert_eq!(find_ids(&db, json!({"age": {"$ne": 30}})).await, ["b"]);
    assert_eq!(find_ids(&db, json!({"age": 31})).await, [] as [&str; 0]);
}

#[tokio::test]
async fn mango_comparison_operators() {
    let db = db_with(vec![
        json!({"_id": "a", "score": 10}),
        json!({"_id": "b", "score": 20}),
        json!({"_id": "c", "score": 30}),
        json!({"_id": "d", "score": 40}),
    ])
    .await;

    assert_eq!(
        find_ids(&db, json!({"score": {"$gt": 20}})).await,
        ["c", "d"]
    );
    assert_eq!(
        find_ids(&db, json!({"score": {"$gte": 20}})).await,
        ["b", "c", "d"]
    );
    assert_eq!(
        find_ids(&db, json!({"score": {"$lt": 30}})).await,
        ["a", "b"]
    );
    assert_eq!(
        find_ids(&db, json!({"score": {"$lte": 30}})).await,
        ["a", "b", "c"]
    );
    assert_eq!(
        find_ids(&db, json!({"score": {"$gte": 20, "$lt": 40}})).await,
        ["b", "c"]
    );
}

#[tokio::test]
async fn mango_in_nin_exists() {
    let db = db_with(vec![
        json!({"_id": "a", "color": "red", "size": 10}),
        json!({"_id": "b", "color": "blue", "size": 20}),
        json!({"_id": "c", "color": "green"}),
    ])
    .await;

    assert_eq!(
        find_ids(&db, json!({"color": {"$in": ["red", "blue"]}})).await,
        ["a", "b"]
    );
    assert_eq!(
        find_ids(&db, json!({"color": {"$nin": ["red"]}})).await,
        ["b", "c"]
    );
    assert_eq!(
        find_ids(&db, json!({"size": {"$exists": true}})).await,
        ["a", "b"]
    );
    assert_eq!(
        find_ids(&db, json!({"size": {"$exists": false}})).await,
        ["c"]
    );
    // A missing field matches no $nin.
    assert_eq!(find_ids(&db, json!({"size": {"$nin": [10]}})).await, ["b"]);
}

#[tokio::test]
async fn mango_logical_operators() {
    let db = db_with(vec![
        json!({"_id": "a", "x": 1, "y": "a"}),
        json!({"_id": "b", "x": 2, "y": "b"}),
        json!({"_id": "c", "x": 3, "y": "a"}),
    ])
    .await;

    assert_eq!(
        find_ids(&db, json!({"$or": [{"x": 1}, {"x": 3}]})).await,
        ["a", "c"]
    );
    assert_eq!(
        find_ids(&db, json!({"$and": [{"y": "a"}, {"x": {"$gt": 1}}]})).await,
        ["c"]
    );
    assert_eq!(
        find_ids(&db, json!({"x": {"$not": {"$eq": 2}}})).await,
        ["a", "c"]
    );
    assert_eq!(
        find_ids(&db, json!({"$nor": [{"x": 1}, {"x": 2}]})).await,
        ["c"]
    );
}

#[tokio::test]
async fn mango_nested_field_query() {
    let db = db_with(vec![
        json!({"_id": "a", "address": {"city": "NYC", "state": "NY"}}),
        json!({"_id": "b", "address": {"city": "LA", "state": "CA"}}),
        json!({"_id": "c", "address": {"city": "SF", "state": "CA"}}),
    ])
    .await;

    assert_eq!(
        find_ids(&db, json!({"address.state": "CA"})).await,
        ["b", "c"]
    );
    assert_eq!(find_ids(&db, json!({"address.city": "NYC"})).await, ["a"]);
    // A sub-document selector is the same as the dotted path.
    assert_eq!(
        find_ids(&db, json!({"address": {"state": "CA", "city": "SF"}})).await,
        ["c"]
    );
}

#[tokio::test]
async fn mango_regex_and_type() {
    let db = db_with(vec![
        json!({"_id": "a", "email": "alice@example.com"}),
        json!({"_id": "b", "email": "bob@test.org"}),
        json!({"_id": "c", "email": 12345}),
    ])
    .await;

    assert_eq!(
        find_ids(&db, json!({"email": {"$regex": ".*@example\\.com$"}})).await,
        ["a"]
    );
    assert_eq!(
        find_ids(&db, json!({"email": {"$type": "string"}})).await,
        ["a", "b"]
    );
    assert_eq!(
        find_ids(&db, json!({"email": {"$type": "number"}})).await,
        ["c"]
    );
}

#[tokio::test]
async fn mango_array_operators() {
    let db = db_with(vec![
        json!({"_id": "a", "tags": ["rust", "db"]}),
        json!({"_id": "b", "tags": ["python", "web", "db"]}),
        json!({"_id": "c", "tags": ["rust", "web", "db"]}),
    ])
    .await;

    assert_eq!(
        find_ids(&db, json!({"tags": {"$all": ["rust", "db"]}})).await,
        ["a", "c"]
    );
    assert_eq!(
        find_ids(&db, json!({"tags": {"$size": 3}})).await,
        ["b", "c"]
    );
    assert_eq!(find_ids(&db, json!({"tags": {"$size": 2}})).await, ["a"]);
    assert_eq!(
        find_ids(&db, json!({"tags": {"$elemMatch": {"$eq": "python"}}})).await,
        ["b"]
    );
}

#[tokio::test]
async fn mango_sort_skip_limit_projection() {
    let db = db_with(vec![
        json!({"_id": "a", "name": "Alice", "age": 30, "city": "NYC"}),
        json!({"_id": "b", "name": "Bob", "age": 25, "city": "LA"}),
        json!({"_id": "c", "name": "Charlie", "age": 35, "city": "SF"}),
        json!({"_id": "d", "name": "Diana", "age": 28, "city": "NYC"}),
    ])
    .await;
    let sorted = |sort: SortField, skip, limit| FindOptions {
        selector: json!({}),
        sort: Some(vec![sort]),
        skip,
        limit,
        ..Default::default()
    };

    let asc = find(&db, sorted(SortField::Simple("age".into()), None, None)).await;
    assert_eq!(ids(&asc), ["b", "d", "a", "c"]);
    let descending = find(&db, sorted(desc("age"), None, None)).await;
    assert_eq!(ids(&descending), ["c", "a", "d", "b"]);
    let page = find(
        &db,
        sorted(SortField::Simple("age".into()), Some(1), Some(2)),
    )
    .await;
    assert_eq!(ids(&page), ["d", "a"]);
    // Ties keep _id order (as a CouchDB index on city would).
    let by_city = find(
        &db,
        FindOptions {
            selector: json!({}),
            sort: Some(vec![SortField::Simple("city".into())]),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(ids(&by_city), ["b", "a", "d", "c"]);

    // Only the requested fields come back (no implicit _id).
    let projected = find(
        &db,
        FindOptions {
            selector: json!({"name": "Alice"}),
            fields: Some(vec!["name".into(), "age".into()]),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(projected, [json!({"name": "Alice", "age": 30})]);
}

#[tokio::test]
async fn mango_empty_selector_matches_all() {
    let db = db_with(vec![
        json!({"_id": "b", "v": 2}),
        json!({"_id": "a", "v": 1}),
        json!({"_id": "c", "v": 3}),
    ])
    .await;

    assert_eq!(find_ids(&db, json!({})).await, ["a", "b", "c"]);
}

// =========================================================================
// Mango indexes
// =========================================================================

#[tokio::test]
async fn mango_create_index_and_query() {
    let db = db_with(vec![
        json!({"_id": "a", "name": "Alice", "age": 30}),
        json!({"_id": "b", "name": "Bob", "age": 25}),
        json!({"_id": "c", "name": "Charlie", "age": 35}),
        json!({"_id": "d", "name": "Diana", "age": 28}),
    ])
    .await;

    let created = db
        .create_index(IndexDefinition {
            name: String::new(),
            fields: vec![SortField::Simple("age".into())],
            ddoc: None,
        })
        .await
        .unwrap();
    assert_eq!(
        (created.result.as_str(), created.name.as_str()),
        ("created", "idx-age")
    );

    let selector = json!({"age": {"$gte": 28}});
    let plan = db
        .explain(FindOptions {
            selector: selector.clone(),
            ..Default::default()
        })
        .await;
    assert_eq!(plan.index.name, "idx-age");
    // Index order: by age.
    assert_eq!(find_ids(&db, selector).await, ["d", "a", "c"]);

    let indexes = db.get_indexes().await;
    assert_eq!(indexes.len(), 1);
    assert_eq!(indexes[0].name, "idx-age");

    let again = db
        .create_index(index_on("idx-age", &["age"]))
        .await
        .unwrap();
    assert_eq!(again.result, "exists");

    db.delete_index("idx-age").await.unwrap();
    assert!(db.get_indexes().await.is_empty());
    assert!(matches!(
        db.delete_index("nonexistent").await,
        Err(rouchdb::RouchError::NotFound(_))
    ));
    // Without the index: _id order.
    assert_eq!(
        find_ids(&db, json!({"age": {"$gte": 28}})).await,
        ["a", "c", "d"]
    );
}

#[tokio::test]
async fn mango_index_with_sort_and_limit() {
    let db = db_with(
        (0..20)
            .map(|i| json!({"_id": format!("doc{i:02}"), "score": i * 5, "label": format!("item{i}")}))
            .collect(),
    )
    .await;
    db.create_index(index_on("", &["score"])).await.unwrap();

    let found = find(
        &db,
        FindOptions {
            selector: json!({"score": {"$gte": 50}}),
            sort: Some(vec![SortField::Simple("score".into())]),
            skip: Some(2),
            limit: Some(3),
            ..Default::default()
        },
    )
    .await;
    // Skipping score=50 and 55.
    assert_eq!(ids(&found), ["doc12", "doc13", "doc14"]);
    let scores: Vec<_> = found.iter().map(|d| d["score"].clone()).collect();
    assert_eq!(scores, [json!(60), json!(65), json!(70)]);
}

#[tokio::test]
async fn mango_multi_field_index() {
    let db = db_with(vec![
        json!({"_id": "a", "type": "invoice", "amount": 200, "status": "paid"}),
        json!({"_id": "b", "type": "invoice", "amount": 100, "status": "pending"}),
        json!({"_id": "c", "type": "receipt", "amount": 150, "status": "paid"}),
    ])
    .await;
    db.create_index(index_on("idx-type-amount", &["type", "amount"]))
        .await
        .unwrap();

    // Index order: by type, then amount.
    assert_eq!(find_ids(&db, json!({"type": "invoice"})).await, ["b", "a"]);
    assert_eq!(
        find_ids(&db, json!({"type": {"$gt": "a"}, "status": "paid"})).await,
        ["a", "c"]
    );
}

// =========================================================================
// Incremental index vs full scan
// =========================================================================

/// Selectors that use the `by-age` index.
fn indexed_selectors() -> Vec<Value> {
    vec![
        json!({"age": {"$gte": null}}),
        json!({"age": {"$gt": 2}}),
        json!({"age": {"$gte": 2, "$lt": 5}}),
        json!({"age": {"$lte": 3}}),
        json!({"age": 3}),
        json!({"age": {"$eq": "x"}}),
        json!({"age": {"$in": [1, 3, "x"]}}),
        json!({"age": {"$exists": true}}),
        json!({"age": {"$ne": 3}}),
        json!({"age": {"$gt": null}, "tag": "t"}),
    ]
}

/// `find` through the index must return what a full scan returns: the
/// same documents in the same order when sorted, the same set otherwise
/// (unsorted results come in index order), and never a document twice.
async fn assert_index_matches_scan(db: &Database, step: &str) {
    for selector in indexed_selectors() {
        let plan = db
            .explain(FindOptions {
                selector: selector.clone(),
                ..Default::default()
            })
            .await;
        assert_eq!(plan.index.name, "by-age", "{step}: {selector}");
        for sort in [
            None,
            Some(vec![SortField::Simple("age".into())]),
            Some(vec![desc("age")]),
        ] {
            let opts = FindOptions {
                selector: selector.clone(),
                sort: sort.clone(),
                ..Default::default()
            };
            let mut indexed = db.find(opts.clone()).await.unwrap().docs;
            let mut scanned = rouchdb::find(db.adapter(), opts).await.unwrap().docs;
            let mut unique = ids(&indexed);
            unique.sort();
            unique.dedup();
            assert_eq!(
                unique.len(),
                indexed.len(),
                "{step}: {selector}: duplicates in {:?}",
                ids(&indexed)
            );
            if sort.is_none() {
                indexed.sort_by(|a, b| a["_id"].as_str().cmp(&b["_id"].as_str()));
                scanned.sort_by(|a, b| a["_id"].as_str().cmp(&b["_id"].as_str()));
            }
            assert_eq!(indexed, scanned, "{step}: {selector} sort={sort:?}");
        }
    }
}

async fn put(db: &Database, id: &str, body: Value) -> String {
    db.put(id, body).await.unwrap().rev.unwrap()
}

async fn update(db: &Database, id: &str, body: Value) -> String {
    let rev = db.get(id).await.unwrap().rev.unwrap().to_string();
    db.update(id, &rev, body).await.unwrap().rev.unwrap()
}

/// Write a revision as replication does (keeping `_rev` and `_revisions`).
async fn put_replicated(db: &Database, doc: Value) {
    let doc = Document::from_json(doc).unwrap();
    let result = db
        .bulk_docs(vec![doc], BulkDocsOptions::replication())
        .await
        .unwrap();
    assert!(result[0].ok, "{:?}", result[0]);
}

async fn run_index_differential(db: &Database) {
    db.create_index(index_on("by-age", &["age"])).await.unwrap();
    assert_index_matches_scan(db, "empty").await;

    for (id, body) in [
        ("a", json!({"age": 1, "tag": "t"})),
        ("b", json!({"age": 2})),
        ("c", json!({"age": "x"})),
        ("d", json!({"tag": "t"})),
        ("e", json!({"age": null})),
    ] {
        put(db, id, body).await;
    }
    assert_index_matches_scan(db, "initial").await;

    update(db, "a", json!({"age": 5, "tag": "t"})).await;
    assert_index_matches_scan(db, "a updated").await;

    update(db, "b", json!({"tag": "t"})).await;
    assert_index_matches_scan(db, "b loses its age").await;

    let rev = db.get("c").await.unwrap().rev.unwrap().to_string();
    db.remove("c", &rev).await.unwrap();
    assert_index_matches_scan(db, "c deleted").await;

    put(db, "c", json!({"age": 3})).await;
    assert_index_matches_scan(db, "c recreated").await;

    update(db, "a", json!({"age": 4})).await;
    update(db, "a", json!({"age": 3, "tag": "t"})).await;
    assert_index_matches_scan(db, "a updated twice between queries").await;

    // Conflicting revisions: the winner (highest hash) is indexed, and the
    // loser once the winner is deleted.
    let first = put(db, "f", json!({"age": 10})).await;
    let first_hash = first.split_once('-').unwrap().1.to_string();
    for (hash, age) in [("a".repeat(32), 11), ("f".repeat(32), 12)] {
        put_replicated(
            db,
            json!({"_id": "f", "_rev": format!("2-{hash}"), "age": age,
                   "_revisions": {"start": 2, "ids": [hash, first_hash]}}),
        )
        .await;
    }
    assert_eq!(db.get("f").await.unwrap().data["age"], 12);
    assert_index_matches_scan(db, "conflict").await;
    db.remove("f", &format!("2-{}", "f".repeat(32)))
        .await
        .unwrap();
    assert_eq!(db.get("f").await.unwrap().data["age"], 11);
    assert_index_matches_scan(db, "winning revision deleted").await;

    put(db, "_design/x", json!({"age": 3})).await;
    assert_index_matches_scan(db, "design document").await;

    for i in 0..20 {
        let age = match i % 4 {
            0 => json!(i % 7),
            1 => json!(f64::from(i) / 4.0),
            2 => json!([i % 3]),
            _ => json!(format!("s{}", i % 5)),
        };
        put(db, &format!("n{i:02}"), json!({"age": age})).await;
    }
    assert_index_matches_scan(db, "bulk").await;

    let rev = db.get("e").await.unwrap().rev.unwrap().to_string();
    db.purge("e", vec![rev]).await.unwrap();
    assert_index_matches_scan(db, "e purged").await;
}

#[tokio::test]
async fn indexed_find_matches_a_full_scan_after_every_write() {
    run_index_differential(&Database::memory("differential")).await;

    let dir = tempfile::tempdir().unwrap();
    let redb = Database::open(dir.path().join("differential.redb"), "differential").unwrap();
    run_index_differential(&redb).await;
}
