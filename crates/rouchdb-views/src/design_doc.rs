use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use rouchdb_core::error::{Result, RouchError};

/// A CouchDB design document.
///
/// Design documents store view definitions, filter functions, validation
/// functions and other application logic. The members CouchDB gives a
/// meaning to are typed fields; everything else is kept verbatim, so any
/// design document CouchDB accepts reads and writes back unchanged:
///
/// - `views` holds the JavaScript `{map, reduce}` views, and `other_views`
///   the other members of the `views` object: the `lib` CommonJS library
///   of the map functions and the views of a Mango (`language: "query"`)
///   index design document, whose `map` is an object;
/// - [`ViewDef::extra`] keeps the other members of a view (`options`, ...);
/// - [`DesignDocument::extra`] keeps every other member: `options`,
///   `autoupdate`, `rewrites`, `_attachments`, custom fields, ...
///
/// Filter, show, list and update functions are JSON values because CouchDB
/// accepts a string or an object for each of them.
///
/// An empty `views`, `filters`, `shows`, `lists` or `updates` object is not
/// written (CouchDB treats it like an absent one).
///
/// Build one with [`DesignDocument::new`] and the `with_*` methods, or with
/// a struct literal ending in `..Default::default()`: fields may be added in
/// minor releases, and a literal that lists every field would then stop
/// compiling. The same goes for [`ViewDef`].
///
/// ```
/// use rouchdb_views::{DesignDocument, ViewDef};
///
/// let ddoc = DesignDocument::new("app")
///     .with_view(
///         "by_type",
///         ViewDef::new("function(doc) { emit(doc.type, 1); }").with_reduce("_count"),
///     )
///     .with_filter("users", "function(doc) { return doc.type === 'user'; }");
/// assert_eq!(ddoc.id, "_design/app");
/// assert_eq!(ddoc.to_json()["views"]["by_type"]["reduce"], "_count");
/// ```
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(from = "RawDesignDocument", into = "RawDesignDocument")]
pub struct DesignDocument {
    /// The document id, `_design/{name}`.
    pub id: String,
    /// The revision to update (`None` to create the document).
    pub rev: Option<String>,
    /// The JavaScript views (`map` is a string), by name.
    pub views: HashMap<String, ViewDef>,
    /// Members of `views` that are not JavaScript view definitions, kept
    /// verbatim: `lib` and Mango index views. When a name is both here and
    /// in `views`, the entry of `views` is written.
    pub other_views: Map<String, Value>,
    pub filters: HashMap<String, Value>,
    pub validate_doc_update: Option<String>,
    pub shows: HashMap<String, Value>,
    pub lists: HashMap<String, Value>,
    pub updates: HashMap<String, Value>,
    pub language: Option<String>,
    /// Every other member of the design document, kept verbatim.
    pub extra: Map<String, Value>,
}

/// A JavaScript view definition: a map function, an optional reduce
/// function (or built-in reduce such as `_count`) and any other member of
/// the view (such as `options`), kept verbatim.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ViewDef {
    pub map: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reduce: Option<String>,
    /// Every other member of the view definition.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ViewDef {
    /// A view with this map function and no reduce.
    pub fn new(map: impl Into<String>) -> Self {
        Self {
            map: map.into(),
            ..Self::default()
        }
    }

    /// The same view with a reduce function (or `_count`, `_sum`, ...).
    pub fn with_reduce(mut self, reduce: impl Into<String>) -> Self {
        self.reduce = Some(reduce.into());
        self
    }
}

/// The JSON shape of a design document, typed only where CouchDB requires
/// a type (a design document CouchDB accepts always deserializes).
#[derive(Serialize, Deserialize)]
struct RawDesignDocument {
    #[serde(rename = "_id")]
    id: String,
    #[serde(rename = "_rev", default, skip_serializing_if = "Option::is_none")]
    rev: Option<String>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    views: Map<String, Value>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    filters: HashMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    validate_doc_update: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    shows: HashMap<String, Value>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    lists: HashMap<String, Value>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    updates: HashMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

impl From<RawDesignDocument> for DesignDocument {
    fn from(raw: RawDesignDocument) -> Self {
        let mut views = HashMap::new();
        let mut other_views = Map::new();
        for (name, view) in raw.views {
            match javascript_view(&name, &view) {
                Some(def) => {
                    views.insert(name, def);
                }
                None => {
                    other_views.insert(name, view);
                }
            }
        }
        DesignDocument {
            id: raw.id,
            rev: raw.rev,
            views,
            other_views,
            filters: raw.filters,
            validate_doc_update: raw.validate_doc_update,
            shows: raw.shows,
            lists: raw.lists,
            updates: raw.updates,
            language: raw.language,
            extra: raw.extra,
        }
    }
}

impl From<DesignDocument> for RawDesignDocument {
    fn from(ddoc: DesignDocument) -> Self {
        let mut views = ddoc.other_views;
        for (name, def) in ddoc.views {
            views.insert(name, serde_json::to_value(def).unwrap_or_default());
        }
        RawDesignDocument {
            id: ddoc.id,
            rev: ddoc.rev,
            views,
            filters: ddoc.filters,
            validate_doc_update: ddoc.validate_doc_update,
            shows: ddoc.shows,
            lists: ddoc.lists,
            updates: ddoc.updates,
            language: ddoc.language,
            extra: ddoc.extra,
        }
    }
}

/// A member of `views` as a JavaScript view definition: not `lib`, with a
/// string `map` and a string `reduce` (if any).
fn javascript_view(name: &str, view: &Value) -> Option<ViewDef> {
    if name == "lib" || !view.get("map").is_some_and(Value::is_string) {
        return None;
    }
    serde_json::from_value(view.clone()).ok()
}

impl DesignDocument {
    /// An empty design document named `name` (with or without the
    /// `_design/` prefix).
    pub fn new(name: &str) -> Self {
        let id = if name.starts_with("_design/") {
            name.to_string()
        } else {
            format!("_design/{}", name)
        };
        Self {
            id,
            ..Self::default()
        }
    }

    /// The same design document with a JavaScript view.
    pub fn with_view(mut self, name: impl Into<String>, view: ViewDef) -> Self {
        self.views.insert(name.into(), view);
        self
    }

    /// The same design document with a filter function.
    pub fn with_filter(mut self, name: impl Into<String>, function: impl Into<String>) -> Self {
        self.filters
            .insert(name.into(), Value::String(function.into()));
        self
    }

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
    use serde_json::json;

    /// Design documents as CouchDB 3.5.1 returns them (`GET` after they
    /// were written through Fauxton, `PUT`, a standalone attachment `PUT`
    /// and `POST /_index`).
    fn couchdb_ddocs() -> Vec<Value> {
        vec![
            json!({"_id": "_design/fauxton", "_rev": "1-8d361a23b4cb8e213f0868ea3d2742c2",
                "views": {"new-view": {"map": "function (doc) {\n  emit(doc._id, 1);\n}"}},
                "language": "javascript"}),
            json!({"_id": "_design/app", "_rev": "2-63135377fa40c43b9e9cdc6bb23080b1",
                "language": "javascript",
                "views": {
                    "lib": {"util": "exports.twice = function(x){ return 2*x; };"},
                    "by_type": {"map": "function (doc) {\n  emit(doc.type, 1);\n}",
                        "reduce": "_count", "options": {"collation": "raw"}},
                    "by_n": {"map": "function(doc){ var u = require(\"views/lib/util\"); emit(u.twice(doc.n), null); }"}
                },
                "filters": {"users": "function(doc, req){ return doc.type === \"user\"; }",
                    "erl": {"src": "x"}},
                "shows": {"s": "function(doc, req){ return doc.name; }"},
                "lists": {"l": "function(head, req){ }"},
                "updates": {"u": "function(doc, req){ return [doc, \"ok\"]; }"},
                "validate_doc_update": "function(newDoc, oldDoc, userCtx){ }",
                "options": {"partitioned": false, "local_seq": true},
                "autoupdate": false,
                "rewrites": [{"from": "/a", "to": "/b"}],
                "custom": {"nested": [1, 2.5, "x", null, true]},
                "_attachments": {"index.html": {"content_type": "text/html", "revpos": 2,
                    "digest": "md5-HjUxfFUJjLDa3k0z8ODAdA==", "length": 11, "stub": true}}}),
            json!({"_id": "_design/mango", "_rev": "2-27b4638f6dd9c5cc57c867f02880e061",
            "language": "query",
            "views": {
                "by-t": {"map": {"fields": {"t": "asc"}, "partial_filter_selector": {}},
                    "reduce": "_count", "options": {"def": {"fields": ["t"]}}},
                "by-name-age": {"map": {"fields": {"name": "asc", "age": "asc"},
                    "partial_filter_selector": {"type": {"$eq": "user"}}},
                    "reduce": "_count",
                    "options": {"def": {"fields": ["name", "age"],
                        "partial_filter_selector": {"type": "user"}}}}
            }}),
            json!({"_id": "_design/part", "_rev": "1-a7086d2ff1ffbeaca1ed7218783a580f",
                "language": "query",
                "views": {"px": {"map": {"fields": {"x": "asc"}, "partial_filter_selector": {}},
                    "reduce": "_count", "options": {"def": {"fields": ["x"]}}}},
                "options": {"partitioned": false}}),
        ]
    }

    #[test]
    fn couchdb_design_documents_round_trip_exactly() {
        for json in couchdb_ddocs() {
            let ddoc = DesignDocument::from_json(json.clone()).unwrap();
            assert_eq!(ddoc.to_json(), json);
            let reparsed: DesignDocument =
                serde_json::from_str(&serde_json::to_string(&ddoc).unwrap()).unwrap();
            assert_eq!(reparsed, ddoc);
        }
    }

    #[test]
    fn members_land_in_their_typed_fields() {
        let ddocs = couchdb_ddocs();
        let app = DesignDocument::from_json(ddocs[1].clone()).unwrap();
        assert_eq!(app.name(), "app");
        let mut js: Vec<&str> = app.views.keys().map(String::as_str).collect();
        js.sort();
        assert_eq!(js, ["by_n", "by_type"]);
        assert_eq!(app.views["by_type"].reduce.as_deref(), Some("_count"));
        assert_eq!(
            app.views["by_type"].extra,
            *json!({"options": {"collation": "raw"}})
                .as_object()
                .unwrap()
        );
        assert_eq!(app.other_views.keys().collect::<Vec<_>>(), ["lib"]);
        assert_eq!(app.filters["erl"], json!({"src": "x"}));
        assert!(app.validate_doc_update.is_some());
        let mut extra: Vec<&str> = app.extra.keys().map(String::as_str).collect();
        extra.sort();
        assert_eq!(
            extra,
            [
                "_attachments",
                "autoupdate",
                "custom",
                "options",
                "rewrites"
            ]
        );

        let mango = DesignDocument::from_json(ddocs[2].clone()).unwrap();
        assert!(mango.views.is_empty());
        assert_eq!(mango.other_views.len(), 2);
        assert_eq!(mango.language.as_deref(), Some("query"));
    }

    #[test]
    fn lib_is_never_a_view() {
        // A CommonJS module may be called `map`: `lib` still is no view.
        let json = json!({"_id": "_design/l", "views": {
            "lib": {"map": "exports.m = 1;", "reduce": "exports.r = 2;"},
            "v": {"map": "function(doc) { emit(require('views/lib/map').m); }"}
        }});
        let ddoc = DesignDocument::from_json(json.clone()).unwrap();
        assert_eq!(ddoc.views.keys().collect::<Vec<_>>(), ["v"]);
        assert_eq!(ddoc.other_views["lib"], json["views"]["lib"]);
        assert_eq!(ddoc.to_json(), json);
    }

    #[test]
    fn a_view_of_views_wins_over_an_other_view_of_the_same_name() {
        // Redefining a Mango index view as a JavaScript view.
        let mut ddoc = DesignDocument::from_json(couchdb_ddocs()[3].clone()).unwrap();
        ddoc.views
            .insert("px".into(), ViewDef::new("function(doc) {}"));
        assert_eq!(
            ddoc.to_json()["views"],
            json!({"px": {"map": "function(doc) {}"}})
        );
    }

    #[test]
    fn construction_and_empty_members() {
        let ddoc = DesignDocument::new("app");
        assert_eq!(ddoc, DesignDocument::new("_design/app"));
        assert_eq!(ddoc.to_json(), json!({"_id": "_design/app"}));
        let json = DesignDocument::new("app")
            .with_view("v", ViewDef::new("m").with_reduce("_sum"))
            .with_filter("f", "fn")
            .to_json();
        assert_eq!(
            json,
            json!({"_id": "_design/app", "views": {"v": {"map": "m", "reduce": "_sum"}},
                "filters": {"f": "fn"}})
        );
        // Empty objects are not written back.
        let parsed =
            DesignDocument::from_json(json!({"_id": "_design/e", "filters": {}, "views": {}}))
                .unwrap();
        assert_eq!(parsed.to_json(), json!({"_id": "_design/e"}));
    }

    #[test]
    fn invalid_design_documents_are_rejected() {
        for json in [
            json!({"views": {}}),
            json!({"_id": "_design/x", "views": []}),
            json!({"_id": "_design/x", "validate_doc_update": 1}),
            json!({"_id": "_design/x", "language": 1}),
        ] {
            assert!(
                matches!(
                    DesignDocument::from_json(json.clone()),
                    Err(RouchError::BadRequest(_))
                ),
                "{json}"
            );
        }
    }
}
