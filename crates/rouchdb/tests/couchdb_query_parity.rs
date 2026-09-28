//! Parity tables: run the same Mango selectors and view queries against a
//! real CouchDB and against RouchDB, and compare the results.
//!
//! Requires CouchDB (see `common`); run with `-- --ignored`.

mod common;

use common::{delete_remote_db, fresh_remote_db};
use rouchdb::{Database, FindOptions};
use serde_json::{Value, json};

/// Documents shared by the Mango parity table. String values stay ASCII
/// lowercase so that CouchDB's ICU collation agrees with RouchDB's.
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
        json!({"_id": "_design/foo", "views": {}}),
    ]
}

async fn couch_find(url: &str, selector: &Value) -> std::result::Result<Vec<String>, String> {
    let resp = reqwest::Client::new()
        .post(format!("{url}/_find"))
        .json(&json!({"selector": selector, "fields": ["_id"], "limit": 1000}))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body: Value = resp.json().await.unwrap();
    if !status.is_success() {
        return Err(body.to_string());
    }
    let mut ids: Vec<String> = body["docs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["_id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    Ok(ids)
}

async fn local_find(db: &Database, selector: &Value) -> std::result::Result<Vec<String>, String> {
    let res = db
        .find(FindOptions {
            selector: selector.clone(),
            ..Default::default()
        })
        .await
        .map_err(|e| e.to_string())?;
    let mut ids: Vec<String> = res
        .docs
        .iter()
        .map(|d| d["_id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    Ok(ids)
}

#[tokio::test]
#[ignore]
async fn mango_selectors_match_couchdb() {
    let url = fresh_remote_db("parity_mango").await;
    let local = Database::memory("local");
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{url}/_bulk_docs"))
        .json(&json!({"docs": mango_corpus()}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    for doc in mango_corpus() {
        let doc = rouchdb::Document::from_json(doc).unwrap();
        local
            .bulk_docs(vec![doc], rouchdb::BulkDocsOptions::new())
            .await
            .unwrap();
    }

    let selectors = vec![
        // F14: nested sub-document selectors
        json!({"address": {"city": "nyc"}}),
        json!({"address": {"$gt": null, "city": "nyc"}}),
        json!({"address": {}}),
        json!({"m": {"b": 1}}),
        // F15: combinators inside a field or $elemMatch
        json!({"age": {"$or": [{"$lt": 5}, {"$gt": 10}]}}),
        json!({"age": {"$and": [{"$gt": 1}, {"$lt": 5}]}}),
        json!({"items": {"$elemMatch": {"$or": [{"subject": "math"}, {"subject": "bio"}]}}}),
        json!({"scores": {"$elemMatch": {"$gt": 40, "$lt": 55}}}),
        // F43: $not over several operators
        json!({"x": {"$not": {"$gt": 5, "$lt": 10}}}),
        // F44: $in / $nin on arrays
        json!({"tags": {"$in": ["rust"]}}),
        json!({"tags": {"$nin": ["rust"]}}),
        json!({"tags": {"$in": [["js"]]}}),
        json!({"tags": {"$all": [["rust", "db"]]}}),
        // F100: missing fields and negations
        json!({"age": {"$ne": 20}}),
        json!({"age": {"$nin": [20]}}),
        json!({"$not": {"age": 20}}),
        json!({"$nor": [{"age": 20}]}),
        json!({"age": {"$not": {"$regex": "x"}}}),
        json!({"$not": {"age": {"$exists": true}}}),
        json!({"address.city.x": {"$exists": false}}),
        // F101: array indexes and escaped dots
        json!({"items.0.name": "x"}),
        json!({"a\\.b": 1}),
        json!({"a.b": 2}),
        // F99: $mod
        json!({"n": {"$mod": [-1, 0]}}),
        json!({"n": {"$mod": [3, 1]}}),
        json!({"v": {"$mod": [5, 0]}}),
        // F46: design documents are never returned
        json!({}),
        json!({"_id": {"$gt": null}}),
        json!({"views": {"$exists": false}}),
        // F79: -0.0 and exact integer/float comparison
        json!({"age": 0}),
        json!({"age": {"$lt": 0}}),
        json!({"big": {"$eq": 9_007_199_254_740_992.0}}),
        json!({"big": {"$gt": 9_007_199_254_740_992.0}}),
        // Other operators
        json!({"scores": {"$allMatch": {"$gt": 40}}}),
        json!({"address": {"$keyMapMatch": {"$eq": "zip"}}}),
        json!({"name": {"$beginsWith": "no"}}),
        json!({"age": {"$type": "number"}}),
        json!({"tags": {"$size": 2}}),
    ];

    let mut mismatches = Vec::new();
    for selector in &selectors {
        let couch = couch_find(&url, selector).await;
        let ours = local_find(&local, selector).await;
        if couch != ours {
            mismatches.push(format!("{selector}: couchdb={couch:?} rouchdb={ours:?}"));
        }
    }

    // Selectors CouchDB rejects must be rejected here too.
    for selector in [
        json!({"$gt": 1}),
        json!({"age": {"$foo": 1}}),
        json!({"s": {"$regex": "[a"}}),
        json!({"age": {"$in": 20}}),
        json!({"age": {"$size": -1}}),
        json!({"age": {"$exists": "yes"}}),
        json!({"$or": {"age": 3}}),
        json!({"a..b": 1}),
        json!({"age": {"$mod": [2.5, 1]}}),
    ] {
        assert!(couch_find(&url, &selector).await.is_err(), "{selector}");
        if local_find(&local, &selector).await.is_ok() {
            mismatches.push(format!("{selector}: accepted by rouchdb"));
        }
    }

    delete_remote_db(&url).await;
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}
