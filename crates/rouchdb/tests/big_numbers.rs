//! JSON numbers beyond `i64`/`u64`/`f64`.
//!
//! With the `arbitrary-precision` feature every number is kept exactly as
//! written, through the local adapters (and a reopen), `bulk_get`,
//! replication and CouchDB (which keeps big integers exact too). Without
//! it, integers outside `i64`/`u64` and long decimals go through `f64`:
//! these tests pin both behaviors (CI runs the suite with and without the
//! feature).

mod backends;
mod common;

use backends::{Backend, KINDS};
use common::fresh_remote_db;
use rouchdb::{BulkGetItem, Database};

const BODY: &str = r#"{"big":18446744073709551616,"neg":-9223372036854775809,"u64":18446744073709551615,"dec":3.141592653589793238462643383279,"small":1.50,"huge":123456789012345678901234567890123456789}"#;

/// The members of `BODY` and their JSON text.
const MEMBERS: [(&str, &str); 6] = [
    ("big", "18446744073709551616"),
    ("neg", "-9223372036854775809"),
    ("u64", "18446744073709551615"),
    ("dec", "3.141592653589793238462643383279"),
    ("small", "1.50"),
    ("huge", "123456789012345678901234567890123456789"),
];

/// Integers that fit neither `i64` nor `u64`.
const BIG_INTEGERS: [&str; 3] = ["big", "neg", "huge"];

fn body() -> serde_json::Value {
    serde_json::from_str(BODY).unwrap()
}

/// Check the numbers of a stored `BODY`: exactly as written with the
/// feature; without it, the big integers became floats and the decimals
/// were rounded to `f64`.
fn check(doc: &serde_json::Value, context: &str) {
    for (key, text) in MEMBERS {
        let got = serde_json::to_string(&doc[key]).unwrap();
        if cfg!(feature = "arbitrary-precision") {
            assert_eq!(got, text, "{context}: {key}");
        } else if BIG_INTEGERS.contains(&key) {
            assert!(doc[key].is_f64(), "{context}: {key} = {got}");
        } else {
            let rounded = match key {
                "dec" => "3.141592653589793",
                "small" => "1.5",
                _ => text,
            };
            assert_eq!(got, rounded, "{context}: {key}");
        }
    }
}

#[tokio::test]
async fn numbers_round_trip_through_local_adapters() {
    for kind in KINDS {
        let b = Backend::open(kind, "numbers");
        b.db.put("n", body()).await.unwrap();
        check(&b.db.get("n").await.unwrap().data, kind);

        let got =
            b.db.adapter()
                .bulk_get(vec![BulkGetItem {
                    id: "n".into(),
                    rev: None,
                }])
                .await
                .unwrap();
        check(got.results[0].docs[0].ok.as_ref().unwrap(), "bulk_get");

        let copy = Backend::open(if kind == "memory" { "redb" } else { "memory" }, "copy");
        assert!(b.db.replicate_to(&copy.db).await.unwrap().ok);
        check(&copy.db.get("n").await.unwrap().data, "replicated");
    }
}

#[tokio::test]
async fn numbers_survive_a_redb_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("n.redb");
    {
        let db = Database::open(&path, "n").unwrap();
        db.put("n", body()).await.unwrap();
    }
    let db = Database::open(&path, "n").unwrap();
    check(&db.get("n").await.unwrap().data, "reopened");
}

/// CouchDB keeps integers of any size exactly (and rounds decimals to
/// doubles): with the feature, big integers go both ways unchanged.
#[tokio::test]
#[ignore = "requires CouchDB"]
async fn big_integers_round_trip_through_couchdb() {
    let url = fresh_remote_db("big_numbers").await;
    let raw = |id: &'static str| {
        let url = format!("{url}/{id}");
        async move { reqwest::get(url).await.unwrap().text().await.unwrap() }
    };
    let remote = Database::http(&url);
    let local = Database::memory("local");
    local.put("pushed", body()).await.unwrap();
    assert!(local.replicate_to(&remote).await.unwrap().ok);
    let pushed = raw("pushed").await;
    let exact = cfg!(feature = "arbitrary-precision");
    for big in ["18446744073709551616", "-9223372036854775809"] {
        assert_eq!(pushed.contains(big), exact, "{pushed}");
    }

    // Written straight to CouchDB, then pulled.
    let resp = reqwest::Client::new()
        .put(format!("{url}/pulled"))
        .body(BODY)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    assert!(local.replicate_from(&remote).await.unwrap().ok);
    let pulled = local.get("pulled").await.unwrap().data;
    let over_http = remote.get("pulled").await.unwrap().data;
    for doc in [&pulled, &over_http] {
        for key in BIG_INTEGERS {
            let text = MEMBERS.iter().find(|(k, _)| *k == key).unwrap().1;
            let got = serde_json::to_string(&doc[key]).unwrap();
            assert_eq!(got == text, exact, "{key}: {got}");
        }
    }
}
