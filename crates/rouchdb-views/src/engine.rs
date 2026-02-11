use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use rouchdb_core::adapter::Adapter;
use rouchdb_core::document::*;
use rouchdb_core::error::Result;

/// A map function that takes a document JSON and returns emitted (key, value) pairs.
pub type MapFn =
    Arc<dyn Fn(&serde_json::Value) -> Vec<(serde_json::Value, serde_json::Value)> + Send + Sync>;

/// A persistent view index that is incrementally updated.
pub struct PersistentViewIndex {
    pub ddoc: String,
    pub view_name: String,
    pub last_seq: Seq,
    /// doc_id -> list of emitted (key, value) pairs.
    pub entries: BTreeMap<String, Vec<(serde_json::Value, serde_json::Value)>>,
}

/// Engine for building and querying persistent views.
///
/// Views are defined as Rust closures (map functions). The engine
/// incrementally updates indexes by reading the changes feed since
/// the last known sequence.
pub struct ViewEngine {
    indexes: HashMap<String, PersistentViewIndex>,
    map_fns: HashMap<String, MapFn>,
}

impl ViewEngine {
    pub fn new() -> Self {
        Self {
            indexes: HashMap::new(),
            map_fns: HashMap::new(),
        }
    }

    /// Register a Rust map function for a design doc view.
    pub fn register_map<F>(&mut self, ddoc: &str, view_name: &str, f: F)
    where
        F: Fn(&serde_json::Value) -> Vec<(serde_json::Value, serde_json::Value)>
            + Send
            + Sync
            + 'static,
    {
        let key = format!("{}/{}", ddoc, view_name);
        self.map_fns.insert(key, Arc::new(f));
    }

    /// Update a view index by fetching changes since the last known seq.
    pub async fn update_index(
        &mut self,
        adapter: &dyn Adapter,
        ddoc: &str,
        view_name: &str,
    ) -> Result<()> {
        let key = format!("{}/{}", ddoc, view_name);

        let map_fn = self
            .map_fns
            .get(&key)
            .ok_or_else(|| {
                rouchdb_core::error::RouchError::BadRequest(format!(
                    "no map function registered for {}/{}",
                    ddoc, view_name
                ))
            })?
            .clone();

        let index = self
            .indexes
            .entry(key)
            .or_insert_with(|| PersistentViewIndex {
                ddoc: ddoc.into(),
                view_name: view_name.into(),
                last_seq: Seq::default(),
                entries: BTreeMap::new(),
            });

        let changes = adapter
            .changes(ChangesOptions {
                since: index.last_seq.clone(),
                include_docs: true,
                ..Default::default()
            })
            .await?;

        for event in &changes.results {
            // Remove old entries for this doc
            index.entries.remove(&event.id);

            // Skip design docs and deleted docs
            if event.deleted || event.id.starts_with("_design/") {
                continue;
            }

            if let Some(ref doc) = event.doc {
                let emitted = map_fn(doc);
                if !emitted.is_empty() {
                    index.entries.insert(event.id.clone(), emitted);
                }
            }
        }

        index.last_seq = changes.last_seq;
        Ok(())
    }

    /// Get a view index by ddoc/view_name.
    pub fn get_index(&self, ddoc: &str, view_name: &str) -> Option<&PersistentViewIndex> {
        let key = format!("{}/{}", ddoc, view_name);
        self.indexes.get(&key)
    }

    /// Get all registered index names.
    pub fn index_names(&self) -> Vec<String> {
        self.indexes.keys().cloned().collect()
    }

    /// Remove indexes not in the given set of valid names.
    pub fn remove_indexes_not_in(&mut self, valid: &std::collections::HashSet<String>) {
        self.indexes.retain(|k, _| valid.contains(k));
        self.map_fns.retain(|k, _| valid.contains(k));
    }
}

impl Default for ViewEngine {
    fn default() -> Self {
        Self::new()
    }
}
