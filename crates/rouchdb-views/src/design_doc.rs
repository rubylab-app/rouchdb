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
    #[serde(default)]
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
