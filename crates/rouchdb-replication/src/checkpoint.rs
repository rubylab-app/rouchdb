use std::sync::atomic::{AtomicBool, Ordering};

use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};

use rouchdb_core::adapter::Adapter;
use rouchdb_core::document::Seq;
use rouchdb_core::error::{Result, RouchError};

/// Maximum number of history entries to retain per checkpoint.
const MAX_HISTORY: usize = 50;

/// A checkpoint document stored as `_local/{replication_id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointDoc {
    /// The `_local` doc revision, round-tripped so CouchDB updates don't 409.
    #[serde(rename = "_rev", default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    pub last_seq: Seq,
    pub session_id: String,
    pub version: u32,
    pub replicator: String,
    pub history: Vec<CheckpointHistory>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointHistory {
    pub last_seq: Seq,
    pub session_id: String,
}

/// Manages checkpoints for a replication session.
pub struct Checkpointer {
    replication_id: String,
    session_id: String,
    /// Set once the source refused a `_local` write (a read-only source):
    /// from then on only the target's checkpoint is read and written.
    read_only_source: AtomicBool,
}

/// Errors meaning the adapter will never accept the write.
fn is_forbidden(e: &RouchError) -> bool {
    matches!(e, RouchError::Forbidden(_) | RouchError::Unauthorized)
}

impl Checkpointer {
    /// Create a new checkpointer for a replication between source and target.
    ///
    /// `filter_fingerprint` is a stable representation of the active filter so
    /// that filtered and unfiltered replications between the same pair of
    /// databases use distinct checkpoints (CouchDB protocol requirement).
    pub fn new(source_id: &str, target_id: &str, filter_fingerprint: &str) -> Self {
        let replication_id = generate_replication_id(source_id, target_id, filter_fingerprint);
        let session_id = uuid::Uuid::new_v4().to_string();
        Self {
            replication_id,
            session_id,
            read_only_source: AtomicBool::new(false),
        }
    }

    pub fn replication_id(&self) -> &str {
        &self.replication_id
    }

    /// Read the checkpoint from both source and target, and find the last
    /// common sequence number.
    pub async fn read_checkpoint(&self, source: &dyn Adapter, target: &dyn Adapter) -> Result<Seq> {
        // Only a missing checkpoint means "start from the beginning"; any other
        // error (auth/network/corruption) must be surfaced rather than silently
        // re-replicating from scratch.
        let to_opt = |r: Result<CheckpointDoc>| -> Result<Option<CheckpointDoc>> {
            match r {
                Ok(doc) => Ok(Some(doc)),
                Err(RouchError::NotFound(_)) => Ok(None),
                Err(e) => Err(e),
            }
        };

        let Some(target_cp) = to_opt(self.read_from(target).await)? else {
            return Ok(Seq::zero()); // never checkpointed on the target
        };
        if self.read_only_source.load(Ordering::Relaxed) {
            return Ok(target_cp.last_seq);
        }

        match to_opt(self.read_from(source).await)? {
            Some(source_cp) => Ok(compare_checkpoints(&source_cp, &target_cp)),
            None => {
                // The target knows this replication but the source does not:
                // either the source was replaced (start over) or it cannot
                // store checkpoints at all. Probe with a write, as PouchDB
                // does, and trust the target alone for a read-only source.
                let probe = self.build_checkpoint_doc(Seq::zero(), Vec::new());
                match source
                    .put_local(&self.replication_id, serde_json::to_value(&probe)?)
                    .await
                {
                    Err(e) if is_forbidden(&e) => {
                        self.read_only_source.store(true, Ordering::Relaxed);
                        Ok(target_cp.last_seq)
                    }
                    _ => Ok(Seq::zero()),
                }
            }
        }
    }

    /// Write the checkpoint to the target, then to the source. A source that
    /// refuses the write (read-only) is remembered and skipped from then on.
    pub async fn write_checkpoint(
        &self,
        source: &dyn Adapter,
        target: &dyn Adapter,
        last_seq: Seq,
    ) -> Result<()> {
        let read_only_source = self.read_only_source.load(Ordering::Relaxed);

        // Carry forward existing history so cross-session divergence recovery
        // works (each side keeps its own _rev, so write them independently).
        let prior_history = self
            .read_from(if read_only_source { target } else { source })
            .await
            .map(|cp| cp.history)
            .unwrap_or_default();

        self.write_one(target, last_seq.clone(), &prior_history)
            .await?;
        if !read_only_source {
            match self.write_one(source, last_seq, &prior_history).await {
                Err(e) if is_forbidden(&e) => {
                    self.read_only_source.store(true, Ordering::Relaxed);
                }
                other => other?,
            }
        }
        Ok(())
    }

    /// Write the checkpoint to one side, injecting its current `_rev` and
    /// retrying once on conflict (required for CouchDB `_local` updates).
    async fn write_one(
        &self,
        adapter: &dyn Adapter,
        last_seq: Seq,
        prior_history: &[CheckpointHistory],
    ) -> Result<()> {
        let current_rev = self.current_rev(adapter).await;
        let mut doc = self.build_checkpoint_doc(last_seq, prior_history.to_vec());
        doc.rev = current_rev;

        match adapter
            .put_local(&self.replication_id, serde_json::to_value(&doc)?)
            .await
        {
            Ok(()) => Ok(()),
            Err(RouchError::Conflict) => {
                // Re-fetch the current rev and retry once.
                doc.rev = self.current_rev(adapter).await;
                adapter
                    .put_local(&self.replication_id, serde_json::to_value(&doc)?)
                    .await
            }
            Err(e) => Err(e),
        }
    }

    async fn current_rev(&self, adapter: &dyn Adapter) -> Option<String> {
        adapter
            .get_local(&self.replication_id)
            .await
            .ok()
            .and_then(|json| json.get("_rev").and_then(|v| v.as_str().map(String::from)))
    }

    async fn read_from(&self, adapter: &dyn Adapter) -> Result<CheckpointDoc> {
        let json = adapter.get_local(&self.replication_id).await?;
        let doc: CheckpointDoc = serde_json::from_value(json)?;
        Ok(doc)
    }

    fn build_checkpoint_doc(
        &self,
        last_seq: Seq,
        mut prior_history: Vec<CheckpointHistory>,
    ) -> CheckpointDoc {
        // Prepend the current session entry, dropping any stale entry for this
        // same session, and cap the total length.
        prior_history.retain(|h| h.session_id != self.session_id);
        let mut history = Vec::with_capacity(prior_history.len() + 1);
        history.push(CheckpointHistory {
            last_seq: last_seq.clone(),
            session_id: self.session_id.clone(),
        });
        history.extend(prior_history);
        history.truncate(MAX_HISTORY);

        CheckpointDoc {
            rev: None,
            last_seq,
            session_id: self.session_id.clone(),
            version: 1,
            replicator: "rouchdb".into(),
            history,
        }
    }
}

/// Generate a deterministic replication ID from source/target identifiers and
/// a filter fingerprint, so distinct filters never share a checkpoint.
fn generate_replication_id(source_id: &str, target_id: &str, filter_fingerprint: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(source_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(target_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(filter_fingerprint.as_bytes());
    let hash = format!("{:x}", hasher.finalize());
    // Replace chars that are special in CouchDB URLs
    hash.replace('/', ".").replace('+', "_")
}

/// Pick the conservative common sequence between two checkpoints.
///
/// Opaque CouchDB string sequences (`"42-g1AAA..."`) cannot be reliably
/// ordered by their numeric prefix, so short-circuit on equality and only
/// trust numeric ordering for genuinely numeric sequences; otherwise fall back
/// to the numeric-min (a harmless re-scan, since replication is idempotent).
fn pick_common(a: &Seq, b: &Seq) -> Seq {
    if a == b {
        return a.clone();
    }
    match (a, b) {
        (Seq::Num(x), Seq::Num(y)) => {
            if x <= y {
                a.clone()
            } else {
                b.clone()
            }
        }
        _ => {
            if a.as_num() <= b.as_num() {
                a.clone()
            } else {
                b.clone()
            }
        }
    }
}

/// Compare source and target checkpoints to find the last common sequence.
///
/// Returns the original `Seq` value (preserving opaque strings from CouchDB)
/// rather than converting to numeric, so it can be passed back as `since`.
fn compare_checkpoints(source: &CheckpointDoc, target: &CheckpointDoc) -> Seq {
    // If sessions match, use the sequence directly
    if source.session_id == target.session_id {
        return pick_common(&source.last_seq, &target.last_seq);
    }

    // Walk through histories to find a common session
    for sh in &source.history {
        for th in &target.history {
            if sh.session_id == th.session_id {
                return pick_common(&sh.last_seq, &th.last_seq);
            }
        }
    }

    // No common point found, start from beginning
    Seq::zero()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cp(last_seq: u64, session: &str, history: Vec<CheckpointHistory>) -> CheckpointDoc {
        CheckpointDoc {
            rev: None,
            last_seq: Seq::Num(last_seq),
            session_id: session.into(),
            version: 1,
            replicator: "rouchdb".into(),
            history,
        }
    }

    fn hist(last_seq: u64, session: &str) -> CheckpointHistory {
        CheckpointHistory {
            last_seq: Seq::Num(last_seq),
            session_id: session.into(),
        }
    }

    #[test]
    fn replication_id_deterministic() {
        let id1 = generate_replication_id("source_a", "target_b", "nofilter");
        let id2 = generate_replication_id("source_a", "target_b", "nofilter");
        assert_eq!(id1, id2);

        let id3 = generate_replication_id("source_a", "target_c", "nofilter");
        assert_ne!(id1, id3);

        // Different filters must yield different replication ids.
        let id4 = generate_replication_id("source_a", "target_b", "docids:a,b");
        assert_ne!(id1, id4);
    }

    #[test]
    fn compare_same_session() {
        let c = cp(42, "sess1", vec![]);
        assert_eq!(compare_checkpoints(&c, &c).as_num(), 42);
    }

    #[test]
    fn compare_different_session_with_history() {
        let source = cp(50, "sess2", vec![hist(50, "sess2"), hist(30, "sess1")]);
        let target = cp(40, "sess3", vec![hist(40, "sess3"), hist(30, "sess1")]);
        // Common session "sess1" at seq 30
        assert_eq!(compare_checkpoints(&source, &target).as_num(), 30);
    }

    #[test]
    fn compare_no_common_session() {
        let source = cp(50, "a", vec![]);
        let target = cp(40, "b", vec![]);
        assert_eq!(compare_checkpoints(&source, &target).as_num(), 0);
    }

    #[test]
    fn compare_same_session_takes_the_smaller_seq_on_either_side() {
        // The target was restored from a backup taken mid-session.
        let ahead = cp(6, "s", vec![hist(6, "s")]);
        let behind = cp(2, "s", vec![hist(2, "s")]);
        assert_eq!(compare_checkpoints(&ahead, &behind), Seq::Num(2));
        assert_eq!(compare_checkpoints(&behind, &ahead), Seq::Num(2));
    }

    #[test]
    fn compare_uses_the_newest_common_session_and_its_smaller_seq() {
        // Histories are newest first. s2 is the newest session both sides
        // know, and each side recorded a different seq for it.
        let source = cp(
            50,
            "s3",
            vec![hist(50, "s3"), hist(40, "s2"), hist(30, "s1")],
        );
        let target = cp(
            45,
            "s4",
            vec![hist(45, "s4"), hist(35, "s2"), hist(30, "s1")],
        );
        assert_eq!(compare_checkpoints(&source, &target), Seq::Num(35));
        assert_eq!(compare_checkpoints(&target, &source), Seq::Num(35));
    }

    #[test]
    fn pick_common_takes_the_smaller_numeric_seq_in_either_order() {
        assert_eq!(pick_common(&Seq::Num(3), &Seq::Num(5)), Seq::Num(3));
        assert_eq!(pick_common(&Seq::Num(5), &Seq::Num(3)), Seq::Num(3));
        assert_eq!(pick_common(&Seq::Num(4), &Seq::Num(4)), Seq::Num(4));
    }

    #[test]
    fn pick_common_takes_the_smaller_opaque_seq_in_either_order() {
        // 3 < 12 by numeric prefix, though "12-..." sorts first as text.
        let low = Seq::Str("3-g1AAAAB".into());
        let high = Seq::Str("12-g1AAAAC".into());
        assert_eq!(pick_common(&low, &high), low);
        assert_eq!(pick_common(&high, &low), low);
        // A numeric seq against an opaque one.
        let opaque = Seq::Str("5-g1AAAAD".into());
        assert_eq!(pick_common(&Seq::Num(7), &opaque), opaque);
        assert_eq!(pick_common(&opaque, &Seq::Num(7)), opaque);
        assert_eq!(pick_common(&Seq::Num(2), &opaque), Seq::Num(2));
    }

    #[test]
    fn only_permission_errors_mark_a_source_read_only() {
        assert!(is_forbidden(&RouchError::Forbidden("read only".into())));
        assert!(is_forbidden(&RouchError::Unauthorized));
        for transient in [
            RouchError::DatabaseError("connection reset".into()),
            RouchError::Conflict,
            RouchError::NotFound("_local/x".into()),
            RouchError::BadRequest("Invalid rev format".into()),
        ] {
            assert!(!is_forbidden(&transient), "{transient:?}");
        }
    }

    fn history_of(doc: &serde_json::Value) -> Vec<(u64, String)> {
        let doc: CheckpointDoc = serde_json::from_value(doc.clone()).unwrap();
        doc.history
            .into_iter()
            .map(|h| (h.last_seq.as_num(), h.session_id))
            .collect()
    }

    #[tokio::test]
    async fn history_keeps_earlier_sessions_newest_first() {
        use rouchdb_adapter_memory::MemoryAdapter;
        let source = MemoryAdapter::new("a");
        let target = MemoryAdapter::new("b");

        let first = Checkpointer::new("a", "b", "nofilter");
        first
            .write_checkpoint(&source, &target, Seq::Num(3))
            .await
            .unwrap();
        let second = Checkpointer::new("a", "b", "nofilter");
        second
            .write_checkpoint(&source, &target, Seq::Num(5))
            .await
            .unwrap();
        // A later write in the same session replaces its own entry.
        second
            .write_checkpoint(&source, &target, Seq::Num(7))
            .await
            .unwrap();

        let expected = vec![
            (7, second.session_id.clone()),
            (3, first.session_id.clone()),
        ];
        for side in [&source, &target] {
            let doc = side.get_local(first.replication_id()).await.unwrap();
            assert_eq!(doc["last_seq"], 7);
            assert_eq!(doc["session_id"], second.session_id.as_str());
            assert_eq!(doc["replicator"], "rouchdb");
            assert_eq!(history_of(&doc), expected);
        }
    }

    #[test]
    fn history_is_capped() {
        let checkpointer = Checkpointer::new("a", "b", "nofilter");
        let prior: Vec<_> = (0..MAX_HISTORY as u64)
            .map(|i| hist(i, &format!("old{i}")))
            .collect();
        let doc = checkpointer.build_checkpoint_doc(Seq::Num(99), prior);
        assert_eq!(doc.history.len(), MAX_HISTORY);
        assert_eq!(doc.history[0].session_id, checkpointer.session_id);
        assert_eq!(doc.history[1].session_id, "old0");
        assert_eq!(
            doc.history[MAX_HISTORY - 1].session_id,
            format!("old{}", MAX_HISTORY - 2)
        );
    }

    #[test]
    fn pick_common_handles_opaque_seqs() {
        // Equal opaque seqs -> that seq.
        let a = Seq::Str("5-abc".into());
        assert_eq!(pick_common(&a, &a), a);
        // Distinct opaque seqs sharing a numeric prefix don't collapse to a
        // spurious "equal"; a deterministic side is chosen.
        let b = Seq::Str("5-xyz".into());
        let picked = pick_common(&a, &b);
        assert!(picked == a || picked == b);
    }
}
