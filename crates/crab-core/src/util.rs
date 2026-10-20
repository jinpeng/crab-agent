//! Small shared helpers.

/// Unix epoch milliseconds, best effort.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Serialize with object keys sorted recursively, so logically equal JSON
/// values produce byte-identical strings regardless of map ordering.
pub fn canonical_json(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let inner: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}:{}", serde_json::to_string(k).unwrap(), canonical_json(&m[k])))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        serde_json::Value::Array(a) => {
            let inner: Vec<String> = a.iter().map(canonical_json).collect();
            format!("[{}]", inner.join(","))
        }
        other => serde_json::to_string(other).unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_nested_object_keys() {
        let v = serde_json::json!({"b": {"y": 1, "x": 2}, "a": [true, null]});
        assert_eq!(canonical_json(&v), "{\"a\":[true,null],\"b\":{\"x\":2,\"y\":1}}");
    }
}
