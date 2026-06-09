use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use rouchdb_core::error::{Result, RouchError};

/// A CouchDB design document.
///
/// Design documents store view definitions, filter functions,
/// validation functions, and other application logic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesignDocument {
    #[serde(rename = "_id")]
    pub id: String,
    #[serde(rename = "_rev", skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    #[serde(default, deserialize_with = "deserialize_views")]
    pub views: HashMap<String, ViewDef>,
    #[serde(default)]
    pub filters: HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validate_doc_update: Option<String>,
    #[serde(default)]
    pub shows: HashMap<String, String>,
    #[serde(default)]
    pub lists: HashMap<String, String>,
    #[serde(default)]
    pub updates: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

/// A view definition containing map and optional reduce functions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewDef {
    pub map: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reduce: Option<String>,
}

/// Deserialize a design document's `views` object, tolerating CouchDB's
/// special `lib` (CommonJS shared-library) entry and any other sibling that
/// is not a `{map, reduce}` view definition. Such entries are skipped rather
/// than failing the whole parse.
fn deserialize_views<'de, D>(d: D) -> std::result::Result<HashMap<String, ViewDef>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: HashMap<String, serde_json::Value> = HashMap::deserialize(d)?;
    let mut out = HashMap::new();
    for (k, v) in raw {
        if k == "lib" {
            continue;
        }
        if v.get("map").and_then(|m| m.as_str()).is_some()
            && let Ok(def) = serde_json::from_value::<ViewDef>(v)
        {
            out.insert(k, def);
        }
    }
    Ok(out)
}

impl DesignDocument {
    /// Parse a design document from a JSON value.
    pub fn from_json(value: serde_json::Value) -> Result<Self> {
        serde_json::from_value(value)
            .map_err(|e| RouchError::BadRequest(format!("invalid design doc: {}", e)))
    }

    /// Convert to a JSON value.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_default()
    }

    /// Get the design document name without the `_design/` prefix.
    pub fn name(&self) -> &str {
        self.id.strip_prefix("_design/").unwrap_or(&self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ddoc_with_views_lib() {
        // A CouchDB ddoc may carry a `views.lib` CommonJS shared library; it
        // must not break parsing of the real views.
        let json = serde_json::json!({
            "_id": "_design/x",
            "views": {
                "by_name": {"map": "function(doc){ emit(doc.name, 1); }"},
                "lib": {"shared": "exports.x = 1"}
            }
        });
        let ddoc = DesignDocument::from_json(json).unwrap();
        assert_eq!(ddoc.views.len(), 1);
        assert!(ddoc.views.contains_key("by_name"));
        assert!(!ddoc.views.contains_key("lib"));
    }
}
