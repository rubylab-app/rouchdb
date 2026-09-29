//! `rouchdb migrate` and the refusal of files written by rouchdb <= 0.4.

use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;
use redb::{Database, TableDefinition};

const REV: &str = "1-0123456789abcdef0123456789abcdef";

/// A file laid out exactly as rouchdb 0.4 writes it, with one document.
fn legacy_file(path: &Path) {
    let db = Database::create(path).unwrap();
    let txn = db.begin_write().unwrap();
    {
        let str_table = |name| TableDefinition::<&str, &[u8]>::new(name);
        let mut docs = txn.open_table(str_table("docs")).unwrap();
        docs.insert(
            "a",
            &br#"{"rev_tree":[{"pos":1,"tree":{"hash":"0123456789abcdef0123456789abcdef","status":"available","deleted":false,"children":[]}}],"seq":1}"#[..],
        )
        .unwrap();
        let mut revs = txn.open_table(str_table("rev_data")).unwrap();
        revs.insert(
            format!("a\0{REV}").as_str(),
            &br#"{"data":{"v":1},"deleted":false}"#[..],
        )
        .unwrap();
        txn.open_table(TableDefinition::<u64, &[u8]>::new("changes"))
            .unwrap()
            .insert(1, &br#"{"doc_id":"a","deleted":false}"#[..])
            .unwrap();
        txn.open_table(str_table("local_docs")).unwrap();
        txn.open_table(str_table("attachments")).unwrap();
        txn.open_table(str_table("metadata"))
            .unwrap()
            .insert("meta", &br#"{"update_seq":1,"db_uuid":"u"}"#[..])
            .unwrap();
    }
    txn.commit().unwrap();
}

#[allow(deprecated)]
fn rouchdb() -> Command {
    Command::cargo_bin("rouchdb").unwrap()
}

#[test]
fn legacy_file_is_refused_with_a_hint_then_migrated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.redb");
    legacy_file(&path);
    let p = path.to_str().unwrap();
    let backup = dir.path().join("old.redb.rouchdb-0.4.bak");

    rouchdb()
        .args(["info", p])
        .assert()
        .failure()
        .stderr(predicate::str::contains("rouchdb 0.4 or earlier"))
        .stderr(predicate::str::contains(format!("rouchdb migrate {p}")));

    rouchdb()
        .args(["migrate", "--dry-run", p])
        .assert()
        .success()
        .stdout(predicate::str::contains("not modified (dry run)"))
        .stdout(predicate::str::contains("documents: 1 live, 0 deleted"));
    assert!(!backup.exists());

    rouchdb()
        .args(["migrate", p])
        .assert()
        .success()
        .stdout(predicate::str::contains("status: upgraded"))
        .stdout(predicate::str::contains(format!(
            "backup: {}",
            backup.display()
        )))
        .stdout(predicate::str::contains("WARNING"))
        .stdout(predicate::str::contains("first `compact`"));
    assert!(backup.exists());

    rouchdb()
        .args(["get", p, "a"])
        .assert()
        .success()
        .stdout(predicate::str::contains(REV));

    rouchdb()
        .args(["migrate", p])
        .assert()
        .success()
        .stdout(predicate::str::contains("already current"));

    // The backup is still a 0.4 file: 0.5 refuses it too.
    rouchdb()
        .args(["info", backup.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("rouchdb migrate"));
}

#[test]
fn migrate_refuses_an_existing_backup_and_supports_no_backup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.redb");
    legacy_file(&path);
    let p = path.to_str().unwrap();
    let taken = dir.path().join("taken");
    std::fs::write(&taken, b"keep me").unwrap();

    rouchdb()
        .args(["migrate", p, "--backup", taken.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"));
    assert_eq!(std::fs::read(&taken).unwrap(), b"keep me");

    rouchdb()
        .args(["migrate", p, "--backup", "x", "--no-backup"])
        .assert()
        .failure();

    rouchdb()
        .args(["migrate", p, "--no-backup"])
        .assert()
        .success()
        .stdout(predicate::str::contains("status: upgraded"))
        .stdout(predicate::str::contains("backup:").not());
    assert!(!dir.path().join("old.redb.rouchdb-0.4.bak").exists());

    rouchdb()
        .args(["migrate", dir.path().join("missing.redb").to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("does not exist"));
}
