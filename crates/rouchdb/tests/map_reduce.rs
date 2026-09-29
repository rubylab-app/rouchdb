//! Map/reduce view queries (`query_view`) on the memory adapter.
//!
//! Expected rows, values and offsets are the ones CouchDB 3.5.1 returns for
//! the equivalent JavaScript views (see `couchdb_query_parity.rs`).

use rouchdb::{Database, ReduceFn, RouchError, ViewQueryOptions, ViewResult, query_view};
use serde_json::{Value, json};

type Emitted = Vec<(Value, Value)>;

async fn db_with(docs: Vec<Value>) -> Database {
    let db = Database::memory("views");
    for doc in docs {
        let id = doc["_id"].as_str().unwrap().to_string();
        db.put(&id, doc).await.unwrap();
    }
    db
}

/// `(id, key, value)` of each row.
fn rows(result: &ViewResult) -> Vec<(Option<&str>, Value, Value)> {
    result
        .rows
        .iter()
        .map(|r| (r.id.as_deref(), r.key.clone(), r.value.clone()))
        .collect()
}

/// `(key, value)` of each reduced row (which has no id).
fn reduced(result: &ViewResult) -> Vec<(Value, Value)> {
    assert!(result.rows.iter().all(|r| r.id.is_none()));
    result
        .rows
        .iter()
        .map(|r| (r.key.clone(), r.value.clone()))
        .collect()
}

fn ids(result: &ViewResult) -> Vec<&str> {
    result
        .rows
        .iter()
        .map(|r| r.id.as_deref().unwrap())
        .collect()
}

#[tokio::test]
async fn view_basic_map() {
    let db = db_with(vec![
        json!({"_id": "a", "type": "person", "name": "Alice", "age": 30}),
        json!({"_id": "b", "type": "person", "name": "Bob", "age": 25}),
        json!({"_id": "c", "type": "city", "name": "NYC"}),
    ])
    .await;
    let map_fn = |doc: &Value| -> Emitted {
        if doc["type"] == "person" {
            vec![(doc["name"].clone(), doc["age"].clone())]
        } else {
            vec![]
        }
    };

    let result = query_view(db.adapter(), &map_fn, None, ViewQueryOptions::new())
        .await
        .unwrap();

    assert_eq!(
        rows(&result),
        [
            (Some("a"), json!("Alice"), json!(30)),
            (Some("b"), json!("Bob"), json!(25)),
        ]
    );
    assert_eq!((result.total_rows, result.offset), (2, 0));
    assert!(result.rows.iter().all(|r| r.doc.is_none()));
}

#[tokio::test]
async fn view_reduce_sum_and_count() {
    let db = db_with(vec![
        json!({"_id": "a", "dept": "eng", "salary": 100}),
        json!({"_id": "b", "dept": "eng", "salary": 120}),
        json!({"_id": "c", "dept": "sales", "salary": 90}),
    ])
    .await;
    let map_fn = |doc: &Value| -> Emitted { vec![(doc["dept"].clone(), doc["salary"].clone())] };
    let query = |reduce, group| {
        let db = &db;
        async move {
            let opts = ViewQueryOptions {
                group,
                ..ViewQueryOptions::new()
            };
            query_view(db.adapter(), &map_fn, Some(&reduce), opts)
                .await
                .unwrap()
        }
    };

    // Integers stay integers, as in CouchDB.
    let sum = query(ReduceFn::Sum, false).await;
    assert_eq!(reduced(&sum), [(json!(null), json!(310))]);
    let count = query(ReduceFn::Count, false).await;
    assert_eq!(reduced(&count), [(json!(null), json!(3))]);
    let by_dept = query(ReduceFn::Sum, true).await;
    assert_eq!(
        reduced(&by_dept),
        [(json!("eng"), json!(220)), (json!("sales"), json!(90))]
    );
}

#[tokio::test]
async fn view_key_range() {
    let db = db_with(
        (0..10)
            .map(|i| json!({"_id": format!("d{i}"), "n": i}))
            .collect(),
    )
    .await;
    let map_fn = |doc: &Value| -> Emitted { vec![(doc["n"].clone(), json!(1))] };

    let result = query_view(
        db.adapter(),
        &map_fn,
        None,
        ViewQueryOptions {
            start_key: Some(json!(3)),
            end_key: Some(json!(7)),
            ..ViewQueryOptions::new()
        },
    )
    .await
    .unwrap();

    assert_eq!(ids(&result), ["d3", "d4", "d5", "d6", "d7"]);
    assert_eq!((result.total_rows, result.offset), (10, 3));
}

#[tokio::test]
async fn view_reduce_sum_on_non_numeric_values() {
    let db = db_with(vec![
        json!({"_id": "doc1", "name": "Alice"}),
        json!({"_id": "doc2", "name": "Bob"}),
    ])
    .await;
    let map_fn = |doc: &Value| -> Emitted { vec![(json!(null), doc["name"].clone())] };

    // Not a silent 0: CouchDB reports a builtin_reduce_error, as the value
    // of the reduced row for _sum (see sum_errors_are_the_reduced_value)
    // and as a 500 for _stats, which RouchDB returns as an error.
    let result = query_view(
        db.adapter(),
        &map_fn,
        Some(&ReduceFn::Sum),
        ViewQueryOptions::new(),
    )
    .await
    .unwrap();
    assert_eq!(reduced(&result), [(json!(null), sum_error(json!("Bob")))]);
    let result = query_view(
        db.adapter(),
        &map_fn,
        Some(&ReduceFn::Stats),
        ViewQueryOptions::new(),
    )
    .await;
    assert!(
        matches!(&result, Err(RouchError::BadRequest(msg)) if msg.contains("builtin_reduce_error")),
        "{:?}",
        result.map(|r| reduced(&r))
    );
}

/// The value CouchDB 3.5.1 reduces `_sum` to when it cannot add `cause`.
fn sum_error(cause: Value) -> Value {
    json!({
        "error": "builtin_reduce_error",
        "reason": "The _sum function requires that map values be numbers, arrays of numbers, \
                   or objects. Objects cannot be mixed with other data structures. Objects can \
                   be arbitrarily nested, provided that the values for all fields are \
                   themselves numbers, arrays of numbers, or objects.",
        "caused_by": cause,
    })
}

#[tokio::test]
async fn sum_errors_are_the_reduced_value() {
    // CouchDB 3.5.1 answers 200 with the error as the value of the row;
    // it adds an index node's rows last to first, so `caused_by` is the
    // last value (in view order) that cannot be added.
    let db = db_with(vec![
        json!({"_id": "k1", "k": 1, "v": 1}),
        json!({"_id": "k2", "k": 2, "v": "a"}),
        json!({"_id": "k3", "k": 3, "v": 2}),
        json!({"_id": "k4", "k": 4, "v": "b"}),
        json!({"_id": "k5", "k": 5, "v": {"x": 1}}),
        json!({"_id": "k6", "k": 6, "v": 7}),
    ])
    .await;
    let map_fn = |doc: &Value| -> Emitted { vec![(doc["k"].clone(), doc["v"].clone())] };
    let query = |start: i64, end: i64, group: bool| {
        let opts = ViewQueryOptions {
            start_key: Some(json!(start)),
            end_key: Some(json!(end)),
            group,
            ..ViewQueryOptions::new()
        };
        let db = &db;
        async move {
            let result = query_view(db.adapter(), &map_fn, Some(&ReduceFn::Sum), opts)
                .await
                .unwrap();
            reduced(&result)
        }
    };
    assert_eq!(
        query(1, 4, false).await,
        [(json!(null), sum_error(json!("b")))]
    );
    assert_eq!(
        query(1, 3, false).await,
        [(json!(null), sum_error(json!("a")))]
    );
    assert_eq!(query(1, 1, false).await, [(json!(null), json!(1))]);
    assert_eq!(
        query(1, 4, true).await,
        [
            (json!(1), json!(1)),
            (json!(2), sum_error(json!("a"))),
            (json!(3), json!(2)),
            (json!(4), sum_error(json!("b"))),
        ]
    );
    // An object cannot be added to a number: going backwards from 7, the
    // object is the value that fails.
    assert_eq!(
        query(5, 6, false).await,
        [(json!(null), sum_error(json!({"x": 1})))]
    );
    assert_eq!(
        query(3, 6, false).await,
        [(json!(null), sum_error(json!({"x": 1})))]
    );
}

/// The reason CouchDB 3.5.1 gives for a key range no row can be in.
fn no_rows(descending: bool) -> String {
    format!(
        "No rows can match your key range, reverse your start_key and end_key or set \
         descending={}",
        !descending
    )
}

async fn view_error(db: &Database, reduce: Option<&ReduceFn>, opts: ViewQueryOptions) -> String {
    match query_view(db.adapter(), &by_dept, reduce, opts.clone()).await {
        Err(RouchError::BadRequest(reason)) => reason,
        other => panic!("{opts:?}: {:?}", other.map(|r| format!("{:?}", rows(&r)))),
    }
}

#[tokio::test]
async fn inverted_key_ranges_are_rejected() {
    let db = by_dept_db().await;
    for reduce in [None, Some(&ReduceFn::Sum)] {
        for group in [false, true] {
            if reduce.is_none() && group {
                continue;
            }
            for (start, end, descending) in [("z", "a", false), ("a", "z", true)] {
                let opts = ViewQueryOptions {
                    start_key: Some(json!(start)),
                    end_key: Some(json!(end)),
                    descending,
                    group,
                    ..ViewQueryOptions::new()
                };
                assert_eq!(view_error(&db, reduce, opts).await, no_rows(descending));
            }
        }
    }
    // Keys of different types compare by collation: an array sorts after
    // any string.
    let opts = ViewQueryOptions {
        start_key: Some(json!(["a"])),
        end_key: Some(json!("z")),
        ..ViewQueryOptions::new()
    };
    assert_eq!(view_error(&db, None, opts).await, no_rows(false));
    // Equal bounds are a range, even when the end is excluded.
    for (descending, inclusive_end, expected) in [
        (false, true, vec!["a", "c"]),
        (true, true, vec!["c", "a"]),
        (false, false, vec![]),
        (true, false, vec![]),
    ] {
        let opts = ViewQueryOptions {
            start_key: Some(json!("eng")),
            end_key: Some(json!("eng")),
            descending,
            inclusive_end,
            ..ViewQueryOptions::new()
        };
        let result = query_view(db.adapter(), &by_dept, None, opts)
            .await
            .unwrap();
        assert_eq!(ids(&result), expected, "{descending} {inclusive_end}");
    }
}

#[tokio::test]
async fn key_is_both_bounds_and_start_or_end_key_replace_one() {
    // In CouchDB `key` sets both bounds and a later `startkey`/`endkey`
    // replaces one: the options mean `key` followed by them.
    let db = by_dept_db().await;
    let query = |key: &str, start: Option<&str>, end: Option<&str>, inclusive_end| {
        let opts = ViewQueryOptions {
            key: Some(json!(key)),
            start_key: start.map(|k| json!(k)),
            end_key: end.map(|k| json!(k)),
            inclusive_end,
            ..ViewQueryOptions::new()
        };
        let db = &db;
        async move {
            let result = query_view(db.adapter(), &by_dept, None, opts)
                .await
                .unwrap();
            ids(&result)
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(query("hr", Some("eng"), None, true).await, ["a", "c", "d"]);
    assert_eq!(query("eng", None, Some("hr"), true).await, ["a", "c", "d"]);
    assert_eq!(query("eng", None, Some("hr"), false).await, ["a", "c"]);
    assert_eq!(query("eng", None, None, true).await, ["a", "c"]);
    assert!(query("eng", None, None, false).await.is_empty());
    let opts = ViewQueryOptions {
        key: Some(json!("eng")),
        start_key: Some(json!("hr")),
        ..ViewQueryOptions::new()
    };
    assert_eq!(view_error(&db, None, opts).await, no_rows(false));
    // A one-element `keys` is `key`.
    let opts = ViewQueryOptions {
        keys: keys(&["hr"]),
        start_key: Some(json!("eng")),
        ..ViewQueryOptions::new()
    };
    let result = query_view(db.adapter(), &by_dept, None, opts)
        .await
        .unwrap();
    assert_eq!(ids(&result), ["a", "c", "d"]);
    // Several keys exclude key and ranges, and each key is the range from
    // itself to itself.
    for opts in [
        ViewQueryOptions {
            key: Some(json!("eng")),
            ..ViewQueryOptions::new()
        },
        ViewQueryOptions {
            start_key: Some(json!("a")),
            ..ViewQueryOptions::new()
        },
        ViewQueryOptions {
            end_key: Some(json!("z")),
            group: true,
            ..ViewQueryOptions::new()
        },
    ] {
        let opts = ViewQueryOptions {
            keys: keys(&["hr", "eng"]),
            ..opts
        };
        let reduce = opts.group.then_some(&ReduceFn::Sum);
        assert_eq!(
            view_error(&db, reduce, opts).await,
            "`keys` is incompatible with `key`, `start_key` and `end_key`"
        );
    }
    let opts = ViewQueryOptions {
        keys: keys(&["hr", "eng"]),
        inclusive_end: false,
        ..ViewQueryOptions::new()
    };
    let result = query_view(db.adapter(), &by_dept, None, opts)
        .await
        .unwrap();
    assert!(result.rows.is_empty());
}

#[tokio::test]
async fn grouped_keys_keep_their_order_when_descending() {
    // CouchDB 3.5.1 keeps the order of `keys` for group=true, descending
    // or not (unlike map rows, which it reverses).
    let db = by_dept_db().await;
    for descending in [false, true] {
        let opts = ViewQueryOptions {
            keys: keys(&["hr", "zzz", "eng", "sales"]),
            group: true,
            descending,
            ..ViewQueryOptions::new()
        };
        let result = query_view(db.adapter(), &by_dept, Some(&ReduceFn::Sum), opts)
            .await
            .unwrap();
        assert_eq!(
            reduced(&result),
            [
                (json!("hr"), json!(5)),
                (json!("eng"), json!(65)),
                (json!("sales"), json!(25))
            ],
            "descending={descending}"
        );
    }
}

#[tokio::test]
async fn grouping_a_map_view_is_rejected() {
    // CouchDB 3.5.1: "Invalid use of grouping on a map view." for a view
    // without reduce and for reduce=false, unless the group level is 0.
    let db = by_dept_db().await;
    for (reduce, reduce_on) in [(None, true), (Some(&ReduceFn::Sum), false)] {
        for (group, group_level) in [(true, None), (false, Some(1)), (true, Some(2))] {
            let opts = ViewQueryOptions {
                group,
                group_level,
                reduce: reduce_on,
                ..ViewQueryOptions::new()
            };
            assert_eq!(
                view_error(&db, reduce, opts).await,
                "Invalid use of grouping on a map view."
            );
        }
        for (group, group_level) in [(false, None), (false, Some(0)), (true, Some(0))] {
            let opts = ViewQueryOptions {
                group,
                group_level,
                reduce: reduce_on,
                ..ViewQueryOptions::new()
            };
            let result = query_view(db.adapter(), &by_dept, reduce, opts)
                .await
                .unwrap();
            assert_eq!(ids(&result), ["a", "c", "d", "b"]);
        }
    }
}

#[tokio::test]
async fn view_reduce_with_zero_emitted_rows() {
    // An empty reduce returns no rows (CouchDB returns {"rows":[]}), not a
    // spurious zero row, whether the database is empty or nothing is
    // emitted.
    let empty = Database::memory("empty");
    let quiet = db_with(vec![json!({"_id": "a", "n": 1})]).await;
    let map_fn = |_doc: &Value| -> Emitted { vec![] };
    for db in [&empty, &quiet] {
        for reduce in [ReduceFn::Count, ReduceFn::Sum, ReduceFn::Stats] {
            for group in [false, true] {
                let opts = ViewQueryOptions {
                    group,
                    ..ViewQueryOptions::new()
                };
                let result = query_view(db.adapter(), &map_fn, Some(&reduce), opts)
                    .await
                    .unwrap();
                assert!(result.rows.is_empty());
                assert_eq!((result.total_rows, result.offset), (0, 0));
            }
        }
    }
}

// =========================================================================
// Views of the CouchDB parity table
// =========================================================================

/// Rows (a, eng, 30), (c, eng, 35), (d, hr, 5), (b, sales, 25).
async fn by_dept_db() -> Database {
    db_with(vec![
        json!({"_id": "a", "dept": "eng", "n": 30}),
        json!({"_id": "b", "dept": "sales", "n": 25}),
        json!({"_id": "c", "dept": "eng", "n": 35}),
        json!({"_id": "d", "dept": "hr", "n": 5}),
    ])
    .await
}

fn by_dept(doc: &Value) -> Emitted {
    vec![(doc["dept"].clone(), doc["n"].clone())]
}

fn keys(keys: &[&str]) -> Option<Vec<Value>> {
    Some(keys.iter().map(|k| json!(k)).collect())
}

#[tokio::test]
async fn keys_queries_report_the_offset_of_the_first_row() {
    let db = by_dept_db().await;
    // (keys, descending, skip, limit, ids, offset)
    let cases = [
        (vec!["hr", "eng"], false, 0, None, vec!["d", "a", "c"], 2),
        (vec!["hr", "eng"], true, 0, None, vec!["c", "a", "d"], 2),
        (
            vec!["sales", "eng", "zzz"],
            true,
            0,
            None,
            vec!["c", "a", "b"],
            2,
        ),
        (vec!["sales"], true, 0, None, vec!["b"], 0),
        (vec!["zzz"], false, 0, None, vec![], 4),
        (vec!["hr", "eng"], false, 1, Some(1), vec!["a"], 3),
    ];
    for (requested, descending, skip, limit, expected_ids, offset) in cases {
        let opts = ViewQueryOptions {
            keys: keys(&requested),
            descending,
            skip,
            limit,
            ..ViewQueryOptions::new()
        };
        let result = query_view(db.adapter(), &by_dept, None, opts)
            .await
            .unwrap();
        let case = format!("keys={requested:?} descending={descending} skip={skip}");
        assert_eq!(ids(&result), expected_ids, "{case}");
        assert_eq!((result.total_rows, result.offset), (4, offset), "{case}");
    }
}

#[tokio::test]
async fn keys_with_non_adjacent_duplicates_repeat_their_rows() {
    // Every requested key contributes its rows where it appears in `keys`,
    // duplicates included, and descending reverses the whole result. This
    // is PouchDB's behaviour (one lookup per key) and a single-shard
    // CouchDB's; a clustered CouchDB (q > 1) merges the rows of its shards
    // and interleaves the duplicates differently.
    let db = by_dept_db().await;
    for (descending, expected, offset) in [
        (false, ["a", "c", "d", "a", "c"], 0),
        (true, ["c", "a", "d", "c", "a"], 2),
    ] {
        let opts = ViewQueryOptions {
            keys: keys(&["eng", "hr", "eng"]),
            descending,
            ..ViewQueryOptions::new()
        };
        let result = query_view(db.adapter(), &by_dept, None, opts)
            .await
            .unwrap();
        assert_eq!(ids(&result), expected, "descending={descending}");
        assert_eq!(result.offset, offset, "descending={descending}");
    }

    // With a reduce: one row per requested key.
    let opts = ViewQueryOptions {
        keys: keys(&["hr", "eng", "hr"]),
        group: true,
        ..ViewQueryOptions::new()
    };
    let result = query_view(db.adapter(), &by_dept, Some(&ReduceFn::Sum), opts)
        .await
        .unwrap();
    assert_eq!(
        reduced(&result),
        [
            (json!("hr"), json!(5)),
            (json!("eng"), json!(65)),
            (json!("hr"), json!(5)),
        ]
    );
}

#[tokio::test]
async fn multi_key_reduce_without_grouping_is_rejected() {
    // CouchDB: "Multi-key fetches for reduce views must use `group=true`",
    // whatever group_level says.
    let db = by_dept_db().await;
    for group_level in [None, Some(0), Some(1)] {
        let opts = ViewQueryOptions {
            keys: keys(&["hr", "eng"]),
            group_level,
            ..ViewQueryOptions::new()
        };
        let result = query_view(db.adapter(), &by_dept, Some(&ReduceFn::Sum), opts).await;
        assert!(
            matches!(result, Err(RouchError::BadRequest(_))),
            "group_level={group_level:?}"
        );
    }
}

/// Documents emitting keys of every shape.
async fn mixed_keys_db() -> Database {
    db_with(vec![
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
    ])
    .await
}

fn mixed(doc: &Value) -> Emitted {
    vec![(doc["k"].clone(), doc["v"].clone())]
}

#[tokio::test]
async fn group_level_truncates_array_keys_only() {
    // group_level groups without `group`, and keys that are not arrays
    // (and []) are kept whole.
    let db = mixed_keys_db().await;
    let query = |group_level, descending| {
        let db = &db;
        async move {
            let opts = ViewQueryOptions {
                group_level: Some(group_level),
                descending,
                ..ViewQueryOptions::new()
            };
            let result = query_view(db.adapter(), &mixed, Some(&ReduceFn::Sum), opts)
                .await
                .unwrap();
            reduced(&result)
        }
    };
    let level1 = vec![
        (json!(null), json!(8)),
        (json!(5), json!(3.75)),
        (json!("str"), json!(1)),
        (json!([]), json!(2)),
        (json!(["x"]), json!(24)),
        (json!(["y"]), json!(7)),
        (json!({"a": 1}), json!(3)),
    ];
    assert_eq!(query(1, false).await, level1);
    let mut level1_descending = level1.clone();
    level1_descending.reverse();
    assert_eq!(query(1, true).await, level1_descending);
    assert_eq!(
        query(2, false).await,
        [
            (json!(null), json!(8)),
            (json!(5), json!(3.75)),
            (json!("str"), json!(1)),
            (json!([]), json!(2)),
            (json!(["x"]), json!(9)),
            (json!(["x", 1]), json!(4)),
            (json!(["x", 2]), json!(11)),
            (json!(["y"]), json!(7)),
            (json!({"a": 1}), json!(3)),
        ]
    );
    assert_eq!(query(0, false).await, [(json!(null), json!(48.75))]);
}

#[tokio::test]
async fn stats_of_floats_and_of_arrays() {
    let db = mixed_keys_db().await;
    let opts = ViewQueryOptions {
        key: Some(json!(5)),
        ..ViewQueryOptions::new()
    };
    let result = query_view(db.adapter(), &mixed, Some(&ReduceFn::Stats), opts)
        .await
        .unwrap();
    assert_eq!(
        reduced(&result),
        [(
            json!(null),
            json!({"sum": 3.75, "count": 2, "min": 1.5, "max": 2.25, "sumsqr": 7.3125})
        )]
    );

    // Arrays of numbers are reduced column by column.
    let db = db_with(vec![
        json!({"_id": "a", "v": [1, 0.5]}),
        json!({"_id": "b", "v": [3, -2]}),
    ])
    .await;
    let map_fn = |doc: &Value| -> Emitted { vec![(doc["_id"].clone(), doc["v"].clone())] };
    let result = query_view(
        db.adapter(),
        &map_fn,
        Some(&ReduceFn::Stats),
        ViewQueryOptions::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        reduced(&result),
        [(
            json!(null),
            json!([
                {"sum": 4, "count": 2, "min": 1, "max": 3, "sumsqr": 10},
                {"sum": -1.5, "count": 2, "min": -2, "max": 0.5, "sumsqr": 4.25}
            ])
        )]
    );
}

#[tokio::test]
async fn stats_rejects_mixed_and_ragged_values() {
    // CouchDB fails all of these (invalid_value / function_clause).
    for values in [
        [json!([1, 2]), json!([3])],
        [json!(1), json!([2, 3])],
        [json!([1, 2]), json!(3)],
    ] {
        let db = db_with(
            values
                .iter()
                .enumerate()
                .map(|(i, v)| json!({"_id": format!("x{i}"), "v": v}))
                .collect(),
        )
        .await;
        let map_fn = |doc: &Value| -> Emitted { vec![(doc["_id"].clone(), doc["v"].clone())] };
        let result = query_view(
            db.adapter(),
            &map_fn,
            Some(&ReduceFn::Stats),
            ViewQueryOptions::new(),
        )
        .await;
        assert!(
            matches!(result, Err(RouchError::BadRequest(_))),
            "{values:?}"
        );
    }
}

#[tokio::test]
async fn sum_pads_arrays_and_adds_numbers_to_the_first_element() {
    let db = db_with(vec![
        json!({"_id": "r1", "rag": [1, 2]}),
        json!({"_id": "r2", "rag": [3]}),
        json!({"_id": "r3", "rag": [4, 5, 6]}),
        json!({"_id": "r4", "rag": 7}),
    ])
    .await;
    let map_fn = |doc: &Value| -> Emitted { vec![(doc["_id"].clone(), doc["rag"].clone())] };
    let sum = |start_key, end_key| {
        let db = &db;
        async move {
            let opts = ViewQueryOptions {
                start_key,
                end_key,
                ..ViewQueryOptions::new()
            };
            let result = query_view(db.adapter(), &map_fn, Some(&ReduceFn::Sum), opts)
                .await
                .unwrap();
            reduced(&result)
        }
    };
    assert_eq!(sum(None, None).await, [(json!(null), json!([15, 7, 6]))]);
    assert_eq!(
        sum(Some(json!("r2")), None).await,
        [(json!(null), json!([14, 5, 6]))]
    );
    assert_eq!(
        sum(None, Some(json!("r2"))).await,
        [(json!(null), json!([4, 2]))]
    );
}

#[tokio::test]
async fn include_docs_follows_linked_revisions() {
    // emit(key, {_id, _rev}) includes that revision of the document, or
    // nothing if the revision does not exist; without _rev, the current one.
    let db = Database::memory("linked");
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
    db.put("lr3", json!({"ref_id": "t"})).await.unwrap();
    let map_fn = |doc: &Value| -> Emitted {
        let Some(id) = doc.get("ref_id") else {
            return vec![];
        };
        let mut value = json!({"_id": id});
        if let Some(rev) = doc.get("ref_rev") {
            value["_rev"] = rev.clone();
        }
        vec![(doc["_id"].clone(), value)]
    };

    let opts = ViewQueryOptions {
        include_docs: true,
        ..ViewQueryOptions::new()
    };
    let result = query_view(db.adapter(), &map_fn, None, opts).await.unwrap();

    let current = db.get("t").await.unwrap().to_json();
    assert_eq!(current["gen"], 2);
    let docs: Vec<_> = result.rows.iter().map(|r| r.doc.clone()).collect();
    assert_eq!(
        docs,
        [
            Some(json!({"_id": "t", "_rev": first, "gen": 1})),
            None,
            Some(current),
        ]
    );
}
