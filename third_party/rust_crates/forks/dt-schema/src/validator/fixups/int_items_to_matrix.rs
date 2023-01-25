// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use std::collections::BTreeMap;

use serde_json::json;

use crate::validator::array::Array;

use super::Fixup;

/// Convert an int array with an "items" declaration
/// to a matrix.
pub struct IntItemsToMatrix {
    value: serde_json::Value,
}

impl Fixup for IntItemsToMatrix {
    fn new(
        propname: &str,
        value: &serde_json::Value,
        _path: crate::path::JsonPath,
    ) -> Result<Option<Self>, super::FixupError> {
        let items: Array = serde_json::from_value(value.clone())?;
        if !items.is_int_array_schema(propname) {
            return Ok(None);
        }

        Ok(Some(IntItemsToMatrix {
            value: value.clone(),
        }))
    }

    fn fixup(mut self) -> Result<serde_json::Value, super::FixupError> {
        let final_schema = if let Some(array) = self
            .value
            .as_object_mut()
            .and_then(|e| e.get_mut("allOf"))
            .and_then(|e| e.as_array_mut())
        {
            array
                .iter_mut()
                .filter_map(|v| v.as_object_mut())
                .find(|o| o.contains_key("items"))
        } else {
            self.value.as_object_mut()
        };

        let object = if let Some(object) = final_schema {
            let items: Array = serde_json::from_value(object.clone().into())?;
            if !items.has_items() || items.is_matrix() {
                return Ok(self.value);
            }
            object
        } else {
            return Ok(self.value);
        };

        let item_keys = ["items", "minItems", "maxItems", "uniqueItems", "default"];
        let mut values: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        for key in item_keys {
            if let Some(value) = object.remove(key) {
                values.insert(key.to_owned(), value);
            }
        }

        // Safe because we checked |items.has_items()| above.
        match values.get_mut("items").unwrap() {
            serde_json::Value::Array(_) => {
                object.insert("items".to_owned(), json!([values]));
            }
            serde_json::Value::Object(_) => {
                object.insert("items".to_owned(), json!(values));
            }
            _ => {}
        };

        Ok(self.value)
    }
}

#[cfg(test)]
mod tests {
    use crate::path::JsonPath;

    use super::*;

    const PROP_NAME: &str = "test-percent";

    #[test]
    fn test_make_matrix_simple() {
        let schema = json!({
         "items": {
             "minimum": 2,
             "maximum": 10,
         },
         "maxItems": 2,
         "minItems": 2,
        });

        let result = IntItemsToMatrix::new(PROP_NAME, &schema, JsonPath::new())
            .expect("Valid schema")
            .expect("Fixup applies")
            .fixup()
            .expect("Fixup OK");

        assert_eq!(
            result,
            json!({
                "items": {
                     "items": {
                         "minimum": 2,
                         "maximum": 10,
                     },
                     "maxItems": 2,
                     "minItems": 2,
                },
            })
        )
    }

    #[test]
    fn test_make_matrix_with_allof() {
        let schema = json!({
            "allOf": [
                true,
                {"maxItems": 2, "minItems": 1, "items": [{"const": 4}]},
            ]
        });

        let result = IntItemsToMatrix::new(PROP_NAME, &schema, JsonPath::new())
            .expect("Valid schema")
            .expect("Fixup applies")
            .fixup()
            .expect("Fixup OK");

        assert_eq!(
            result,
            json!({
                "allOf": [
                    true,
                    {
                        "items": [{
                            "maxItems": 2,
                            "minItems": 1,
                            "items": [{"const": 4}]
                        }]
                    }
                ]
            })
        );
    }

    #[test]
    fn test_skips_existing_matrix() {
        let schema = json!({
            "items": {
                "items": {
                    "minItems": 2,
                }
            }
        });

        let result = IntItemsToMatrix::new(PROP_NAME, &schema, JsonPath::new())
            .expect("Valid schema")
            .expect("Fixup applies")
            .fixup()
            .expect("Fixup OK");

        assert_eq!(result, schema);
    }
}
