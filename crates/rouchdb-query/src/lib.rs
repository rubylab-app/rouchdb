/// Query engine for RouchDB — Mango selectors and map/reduce views.
///
/// Provides two query mechanisms:
///
/// 1. **Mango queries** — CouchDB-compatible selector-based document matching
///    with field projection, sorting, and pagination.
///
/// 2. **Map/reduce views** — Temporary (ad-hoc) views using Rust closures
///    with built-in reduce functions (sum, count, stats) and custom reducers.
pub mod mango;
pub mod mapreduce;

pub use mango::{
    BuiltIndex, CreateIndexResponse, ExplainIndex, ExplainResponse, FindOptions, FindResponse,
    IndexDefinition, IndexFields, IndexInfo, SortDirection, SortField, build_index, find,
    matches_selector,
};
pub use mapreduce::{
    EmittedRow, ReduceFn, StaleOption, ViewQueryOptions, ViewResult, ViewRow, query_view,
};
