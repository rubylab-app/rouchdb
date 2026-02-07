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

pub use mango::{find, matches_selector, FindOptions, FindResponse, SortField};
pub use mapreduce::{
    query_view, EmittedRow, ReduceFn, ViewQueryOptions, ViewResult, ViewRow,
};
