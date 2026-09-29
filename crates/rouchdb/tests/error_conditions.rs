//! Error conditions of the `Database` API: every failure has its own
//! `RouchError` variant, the same on the memory and redb backends as
//! against CouchDB, and a failed call writes nothing.

mod backends;
mod common;

use backends::backends;
use common::fresh_remote_db;
use rouchdb::{ChangesOptions, Database, FindOptions, GetOptions, RouchError, Seq, SortField};

/// How an error is recognized.
type Check = fn(&RouchError) -> bool;

fn not_found(e: &RouchError) -> bool {
    matches!(e, RouchError::NotFound(_))
}
fn conflict(e: &RouchError) -> bool {
    matches!(e, RouchError::Conflict)
}
fn missing_id(e: &RouchError) -> bool {
    matches!(e, RouchError::MissingId)
}
fn invalid_rev(e: &RouchError) -> bool {
    matches!(e, RouchError::InvalidRev(_))
}
fn bad_request(e: &RouchError) -> bool {
    matches!(e, RouchError::BadRequest(_))
}

fn check<T: std::fmt::Debug>(what: &str, result: rouchdb::Result<T>, expected: Check) {
    match result {
        Err(e) if expected(&e) => {}
        other => panic!("{what}: unexpected {other:?}"),
    }
}

/// Runs every failing call of the table against `db` and checks that none
/// of them wrote anything.
async fn error_table(db: &Database, label: &str) {
    let doc = db.put("doc", serde_json::json!({"v": 1})).await.unwrap();
    let stale = doc.rev.unwrap();
    let current = db
        .update("doc", &stale, serde_json::json!({"v": 2}))
        .await
        .unwrap()
        .rev
        .unwrap();
    let gone = db.put("gone", serde_json::json!({})).await.unwrap();
    db.remove("gone", &gone.rev.unwrap()).await.unwrap();
    let seq = db.info().await.unwrap().update_seq;
    let body = || serde_json::json!({"v": 3});
    let at = |what: &str| format!("{label}: {what}");

    // Reading what does not exist.
    check(&at("get missing"), db.get("missing").await, not_found);
    check(&at("get deleted"), db.get("gone").await, not_found);
    let unknown_rev = GetOptions {
        rev: Some("9-deadbeef".into()),
        ..Default::default()
    };
    check(
        &at("get unknown rev"),
        db.get_with_opts("doc", unknown_rev).await,
        not_found,
    );
    check(
        &at("get_attachment missing attachment"),
        db.get_attachment("doc", "nope.txt").await,
        not_found,
    );
    check(
        &at("get_attachment missing doc"),
        db.get_attachment("missing", "nope.txt").await,
        not_found,
    );
    check(
        &at("get_design missing"),
        db.get_design("nope").await,
        not_found,
    );
    check(
        &at("partition get missing"),
        db.partition("users").get("nobody").await,
        not_found,
    );

    // Writes without an id.
    check(&at("put empty id"), db.put("", body()).await, missing_id);
    check(
        &at("update empty id"),
        db.update("", &current, body()).await,
        missing_id,
    );
    check(
        &at("remove empty id"),
        db.remove("", &current).await,
        missing_id,
    );

    // Writes that do not build on the current revision.
    check(&at("put existing"), db.put("doc", body()).await, conflict);
    check(
        &at("update stale"),
        db.update("doc", &stale, body()).await,
        conflict,
    );
    check(
        &at("remove stale"),
        db.remove("doc", &stale).await,
        conflict,
    );
    check(
        &at("update unknown rev"),
        db.update("doc", "1-bogusrevisionhash", body()).await,
        conflict,
    );
    check(
        &at("remove unknown rev"),
        db.remove("doc", "1-bogusrevisionhash").await,
        conflict,
    );
    check(
        &at("put_attachment stale"),
        db.put_attachment("doc", "a.txt", &stale, b"x".to_vec(), "text/plain")
            .await,
        conflict,
    );

    // Malformed input.
    check(
        &at("update malformed rev"),
        db.update("doc", "garbage", body()).await,
        invalid_rev,
    );
    check(
        &at("remove malformed rev"),
        db.remove("doc", "garbage").await,
        invalid_rev,
    );
    check(
        &at("put unknown special member"),
        db.put("bad", serde_json::json!({"_foo": 1})).await,
        bad_request,
    );
    check(
        &at("put non-object"),
        db.put("bad", serde_json::json!([1, 2])).await,
        bad_request,
    );
    check(
        &at("post non-object"),
        db.post(serde_json::json!("text")).await,
        bad_request,
    );
    check(
        &at("find invalid selector"),
        db.find(FindOptions {
            selector: serde_json::json!({"v": {"$bogus": 1}}),
            ..Default::default()
        })
        .await,
        bad_request,
    );
    check(
        &at("find empty sort field"),
        db.find(FindOptions {
            selector: serde_json::json!({"v": {"$gt": 0}}),
            sort: Some(vec![SortField::Simple(String::new())]),
            ..Default::default()
        })
        .await,
        bad_request,
    );
    check(
        &at("changes invalid selector"),
        db.changes(ChangesOptions {
            selector: Some(serde_json::json!({"v": {"$bogus": 1}})),
            ..Default::default()
        })
        .await,
        bad_request,
    );

    // Removing what does not exist.
    check(
        &at("remove_attachment missing"),
        db.remove_attachment("doc", "nope.txt", &current).await,
        not_found,
    );
    check(
        &at("delete_index missing"),
        db.delete_index("no-such-index").await,
        not_found,
    );

    // Nothing was written.
    let after = db.info().await.unwrap();
    assert_eq!(after.update_seq.as_num(), seq.as_num(), "{label}");
    assert_eq!((after.doc_count, after.doc_del_count), (1, 1), "{label}");
    let got = db.get("doc").await.unwrap();
    assert_eq!(got.rev.unwrap().to_string(), current, "{label}");
    assert_eq!(got.data, serde_json::json!({"v": 2}), "{label}");
    assert!(got.attachments.is_empty(), "{label}");
}

#[tokio::test]
async fn error_table_local() {
    for b in backends("errors") {
        error_table(&b.db, b.name).await;
        assert_eq!(b.db.info().await.unwrap().update_seq, Seq::Num(4));
    }
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn error_table_couchdb() {
    let url = fresh_remote_db("err_table").await;
    let db = Database::http(&url);
    error_table(&db, "couchdb").await;
}

/// A `validate_doc_update` rejection keeps its kind: `forbidden` and
/// `unauthorized` are distinct errors, and nothing is written.
#[tokio::test]
#[ignore = "requires CouchDB"]
async fn error_validate_doc_update_rejections() {
    let url = fresh_remote_db("err_vdu").await;
    let db = Database::http(&url);
    db.put(
        "_design/validate",
        serde_json::json!({
            "validate_doc_update": "function(doc) { \
                if (doc.kind === 'f') { throw({forbidden: 'no f'}); } \
                if (doc.kind === 'u') { throw({unauthorized: 'no u'}); } }"
        }),
    )
    .await
    .unwrap();
    let seq = db.info().await.unwrap().update_seq.as_num();

    let forbidden = db.put("a", serde_json::json!({"kind": "f"})).await;
    assert!(
        matches!(&forbidden, Err(RouchError::Forbidden(reason)) if reason == "no f"),
        "{forbidden:?}"
    );
    let unauthorized = db.put("b", serde_json::json!({"kind": "u"})).await;
    assert!(
        matches!(unauthorized, Err(RouchError::Unauthorized)),
        "{unauthorized:?}"
    );
    assert!(matches!(db.get("a").await, Err(RouchError::NotFound(_))));
    assert!(matches!(db.get("b").await, Err(RouchError::NotFound(_))));
    assert_eq!(db.info().await.unwrap().update_seq.as_num(), seq);

    // A valid document still goes through.
    db.put("c", serde_json::json!({"kind": "ok"}))
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn error_get_deleted_doc() {
    let url = fresh_remote_db("err_deleted").await;
    let db = Database::http(&url);

    let r1 = db
        .put("doc1", serde_json::json!({"v": 1, "tags": ["a"]}))
        .await
        .unwrap();
    let tomb = db.remove("doc1", &r1.rev.unwrap()).await.unwrap();
    let tomb_rev = tomb.rev.unwrap();

    let result = db.get("doc1").await;
    assert!(matches!(result, Err(RouchError::NotFound(_))));

    // Like on the local backends, the tombstone itself has no body.
    let got = db
        .get_with_opts(
            "doc1",
            GetOptions {
                rev: Some(tomb_rev.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        got.to_json(),
        serde_json::json!({"_id": "doc1", "_rev": tomb_rev, "_deleted": true})
    );
}
