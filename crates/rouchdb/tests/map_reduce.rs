//! Map/reduce view queries on replicated data.

mod common;

use common::fresh_remote_db;
use rouchdb::{Database, ReduceFn, ViewQueryOptions, query_view};

#[tokio::test]
#[ignore]
async fn view_basic_map() {
    let url = fresh_remote_db("view_map").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    remote
        .put(
            "a",
            serde_json::json!({"type": "person", "name": "Alice", "age": 30}),
        )
        .await
        .unwrap();
    remote
        .put(
            "b",
            serde_json::json!({"type": "person", "name": "Bob", "age": 25}),
        )
        .await
        .unwrap();
    remote
        .put("c", serde_json::json!({"type": "city", "name": "NYC"}))
        .await
        .unwrap();

    local.replicate_from(&remote).await.unwrap();

    let map_fn = |doc: &serde_json::Value| -> Vec<(serde_json::Value, serde_json::Value)> {
        if doc.get("type").and_then(|t| t.as_str()) == Some("person") {
            vec![(doc["name"].clone(), doc["age"].clone())]
        } else {
            vec![]
        }
    };

    let results = query_view(local.adapter(), &map_fn, None, ViewQueryOptions::new())
        .await
        .unwrap();

    assert_eq!(results.rows.len(), 2);
    assert_eq!(results.rows[0].key, "Alice");
    assert_eq!(results.rows[0].value, 30);
    assert_eq!(results.rows[1].key, "Bob");
}

#[tokio::test]
#[ignore]
async fn view_reduce_sum_and_count() {
    let url = fresh_remote_db("view_reduce").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    remote
        .put("a", serde_json::json!({"dept": "eng", "salary": 100}))
        .await
        .unwrap();
    remote
        .put("b", serde_json::json!({"dept": "eng", "salary": 120}))
        .await
        .unwrap();
    remote
        .put("c", serde_json::json!({"dept": "sales", "salary": 90}))
        .await
        .unwrap();

    local.replicate_from(&remote).await.unwrap();

    let map_fn = |doc: &serde_json::Value| -> Vec<(serde_json::Value, serde_json::Value)> {
        vec![(doc["dept"].clone(), doc["salary"].clone())]
    };

    // Sum all salaries
    let results = query_view(
        local.adapter(),
        &map_fn,
        Some(&ReduceFn::Sum),
        ViewQueryOptions {
            reduce: true,
            ..ViewQueryOptions::new()
        },
    )
    .await
    .unwrap();

    assert_eq!(results.rows.len(), 1);
    assert_eq!(results.rows[0].value, 310.0);

    // Count
    let results = query_view(
        local.adapter(),
        &map_fn,
        Some(&ReduceFn::Count),
        ViewQueryOptions {
            reduce: true,
            ..ViewQueryOptions::new()
        },
    )
    .await
    .unwrap();
    assert_eq!(results.rows[0].value, 3);

    // Group by department
    let results = query_view(
        local.adapter(),
        &map_fn,
        Some(&ReduceFn::Sum),
        ViewQueryOptions {
            reduce: true,
            group: true,
            ..ViewQueryOptions::new()
        },
    )
    .await
    .unwrap();

    assert_eq!(results.rows.len(), 2);
    let eng = results.rows.iter().find(|r| r.key == "eng").unwrap();
    assert_eq!(eng.value, 220.0);
    let sales = results.rows.iter().find(|r| r.key == "sales").unwrap();
    assert_eq!(sales.value, 90.0);
}

#[tokio::test]
#[ignore]
async fn view_key_range() {
    let url = fresh_remote_db("view_range").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    for i in 0..10 {
        remote
            .put(&format!("d{}", i), serde_json::json!({"n": i}))
            .await
            .unwrap();
    }

    local.replicate_from(&remote).await.unwrap();

    let map_fn = |doc: &serde_json::Value| -> Vec<(serde_json::Value, serde_json::Value)> {
        vec![(doc["n"].clone(), serde_json::json!(1))]
    };

    let results = query_view(
        local.adapter(),
        &map_fn,
        None,
        ViewQueryOptions {
            start_key: Some(serde_json::json!(3)),
            end_key: Some(serde_json::json!(7)),
            ..ViewQueryOptions::new()
        },
    )
    .await
    .unwrap();

    assert_eq!(results.rows.len(), 5); // 3, 4, 5, 6, 7
}
