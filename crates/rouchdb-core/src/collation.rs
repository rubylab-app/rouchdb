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
pub fn collate(a: &Value, b: &Value) -> Ordering {
    let rank_a = type_rank(a);
    let rank_b = type_rank(b);

    if rank_a != rank_b {
        return rank_a.cmp(&rank_b);
    }

    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        (Value::Number(a), Value::Number(b)) => {
            let fa = a.as_f64().unwrap_or(0.0);
            let fb = b.as_f64().unwrap_or(0.0);
            // total_cmp gives a well-defined ordering for all f64 values
            // including NaN (which shouldn't appear in JSON but be safe)
            fa.total_cmp(&fb)
        }
        (Value::String(a), Value::String(b)) => a.cmp(b),
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
            // Key-by-key comparison; fewer keys sort first.
            // Keys are sorted before comparison.
            let mut keys_a: Vec<&String> = a.keys().collect();
            let mut keys_b: Vec<&String> = b.keys().collect();
            keys_a.sort();
            keys_b.sort();

            for (ka, kb) in keys_a.iter().zip(keys_b.iter()) {
                match ka.cmp(kb) {
                    Ordering::Equal => {}
                    other => return other,
                }
                match collate(&a[*ka], &b[*kb]) {
                    Ordering::Equal => continue,
                    other => return other,
                }
            }
            a.len().cmp(&b.len())
        }
        _ => Ordering::Equal, // Should be unreachable due to rank check
    }
}

// ---------------------------------------------------------------------------
// Indexable string encoding
// ---------------------------------------------------------------------------

/// Encode a JSON value into a string that sorts lexicographically in CouchDB
/// collation order. Used as keys in the storage engine.
///
/// Format:
/// - Null:    `"1"`
/// - Bool:    `"2F"` / `"2T"`
/// - Number:  `"3"` + encoded number
/// - String:  `"4"` + string value
/// - Array:   `"5"` + encoded elements separated by null byte
/// - Object:  `"6"` + encoded key-value pairs
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
            encode_number(n.as_f64().unwrap_or(0.0), out);
        }
        Value::String(s) => {
            out.push('4');
            out.push_str(s);
        }
        Value::Array(arr) => {
            out.push('5');
            for (i, elem) in arr.iter().enumerate() {
                if i > 0 {
                    out.push('\0');
                }
                encode_value(elem, out);
            }
        }
        Value::Object(obj) => {
            out.push('6');
            let mut keys: Vec<&String> = obj.keys().collect();
            keys.sort();
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push('\0');
                }
                out.push_str(key);
                out.push('\0');
                encode_value(&obj[*key], out);
            }
        }
    }
}

/// Encode a number such that the resulting string sorts lexicographically
/// in numeric order.
///
/// Scheme (matching PouchDB's `numToIndexableString`):
/// - Negative numbers: `0` + inverted representation
/// - Zero: `1`
/// - Positive numbers: `2` + magnitude (zero-padded to 3 digits) + mantissa
fn encode_number(n: f64, out: &mut String) {
    if n.is_nan() {
        out.push('0'); // Sort NaN before all real numbers
        return;
    }
    if n == f64::NEG_INFINITY {
        out.push('0');
        out.push_str("00000");
        return;
    }
    if n == f64::INFINITY {
        out.push('2');
        out.push_str("99999");
        return;
    }
    if n == 0.0 {
        out.push('1');
        return;
    }

    let is_negative = n < 0.0;
    let abs_n = n.abs();

    // Represent as: mantissa * 10^exponent
    // where 1 <= mantissa < 10
    let exponent = abs_n.log10().floor() as i64;
    let mantissa = abs_n / 10f64.powi(exponent as i32);

    // We offset the exponent by 10000 so it's always positive and
    // zero-pad to 5 digits for consistent lexicographic ordering.
    if is_negative {
        // For negatives: invert so larger (closer to zero) sorts first
        // Prefix with '0', then (10000 - exponent), then (10 - mantissa)
        out.push('0');
        let inv_exp = 10000 - exponent;
        out.push_str(&format!("{:0>5}", inv_exp));
        let inv_mantissa = 10.0 - mantissa;
        let m_str = format!("{:.10}", inv_mantissa);
        out.push_str(m_str.trim_end_matches('0'));
    } else {
        // For positives: prefix with '2', then (exponent + 10000), then mantissa
        out.push('2');
        let adj_exp = exponent + 10000;
        out.push_str(&format!("{:0>5}", adj_exp));
        let m_str = format!("{:.10}", mantissa);
        out.push_str(m_str.trim_end_matches('0'));
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
}
