//! Map/reduce view engine.
//!
//! Users define views by providing map functions (Rust closures) and optional
//! reduce functions. The engine runs the map over all documents and collects
//! key-value pairs, then optionally reduces them.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};

use serde_json::Value;

use rouchdb_core::adapter::Adapter;
use rouchdb_core::collation::collate;
use rouchdb_core::document::{AllDocsOptions, GetOptions};
use rouchdb_core::error::{Result, RouchError};

/// A key-value pair emitted by a map function.
///
/// `#[non_exhaustive]`: build it with [`EmittedRow::new`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct EmittedRow {
    pub id: String,
    pub key: serde_json::Value,
    pub value: serde_json::Value,
}

impl EmittedRow {
    /// Document `id` emitted `key` and `value`.
    pub fn new(id: impl Into<String>, key: serde_json::Value, value: serde_json::Value) -> Self {
        Self {
            id: id.into(),
            key,
            value,
        }
    }
}

/// Built-in reduce functions matching CouchDB's built-ins.
///
/// `#[non_exhaustive]`: more built-ins (such as CouchDB's
/// `_approx_count_distinct`) may be added in minor releases.
#[non_exhaustive]
pub enum ReduceFn {
    /// Sum numeric values (arrays element-wise, objects field by field).
    Sum,
    /// Count the number of rows.
    Count,
    /// Compute statistics (sum, count, min, max, sumsqr).
    Stats,
    /// Custom reduce function, called as `f(keys, values, rereduce)`.
    ///
    /// As in CouchDB, each key is a `[key, doc_id]` pair. All the values of
    /// a group are reduced in one call, so `rereduce` is always `false`.
    #[allow(clippy::type_complexity)]
    Custom(Box<dyn Fn(&[serde_json::Value], &[serde_json::Value], bool) -> serde_json::Value>),
}

/// Options for querying a view.
///
/// `ViewQueryOptions::default()` is the same as [`ViewQueryOptions::new`]:
/// CouchDB's defaults, with `reduce` and `inclusive_end` on. Set the options
/// you need and fill the rest with `..Default::default()`: fields may be
/// added in minor releases, and a literal that lists every field would then
/// stop compiling.
///
/// ```
/// use rouchdb_query::ViewQueryOptions;
/// use serde_json::json;
///
/// let opts = ViewQueryOptions {
///     key: Some(json!("alice")),
///     include_docs: true,
///     reduce: false,
///     ..Default::default()
/// };
/// assert!(opts.inclusive_end);
/// ```
#[derive(Debug, Clone)]
pub struct ViewQueryOptions {
    /// Only return rows with this exact key. With `start_key` or `end_key`
    /// it is the other bound of the range (as when they follow `key` in a
    /// CouchDB query string).
    pub key: Option<serde_json::Value>,
    /// Return rows matching any of these keys, in the given order. Several
    /// keys cannot be combined with `key`, `start_key` or `end_key`.
    pub keys: Option<Vec<serde_json::Value>>,
    /// Start of key range (inclusive). A range no row can be in (a start
    /// after the end, or before it when descending) is a `BadRequest`, as
    /// in CouchDB.
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
    /// Include the full document in each row. A value of the form
    /// `{"_id": ...}` (optionally with `_rev`) includes that document
    /// instead. Not allowed together with reduce.
    pub include_docs: bool,
    /// Whether to run the reduce function, if one is given. On by default,
    /// like in CouchDB.
    pub reduce: bool,
    /// Group by key. Grouping (this, or a `group_level` above 0) without a
    /// reduce is a `BadRequest`, as in CouchDB.
    pub group: bool,
    /// Group to this many array elements of the key.
    pub group_level: Option<u64>,
    /// Use stale index without rebuilding. Only persistent views
    /// (`ViewEngine::query`) have an index; ad-hoc `query_view` always
    /// reads the current documents.
    pub stale: StaleOption,
}

/// Controls whether the index is rebuilt before querying.
///
/// `#[non_exhaustive]`: CouchDB's newer `update` parameter (`true`, `false`,
/// `lazy`) may be mapped to new variants in a minor release.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
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
    /// CouchDB's defaults: `inclusive_end` and `reduce` are on, everything
    /// else is off or unset. Same as `ViewQueryOptions::default()`.
    pub fn new() -> Self {
        Self {
            key: None,
            keys: None,
            start_key: None,
            end_key: None,
            inclusive_end: true,
            descending: false,
            skip: 0,
            limit: None,
            include_docs: false,
            reduce: true,
            group: false,
            group_level: None,
            stale: StaleOption::False,
        }
    }
}

impl Default for ViewQueryOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of querying a view.
///
/// `#[non_exhaustive]`, like [`ViewRow`]: fields may be added in minor
/// releases (CouchDB can also return `update_seq`).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ViewResult {
    /// Rows in the whole view (for a reduce query, the number of reduced rows
    /// before skip/limit).
    pub total_rows: u64,
    /// Position of the first returned row in the view (for a reduce query,
    /// the skip).
    pub offset: u64,
    pub rows: Vec<ViewRow>,
}

/// A single row in a view result.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ViewRow {
    pub id: Option<String>,
    pub key: serde_json::Value,
    pub value: serde_json::Value,
    pub doc: Option<serde_json::Value>,
}

/// Run a temporary (ad-hoc) map/reduce query.
///
/// The `map_fn` receives a document JSON and returns emitted key-value pairs.
/// Design documents are not passed to the map function, as in CouchDB.
pub async fn query_view(
    adapter: &dyn Adapter,
    map_fn: &dyn Fn(&serde_json::Value) -> Vec<(serde_json::Value, serde_json::Value)>,
    reduce_fn: Option<&ReduceFn>,
    opts: ViewQueryOptions,
) -> Result<ViewResult> {
    // Reject invalid option combinations before scanning.
    reducer(reduce_fn, &opts)?;

    // Run map over all documents
    let all = adapter
        .all_docs(AllDocsOptions {
            include_docs: true,
            ..AllDocsOptions::new()
        })
        .await?;

    let mut emitted: Vec<EmittedRow> = Vec::new();
    let mut docs: HashMap<String, Value> = HashMap::new();

    // A range query only returns document rows, whose key is the id.
    for row in all.rows {
        if row.key.starts_with("_design/") {
            continue;
        }
        if let Some(doc_json) = row.doc {
            for (key, value) in map_fn(&doc_json) {
                emitted.push(EmittedRow {
                    id: row.key.clone(),
                    key,
                    value,
                });
            }
            if opts.include_docs {
                docs.insert(row.key, doc_json);
            }
        }
    }

    let mut result = query_emitted(emitted, reduce_fn, &opts)?;
    if opts.include_docs {
        attach_docs_from(adapter, &mut result.rows, &docs).await?;
    }
    Ok(result)
}

/// Run a view query over the rows emitted by a map function: sort them,
/// select the requested keys or key range, reduce or group, and apply skip
/// and limit.
///
/// The returned rows have no `doc`; use [`attach_docs`] for `include_docs`.
pub fn query_emitted(
    mut rows: Vec<EmittedRow>,
    reduce_fn: Option<&ReduceFn>,
    opts: &ViewQueryOptions,
) -> Result<ViewResult> {
    sort_emitted(&mut rows);
    query_sorted(&rows, reduce_fn, opts)
}

/// Sort emitted rows in view order: by key (CouchDB collation), then doc id.
pub fn sort_emitted(rows: &mut [EmittedRow]) {
    rows.sort_by(|a, b| collate(&a.key, &b.key).then_with(|| a.id.cmp(&b.id)));
}

/// Like [`query_emitted`], for rows already in view order (see
/// [`sort_emitted`]); only the selected rows are copied.
pub fn query_sorted(
    rows: &[EmittedRow],
    reduce_fn: Option<&ReduceFn>,
    opts: &ViewQueryOptions,
) -> Result<ViewResult> {
    // Like CouchDB, a single-element `keys` is the same query as `key`
    // (`keys` wins over `key`, as it does with several keys).
    let single_key;
    let opts = match opts.keys.as_deref() {
        Some([key]) => {
            single_key = ViewQueryOptions {
                key: Some(key.clone()),
                keys: None,
                ..opts.clone()
            };
            &single_key
        }
        _ => opts,
    };
    let reduce = reducer(reduce_fn, opts)?;
    let total = rows.len();

    // Grouping level: group_level overrides group, and 0 means no grouping.
    let grouped = opts.group_level.map(|l| l > 0).unwrap_or(opts.group);

    if let Some(reduce) = reduce {
        let groups = if let Some(ref keys) = opts.keys {
            // One reduced row per requested key, in the order of the keys
            // even when descending (as in CouchDB).
            let mut groups = Vec::new();
            for key in keys {
                let (lo, hi) = equal_range(rows, key);
                if lo < hi {
                    groups.extend(group_reduce(&rows[lo..hi], reduce, opts.group_level)?);
                }
            }
            groups
        } else {
            let (lo, hi) = range_bounds(rows, opts);
            let selected = &rows[lo..hi];
            if grouped {
                let mut groups = group_reduce(selected, reduce, opts.group_level)?;
                if opts.descending {
                    groups.reverse();
                }
                groups
            } else if selected.is_empty() {
                // An empty reduce yields no rows (CouchDB returns {"rows":[]}),
                // not a spurious zero row.
                Vec::new()
            } else {
                vec![ViewRow {
                    id: None,
                    key: Value::Null,
                    value: apply_reduce(reduce, selected)?,
                    doc: None,
                }]
            }
        };

        // Apply skip/limit to the reduced/grouped rows as well.
        let reduced_total = groups.len() as u64;
        let rows: Vec<ViewRow> = groups
            .into_iter()
            .skip(opts.skip as usize)
            .take(opts.limit.unwrap_or(u64::MAX) as usize)
            .collect();

        return Ok(ViewResult {
            total_rows: reduced_total,
            offset: opts.skip,
            rows,
        });
    }

    // Select the page of rows (in query order) and the position of the first
    // selected row in the view, which CouchDB reports as the offset. Only the
    // returned rows are copied.
    let skip = opts.skip as usize;
    let limit = opts.limit.map_or(usize::MAX, |l| l as usize);
    let (page, first_position): (Vec<&EmittedRow>, usize) = if let Some(ref keys) = opts.keys {
        let mut positions: Vec<usize> = Vec::new();
        for key in keys {
            let (lo, hi) = equal_range(rows, key);
            positions.extend(lo..hi);
        }
        if opts.descending {
            positions.reverse();
        }
        let first = positions
            .first()
            .map_or(total, |&i| if opts.descending { total - 1 - i } else { i });
        let page = positions
            .into_iter()
            .skip(skip)
            .take(limit)
            .map(|i| &rows[i])
            .collect();
        (page, first)
    } else {
        let (lo, hi) = range_bounds(rows, opts);
        let selected = &rows[lo..hi];
        if opts.descending {
            let page = selected.iter().rev().skip(skip).take(limit).collect();
            (page, total - hi)
        } else {
            (selected.iter().skip(skip).take(limit).collect(), lo)
        }
    };

    let rows: Vec<ViewRow> = page
        .into_iter()
        .map(|r| ViewRow {
            id: Some(r.id.clone()),
            key: r.key.clone(),
            value: r.value.clone(),
            doc: None,
        })
        .collect();

    Ok(ViewResult {
        total_rows: total as u64,
        offset: first_position.saturating_add(skip).min(total) as u64,
        rows,
    })
}

/// The reduce function to run for these options, if any, after checking
/// the option combinations CouchDB rejects (with a `query_parse_error`).
fn reducer<'a>(
    reduce_fn: Option<&'a ReduceFn>,
    opts: &ViewQueryOptions,
) -> Result<Option<&'a ReduceFn>> {
    let parse_error = |reason: &str| Err(RouchError::BadRequest(reason.into()));
    if opts.keys.as_ref().is_some_and(|keys| keys.len() != 1)
        && (opts.key.is_some() || opts.start_key.is_some() || opts.end_key.is_some())
    {
        return parse_error("`keys` is incompatible with `key`, `start_key` and `end_key`");
    }
    // A single key is `key`, which is both bounds unless replaced.
    let key = match opts.keys.as_deref() {
        Some([key]) => Some(key),
        _ => opts.key.as_ref(),
    };
    let start = opts.start_key.as_ref().or(key);
    let end = opts.end_key.as_ref().or(key);
    if let (Some(start), Some(end)) = (start, end) {
        match (opts.descending, collate(start, end)) {
            (false, Ordering::Greater) => {
                return parse_error(
                    "No rows can match your key range, reverse your start_key and end_key \
                     or set descending=true",
                );
            }
            (true, Ordering::Less) => {
                return parse_error(
                    "No rows can match your key range, reverse your start_key and end_key \
                     or set descending=false",
                );
            }
            _ => {}
        }
    }
    let reduce = reduce_fn.filter(|_| opts.reduce);
    let grouped = opts.group_level.map(|l| l > 0).unwrap_or(opts.group);
    if reduce.is_none() && grouped {
        return parse_error("Invalid use of grouping on a map view.");
    }
    if reduce.is_some() {
        if opts.include_docs {
            return Err(RouchError::BadRequest(
                "`include_docs` is invalid for reduce".into(),
            ));
        }
        // CouchDB needs exact grouping (`group=true`, no `group_level`)
        // to reduce several keys, one row per key; a single key is `key`.
        let exact_group = opts.group && opts.group_level.is_none();
        let single_key = matches!(opts.keys.as_deref(), Some([_]));
        if opts.keys.is_some() && !single_key && !exact_group {
            return Err(RouchError::BadRequest(
                "Multi-key fetches for reduce views must use `group=true`".into(),
            ));
        }
    }
    Ok(reduce)
}

/// Index range `[lo, hi)` of the rows (sorted ascending) whose key equals `key`.
///
/// Unlike CouchDB, which reads a key as the range from the key to itself
/// and so returns nothing for it with `inclusive_end=false`, a key always
/// selects its rows, even with `inclusive_end: false`.
fn equal_range(rows: &[EmittedRow], key: &Value) -> (usize, usize) {
    (lower_bound(rows, key), upper_bound(rows, key))
}

/// First row whose key is not less than `key`.
fn lower_bound(rows: &[EmittedRow], key: &Value) -> usize {
    rows.partition_point(|r| collate(&r.key, key) == Ordering::Less)
}

/// First row whose key is greater than `key`.
fn upper_bound(rows: &[EmittedRow], key: &Value) -> usize {
    rows.partition_point(|r| collate(&r.key, key) != Ordering::Greater)
}

/// Index range `[lo, hi)`, in ascending order, selected by `key` or by
/// `start_key`/`end_key` (which swap roles when descending). With either of
/// them, `key` is the other bound, as when they follow `key` in a CouchDB
/// query string.
fn range_bounds(rows: &[EmittedRow], opts: &ViewQueryOptions) -> (usize, usize) {
    if let Some(ref key) = opts.key
        && opts.start_key.is_none()
        && opts.end_key.is_none()
    {
        return equal_range(rows, key);
    }
    let start = opts.start_key.as_ref().or(opts.key.as_ref());
    let end = opts.end_key.as_ref().or(opts.key.as_ref());
    let (mut lo, mut hi) = (0, rows.len());
    if opts.descending {
        if let Some(start) = start {
            hi = upper_bound(rows, start);
        }
        if let Some(end) = end {
            lo = if opts.inclusive_end {
                lower_bound(rows, end)
            } else {
                upper_bound(rows, end)
            };
        }
    } else {
        if let Some(start) = start {
            lo = lower_bound(rows, start);
        }
        if let Some(end) = end {
            hi = if opts.inclusive_end {
                upper_bound(rows, end)
            } else {
                lower_bound(rows, end)
            };
        }
    }
    (lo, hi.max(lo))
}

/// Fill in `doc` for the rows of a map query (`include_docs`), reading the
/// documents from the adapter.
///
/// A row whose value is an object with an `_id` (and optionally a `_rev`)
/// gets that linked document instead, as in CouchDB. Rows whose document
/// does not exist keep `doc: None`.
pub async fn attach_docs(adapter: &dyn Adapter, rows: &mut [ViewRow]) -> Result<()> {
    attach_docs_from(adapter, rows, &HashMap::new()).await
}

/// Like [`attach_docs`], taking current documents from `loaded` when present.
async fn attach_docs_from(
    adapter: &dyn Adapter,
    rows: &mut [ViewRow],
    loaded: &HashMap<String, Value>,
) -> Result<()> {
    // The (id, rev) each row includes.
    let targets: Vec<Option<(String, Option<String>)>> = rows
        .iter()
        .map(|row| {
            let linked = row.value.get("_id").and_then(Value::as_str);
            match linked {
                Some(id) => Some((
                    id.to_string(),
                    row.value
                        .get("_rev")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                )),
                None => row.id.clone().map(|id| (id, None)),
            }
        })
        .collect();

    // Current revisions not already loaded are fetched in one call.
    let mut missing: Vec<String> = targets
        .iter()
        .flatten()
        .filter(|(id, rev)| rev.is_none() && !loaded.contains_key(id))
        .map(|(id, _)| id.clone())
        .collect();
    missing.sort();
    missing.dedup();
    let mut fetched: HashMap<String, Value> = HashMap::new();
    if !missing.is_empty() {
        let response = adapter
            .all_docs(AllDocsOptions {
                keys: Some(missing),
                include_docs: true,
                ..AllDocsOptions::new()
            })
            .await?;
        // Only live documents carry a doc; their key is the id.
        for row in response.rows {
            if let Some(doc) = row.doc {
                fetched.insert(row.key, doc);
            }
        }
    }

    for (row, target) in rows.iter_mut().zip(targets) {
        row.doc = match target {
            None => None,
            Some((id, Some(rev))) => {
                let opts = GetOptions {
                    rev: Some(rev),
                    ..Default::default()
                };
                match adapter.get(&id, opts).await {
                    Ok(doc) => Some(doc.to_json()),
                    Err(RouchError::NotFound(_)) => None,
                    Err(e) => return Err(e),
                }
            }
            Some((id, None)) => loaded.get(&id).or_else(|| fetched.get(&id)).cloned(),
        };
    }
    Ok(())
}

/// Reduce consecutive rows with the same (group-level truncated) key.
fn group_reduce(
    rows: &[EmittedRow],
    reduce: &ReduceFn,
    group_level: Option<u64>,
) -> Result<Vec<ViewRow>> {
    let mut result = Vec::new();
    let mut start = 0;
    while start < rows.len() {
        let key = group_key(&rows[start].key, group_level);
        let mut end = start + 1;
        while end < rows.len()
            && collate(&group_key(&rows[end].key, group_level), &key) == Ordering::Equal
        {
            end += 1;
        }
        result.push(ViewRow {
            id: None,
            key,
            value: apply_reduce(reduce, &rows[start..end])?,
            doc: None,
        });
        start = end;
    }
    Ok(result)
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

fn apply_reduce(reduce: &ReduceFn, rows: &[EmittedRow]) -> Result<Value> {
    let values = || rows.iter().map(|r| &r.value);
    match reduce {
        ReduceFn::Sum => Ok(builtin_sum(&values().collect::<Vec<_>>())),
        ReduceFn::Count => Ok(serde_json::json!(rows.len())),
        ReduceFn::Stats => builtin_stats(values()),
        ReduceFn::Custom(f) => {
            let keys: Vec<Value> = rows
                .iter()
                .map(|r| serde_json::json!([r.key, r.id]))
                .collect();
            let values: Vec<Value> = values().cloned().collect();
            Ok(f(&keys, &values, false))
        }
    }
}

// ---------------------------------------------------------------------------
// Built-in reduce functions
// ---------------------------------------------------------------------------

/// A number that stays an integer while every input is one (like Erlang).
#[derive(Debug, Clone, Copy)]
enum Num {
    Int(i128),
    Float(f64),
}

impl Num {
    fn from_json(value: &Value) -> Option<Num> {
        let n = value.as_number()?;
        Some(if let Some(i) = n.as_i64() {
            Num::Int(i.into())
        } else if let Some(u) = n.as_u64() {
            Num::Int(u.into())
        } else {
            Num::Float(n.as_f64()?)
        })
    }

    fn as_f64(self) -> f64 {
        match self {
            Num::Int(i) => i as f64,
            Num::Float(f) => f,
        }
    }

    fn add(self, other: Num) -> Num {
        match (self, other) {
            (Num::Int(a), Num::Int(b)) => a
                .checked_add(b)
                .map_or(Num::Float(a as f64 + b as f64), Num::Int),
            _ => Num::Float(self.as_f64() + other.as_f64()),
        }
    }

    fn square(self) -> Num {
        match self {
            Num::Int(a) => a
                .checked_mul(a)
                .map_or(Num::Float((a as f64).powi(2)), Num::Int),
            Num::Float(f) => Num::Float(f * f),
        }
    }

    fn min(self, other: Num) -> Num {
        if self.compare(other) == Ordering::Greater {
            other
        } else {
            self
        }
    }

    fn max(self, other: Num) -> Num {
        if self.compare(other) == Ordering::Less {
            other
        } else {
            self
        }
    }

    fn compare(self, other: Num) -> Ordering {
        collate(&self.to_json(), &other.to_json())
    }

    fn to_json(self) -> Value {
        match self {
            Num::Int(i) => {
                if let Ok(i) = i64::try_from(i) {
                    Value::from(i)
                } else if let Ok(u) = u64::try_from(i) {
                    Value::from(u)
                } else {
                    Value::from(i as f64)
                }
            }
            Num::Float(f) => Value::from(f),
        }
    }
}

/// CouchDB's reason for a `_sum` of values it cannot add.
const SUM_ERROR: &str = "The _sum function requires that map values be numbers, arrays of \
     numbers, or objects. Objects cannot be mixed with other data structures. Objects can be \
     arbitrarily nested, provided that the values for all fields are themselves numbers, \
     arrays of numbers, or objects.";

/// Partial `_sum`: a number, an array of numbers, or an object of sums.
enum Sum {
    Num(Num),
    Array(Vec<Num>),
    Object(BTreeMap<String, Sum>),
}

impl Sum {
    /// `None` for a value `_sum` cannot add.
    fn from_json(value: &Value) -> Option<Sum> {
        match value {
            Value::Number(_) => Num::from_json(value).map(Sum::Num),
            Value::Array(items) => numbers(items).map(Sum::Array),
            Value::Object(map) => map
                .iter()
                .map(|(k, v)| Some((k.clone(), Sum::from_json(v)?)))
                .collect::<Option<_>>()
                .map(Sum::Object),
            _ => None,
        }
    }

    /// `None` when the two cannot be added (an object and a number or an
    /// array).
    fn add(self, other: Sum) -> Option<Sum> {
        match (self, other) {
            (Sum::Num(a), Sum::Num(b)) => Some(Sum::Num(a.add(b))),
            // A number is added to the first element of an array.
            (Sum::Num(a), Sum::Array(b)) | (Sum::Array(b), Sum::Num(a)) => {
                Some(Sum::Array(add_arrays(vec![a], b)))
            }
            (Sum::Array(a), Sum::Array(b)) => Some(Sum::Array(add_arrays(a, b))),
            (Sum::Object(mut a), Sum::Object(b)) => {
                for (k, v) in b {
                    let sum = match a.remove(&k) {
                        Some(acc) => acc.add(v)?,
                        None => v,
                    };
                    a.insert(k, sum);
                }
                Some(Sum::Object(a))
            }
            _ => None,
        }
    }

    fn to_json(&self) -> Value {
        match self {
            Sum::Num(n) => n.to_json(),
            Sum::Array(items) => Value::Array(items.iter().map(|n| n.to_json()).collect()),
            Sum::Object(map) => {
                Value::Object(map.iter().map(|(k, v)| (k.clone(), v.to_json())).collect())
            }
        }
    }
}

fn numbers(items: &[Value]) -> Option<Vec<Num>> {
    items.iter().map(Num::from_json).collect()
}

/// Element-wise sum; the shorter array is padded with zeros.
fn add_arrays(mut a: Vec<Num>, b: Vec<Num>) -> Vec<Num> {
    for (i, n) in b.into_iter().enumerate() {
        match a.get_mut(i) {
            Some(acc) => *acc = acc.add(n),
            None => a.push(n),
        }
    }
    a
}

/// Add `values` in order (`None` if there are none); `Err` holds the first
/// value that could not be added.
fn sum_in_order<'a>(
    values: impl Iterator<Item = &'a Value>,
) -> std::result::Result<Option<Sum>, &'a Value> {
    let mut sum: Option<Sum> = None;
    for value in values {
        let next = Sum::from_json(value).ok_or(value)?;
        sum = Some(match sum {
            None => next,
            Some(acc) => acc.add(next).ok_or(value)?,
        });
    }
    Ok(sum)
}

/// CouchDB's `_sum`: integers stay integers, arrays of numbers are summed
/// element-wise, objects field by field.
///
/// Values it cannot add make the reduced value CouchDB's error object
/// (`{"error": "builtin_reduce_error", "reason": ..., "caused_by": value}`),
/// which CouchDB returns as the value of the row (with a 200), not as a
/// query error. CouchDB adds the rows of an index node last to first, so
/// `caused_by` is the first value that fails going backwards.
fn builtin_sum(values: &[&Value]) -> Value {
    match sum_in_order(values.iter().copied()) {
        Ok(sum) => sum.map_or(serde_json::json!(0), |sum| sum.to_json()),
        Err(_) => {
            let culprit = sum_in_order(values.iter().rev().copied())
                .err()
                .unwrap_or(values[values.len() - 1]);
            serde_json::json!({
                "error": "builtin_reduce_error",
                "reason": SUM_ERROR,
                "caused_by": culprit,
            })
        }
    }
}

#[derive(Clone, Copy)]
struct Stats {
    sum: Num,
    count: u64,
    min: Num,
    max: Num,
    sumsqr: Num,
}

impl Stats {
    fn new(n: Num) -> Stats {
        Stats {
            sum: n,
            count: 1,
            min: n,
            max: n,
            sumsqr: n.square(),
        }
    }

    fn push(&mut self, n: Num) {
        self.sum = self.sum.add(n);
        self.count += 1;
        self.min = self.min.min(n);
        self.max = self.max.max(n);
        self.sumsqr = self.sumsqr.add(n.square());
    }

    fn to_json(self) -> Value {
        serde_json::json!({
            "sum": self.sum.to_json(),
            "count": self.count,
            "min": self.min.to_json(),
            "max": self.max.to_json(),
            "sumsqr": self.sumsqr.to_json(),
        })
    }
}

/// CouchDB's `_stats` over numbers, or element-wise over arrays of numbers
/// of the same length; anything else is an error.
fn builtin_stats<'a>(values: impl Iterator<Item = &'a Value>) -> Result<Value> {
    let error = |v: &Value| {
        RouchError::BadRequest(format!(
            "builtin_reduce_error: the _stats function requires that map values be numbers \
             or arrays of numbers, not {v}"
        ))
    };
    let mut scalar: Option<Stats> = None;
    let mut columns: Option<Vec<Stats>> = None;
    for value in values {
        match value {
            Value::Number(_) if columns.is_none() => {
                let n = Num::from_json(value).ok_or_else(|| error(value))?;
                match scalar {
                    Some(ref mut s) => s.push(n),
                    None => scalar = Some(Stats::new(n)),
                }
            }
            Value::Array(items) if scalar.is_none() => {
                let nums = items
                    .iter()
                    .map(|v| Num::from_json(v).ok_or_else(|| error(v)))
                    .collect::<Result<Vec<_>>>()?;
                match columns {
                    Some(ref mut cols) if cols.len() == nums.len() => {
                        for (s, n) in cols.iter_mut().zip(nums) {
                            s.push(n);
                        }
                    }
                    Some(_) => return Err(error(value)),
                    None => columns = Some(nums.into_iter().map(Stats::new).collect()),
                }
            }
            _ => return Err(error(value)),
        }
    }
    Ok(match (scalar, columns) {
        (Some(s), _) => s.to_json(),
        (None, Some(cols)) => Value::Array(cols.into_iter().map(Stats::to_json).collect()),
        (None, None) => Stats::new(Num::Int(0)).to_json(),
    })
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
    async fn reduce_group_level_zero_is_global() {
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
                group_level: Some(0),
                ..ViewQueryOptions::new()
            },
        )
        .await
        .unwrap();
        // group_level=0 collapses everything into one global group.
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].value, serde_json::json!(3));
    }

    #[tokio::test]
    async fn reduce_grouped_honors_skip_and_limit() {
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
                skip: 1,
                limit: Some(1),
                ..ViewQueryOptions::new()
            },
        )
        .await
        .unwrap();
        // Groups sorted by key: "LA"(1), "NYC"(2). skip 1 -> NYC; limit 1.
        assert_eq!(result.total_rows, 2);
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].key, "NYC");
        assert_eq!(result.rows[0].value, serde_json::json!(2));
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
        assert_eq!(result.rows[0].value, serde_json::json!(90)); // 30 + 25 + 35
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

    // --- Regression tests for audited findings ---

    fn by_city(doc: &serde_json::Value) -> Vec<(serde_json::Value, serde_json::Value)> {
        match doc.get("city") {
            Some(c) => vec![(c.clone(), doc["age"].clone())],
            None => vec![],
        }
    }

    #[tokio::test]
    async fn include_docs_fills_row_docs() {
        // F16: include_docs returns each row's document.
        let db = setup_db().await;
        let result = query_view(
            &db,
            &by_city,
            None,
            ViewQueryOptions {
                include_docs: true,
                ..ViewQueryOptions::new()
            },
        )
        .await
        .unwrap();
        assert_eq!(result.rows.len(), 3);
        for row in &result.rows {
            let doc = row.doc.as_ref().expect("doc included");
            assert_eq!(doc["_id"], serde_json::json!(row.id.clone().unwrap()));
            assert!(doc["_rev"].is_string());
        }
    }

    #[tokio::test]
    async fn include_docs_follows_linked_ids() {
        // F16: a value {"_id": X} includes document X instead.
        let db = setup_db().await;
        let result = query_view(
            &db,
            &|doc| match doc["_id"].as_str() {
                Some("alice") => vec![(serde_json::json!(1), serde_json::json!({"_id": "bob"}))],
                Some("bob") => vec![(serde_json::json!(2), serde_json::json!({"_id": "nobody"}))],
                _ => vec![],
            },
            None,
            ViewQueryOptions {
                include_docs: true,
                ..ViewQueryOptions::new()
            },
        )
        .await
        .unwrap();
        assert_eq!(result.rows[0].doc.as_ref().unwrap()["name"], "Bob");
        assert!(result.rows[1].doc.is_none());
    }

    #[tokio::test]
    async fn include_docs_is_invalid_for_reduce() {
        let db = setup_db().await;
        let err = query_view(
            &db,
            &by_city,
            Some(&ReduceFn::Count),
            ViewQueryOptions {
                include_docs: true,
                ..ViewQueryOptions::new()
            },
        )
        .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn design_docs_are_not_mapped() {
        // F50: like CouchDB (and ViewEngine), map functions skip _design/ docs.
        let db = setup_db().await;
        db.bulk_docs(
            vec![Document {
                id: "_design/app".into(),
                rev: None,
                deleted: false,
                data: serde_json::json!({"views": {}}),
                attachments: HashMap::new(),
            }],
            BulkDocsOptions::new(),
        )
        .await
        .unwrap();
        let result = query_view(
            &db,
            &|doc| vec![(doc["_id"].clone(), serde_json::json!(1))],
            Some(&ReduceFn::Count),
            ViewQueryOptions::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.rows[0].value, serde_json::json!(3));
    }

    #[tokio::test]
    async fn total_rows_and_offset_describe_the_whole_view() {
        // F53: total_rows counts every row of the view and offset is the
        // position of the first returned row.
        let db = setup_db().await;
        let q = |opts: ViewQueryOptions| {
            let db = &db;
            async move { query_view(db, &by_city, None, opts).await.unwrap() }
        };
        // Sorted rows: LA/bob, NYC/alice, NYC/charlie
        let r = q(ViewQueryOptions {
            key: Some(serde_json::json!("NYC")),
            ..ViewQueryOptions::new()
        })
        .await;
        assert_eq!((r.total_rows, r.offset, r.rows.len()), (3, 1, 2));
        let r = q(ViewQueryOptions {
            start_key: Some(serde_json::json!("NYC")),
            descending: true,
            ..ViewQueryOptions::new()
        })
        .await;
        assert_eq!((r.total_rows, r.offset, r.rows.len()), (3, 0, 3));
        let r = q(ViewQueryOptions {
            start_key: Some(serde_json::json!("LA")),
            end_key: Some(serde_json::json!("LA")),
            descending: true,
            ..ViewQueryOptions::new()
        })
        .await;
        assert_eq!((r.total_rows, r.offset, r.rows.len()), (3, 2, 1));
        let r = q(ViewQueryOptions {
            key: Some(serde_json::json!("NYC")),
            skip: 5,
            ..ViewQueryOptions::new()
        })
        .await;
        assert_eq!((r.total_rows, r.offset, r.rows.len()), (3, 3, 0));
    }

    #[tokio::test]
    async fn builtin_sum_and_stats_follow_couchdb() {
        // F54: integers stay integers, arrays and objects are summed
        // element-wise; non-numeric values make the value of the reduced
        // row an error (a query error for _stats).
        let db = setup_db().await;
        let run = |map: fn(&serde_json::Value) -> Vec<(serde_json::Value, serde_json::Value)>,
                   reduce: ReduceFn| {
            let db = &db;
            async move { query_view(db, &map, Some(&reduce), ViewQueryOptions::new()).await }
        };
        let r = run(
            |d| vec![(serde_json::Value::Null, d["age"].clone())],
            ReduceFn::Sum,
        )
        .await
        .unwrap();
        assert_eq!(r.rows[0].value, serde_json::json!(90));
        let r = run(
            |d| vec![(serde_json::Value::Null, serde_json::json!([d["age"], 1]))],
            ReduceFn::Sum,
        )
        .await
        .unwrap();
        assert_eq!(r.rows[0].value, serde_json::json!([90, 3]));
        let r = run(
            |d| {
                vec![(
                    serde_json::Value::Null,
                    serde_json::json!({"a": d["age"], "n": {"x": 0.5}}),
                )]
            },
            ReduceFn::Sum,
        )
        .await
        .unwrap();
        assert_eq!(
            r.rows[0].value,
            serde_json::json!({"a": 90, "n": {"x": 1.5}})
        );
        let r = run(
            |d| vec![(serde_json::Value::Null, d["name"].clone())],
            ReduceFn::Sum,
        )
        .await
        .unwrap();
        assert_eq!(r.rows[0].value["error"], "builtin_reduce_error");

        let r = run(
            |d| vec![(serde_json::Value::Null, d["age"].clone())],
            ReduceFn::Stats,
        )
        .await
        .unwrap();
        assert_eq!(
            r.rows[0].value,
            serde_json::json!({"sum": 90, "count": 3, "min": 25, "max": 35, "sumsqr": 2750})
        );
        let r = run(
            |d| vec![(serde_json::Value::Null, serde_json::json!([d["age"], 2]))],
            ReduceFn::Stats,
        )
        .await
        .unwrap();
        assert_eq!(r.rows[0].value[1]["sumsqr"], serde_json::json!(12));
        assert!(
            run(
                |d| vec![(serde_json::Value::Null, d["name"].clone())],
                ReduceFn::Stats
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn new_options_reduce_by_default() {
        // F104: like CouchDB, a view with a reduce function reduces unless
        // reduce=false is asked for.
        let db = setup_db().await;
        let r = query_view(
            &db,
            &by_city,
            Some(&ReduceFn::Count),
            ViewQueryOptions::new(),
        )
        .await
        .unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0].value, serde_json::json!(3));
        let r = query_view(
            &db,
            &by_city,
            Some(&ReduceFn::Count),
            ViewQueryOptions {
                reduce: false,
                ..ViewQueryOptions::new()
            },
        )
        .await
        .unwrap();
        assert_eq!(r.rows.len(), 3);
    }

    #[tokio::test]
    async fn custom_reduce_receives_key_and_id_pairs() {
        // F105: like CouchDB, keys are [key, doc_id] pairs.
        let db = setup_db().await;
        let reduce = ReduceFn::Custom(Box::new(|keys, _values, _rereduce| {
            serde_json::Value::Array(keys.to_vec())
        }));
        let r = query_view(
            &db,
            &by_city,
            Some(&reduce),
            ViewQueryOptions {
                group: true,
                ..ViewQueryOptions::new()
            },
        )
        .await
        .unwrap();
        assert_eq!(r.rows[0].value, serde_json::json!([["LA", "bob"]]));
    }

    #[test]
    fn default_view_options_are_couchdb_defaults() {
        // `..Default::default()` must not silently turn reduce or the
        // inclusive end off (it did before 0.5).
        let d = ViewQueryOptions::default();
        assert!(d.reduce && d.inclusive_end);
        assert!(!d.descending && !d.include_docs && !d.group);
        assert_eq!((d.skip, d.limit, d.group_level), (0, None, None));
        assert!(d.key.is_none() && d.keys.is_none());
        assert!(d.start_key.is_none() && d.end_key.is_none());
        assert_eq!(d.stale, StaleOption::False);
    }

    #[tokio::test]
    async fn default_view_options_reduce_and_include_the_end_key() {
        let db = MemoryAdapter::new("t");
        for id in ["a", "b", "c"] {
            db.bulk_docs(
                vec![Document::from_json(serde_json::json!({"_id": id})).unwrap()],
                BulkDocsOptions::new(),
            )
            .await
            .unwrap();
        }
        let map = |doc: &serde_json::Value| vec![(doc["_id"].clone(), serde_json::json!(1))];
        let reduced = query_view(&db, &map, Some(&ReduceFn::Count), Default::default())
            .await
            .unwrap();
        assert_eq!(reduced.rows.len(), 1);
        assert_eq!(reduced.rows[0].value, serde_json::json!(3));
        let range = query_view(
            &db,
            &map,
            None,
            ViewQueryOptions {
                end_key: Some(serde_json::json!("b")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let keys: Vec<_> = range.rows.iter().map(|r| r.key.clone()).collect();
        assert_eq!(keys, [serde_json::json!("a"), serde_json::json!("b")]);
    }

    #[test]
    fn reduce_keys_need_exact_grouping_like_couchdb() {
        // CouchDB 3.5.1 (`couch_mrview_util:validate_args`): with a reduce,
        // a `keys` list of more than one key needs `group=true` and no
        // `group_level`; a single key behaves like `key`.
        use serde_json::json;
        let rows: Vec<EmittedRow> = [("a1", "a"), ("a2", "a"), ("b1", "b")]
            .into_iter()
            .map(|(id, prefix)| EmittedRow {
                id: id.into(),
                key: json!([prefix, id]),
                value: json!(1),
            })
            .collect();
        let two = || Some(vec![json!(["a", "a1"]), json!(["a", "a2"])]);
        let one = || Some(vec![json!(["a", "a1"])]);
        let run = |opts: ViewQueryOptions| {
            query_emitted(rows.clone(), Some(&ReduceFn::Count), &opts).map(|r| {
                r.rows
                    .into_iter()
                    .map(|row| (row.key, row.value))
                    .collect::<Vec<_>>()
            })
        };

        for opts in [
            ViewQueryOptions {
                keys: two(),
                group_level: Some(1),
                ..ViewQueryOptions::new()
            },
            ViewQueryOptions {
                keys: two(),
                group: true,
                group_level: Some(1),
                ..ViewQueryOptions::new()
            },
            ViewQueryOptions {
                keys: two(),
                group_level: Some(0),
                ..ViewQueryOptions::new()
            },
            ViewQueryOptions {
                keys: two(),
                ..ViewQueryOptions::new()
            },
            ViewQueryOptions {
                keys: Some(vec![]),
                ..ViewQueryOptions::new()
            },
        ] {
            match run(opts.clone()) {
                Err(RouchError::BadRequest(reason)) => assert_eq!(
                    reason,
                    "Multi-key fetches for reduce views must use `group=true`"
                ),
                other => panic!("{opts:?} must be rejected, got {other:?}"),
            }
        }

        let grouped = run(ViewQueryOptions {
            keys: two(),
            group: true,
            ..ViewQueryOptions::new()
        });
        assert_eq!(
            grouped.unwrap(),
            [
                (json!(["a", "a1"]), json!(1)),
                (json!(["a", "a2"]), json!(1))
            ]
        );
        let single = |group, group_level| {
            run(ViewQueryOptions {
                keys: one(),
                group,
                group_level,
                ..ViewQueryOptions::new()
            })
            .unwrap()
        };
        assert_eq!(single(false, None), [(Value::Null, json!(1))]);
        assert_eq!(single(false, Some(1)), [(json!(["a"]), json!(1))]);
        assert_eq!(single(true, None), [(json!(["a", "a1"]), json!(1))]);
        // `keys` wins over `key`, whatever its length.
        let with_key = run(ViewQueryOptions {
            key: Some(json!(["b", "b1"])),
            keys: one(),
            ..ViewQueryOptions::new()
        });
        assert_eq!(with_key.unwrap(), [(Value::Null, json!(1))]);
        // Without the reduce any `keys` list is fine (but not grouping).
        let map = query_emitted(
            rows.clone(),
            Some(&ReduceFn::Count),
            &ViewQueryOptions {
                keys: two(),
                reduce: false,
                ..ViewQueryOptions::new()
            },
        )
        .unwrap();
        assert_eq!(map.rows.len(), 2);
        assert_eq!(map.offset, 0);
    }

    #[tokio::test]
    async fn query_view_reduces_a_single_key_without_grouping() {
        // `query_view` validates before mapping: a one-element `keys` must
        // pass that check too (CouchDB treats it as `key`).
        let db = setup_db().await;
        let r = query_view(
            &db,
            &by_city,
            Some(&ReduceFn::Count),
            ViewQueryOptions {
                keys: Some(vec![serde_json::json!("NYC")]),
                ..ViewQueryOptions::new()
            },
        )
        .await
        .unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0].key, Value::Null);
        assert_eq!(r.rows[0].value, serde_json::json!(2));
    }

    #[tokio::test]
    async fn keys_queries_keep_key_order_and_duplicates() {
        // F106: keys are looked up by binary search; results follow the
        // order of `keys` (reversed when descending), duplicates included.
        let db = setup_db().await;
        let keys = Some(vec![
            serde_json::json!("NYC"),
            serde_json::json!("nope"),
            serde_json::json!("LA"),
            serde_json::json!("LA"),
        ]);
        let r = query_view(
            &db,
            &by_city,
            None,
            ViewQueryOptions {
                keys: keys.clone(),
                ..ViewQueryOptions::new()
            },
        )
        .await
        .unwrap();
        let ids: Vec<_> = r.rows.iter().map(|r| r.id.clone().unwrap()).collect();
        assert_eq!(ids, ["alice", "charlie", "bob", "bob"]);
        let r = query_view(
            &db,
            &by_city,
            None,
            ViewQueryOptions {
                keys: keys.clone(),
                descending: true,
                ..ViewQueryOptions::new()
            },
        )
        .await
        .unwrap();
        let ids: Vec<_> = r.rows.iter().map(|r| r.id.clone().unwrap()).collect();
        assert_eq!(ids, ["bob", "bob", "charlie", "alice"]);

        // With a reduce, multi-key queries need grouping and give one row
        // per requested key.
        let r = query_view(
            &db,
            &by_city,
            Some(&ReduceFn::Count),
            ViewQueryOptions {
                keys: keys.clone(),
                group: true,
                ..ViewQueryOptions::new()
            },
        )
        .await
        .unwrap();
        let rows: Vec<_> = r
            .rows
            .iter()
            .map(|r| (r.key.clone(), r.value.clone()))
            .collect();
        assert_eq!(
            rows,
            [
                (serde_json::json!("NYC"), serde_json::json!(2)),
                (serde_json::json!("LA"), serde_json::json!(1)),
                (serde_json::json!("LA"), serde_json::json!(1)),
            ]
        );
        assert!(
            query_view(
                &db,
                &by_city,
                Some(&ReduceFn::Count),
                ViewQueryOptions {
                    keys,
                    ..ViewQueryOptions::new()
                },
            )
            .await
            .is_err()
        );
    }
}
