// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use std::collections::HashSet;

fn generate_items<'a>(
    value: &'a serde_json::Value,
    key: &'static str,
) -> Box<dyn Iterator<Item = &'a serde_json::Value> + 'a> {
    match value {
        serde_json::Value::Object(o) => Box::new(o.iter().flat_map(move |(k, v)| {
            if k == key {
                Box::new(Some(v).into_iter())
            } else {
                generate_items(v, key)
            }
        })),
        serde_json::Value::Array(a) => Box::new(a.iter().flat_map(move |v| generate_items(v, key))),
        _ => Box::new([].into_iter()),
    }
}

/// Given |property_schema| which is the schema for a "compatible" property, returns a set
/// of all possible compatibles the schema could accept.
pub fn extract_node_compatibles(property_schema: &serde_json::Value) -> HashSet<String> {
    let mut result = HashSet::new();

    result.extend(
        generate_items(property_schema, "enum")
            .filter_map(|v| {
                v.as_array().map(|array| {
                    array
                        .iter()
                        .filter_map(|v| v.as_str().map(|v| v.to_owned()))
                })
            })
            .flatten(),
    );
    result.extend(
        generate_items(property_schema, "const").filter_map(|v| v.as_str().map(|v| v.to_owned())),
    );
    result.extend(
        generate_items(property_schema, "pattern").filter_map(|v| v.as_str().map(|v| v.to_owned())),
    );

    result
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    #[test]
    fn test_generate_items() {
        let value = json!({
            "array": [
                {
                    "hello": "one",
                },
                "two",
                "three",
            ],
            "oneOf": {
                "hello": "there",
                "ignore": "me",
            }
        });

        let result = generate_items(&value, "hello").collect::<Vec<_>>();

        assert_eq!(result, vec!["one", "there"]);
    }

    #[test]
    fn test_extract_compatibles() {
        let value = json!({
            "oneOf": {
            "enum": ["hello,there", "another-one"],
            "const": "value",
            "pattern": "^[oO]+h,regexp?$"
            }
        });

        let mut result = extract_node_compatibles(&value)
            .into_iter()
            .collect::<Vec<_>>();
        result.sort();
        assert_eq!(
            result,
            vec!["^[oO]+h,regexp?$", "another-one", "hello,there", "value"]
        );
    }
}
