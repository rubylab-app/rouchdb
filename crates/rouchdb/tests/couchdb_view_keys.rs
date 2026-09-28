//! View `keys` + grouping validation compared with a live CouchDB: with a
//! reduce, several keys need `group=true` without `group_level`, and a single
//! key behaves like `key`.
//!
//! Requires CouchDB (see `common`); run with `-- --ignored`.

mod common;

use common::{delete_remote_db, fresh_remote_db};
use rouchdb::{Database, ReduceFn, ViewQueryOptions, query_view};
use serde_json::{Value, json};

/// Encode a query-string value.
fn enc(value: &Value) -> String {
    value
        .to_string()
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[tokio::test]
#[ignore]
async fn view_keys_and_grouping_rules_match_couchdb() {
    let url = fresh_remote_db("c2_view_keys").await;
    let client = reqwest::Client::new();
    let docs = [
        json!({"_id": "a1", "p": "a"}),
        json!({"_id": "a2", "p": "a"}),
        json!({"_id": "b1", "p": "b"}),
    ];
    let resp = client
        .post(format!("{url}/_bulk_docs"))
        .json(&json!({ "docs": docs }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let resp = client
        .put(format!("{url}/_design/v"))
        .json(&json!({"views": {"by": {
            "map": "function(doc){ emit([doc.p, doc._id], 1); }",
            "reduce": "_count",
        }}}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    let local = Database::memory("local");
    for doc in &docs {
        local
            .put(doc["_id"].as_str().unwrap(), doc.clone())
            .await
            .unwrap();
    }
    let map = |doc: &Value| vec![(json!([doc["p"], doc["_id"]]), json!(1))];

    let two = json!([["a", "a1"], ["a", "a2"]]);
    let one = json!([["a", "a1"]]);
    let cases: Vec<(String, ViewQueryOptions)> = vec![
        (
            format!("keys={}&group_level=1", enc(&two)),
            ViewQueryOptions {
                keys: Some(vec![json!(["a", "a1"]), json!(["a", "a2"])]),
                group_level: Some(1),
                ..ViewQueryOptions::new()
            },
        ),
        (
            format!("keys={}&group=true&group_level=1", enc(&two)),
            ViewQueryOptions {
                keys: Some(vec![json!(["a", "a1"]), json!(["a", "a2"])]),
                group: true,
                group_level: Some(1),
                ..ViewQueryOptions::new()
            },
        ),
        (
            format!("keys={}", enc(&two)),
            ViewQueryOptions {
                keys: Some(vec![json!(["a", "a1"]), json!(["a", "a2"])]),
                ..ViewQueryOptions::new()
            },
        ),
        (
            format!("keys={}&group=true", enc(&two)),
            ViewQueryOptions {
                keys: Some(vec![json!(["a", "a1"]), json!(["a", "a2"])]),
                group: true,
                ..ViewQueryOptions::new()
            },
        ),
        (
            format!("keys={}&reduce=false", enc(&two)),
            ViewQueryOptions {
                keys: Some(vec![json!(["a", "a1"]), json!(["a", "a2"])]),
                reduce: false,
                ..ViewQueryOptions::new()
            },
        ),
        (
            format!("keys={}", enc(&one)),
            ViewQueryOptions {
                keys: Some(vec![json!(["a", "a1"])]),
                ..ViewQueryOptions::new()
            },
        ),
        (
            format!("keys={}&group_level=1", enc(&one)),
            ViewQueryOptions {
                keys: Some(vec![json!(["a", "a1"])]),
                group_level: Some(1),
                ..ViewQueryOptions::new()
            },
        ),
        (
            format!("keys={}&group=true", enc(&one)),
            ViewQueryOptions {
                keys: Some(vec![json!(["a", "a1"])]),
                group: true,
                ..ViewQueryOptions::new()
            },
        ),
    ];

    let mut mismatches = Vec::new();
    for (query, opts) in cases {
        let resp = client
            .get(format!("{url}/_design/v/_view/by?{query}"))
            .send()
            .await
            .unwrap();
        let ok = resp.status().is_success();
        let body: Value = resp.json().await.unwrap();
        let couch = if ok {
            Ok(body["rows"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| (r["key"].clone(), r["value"].clone()))
                .collect::<Vec<_>>())
        } else {
            Err(body["reason"].as_str().unwrap_or_default().to_string())
        };
        let ours = query_view(local.adapter(), &map, Some(&ReduceFn::Count), opts)
            .await
            .map(|r| {
                r.rows
                    .into_iter()
                    .map(|row| (row.key, row.value))
                    .collect::<Vec<_>>()
            })
            .map_err(|e| match e {
                rouchdb::RouchError::BadRequest(reason) => reason,
                other => other.to_string(),
            });
        if couch != ours {
            mismatches.push(format!(
                "{query}\n  couchdb: {couch:?}\n  rouchdb: {ours:?}"
            ));
        }
    }
    delete_remote_db(&url).await;
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}
