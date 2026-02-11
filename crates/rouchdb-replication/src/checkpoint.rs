use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};

use rouchdb_core::adapter::Adapter;
use rouchdb_core::document::Seq;
use rouchdb_core::error::Result;

/// A checkpoint document stored as `_local/{replication_id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointDoc {
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
    pub fn new(source_id: &str, target_id: &str) -> Self {
        let replication_id = generate_replication_id(source_id, target_id);
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
        let source_cp = self.read_from(source).await;
        let target_cp = self.read_from(target).await;

        match (source_cp, target_cp) {
            (Ok(s), Ok(t)) => Ok(compare_checkpoints(&s, &t)),
            _ => Ok(Seq::zero()), // No checkpoint found, start from beginning
        }
    }

    /// Write the checkpoint to both source and target.
    pub async fn write_checkpoint(
        &self,
        source: &dyn Adapter,
        target: &dyn Adapter,
        last_seq: Seq,
    ) -> Result<()> {
        let doc = self.build_checkpoint_doc(last_seq);
        let json = serde_json::to_value(&doc)?;

        // Write to both sides — fail if either side fails to keep them in sync
        let source_result = source.put_local(&self.replication_id, json.clone()).await;
        let target_result = target.put_local(&self.replication_id, json).await;

        match (source_result, target_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(e), _) | (_, Err(e)) => Err(e),
        }
    }

    async fn read_from(&self, adapter: &dyn Adapter) -> Result<CheckpointDoc> {
        let json = adapter.get_local(&self.replication_id).await?;
        let doc: CheckpointDoc = serde_json::from_value(json)?;
        Ok(doc)
    }

    fn build_checkpoint_doc(&self, last_seq: Seq) -> CheckpointDoc {
        CheckpointDoc {
            last_seq: last_seq.clone(),
            session_id: self.session_id.clone(),
            version: 1,
            replicator: "rouchdb".into(),
            history: vec![CheckpointHistory {
                last_seq,
                session_id: self.session_id.clone(),
            }],
        }
    }
}

/// Generate a deterministic replication ID from source and target identifiers.
fn generate_replication_id(source_id: &str, target_id: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(source_id.as_bytes());
    hasher.update(target_id.as_bytes());
    let hash = format!("{:x}", hasher.finalize());
    // Replace chars that are special in CouchDB URLs
    hash.replace('/', ".").replace('+', "_")
}

/// Compare source and target checkpoints to find the last common sequence.
///
/// Returns the original `Seq` value (preserving opaque strings from CouchDB)
/// rather than converting to numeric, so it can be passed back as `since`.
fn compare_checkpoints(source: &CheckpointDoc, target: &CheckpointDoc) -> Seq {
    // If sessions match, use the sequence directly
    if source.session_id == target.session_id {
        return if source.last_seq.as_num() <= target.last_seq.as_num() {
            source.last_seq.clone()
        } else {
            target.last_seq.clone()
        };
    }

    // Walk through histories to find a common session
    for sh in &source.history {
        for th in &target.history {
            if sh.session_id == th.session_id {
                return if sh.last_seq.as_num() <= th.last_seq.as_num() {
                    sh.last_seq.clone()
                } else {
                    th.last_seq.clone()
                };
            }
        }
    }

    // No common point found, start from beginning
    Seq::zero()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replication_id_deterministic() {
        let id1 = generate_replication_id("source_a", "target_b");
        let id2 = generate_replication_id("source_a", "target_b");
        assert_eq!(id1, id2);

        let id3 = generate_replication_id("source_a", "target_c");
        assert_ne!(id1, id3);
    }

    #[test]
    fn compare_same_session() {
        let cp = CheckpointDoc {
            last_seq: Seq::Num(42),
            session_id: "sess1".into(),
            version: 1,
            replicator: "rouchdb".into(),
            history: vec![],
        };
        assert_eq!(compare_checkpoints(&cp, &cp).as_num(), 42);
    }

    #[test]
    fn compare_different_session_with_history() {
        let source = CheckpointDoc {
            last_seq: Seq::Num(50),
            session_id: "sess2".into(),
            version: 1,
            replicator: "rouchdb".into(),
            history: vec![
                CheckpointHistory {
                    last_seq: Seq::Num(50),
                    session_id: "sess2".into(),
                },
                CheckpointHistory {
                    last_seq: Seq::Num(30),
                    session_id: "sess1".into(),
                },
            ],
        };
        let target = CheckpointDoc {
            last_seq: Seq::Num(40),
            session_id: "sess3".into(),
            version: 1,
            replicator: "rouchdb".into(),
            history: vec![
                CheckpointHistory {
                    last_seq: Seq::Num(40),
                    session_id: "sess3".into(),
                },
                CheckpointHistory {
                    last_seq: Seq::Num(30),
                    session_id: "sess1".into(),
                },
            ],
        };
        // Common session "sess1" at seq 30
        assert_eq!(compare_checkpoints(&source, &target).as_num(), 30);
    }

    #[test]
    fn compare_no_common_session() {
        let source = CheckpointDoc {
            last_seq: Seq::Num(50),
            session_id: "a".into(),
            version: 1,
            replicator: "rouchdb".into(),
            history: vec![],
        };
        let target = CheckpointDoc {
            last_seq: Seq::Num(40),
            session_id: "b".into(),
            version: 1,
            replicator: "rouchdb".into(),
            history: vec![],
        };
        assert_eq!(compare_checkpoints(&source, &target).as_num(), 0);
    }
}
