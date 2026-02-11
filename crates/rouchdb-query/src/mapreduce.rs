//! Map/reduce view engine.
//!
//! Users define views by providing map functions (Rust closures) and optional
//! reduce functions. The engine runs the map over all documents and collects
//! key-value pairs, then optionally reduces them.

use std::cmp::Ordering;

use rouchdb_core::adapter::Adapter;
use rouchdb_core::collation::collate;
use rouchdb_core::document::AllDocsOptions;
use rouchdb_core::error::Result;

/// A key-value pair emitted by a map function.
#[derive(Debug, Clone)]
pub struct EmittedRow {
    pub id: String,
    pub key: serde_json::Value,
    pub value: serde_json::Value,
}

/// Built-in reduce functions matching CouchDB's built-ins.
pub enum ReduceFn {
    /// Sum all numeric values.
    Sum,
    /// Count the number of rows.
    Count,
    /// Compute statistics (sum, count, min, max, sumsqr).
    Stats,
    /// Custom reduce function.
    #[allow(clippy::type_complexity)]
    Custom(Box<dyn Fn(&[serde_json::Value], &[serde_json::Value], bool) -> serde_json::Value>),
}

/// Options for querying a view.
#[derive(Debug, Clone, Default)]
pub struct ViewQueryOptions {
    /// Only return rows with this exact key.
    pub key: Option<serde_json::Value>,
    /// Return rows matching any of these keys, in the given order.
    pub keys: Option<Vec<serde_json::Value>>,
    /// Start of key range (inclusive).
    pub start_key: Option<serde_json::Value>,
    /// End of key range (inclusive by default).
    pub end_key: Option<serde_json::Value>,
    /// Whether to include the end_key in the range.
    pub inclusive_end: bool,
    /// Reverse the order.
    pub descending: bool,
    /// Number of rows to skip.
    pub skip: u64,
    /// Maximum number of rows.
    pub limit: Option<u64>,
    /// Include the full document in each row.
    pub include_docs: bool,
    /// Whether to run the reduce function.
    pub reduce: bool,
    /// Group by key (requires reduce).
    pub group: bool,
    /// Group to this many array elements of the key.
    pub group_level: Option<u64>,
    /// Use stale index without rebuilding.
    pub stale: StaleOption,
}

/// Controls whether the index is rebuilt before querying.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum StaleOption {
    /// Always rebuild the index before querying (default).
    #[default]
    False,
    /// Use the index as-is, do not rebuild.
    Ok,
    /// Use the index as-is, then rebuild in the background.
    UpdateAfter,
}

impl ViewQueryOptions {
    pub fn new() -> Self {
        Self {
            inclusive_end: true,
            ..Default::default()
        }
    }
}

/// Result of querying a view.
#[derive(Debug, Clone)]
pub struct ViewResult {
    pub total_rows: u64,
    pub offset: u64,
    pub rows: Vec<ViewRow>,
}

/// A single row in a view result.
#[derive(Debug, Clone)]
pub struct ViewRow {
    pub id: Option<String>,
    pub key: serde_json::Value,
    pub value: serde_json::Value,
    pub doc: Option<serde_json::Value>,
}

/// Run a temporary (ad-hoc) map/reduce query.
///
/// The `map_fn` receives a document JSON and returns emitted key-value pairs.
pub async fn query_view(
    adapter: &dyn Adapter,
    map_fn: &dyn Fn(&serde_json::Value) -> Vec<(serde_json::Value, serde_json::Value)>,
    reduce_fn: Option<&ReduceFn>,
    opts: ViewQueryOptions,
) -> Result<ViewResult> {
    // Run map over all documents
    let all = adapter
        .all_docs(AllDocsOptions {
            include_docs: true,
            ..AllDocsOptions::new()
        })
        .await?;

    let mut emitted: Vec<EmittedRow> = Vec::new();

    for row in &all.rows {
        if let Some(ref doc_json) = row.doc {
            let pairs = map_fn(doc_json);
            for (key, value) in pairs {
                emitted.push(EmittedRow {
                    id: row.id.clone(),
                    key,
                    value,
                });
            }
        }
    }

    // Sort by key using CouchDB collation
    emitted.sort_by(|a, b| {
        let cmp = collate(&a.key, &b.key);
        if cmp == Ordering::Equal {
            a.id.cmp(&b.id)
        } else {
            cmp
        }
    });

    if opts.descending {
        emitted.reverse();
    }

    // Filter by keys (multi-key lookup) or by key range
    let emitted = if let Some(ref keys) = opts.keys {
        let mut ordered_rows = Vec::new();
        for search_key in keys {
            for row in &emitted {
                if collate(&row.key, search_key) == Ordering::Equal {
                    ordered_rows.push(row.clone());
                }
            }
        }
        ordered_rows
    } else {
        filter_by_range(emitted, &opts)
    };

    let total_rows = emitted.len() as u64;

    // Reduce
    if opts.reduce
        && let Some(reduce) = reduce_fn
    {
        let rows = if opts.group || opts.group_level.is_some() {
            group_reduce(&emitted, reduce, opts.group_level)
        } else {
            let keys: Vec<serde_json::Value> = emitted.iter().map(|r| r.key.clone()).collect();
            let values: Vec<serde_json::Value> = emitted.iter().map(|r| r.value.clone()).collect();
            let result = apply_reduce(reduce, &keys, &values, false);
            vec![ViewRow {
                id: None,
                key: serde_json::Value::Null,
                value: result,
                doc: None,
            }]
        };

        return Ok(ViewResult {
            total_rows: rows.len() as u64,
            offset: 0,
            rows,
        });
    }

    // Apply skip and limit
    let skip = opts.skip as usize;
    let rows: Vec<ViewRow> = emitted
        .into_iter()
        .skip(skip)
        .take(opts.limit.unwrap_or(u64::MAX) as usize)
        .map(|r| ViewRow {
            id: Some(r.id),
            key: r.key,
            value: r.value,
            doc: None,
        })
        .collect();

    Ok(ViewResult {
        total_rows,
        offset: opts.skip,
        rows,
    })
}

fn filter_by_range(rows: Vec<EmittedRow>, opts: &ViewQueryOptions) -> Vec<EmittedRow> {
    rows.into_iter()
        .filter(|r| {
            if let Some(ref key) = opts.key {
                return collate(&r.key, key) == Ordering::Equal;
            }

            if let Some(ref start) = opts.start_key {
                if opts.descending {
                    if collate(&r.key, start) == Ordering::Greater {
                        return false;
                    }
                } else if collate(&r.key, start) == Ordering::Less {
                    return false;
                }
            }

            if let Some(ref end) = opts.end_key {
                if opts.descending {
                    let cmp = collate(&r.key, end);
                    if opts.inclusive_end {
                        if cmp == Ordering::Less {
                            return false;
                        }
                    } else if cmp != Ordering::Greater {
                        return false;
                    }
                } else {
                    let cmp = collate(&r.key, end);
                    if opts.inclusive_end {
                        if cmp == Ordering::Greater {
                            return false;
                        }
                    } else if cmp != Ordering::Less {
                        return false;
                    }
                }
            }

            true
        })
        .collect()
}

fn group_reduce(rows: &[EmittedRow], reduce: &ReduceFn, group_level: Option<u64>) -> Vec<ViewRow> {
    if rows.is_empty() {
        return vec![];
    }

    let mut result = Vec::new();
    let mut current_key = group_key(&rows[0].key, group_level);
    let mut keys = vec![rows[0].key.clone()];
    let mut values = vec![rows[0].value.clone()];

    for row in &rows[1..] {
        let gk = group_key(&row.key, group_level);
        if collate(&gk, &current_key) == Ordering::Equal {
            keys.push(row.key.clone());
            values.push(row.value.clone());
        } else {
            // Emit group
            let reduced = apply_reduce(reduce, &keys, &values, false);
            result.push(ViewRow {
                id: None,
                key: current_key,
                value: reduced,
                doc: None,
            });

            current_key = gk;
            keys = vec![row.key.clone()];
            values = vec![row.value.clone()];
        }
    }

    // Emit last group
    let reduced = apply_reduce(reduce, &keys, &values, false);
    result.push(ViewRow {
        id: None,
        key: current_key,
        value: reduced,
        doc: None,
    });

    result
}

fn group_key(key: &serde_json::Value, group_level: Option<u64>) -> serde_json::Value {
    match group_level {
        None => key.clone(), // Full grouping
        Some(level) => {
            if let Some(arr) = key.as_array() {
                let truncated: Vec<serde_json::Value> =
                    arr.iter().take(level as usize).cloned().collect();
                serde_json::Value::Array(truncated)
            } else {
                key.clone()
            }
        }
    }
}

fn apply_reduce(
    reduce: &ReduceFn,
    keys: &[serde_json::Value],
    values: &[serde_json::Value],
    rereduce: bool,
) -> serde_json::Value {
    match reduce {
        ReduceFn::Sum => {
            let sum: f64 = values.iter().filter_map(|v| v.as_f64()).sum();
            serde_json::json!(sum)
        }
        ReduceFn::Count => {
            serde_json::json!(values.len())
        }
        ReduceFn::Stats => {
            let nums: Vec<f64> = values.iter().filter_map(|v| v.as_f64()).collect();
            let count = nums.len();
            if count == 0 {
                return serde_json::json!({"sum": 0, "count": 0, "min": 0, "max": 0, "sumsqr": 0});
            }
            let sum: f64 = nums.iter().sum();
            let min = nums.iter().copied().fold(f64::INFINITY, f64::min);
            let max = nums.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            let sumsqr: f64 = nums.iter().map(|n| n * n).sum();
            serde_json::json!({
                "sum": sum,
                "count": count,
                "min": min,
                "max": max,
                "sumsqr": sumsqr
            })
        }
        ReduceFn::Custom(f) => f(keys, values, rereduce),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rouchdb_adapter_memory::MemoryAdapter;
    use rouchdb_core::document::{BulkDocsOptions, Document};
    use std::collections::HashMap;

    async fn setup_db() -> MemoryAdapter {
        let db = MemoryAdapter::new("test");
        let docs = vec![
            Document {
                id: "alice".into(),
                rev: None,
                deleted: false,
                data: serde_json::json!({"name": "Alice", "age": 30, "city": "NYC"}),
                attachments: HashMap::new(),
            },
            Document {
                id: "bob".into(),
                rev: None,
                deleted: false,
                data: serde_json::json!({"name": "Bob", "age": 25, "city": "LA"}),
                attachments: HashMap::new(),
            },
            Document {
                id: "charlie".into(),
                rev: None,
                deleted: false,
                data: serde_json::json!({"name": "Charlie", "age": 35, "city": "NYC"}),
                attachments: HashMap::new(),
            },
        ];
        db.bulk_docs(docs, BulkDocsOptions::new()).await.unwrap();
        db
    }

    #[tokio::test]
    async fn map_emits_all() {
        let db = setup_db().await;

        let result = query_view(
            &db,
            &|doc| {
                let name = doc.get("name").cloned().unwrap_or(serde_json::Value::Null);
                vec![(name, serde_json::json!(1))]
            },
            None,
            ViewQueryOptions::new(),
        )
        .await
        .unwrap();

        assert_eq!(result.total_rows, 3);
        // Sorted by key (name): Alice, Bob, Charlie
        assert_eq!(result.rows[0].key, "Alice");
        assert_eq!(result.rows[1].key, "Bob");
        assert_eq!(result.rows[2].key, "Charlie");
    }

    #[tokio::test]
    async fn map_with_key_filter() {
        let db = setup_db().await;

        let result = query_view(
            &db,
            &|doc| {
                let name = doc.get("name").cloned().unwrap_or(serde_json::Value::Null);
                vec![(name, serde_json::json!(1))]
            },
            None,
            ViewQueryOptions {
                key: Some(serde_json::json!("Bob")),
                ..ViewQueryOptions::new()
            },
        )
        .await
        .unwrap();

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].key, "Bob");
    }

    #[tokio::test]
    async fn reduce_sum() {
        let db = setup_db().await;

        let result = query_view(
            &db,
            &|doc| {
                let age = doc.get("age").cloned().unwrap_or(serde_json::json!(0));
                vec![(serde_json::Value::Null, age)]
            },
            Some(&ReduceFn::Sum),
            ViewQueryOptions {
                reduce: true,
                ..ViewQueryOptions::new()
            },
        )
        .await
        .unwrap();

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].value, serde_json::json!(90.0)); // 30 + 25 + 35
    }

    #[tokio::test]
    async fn reduce_count() {
        let db = setup_db().await;

        let result = query_view(
            &db,
            &|doc| {
                let city = doc.get("city").cloned().unwrap_or(serde_json::Value::Null);
                vec![(city, serde_json::json!(1))]
            },
            Some(&ReduceFn::Count),
            ViewQueryOptions {
                reduce: true,
                ..ViewQueryOptions::new()
            },
        )
        .await
        .unwrap();

        assert_eq!(result.rows[0].value, serde_json::json!(3));
    }

    #[tokio::test]
    async fn reduce_group() {
        let db = setup_db().await;

        let result = query_view(
            &db,
            &|doc| {
                let city = doc.get("city").cloned().unwrap_or(serde_json::Value::Null);
                vec![(city, serde_json::json!(1))]
            },
            Some(&ReduceFn::Count),
            ViewQueryOptions {
                reduce: true,
                group: true,
                ..ViewQueryOptions::new()
            },
        )
        .await
        .unwrap();

        assert_eq!(result.rows.len(), 2); // LA, NYC
        // LA: 1, NYC: 2
        assert_eq!(result.rows[0].key, "LA");
        assert_eq!(result.rows[0].value, serde_json::json!(1));
        assert_eq!(result.rows[1].key, "NYC");
        assert_eq!(result.rows[1].value, serde_json::json!(2));
    }

    #[tokio::test]
    async fn reduce_stats() {
        let db = setup_db().await;

        let result = query_view(
            &db,
            &|doc| {
                let age = doc.get("age").cloned().unwrap_or(serde_json::json!(0));
                vec![(serde_json::Value::Null, age)]
            },
            Some(&ReduceFn::Stats),
            ViewQueryOptions {
                reduce: true,
                ..ViewQueryOptions::new()
            },
        )
        .await
        .unwrap();

        let stats = &result.rows[0].value;
        assert_eq!(stats["count"], 3);
        assert_eq!(stats["sum"], 90.0);
        assert_eq!(stats["min"], 25.0);
        assert_eq!(stats["max"], 35.0);
    }

    #[tokio::test]
    async fn descending_and_limit() {
        let db = setup_db().await;

        let result = query_view(
            &db,
            &|doc| {
                let name = doc.get("name").cloned().unwrap_or(serde_json::Value::Null);
                vec![(name, serde_json::json!(1))]
            },
            None,
            ViewQueryOptions {
                descending: true,
                limit: Some(2),
                ..ViewQueryOptions::new()
            },
        )
        .await
        .unwrap();

        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0].key, "Charlie");
        assert_eq!(result.rows[1].key, "Bob");
    }

    #[tokio::test]
    async fn start_end_key_range() {
        let db = setup_db().await;

        let result = query_view(
            &db,
            &|doc| {
                let name = doc.get("name").cloned().unwrap_or(serde_json::Value::Null);
                vec![(name, serde_json::json!(1))]
            },
            None,
            ViewQueryOptions {
                start_key: Some(serde_json::json!("Bob")),
                end_key: Some(serde_json::json!("Charlie")),
                ..ViewQueryOptions::new()
            },
        )
        .await
        .unwrap();

        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0].key, "Bob");
        assert_eq!(result.rows[1].key, "Charlie");
    }
}
