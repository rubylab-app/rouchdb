//! Nesting-depth aware JSON handling for document bodies.
//!
//! serde_json refuses to decode input nested more than 127 containers
//! deep, while CouchDB accepts (and serves) far deeper documents. rouchdb
//! stores documents up to [`MAX_NESTING_DEPTH`] levels deep: writes check
//! the limit with [`check_document_depth`], and everything that decodes a
//! stored or remote document uses [`from_slice`], which lifts serde_json's
//! limit for input within the allowed depth.
//!
//! The limit exists because serializing, cloning, comparing and dropping a
//! `serde_json::Value` recurse once per level: 1000 levels stay well within
//! a 2 MiB thread stack (Tokio's default) even in debug builds.

use serde::de::DeserializeOwned;

use crate::error::{Result, RouchError};

/// Deepest document body rouchdb stores: the number of objects and arrays
/// on the deepest path, the top-level object included.
pub const MAX_NESTING_DEPTH: usize = 1000;

/// Stack for the thread that decodes documents deeper than serde_json allows
/// (reserved, not committed, until used).
const DEEP_DECODE_STACK: usize = 64 * 1024 * 1024;

/// Nesting depth of a JSON value: the number of containers on its deepest
/// path (0 for a scalar). Iterative, so it is safe on any input.
pub fn value_depth(value: &serde_json::Value) -> usize {
    use serde_json::Value;
    let mut max = 0;
    let mut stack = vec![(value, 1usize)];
    while let Some((value, depth)) = stack.pop() {
        let children: Box<dyn Iterator<Item = &Value>> = match value {
            Value::Array(items) => Box::new(items.iter()),
            Value::Object(map) => Box::new(map.values()),
            _ => continue,
        };
        max = max.max(depth);
        stack.extend(
            children
                .filter(|c| c.is_array() || c.is_object())
                .map(|c| (c, depth + 1)),
        );
    }
    max
}

/// Nesting depth of JSON text (containers on its deepest path), without
/// parsing it. Iterative; brackets inside strings are ignored.
pub fn text_depth(bytes: &[u8]) -> usize {
    let (mut depth, mut max) = (0usize, 0usize);
    let (mut in_string, mut escaped) = (false, false);
    for &b in bytes {
        if in_string {
            match b {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'[' | b'{' => {
                depth += 1;
                max = max.max(depth);
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    max
}

/// Reject a document body nested deeper than [`MAX_NESTING_DEPTH`].
pub fn check_document_depth(body: &serde_json::Value) -> Result<()> {
    if value_depth(body) > MAX_NESTING_DEPTH {
        return Err(RouchError::BadRequest(format!(
            "Document nesting exceeds the maximum depth of {}",
            MAX_NESTING_DEPTH
        )));
    }
    Ok(())
}

/// Whether serde_json gave up because the input is nested too deeply.
fn is_recursion_limit(e: &serde_json::Error) -> bool {
    e.is_syntax() && e.to_string().starts_with("recursion limit exceeded")
}

/// Decode JSON that may be nested up to `max_depth` containers deep.
///
/// Input within serde_json's limit (the common case) is decoded directly,
/// with no extra pass. Deeper input, up to `max_depth`, is decoded without
/// that limit on a helper thread with a large stack; anything deeper is an
/// error.
pub fn from_slice<T>(bytes: &[u8], max_depth: usize) -> serde_json::Result<T>
where
    T: DeserializeOwned + Send + 'static,
{
    match serde_json::from_slice(bytes) {
        Err(e) if is_recursion_limit(&e) => {}
        shallow => return shallow,
    }
    let depth = text_depth(bytes);
    if depth > max_depth {
        return Err(serde::de::Error::custom(format!(
            "JSON nested {} levels deep exceeds the maximum of {}",
            depth, max_depth
        )));
    }
    let owned = bytes.to_vec();
    let decode = move || -> serde_json::Result<T> {
        let mut de = serde_json::Deserializer::from_slice(&owned);
        de.disable_recursion_limit();
        let value = T::deserialize(&mut de)?;
        de.end()?;
        Ok(value)
    };
    match std::thread::Builder::new()
        .name("rouchdb-json-decode".into())
        .stack_size(DEEP_DECODE_STACK)
        .spawn(decode)
    {
        Ok(handle) => handle
            .join()
            .unwrap_or_else(|_| Err(serde::de::Error::custom("deep JSON decoding panicked"))),
        Err(e) => Err(serde::de::Error::custom(format!(
            "cannot spawn a thread to decode deep JSON: {}",
            e
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nested_text(depth: usize) -> String {
        format!("{}1{}", "[".repeat(depth), "]".repeat(depth))
    }

    #[test]
    fn depths() {
        assert_eq!(value_depth(&serde_json::json!(1)), 0);
        assert_eq!(value_depth(&serde_json::json!([])), 1);
        assert_eq!(value_depth(&serde_json::json!({"a": [1, {"b": []}]})), 4);
        assert_eq!(value_depth(&serde_json::json!([[1], [[2]], 3])), 3);
        assert_eq!(text_depth(b"1"), 0);
        assert_eq!(text_depth(br#"{"a": [1, {"b": []}]}"#), 4);
        assert_eq!(text_depth(br#"["[[[{", "\"]]", {}]"#), 2);
        assert_eq!(text_depth(br#"[[1]] [["#), 2);
    }

    #[test]
    fn decodes_up_to_the_limit() {
        // serde_json alone decodes 127 levels.
        for depth in [1, 127, 128, 300, MAX_NESTING_DEPTH] {
            let text = nested_text(depth);
            let value: serde_json::Value = from_slice(text.as_bytes(), MAX_NESTING_DEPTH).unwrap();
            assert_eq!(value_depth(&value), depth);
            assert_eq!(serde_json::to_string(&value).unwrap(), text);
        }
        let too_deep = nested_text(MAX_NESTING_DEPTH + 1);
        let err = from_slice::<serde_json::Value>(too_deep.as_bytes(), MAX_NESTING_DEPTH)
            .unwrap_err()
            .to_string();
        assert!(err.contains("exceeds the maximum of 1000"), "{err}");
        // Malformed deep input is still an error (trailing data included).
        let bad = format!("{} x", nested_text(200));
        assert!(from_slice::<serde_json::Value>(bad.as_bytes(), 1000).is_err());
        assert!(from_slice::<serde_json::Value>(b"[1] x", 1000).is_err());
    }

    #[test]
    fn serde_json_depth_limit_is_detected() {
        let at_limit = nested_text(127);
        assert!(serde_json::from_str::<serde_json::Value>(&at_limit).is_ok());
        let err = serde_json::from_str::<serde_json::Value>(&nested_text(128)).unwrap_err();
        assert!(is_recursion_limit(&err), "{err}");
        let err = serde_json::from_str::<serde_json::Value>("[1,]").unwrap_err();
        assert!(!is_recursion_limit(&err), "{err}");
    }

    #[test]
    fn documents_are_checked_against_the_limit() {
        let doc = |depth: usize| {
            let text = format!(r#"{{"v": {}}}"#, nested_text(depth - 1));
            from_slice::<serde_json::Value>(text.as_bytes(), usize::MAX).unwrap()
        };
        assert!(check_document_depth(&doc(MAX_NESTING_DEPTH)).is_ok());
        assert!(matches!(
            check_document_depth(&doc(MAX_NESTING_DEPTH + 1)),
            Err(RouchError::BadRequest(_))
        ));
    }
}
