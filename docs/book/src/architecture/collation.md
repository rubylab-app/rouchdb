# CouchDB Collation

CouchDB defines a specific ordering for JSON values that differs from naive
lexicographic comparison of serialized JSON strings. This ordering is inherited
from Erlang's term comparison and is used everywhere keys are compared: view
indexes, `_all_docs` key ranges, Mango query evaluation, and internal storage
engine key encoding.

RouchDB implements this ordering in `rouchdb-core/src/collation.rs`.

## Type Ordering

Different JSON types sort in this fixed order, from lowest to highest:

```
null  <  boolean  <  number  <  string  <  array  <  object
```

A `null` value is always less than `false`, which is always less than `-1000`,
which is always less than `""`, and so on. The type boundary is absolute --
there is no number large enough to sort after even the empty string.

Internally, each type is assigned a numeric rank:

| JSON Type | Rank |
|-----------|------|
| `null`    | 1    |
| `boolean` | 2    |
| `number`  | 3    |
| `string`  | 4    |
| `array`   | 5    |
| `object`  | 6    |

When two values have different ranks, the comparison is immediate. Same-rank
values are compared with type-specific rules described below.

## Within-Type Comparison Rules

### Null

All nulls are equal.

### Boolean

`false < true`.

### Number

Compared by exact numeric value: integers beyond 2^53 are not rounded, an
integer and a float are compared exactly, and `-0.0` equals `0`.
`-100 < -1 < 0 < 1 < 1.5 < 2`.

### String

Compared by UTF-16 code units, like PouchDB (JavaScript). `"a" < "aa" < "b"`,
and `"B" < "a"`. CouchDB itself uses ICU collation for strings (`"a" < "B"`),
which RouchDB does not implement.

### Array

Element-by-element using recursive `collate`. If all shared elements are equal,
the shorter array sorts first.

```
[]       < [1]
[1]      < [2]
[1]      < [1, 2]
[1, "a"] < [1, "b"]
```

### Object

Compared key-by-key in map order. For each key pair, the key strings are
compared; if equal, the values are compared recursively. If all shared
key-value pairs are equal, the object with fewer keys sorts first. CouchDB uses
the document's key order; `serde_json` keeps keys sorted, so RouchDB compares
them in sorted order.

```
{}           < {"a": 1}
{"a": 1}     < {"a": 2}
{"a": 1}     < {"b": 1}
{"a": 1}     < {"a": 1, "b": 2}
```

## The `collate` Function

```rust
pub fn collate(a: &Value, b: &Value) -> Ordering
```

This is the primary comparison entry point. It first compares type ranks, then
delegates to type-specific comparison. It can be used anywhere you need
CouchDB-compatible ordering of arbitrary JSON values.

### Usage Examples

```rust
use serde_json::json;
use rouchdb_core::collation::collate;
use std::cmp::Ordering;

// Cross-type: null < number
assert_eq!(collate(&json!(null), &json!(42)), Ordering::Less);

// Cross-type: number < string
assert_eq!(collate(&json!(9999), &json!("")), Ordering::Less);

// Same type: numeric comparison
assert_eq!(collate(&json!(-1), &json!(0)), Ordering::Less);

// Same type: array element-by-element
assert_eq!(collate(&json!([1, 2]), &json!([1, 3])), Ordering::Less);
```

## Indexable String Encoding

Storage engines like redb store keys as byte arrays and compare them
lexicographically. To preserve CouchDB collation order in a byte-ordered
key-value store, JSON values must be encoded into strings that sort
lexicographically in the same order as `collate`.

```rust
pub fn to_indexable_string(v: &Value) -> String
```

### Encoding Scheme

Each value is encoded with a type-prefix character that preserves the
cross-type ordering:

| Type    | Prefix | Encoding |
|---------|--------|----------|
| Null    | `1`    | Just the prefix character |
| Boolean | `2`    | `2F` for false, `2T` for true |
| Number  | `3`    | `3` + encoded number (see below) |
| String  | `4`    | `4` + the string, remapped so byte order is UTF-16 order |
| Array   | `5`    | `5` + encoded elements |
| Object  | `6`    | `6` + encoded keys and values |

Every encoded value ends with a `\0` terminator (`\0`, `\1` and `\2` inside
strings are escaped), so an array or object that is a prefix of another sorts
first.

Because the prefix characters are `1` through `6`, the inter-type ordering is
automatically correct: any null-encoded string (`"1..."`) sorts before any
boolean-encoded string (`"2..."`), and so on.

### Number Encoding

Numbers require special treatment because naive string representations do not
sort correctly (`"9" > "10"` lexicographically). A number is written as its
significant decimal digits `d.ddd` and a decimal exponent, exactly (integers
use all their digits; other floats use the shortest representation that
round-trips):

**Zero** (including `-0.0`): `"1"`.

**Positive numbers:** `"2"`, the exponent plus 500 in 3 digits, then the
significant digits.

**Negative numbers:** `"0"`, 500 minus the exponent in 3 digits, then the
nines' complement of the significant digits, then `":"` (which sorts after
every digit), so that numbers closer to zero sort later.

```
-100      ->  "3" + "0" + "498" + "8:"   (sorts first)
-1        ->  "3" + "0" + "500" + "8:"
 0        ->  "3" + "1"
 1        ->  "3" + "2" + "500" + "1"
 1.5      ->  "3" + "2" + "500" + "15"
 100      ->  "3" + "2" + "502" + "1"    (sorts last)
```

(each followed by the `\0` terminator).

### Array and Object Encoding

Arrays encode each element recursively. Because every element ends with the
`\0` terminator and `\0` is the lowest byte value, a shorter array with a
matching prefix always sorts before a longer one (and `[[1], 2]` differs from
`[[1, 2]]`).

Objects encode alternating keys and values in map order.

## Why This Matters

### Views

Map/reduce views emit keys that are stored in sorted order. The storage
engine must compare these keys in CouchDB collation order. By encoding
them with `to_indexable_string`, ordinary byte-level comparison produces the
correct ordering.

### `_all_docs` Key Ranges

The `startkey`/`endkey` parameters on `_all_docs` use CouchDB collation. The
adapter encodes the boundary values and performs byte-range scans.

### Mango Queries

Mango `$gt`, `$gte`, `$lt`, `$lte` operators compare values according to
CouchDB collation. The `rouchdb-query` crate uses `collate` for these
comparisons.

### Replication Correctness

If two replicas sort the same view index differently, they will produce
different results for the same query. CouchDB collation ensures all replicas
agree on ordering.

## Verifying Correctness

The test suite confirms that comparing `to_indexable_string` encodings gives
the same result as `collate` for every pair of a mixed corpus (large
integers, floats, `-0.0`, control and supplementary characters, nested arrays
and objects), and that it preserves ordering across the full type spectrum:

```rust
let values = vec![
    json!(null), json!(false), json!(true),
    json!(0), json!(1), json!(100),
    json!("a"), json!("b"),
    json!([]), json!({}),
];
let encoded: Vec<String> = values.iter().map(to_indexable_string).collect();

for i in 0..encoded.len() {
    for j in (i + 1)..encoded.len() {
        assert!(encoded[i] < encoded[j]);
    }
}
```

The negative-number tests further verify that `-100 < -1 < 0` is preserved
in the encoded representation.
