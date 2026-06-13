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

        let source_cp = to_opt(self.read_from(source).await)?;
        let target_cp = to_opt(self.read_from(target).await)?;

        match (source_cp, target_cp) {
            (Some(s), Some(t)) => Ok(compare_checkpoints(&s, &t)),
            _ => Ok(Seq::zero()), // at least one side has never been checkpointed
        }
    }

    /// Write the checkpoint to both source and target.
    pub async fn write_checkpoint(
        &self,
        source: &dyn Adapter,
        target: &dyn Adapter,
        last_seq: Seq,
    ) -> Result<()> {
        // Carry forward existing history so cross-session divergence recovery
        // works (each side keeps its own _rev, so write them independently).
        let prior_history = self
            .read_from(source)
            .await
            .map(|cp| cp.history)
            .unwrap_or_default();

        self.write_one(source, last_seq.clone(), &prior_history)
            .await?;
        self.write_one(target, last_seq, &prior_history).await?;
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
