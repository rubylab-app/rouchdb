//! Mango query operator coverage: equality, comparison, logical, arrays, etc.

mod common;

use std::collections::HashMap;

use common::{delete_remote_db, fresh_remote_db};
use rouchdb::{Database, FindOptions, IndexDefinition, SortField};

#[tokio::test]
#[ignore]
async fn mango_equality_and_inequality() {
    let url = fresh_remote_db("mango_eq").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    remote
        .put("a", serde_json::json!({"name": "Alice", "age": 30}))
        .await
        .unwrap();
    remote
        .put("b", serde_json::json!({"name": "Bob", "age": 25}))
        .await
        .unwrap();
    remote
        .put("c", serde_json::json!({"name": "Charlie", "age": 30}))
        .await
        .unwrap();

    local.replicate_from(&remote).await.unwrap();

    // $eq
    let result = local
        .find(FindOptions {
            selector: serde_json::json!({"age": {"$eq": 30}}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(result.docs.len(), 2);

    // Implicit $eq
    let result = local
        .find(FindOptions {
            selector: serde_json::json!({"name": "Bob"}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(result.docs.len(), 1);
    assert_eq!(result.docs[0]["name"], "Bob");

    // $ne
    let result = local
        .find(FindOptions {
            selector: serde_json::json!({"age": {"$ne": 30}}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(result.docs.len(), 1);
    assert_eq!(result.docs[0]["name"], "Bob");

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn mango_comparison_operators() {
    let url = fresh_remote_db("mango_cmp").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    remote
        .put("a", serde_json::json!({"score": 10}))
        .await
        .unwrap();
    remote
        .put("b", serde_json::json!({"score": 20}))
        .await
        .unwrap();
    remote
        .put("c", serde_json::json!({"score": 30}))
        .await
        .unwrap();
    remote
        .put("d", serde_json::json!({"score": 40}))
        .await
        .unwrap();

    local.replicate_from(&remote).await.unwrap();

    let result = local
        .find(FindOptions {
            selector: serde_json::json!({"score": {"$gt": 20}}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(result.docs.len(), 2);

    let result = local
        .find(FindOptions {
            selector: serde_json::json!({"score": {"$gte": 20}}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(result.docs.len(), 3);

    let result = local
        .find(FindOptions {
            selector: serde_json::json!({"score": {"$lt": 30}}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(result.docs.len(), 2);

    let result = local
        .find(FindOptions {
            selector: serde_json::json!({"score": {"$gte": 20, "$lt": 40}}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(result.docs.len(), 2);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn mango_in_nin_exists() {
    let url = fresh_remote_db("mango_in").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    remote
        .put("a", serde_json::json!({"color": "red", "size": 10}))
        .await
        .unwrap();
    remote
        .put("b", serde_json::json!({"color": "blue", "size": 20}))
        .await
        .unwrap();
    remote
        .put("c", serde_json::json!({"color": "green"}))
        .await
        .unwrap();

    local.replicate_from(&remote).await.unwrap();

    let result = local
        .find(FindOptions {
            selector: serde_json::json!({"color": {"$in": ["red", "blue"]}}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(result.docs.len(), 2);

    let result = local
        .find(FindOptions {
            selector: serde_json::json!({"color": {"$nin": ["red"]}}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(result.docs.len(), 2);

    let result = local
        .find(FindOptions {
            selector: serde_json::json!({"size": {"$exists": true}}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(result.docs.len(), 2);

    let result = local
        .find(FindOptions {
            selector: serde_json::json!({"size": {"$exists": false}}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(result.docs.len(), 1);
    assert_eq!(result.docs[0]["color"], "green");

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn mango_logical_operators() {
    let url = fresh_remote_db("mango_logic").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    remote
        .put("a", serde_json::json!({"x": 1, "y": "a"}))
        .await
        .unwrap();
    remote
        .put("b", serde_json::json!({"x": 2, "y": "b"}))
        .await
        .unwrap();
    remote
        .put("c", serde_json::json!({"x": 3, "y": "a"}))
        .await
        .unwrap();

    local.replicate_from(&remote).await.unwrap();

    // $or
    let result = local
        .find(FindOptions {
            selector: serde_json::json!({"$or": [{"x": 1}, {"x": 3}]}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(result.docs.len(), 2);

    // $and
    let result = local
        .find(FindOptions {
            selector: serde_json::json!({"$and": [{"y": "a"}, {"x": {"$gt": 1}}]}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(result.docs.len(), 1);
    assert_eq!(result.docs[0]["x"], 3);

    // $not
    let result = local
        .find(FindOptions {
            selector: serde_json::json!({"x": {"$not": {"$eq": 2}}}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(result.docs.len(), 2);

    // $nor
    let result = local
        .find(FindOptions {
            selector: serde_json::json!({"$nor": [{"x": 1}, {"x": 2}]}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(result.docs.len(), 1);
    assert_eq!(result.docs[0]["x"], 3);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn mango_nested_field_query() {
    let url = fresh_remote_db("mango_nested").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    remote
        .put(
            "a",
            serde_json::json!({"address": {"city": "NYC", "state": "NY"}}),
        )
        .await
        .unwrap();
    remote
        .put(
            "b",
            serde_json::json!({"address": {"city": "LA", "state": "CA"}}),
        )
        .await
        .unwrap();
    remote
        .put(
            "c",
            serde_json::json!({"address": {"city": "SF", "state": "CA"}}),
        )
        .await
        .unwrap();

    local.replicate_from(&remote).await.unwrap();

    let result = local
        .find(FindOptions {
            selector: serde_json::json!({"address.state": "CA"}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(result.docs.len(), 2);

    let result = local
        .find(FindOptions {
            selector: serde_json::json!({"address.city": "NYC"}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(result.docs.len(), 1);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn mango_regex_and_type() {
    let url = fresh_remote_db("mango_regex").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    remote
        .put("a", serde_json::json!({"email": "alice@example.com"}))
        .await
        .unwrap();
    remote
        .put("b", serde_json::json!({"email": "bob@test.org"}))
        .await
        .unwrap();
    remote
        .put("c", serde_json::json!({"email": 12345}))
        .await
        .unwrap();

    local.replicate_from(&remote).await.unwrap();

    let result = local
        .find(FindOptions {
            selector: serde_json::json!({"email": {"$regex": ".*@example\\.com$"}}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(result.docs.len(), 1);
    assert_eq!(result.docs[0]["email"], "alice@example.com");

    let result = local
        .find(FindOptions {
            selector: serde_json::json!({"email": {"$type": "string"}}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(result.docs.len(), 2);

    let result = local
        .find(FindOptions {
            selector: serde_json::json!({"email": {"$type": "number"}}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(result.docs.len(), 1);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn mango_array_operators() {
    let url = fresh_remote_db("mango_arr").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    remote
        .put("a", serde_json::json!({"tags": ["rust", "db"]}))
        .await
        .unwrap();
    remote
        .put("b", serde_json::json!({"tags": ["python", "web", "db"]}))
        .await
        .unwrap();
    remote
        .put("c", serde_json::json!({"tags": ["rust", "web", "db"]}))
        .await
        .unwrap();

    local.replicate_from(&remote).await.unwrap();

    let result = local
        .find(FindOptions {
            selector: serde_json::json!({"tags": {"$all": ["rust", "db"]}}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(result.docs.len(), 2);

    let result = local
        .find(FindOptions {
            selector: serde_json::json!({"tags": {"$size": 3}}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(result.docs.len(), 2);

    let result = local
        .find(FindOptions {
            selector: serde_json::json!({"tags": {"$size": 2}}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(result.docs.len(), 1);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn mango_sort_skip_limit_projection() {
    let url = fresh_remote_db("mango_sort").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    remote
        .put(
            "a",
            serde_json::json!({"name": "Alice", "age": 30, "city": "NYC"}),
        )
        .await
        .unwrap();
    remote
        .put(
            "b",
            serde_json::json!({"name": "Bob", "age": 25, "city": "LA"}),
        )
        .await
        .unwrap();
    remote
        .put(
            "c",
            serde_json::json!({"name": "Charlie", "age": 35, "city": "SF"}),
        )
        .await
        .unwrap();
    remote
        .put(
            "d",
            serde_json::json!({"name": "Diana", "age": 28, "city": "NYC"}),
        )
        .await
        .unwrap();

    local.replicate_from(&remote).await.unwrap();

    // Sort ascending by age
    let result = local
        .find(FindOptions {
            selector: serde_json::json!({}),
            sort: Some(vec![SortField::Simple("age".into())]),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(result.docs[0]["name"], "Bob");
    assert_eq!(result.docs[3]["name"], "Charlie");

    // Sort descending
    let mut dir = HashMap::new();
    dir.insert("age".to_string(), "desc".to_string());
    let result = local
        .find(FindOptions {
            selector: serde_json::json!({}),
            sort: Some(vec![SortField::WithDirection(dir)]),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(result.docs[0]["name"], "Charlie");

    // Skip and limit
    let result = local
        .find(FindOptions {
            selector: serde_json::json!({}),
            sort: Some(vec![SortField::Simple("age".into())]),
            skip: Some(1),
            limit: Some(2),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(result.docs.len(), 2);
    assert_eq!(result.docs[0]["name"], "Diana");

    // Projection
    let result = local
        .find(FindOptions {
            selector: serde_json::json!({"name": "Alice"}),
            fields: Some(vec!["name".into(), "age".into()]),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(result.docs.len(), 1);
    assert_eq!(result.docs[0]["name"], "Alice");
    assert_eq!(result.docs[0]["age"], 30);
    assert!(result.docs[0].get("city").is_none());

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn mango_empty_selector_matches_all() {
    let url = fresh_remote_db("mango_empty").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    remote.put("a", serde_json::json!({"v": 1})).await.unwrap();
    remote.put("b", serde_json::json!({"v": 2})).await.unwrap();
    remote.put("c", serde_json::json!({"v": 3})).await.unwrap();

    local.replicate_from(&remote).await.unwrap();

    let result = local
        .find(FindOptions {
            selector: serde_json::json!({}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(result.docs.len(), 3);

    delete_remote_db(&url).await;
}

// =========================================================================
// Mango indexes
// =========================================================================

#[tokio::test]
#[ignore]
async fn mango_create_index_and_query() {
    let url = fresh_remote_db("mango_idx").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    remote
        .put("a", serde_json::json!({"name": "Alice", "age": 30}))
        .await
        .unwrap();
    remote
        .put("b", serde_json::json!({"name": "Bob", "age": 25}))
        .await
        .unwrap();
    remote
        .put("c", serde_json::json!({"name": "Charlie", "age": 35}))
        .await
        .unwrap();
    remote
        .put("d", serde_json::json!({"name": "Diana", "age": 28}))
        .await
        .unwrap();

    local.replicate_from(&remote).await.unwrap();

    // Create an index on "age"
    let idx_result = local
        .create_index(IndexDefinition {
            name: String::new(),
            fields: vec![SortField::Simple("age".into())],
            ddoc: None,
        })
        .await
        .unwrap();
    assert_eq!(idx_result.result, "created");
    assert_eq!(idx_result.name, "idx-age");

    // Query using the index
    let found = local
        .find(FindOptions {
            selector: serde_json::json!({"age": {"$gte": 30}}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(found.docs.len(), 2);

    // Check that we can get indexes
    let indexes = local.get_indexes().await;
    assert_eq!(indexes.len(), 1);
    assert_eq!(indexes[0].name, "idx-age");

    // Creating again returns "exists"
    let idx_result2 = local
        .create_index(IndexDefinition {
            name: "idx-age".into(),
            fields: vec![SortField::Simple("age".into())],
            ddoc: None,
        })
        .await
        .unwrap();
    assert_eq!(idx_result2.result, "exists");

    // Delete the index
    local.delete_index("idx-age").await.unwrap();
    let indexes = local.get_indexes().await;
    assert!(indexes.is_empty());

    // Deleting nonexistent returns error
    assert!(local.delete_index("nonexistent").await.is_err());

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn mango_index_with_sort_and_limit() {
    let url = fresh_remote_db("mango_idx_sort").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    for i in 0..20 {
        remote
            .put(
                &format!("doc{:02}", i),
                serde_json::json!({"score": i * 5, "label": format!("item{}", i)}),
            )
            .await
            .unwrap();
    }

    local.replicate_from(&remote).await.unwrap();

    // Create index on "score"
    local
        .create_index(IndexDefinition {
            name: String::new(),
            fields: vec![SortField::Simple("score".into())],
            ddoc: None,
        })
        .await
        .unwrap();

    // Query with sort, skip, limit using the index
    let found = local
        .find(FindOptions {
            selector: serde_json::json!({"score": {"$gte": 50}}),
            sort: Some(vec![SortField::Simple("score".into())]),
            skip: Some(2),
            limit: Some(3),
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(found.docs.len(), 3);
    // After skipping the first 2 matches (score=50, score=55), we get score=60,65,70
    assert_eq!(found.docs[0]["score"], 60);
    assert_eq!(found.docs[1]["score"], 65);
    assert_eq!(found.docs[2]["score"], 70);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn mango_multi_field_index() {
    let url = fresh_remote_db("mango_idx_multi").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    remote
        .put(
            "a",
            serde_json::json!({"type": "invoice", "amount": 100, "status": "paid"}),
        )
        .await
        .unwrap();
    remote
        .put(
            "b",
            serde_json::json!({"type": "invoice", "amount": 200, "status": "pending"}),
        )
        .await
        .unwrap();
    remote
        .put(
            "c",
            serde_json::json!({"type": "receipt", "amount": 150, "status": "paid"}),
        )
        .await
        .unwrap();

    local.replicate_from(&remote).await.unwrap();

    // Create index on ("type", "amount")
    local
        .create_index(IndexDefinition {
            name: "idx-type-amount".into(),
            fields: vec![
                SortField::Simple("type".into()),
                SortField::Simple("amount".into()),
            ],
            ddoc: None,
        })
        .await
        .unwrap();

    // Query that hits the first field of the index
    let found = local
        .find(FindOptions {
            selector: serde_json::json!({"type": "invoice"}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(found.docs.len(), 2);

    delete_remote_db(&url).await;
}
