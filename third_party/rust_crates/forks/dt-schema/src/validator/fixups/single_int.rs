// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.


use std::collections::HashMap;

use serde_json::json;

use crate::{path::JsonPath, validator::util::IsSchema};

use super::Fixup;

pub struct SingleIntFixup {
    map: serde_json::Map<String, serde_json::Value>,
}

/// Convert a single int value into an array.
impl Fixup for SingleIntFixup {
    fn new(
        _propname: &str,
        value: &serde_json::Value,
        _path: JsonPath,
    ) -> Result<Option<Self>, super::FixupError> {
        match value.as_object() {
            Some(o) => Ok(Some(SingleIntFixup { map: o.clone() })),
            None => Ok(None),
        }
    }

    fn fixup(mut self) -> Result<serde_json::Value, super::FixupError> {
        if self.map.is_int_schema() {
            let mut values = HashMap::new();
            for key in ["const", "enum", "minimum", "maximum"] {
                if let Some(value) = self.map.remove(key) {
                    values.insert(key.to_owned(), value);
                }
            }
            self.map
                .insert("items".to_owned(), json!([{ "items": [values] }]));
        }

        Ok(self.map.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_int_becomes_many() {
        let single_int = json!({
            "const": 4
        });

        let result = SingleIntFixup::new("", &single_int, JsonPath::new())
            .expect("Valid schema")
            .unwrap()
            .fixup()
            .expect("Fixup succeeds");
        assert_eq!(result, json!({"items": [{"items": [{"const": 4}]}]}));
    }

    #[test]
    fn test_int_list_ignored() {
        let int_list = json!({
            "minItems": 2,
            "maxItems": 3,
        });

        let result = SingleIntFixup::new("", &int_list, JsonPath::new())
            .expect("Valid schema")
            .unwrap()
            .fixup()
            .expect("Fixup succeeds");
        assert_eq!(result, int_list);
    }
}
