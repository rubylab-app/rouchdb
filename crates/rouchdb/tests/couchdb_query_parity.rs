//! Parity tables: run the same Mango selectors and view queries against a
//! real CouchDB and against RouchDB, and compare the results.
//!
//! Requires CouchDB (see `common`); run with `-- --ignored`.

mod common;

use common::fresh_remote_db;
use rouchdb::{Database, FindOptions, ReduceFn, ViewQueryOptions, query_view};
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

    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

/// Query a CouchDB view and return the response body.
async fn couch_view(url: &str, view: &str, query: &str) -> Value {
    reqwest::Client::new()
        .get(format!("{url}/_design/parity/_view/{view}?{query}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Revision hashes differ between CouchDB and RouchDB, so compare bodies.
fn without_rev(doc: &Value) -> Value {
    let mut doc = doc.clone();
    if let Some(obj) = doc.as_object_mut() {
        obj.remove("_rev");
    }
    doc
}

fn rows_json(result: &rouchdb::ViewResult) -> Value {
    Value::Array(
        result
            .rows
            .iter()
            .map(|r| {
                let mut row = json!({"key": r.key, "value": r.value});
                if let Some(ref id) = r.id {
                    row["id"] = json!(id);
                }
                if let Some(ref doc) = r.doc {
                    row["doc"] = without_rev(doc);
                }
                row
            })
            .collect(),
    )
}

#[tokio::test]
#[ignore]
async fn views_match_couchdb() {
    let url = fresh_remote_db("parity_views").await;
    let local = Database::memory("local");
    let docs = vec![
        json!({"_id": "a", "dept": "eng", "n": 30, "arr": [1, 2], "obj": {"x": 1, "y": 2}, "ref": "b"}),
        json!({"_id": "b", "dept": "sales", "n": 25, "arr": [3, 4], "obj": {"x": 2}}),
        json!({"_id": "c", "dept": "eng", "n": 35, "arr": [1, 0], "obj": {"z": 1}, "ref": "a"}),
        json!({"_id": "d", "dept": "hr", "n": 5, "arr": [0, 1], "obj": {"x": 0}, "ref": "zzz"}),
    ];
    let mut ddoc = json!({"_id": "_design/parity", "views": {
        "by_dept": {"map": "function(doc){ if (doc.dept) emit(doc.dept, doc.n); }", "reduce": "_sum"},
        "count": {"map": "function(doc){ emit(doc._id, 1); }", "reduce": "_count"},
        "stats": {"map": "function(doc){ if (doc.dept) emit(doc.dept, doc.n); }", "reduce": "_stats"},
        "arrs": {"map": "function(doc){ if (doc.arr) emit(doc.dept, doc.arr); }", "reduce": "_sum"},
        "objs": {"map": "function(doc){ if (doc.obj) emit(doc.dept, doc.obj); }", "reduce": "_sum"},
        "linked": {"map": "function(doc){ if (doc.ref) emit(doc._id, {_id: doc.ref}); }"},
        "custom": {"map": "function(doc){ if (doc.dept) emit([doc.dept, doc.n], 1); }",
                   "reduce": "function(keys, values, rereduce){ if (rereduce) return sum(values); return keys.length; }"}
    }});
    let client = reqwest::Client::new();
    let mut all = docs.clone();
    all.push(ddoc.take());
    let resp = client
        .post(format!("{url}/_bulk_docs"))
        .json(&json!({"docs": all}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    for doc in all {
        let doc = rouchdb::Document::from_json(doc).unwrap();
        local
            .bulk_docs(vec![doc], rouchdb::BulkDocsOptions::new())
            .await
            .unwrap();
    }

    let by_dept = |doc: &Value| -> Vec<(Value, Value)> {
        match doc.get("dept") {
            Some(d) => vec![(d.clone(), doc["n"].clone())],
            None => vec![],
        }
    };
    let by_id = |doc: &Value| -> Vec<(Value, Value)> { vec![(doc["_id"].clone(), json!(1))] };
    let arrs = |doc: &Value| -> Vec<(Value, Value)> {
        match doc.get("arr") {
            Some(a) => vec![(doc["dept"].clone(), a.clone())],
            None => vec![],
        }
    };
    let objs = |doc: &Value| -> Vec<(Value, Value)> {
        match doc.get("obj") {
            Some(o) => vec![(doc["dept"].clone(), o.clone())],
            None => vec![],
        }
    };
    let linked = |doc: &Value| -> Vec<(Value, Value)> {
        match doc.get("ref") {
            Some(r) => vec![(doc["_id"].clone(), json!({"_id": r}))],
            None => vec![],
        }
    };
    let custom_map = |doc: &Value| -> Vec<(Value, Value)> {
        match doc.get("dept") {
            Some(d) => vec![(json!([d, doc["n"]]), json!(1))],
            None => vec![],
        }
    };
    // CouchDB passes [key, docid] pairs to a custom reduce.
    let custom_reduce = ReduceFn::Custom(Box::new(|keys, values, rereduce| {
        if rereduce {
            json!(values.iter().filter_map(Value::as_u64).sum::<u64>())
        } else {
            assert!(
                keys.iter()
                    .all(|k| k.as_array().is_some_and(|p| p.len() == 2))
            );
            json!(keys.len())
        }
    }));

    type MapFn<'a> = &'a dyn Fn(&Value) -> Vec<(Value, Value)>;
    let cases: Vec<(&str, MapFn, Option<&ReduceFn>, &str, ViewQueryOptions)> = vec![
        // F53: total_rows / offset
        (
            "by_dept",
            &by_dept,
            Some(&ReduceFn::Sum),
            "reduce=false&key=%22eng%22",
            ViewQueryOptions {
                reduce: false,
                key: Some(json!("eng")),
                ..ViewQueryOptions::new()
            },
        ),
        (
            "by_dept",
            &by_dept,
            Some(&ReduceFn::Sum),
            "reduce=false&startkey=%22hr%22",
            ViewQueryOptions {
                reduce: false,
                start_key: Some(json!("hr")),
                ..ViewQueryOptions::new()
            },
        ),
        (
            "by_dept",
            &by_dept,
            Some(&ReduceFn::Sum),
            "reduce=false&startkey=%22hr%22&descending=true",
            ViewQueryOptions {
                reduce: false,
                start_key: Some(json!("hr")),
                descending: true,
                ..ViewQueryOptions::new()
            },
        ),
        (
            "by_dept",
            &by_dept,
            Some(&ReduceFn::Sum),
            "reduce=false&skip=1&limit=2",
            ViewQueryOptions {
                reduce: false,
                skip: 1,
                limit: Some(2),
                ..ViewQueryOptions::new()
            },
        ),
        // F104: reduce is on by default when a reduce function is given
        (
            "by_dept",
            &by_dept,
            Some(&ReduceFn::Sum),
            "",
            ViewQueryOptions::new(),
        ),
        (
            "by_dept",
            &by_dept,
            Some(&ReduceFn::Sum),
            "group=true",
            ViewQueryOptions {
                group: true,
                ..ViewQueryOptions::new()
            },
        ),
        // F50: design documents are not mapped
        (
            "count",
            &by_id,
            Some(&ReduceFn::Count),
            "",
            ViewQueryOptions::new(),
        ),
        (
            "count",
            &by_id,
            Some(&ReduceFn::Count),
            "reduce=false",
            ViewQueryOptions {
                reduce: false,
                ..ViewQueryOptions::new()
            },
        ),
        // F54: _sum / _stats keep integers and handle arrays and objects
        (
            "stats",
            &by_dept,
            Some(&ReduceFn::Stats),
            "",
            ViewQueryOptions::new(),
        ),
        (
            "stats",
            &by_dept,
            Some(&ReduceFn::Stats),
            "group=true",
            ViewQueryOptions {
                group: true,
                ..ViewQueryOptions::new()
            },
        ),
        (
            "arrs",
            &arrs,
            Some(&ReduceFn::Sum),
            "",
            ViewQueryOptions::new(),
        ),
        (
            "objs",
            &objs,
            Some(&ReduceFn::Sum),
            "",
            ViewQueryOptions::new(),
        ),
        // F16: include_docs, including linked documents
        (
            "by_dept",
            &by_dept,
            Some(&ReduceFn::Sum),
            "reduce=false&include_docs=true&limit=2",
            ViewQueryOptions {
                reduce: false,
                include_docs: true,
                limit: Some(2),
                ..ViewQueryOptions::new()
            },
        ),
        (
            "linked",
            &linked,
            None,
            "include_docs=true",
            ViewQueryOptions {
                include_docs: true,
                ..ViewQueryOptions::new()
            },
        ),
        // F105: custom reduce receives [key, id] pairs
        (
            "custom",
            &custom_map,
            Some(&custom_reduce),
            "group_level=1",
            ViewQueryOptions {
                group_level: Some(1),
                ..ViewQueryOptions::new()
            },
        ),
    ];

    let mut mismatches = Vec::new();
    for (view, map_fn, reduce, query, opts) in cases {
        let couch = couch_view(&url, view, query).await;
        let ours = query_view(local.adapter(), map_fn, reduce, opts)
            .await
            .unwrap();
        let mut expected_rows = couch["rows"].clone();
        // Linked docs to a missing id come back as null from CouchDB.
        for row in expected_rows.as_array_mut().unwrap() {
            match row.get("doc") {
                Some(Value::Null) => {
                    row.as_object_mut().unwrap().remove("doc");
                }
                Some(doc) => row["doc"] = without_rev(doc),
                None => {}
            }
        }
        let same_rows = rows_json(&ours) == expected_rows;
        let same_counts = couch.get("total_rows").is_none()
            || (couch["total_rows"] == json!(ours.total_rows)
                && couch["offset"] == json!(ours.offset));
        if !same_rows || !same_counts {
            mismatches.push(format!(
                "{view}?{query}:\n  couchdb={couch}\n  rouchdb=total_rows {} offset {} rows {}",
                ours.total_rows,
                ours.offset,
                rows_json(&ours)
            ));
        }
    }

    // CouchDB rejects include_docs on a reduced query.
    let err = query_view(
        local.adapter(),
        &by_dept,
        Some(&ReduceFn::Sum),
        ViewQueryOptions {
            include_docs: true,
            ..ViewQueryOptions::new()
        },
    )
    .await;
    assert!(err.is_err());

    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

#[tokio::test]
#[ignore]
async fn http_database_runs_mango_on_couchdb() {
    // F48: find/create_index on Database::http must use CouchDB's _find and
    // _index instead of downloading every document.
    let url = fresh_remote_db("remote_mango").await;
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
}
