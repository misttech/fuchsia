// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use std::collections::HashMap;

use serde_json::json;

use crate::{path::JsonPath, validator::util::IsSchema};

use super::Fixup;

pub struct SingleStringFixup {
    map: serde_json::Map<String, serde_json::Value>,
}

/// Convert a string into an array.
impl Fixup for SingleStringFixup {
    fn new(
        _propname: &str,
        value: &serde_json::Value,
        _path: JsonPath,
    ) -> Result<Option<Self>, super::FixupError> {
        match value.as_object() {
            Some(o) => Ok(Some(SingleStringFixup { map: o.clone() })),
            None => Ok(None),
        }
    }

    fn fixup(mut self) -> Result<serde_json::Value, super::FixupError> {
        if self.map.is_string_schema() {
            let mut values = HashMap::new();
            for key in ["const", "enum", "pattern"] {
                if let Some(value) = self.map.remove(key) {
                    values.insert(key.to_owned(), value);
                }
            }
            self.map.insert("items".to_owned(), json!(values));
        }

        Ok(self.map.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_string_becomes_many() {
        let single_string = json!({
            "const": "a-string"
        });

        let result = SingleStringFixup::new("", &single_string, JsonPath::new())
            .expect("Valid schema")
            .unwrap()
            .fixup()
            .expect("Fixup succeeds");
        assert_eq!(result, json!({"items": {"const": "a-string"}}));
    }

    #[test]
    fn test_string_list_ignored() {
        let string_list = json!({
            "minItems": 2,
            "maxItems": 3,
        });

        let result = SingleStringFixup::new("", &string_list, JsonPath::new())
            .expect("Valid schema")
            .unwrap()
            .fixup()
            .expect("Fixup succeeds");
        assert_eq!(result, string_list);
    }
}
