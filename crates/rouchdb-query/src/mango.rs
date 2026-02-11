//! Mango query engine — CouchDB-compatible selector-based document queries.
//!
//! Supports the standard Mango operators: `$eq`, `$ne`, `$gt`, `$gte`, `$lt`,
//! `$lte`, `$in`, `$nin`, `$exists`, `$regex`, `$elemMatch`, `$all`, `$size`,
//! `$or`, `$and`, `$not`, `$nor`, `$mod`, `$type`.

use std::collections::HashMap;

use regex::Regex;
use serde::{Deserialize, Serialize};

use rouchdb_core::adapter::Adapter;
use rouchdb_core::collation::collate;
use rouchdb_core::document::AllDocsOptions;
use rouchdb_core::error::Result;

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

/// A built in-memory index: sorted entries of (composite_key, doc_id).
#[derive(Debug, Clone)]
pub struct BuiltIndex {
    pub def: IndexDefinition,
    pub entries: Vec<(Vec<serde_json::Value>, String)>,
}

impl BuiltIndex {
    /// Find doc IDs matching a simple equality/range selector on the indexed fields.
    pub fn find_matching(&self, selector: &serde_json::Value) -> Vec<String> {
        if self.def.fields.is_empty() {
            return Vec::new();
        }

        // Extract the first indexed field and its conditions
        let (first_field, _) = self.def.fields[0].field_and_direction();

        if let Some(conditions) = selector.get(first_field) {
            match conditions {
                serde_json::Value::Object(ops) => {
                    // Range query: use binary search
                    self.entries
                        .iter()
                        .filter(|(key, _)| {
                            if key.is_empty() {
                                return false;
                            }
                            let val = &key[0];
                            for (op, operand) in ops {
                                let matches = match op.as_str() {
                                    "$eq" => collate(val, operand) == std::cmp::Ordering::Equal,
                                    "$gt" => collate(val, operand) == std::cmp::Ordering::Greater,
                                    "$gte" => collate(val, operand) != std::cmp::Ordering::Less,
                                    "$lt" => collate(val, operand) == std::cmp::Ordering::Less,
                                    "$lte" => collate(val, operand) != std::cmp::Ordering::Greater,
                                    _ => true, // Unknown op, don't filter
                                };
                                if !matches {
                                    return false;
                                }
                            }
                            true
                        })
                        .map(|(_, id)| id.clone())
                        .collect()
                }
                // Implicit $eq
                other => self
                    .entries
                    .iter()
                    .filter(|(key, _)| {
                        !key.is_empty() && collate(&key[0], other) == std::cmp::Ordering::Equal
                    })
                    .map(|(_, id)| id.clone())
                    .collect(),
            }
        } else {
            // Selector doesn't use the indexed field, can't use index
            self.entries.iter().map(|(_, id)| id.clone()).collect()
        }
    }
}

/// Build an index from all documents in an adapter.
pub async fn build_index(adapter: &dyn Adapter, def: &IndexDefinition) -> Result<BuiltIndex> {
    let all = adapter
        .all_docs(AllDocsOptions {
            include_docs: true,
            ..AllDocsOptions::new()
        })
        .await?;

    let mut entries: Vec<(Vec<serde_json::Value>, String)> = Vec::new();

    for row in &all.rows {
        if let Some(ref doc_json) = row.doc {
            let key: Vec<serde_json::Value> = def
                .fields
                .iter()
                .map(|sf| {
                    let (field, _) = sf.field_and_direction();
                    get_nested_field(doc_json, field)
                        .cloned()
                        .unwrap_or(serde_json::Value::Null)
                })
                .collect();
            entries.push((key, row.id.clone()));
        }
    }

    // Sort by composite key
    entries.sort_by(|(a, _), (b, _)| {
        for (va, vb) in a.iter().zip(b.iter()) {
            let cmp = collate(va, vb);
            if cmp != std::cmp::Ordering::Equal {
                return cmp;
            }
        }
        std::cmp::Ordering::Equal
    });

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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SortField {
    /// Simple field name (ascending).
    Simple(String),
    /// Field with direction: `{"field": "asc"}` or `{"field": "desc"}`.
    WithDirection(HashMap<String, String>),
}

impl SortField {
    pub fn field_and_direction(&self) -> (&str, SortDirection) {
        match self {
            SortField::Simple(f) => (f.as_str(), SortDirection::Asc),
            SortField::WithDirection(map) => {
                let (field, dir) = map.iter().next().unwrap();
                let direction = if dir == "desc" {
                    SortDirection::Desc
                } else {
                    SortDirection::Asc
                };
                (field.as_str(), direction)
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
pub async fn find(adapter: &dyn Adapter, opts: FindOptions) -> Result<FindResponse> {
    // Fetch all documents
    let all = adapter
        .all_docs(AllDocsOptions {
            include_docs: true,
            ..AllDocsOptions::new()
        })
        .await?;

    let mut matched: Vec<serde_json::Value> = Vec::new();

    for row in &all.rows {
        if let Some(ref doc_json) = row.doc
            && matches_selector(doc_json, &opts.selector)
        {
            matched.push(doc_json.clone());
        }
    }

    // Sort
    if let Some(ref sort_fields) = opts.sort {
        matched.sort_by(|a, b| {
            for sf in sort_fields {
                let (field, direction) = sf.field_and_direction();
                let va = get_nested_field(a, field);
                let vb = get_nested_field(b, field);
                let va = va.unwrap_or(&serde_json::Value::Null);
                let vb = vb.unwrap_or(&serde_json::Value::Null);
                let cmp = collate(va, vb);
                let cmp = if direction == SortDirection::Desc {
                    cmp.reverse()
                } else {
                    cmp
                };
                if cmp != std::cmp::Ordering::Equal {
                    return cmp;
                }
            }
            std::cmp::Ordering::Equal
        });
    }

    // Skip
    if let Some(skip) = opts.skip {
        matched = matched.into_iter().skip(skip as usize).collect();
    }

    // Limit
    if let Some(limit) = opts.limit {
        matched.truncate(limit as usize);
    }

    // Field projection
    if let Some(ref fields) = opts.fields {
        matched = matched
            .into_iter()
            .map(|doc| project(doc, fields))
            .collect();
    }

    Ok(FindResponse { docs: matched })
}

/// Check if a document matches a Mango selector.
pub fn matches_selector(doc: &serde_json::Value, selector: &serde_json::Value) -> bool {
    match selector {
        serde_json::Value::Object(map) => {
            for (key, condition) in map {
                if !match_condition(doc, key, condition) {
                    return false;
                }
            }
            true
        }
        _ => false,
    }
}

fn match_condition(doc: &serde_json::Value, key: &str, condition: &serde_json::Value) -> bool {
    // Check for logical operators
    match key {
        "$and" => return match_and(doc, condition),
        "$or" => return match_or(doc, condition),
        "$not" => return match_not(doc, condition),
        "$nor" => return match_nor(doc, condition),
        _ => {}
    }

    let field_value = get_nested_field(doc, key);

    match condition {
        // Shorthand: {"field": value} means {"field": {"$eq": value}}
        serde_json::Value::Object(ops) => {
            for (op, operand) in ops {
                if !match_operator(field_value, op, operand) {
                    return false;
                }
            }
            true
        }
        // Implicit $eq
        other => match_operator(field_value, "$eq", other),
    }
}

fn match_operator(
    field_value: Option<&serde_json::Value>,
    op: &str,
    operand: &serde_json::Value,
) -> bool {
    match op {
        "$eq" => field_value.is_some_and(|v| collate(v, operand) == std::cmp::Ordering::Equal),
        "$ne" => field_value.is_none_or(|v| collate(v, operand) != std::cmp::Ordering::Equal),
        "$gt" => field_value.is_some_and(|v| collate(v, operand) == std::cmp::Ordering::Greater),
        "$gte" => field_value.is_some_and(|v| collate(v, operand) != std::cmp::Ordering::Less),
        "$lt" => field_value.is_some_and(|v| collate(v, operand) == std::cmp::Ordering::Less),
        "$lte" => field_value.is_some_and(|v| collate(v, operand) != std::cmp::Ordering::Greater),
        "$in" => {
            if let Some(arr) = operand.as_array() {
                field_value.is_some_and(|v| {
                    arr.iter()
                        .any(|item| collate(v, item) == std::cmp::Ordering::Equal)
                })
            } else {
                false
            }
        }
        "$nin" => {
            if let Some(arr) = operand.as_array() {
                field_value.is_none_or(|v| {
                    !arr.iter()
                        .any(|item| collate(v, item) == std::cmp::Ordering::Equal)
                })
            } else {
                true
            }
        }
        "$exists" => {
            let should_exist = operand.as_bool().unwrap_or(true);
            if should_exist {
                field_value.is_some()
            } else {
                field_value.is_none()
            }
        }
        "$type" => {
            if let Some(type_name) = operand.as_str() {
                field_value.is_some_and(|v| json_type_name(v) == type_name)
            } else {
                false
            }
        }
        "$regex" => {
            if let Some(pattern) = operand.as_str() {
                field_value.is_some_and(|v| {
                    if let Some(s) = v.as_str() {
                        Regex::new(pattern).is_ok_and(|re| re.is_match(s))
                    } else {
                        false
                    }
                })
            } else {
                false
            }
        }
        "$size" => {
            if let Some(expected_size) = operand.as_u64() {
                field_value.is_some_and(|v| {
                    v.as_array()
                        .is_some_and(|arr| arr.len() as u64 == expected_size)
                })
            } else {
                false
            }
        }
        "$all" => {
            if let Some(required) = operand.as_array() {
                field_value.is_some_and(|v| {
                    if let Some(arr) = v.as_array() {
                        required.iter().all(|req| {
                            arr.iter()
                                .any(|item| collate(item, req) == std::cmp::Ordering::Equal)
                        })
                    } else {
                        false
                    }
                })
            } else {
                false
            }
        }
        "$elemMatch" => field_value.is_some_and(|v| {
            if let Some(arr) = v.as_array() {
                arr.iter().any(|elem| matches_selector(elem, operand))
            } else {
                false
            }
        }),
        "$not" => {
            // Field-level $not: negate the sub-condition applied to this field's value
            if let Some(ops) = operand.as_object() {
                for (sub_op, sub_operand) in ops {
                    if match_operator(field_value, sub_op, sub_operand) {
                        return false;
                    }
                }
                true
            } else {
                // Implicit $eq negation
                !match_operator(field_value, "$eq", operand)
            }
        }
        "$mod" => {
            if let Some(arr) = operand.as_array() {
                if arr.len() == 2 {
                    let divisor = arr[0].as_i64();
                    let remainder = arr[1].as_i64();
                    if let (Some(d), Some(r)) = (divisor, remainder) {
                        field_value
                            .is_some_and(|v| v.as_i64().is_some_and(|n| d != 0 && n % d == r))
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            }
        }
        _ => false,
    }
}

fn match_and(doc: &serde_json::Value, condition: &serde_json::Value) -> bool {
    if let Some(arr) = condition.as_array() {
        arr.iter().all(|sub| matches_selector(doc, sub))
    } else {
        false
    }
}

fn match_or(doc: &serde_json::Value, condition: &serde_json::Value) -> bool {
    if let Some(arr) = condition.as_array() {
        arr.iter().any(|sub| matches_selector(doc, sub))
    } else {
        false
    }
}

fn match_not(doc: &serde_json::Value, condition: &serde_json::Value) -> bool {
    !matches_selector(doc, condition)
}

fn match_nor(doc: &serde_json::Value, condition: &serde_json::Value) -> bool {
    if let Some(arr) = condition.as_array() {
        !arr.iter().any(|sub| matches_selector(doc, sub))
    } else {
        false
    }
}

/// Get a nested field from a JSON value using dot notation.
fn get_nested_field<'a>(doc: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut current = doc;
    for part in path.split('.') {
        match current.get(part) {
            Some(v) => current = v,
            None => return None,
        }
    }
    Some(current)
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
fn project(doc: serde_json::Value, fields: &[String]) -> serde_json::Value {
    let mut result = serde_json::Map::new();

    if let serde_json::Value::Object(map) = &doc {
        for field in fields {
            // Always include _id and _rev
            if let Some(val) = map.get(field) {
                result.insert(field.clone(), val.clone());
            }
        }
        // Always include _id
        if let Some(id) = map.get("_id") {
            result
                .entry("_id".to_string())
                .or_insert_with(|| id.clone());
        }
    }

    serde_json::Value::Object(result)
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
        let projected = project(d, &["name".to_string()]);

        assert_eq!(projected["_id"], "doc1");
        assert_eq!(projected["name"], "Alice");
        assert!(projected.get("age").is_none());
    }

    // --- Missing fields ---

    #[test]
    fn missing_field_ne_matches() {
        // $ne on missing field should match (field != value is true when field doesn't exist)
        let d = doc(serde_json::json!({"name": "Alice"}));
        assert!(matches_selector(
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
}
