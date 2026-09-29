use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use rouchdb_core::adapter::Adapter;
use rouchdb_core::document::*;
use rouchdb_core::error::Result;
use rouchdb_query::{
    EmittedRow, ReduceFn, StaleOption, ViewQueryOptions, ViewResult, attach_docs, query_sorted,
    sort_emitted,
};

/// A map function that takes a document JSON and returns emitted (key, value) pairs.
pub type MapFn =
    Arc<dyn Fn(&serde_json::Value) -> Vec<(serde_json::Value, serde_json::Value)> + Send + Sync>;

/// A persistent view index that is incrementally updated.
///
/// `#[non_exhaustive]`: built by [`ViewEngine`], read by callers.
#[non_exhaustive]
pub struct PersistentViewIndex {
    pub ddoc: String,
    pub view_name: String,
    pub last_seq: Seq,
    /// doc_id -> list of emitted (key, value) pairs.
    pub entries: BTreeMap<String, Vec<(serde_json::Value, serde_json::Value)>>,
}

/// Bookkeeping kept next to each index.
#[derive(Default)]
struct IndexState {
    /// Ids of the live (not deleted) documents the index has seen, design
    /// documents included, to notice documents that vanish without a change
    /// (purge) or appear with sequences the index already passed (database
    /// recreated).
    live: HashSet<String>,
    /// `doc_count - live.len()` when the index was last built from scratch
    /// (0 for a consistent adapter).
    baseline_gap: i64,
    /// The last change applied (numeric seq, id, rev), re-read on the next
    /// update to notice a database that was recreated behind the index.
    last_change: Option<(u64, String, String)>,
    /// Rows in view order, rebuilt lazily after the index changes.
    sorted: Option<Vec<EmittedRow>>,
}

/// Engine for building and querying persistent views.
///
/// Views are defined as Rust closures (map functions). The engine
/// incrementally updates indexes by reading the changes feed since
/// the last known sequence.
pub struct ViewEngine {
    indexes: HashMap<String, PersistentViewIndex>,
    map_fns: HashMap<String, MapFn>,
    states: HashMap<String, IndexState>,
}

impl ViewEngine {
    pub fn new() -> Self {
        Self {
            indexes: HashMap::new(),
            map_fns: HashMap::new(),
            states: HashMap::new(),
        }
    }

    /// Register a Rust map function for a design doc view.
    ///
    /// Registering a new function for an existing view discards its index,
    /// which is rebuilt with the new function on the next update.
    pub fn register_map<F>(&mut self, ddoc: &str, view_name: &str, f: F)
    where
        F: Fn(&serde_json::Value) -> Vec<(serde_json::Value, serde_json::Value)>
            + Send
            + Sync
            + 'static,
    {
        let key = format!("{}/{}", ddoc, view_name);
        self.indexes.remove(&key);
        self.states.remove(&key);
        self.map_fns.insert(key, Arc::new(f));
    }

    /// Update a view index by fetching changes since the last known seq.
    ///
    /// The index is rebuilt from scratch when the database no longer
    /// matches it: its `update_seq` is behind the index or the change the
    /// index stopped at now belongs to another document (the database was
    /// destroyed and recreated), or its document count disagrees with the
    /// documents the index has seen (documents were purged).
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
            .entry(key.clone())
            .or_insert_with(|| PersistentViewIndex {
                ddoc: ddoc.into(),
                view_name: view_name.into(),
                last_seq: Seq::default(),
                entries: BTreeMap::new(),
            });
        let state = self.states.entry(key).or_default();

        let info = adapter.info().await?;
        let behind = matches!(
            (&index.last_seq, &info.update_seq),
            (Seq::Num(last), Seq::Num(now)) if last > now
        );
        if !behind
            && index.last_seq != Seq::default()
            && apply_changes(adapter, &map_fn, index, state)
                .await?
                .is_some()
        {
            // The document count can only be compared when no write
            // happened between reading it and reading the changes. A purge
            // bumps update_seq without leaving a change, so check that
            // update_seq itself did not move rather than matching it
            // against the last change.
            let settled = adapter.info().await?.update_seq == info.update_seq;
            let gap = info.doc_count as i64 - state.live.len() as i64;
            if !settled || gap == state.baseline_gap {
                return Ok(());
            }
        }

        // Build the index from scratch.
        reset(index, state);
        apply_changes(adapter, &map_fn, index, state).await?;
        let settled = adapter.info().await?.update_seq == info.update_seq;
        state.baseline_gap = if settled {
            info.doc_count as i64 - state.live.len() as i64
        } else {
            0
        };
        Ok(())
    }

    /// Query a view with the same options as `query_view`, using its index.
    ///
    /// With `stale: False` (the default) the index is brought up to date
    /// first; with `Ok` it is used as it is (built if it does not exist
    /// yet); with `UpdateAfter` it is used as it is and updated before
    /// returning.
    pub async fn query(
        &mut self,
        adapter: &dyn Adapter,
        ddoc: &str,
        view_name: &str,
        reduce_fn: Option<&ReduceFn>,
        opts: ViewQueryOptions,
    ) -> Result<ViewResult> {
        let key = format!("{}/{}", ddoc, view_name);
        if opts.stale == StaleOption::False || !self.indexes.contains_key(&key) {
            self.update_index(adapter, ddoc, view_name).await?;
        }

        let index = &self.indexes[&key];
        let state = self.states.entry(key).or_default();
        let rows = state.sorted.get_or_insert_with(|| {
            let mut rows: Vec<EmittedRow> = index
                .entries
                .iter()
                .flat_map(|(id, pairs)| {
                    pairs
                        .iter()
                        .map(|(k, v)| EmittedRow::new(id.clone(), k.clone(), v.clone()))
                })
                .collect();
            sort_emitted(&mut rows);
            rows
        });

        let mut result = query_sorted(rows, reduce_fn, &opts)?;
        if opts.include_docs {
            attach_docs(adapter, &mut result.rows).await?;
        }
        if opts.stale == StaleOption::UpdateAfter {
            self.update_index(adapter, ddoc, view_name).await?;
        }
        Ok(result)
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
        self.states.retain(|k, _| valid.contains(k));
    }
}

/// Empty an index so the next update rebuilds it from the start.
fn reset(index: &mut PersistentViewIndex, state: &mut IndexState) {
    index.entries.clear();
    index.last_seq = Seq::default();
    state.live.clear();
    state.last_change = None;
    state.sorted = None;
}

/// Apply the changes since the index's last sequence and return the new
/// one, or `None` (applying nothing) if the change the index stopped at was
/// replaced by another document or revision.
async fn apply_changes(
    adapter: &dyn Adapter,
    map_fn: &MapFn,
    index: &mut PersistentViewIndex,
    state: &mut IndexState,
) -> Result<Option<Seq>> {
    // With numeric sequences, start one change earlier to re-read the last
    // applied change (applying it again is harmless).
    let since = match (&index.last_seq, &state.last_change) {
        (Seq::Num(n), Some((seq, _, _))) if n == seq && *n > 0 => Seq::Num(n - 1),
        _ => index.last_seq.clone(),
    };
    let changes = adapter
        .changes(ChangesOptions {
            since,
            include_docs: true,
            ..Default::default()
        })
        .await?;

    if let (Some((seq, id, rev)), Some(first)) = (&state.last_change, changes.results.first())
        && first.seq == Seq::Num(*seq)
        && (&first.id != id || first.changes.first().map(|c| &c.rev) != Some(rev))
    {
        return Ok(None);
    }

    if !changes.results.is_empty() {
        state.sorted = None;
    }
    for event in &changes.results {
        // Remove old entries for this doc
        index.entries.remove(&event.id);
        if event.deleted {
            state.live.remove(&event.id);
            continue;
        }
        state.live.insert(event.id.clone());

        // Skip design docs
        if event.id.starts_with("_design/") {
            continue;
        }

        if let Some(ref doc) = event.doc {
            let emitted = map_fn(doc);
            if !emitted.is_empty() {
                index.entries.insert(event.id.clone(), emitted);
            }
        }
    }

    if let Some(last) = changes.results.last() {
        state.last_change = match (&last.seq, last.changes.first()) {
            (Seq::Num(seq), Some(change)) => Some((*seq, last.id.clone(), change.rev.clone())),
            _ => None,
        };
    }
    index.last_seq = changes.last_seq;
    Ok(Some(index.last_seq.clone()))
}

impl Default for ViewEngine {
    fn default() -> Self {
        Self::new()
    }
}
