# Querying

RouchDB provides two query mechanisms, both compatible with CouchDB semantics:

1. **Mango queries** -- declarative, JSON-based selectors (like MongoDB's query language).
2. **Map/reduce views** -- programmatic queries using Rust closures with optional aggregation.

## Mango Queries

Mango is the simplest way to find documents. You provide a `selector` (a JSON object describing the match criteria) and the engine scans all documents, returning those that match.

### Basic Find

```rust
use rouchdb::{Database, FindOptions};
use serde_json::json;

let db = Database::memory("mydb");
db.put("alice", json!({"name": "Alice", "age": 30, "city": "NYC"})).await?;
db.put("bob", json!({"name": "Bob", "age": 25, "city": "LA"})).await?;
db.put("carol", json!({"name": "Carol", "age": 35, "city": "NYC"})).await?;

let result = db.find(FindOptions {
    selector: json!({"age": {"$gte": 28}}),
    ..Default::default()
}).await?;

// Returns Alice (30) and Carol (35)
for doc in &result.docs {
    println!("{}", doc["name"]);
}
```

The `FindResponse` contains a single field: `docs`, a `Vec<serde_json::Value>` of matching documents with `_id` and `_rev` included.

### FindOptions

```rust
use rouchdb::{FindOptions, SortField};
use std::collections::HashMap;

let opts = FindOptions {
    selector: json!({"city": "NYC"}),
    fields: Some(vec!["name".into(), "age".into()]),
    sort: Some(vec![
        SortField::Simple("age".into()),
        // Or with explicit direction:
        SortField::WithDirection(HashMap::from([
            ("name".into(), "desc".into())
        ])),
    ]),
    limit: Some(10),
    skip: Some(0),
};
```

- `selector` -- the query (see operators below).
- `fields` -- field projection; only these fields are returned (list `_id` explicitly if you need it). Nested paths such as `address.city` keep their structure.
- `sort` -- sort by one or more fields, ascending (`"asc"`) or descending (`"desc"`). As in CouchDB (which sorts through an index on those fields), documents that lack a sort field are left out of sorted results; unlike CouchDB, no index is required.
- `limit` -- maximum number of results.
- `skip` -- number of results to skip (for pagination).

### Comparison Operators

| Operator | Description | Example |
|----------|-------------|---------|
| `$eq` | Equal (also the implicit default) | `{"age": {"$eq": 30}}` or `{"age": 30}` |
| `$ne` | Not equal | `{"status": {"$ne": "archived"}}` |
| `$gt` | Greater than | `{"age": {"$gt": 20}}` |
| `$gte` | Greater than or equal | `{"age": {"$gte": 21}}` |
| `$lt` | Less than | `{"price": {"$lt": 100}}` |
| `$lte` | Less than or equal | `{"price": {"$lte": 99.99}}` |
| `$in` | Value is in array | `{"color": {"$in": ["red", "blue"]}}` |
| `$nin` | Value is not in array | `{"color": {"$nin": ["green"]}}` |

You can combine multiple operators on the same field to express ranges:

```rust
// Documents where 20 < age < 40
let selector = json!({"age": {"$gt": 20, "$lt": 40}});
```

### Existence and Type Operators

| Operator | Description | Example |
|----------|-------------|---------|
| `$exists` | Field exists (or not) | `{"email": {"$exists": true}}` |
| `$type` | Field is a specific JSON type | `{"age": {"$type": "number"}}` |

Supported type names: `"null"`, `"boolean"`, `"number"`, `"string"`, `"array"`, `"object"`.

### String Operators

| Operator | Description | Example |
|----------|-------------|---------|
| `$regex` | Matches a regular expression | `{"name": {"$regex": "^Ali"}}` |

Patterns use PCRE-like syntax (via `fancy-regex`), including lookaround and
backreferences, as CouchDB does.

### Array Operators

| Operator | Description | Example |
|----------|-------------|---------|
| `$all` | Array contains all listed elements (exact equality: `50.0` is not `50`) | `{"tags": {"$all": ["rust", "db"]}}` |
| `$size` | Array has exactly N elements | `{"tags": {"$size": 3}}` |
| `$elemMatch` | At least one element matches sub-selector | See below |

`$elemMatch` example with an array of objects:

```rust
let selector = json!({
    "scores": {
        "$elemMatch": {
            "subject": "math",
            "grade": {"$gt": 80}
        }
    }
});
```

### Arithmetic Operators

| Operator | Description | Example |
|----------|-------------|---------|
| `$mod` | Modulo: `[divisor, remainder]` | `{"n": {"$mod": [3, 1]}}` |

### Logical Operators

| Operator | Description | Example |
|----------|-------------|---------|
| `$and` | All sub-selectors must match | `{"$and": [{"age": {"$gte": 18}}, {"active": true}]}` |
| `$or` | At least one sub-selector must match | `{"$or": [{"city": "NYC"}, {"city": "LA"}]}` |
| `$not` | Negate a selector (the argument must be an object) | `{"$not": {"status": "archived"}}` |
| `$nor` | None of the sub-selectors match | `{"$nor": [{"status": "banned"}, {"age": {"$lt": 13}}]}` |

An empty `$and`, `$or` or `$nor` is always true, so `{"age": 22, "$or": []}`
is `{"age": 22}`; but, as in CouchDB, a query whose selector is nothing but an
empty combinator (`{"$and": []}`, `{"$nor": []}`, `{"f": {"$or": []}}`)
returns no documents.

Note: multiple fields in the same selector object are an implicit `$and`:

```rust
// Equivalent to $and
let selector = json!({"name": "Alice", "age": {"$gte": 25}});
```

`$not` can also be used at the field level:

```rust
// Field-level negation: age is NOT greater than 30
let selector = json!({"age": {"$not": {"$gt": 30}}});
```

### Nested Fields

Use dot notation to query nested objects:

```rust
let selector = json!({"address.city": "NYC"});
```

## Map/Reduce Views

Map/reduce gives you full programmatic control. You provide a **map function** (a Rust closure) that receives each document and emits key-value pairs. An optional **reduce function** aggregates the emitted values.

### Map-Only Query

```rust
use rouchdb::{query_view, ViewQueryOptions};

let result = query_view(
    db.adapter(),
    &|doc| {
        // Emit the city as the key, 1 as the value
        let city = doc.get("city").cloned().unwrap_or(json!(null));
        vec![(city, json!(1))]
    },
    None, // no reduce
    ViewQueryOptions::new(),
).await?;

for row in &result.rows {
    println!("key={}, id={}", row.key, row.id.as_deref().unwrap_or(""));
}
```

The map closure receives a `&serde_json::Value` (the full document including `_id` and `_rev`) and returns a `Vec<(serde_json::Value, serde_json::Value)>` of emitted key-value pairs. You can emit zero, one, or multiple pairs per document.

Results are sorted by key using CouchDB's collation order.

### Key Filtering

```rust
let result = query_view(
    db.adapter(),
    &|doc| {
        let name = doc.get("name").cloned().unwrap_or(json!(null));
        vec![(name, json!(1))]
    },
    None,
    ViewQueryOptions {
        key: Some(json!("Bob")),       // exact key match
        ..ViewQueryOptions::new()
    },
).await?;
```

`ViewQueryOptions` fields:
- `key` -- return only rows with this exact key. With `start_key` or `end_key` it is the other bound of the range (as when `startkey` follows `key` in a CouchDB query string).
- `start_key` / `end_key` -- define a key range (inclusive by default). A range no row can be in (a start after the end, or before it when descending) is a `BadRequest`, as in CouchDB.
- `inclusive_end` -- whether to include the end key.
- `descending` -- reverse the sort order.
- `skip` / `limit` -- pagination.
- `include_docs` -- embed full documents (a value `{"_id": ...}` embeds that document instead).
- `reduce` -- whether to run the reduce function (on by default with `new()`, as in CouchDB).
- `group` -- group reduced results by key. Grouping (`group`, or a `group_level` above 0) without a reduce is a `BadRequest`.
- `group_level` -- for array keys, group by the first N elements.
- `keys` -- return the rows of these keys, in this order (reversed when `descending`, except for grouped reduce rows, which keep it). With a reduce, more than one key requires `group: true` without `group_level` (CouchDB's rule); a single key behaves like `key`. Several keys cannot be combined with `key`, `start_key` or `end_key`.

### Built-In Reduce Functions

RouchDB provides three built-in reduce functions matching CouchDB:

```rust
use rouchdb::ReduceFn;

// Sum all emitted numeric values
let sum_result = query_view(
    db.adapter(),
    &|doc| {
        let age = doc.get("age").cloned().unwrap_or(json!(0));
        vec![(json!(null), age)]
    },
    Some(&ReduceFn::Sum),
    ViewQueryOptions {
        reduce: true,
        ..ViewQueryOptions::new()
    },
).await?;
// sum_result.rows[0].value == 90 (30 + 25 + 35; integers stay integers)
```

- `ReduceFn::Sum` -- sums numbers (arrays element-wise, objects field by field). Like CouchDB, values it cannot add make the reduced value an error object (`{"error": "builtin_reduce_error", "reason": ..., "caused_by": <value>}`) instead of failing the query.
- `ReduceFn::Count` -- counts the number of rows.
- `ReduceFn::Stats` -- computes `{"sum", "count", "min", "max", "sumsqr"}`.

### Group By

Group reduce results by key:

```rust
let result = query_view(
    db.adapter(),
    &|doc| {
        let city = doc.get("city").cloned().unwrap_or(json!(null));
        vec![(city, json!(1))]
    },
    Some(&ReduceFn::Count),
    ViewQueryOptions {
        reduce: true,
        group: true,
        ..ViewQueryOptions::new()
    },
).await?;

// Returns: [{"key": "LA", "value": 1}, {"key": "NYC", "value": 2}]
```

For compound (array) keys, use `group_level` to control grouping granularity:

```rust
let result = query_view(
    db.adapter(),
    &|doc| {
        let year = doc.get("year").cloned().unwrap_or(json!(null));
        let month = doc.get("month").cloned().unwrap_or(json!(null));
        vec![(json!([year, month]), json!(1))]
    },
    Some(&ReduceFn::Count),
    ViewQueryOptions {
        reduce: true,
        group: true,
        group_level: Some(1), // group by year only
        ..ViewQueryOptions::new()
    },
).await?;
```

### Custom Reduce

For aggregations not covered by the built-ins, use `ReduceFn::Custom`:

```rust
let max_reduce = ReduceFn::Custom(Box::new(|_keys, values, _rereduce| {
    let max = values
        .iter()
        .filter_map(|v| v.as_f64())
        .fold(f64::NEG_INFINITY, f64::max);
    json!(max)
}));

let result = query_view(
    db.adapter(),
    &|doc| {
        let age = doc.get("age").cloned().unwrap_or(json!(0));
        vec![(json!(null), age)]
    },
    Some(&max_reduce),
    ViewQueryOptions {
        reduce: true,
        ..ViewQueryOptions::new()
    },
).await?;
```

The custom function receives `(keys, values, rereduce)`. When `rereduce` is `true`, the function is being called to combine previously-reduced results.

## Mango vs Map/Reduce: When to Use Each

| Use Case | Recommendation |
|----------|---------------|
| Simple field equality/range filters | Mango |
| Field projection (return only some fields) | Mango |
| Aggregation (sums, counts, statistics) | Map/reduce |
| Complex key transformations | Map/reduce |
| Grouping by compound keys | Map/reduce |
| Quick ad-hoc queries | Mango |
| Custom sort by computed value | Map/reduce |

Both approaches scan all documents (no persistent indexes yet), so performance is similar for small to medium databases. Map/reduce is more powerful but requires writing Rust closures, while Mango selectors can be built from JSON configuration at runtime.
