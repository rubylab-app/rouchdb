/// CouchDB collation order.
///
/// CouchDB (and PouchDB) use an Erlang-derived ordering for keys:
///
/// ```text
/// null < boolean < number < string < array < object
/// ```
///
/// This module provides comparison and encoding functions that match this
/// ordering, ensuring consistent behavior across local storage and remote
/// CouchDB instances.
use serde_json::Value;
use std::cmp::Ordering;

// ---------------------------------------------------------------------------
// Type ranking
// ---------------------------------------------------------------------------

/// Numeric type rank matching CouchDB collation.
fn type_rank(v: &Value) -> u8 {
    match v {
        Value::Null => 1,
        Value::Bool(_) => 2,
        Value::Number(_) => 3,
        Value::String(_) => 4,
        Value::Array(_) => 5,
        Value::Object(_) => 6,
    }
}

// ---------------------------------------------------------------------------
// Comparison
// ---------------------------------------------------------------------------

/// Compare two JSON values using CouchDB collation order.
///
/// Strings compare by UTF-16 code units, like PouchDB (JavaScript). CouchDB
/// itself uses ICU collation for strings (e.g. `"apple" < "Banana"`), which
/// is not implemented here.
///
/// Objects compare key by key in *sorted* key order: `serde_json` (without
/// its `preserve_order` feature) does not keep the document's key order,
/// which CouchDB uses (`{"b":1} < {"b":2,"a":1}` in CouchDB, the other way
/// round here). This is an accepted difference, see the book's
/// "Differences from CouchDB" page.
pub fn collate(a: &Value, b: &Value) -> Ordering {
    let rank_a = type_rank(a);
    let rank_b = type_rank(b);

    if rank_a != rank_b {
        return rank_a.cmp(&rank_b);
    }

    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        (Value::Number(a), Value::Number(b)) => compare_numbers(a, b),
        (Value::String(a), Value::String(b)) => compare_strings(a, b),
        (Value::Array(a), Value::Array(b)) => {
            // Element-by-element, shorter arrays sort first
            for (ea, eb) in a.iter().zip(b.iter()) {
                match collate(ea, eb) {
                    Ordering::Equal => continue,
                    other => return other,
                }
            }
            a.len().cmp(&b.len())
        }
        (Value::Object(a), Value::Object(b)) => {
            // Key-by-key comparison in map order; fewer keys sort first.
            // CouchDB uses the document's key order, but serde_json (without
            // `preserve_order`) keeps keys sorted, so that is the order here.
            for ((ka, va), (kb, vb)) in a.iter().zip(b.iter()) {
                match compare_strings(ka, kb) {
                    Ordering::Equal => {}
                    other => return other,
                }
                match collate(va, vb) {
                    Ordering::Equal => continue,
                    other => return other,
                }
            }
            a.len().cmp(&b.len())
        }
        _ => Ordering::Equal, // Should be unreachable due to rank check
    }
}

/// Compare two strings by UTF-16 code units (JavaScript string order).
///
/// UTF-8 byte order is code point order, which only differs from UTF-16
/// order when a supplementary character (a surrogate pair in UTF-16) meets
/// a character in U+E000..=U+FFFF, so only the first differing characters
/// need to be looked at.
fn compare_strings(a: &str, b: &str) -> Ordering {
    let common = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
    // The shared prefix is identical in both, so a char boundary in `a` is
    // one in `b` too.
    let mut start = common;
    while !a.is_char_boundary(start) {
        start -= 1;
    }
    match (a[start..].chars().next(), b[start..].chars().next()) {
        (Some(x), Some(y)) => utf16_units(x).cmp(&utf16_units(y)),
        (x, y) => x.is_some().cmp(&y.is_some()),
    }
}

/// The UTF-16 code units of a character, as a comparable pair.
fn utf16_units(c: char) -> (u16, u16) {
    let mut buf = [0u16; 2];
    match c.encode_utf16(&mut buf) {
        [unit] => (*unit, 0),
        [high, low] => (*high, *low),
        _ => unreachable!("a char is one or two UTF-16 units"),
    }
}

/// Compare two JSON numbers exactly.
///
/// `serde_json::Number` can hold `u64`, `i64`, or `f64`. Converting straight
/// to `f64` (as a naive implementation does) collapses integers larger than
/// 2^53 onto the same float, making distinct values compare `Equal` and
/// breaking Mango `$eq`/range queries. Integers are compared via `i128`, an
/// integer and a float are compared exactly, and `-0.0` equals `0`.
fn compare_numbers(a: &serde_json::Number, b: &serde_json::Number) -> Ordering {
    match (as_i128(a), as_i128(b)) {
        (Some(ia), Some(ib)) => ia.cmp(&ib),
        (Some(ia), None) => compare_int_float(ia, b.as_f64().unwrap_or(0.0)),
        (None, Some(ib)) => compare_int_float(ib, a.as_f64().unwrap_or(0.0)).reverse(),
        (None, None) => {
            let fa = a.as_f64().unwrap_or(0.0);
            let fb = b.as_f64().unwrap_or(0.0);
            // JSON numbers are finite, so only -0.0 vs 0.0 needs care, and
            // partial_cmp treats them as equal.
            fa.partial_cmp(&fb).unwrap_or(Ordering::Equal)
        }
    }
}

fn as_i128(n: &serde_json::Number) -> Option<i128> {
    if let Some(i) = n.as_i64() {
        Some(i as i128)
    } else {
        n.as_u64().map(|u| u as i128)
    }
}

/// Compare an integer with a finite float without rounding either.
fn compare_int_float(i: i128, f: f64) -> Ordering {
    let whole = f.trunc();
    // Saturates beyond the i128 range, where the integer part alone decides.
    match i.cmp(&(whole as i128)) {
        Ordering::Equal => whole.partial_cmp(&f).unwrap_or(Ordering::Equal),
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Indexable string encoding
// ---------------------------------------------------------------------------

/// Encode a JSON value into a string whose byte order is the CouchDB
/// collation order of the values (`a.cmp(&b)` on the encodings equals
/// `collate(a, b)`), usable as a key in a sorted store.
///
/// Every encoded value starts with its type rank and ends with a `\0`
/// terminator, so an array or object that is a prefix of another sorts
/// first:
/// - Null:    `"1"`
/// - Bool:    `"2F"` / `"2T"`
/// - Number:  `"3"` + encoded number (exact, see below)
/// - String:  `"4"` + UTF-16 code units remapped so that byte order is
///   UTF-16 order, with `\0`, `\1`, `\2` escaped as in pouchdb-collate
/// - Array:   `"5"` + encoded elements
/// - Object:  `"6"` + encoded keys and values
pub fn to_indexable_string(v: &Value) -> String {
    let mut s = String::new();
    encode_value(v, &mut s);
    s
}

fn encode_value(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push('1'),
        Value::Bool(b) => {
            out.push('2');
            out.push(if *b { 'T' } else { 'F' });
        }
        Value::Number(n) => {
            out.push('3');
            encode_number(n, out);
        }
        Value::String(s) => {
            out.push('4');
            encode_string(s, out);
        }
        Value::Array(arr) => {
            out.push('5');
            for elem in arr {
                encode_value(elem, out);
            }
        }
        Value::Object(obj) => {
            out.push('6');
            for (key, value) in obj {
                out.push('4');
                encode_string(key, out);
                out.push('\0');
                encode_value(value, out);
            }
        }
    }
    out.push('\0');
}

/// Encode a string so that byte order is UTF-16 code unit order.
///
/// Units below the surrogates map to themselves; surrogates and
/// U+E000..=U+FFFF are moved above U+FFFF (in that order), so they keep
/// their relative UTF-16 order. `\0`, `\1` and `\2` are escaped to keep
/// `\0` free for the terminator.
fn encode_string(s: &str, out: &mut String) {
    for unit in s.encode_utf16() {
        let code = match unit {
            0 => {
                out.push_str("\u{1}\u{1}");
                continue;
            }
            1 => {
                out.push_str("\u{1}\u{2}");
                continue;
            }
            2 => {
                out.push_str("\u{2}\u{2}");
                continue;
            }
            0xD800..=0xDFFF => 0x1_0000 + u32::from(unit - 0xD800),
            0xE000..=0xFFFF => 0x1_0800 + u32::from(unit - 0xE000),
            _ => u32::from(unit),
        };
        out.push(char::from_u32(code).expect("remapped unit is a valid char"));
    }
}

/// Encode a number exactly, so that lexicographic order is numeric order:
/// - Zero (including `-0.0`): `1`
/// - Positive: `2` + (exponent + 500, 3 digits) + significant digits
/// - Negative: `0` + (500 - exponent, 3 digits) + nines' complement of the
///   significant digits + `:` (which sorts after every digit)
///
/// Integers (and integral floats) use their exact decimal digits; other
/// floats use the shortest representation that round-trips, which orders
/// like the float itself.
fn encode_number(n: &serde_json::Number, out: &mut String) {
    let (negative, digits) = if let Some(i) = as_i128(n) {
        (i < 0, i.unsigned_abs().to_string())
    } else {
        let f = n.as_f64().unwrap_or(0.0);
        let digits = if f.fract() == 0.0 {
            format!("{:.0}", f.abs())
        } else {
            format!("{:e}", f.abs())
        };
        (f < 0.0, digits)
    };

    // Split into significant digits and a decimal exponent (d.ddd × 10^exp).
    let (mantissa, exp) = match digits.split_once('e') {
        Some((m, e)) => (m.replace('.', ""), e.parse::<i32>().unwrap_or(0)),
        None => (digits.clone(), digits.len() as i32 - 1),
    };
    let significant = mantissa.trim_end_matches('0');
    if significant.is_empty() {
        out.push('1');
        return;
    }

    if negative {
        out.push('0');
        out.push_str(&format!("{:03}", 500 - exp));
        out.extend(significant.bytes().map(|d| char::from(b'9' - (d - b'0'))));
        out.push(':');
    } else {
        out.push('2');
        out.push_str(&format!("{:03}", 500 + exp));
        out.push_str(significant);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn type_ordering() {
        let values = vec![
            json!(null),
            json!(false),
            json!(true),
            json!(-1),
            json!(0),
            json!(1),
            json!(1.5),
            json!(""),
            json!("a"),
            json!("b"),
            json!([]),
            json!([1]),
            json!({}),
            json!({"a": 1}),
        ];

        for i in 0..values.len() {
            for j in (i + 1)..values.len() {
                assert_eq!(
                    collate(&values[i], &values[j]),
                    Ordering::Less,
                    "{:?} should be less than {:?}",
                    values[i],
                    values[j]
                );
            }
        }
    }

    #[test]
    fn null_equality() {
        assert_eq!(collate(&json!(null), &json!(null)), Ordering::Equal);
    }

    #[test]
    fn bool_ordering() {
        assert_eq!(collate(&json!(false), &json!(true)), Ordering::Less);
        assert_eq!(collate(&json!(true), &json!(true)), Ordering::Equal);
    }

    #[test]
    fn number_ordering() {
        assert_eq!(collate(&json!(-100), &json!(-1)), Ordering::Less);
        assert_eq!(collate(&json!(0), &json!(1)), Ordering::Less);
        assert_eq!(collate(&json!(1), &json!(2)), Ordering::Less);
        assert_eq!(collate(&json!(1.5), &json!(2)), Ordering::Less);
    }

    #[test]
    fn large_integer_precision() {
        // Distinct integers beyond f64's 2^53 exact range must not collapse
        // to Equal (regression: naive as_f64 comparison loses precision).
        let a = json!(9_007_199_254_740_992_i64); // 2^53
        let b = json!(9_007_199_254_740_993_i64); // 2^53 + 1
        assert_eq!(collate(&a, &b), Ordering::Less);
        assert_eq!(collate(&b, &a), Ordering::Greater);
        assert_eq!(collate(&a, &a), Ordering::Equal);

        // u64 above i64::MAX vs a smaller value.
        let big = json!(u64::MAX);
        let small = json!(1_i64);
        assert_eq!(collate(&small, &big), Ordering::Less);

        // Mixed integer / float still orders correctly.
        assert_eq!(collate(&json!(2_i64), &json!(1.5)), Ordering::Greater);
    }

    #[test]
    fn string_ordering() {
        assert_eq!(collate(&json!("a"), &json!("b")), Ordering::Less);
        assert_eq!(collate(&json!("aa"), &json!("b")), Ordering::Less);
    }

    #[test]
    fn array_ordering() {
        assert_eq!(collate(&json!([]), &json!([1])), Ordering::Less);
        assert_eq!(collate(&json!([1]), &json!([2])), Ordering::Less);
        assert_eq!(collate(&json!([1]), &json!([1, 2])), Ordering::Less);
    }

    #[test]
    fn accepted_divergence_objects_collate_in_sorted_key_order() {
        // CouchDB 3.5.1 sorts the view keys {"b":2,"a":1} and {"b":1} as
        // [{"b":1}, {"b":2,"a":1}] (document key order). serde_json keeps
        // keys sorted, so here {"a":1,"b":2} < {"b":1}. Accepted difference
        // (book: "Differences from CouchDB"); a change must be deliberate.
        let a: Value = serde_json::from_str(r#"{"b":2,"a":1}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"b":1}"#).unwrap();
        assert_eq!(collate(&a, &b), Ordering::Less);
        assert_eq!(a.as_object().unwrap().keys().next().unwrap(), "a");
    }

    #[test]
    fn object_ordering() {
        assert_eq!(collate(&json!({}), &json!({"a": 1})), Ordering::Less);
        assert_eq!(collate(&json!({"a": 1}), &json!({"a": 2})), Ordering::Less);
        assert_eq!(collate(&json!({"a": 1}), &json!({"b": 1})), Ordering::Less);
    }

    #[test]
    fn indexable_string_preserves_order() {
        let values = vec![
            json!(null),
            json!(false),
            json!(true),
            json!(0),
            json!(1),
            json!(100),
            json!("a"),
            json!("b"),
            json!([]),
            json!({}),
        ];

        let encoded: Vec<String> = values.iter().map(to_indexable_string).collect();

        for i in 0..encoded.len() {
            for j in (i + 1)..encoded.len() {
                assert!(
                    encoded[i] < encoded[j],
                    "encoded({:?}) = {:?} should be < encoded({:?}) = {:?}",
                    values[i],
                    encoded[i],
                    values[j],
                    encoded[j]
                );
            }
        }
    }

    #[test]
    fn indexable_string_negative_numbers() {
        let small = to_indexable_string(&json!(-100));
        let big = to_indexable_string(&json!(-1));
        let zero = to_indexable_string(&json!(0));
        assert!(small < big, "-100 should sort before -1");
        assert!(big < zero, "-1 should sort before 0");
    }

    #[test]
    fn negative_zero_equals_zero() {
        // F79: -0.0 and 0 are the same number.
        assert_eq!(collate(&json!(-0.0), &json!(0)), Ordering::Equal);
        assert_eq!(collate(&json!(-0.0), &json!(0.0)), Ordering::Equal);
        assert_eq!(collate(&json!(-0.0), &json!(-1)), Ordering::Greater);
    }

    #[test]
    fn integer_float_comparison_is_exact() {
        // F79: 2^53 + 1 is greater than the float 2^53.
        let int = json!(9_007_199_254_740_993_i64);
        let float = json!(9_007_199_254_740_992.0);
        assert_eq!(collate(&int, &float), Ordering::Greater);
        assert_eq!(collate(&float, &int), Ordering::Less);
        assert_eq!(collate(&json!(3), &json!(3.0)), Ordering::Equal);
        assert_eq!(collate(&json!(3), &json!(2.5)), Ordering::Greater);
        assert_eq!(collate(&json!(-3), &json!(-2.5)), Ordering::Less);
        assert_eq!(collate(&json!(u64::MAX), &json!(1e300)), Ordering::Less);
        assert_eq!(collate(&json!(i64::MIN), &json!(-1e300)), Ordering::Greater);
    }

    #[test]
    fn strings_compare_by_utf16_code_units() {
        // F78: like PouchDB (JavaScript), a supplementary character (a
        // surrogate pair) sorts before U+E000..U+FFFF.
        assert_eq!(
            collate(&json!("\u{1F600}"), &json!("\u{FF21}")),
            Ordering::Less
        );
        assert_eq!(
            collate(&json!("a\u{1F600}"), &json!("a\u{E000}")),
            Ordering::Less
        );
        assert_eq!(
            collate(&json!("\u{1F600}"), &json!("\u{1F601}")),
            Ordering::Less
        );
        assert_eq!(collate(&json!("z"), &json!("\u{E9}")), Ordering::Less);
        assert_eq!(collate(&json!("ab"), &json!("a")), Ordering::Greater);
    }

    #[test]
    fn indexable_string_matches_collate() {
        // F81: the encoding must order (and equate) values exactly like
        // `collate`.
        let values = vec![
            json!(null),
            json!(false),
            json!(true),
            json!(-1e300),
            json!(i64::MIN),
            json!(-1.9),
            json!(-1.5),
            json!(-1),
            json!(-1e-300),
            json!(-0.0),
            json!(0),
            json!(0.0),
            json!(1e-300),
            json!(1),
            json!(1.0),
            json!(1.00000000001),
            json!(9.5),
            json!(9.99999999999),
            json!(10),
            json!(100),
            json!(1_152_921_504_606_846_976_u64),
            json!(1_152_921_504_606_846_976.0),
            json!(9_007_199_254_740_992_u64),
            json!(9_007_199_254_740_992.0),
            json!(9_007_199_254_740_993_u64),
            json!(u64::MAX),
            json!(1e300),
            json!(""),
            json!("\u{0}"),
            json!("\u{1}"),
            json!("\u{2}"),
            json!("B"),
            json!("a"),
            json!("a\u{0}b"),
            json!("ab"),
            json!("b"),
            json!("\u{E9}"),
            json!("\u{D7FF}"),
            json!("\u{10000}"),
            json!("\u{1F600}"),
            json!("\u{1F601}"),
            json!("\u{10FFFF}"),
            json!("\u{E000}"),
            json!("\u{FF21}"),
            json!("\u{FFFF}"),
            json!([]),
            json!([null]),
            json!([""]),
            json!(["", 1]),
            json!(["\u{0}"]),
            json!(["\u{1}"]),
            json!(["\u{2}"]),
            json!(["\u{2}", 1]),
            json!([1]),
            json!([[1], 2]),
            json!([[1, 2]]),
            json!([1, 2]),
            json!(["a"]),
            json!({}),
            json!({"": null}),
            json!({"a": null}),
            json!({"a": 1}),
            json!({"a": 1, "b": 2}),
            json!({"a": 2}),
            json!({"a ": 1}),
            json!({"a!": 1}),
            json!({"a\u{0}": 1}),
            json!({"ab": 1}),
            json!({"b": 1}),
            json!({"\u{E000}": 1}),
            json!({"\u{1F600}": 1}),
        ];
        for a in &values {
            for b in &values {
                assert_eq!(
                    to_indexable_string(a).cmp(&to_indexable_string(b)),
                    collate(a, b),
                    "{a} vs {b}"
                );
            }
        }
    }

    #[test]
    fn indexable_string_format() {
        // The format documented on `to_indexable_string`: type rank, value,
        // `\0` terminator; \0, \1 and \2 escaped as in pouchdb-collate.
        for (value, encoded) in [
            (json!(null), "1\0"),
            (json!(false), "2F\0"),
            (json!(true), "2T\0"),
            (json!(0), "31\0"),
            (json!(-0.0), "31\0"),
            (json!(1), "325001\0"),
            (json!(1.5), "3250015\0"),
            (json!(-1), "305008:\0"),
            (json!(120), "3250212\0"),
            (
                json!("a\u{0}\u{1}\u{2}\u{3}"),
                "4a\u{1}\u{1}\u{1}\u{2}\u{2}\u{2}\u{3}\0",
            ),
            (json!([]), "5\0"),
            (json!([null, "b"]), "51\u{0}4b\0\0"),
            (json!({"a": 1}), "64a\u{0}325001\0\0"),
        ] {
            assert_eq!(to_indexable_string(&value), encoded, "{value}");
        }
    }

    mod properties {
        use super::*;
        use proptest::prelude::*;

        /// Characters around every boundary of the string encoding: the
        /// escaped \0, \1 and \2, ASCII below and above the type ranks,
        /// Latin-1, both sides of the surrogate range, and supplementary
        /// characters (surrogate pairs in UTF-16).
        const CHARS: &[char] = &[
            '\u{0}',
            '\u{1}',
            '\u{2}',
            '\u{3}',
            ' ',
            '!',
            '1',
            'B',
            'a',
            'b',
            'z',
            '\u{E9}',
            '\u{F1}',
            '\u{7FF}',
            '\u{D7FF}',
            '\u{E000}',
            '\u{FF21}',
            '\u{FFFF}',
            '\u{10000}',
            '\u{1F600}',
            '\u{1F601}',
            '\u{10FFFF}',
        ];

        fn string() -> impl Strategy<Value = String> {
            prop::collection::vec(prop::sample::select(CHARS), 0..4)
                .prop_map(|chars| chars.into_iter().collect())
        }

        /// Object keys: few enough to be shared between objects, with keys
        /// that are prefixes of others.
        fn key() -> impl Strategy<Value = String> {
            prop_oneof![
                prop::sample::select(vec![
                    "",
                    "a",
                    "a ",
                    "a!",
                    "a\u{0}",
                    "ab",
                    "b",
                    "\u{E000}",
                    "\u{1F600}"
                ])
                .prop_map(String::from),
                string(),
            ]
        }

        fn number() -> impl Strategy<Value = Value> {
            prop_oneof![
                (-3i64..=3).prop_map(Value::from),
                any::<i64>().prop_map(Value::from),
                any::<u64>().prop_map(Value::from),
                // Fractions, integral floats and -0.0.
                (
                    -40i32..=40,
                    prop::sample::select(vec![1.0, 2.0, 4.0, 10.0, 3.0])
                )
                    .prop_map(|(n, d)| json!(f64::from(n) / d)),
                prop::sample::select(vec![
                    -0.0,
                    1e-300,
                    -1e-300,
                    1e300,
                    -1e300,
                    9_007_199_254_740_992.0
                ])
                .prop_map(|f| json!(f)),
                any::<f64>()
                    .prop_filter("JSON numbers are finite", |f| f.is_finite())
                    .prop_map(|f| json!(f)),
            ]
        }

        /// Nested JSON values of every type.
        fn value() -> impl Strategy<Value = Value> {
            let leaf = prop_oneof![
                Just(Value::Null),
                any::<bool>().prop_map(Value::Bool),
                number(),
                string().prop_map(Value::String),
            ];
            leaf.prop_recursive(3, 24, 4, |inner| {
                prop_oneof![
                    prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
                    prop::collection::vec((key(), inner), 0..4)
                        .prop_map(|entries| Value::Object(entries.into_iter().collect())),
                ]
            })
        }

        proptest! {
            #![proptest_config(ProptestConfig {
                cases: 300,
                failure_persistence: None,
                // Bound shrinking so a failure is reported in seconds (the
                // default is unbounded, which can take minutes on nested
                // values and makes mutation runs time out).
                max_shrink_iters: 1024,
                ..ProptestConfig::default()
            })]

            /// `collate` is a total order, and ordering by the indexable
            /// encoding is the same order.
            #[test]
            fn collate_is_a_total_order_matched_by_the_encoding(
                values in prop::collection::vec(value(), 1..10)
            ) {
                let encoded: Vec<String> = values.iter().map(to_indexable_string).collect();
                for (i, a) in values.iter().enumerate() {
                    prop_assert_eq!(collate(a, a), Ordering::Equal, "reflexive: {}", a);
                    for (j, b) in values.iter().enumerate() {
                        let ab = collate(a, b);
                        prop_assert_eq!(ab, collate(b, a).reverse(), "antisymmetric: {} {}", a, b);
                        prop_assert_eq!(
                            encoded[i].cmp(&encoded[j]),
                            ab,
                            "encoding of {} ({:?}) vs {} ({:?})",
                            a,
                            encoded[i],
                            b,
                            encoded[j]
                        );
                        for c in &values {
                            if ab != Ordering::Greater && collate(b, c) != Ordering::Greater {
                                prop_assert_ne!(
                                    collate(a, c),
                                    Ordering::Greater,
                                    "transitive: {} <= {} <= {}",
                                    a,
                                    b,
                                    c
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
