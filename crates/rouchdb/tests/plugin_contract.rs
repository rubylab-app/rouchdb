//! The `Plugin` contract for every write of the `Database` API, on the
//! memory and the redb backend:
//!
//! - `before_write` runs in registration order, each plugin seeing the
//!   changes of the previous ones, and a rejection is returned unchanged to
//!   the caller with nothing written;
//! - `after_write` runs in registration order once the write is committed;
//!   its error is returned to the caller but does not undo the write;
//! - `on_destroy` runs in registration order and an error stops the
//!   destruction.

mod backends;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use backends::{Backend, KINDS};
use rouchdb::{
    BulkDocsOptions, Database, DesignDocument, DocResult, Document, GetOptions, Plugin, Result,
    RouchError, Seq,
};

type Log = Arc<Mutex<Vec<String>>>;

/// An error a plugin returns, and how to recognize it.
type ErrorCase = (fn() -> RouchError, fn(&RouchError) -> bool);

/// What a probe plugin does in `before_write`.
#[derive(Clone, Copy)]
enum Before {
    /// Accept the documents unchanged.
    Pass,
    /// Append the plugin name to each document's `stamps`.
    Stamp,
    /// Reject the write.
    Reject(fn() -> RouchError),
}

/// A plugin that logs its calls into a shared log.
struct Probe {
    name: &'static str,
    log: Log,
    before: Before,
    /// Fails the first `after_write` with this error (a flaky audit log,
    /// say), then succeeds.
    after_error: Mutex<Option<fn() -> RouchError>>,
    destroy_error: Option<fn() -> RouchError>,
}

impl Probe {
    fn new(name: &'static str, log: &Log) -> Self {
        Probe {
            name,
            log: log.clone(),
            before: Before::Pass,
            after_error: Mutex::new(None),
            destroy_error: None,
        }
    }

    fn before(mut self, before: Before) -> Self {
        self.before = before;
        self
    }

    fn after_error_once(self, error: fn() -> RouchError) -> Self {
        *self.after_error.lock().unwrap() = Some(error);
        self
    }

    fn destroy_error(mut self, error: fn() -> RouchError) -> Self {
        self.destroy_error = Some(error);
        self
    }

    fn push(&self, entry: String) {
        self.log.lock().unwrap().push(entry);
    }
}

#[async_trait::async_trait]
impl Plugin for Probe {
    fn name(&self) -> &str {
        self.name
    }

    async fn before_write(&self, docs: &mut Vec<Document>) -> Result<()> {
        for doc in docs.iter() {
            // What the previous plugins stamped on the document.
            self.push(format!(
                "{}.before {} {}",
                self.name, doc.id, doc.data["stamps"]
            ));
        }
        match self.before {
            Before::Pass => Ok(()),
            Before::Stamp => {
                for doc in docs.iter_mut() {
                    let mut stamps = doc.data["stamps"].as_array().cloned().unwrap_or_default();
                    stamps.push(self.name.into());
                    doc.data["stamps"] = stamps.into();
                }
                Ok(())
            }
            Before::Reject(error) => Err(error()),
        }
    }

    async fn after_write(&self, results: &[DocResult]) -> Result<()> {
        for r in results {
            self.push(format!(
                "{}.after {} {}",
                self.name,
                r.id,
                r.rev.as_deref().unwrap_or("-")
            ));
        }
        match self.after_error.lock().unwrap().take() {
            Some(error) => Err(error()),
            None => Ok(()),
        }
    }

    async fn on_destroy(&self) -> Result<()> {
        self.push(format!("{}.destroy", self.name));
        match self.destroy_error {
            Some(error) => Err(error()),
            None => Ok(()),
        }
    }
}

fn take(log: &Log) -> Vec<String> {
    std::mem::take(&mut *log.lock().unwrap())
}

// ---------------------------------------------------------------------------
// The writes of the Database API
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Op {
    Put,
    Update,
    Remove,
    Post,
    BulkDocs,
    PutDesign,
    PutAttachment,
    RemoveAttachment,
}

/// Writes of a document body, which `before_write` sees.
const DOC_OPS: [Op; 6] = [
    Op::Put,
    Op::Update,
    Op::Remove,
    Op::Post,
    Op::BulkDocs,
    Op::PutDesign,
];

/// Attachment writes.
const ATTACHMENT_OPS: [Op; 2] = [Op::PutAttachment, Op::RemoveAttachment];

impl Op {
    /// The document the operation writes.
    fn id(self) -> &'static str {
        match self {
            Op::Post => "posted",
            Op::PutDesign => "_design/app",
            _ => "d",
        }
    }

    /// Store what the operation needs, bypassing the plugins, and return
    /// the revision it builds on.
    async fn setup(self, db: &Database) -> Option<String> {
        let adapter = db.adapter();
        let doc = Document::from_json(serde_json::json!({"_id": "d", "v": 0})).unwrap();
        match self {
            Op::Put | Op::Post | Op::BulkDocs | Op::PutDesign => None,
            Op::Update | Op::Remove | Op::PutAttachment | Op::RemoveAttachment => {
                let rev = adapter
                    .bulk_docs(vec![doc], BulkDocsOptions::new())
                    .await
                    .unwrap()[0]
                    .rev
                    .clone()
                    .unwrap();
                if !matches!(self, Op::RemoveAttachment) {
                    return Some(rev);
                }
                adapter
                    .put_attachment("d", "a.txt", &rev, b"hi".to_vec(), "text/plain")
                    .await
                    .unwrap()
                    .rev
            }
        }
    }

    /// Run the operation through the `Database` API.
    async fn run(self, db: &Database, rev: Option<&str>) -> Result<DocResult> {
        let body = serde_json::json!({"v": 1});
        match self {
            Op::Put => db.put("d", body).await,
            Op::Update => db.update("d", rev.unwrap(), body).await,
            Op::Remove => db.remove("d", rev.unwrap()).await,
            Op::Post => db.post(serde_json::json!({"_id": "posted", "v": 1})).await,
            Op::BulkDocs => {
                let doc = Document::from_json(serde_json::json!({"_id": "d", "v": 1})).unwrap();
                let mut results = db.bulk_docs(vec![doc], BulkDocsOptions::new()).await?;
                assert_eq!(results.len(), 1, "{results:?}");
                Ok(results.remove(0))
            }
            Op::PutDesign => {
                db.put_design(DesignDocument {
                    id: "_design/app".into(),
                    rev: None,
                    views: HashMap::new(),
                    filters: HashMap::new(),
                    validate_doc_update: None,
                    shows: HashMap::new(),
                    lists: HashMap::new(),
                    updates: HashMap::new(),
                    language: None,
                    ..Default::default()
                })
                .await
            }
            Op::PutAttachment => {
                db.put_attachment("d", "a.txt", rev.unwrap(), b"hi".to_vec(), "text/plain")
                    .await
            }
            Op::RemoveAttachment => db.remove_attachment("d", "a.txt", rev.unwrap()).await,
        }
    }
}

/// The database's update sequence and the winning revision of the
/// operation's document.
async fn state(db: &Database, op: Op) -> (Seq, Option<String>) {
    let seq = db.info().await.unwrap().update_seq;
    let rev = match db.get_with_opts(op.id(), GetOptions::default()).await {
        Ok(doc) => Some(doc.rev.unwrap().to_string()),
        Err(RouchError::NotFound(_)) => None,
        Err(e) => panic!("{op:?}: {e}"),
    };
    (seq, rev)
}

// ---------------------------------------------------------------------------
// before_write
// ---------------------------------------------------------------------------

#[tokio::test]
async fn before_write_runs_in_order_and_chains_changes() {
    for kind in KINDS {
        for op in DOC_OPS {
            let log = Log::default();
            let b = Backend::open(kind, "plugins").configure(|db| {
                db.with_plugin(Arc::new(Probe::new("a", &log).before(Before::Stamp)))
                    .with_plugin(Arc::new(Probe::new("b", &log).before(Before::Stamp)))
                    .with_plugin(Arc::new(Probe::new("c", &log)))
            });
            let rev = op.setup(&b.db).await;
            let result = op.run(&b.db, rev.as_deref()).await.unwrap();
            assert!(result.ok, "{kind} {op:?}: {result:?}");
            let id = op.id();
            let new_rev = result.rev.clone().unwrap();
            assert_eq!(
                take(&log),
                [
                    format!("a.before {id} null"),
                    format!("b.before {id} [\"a\"]"),
                    format!("c.before {id} [\"a\",\"b\"]"),
                    format!("a.after {id} {new_rev}"),
                    format!("b.after {id} {new_rev}"),
                    format!("c.after {id} {new_rev}"),
                ],
                "{kind} {op:?}"
            );
            // What the plugins changed is what gets stored.
            let stored =
                b.db.get_with_opts(
                    id,
                    GetOptions {
                        rev: Some(new_rev),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                stored.data["stamps"],
                serde_json::json!(["a", "b"]),
                "{kind} {op:?}"
            );
        }
    }
}

#[tokio::test]
async fn before_write_rejection_is_returned_and_nothing_is_written() {
    let errors: [ErrorCase; 3] = [
        (
            || RouchError::Forbidden("no".into()),
            |e| matches!(e, RouchError::Forbidden(r) if r == "no"),
        ),
        (
            || RouchError::Unauthorized,
            |e| matches!(e, RouchError::Unauthorized),
        ),
        (
            || RouchError::BadRequest("invalid".into()),
            |e| matches!(e, RouchError::BadRequest(r) if r == "invalid"),
        ),
    ];
    for kind in KINDS {
        for op in DOC_OPS {
            for (error, is_expected) in errors {
                let log = Log::default();
                let b = Backend::open(kind, "plugins").configure(|db| {
                    db.with_plugin(Arc::new(Probe::new("a", &log).before(Before::Stamp)))
                        .with_plugin(Arc::new(
                            Probe::new("gate", &log).before(Before::Reject(error)),
                        ))
                        .with_plugin(Arc::new(Probe::new("c", &log)))
                });
                let rev = op.setup(&b.db).await;
                let before = state(&b.db, op).await;

                let result = op.run(&b.db, rev.as_deref()).await;
                let err = result.expect_err(&format!("{kind} {op:?} was written"));
                assert!(is_expected(&err), "{kind} {op:?}: {err:?}");

                // Nothing was written: same sequence, same document state.
                assert_eq!(state(&b.db, op).await, before, "{kind} {op:?}");
                // The rejection stops the chain; no after_write.
                let id = op.id();
                assert_eq!(
                    take(&log),
                    [
                        format!("a.before {id} null"),
                        format!("gate.before {id} [\"a\"]"),
                    ],
                    "{kind} {op:?}"
                );
            }
        }
    }
}

#[tokio::test]
async fn before_write_rejection_leaves_a_batch_unwritten() {
    for kind in KINDS {
        let log = Log::default();
        let b = Backend::open(kind, "plugins").configure(|db| {
            db.with_plugin(Arc::new(
                Probe::new("gate", &log)
                    .before(Before::Reject(|| RouchError::Forbidden("no".into()))),
            ))
        });
        let docs = ["x", "y", "z"]
            .iter()
            .map(|id| Document::from_json(serde_json::json!({"_id": id})).unwrap())
            .collect();
        let result = b.db.bulk_docs(docs, BulkDocsOptions::new()).await;
        assert!(
            matches!(result, Err(RouchError::Forbidden(_))),
            "{kind}: {result:?}"
        );
        let info = b.db.info().await.unwrap();
        assert_eq!(
            (info.doc_count, info.update_seq),
            (0, Seq::Num(0)),
            "{kind}"
        );
        for id in ["x", "y", "z"] {
            assert!(
                matches!(b.db.get(id).await, Err(RouchError::NotFound(_))),
                "{kind} {id}"
            );
        }
    }
}

/// `put_attachment` and `remove_attachment` write a new revision of the
/// document, so `before_write` sees it and can reject it, as CouchDB runs
/// `validate_doc_update` on attachment updates.
#[tokio::test]
async fn before_write_runs_on_attachment_writes() {
    for kind in KINDS {
        for op in ATTACHMENT_OPS {
            let log = Log::default();
            let b = Backend::open(kind, "plugins").configure(|db| {
                db.with_plugin(Arc::new(
                    Probe::new("gate", &log)
                        .before(Before::Reject(|| RouchError::Forbidden("read-only".into()))),
                ))
            });
            let rev = op.setup(&b.db).await;
            let before = state(&b.db, op).await;
            let result = op.run(&b.db, rev.as_deref()).await;
            assert!(
                matches!(result, Err(RouchError::Forbidden(_))),
                "{kind} {op:?}: {result:?}"
            );
            assert_eq!(state(&b.db, op).await, before, "{kind} {op:?}");
            assert_eq!(take(&log), ["gate.before d null"], "{kind} {op:?}");
        }
    }
}

/// A plugin that records the documents `before_write` gets, then either
/// changes them (which must not be stored) or drops them.
struct Capture {
    seen: Mutex<Vec<Document>>,
    drop_docs: bool,
}

#[async_trait::async_trait]
impl Plugin for Capture {
    fn name(&self) -> &str {
        "capture"
    }

    async fn before_write(&self, docs: &mut Vec<Document>) -> Result<()> {
        self.seen.lock().unwrap().extend(docs.iter().cloned());
        if self.drop_docs {
            docs.clear();
        }
        for doc in docs.iter_mut() {
            doc.data["stamp"] = true.into();
            doc.attachments.clear();
        }
        Ok(())
    }
}

impl Capture {
    fn take(&self) -> Vec<Document> {
        std::mem::take(&mut *self.seen.lock().unwrap())
    }
}

fn attachment_names(doc: &Document) -> Vec<&str> {
    let mut names: Vec<&str> = doc.attachments.keys().map(String::as_str).collect();
    names.sort_unstable();
    names
}

/// `before_write` gets the revision an attachment write creates (the
/// parent's body and attachments with the change applied); its changes are
/// not stored.
#[tokio::test]
async fn before_write_sees_attachment_edits_without_changing_them() {
    for kind in KINDS {
        let capture = Arc::new(Capture {
            seen: Mutex::new(Vec::new()),
            drop_docs: false,
        });
        let b = Backend::open(kind, "plugins").configure(|db| db.with_plugin(capture.clone()));
        let adapter = b.db.adapter();
        let doc = Document::from_json(serde_json::json!({"_id": "d", "v": 0})).unwrap();
        let r1 = adapter
            .bulk_docs(vec![doc], BulkDocsOptions::new())
            .await
            .unwrap()[0]
            .rev
            .clone()
            .unwrap();
        let r2 = adapter
            .put_attachment("d", "a.txt", &r1, b"hi".to_vec(), "text/plain")
            .await
            .unwrap()
            .rev
            .unwrap();

        let bytes = vec![0u8, 1, 255];
        let r3 =
            b.db.put_attachment("d", "b.bin", &r2, bytes.clone(), "application/x-raw")
                .await
                .unwrap()
                .rev
                .unwrap();
        let seen = capture.take();
        assert_eq!(seen.len(), 1, "{kind}");
        let probe = &seen[0];
        assert_eq!(probe.id, "d", "{kind}");
        assert_eq!(probe.rev.as_ref().unwrap().to_string(), r2, "{kind}");
        assert!(!probe.deleted, "{kind}");
        assert_eq!(probe.data, serde_json::json!({"v": 0}), "{kind}");
        assert_eq!(attachment_names(probe), ["a.txt", "b.bin"], "{kind}");
        assert!(probe.attachments["a.txt"].stub, "{kind}");
        let new = &probe.attachments["b.bin"];
        assert_eq!(new.data.as_deref(), Some(&bytes[..]), "{kind}");
        assert_eq!(
            (new.content_type.as_str(), new.length, new.stub),
            ("application/x-raw", 3, false),
            "{kind}"
        );

        // The plugin's changes (a stamp, no attachments) are not stored.
        let stored = b.db.get("d").await.unwrap();
        assert_eq!(stored.rev.as_ref().unwrap().to_string(), r3, "{kind}");
        assert_eq!(stored.data, serde_json::json!({"v": 0}), "{kind}");
        assert_eq!(attachment_names(&stored), ["a.txt", "b.bin"], "{kind}");
        assert_eq!(stored.attachments["b.bin"].digest, new.digest, "{kind}");

        b.db.remove_attachment("d", "a.txt", &r3).await.unwrap();
        let seen = capture.take();
        assert_eq!(seen.len(), 1, "{kind}");
        assert_eq!(seen[0].rev.as_ref().unwrap().to_string(), r3, "{kind}");
        assert_eq!(seen[0].data, serde_json::json!({"v": 0}), "{kind}");
        assert_eq!(attachment_names(&seen[0]), ["b.bin"], "{kind}");

        // A write on a stale revision shows that revision, then conflicts.
        let err =
            b.db.put_attachment("d", "c.txt", &r2, b"c".to_vec(), "text/plain")
                .await
                .unwrap_err();
        assert!(matches!(err, RouchError::Conflict), "{kind}: {err:?}");
        let seen = capture.take();
        assert_eq!(seen[0].rev.as_ref().unwrap().to_string(), r2, "{kind}");
        assert_eq!(attachment_names(&seen[0]), ["a.txt", "c.txt"], "{kind}");

        // A write on a document that cannot be read still goes through
        // before_write (with an empty body), then fails as without plugins.
        let err =
            b.db.put_attachment("nope", "x", &r1, b"x".to_vec(), "text/plain")
                .await
                .unwrap_err();
        assert!(matches!(err, RouchError::NotFound(_)), "{kind}: {err:?}");
        let seen = capture.take();
        assert_eq!(seen.len(), 1, "{kind}");
        assert_eq!(seen[0].id, "nope", "{kind}");
        assert_eq!(seen[0].data, serde_json::json!({}), "{kind}");
        assert_eq!(attachment_names(&seen[0]), ["x"], "{kind}");
    }
}

#[tokio::test]
async fn dropping_an_attachment_edit_rejects_it() {
    for kind in KINDS {
        for op in ATTACHMENT_OPS {
            let b = Backend::open(kind, "plugins").configure(|db| {
                db.with_plugin(Arc::new(Capture {
                    seen: Mutex::new(Vec::new()),
                    drop_docs: true,
                }))
            });
            let rev = op.setup(&b.db).await;
            let before = state(&b.db, op).await;
            let result = op.run(&b.db, rev.as_deref()).await;
            assert!(
                matches!(&result, Err(RouchError::Forbidden(r)) if r == "dropped by plugin capture"),
                "{kind} {op:?}: {result:?}"
            );
            assert_eq!(state(&b.db, op).await, before, "{kind} {op:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// after_write
// ---------------------------------------------------------------------------

#[tokio::test]
async fn after_write_runs_in_order_on_attachment_writes() {
    for kind in KINDS {
        for op in ATTACHMENT_OPS {
            let log = Log::default();
            let b = Backend::open(kind, "plugins").configure(|db| {
                db.with_plugin(Arc::new(Probe::new("a", &log)))
                    .with_plugin(Arc::new(Probe::new("b", &log)))
            });
            let rev = op.setup(&b.db).await;
            let result = op.run(&b.db, rev.as_deref()).await.unwrap();
            let new_rev = result.rev.unwrap();
            assert_eq!(
                take(&log)
                    .into_iter()
                    .filter(|e| e.contains(".after "))
                    .collect::<Vec<_>>(),
                [
                    format!("a.after d {new_rev}"),
                    format!("b.after d {new_rev}")
                ],
                "{kind} {op:?}"
            );
            assert_eq!(state(&b.db, op).await.1, Some(new_rev), "{kind} {op:?}");
        }
    }
}

#[tokio::test]
async fn after_write_error_is_returned_but_the_write_is_kept() {
    for kind in KINDS {
        for op in DOC_OPS.into_iter().chain(ATTACHMENT_OPS) {
            let log = Log::default();
            let b = Backend::open(kind, "plugins").configure(|db| {
                db.with_plugin(Arc::new(
                    Probe::new("audit", &log)
                        .after_error_once(|| RouchError::Forbidden("audit".into())),
                ))
                .with_plugin(Arc::new(Probe::new("later", &log)))
            });
            let rev = op.setup(&b.db).await;
            let (seq, _) = state(&b.db, op).await;

            let result = op.run(&b.db, rev.as_deref()).await;
            assert!(
                matches!(&result, Err(RouchError::Forbidden(r)) if r == "audit"),
                "{kind} {op:?}: {result:?}"
            );

            // The write is committed under the revision after_write saw, and
            // the plugins after the failing one are not called.
            let after: Vec<String> = take(&log)
                .into_iter()
                .filter(|e| e.contains(".after "))
                .collect();
            assert_eq!(after.len(), 1, "{kind} {op:?}: {after:?}");
            let prefix = format!("audit.after {} ", op.id());
            let written = after[0].strip_prefix(&prefix).unwrap().to_string();
            let (new_seq, current) = state(&b.db, op).await;
            assert_eq!(new_seq.as_num(), seq.as_num() + 1, "{kind} {op:?}");
            if matches!(op, Op::Remove) {
                assert_eq!(current, None, "{kind} {op:?}");
            } else {
                assert_eq!(current.as_deref(), Some(written.as_str()), "{kind} {op:?}");
            }

            // Retrying the same write (the audit plugin now succeeds)
            // conflicts with the committed one.
            let retry = op.run(&b.db, rev.as_deref()).await;
            match op {
                // A bulk write reports the conflict per document.
                Op::BulkDocs => {
                    let r = retry.unwrap();
                    assert_eq!(r.error.as_deref(), Some("conflict"), "{kind}: {r:?}");
                }
                // Like CouchDB's DELETE, removing a document that is already
                // deleted is not_found ("deleted"), not a conflict.
                Op::Remove => assert!(
                    matches!(retry, Err(RouchError::NotFound(_))),
                    "{kind} {op:?}: {retry:?}"
                ),
                _ => assert!(
                    matches!(retry, Err(RouchError::Conflict)),
                    "{kind} {op:?}: {retry:?}"
                ),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// on_destroy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn on_destroy_runs_in_order_before_the_data_is_destroyed() {
    for kind in KINDS {
        let log = Log::default();
        let b = Backend::open(kind, "plugins").configure(|db| {
            db.with_plugin(Arc::new(Probe::new("a", &log)))
                .with_plugin(Arc::new(Probe::new("b", &log)))
        });
        b.db.put("d", serde_json::json!({})).await.unwrap();
        take(&log);

        b.db.destroy().await.unwrap();
        assert_eq!(take(&log), ["a.destroy", "b.destroy"], "{kind}");
        assert!(
            matches!(b.db.get("d").await, Err(RouchError::NotFound(_))),
            "{kind}"
        );
        assert_eq!(b.db.info().await.unwrap().doc_count, 0, "{kind}");
    }
}

#[tokio::test]
async fn on_destroy_error_stops_the_destruction() {
    for kind in KINDS {
        let log = Log::default();
        let b = Backend::open(kind, "plugins").configure(|db| {
            db.with_plugin(Arc::new(Probe::new("a", &log)))
                .with_plugin(Arc::new(
                    Probe::new("keeper", &log)
                        .destroy_error(|| RouchError::Forbidden("keep".into())),
                ))
                .with_plugin(Arc::new(Probe::new("c", &log)))
        });
        let rev =
            b.db.put("d", serde_json::json!({"v": 1}))
                .await
                .unwrap()
                .rev;
        take(&log);

        let result = b.db.destroy().await;
        assert!(
            matches!(&result, Err(RouchError::Forbidden(r)) if r == "keep"),
            "{kind}: {result:?}"
        );
        assert_eq!(take(&log), ["a.destroy", "keeper.destroy"], "{kind}");
        // The data is untouched.
        let doc = b.db.get("d").await.unwrap();
        assert_eq!(doc.rev.map(|r| r.to_string()), rev, "{kind}");
        assert_eq!(doc.data["v"], 1, "{kind}");
        assert_eq!(b.db.info().await.unwrap().doc_count, 1, "{kind}");
    }
}
