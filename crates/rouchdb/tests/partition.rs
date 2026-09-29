//! `Partition` queries are exactly the database's queries restricted to the
//! ids starting with `"{name}:"`, on the memory and the redb backend.

mod backends;

use backends::{Backend, KINDS, backends, row_ids};
use rouchdb::{AllDocsOptions, AllDocsResponse, Database, FindOptions, RouchError};

const PREFIX: &str = "users:";

/// Real `users:` documents next to ids that are easy to confuse with them
/// (sorting just before or after the partition, or containing its name).
const IDS: &[&str] = &[
    "a:users:b",
    "orders:1",
    "users",
    "users2",
    "users9",
    "users:",
    "users:a",
    "users:b",
    "users:b:c",
    "users:c",
    "users:\u{e9}",
    "users:\u{1F600}",
    "users:\u{10FFFF}",
    "users:\u{10FFFF}z",
    "users;",
    "usersX:1",
    "users_:1",
    "zzz",
];

/// A deleted `users:` document (listed only when requested by key).
const DELETED: &str = "users:gone";

async fn fill(db: &Database) {
    for id in IDS {
        db.put(id, serde_json::json!({"id": id})).await.unwrap();
    }
    let rev = db.put(DELETED, serde_json::json!({})).await.unwrap().rev;
    db.remove(DELETED, &rev.unwrap()).await.unwrap();
}

/// The rows of a response as comparable JSON.
fn rows(response: &AllDocsResponse) -> Vec<serde_json::Value> {
    response
        .rows
        .iter()
        .map(|r| serde_json::to_value(r).unwrap())
        .collect()
}

/// What `partition.all_docs(opts)` must return: the database's rows for the
/// same query without paging, restricted to the partition, then paged.
async fn expected(db: &Database, opts: &AllDocsOptions) -> Vec<serde_json::Value> {
    let unpaged = db
        .all_docs(AllDocsOptions {
            skip: 0,
            limit: None,
            ..opts.clone()
        })
        .await
        .unwrap();
    rows(&unpaged)
        .into_iter()
        .filter(|r| r["id"].as_str().unwrap().starts_with(PREFIX))
        .skip(opts.skip as usize)
        .take(opts.limit.map_or(usize::MAX, |l| l as usize))
        .collect()
}

/// The ids a range query must return, computed from the id list alone.
fn model(opts: &AllDocsOptions) -> Vec<String> {
    let mut ids: Vec<&str> = IDS
        .iter()
        .copied()
        .filter(|id| id.starts_with(PREFIX))
        .collect();
    ids.sort();
    if opts.descending {
        ids.reverse();
    }
    let (desc, inclusive) = (opts.descending, opts.inclusive_end);
    ids.retain(|&id| {
        let after_start = opts
            .start_key
            .as_deref()
            .is_none_or(|s| if desc { id <= s } else { id >= s });
        let before_end = opts
            .end_key
            .as_deref()
            .is_none_or(|e| match (desc, inclusive) {
                (false, true) => id <= e,
                (false, false) => id < e,
                (true, true) => id >= e,
                (true, false) => id > e,
            });
        after_start && before_end
    });
    ids.into_iter()
        .skip(opts.skip as usize)
        .take(opts.limit.map_or(usize::MAX, |l| l as usize))
        .map(str::to_string)
        .collect()
}

fn key(k: &str) -> Option<String> {
    Some(k.to_string())
}

/// Range queries over every combination of bounds, direction, inclusive
/// end and paging.
fn range_queries() -> Vec<AllDocsOptions> {
    let bounds = [
        (None, None),
        (key("a"), None),
        (None, key("zzz")),
        (key("users"), key("users;")),
        (key("users;"), None),
        (None, key("users;")),
        (key("users2"), key("usersX:1")),
        (key("users:b"), None),
        (None, key("users:b")),
        (key("users:a"), key("users:c")),
        (key("users:b"), key("users:b")),
        (key("users:c"), key("users:a")),
        (key("users:b:"), key("users:bz")),
        (key("zzz"), None),
        (None, key("a")),
    ];
    let paging = [
        (0, None),
        (0, Some(0)),
        (0, Some(1)),
        (1, Some(2)),
        (3, None),
        (100, None),
    ];
    let mut queries = Vec::new();
    for (start_key, end_key) in bounds {
        for descending in [false, true] {
            for inclusive_end in [true, false] {
                for (i, (skip, limit)) in paging.into_iter().enumerate() {
                    queries.push(AllDocsOptions {
                        start_key: start_key.clone(),
                        end_key: end_key.clone(),
                        descending,
                        inclusive_end,
                        skip,
                        limit,
                        include_docs: i % 2 == 0,
                        ..AllDocsOptions::new()
                    });
                }
            }
        }
    }
    queries
}

#[tokio::test]
async fn partition_all_docs_equals_the_filtered_database_query() {
    for b in backends("partition") {
        fill(&b.db).await;
        let users = b.db.partition("users");
        for opts in range_queries() {
            let got = users.all_docs(opts.clone()).await.unwrap();
            assert_eq!(row_ids(&got), model(&opts), "{}: {opts:?}", b.name);
            assert_eq!(
                rows(&got),
                expected(&b.db, &opts).await,
                "{}: {opts:?}",
                b.name
            );
            // The local adapters report the skip that was applied.
            assert_eq!(got.offset, opts.skip, "{}: {opts:?}", b.name);
        }
    }
}

#[tokio::test]
async fn partition_all_docs_by_key_stays_in_the_partition() {
    for b in backends("partition") {
        fill(&b.db).await;
        let users = b.db.partition("users");
        let requested = [
            "users:c",
            "orders:1",
            "users:a",
            "users:missing",
            "usersX:1",
            DELETED,
            "users:c",
            "users",
        ];
        for descending in [false, true] {
            for (skip, limit) in [(0, None), (1, Some(2))] {
                let opts = AllDocsOptions {
                    keys: Some(requested.iter().map(|k| k.to_string()).collect()),
                    descending,
                    skip,
                    limit,
                    ..AllDocsOptions::new()
                };
                let got = users.all_docs(opts.clone()).await.unwrap();
                assert_eq!(
                    rows(&got),
                    expected(&b.db, &opts).await,
                    "{}: {opts:?}",
                    b.name
                );
            }
        }
        let got = users
            .all_docs(AllDocsOptions {
                keys: Some(requested.iter().map(|k| k.to_string()).collect()),
                ..AllDocsOptions::new()
            })
            .await
            .unwrap();
        // Request order, duplicates and the deleted document included.
        assert_eq!(
            row_ids(&got),
            ["users:c", "users:a", DELETED, "users:c"],
            "{}",
            b.name
        );
        assert_eq!(got.rows[2].value.deleted, Some(true), "{}", b.name);

        for (k, ids) in [
            ("users:b", vec!["users:b"]),
            ("users:", vec!["users:"]),
            ("users:missing", vec![]),
            ("usersX:1", vec![]),
            ("orders:1", vec![]),
            ("users", vec![]),
        ] {
            let got = users
                .all_docs(AllDocsOptions {
                    key: key(k),
                    ..AllDocsOptions::new()
                })
                .await
                .unwrap();
            assert_eq!(row_ids(&got), ids, "{}: key {k}", b.name);
        }
    }
}

/// Every document of the partition is listed, whatever its id, and an
/// exclusive end is only applied to the caller's own end key.
#[tokio::test]
async fn partition_all_docs_edge_ids() {
    for b in backends("partition") {
        let edge = ["users:", "users:a", "users:\u{10FFFF}", "users:\u{10FFFF}z"];
        for id in edge.iter().chain(&["users9", "users;"]) {
            b.db.put(id, serde_json::json!({})).await.unwrap();
        }
        let users = b.db.partition("users");
        let query = |descending, inclusive_end| AllDocsOptions {
            descending,
            inclusive_end,
            ..AllDocsOptions::new()
        };
        for inclusive_end in [true, false] {
            let asc = users.all_docs(query(false, inclusive_end)).await.unwrap();
            assert_eq!(
                row_ids(&asc),
                edge,
                "{} inclusive_end={inclusive_end}",
                b.name
            );
            let desc = users.all_docs(query(true, inclusive_end)).await.unwrap();
            let mut reversed = edge.to_vec();
            reversed.reverse();
            assert_eq!(
                row_ids(&desc),
                reversed,
                "{} descending inclusive_end={inclusive_end}",
                b.name
            );
        }
    }
}

#[tokio::test]
async fn partition_all_docs_clamps_ranges_outside_the_partition() {
    for b in backends("partition") {
        for id in [
            "users:alice",
            "users:bob",
            "userz:1",
            "zzz:other",
            "a:users:x",
        ] {
            b.db.put(id, serde_json::json!({"type": "user"}))
                .await
                .unwrap();
        }
        let users = b.db.partition("users");
        let query = |start: &str, end: &str, descending| AllDocsOptions {
            start_key: key(start),
            end_key: key(end),
            descending,
            include_docs: true,
            ..AllDocsOptions::new()
        };
        let got = users.all_docs(query("a", "zzzz", false)).await.unwrap();
        assert_eq!(row_ids(&got), ["users:alice", "users:bob"], "{}", b.name);
        assert_eq!(got.rows[1].doc.as_ref().unwrap()["_id"], "users:bob");
        let got = users.all_docs(query("zzzz", "a", true)).await.unwrap();
        assert_eq!(row_ids(&got), ["users:bob", "users:alice"], "{}", b.name);
        // A range entirely outside the partition is empty.
        let got = users.all_docs(query("zzz:", "zzz:~", false)).await.unwrap();
        assert!(got.rows.is_empty(), "{}: {:?}", b.name, row_ids(&got));
    }
}

#[tokio::test]
async fn partition_with_empty_name_is_the_colon_prefix() {
    for b in backends("partition") {
        for id in [":doc1", ":", "::x", "normal_doc", "a:b", "9"] {
            b.db.put(id, serde_json::json!({})).await.unwrap();
        }
        let got =
            b.db.partition("")
                .all_docs(AllDocsOptions::new())
                .await
                .unwrap();
        assert_eq!(row_ids(&got), [":", "::x", ":doc1"], "{}", b.name);
    }
}

#[tokio::test]
async fn partition_find_takes_any_selector_the_database_takes() {
    for b in backends("partition") {
        for id in ["users:a", "users:b", "orders:1", "users"] {
            b.db.put(id, serde_json::json!({"o": {}})).await.unwrap();
        }
        let users = b.db.partition("users");
        let find = |selector: serde_json::Value| {
            users.find(FindOptions {
                selector,
                ..Default::default()
            })
        };
        let ids = |r: rouchdb::FindResponse| -> Vec<String> {
            r.docs
                .iter()
                .map(|d| d["_id"].as_str().unwrap().to_string())
                .collect()
        };
        // `{}` matches every document of the partition, as `{}` does in
        // the database (nested in a combinator it would match none).
        assert_eq!(
            ids(find(serde_json::json!({})).await.unwrap()),
            ["users:a", "users:b"],
            "{}",
            b.name
        );
        assert_eq!(
            ids(find(serde_json::json!({"o": {}})).await.unwrap()),
            ["users:a", "users:b"],
            "{}",
            b.name
        );
        // A selector the database rejects is rejected in a partition too,
        // although it would be valid inside a combinator.
        for selector in [
            serde_json::json!({"$gt": 1}),
            serde_json::json!({"$not": {}}),
        ] {
            let result = find(selector.clone()).await;
            assert!(
                matches!(result, Err(RouchError::BadRequest(_))),
                "{}: {selector}: {result:?}",
                b.name
            );
        }
    }
}

#[tokio::test]
async fn partition_find_matches_the_prefix_literally() {
    for b in backends("partition") {
        for id in [
            "user.test:doc1",
            "userXtest:doc2",
            "user_test:doc3",
            "user.testing:doc4",
            "a:user.test:doc5",
        ] {
            b.db.put(id, serde_json::json!({"type": "a"}))
                .await
                .unwrap();
        }
        // "." and "+" are regex metacharacters.
        b.db.put("c++:doc6", serde_json::json!({"type": "a"}))
            .await
            .unwrap();
        b.db.put("cxx:doc7", serde_json::json!({"type": "a"}))
            .await
            .unwrap();
        for (partition, ids) in [
            ("user.test", vec!["user.test:doc1"]),
            ("c++", vec!["c++:doc6"]),
        ] {
            let result =
                b.db.partition(partition)
                    .find(FindOptions {
                        selector: serde_json::json!({"type": {"$exists": true}}),
                        ..Default::default()
                    })
                    .await
                    .unwrap();
            let found: Vec<&str> = result
                .docs
                .iter()
                .map(|d| d["_id"].as_str().unwrap())
                .collect();
            assert_eq!(found, ids, "{}: partition {partition}", b.name);
        }
    }
}

#[tokio::test]
async fn partition_get_and_put_prefix_the_id() {
    for kind in KINDS {
        let b = Backend::open(kind, "partition");
        let users = b.db.partition("users");
        let r = users
            .put("alice", serde_json::json!({"n": 1}))
            .await
            .unwrap();
        assert_eq!(r.id, "users:alice");
        let r2 = users
            .put("users:bob", serde_json::json!({"n": 2}))
            .await
            .unwrap();
        assert_eq!(r2.id, "users:bob", "an already prefixed id is kept");
        assert_eq!(users.get("alice").await.unwrap().id, "users:alice");
        assert_eq!(users.get("users:alice").await.unwrap().data["n"], 1);
        assert_eq!(b.db.get("users:bob").await.unwrap().data["n"], 2);
        assert!(
            matches!(users.get("carol").await, Err(RouchError::NotFound(_))),
            "{kind}"
        );
        // Writing an existing id without its revision conflicts.
        assert!(
            matches!(
                users.put("alice", serde_json::json!({})).await,
                Err(RouchError::Conflict)
            ),
            "{kind}"
        );
        let all = b.db.all_docs(AllDocsOptions::new()).await.unwrap();
        assert_eq!(row_ids(&all), ["users:alice", "users:bob"], "{kind}");
    }
}
