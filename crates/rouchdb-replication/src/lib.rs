/// CouchDB replication protocol implementation.
///
/// Implements the standard CouchDB replication algorithm:
/// 1. Generate deterministic replication ID
/// 2. Read checkpoint from both source and target
/// 3. Fetch changes from source since last checkpoint
/// 4. Compute revision diff against target
/// 5. Fetch missing documents from source
/// 6. Write to target with new_edits=false
/// 7. Save checkpoint to both sides
mod checkpoint;
mod protocol;

pub use checkpoint::Checkpointer;
pub use protocol::{
    ReplicationEvent, ReplicationFilter, ReplicationHandle, ReplicationOptions, ReplicationResult,
    replicate, replicate_live, replicate_with_events,
};
