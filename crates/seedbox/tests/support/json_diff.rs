//! JSON path differences for comparison diagnostics in CI logs.

/// Every JSON path where `a` and `b` differ, with both values abbreviated.
pub fn json_differences(
    path: &str,
    a: &serde_json::Value,
    b: &serde_json::Value,
    out: &mut Vec<String>,
) {
    use serde_json::Value;
    match (a, b) {
        (Value::Object(left), Value::Object(right)) => {
            let keys: std::collections::BTreeSet<_> = left.keys().chain(right.keys()).collect();
            for key in keys {
                json_differences(
                    &format!("{path}.{key}"),
                    left.get(key).unwrap_or(&Value::Null),
                    right.get(key).unwrap_or(&Value::Null),
                    out,
                );
            }
        }
        (Value::Array(left), Value::Array(right)) if left.len() == right.len() => {
            for (index, (left, right)) in left.iter().zip(right).enumerate() {
                json_differences(&format!("{path}[{index}]"), left, right, out);
            }
        }
        _ if a != b => {
            let abbreviate = |value: &Value| {
                let text = value.to_string();
                match text.char_indices().nth(120) {
                    Some((end, _)) => format!("{}…", &text[..end]),
                    None => text,
                }
            };
            out.push(format!("{path}: {} != {}", abbreviate(a), abbreviate(b)));
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_differences_name_each_differing_path() {
        let a = serde_json::json!({"seed": 1, "inputs": {"boot": "aa", "same": [1, 2]}, "only_a": true});
        let b =
            serde_json::json!({"seed": 1, "inputs": {"boot": "bb", "same": [1, 2]}, "list": [1]});
        let mut differences = Vec::new();
        json_differences("spec", &a, &b, &mut differences);
        assert_eq!(
            differences,
            [
                "spec.inputs.boot: \"aa\" != \"bb\"",
                "spec.list: null != [1]",
                "spec.only_a: true != null",
            ]
        );
    }
}
