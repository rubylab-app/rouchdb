//! Mango query engine — CouchDB-compatible selector-based document queries.
//!
//! Supports the standard Mango operators: `$eq`, `$ne`, `$gt`, `$gte`, `$lt`,
//! `$lte`, `$in`, `$nin`, `$exists`, `$regex`, `$beginsWith`, `$elemMatch`,
//! `$allMatch`, `$keyMapMatch`, `$all`, `$size`, `$or`, `$and`, `$not`,
//! `$nor`, `$mod`, `$type`.
//!
//! Selectors are normalized the way CouchDB does it: nested sub-documents
//! are dotted paths (`{"a": {"b": 1}}` is `{"a.b": 1}`), combinators nested
//! inside a field apply to that field, and negations are pushed down to the
//! field conditions. A field that is missing from a document only matches
//! `{"$exists": false}`.

use std::cmp::Ordering;
use std::collections::HashMap;

use fancy_regex::Regex;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use rouchdb_core::adapter::Adapter;
use rouchdb_core::collation::collate;
use rouchdb_core::document::{AllDocsOptions, ChangeEvent};
use rouchdb_core::error::{Result, RouchError};

/// Definition of a Mango index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexDefinition {
    /// Index name (auto-generated if not provided).
    pub name: String,
    /// Fields to index, in order.
    pub fields: Vec<SortField>,
    /// Optional design document name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ddoc: Option<String>,
}

/// Information about an existing index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexInfo {
    /// Index name.
    pub name: String,
    /// Design document ID (if any).
    pub ddoc: Option<String>,
    /// Indexed fields.
    pub def: IndexFields,
}

/// The fields portion of an index definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexFields {
    pub fields: Vec<SortField>,
}

/// Result of creating an index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateIndexResponse {
    /// `"created"` or `"exists"`.
    pub result: String,
    /// Index name.
    pub name: String,
}

/// Response from `explain()` describing how a query would be executed.
#[derive(Debug, Clone, Serialize)]
pub struct ExplainResponse {
    pub dbname: String,
    pub index: ExplainIndex,
    pub selector: serde_json::Value,
    pub fields: Option<Vec<String>>,
}

/// Description of the index used by a query.
#[derive(Debug, Clone, Serialize)]
pub struct ExplainIndex {
    pub ddoc: Option<String>,
    pub name: String,
    #[serde(rename = "type")]
    pub index_type: String,
    pub def: IndexFields,
}

/// A built in-memory index: entries of (composite_key, doc_id) sorted by key
/// (CouchDB collation) and then by doc id.
#[derive(Debug, Clone)]
pub struct BuiltIndex {
    pub def: IndexDefinition,
    pub entries: Vec<(Vec<serde_json::Value>, String)>,
}

impl BuiltIndex {
    /// Find doc IDs matching a simple equality/range selector on the indexed fields.
    ///
    /// The result is a superset of the matching documents: only `$eq`, `$gt`,
    /// `$gte`, `$lt` and `$lte` on the first indexed field narrow it (by
    /// binary search), so callers must still check each candidate against
    /// the full selector.
    pub fn find_matching(&self, selector: &serde_json::Value) -> Vec<String> {
        if self.def.fields.is_empty() {
            return Vec::new();
        }

        let (first_field, _) = self.def.fields[0].field_and_direction();
        let (mut start, mut end) = (0, self.entries.len());
        match selector.get(first_field) {
            Some(Value::Object(ops)) => {
                for (op, operand) in ops {
                    match op.as_str() {
                        "$eq" => {
                            start = start.max(self.lower_bound(operand));
                            end = end.min(self.upper_bound(operand));
                        }
                        "$gt" => start = start.max(self.upper_bound(operand)),
                        "$gte" => start = start.max(self.lower_bound(operand)),
                        "$lt" => end = end.min(self.lower_bound(operand)),
                        "$lte" => end = end.min(self.upper_bound(operand)),
                        // Anything else does not narrow the range.
                        _ => {}
                    }
                }
            }
            // Implicit $eq
            Some(other) => {
                start = self.lower_bound(other);
                end = self.upper_bound(other);
            }
            // Selector doesn't use the indexed field, can't narrow.
            None => {}
        }

        if start >= end {
            return Vec::new();
        }
        self.entries[start..end]
            .iter()
            .map(|(_, id)| id.clone())
            .collect()
    }

    /// Bring the index up to date with a batch of changes read from the
    /// changes feed with `include_docs: true`.
    ///
    /// The previous entry of every changed document is dropped, and live,
    /// non-design documents are indexed again from the included body.
    pub fn apply_changes(&mut self, changes: &[ChangeEvent]) {
        if changes.is_empty() {
            return;
        }
        // Last change wins if a document appears more than once.
        let mut latest: HashMap<&str, &ChangeEvent> = HashMap::new();
        for event in changes {
            latest.insert(event.id.as_str(), event);
        }
        self.entries
            .retain(|(_, id)| !latest.contains_key(id.as_str()));
        for (id, event) in latest {
            if event.deleted || is_design_doc(id) {
                continue;
            }
            if let Some(ref doc) = event.doc {
                self.entries
                    .push((index_key(&self.def, doc), id.to_string()));
            }
        }
        sort_entries(&mut self.entries);
    }

    /// First entry whose leading key is not less than `value`.
    fn lower_bound(&self, value: &Value) -> usize {
        self.entries
            .partition_point(|(key, _)| collate(&key[0], value) == Ordering::Less)
    }

    /// First entry whose leading key is greater than `value`.
    fn upper_bound(&self, value: &Value) -> usize {
        self.entries
            .partition_point(|(key, _)| collate(&key[0], value) != Ordering::Greater)
    }
}

/// Composite index key of a document; missing fields are indexed as `null`.
fn index_key(def: &IndexDefinition, doc: &Value) -> Vec<Value> {
    def.fields
        .iter()
        .map(|sf| {
            let (field, _) = sf.field_and_direction();
            get_nested_field(doc, field).cloned().unwrap_or(Value::Null)
        })
        .collect()
}

/// Sort index entries by composite key, then by doc id.
fn sort_entries(entries: &mut [(Vec<Value>, String)]) {
    entries.sort_by(|(ka, ia), (kb, ib)| {
        for (va, vb) in ka.iter().zip(kb.iter()) {
            let cmp = collate(va, vb);
            if cmp != Ordering::Equal {
                return cmp;
            }
        }
        ia.cmp(ib)
    });
}

fn is_design_doc(id: &str) -> bool {
    id.starts_with("_design/")
}

/// Build an index from all documents in an adapter.
///
/// Design documents are not indexed.
pub async fn build_index(adapter: &dyn Adapter, def: &IndexDefinition) -> Result<BuiltIndex> {
    let all = adapter
        .all_docs(AllDocsOptions {
            include_docs: true,
            ..AllDocsOptions::new()
        })
        .await?;

    let mut entries: Vec<(Vec<serde_json::Value>, String)> = Vec::new();

    for row in &all.rows {
        if is_design_doc(&row.id) {
            continue;
        }
        if let Some(ref doc_json) = row.doc {
            entries.push((index_key(def, doc_json), row.id.clone()));
        }
    }

    sort_entries(&mut entries);

    Ok(BuiltIndex {
        def: def.clone(),
        entries,
    })
}

/// Options for a Mango find query.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FindOptions {
    /// The selector (query) to match documents against.
    pub selector: serde_json::Value,
    /// Fields to include in the result (projection).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<Vec<String>>,
    /// Sort specification.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort: Option<Vec<SortField>>,
    /// Maximum number of results.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    /// Number of results to skip.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip: Option<u64>,
}

/// A single sort field with direction.
///
/// Deserializing accepts a field name or an object with exactly one field
/// whose direction is `"asc"` or `"desc"`, and rejects anything else (as
/// CouchDB does with `invalid_sort_field`).
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum SortField {
    /// Simple field name (ascending).
    Simple(String),
    /// Field with direction: `{"field": "asc"}` or `{"field": "desc"}`.
    WithDirection(HashMap<String, String>),
}

impl<'de> Deserialize<'de> for SortField {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let value = Value::deserialize(d)?;
        let sort_field = match value {
            Value::String(field) => SortField::Simple(field),
            Value::Object(map) => SortField::WithDirection(
                map.into_iter()
                    .map(|(k, v)| (k, v.as_str().unwrap_or_default().to_string()))
                    .collect(),
            ),
            other => {
                return Err(serde::de::Error::custom(format!(
                    "Invalid sort field: {other}"
                )));
            }
        };
        // Report the bare reason, without `RouchError`'s "bad request: ".
        if let Err(RouchError::BadRequest(reason)) = sort_field.try_field_and_direction() {
            return Err(serde::de::Error::custom(reason));
        }
        Ok(sort_field)
    }
}

impl SortField {
    /// The field and its direction.
    ///
    /// An invalid `WithDirection` map (empty, several fields, or a direction
    /// other than `"asc"`/`"desc"`) does not panic; use
    /// [`try_field_and_direction`](Self::try_field_and_direction) to reject it.
    pub fn field_and_direction(&self) -> (&str, SortDirection) {
        match self {
            SortField::Simple(f) => (f.as_str(), SortDirection::Asc),
            SortField::WithDirection(map) => match map.iter().next() {
                Some((field, dir)) => {
                    let direction = if dir == "desc" {
                        SortDirection::Desc
                    } else {
                        SortDirection::Asc
                    };
                    (field.as_str(), direction)
                }
                None => ("", SortDirection::Asc),
            },
        }
    }

    /// The field and its direction, or `BadRequest` if the sort field is not
    /// a single field with an `"asc"` or `"desc"` direction.
    pub fn try_field_and_direction(&self) -> Result<(&str, SortDirection)> {
        let invalid = || {
            let json = serde_json::to_string(self).unwrap_or_default();
            RouchError::BadRequest(format!("Invalid sort field: {json}"))
        };
        match self {
            SortField::Simple(f) => Ok((f.as_str(), SortDirection::Asc)),
            SortField::WithDirection(map) => {
                if map.len() != 1 {
                    return Err(invalid());
                }
                let (field, dir) = map.iter().next().ok_or_else(invalid)?;
                match dir.as_str() {
                    "asc" => Ok((field.as_str(), SortDirection::Asc)),
                    "desc" => Ok((field.as_str(), SortDirection::Desc)),
                    _ => Err(invalid()),
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SortDirection {
    Asc,
    Desc,
}

/// Result of a find query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindResponse {
    pub docs: Vec<serde_json::Value>,
}

/// Execute a Mango find query against an adapter.
///
/// The selector and sort are validated before any document is read;
/// invalid ones return `BadRequest`.
pub async fn find(adapter: &dyn Adapter, opts: FindOptions) -> Result<FindResponse> {
    let query = Query::new(&opts)?;

    // Fetch all documents
    let all = adapter
        .all_docs(AllDocsOptions {
            include_docs: true,
            ..AllDocsOptions::new()
        })
        .await?;

    Ok(query.run(all.rows.into_iter().filter_map(|row| row.doc), &opts))
}

/// Apply a Mango query (selector, sort, skip, limit and field projection)
/// to an in-memory set of documents, such as the candidates returned by an
/// index.
///
/// Design documents are never returned, matching CouchDB.
pub fn find_in_docs<I>(docs: I, opts: &FindOptions) -> Result<FindResponse>
where
    I: IntoIterator<Item = serde_json::Value>,
{
    Ok(Query::new(opts)?.run(docs, opts))
}

/// A validated find query: compiled selector plus parsed sort paths.
struct Query {
    selector: CompiledSelector,
    sort: Vec<(Vec<String>, SortDirection)>,
    /// The selector is nothing but an empty combinator (see
    /// [`is_empty_combinator`]): CouchDB does not run the query at all.
    no_op: bool,
}

impl Query {
    fn new(opts: &FindOptions) -> Result<Self> {
        let selector = CompiledSelector::new(&opts.selector)?;
        let mut sort = Vec::new();
        for sf in opts.sort.iter().flatten() {
            let (field, direction) = sf.try_field_and_direction()?;
            sort.push((parse_field(field)?, direction));
        }
        Ok(Query {
            selector,
            sort,
            no_op: is_empty_combinator(&opts.selector),
        })
    }

    fn run<I>(&self, docs: I, opts: &FindOptions) -> FindResponse
    where
        I: IntoIterator<Item = serde_json::Value>,
    {
        if self.no_op {
            return FindResponse { docs: Vec::new() };
        }
        let mut matched: Vec<Value> = docs
            .into_iter()
            .filter(|doc| {
                !doc.get("_id")
                    .and_then(Value::as_str)
                    .is_some_and(is_design_doc)
                    && self.selector.matches(doc)
            })
            .collect();

        // CouchDB sorts with an index on the sort fields, which only holds
        // the documents that have all of them: a document missing a sort
        // field is never part of a sorted result.
        matched.retain(|doc| {
            self.sort
                .iter()
                .all(|(path, _)| lookup(doc, path).is_some())
        });

        // CouchDB serves a sort by walking an index on the sort fields (in
        // one direction for all of them), backwards for a descending sort:
        // ties come in index order, reversed when descending. The input is
        // in index order (or _id order without one) and the sort is stable.
        if !self.sort.is_empty() {
            if self.sort[0].1 == SortDirection::Desc {
                matched.reverse();
            }
            matched.sort_by(|a, b| {
                for (path, direction) in &self.sort {
                    let va = lookup(a, path).unwrap_or(&Value::Null);
                    let vb = lookup(b, path).unwrap_or(&Value::Null);
                    let cmp = collate(va, vb);
                    let cmp = if *direction == SortDirection::Desc {
                        cmp.reverse()
                    } else {
                        cmp
                    };
                    if cmp != Ordering::Equal {
                        return cmp;
                    }
                }
                Ordering::Equal
            });
        }

        // Skip and limit
        let skip = opts.skip.unwrap_or(0) as usize;
        let limit = opts.limit.map_or(usize::MAX, |l| l as usize);
        let page = matched.into_iter().skip(skip).take(limit);

        // Field projection
        let docs = match opts.fields {
            Some(ref fields) => page.map(|doc| project(doc, fields)).collect(),
            None => page.collect(),
        };
        FindResponse { docs }
    }
}

/// Whether a `_find` selector normalizes to a bare empty `$and` / `$or`, such
/// as `{"$and": []}`, `{"$nor": []}`, `{"f": {"$or": []}}` or
/// `{"$not": {"$and": []}}`.
///
/// CouchDB returns no documents for such a query (its cursor turns it into an
/// empty index range), although an empty combinator is otherwise always true:
/// `{"f": 1, "$or": []}` is `{"f": 1}`, and the `_changes` selector filter
/// passes every change for `{"$and": []}`. Field names over a combinator
/// vanish in CouchDB's normalization, and negations keep it empty.
fn is_empty_combinator(selector: &Value) -> bool {
    let Value::Object(map) = selector else {
        return false;
    };
    let mut entries = map.iter();
    let (Some((key, arg)), None) = (entries.next(), entries.next()) else {
        return false;
    };
    match key.as_str() {
        "$and" | "$or" | "$nor" => arg.as_array().is_some_and(Vec::is_empty),
        "$not" => is_empty_combinator(arg),
        op if op.starts_with('$') => false,
        _field => is_empty_combinator(arg),
    }
}

/// Check if a document matches a Mango selector.
///
/// An invalid selector matches nothing; use [`CompiledSelector::new`] to
/// get the validation error.
pub fn matches_selector(doc: &serde_json::Value, selector: &serde_json::Value) -> bool {
    CompiledSelector::new(selector).is_ok_and(|s| s.matches(doc))
}

// ---------------------------------------------------------------------------
// Selector compilation
// ---------------------------------------------------------------------------

/// A Mango selector parsed and validated once, so it can be matched against
/// many documents without re-parsing it or recompiling its regexes.
#[derive(Debug, Clone)]
pub struct CompiledSelector {
    root: Node,
}

impl CompiledSelector {
    /// Parse and validate a selector. Malformed selectors (unknown
    /// operators, bad operator arguments, invalid regexes, conditions
    /// without a field name) return `BadRequest`.
    pub fn new(selector: &serde_json::Value) -> Result<Self> {
        let root = match selector {
            Value::Object(map) => compile_object(map, &[], false, false)?,
            other => {
                return Err(bad_request(format!(
                    "Selector must be a JSON object, not: {other}"
                )));
            }
        };
        Ok(CompiledSelector { root })
    }

    /// Check if a document matches this selector.
    pub fn matches(&self, doc: &serde_json::Value) -> bool {
        self.root.matches(doc)
    }
}

/// Normalized selector tree: combinators over field conditions.
#[derive(Debug, Clone)]
enum Node {
    And(Vec<Node>),
    Or(Vec<Node>),
    /// A condition on the value at `path` (an empty path is the value
    /// itself, used inside `$elemMatch`, `$allMatch` and `$keyMapMatch`).
    Field {
        path: Vec<String>,
        cond: Cond,
    },
}

/// A condition applied to a field value that exists.
#[derive(Debug, Clone)]
enum Cond {
    Eq(Value),
    Ne(Value),
    Lt(Value),
    Lte(Value),
    Gt(Value),
    Gte(Value),
    In(Vec<Value>),
    Nin(Vec<Value>),
    Exists(bool),
    Type(String),
    Regex(Regex),
    BeginsWith(String),
    Size(u64),
    Mod(i64, i64),
    All(Vec<Value>),
    ElemMatch(Box<Node>),
    AllMatch(Box<Node>),
    KeyMapMatch(Box<Node>),
    Not(Box<Cond>),
}

impl Cond {
    /// The complement of this condition (for a field that exists).
    fn negate(self) -> Cond {
        match self {
            Cond::Eq(v) => Cond::Ne(v),
            Cond::Ne(v) => Cond::Eq(v),
            Cond::Lt(v) => Cond::Gte(v),
            Cond::Lte(v) => Cond::Gt(v),
            Cond::Gt(v) => Cond::Lte(v),
            Cond::Gte(v) => Cond::Lt(v),
            Cond::In(v) => Cond::Nin(v),
            Cond::Nin(v) => Cond::In(v),
            Cond::Exists(b) => Cond::Exists(!b),
            Cond::Not(c) => *c,
            other => Cond::Not(Box::new(other)),
        }
    }

    fn matches(&self, value: &Value) -> bool {
        let eq = |a: &Value, b: &Value| collate(a, b) == Ordering::Equal;
        match self {
            Cond::Eq(arg) => eq(value, arg),
            Cond::Ne(arg) => !eq(value, arg),
            Cond::Lt(arg) => collate(value, arg) == Ordering::Less,
            Cond::Lte(arg) => collate(value, arg) != Ordering::Greater,
            Cond::Gt(arg) => collate(value, arg) == Ordering::Greater,
            Cond::Gte(arg) => collate(value, arg) != Ordering::Less,
            Cond::In(args) => in_matches(args, value),
            Cond::Nin(args) => !in_matches(args, value),
            Cond::Exists(should_exist) => *should_exist,
            Cond::Type(name) => json_type_name(value) == name,
            // A pattern that exceeds the backtracking limit does not match,
            // as in CouchDB (which catches `re:run` errors).
            Cond::Regex(re) => value
                .as_str()
                .is_some_and(|s| re.is_match(s).unwrap_or(false)),
            Cond::BeginsWith(prefix) => value.as_str().is_some_and(|s| s.starts_with(prefix)),
            Cond::Size(n) => value.as_array().is_some_and(|a| a.len() as u64 == *n),
            Cond::Mod(divisor, remainder) => {
                // i128 so that i64::MIN % -1 (and u64 values) cannot overflow.
                let n = value
                    .as_i64()
                    .map(i128::from)
                    .or_else(|| value.as_u64().map(i128::from));
                n.is_some_and(|n| n % i128::from(*divisor) == i128::from(*remainder))
            }
            Cond::All(args) => match value {
                Value::Array(items) => {
                    // Like CouchDB, membership is exact term equality
                    // (`lists:member`): 50.0 is not an element of [50].
                    let has_args = !args.is_empty() && args.iter().all(|a| items.contains(a));
                    // {"$all": [[1, 2]]} also matches the array [1, 2]
                    // itself, compared with Erlang's `==` (50.0 == 50).
                    let is_args =
                        matches!(args.as_slice(), [arg @ Value::Array(_)] if value_eq(arg, value));
                    has_args || is_args
                }
                _ => false,
            },
            Cond::ElemMatch(node) => value
                .as_array()
                .is_some_and(|a| a.iter().any(|e| node.matches(e))),
            Cond::AllMatch(node) => value
                .as_array()
                .is_some_and(|a| !a.is_empty() && a.iter().all(|e| node.matches(e))),
            Cond::KeyMapMatch(node) => value
                .as_object()
                .is_some_and(|o| o.keys().any(|k| node.matches(&Value::String(k.clone())))),
            Cond::Not(cond) => !cond.matches(value),
        }
    }
}

/// Erlang's `==` on JSON terms: numbers are equal when their values are
/// (`50 == 50.0`), everything else must be identical.
fn value_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_i64(), y.as_i64()) {
            (Some(x), Some(y)) => x == y,
            _ if x.is_u64() && y.is_u64() => x.as_u64() == y.as_u64(),
            _ => x.as_f64() == y.as_f64(),
        },
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(x, y)| value_eq(x, y))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y)
                    .all(|((kx, vx), (ky, vy))| kx == ky && value_eq(vx, vy))
        }
        _ => a == b,
    }
}

/// `$in`: on an array field, any element equals any argument.
fn in_matches(args: &[Value], value: &Value) -> bool {
    let eq = |a: &Value, b: &Value| collate(a, b) == Ordering::Equal;
    match value {
        Value::Array(items) => items.iter().any(|i| args.iter().any(|a| eq(i, a))),
        _ => args.iter().any(|a| eq(value, a)),
    }
}

impl Node {
    fn always() -> Node {
        Node::And(Vec::new())
    }

    fn never() -> Node {
        Node::Or(Vec::new())
    }

    fn matches(&self, value: &Value) -> bool {
        match self {
            Node::And(nodes) => nodes.iter().all(|n| n.matches(value)),
            Node::Or(nodes) => nodes.iter().any(|n| n.matches(value)),
            Node::Field { path, cond } => match resolve(value, path) {
                Lookup::Found(v) => cond.matches(v),
                // Only {"$exists": false} matches a missing field.
                Lookup::Missing => matches!(cond, Cond::Exists(false)),
                Lookup::BadPath => false,
            },
        }
    }
}

fn bad_request(reason: String) -> RouchError {
    RouchError::BadRequest(reason)
}

fn bad_arg(op: &str, arg: &Value) -> RouchError {
    bad_request(format!("Bad argument for operator {op}: {arg}"))
}

/// Compile the conditions of a selector object that apply at `path`.
///
/// `negate` is set under an odd number of `$not`/`$nor`, in which case the
/// conditions are negated and combined with De Morgan's laws. `nested` is
/// set inside `$elemMatch`/`$allMatch`/`$keyMapMatch`, where operators may
/// apply to the value itself (an empty path).
fn compile_object(
    map: &Map<String, Value>,
    path: &[String],
    negate: bool,
    nested: bool,
) -> Result<Node> {
    if map.is_empty() {
        // `{}` as a whole selector matches everything; as the condition of
        // a field it is an equality test against `{}`.
        if path.is_empty() {
            return Ok(if negate {
                Node::never()
            } else {
                Node::always()
            });
        }
        return leaf(path, Cond::Eq(Value::Object(Map::new())), negate, nested);
    }

    let mut nodes = Vec::with_capacity(map.len());
    for (key, value) in map {
        nodes.push(compile_entry(key, value, path, negate, nested)?);
    }
    Ok(if nodes.len() == 1 {
        nodes.remove(0)
    } else if negate {
        Node::Or(nodes)
    } else {
        Node::And(nodes)
    })
}

fn compile_entry(
    key: &str,
    value: &Value,
    path: &[String],
    negate: bool,
    nested: bool,
) -> Result<Node> {
    match key {
        "$and" | "$or" | "$nor" => {
            let args = value.as_array().ok_or_else(|| bad_arg(key, value))?;
            // CouchDB treats an empty combinator as always true, whatever it
            // is and however it is negated (see `find` for the one exception:
            // a query that is nothing but an empty combinator).
            if args.is_empty() {
                return Ok(Node::always());
            }
            // $nor is the conjunction of the negated arguments.
            let arg_negate = if key == "$nor" { !negate } else { negate };
            let children = args
                .iter()
                .map(|arg| compile_argument(arg, path, arg_negate, nested))
                .collect::<Result<Vec<_>>>()?;
            let conjunction = (key != "$or") != negate;
            Ok(if conjunction {
                Node::And(children)
            } else {
                Node::Or(children)
            })
        }
        "$not" => match value {
            Value::Object(map) => compile_object(map, path, !negate, nested),
            _ => Err(bad_arg(key, value)),
        },
        op if op.starts_with('$') => leaf(path, compile_operator(op, value)?, negate, nested),
        field => {
            let mut sub_path = path.to_vec();
            sub_path.extend(parse_field(field)?);
            compile_value(value, &sub_path, negate, nested)
        }
    }
}

/// Compile the argument of a combinator or of `$not`.
fn compile_argument(arg: &Value, path: &[String], negate: bool, nested: bool) -> Result<Node> {
    match arg {
        Value::Object(map) => compile_object(map, path, negate, nested),
        // A scalar under a field is an implicit $eq.
        _ if !path.is_empty() || nested => compile_value(arg, path, negate, nested),
        // A scalar in place of a whole selector matches nothing.
        _ => Ok(if negate {
            Node::always()
        } else {
            Node::never()
        }),
    }
}

/// Compile the condition given for a field: a sub-selector object or a
/// value that the field must equal.
fn compile_value(value: &Value, path: &[String], negate: bool, nested: bool) -> Result<Node> {
    match value {
        Value::Object(map) => compile_object(map, path, negate, nested),
        other => leaf(path, Cond::Eq(other.clone()), negate, nested),
    }
}

fn leaf(path: &[String], cond: Cond, negate: bool, nested: bool) -> Result<Node> {
    if path.is_empty() && !nested {
        return Err(bad_request(
            "One or more conditions is missing a field name.".into(),
        ));
    }
    let cond = if negate { cond.negate() } else { cond };
    Ok(Node::Field {
        path: path.to_vec(),
        cond,
    })
}

/// Compile the selector argument of `$elemMatch`, `$allMatch` and
/// `$keyMapMatch`, which is matched against each element (or key).
fn compile_nested(op: &str, arg: &Value) -> Result<Node> {
    match arg {
        // An empty sub-selector matches no element (as in CouchDB).
        Value::Object(map) if map.is_empty() => Ok(Node::never()),
        Value::Object(map) => compile_object(map, &[], false, true),
        // A bare value in $elemMatch is an implicit $eq on each element.
        _ if op == "$elemMatch" => compile_value(arg, &[], false, true),
        _ => Err(bad_arg(op, arg)),
    }
}

fn compile_operator(op: &str, arg: &Value) -> Result<Cond> {
    let array_arg = || arg.as_array().cloned().ok_or_else(|| bad_arg(op, arg));
    let string_arg = || {
        arg.as_str()
            .map(str::to_string)
            .ok_or_else(|| bad_arg(op, arg))
    };
    Ok(match op {
        "$eq" => Cond::Eq(arg.clone()),
        "$ne" => Cond::Ne(arg.clone()),
        "$lt" => Cond::Lt(arg.clone()),
        "$lte" => Cond::Lte(arg.clone()),
        "$gt" => Cond::Gt(arg.clone()),
        "$gte" => Cond::Gte(arg.clone()),
        "$in" => Cond::In(array_arg()?),
        "$nin" => Cond::Nin(array_arg()?),
        "$all" => Cond::All(array_arg()?),
        "$exists" => Cond::Exists(arg.as_bool().ok_or_else(|| bad_arg(op, arg))?),
        "$type" => Cond::Type(string_arg()?),
        "$beginsWith" => Cond::BeginsWith(string_arg()?),
        "$regex" => {
            let pattern = string_arg()?;
            // fancy-regex, like CouchDB's PCRE, supports lookaround and
            // backreferences.
            let re = Regex::new(&pattern).map_err(|_| bad_arg(op, arg))?;
            Cond::Regex(re)
        }
        "$size" => Cond::Size(arg.as_u64().ok_or_else(|| bad_arg(op, arg))?),
        "$mod" => match arg.as_array().map(Vec::as_slice) {
            Some([d, r]) => match (d.as_i64(), r.as_i64()) {
                (Some(d), Some(r)) if d != 0 => Cond::Mod(d, r),
                _ => return Err(bad_arg(op, arg)),
            },
            _ => return Err(bad_arg(op, arg)),
        },
        "$elemMatch" => Cond::ElemMatch(Box::new(compile_nested(op, arg)?)),
        "$allMatch" => Cond::AllMatch(Box::new(compile_nested(op, arg)?)),
        "$keyMapMatch" => Cond::KeyMapMatch(Box::new(compile_nested(op, arg)?)),
        _ => return Err(bad_request(format!("Invalid operator: {op}"))),
    })
}

// ---------------------------------------------------------------------------
// Field paths
// ---------------------------------------------------------------------------

/// Split a field name into path segments on unescaped dots, dropping the
/// escaping backslashes (`"a\\.b"` is the single field `a.b`), like CouchDB.
fn split_field(field: &str) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut prev = None;
    for c in field.chars() {
        if c == '.' && prev != Some('\\') {
            parts.push(String::new());
        } else if c != '\\' {
            parts.last_mut().expect("parts is never empty").push(c);
        }
        prev = Some(c);
    }
    parts
}

/// Like [`split_field`], but rejects empty segments (`"a..b"`).
fn parse_field(field: &str) -> Result<Vec<String>> {
    let parts = split_field(field);
    if parts.iter().any(String::is_empty) {
        return Err(bad_request(format!("Invalid field name: {field}")));
    }
    Ok(parts)
}

enum Lookup<'a> {
    Found(&'a Value),
    /// An object along the path lacks the next field.
    Missing,
    /// The path runs into a scalar, or into an array with a segment that is
    /// not a valid index.
    BadPath,
}

fn resolve<'a>(value: &'a Value, path: &[String]) -> Lookup<'a> {
    let mut current = value;
    for segment in path {
        current = match current {
            Value::Object(map) => match map.get(segment) {
                Some(v) => v,
                None => return Lookup::Missing,
            },
            Value::Array(items) => match segment.parse::<usize>().ok().and_then(|i| items.get(i)) {
                Some(v) => v,
                None => return Lookup::BadPath,
            },
            _ => return Lookup::BadPath,
        };
    }
    Lookup::Found(current)
}

fn lookup<'a>(value: &'a Value, path: &[String]) -> Option<&'a Value> {
    match resolve(value, path) {
        Lookup::Found(v) => Some(v),
        _ => None,
    }
}

/// Get a nested field from a JSON value using dot notation.
///
/// Numeric segments index into arrays (`items.0.name`) and `\.` escapes a
/// dot that is part of a field name.
pub fn get_nested_field<'a>(
    doc: &'a serde_json::Value,
    path: &str,
) -> Option<&'a serde_json::Value> {
    lookup(doc, &split_field(path))
}

/// Return the CouchDB type name for a JSON value.
fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Project a document to only include the specified fields.
///
/// Like CouchDB, nested paths keep their structure (`address.city` yields
/// `{"address": {"city": ...}}`), missing fields are skipped, and only the
/// requested fields are returned (`_id` is not added implicitly).
fn project(doc: serde_json::Value, fields: &[String]) -> serde_json::Value {
    let mut result = Map::new();
    for field in fields {
        let path = split_field(field);
        let Some(value) = lookup(&doc, &path) else {
            continue;
        };
        let mut target = &mut result;
        let (last, parents) = path.split_last().expect("split_field is never empty");
        for segment in parents {
            let entry = target
                .entry(segment.clone())
                .or_insert_with(|| Value::Object(Map::new()));
            if !entry.is_object() {
                *entry = Value::Object(Map::new());
            }
            target = entry.as_object_mut().expect("just made an object");
        }
        target.insert(last.clone(), value.clone());
    }
    Value::Object(result)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(json: serde_json::Value) -> serde_json::Value {
        json
    }

    #[test]
    fn elem_match_operator_on_scalar_array() {
        let d = doc(serde_json::json!({"scores": [50, 85, 60]}));
        // Operator expression applied directly to each element.
        assert!(matches_selector(
            &d,
            &serde_json::json!({"scores": {"$elemMatch": {"$gt": 80}}})
        ));
        assert!(!matches_selector(
            &d,
            &serde_json::json!({"scores": {"$elemMatch": {"$gt": 90}}})
        ));
        // Bare scalar operand -> implicit $eq against each element.
        assert!(matches_selector(
            &d,
            &serde_json::json!({"scores": {"$elemMatch": 85}})
        ));
    }

    #[test]
    fn elem_match_subdocument_array() {
        let d = doc(serde_json::json!({
            "items": [{"subject": "math", "score": 90}, {"subject": "art", "score": 70}]
        }));
        assert!(matches_selector(
            &d,
            &serde_json::json!({"items": {"$elemMatch": {"subject": "math"}}})
        ));
        assert!(!matches_selector(
            &d,
            &serde_json::json!({"items": {"$elemMatch": {"subject": "history"}}})
        ));
    }

    // --- Basic matching ---

    #[test]
    fn eq_implicit() {
        let d = doc(serde_json::json!({"name": "Alice", "age": 30}));
        assert!(matches_selector(&d, &serde_json::json!({"name": "Alice"})));
        assert!(!matches_selector(&d, &serde_json::json!({"name": "Bob"})));
    }

    #[test]
    fn eq_explicit() {
        let d = doc(serde_json::json!({"age": 30}));
        assert!(matches_selector(
            &d,
            &serde_json::json!({"age": {"$eq": 30}})
        ));
    }

    #[test]
    fn ne() {
        let d = doc(serde_json::json!({"age": 30}));
        assert!(matches_selector(
            &d,
            &serde_json::json!({"age": {"$ne": 25}})
        ));
        assert!(!matches_selector(
            &d,
            &serde_json::json!({"age": {"$ne": 30}})
        ));
    }

    #[test]
    fn gt_gte_lt_lte() {
        let d = doc(serde_json::json!({"age": 30}));

        assert!(matches_selector(
            &d,
            &serde_json::json!({"age": {"$gt": 20}})
        ));
        assert!(!matches_selector(
            &d,
            &serde_json::json!({"age": {"$gt": 30}})
        ));

        assert!(matches_selector(
            &d,
            &serde_json::json!({"age": {"$gte": 30}})
        ));
        assert!(!matches_selector(
            &d,
            &serde_json::json!({"age": {"$gte": 31}})
        ));

        assert!(matches_selector(
            &d,
            &serde_json::json!({"age": {"$lt": 40}})
        ));
        assert!(!matches_selector(
            &d,
            &serde_json::json!({"age": {"$lt": 30}})
        ));

        assert!(matches_selector(
            &d,
            &serde_json::json!({"age": {"$lte": 30}})
        ));
        assert!(!matches_selector(
            &d,
            &serde_json::json!({"age": {"$lte": 29}})
        ));
    }

    #[test]
    fn in_nin() {
        let d = doc(serde_json::json!({"color": "red"}));

        assert!(matches_selector(
            &d,
            &serde_json::json!({"color": {"$in": ["red", "blue"]}})
        ));
        assert!(!matches_selector(
            &d,
            &serde_json::json!({"color": {"$in": ["green", "blue"]}})
        ));

        assert!(matches_selector(
            &d,
            &serde_json::json!({"color": {"$nin": ["green", "blue"]}})
        ));
        assert!(!matches_selector(
            &d,
            &serde_json::json!({"color": {"$nin": ["red", "blue"]}})
        ));
    }

    #[test]
    fn exists() {
        let d = doc(serde_json::json!({"name": "Alice"}));

        assert!(matches_selector(
            &d,
            &serde_json::json!({"name": {"$exists": true}})
        ));
        assert!(!matches_selector(
            &d,
            &serde_json::json!({"age": {"$exists": true}})
        ));
        assert!(matches_selector(
            &d,
            &serde_json::json!({"age": {"$exists": false}})
        ));
    }

    #[test]
    fn type_check() {
        let d = doc(serde_json::json!({"name": "Alice", "age": 30, "active": true}));

        assert!(matches_selector(
            &d,
            &serde_json::json!({"name": {"$type": "string"}})
        ));
        assert!(matches_selector(
            &d,
            &serde_json::json!({"age": {"$type": "number"}})
        ));
        assert!(matches_selector(
            &d,
            &serde_json::json!({"active": {"$type": "boolean"}})
        ));
    }

    #[test]
    fn regex_match() {
        let d = doc(serde_json::json!({"name": "Alice"}));

        assert!(matches_selector(
            &d,
            &serde_json::json!({"name": {"$regex": "^Ali"}})
        ));
        assert!(!matches_selector(
            &d,
            &serde_json::json!({"name": {"$regex": "^Bob"}})
        ));
    }

    #[test]
    fn size_operator() {
        let d = doc(serde_json::json!({"tags": ["a", "b", "c"]}));

        assert!(matches_selector(
            &d,
            &serde_json::json!({"tags": {"$size": 3}})
        ));
        assert!(!matches_selector(
            &d,
            &serde_json::json!({"tags": {"$size": 2}})
        ));
    }

    #[test]
    fn all_operator() {
        let d = doc(serde_json::json!({"tags": ["a", "b", "c"]}));

        assert!(matches_selector(
            &d,
            &serde_json::json!({"tags": {"$all": ["a", "c"]}})
        ));
        assert!(!matches_selector(
            &d,
            &serde_json::json!({"tags": {"$all": ["a", "d"]}})
        ));
    }

    #[test]
    fn elem_match() {
        let d = doc(serde_json::json!({
            "scores": [
                {"subject": "math", "grade": 90},
                {"subject": "english", "grade": 75}
            ]
        }));

        assert!(matches_selector(
            &d,
            &serde_json::json!({"scores": {"$elemMatch": {"subject": "math", "grade": {"$gt": 80}}}})
        ));
        assert!(!matches_selector(
            &d,
            &serde_json::json!({"scores": {"$elemMatch": {"subject": "math", "grade": {"$gt": 95}}}})
        ));
    }

    #[test]
    fn mod_operator() {
        let d = doc(serde_json::json!({"n": 10}));

        assert!(matches_selector(
            &d,
            &serde_json::json!({"n": {"$mod": [3, 1]}})
        ));
        assert!(!matches_selector(
            &d,
            &serde_json::json!({"n": {"$mod": [3, 0]}})
        ));
    }

    // --- Logical operators ---

    #[test]
    fn and_operator() {
        let d = doc(serde_json::json!({"age": 30, "active": true}));

        assert!(matches_selector(
            &d,
            &serde_json::json!({"$and": [{"age": {"$gte": 20}}, {"active": true}]})
        ));
        assert!(!matches_selector(
            &d,
            &serde_json::json!({"$and": [{"age": {"$gte": 20}}, {"active": false}]})
        ));
    }

    #[test]
    fn or_operator() {
        let d = doc(serde_json::json!({"age": 30}));

        assert!(matches_selector(
            &d,
            &serde_json::json!({"$or": [{"age": 30}, {"age": 40}]})
        ));
        assert!(!matches_selector(
            &d,
            &serde_json::json!({"$or": [{"age": 20}, {"age": 40}]})
        ));
    }

    #[test]
    fn not_operator() {
        let d = doc(serde_json::json!({"age": 30}));

        assert!(matches_selector(
            &d,
            &serde_json::json!({"$not": {"age": 40}})
        ));
        assert!(!matches_selector(
            &d,
            &serde_json::json!({"$not": {"age": 30}})
        ));
    }

    #[test]
    fn nor_operator() {
        let d = doc(serde_json::json!({"age": 30}));

        assert!(matches_selector(
            &d,
            &serde_json::json!({"$nor": [{"age": 20}, {"age": 40}]})
        ));
        assert!(!matches_selector(
            &d,
            &serde_json::json!({"$nor": [{"age": 30}, {"age": 40}]})
        ));
    }

    // --- Nested fields ---

    #[test]
    fn nested_field_access() {
        let d = doc(serde_json::json!({"address": {"city": "NYC", "zip": "10001"}}));

        assert!(matches_selector(
            &d,
            &serde_json::json!({"address.city": "NYC"})
        ));
        assert!(!matches_selector(
            &d,
            &serde_json::json!({"address.city": "LA"})
        ));
    }

    // --- Multiple conditions ---

    #[test]
    fn multiple_field_conditions() {
        let d = doc(serde_json::json!({"name": "Alice", "age": 30}));

        // Both must match (implicit AND)
        assert!(matches_selector(
            &d,
            &serde_json::json!({"name": "Alice", "age": {"$gte": 25}})
        ));
        assert!(!matches_selector(
            &d,
            &serde_json::json!({"name": "Alice", "age": {"$gte": 35}})
        ));
    }

    #[test]
    fn combined_operators_on_field() {
        let d = doc(serde_json::json!({"age": 30}));

        // Range: 20 < age < 40
        assert!(matches_selector(
            &d,
            &serde_json::json!({"age": {"$gt": 20, "$lt": 40}})
        ));
        assert!(!matches_selector(
            &d,
            &serde_json::json!({"age": {"$gt": 30, "$lt": 40}})
        ));
    }

    // --- Projection ---

    #[test]
    fn project_fields() {
        let d = serde_json::json!({"_id": "doc1", "_rev": "1-abc", "name": "Alice", "age": 30});
        let projected = project(d.clone(), &["name".to_string()]);

        // Like CouchDB, only the requested fields are returned.
        assert_eq!(projected, serde_json::json!({"name": "Alice"}));
        let projected = project(d, &["_id".to_string(), "name".to_string()]);
        assert_eq!(projected["_id"], "doc1");
    }

    // --- Missing fields ---

    #[test]
    fn missing_field_ne_does_not_match() {
        // Like CouchDB, $ne never matches a document that lacks the field.
        let d = doc(serde_json::json!({"name": "Alice"}));
        assert!(!matches_selector(
            &d,
            &serde_json::json!({"age": {"$ne": 30}})
        ));
    }

    #[test]
    fn missing_field_eq_fails() {
        let d = doc(serde_json::json!({"name": "Alice"}));
        assert!(!matches_selector(
            &d,
            &serde_json::json!({"age": {"$eq": 30}})
        ));
    }

    // --- Regression tests for audited findings ---

    #[test]
    fn nested_subdocument_selector_matches() {
        // F14: {"a": {"b": 1}} is shorthand for {"a.b": 1}.
        let d = doc(serde_json::json!({"address": {"city": "NYC", "zip": "10001"}}));
        assert!(matches_selector(
            &d,
            &serde_json::json!({"address": {"city": "NYC"}})
        ));
        assert!(!matches_selector(
            &d,
            &serde_json::json!({"address": {"city": "LA"}})
        ));
        // Operators and sub-fields compose inside the same object.
        assert!(matches_selector(
            &d,
            &serde_json::json!({"address": {"$exists": true, "city": {"$gt": "A"}}})
        ));
        // An empty object is an equality match against `{}`.
        assert!(!matches_selector(&d, &serde_json::json!({"address": {}})));
        assert!(matches_selector(
            &serde_json::json!({"address": {}}),
            &serde_json::json!({"address": {}})
        ));
    }

    #[test]
    fn combinators_inside_field_and_elem_match() {
        // F15: $or/$and/$nor apply to the field value when nested in a field.
        let d = doc(serde_json::json!({"age": 3}));
        assert!(matches_selector(
            &d,
            &serde_json::json!({"age": {"$or": [{"$lt": 5}, {"$gt": 10}]}})
        ));
        assert!(!matches_selector(
            &d,
            &serde_json::json!({"age": {"$and": [{"$gt": 1}, {"$gt": 5}]}})
        ));
        assert!(matches_selector(
            &d,
            &serde_json::json!({"age": {"$nor": [{"$gt": 5}, {"$eq": 4}]}})
        ));
        let d = doc(serde_json::json!({
            "items": [{"subject": "math"}, {"subject": "art"}]
        }));
        assert!(matches_selector(
            &d,
            &serde_json::json!({"items": {"$elemMatch": {"$or": [{"subject": "bio"}, {"subject": "math"}]}}})
        ));
        assert!(!matches_selector(
            &d,
            &serde_json::json!({"items": {"$elemMatch": {"$or": [{"subject": "bio"}, {"subject": "gym"}]}}})
        ));
    }

    #[test]
    fn not_with_several_operators_negates_the_conjunction() {
        // F43: {"$not": {"$gt": 5, "$lt": 10}} is NOT (x > 5 AND x < 10).
        let sel = serde_json::json!({"x": {"$not": {"$gt": 5, "$lt": 10}}});
        assert!(matches_selector(&serde_json::json!({"x": 3}), &sel));
        assert!(!matches_selector(&serde_json::json!({"x": 7}), &sel));
        assert!(matches_selector(&serde_json::json!({"x": 12}), &sel));
    }

    #[test]
    fn in_and_nin_compare_array_elements() {
        // F44: $in/$nin match element-wise when the field is an array.
        let d = doc(serde_json::json!({"tags": ["rust", "db"]}));
        assert!(matches_selector(
            &d,
            &serde_json::json!({"tags": {"$in": ["rust"]}})
        ));
        assert!(!matches_selector(
            &d,
            &serde_json::json!({"tags": {"$nin": ["rust"]}})
        ));
        assert!(matches_selector(
            &d,
            &serde_json::json!({"tags": {"$nin": ["js"]}})
        ));
        // CouchDB does not compare the whole array against an $in argument.
        assert!(!matches_selector(
            &d,
            &serde_json::json!({"tags": {"$in": [["rust", "db"]]}})
        ));
    }

    #[test]
    fn mod_does_not_panic_on_overflow_and_rejects_zero_divisor() {
        // F99: i64::MIN % -1 overflows; the mathematical remainder is 0.
        let d = doc(serde_json::json!({"n": i64::MIN}));
        assert!(matches_selector(
            &d,
            &serde_json::json!({"n": {"$mod": [-1, 0]}})
        ));
        assert!(CompiledSelector::new(&serde_json::json!({"n": {"$mod": [0, 1]}})).is_err());
        assert!(CompiledSelector::new(&serde_json::json!({"n": {"$mod": [2.5, 1]}})).is_err());
    }

    #[test]
    fn missing_field_does_not_match_negations() {
        // F100: CouchDB only matches a missing field with {"$exists": false}.
        let d = doc(serde_json::json!({"name": "Alice"}));
        for sel in [
            serde_json::json!({"age": {"$ne": 30}}),
            serde_json::json!({"age": {"$nin": [30]}}),
            serde_json::json!({"$not": {"age": 30}}),
            serde_json::json!({"$nor": [{"age": 30}]}),
            serde_json::json!({"age": {"$not": {"$regex": "x"}}}),
        ] {
            assert!(!matches_selector(&d, &sel), "{sel} must not match");
        }
        assert!(matches_selector(
            &d,
            &serde_json::json!({"$not": {"age": {"$exists": true}}})
        ));
        // A path that runs into a scalar is a bad path, not a missing field.
        let d = doc(serde_json::json!({"a": 5}));
        assert!(!matches_selector(
            &d,
            &serde_json::json!({"a.b": {"$exists": false}})
        ));
    }

    #[test]
    fn array_index_and_escaped_dot_paths() {
        // F101: numeric segments index arrays and `\.` escapes a dot.
        let d = doc(serde_json::json!({
            "items": [{"name": "x"}, {"name": "y"}],
            "a.b": 1,
            "a": {"b": 2}
        }));
        assert!(matches_selector(
            &d,
            &serde_json::json!({"items.0.name": "x"})
        ));
        assert!(matches_selector(
            &d,
            &serde_json::json!({"items.1.name": "y"})
        ));
        assert!(!matches_selector(
            &d,
            &serde_json::json!({"items.2.name": "x"})
        ));
        assert!(matches_selector(&d, &serde_json::json!({"a\\.b": 1})));
        assert!(matches_selector(&d, &serde_json::json!({"a.b": 2})));
        assert_eq!(
            get_nested_field(&d, "items.1.name"),
            Some(&serde_json::json!("y"))
        );
        assert!(CompiledSelector::new(&serde_json::json!({"a..b": 1})).is_err());
    }

    #[test]
    fn invalid_selectors_are_rejected() {
        // F102: invalid regexes (and other malformed selectors) are errors,
        // not silent non-matches.
        for sel in [
            serde_json::json!({"s": {"$regex": "[a"}}),
            serde_json::json!({"s": {"$foo": 1}}),
            serde_json::json!({"s": {"$in": 1}}),
            serde_json::json!({"s": {"$size": -1}}),
            serde_json::json!({"s": {"$exists": "yes"}}),
            serde_json::json!({"$or": {"s": 1}}),
            serde_json::json!({"$gt": 1}),
        ] {
            assert!(
                CompiledSelector::new(&sel).is_err(),
                "{sel} must be rejected"
            );
            assert!(!matches_selector(&serde_json::json!({"s": "a"}), &sel));
        }
    }

    /// Ids returned by `find_in_docs`, in order.
    fn find_ids(docs: &[Value], opts: FindOptions) -> Vec<String> {
        find_in_docs(docs.to_vec(), &opts)
            .unwrap()
            .docs
            .iter()
            .map(|d| d["_id"].as_str().unwrap().to_string())
            .collect()
    }

    fn select(docs: &[Value], selector: Value) -> Vec<String> {
        find_ids(
            docs,
            FindOptions {
                selector,
                ..Default::default()
            },
        )
    }

    #[test]
    fn empty_combinators_are_true_but_a_bare_one_finds_nothing() {
        // CouchDB 3.5.1: an empty $and / $or / $nor is always true
        // (`mango_selector:match`), but a _find whose selector normalizes to
        // nothing else returns no documents (`mango_cursor:maybe_noop_range`).
        use serde_json::json;
        let docs = [json!({"_id": "a", "f": 1}), json!({"_id": "mi", "g": 1})];
        for sel in [
            json!({"$and": []}),
            json!({"$nor": []}),
            json!({"$or": []}),
            json!({"f": {"$and": []}}),
            json!({"f": {"$or": []}}),
            json!({"f": {"$nor": []}}),
            json!({"f": {"g": {"$and": []}}}),
            json!({"$not": {"$or": []}}),
            json!({"$not": {"$and": []}}),
            json!({"$not": {"$nor": []}}),
            json!({"f": {"$not": {"$and": []}}}),
        ] {
            assert!(select(&docs, sel.clone()).is_empty(), "{sel}");
        }
        for (sel, expected) in [
            (json!({"f": {"$exists": true}, "$or": []}), vec!["a"]),
            (json!({"f": {"$exists": true}, "$and": []}), vec!["a"]),
            (json!({"f": {"$and": [], "$gt": 0}}), vec!["a"]),
            (json!({"$and": [{"f": 1}, {"$nor": []}]}), vec!["a"]),
            (json!({"$or": [{"$and": []}, {"f": 1}]}), vec!["a", "mi"]),
            (json!({"$and": [{"$and": []}]}), vec!["a", "mi"]),
            // Operators are not field names: the combinator stays nested.
            (json!({"f": {"$not": {"$exists": false}}}), vec!["a"]),
        ] {
            assert_eq!(select(&docs, sel.clone()), expected, "{sel}");
        }
        // An operator's argument is not a selector of its own.
        let arrays = [json!({"_id": "x", "f": [1]}), json!({"_id": "y", "f": []})];
        assert_eq!(
            select(&arrays, json!({"f": {"$elemMatch": {"$and": []}}})),
            ["x"]
        );
        assert_eq!(
            select(&arrays, json!({"f": {"$allMatch": {"$or": []}}})),
            ["x"]
        );
        // Matching alone (the `_changes` selector filter) has no exception.
        for sel in [json!({"$and": []}), json!({"$or": []}), json!({"$nor": []})] {
            assert!(matches_selector(&docs[1], &sel), "{sel}");
        }
    }

    #[test]
    fn all_uses_exact_membership() {
        // CouchDB 3.5.1: `$all` checks membership with `lists:member` (exact
        // terms, 50.0 =/= 50) and the whole-array form with `==` (50.0 == 50).
        use serde_json::json;
        let docs = [
            json!({"_id": "a1", "sc": [50, 60]}),
            json!({"_id": "a2", "sc": [50]}),
            json!({"_id": "a3", "sc": 50}),
            json!({"_id": "a4", "sc": [50.0]}),
            json!({"_id": "a5", "sc": [50.0, 60]}),
        ];
        for (sel, expected) in [
            (json!({"sc": {"$all": [50.0]}}), vec!["a4", "a5"]),
            (json!({"sc": {"$all": [50]}}), vec!["a1", "a2"]),
            (json!({"sc": {"$all": [50, 60]}}), vec!["a1"]),
            (json!({"sc": {"$all": [[50]]}}), vec!["a2", "a4"]),
            (json!({"sc": {"$all": [[50.0]]}}), vec!["a2", "a4"]),
            (json!({"sc": {"$all": [[50.0, 60]]}}), vec!["a1", "a5"]),
            (json!({"sc": {"$all": []}}), vec![]),
        ] {
            assert_eq!(select(&docs, sel.clone()), expected, "{sel}");
        }
        assert!(value_eq(&json!({"a": [1]}), &json!({"a": [1.0]})));
        assert!(!value_eq(&json!({"a": 1}), &json!({"b": 1})));
        assert!(!value_eq(&json!([1, 2]), &json!([1])));
        assert!(!value_eq(&json!(1), &json!("1")));
        assert!(value_eq(&json!(u64::MAX), &json!(u64::MAX)));
        // Integers compare exactly, beyond f64 precision.
        assert!(!value_eq(&json!(i64::MIN), &json!(i64::MIN + 1)));
        assert!(value_eq(&json!(i64::MIN), &json!(i64::MIN)));
        assert!(!value_eq(&json!(u64::MAX), &json!(u64::MAX - 1)));
    }

    #[test]
    fn not_needs_an_object_argument() {
        // CouchDB 3.5.1: 400 bad_arg "Bad argument for operator $not: 5".
        use serde_json::json;
        for (sel, arg) in [
            (json!({"f": {"$not": 5}}), "5"),
            (json!({"$not": 5}), "5"),
            (json!({"f": {"$not": [5]}}), "[5]"),
            (json!({"f": {"$not": null}}), "null"),
            (json!({"f": {"$elemMatch": {"$not": 5}}}), "5"),
        ] {
            let Err(RouchError::BadRequest(reason)) = CompiledSelector::new(&sel) else {
                panic!("{sel} must be rejected");
            };
            assert_eq!(reason, format!("Bad argument for operator $not: {arg}"));
        }
    }

    #[test]
    fn sorted_find_skips_documents_without_the_sort_fields() {
        // CouchDB 3.5.1 serves a sort from an index on the sort fields, which
        // holds no document missing one of them.
        use serde_json::json;
        let docs = [
            json!({"_id": "a1", "f": 1, "g": {"h": 1}}),
            json!({"_id": "a3", "f": null}),
            json!({"_id": "mi", "g": 1}),
        ];
        let sorted = |selector: Value, sort: Value| {
            find_ids(
                &docs,
                FindOptions {
                    selector,
                    sort: Some(serde_json::from_value(sort).unwrap()),
                    ..Default::default()
                },
            )
        };
        let all = json!({"_id": {"$gt": null}});
        assert_eq!(sorted(all.clone(), json!(["f"])), ["a3", "a1"]);
        assert_eq!(sorted(all.clone(), json!([{"f": "desc"}])), ["a1", "a3"]);
        assert_eq!(sorted(all.clone(), json!(["g.h"])), ["a1"]);
        assert_eq!(sorted(all.clone(), json!(["f", "g"])), ["a1"]);
        assert_eq!(sorted(all.clone(), json!(["_id"])), ["a1", "a3", "mi"]);
        assert!(sorted(json!({"f": {"$exists": false}}), json!(["f"])).is_empty());
        // Without a sort nothing is skipped.
        assert_eq!(select(&docs, all), ["a1", "a3", "mi"]);
    }

    #[test]
    fn sort_ties_follow_the_input_order_reversed_when_descending() {
        // CouchDB 3.5.1 walks the index backwards for a descending sort, so
        // ties come in reverse index (here _id) order.
        use serde_json::json;
        let docs = [
            json!({"_id": "a", "k": 1}),
            json!({"_id": "b", "k": 2}),
            json!({"_id": "c", "k": 1}),
            json!({"_id": "d", "k": 2}),
        ];
        let sorted = |sort: Value| {
            find_ids(
                &docs,
                FindOptions {
                    selector: json!({"k": {"$gt": null}}),
                    sort: Some(serde_json::from_value(sort).unwrap()),
                    ..Default::default()
                },
            )
        };
        assert_eq!(sorted(json!(["k"])), ["a", "c", "b", "d"]);
        assert_eq!(sorted(json!([{"k": "asc"}])), ["a", "c", "b", "d"]);
        assert_eq!(sorted(json!([{"k": "desc"}])), ["d", "b", "c", "a"]);
    }

    #[test]
    fn regex_supports_lookaround_and_backreferences_like_pcre() {
        // CouchDB 3.5.1 (PCRE) accepts these; `(?=a)` matches "ab".
        use serde_json::json;
        let d = json!({"s": "ab", "t": "aa"});
        for (sel, expected) in [
            (json!({"s": {"$regex": "(?=a)"}}), true),
            (json!({"s": {"$regex": "^a(?!c)"}}), true),
            (json!({"s": {"$regex": "^a(?!b)"}}), false),
            (json!({"s": {"$regex": "(?<=a)b"}}), true),
            (json!({"s": {"$regex": "(a)\\1"}}), false),
            (json!({"t": {"$regex": "(a)\\1"}}), true),
            (json!({"s": {"$regex": "(?>a|ab)c|ab"}}), true),
        ] {
            assert!(CompiledSelector::new(&sel).is_ok(), "{sel}");
            assert_eq!(matches_selector(&d, &sel), expected, "{sel}");
        }
        // Still rejected when the pattern is malformed.
        let Err(RouchError::BadRequest(reason)) =
            CompiledSelector::new(&json!({"s": {"$regex": "[a"}}))
        else {
            panic!("invalid regex must be rejected");
        };
        assert_eq!(reason, r#"Bad argument for operator $regex: "[a""#);
        // A pattern that exhausts the backtracking limit matches nothing
        // instead of failing the query (CouchDB catches `re:run` errors).
        let evil = json!({"s": {"$regex": "^(a+)+\\1$"}});
        let long = json!({"s": format!("{}b", "a".repeat(64))});
        assert!(!matches_selector(&long, &evil));
    }

    #[test]
    fn error_reasons_are_couchdb_texts() {
        // The server maps these CouchDB reasons back to CouchDB's error
        // names (invalid_operator, invalid_selector, ...).
        use serde_json::json;
        for (sel, reason) in [
            (json!({"a": {"$foo": 1}}), "Invalid operator: $foo"),
            (json!({"a": {"$in": 1}}), "Bad argument for operator $in: 1"),
            (
                json!({"$gt": 1}),
                "One or more conditions is missing a field name.",
            ),
            (json!({"a..b": 1}), "Invalid field name: a..b"),
            (json!(5), "Selector must be a JSON object, not: 5"),
        ] {
            match CompiledSelector::new(&sel) {
                Err(RouchError::BadRequest(r)) => assert_eq!(r, reason, "{sel}"),
                other => panic!("{sel}: {other:?}"),
            }
        }
        let err = serde_json::from_value::<SortField>(json!({"a": "up"})).unwrap_err();
        assert_eq!(err.to_string(), r#"Invalid sort field: {"a":"up"}"#);
        let err = serde_json::from_value::<SortField>(json!(5)).unwrap_err();
        assert_eq!(err.to_string(), "Invalid sort field: 5");
    }

    #[test]
    fn key_map_match_tests_object_keys() {
        use serde_json::json;
        let d = json!({"m": {"b": 1, "c": 2}, "a": [1]});
        assert!(matches_selector(
            &d,
            &json!({"m": {"$keyMapMatch": {"$eq": "c"}}})
        ));
        assert!(!matches_selector(
            &d,
            &json!({"m": {"$keyMapMatch": {"$eq": "z"}}})
        ));
        assert!(!matches_selector(
            &d,
            &json!({"a": {"$keyMapMatch": {"$eq": "0"}}})
        ));
    }

    #[test]
    fn known_divergence_object_key_order_is_not_significant() {
        // KNOWN DIVERGENCE (deferred): CouchDB 3.5.1 compares objects in
        // document key order, so {"b":1,"a":2} only equals {"b":1,"a":2}:
        // `{"m": {"$eq": {"a": 2, "b": 1}}}` does not match that document
        // (and view keys collate the same way). serde_json without the
        // `preserve_order` feature sorts object keys, so RouchDB cannot see
        // the order; enabling it would change every serde_json map in the
        // dependency graph and the revision hashes. This test pins today's
        // behavior so a change to it is deliberate.
        let d: Value = serde_json::from_str(r#"{"m": {"b": 1, "a": 2}}"#).unwrap();
        let sel: Value = serde_json::from_str(r#"{"m": {"$eq": {"a": 2, "b": 1}}}"#).unwrap();
        assert!(matches_selector(&d, &sel));
    }

    #[test]
    fn empty_sort_field_is_an_error_not_a_panic() {
        // F45: {} (or several keys, or a bad direction) is an invalid sort.
        for bad in [
            serde_json::json!({}),
            serde_json::json!({"a": "asc", "b": "desc"}),
            serde_json::json!({"a": "up"}),
            serde_json::json!(5),
        ] {
            assert!(serde_json::from_value::<SortField>(bad).is_err());
        }
        let ok: SortField = serde_json::from_value(serde_json::json!({"a": "desc"})).unwrap();
        assert_eq!(ok.field_and_direction(), ("a", SortDirection::Desc));

        // A hand-built empty map must not panic either.
        let empty = SortField::WithDirection(HashMap::new());
        let _ = empty.field_and_direction();
        assert!(empty.try_field_and_direction().is_err());
        let opts = FindOptions {
            selector: serde_json::json!({}),
            sort: Some(vec![empty]),
            ..Default::default()
        };
        assert!(find_in_docs(vec![serde_json::json!({"_id": "a"})], &opts).is_err());
    }

    #[test]
    fn find_skips_design_documents() {
        // F46: _find never returns design documents.
        let docs = vec![
            serde_json::json!({"_id": "_design/app", "views": {}}),
            serde_json::json!({"_id": "a", "x": 1}),
        ];
        let opts = FindOptions {
            selector: serde_json::json!({}),
            ..Default::default()
        };
        let res = find_in_docs(docs.clone(), &opts).unwrap();
        assert_eq!(res.docs.len(), 1);
        assert_eq!(res.docs[0]["_id"], "a");
        let opts = FindOptions {
            selector: serde_json::json!({"x": {"$exists": false}}),
            ..Default::default()
        };
        assert!(find_in_docs(docs, &opts).unwrap().docs.is_empty());
    }

    #[test]
    fn projection_supports_nested_paths_without_forcing_id() {
        // F103: fields are extracted by path and only requested fields return.
        let d = serde_json::json!({
            "_id": "d1",
            "address": {"city": "NYC", "zip": "1"},
            "tags": ["rust", "db"],
            "age": 3
        });
        let p = project(
            d,
            &[
                "address.city".to_string(),
                "nope".to_string(),
                "tags.0".to_string(),
            ],
        );
        assert_eq!(
            p,
            serde_json::json!({"address": {"city": "NYC"}, "tags": {"0": "rust"}})
        );
    }

    // --- Index candidates ---

    fn age_index() -> IndexDefinition {
        IndexDefinition {
            name: "by-age".into(),
            fields: vec![SortField::Simple("age".into())],
            ddoc: None,
        }
    }

    /// An adapter holding `a`..`e` with ages 1..5, `none` without an age
    /// and a design document.
    async fn aged_adapter() -> rouchdb_adapter_memory::MemoryAdapter {
        use rouchdb_core::document::{BulkDocsOptions, Document};
        let db = rouchdb_adapter_memory::MemoryAdapter::new("index");
        let mut docs = vec![
            serde_json::json!({"_id": "none", "name": "x"}),
            serde_json::json!({"_id": "_design/app", "age": 3}),
        ];
        for (id, age) in [("c", 3), ("a", 1), ("e", 5), ("b", 2), ("d", 4)] {
            docs.push(serde_json::json!({"_id": id, "age": age}));
        }
        let docs = docs
            .into_iter()
            .map(|d| Document::from_json(d).unwrap())
            .collect();
        db.bulk_docs(docs, BulkDocsOptions::new()).await.unwrap();
        db
    }

    #[tokio::test]
    async fn index_candidates_are_narrowed_by_the_first_field() {
        // Only $eq, $gt, $gte, $lt and $lte on the first field narrow the
        // candidates (in index order: key, then id). A missing field is
        // indexed as null and design documents are not indexed.
        let db = aged_adapter().await;
        let index = build_index(&db, &age_index()).await.unwrap();
        let cases = [
            (serde_json::json!({}), vec!["none", "a", "b", "c", "d", "e"]),
            (serde_json::json!({"age": 3}), vec!["c"]),
            (serde_json::json!({"age": {"$eq": 3}}), vec!["c"]),
            (serde_json::json!({"age": {"$gt": 3}}), vec!["d", "e"]),
            (serde_json::json!({"age": {"$gte": 3}}), vec!["c", "d", "e"]),
            (
                serde_json::json!({"age": {"$lt": 3}}),
                vec!["none", "a", "b"],
            ),
            (
                serde_json::json!({"age": {"$lte": 3}}),
                vec!["none", "a", "b", "c"],
            ),
            (
                serde_json::json!({"age": {"$gt": 1, "$lte": 4}}),
                vec!["b", "c", "d"],
            ),
            (serde_json::json!({"age": {"$gt": 4, "$lt": 2}}), vec![]),
            (
                serde_json::json!({"age": {"$ne": 3}}),
                vec!["none", "a", "b", "c", "d", "e"],
            ),
            (
                serde_json::json!({"name": "x"}),
                vec!["none", "a", "b", "c", "d", "e"],
            ),
        ];
        for (selector, expected) in cases {
            assert_eq!(index.find_matching(&selector), expected, "{selector}");
        }
    }

    #[tokio::test]
    async fn apply_changes_drops_deleted_and_design_documents() {
        use rouchdb_core::document::{ChangeRev, Seq};
        let db = aged_adapter().await;
        let mut index = build_index(&db, &age_index()).await.unwrap();
        let change =
            |seq: u64, id: &str, deleted: bool, doc: Option<serde_json::Value>| ChangeEvent {
                seq: Seq::Num(seq),
                id: id.into(),
                changes: vec![ChangeRev {
                    rev: format!("{seq}-x"),
                }],
                deleted,
                doc,
                conflicts: None,
            };
        index.apply_changes(&[
            // Deleted: leaves the index even though its tombstone has a body.
            change(
                10,
                "c",
                true,
                Some(serde_json::json!({"_id": "c", "_deleted": true})),
            ),
            change(
                11,
                "_design/other",
                false,
                Some(serde_json::json!({"age": 3})),
            ),
            // Updated twice in one batch: only the last version counts.
            change(
                12,
                "a",
                false,
                Some(serde_json::json!({"_id": "a", "age": 7})),
            ),
            change(
                13,
                "a",
                false,
                Some(serde_json::json!({"_id": "a", "age": 6})),
            ),
            change(
                14,
                "f",
                false,
                Some(serde_json::json!({"_id": "f", "age": 3})),
            ),
        ]);
        assert_eq!(
            index.find_matching(&serde_json::json!({})),
            ["none", "b", "f", "d", "e", "a"]
        );
        assert_eq!(index.find_matching(&serde_json::json!({"age": 3})), ["f"]);
        // An empty batch changes nothing.
        index.apply_changes(&[]);
        assert_eq!(index.find_matching(&serde_json::json!({"age": 6})), ["a"]);
    }
}
