// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.


use serde_json::json;

use crate::path::JsonPath;

use super::{Fixup, FixupError};

pub struct ItemsSizeFixup {
    value: serde_json::Value,
    path: JsonPath,
}

impl ItemsSizeFixup {
    fn do_one_fixup(item: &mut serde_json::Value, path: JsonPath) -> Result<(), FixupError> {
        match item {
            serde_json::Value::Array(array) => {
                for (i, item) in array.iter_mut().enumerate() {
                    Self::do_one_fixup(item, path.extend_index_only(i))?;
                }
            }
            serde_json::Value::Object(obj) => {
                obj.remove("description");
                if let Some(items) = obj.get("items") {
                    if let Some(array) = items.as_array() {
                        let length = json!(array.len());
                        obj.entry("minItems").or_insert(length.clone());
                        obj.entry("maxItems").or_insert(length);
                    }
                    obj.insert("type".to_owned(), json!("array"));

                    Self::do_one_fixup(obj.get_mut("items").unwrap(), path.extend("items"))?;
                } else {
                    match (obj.get("minItems"), obj.get("maxItems")) {
                        (Some(min), None) => obj.insert("maxItems".to_owned(), min.clone()),
                        (None, Some(max)) => obj.insert("minItems".to_owned(), max.clone()),
                        _ => None,
                    };
                }
            }
            _ => {}
        }

        Ok(())
    }
}

impl Fixup for ItemsSizeFixup {
    fn new(
        _propname: &str,
        value: &serde_json::Value,
        path: JsonPath,
    ) -> Result<Option<Self>, super::FixupError> {
        match value {
            serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
                Ok(Some(ItemsSizeFixup {
                    value: value.clone(),
                    path,
                }))
            }
            _ => Ok(None),
        }
    }

    fn fixup(mut self) -> Result<serde_json::Value, super::FixupError> {
        Self::do_one_fixup(&mut self.value, self.path)?;
        Ok(self.value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_implicit_size() {
        let array = json!({
            "items": [{"const": "a"}, {"const": "b"}]
        });
        let result = ItemsSizeFixup::new("", &array, JsonPath::new())
            .unwrap()
            .unwrap()
            .fixup()
            .expect("fixup ok");

        assert_eq!(
            result,
            json!({
                "minItems": 2,
                "maxItems": 2,
                "type": "array",
                "items": [{"const": "a"}, {"const": "b"}]
            })
        )
    }

    #[test]
    fn test_implicit_max_size() {
        let array = json!({
            "items": [{"const": "a"}, {"const": "b"}],
            "minItems": 1
        });
        let result = ItemsSizeFixup::new("", &array, JsonPath::new())
            .unwrap()
            .unwrap()
            .fixup()
            .expect("fixup ok");

        assert_eq!(
            result,
            json!({
                "minItems": 1,
                "maxItems": 2,
                "type": "array",
                "items": [{"const": "a"}, {"const": "b"}]
            })
        )
    }

    #[test]
    fn test_no_items() {
        let array = json!({
            "minItems": 1
        });
        let result = ItemsSizeFixup::new("", &array, JsonPath::new())
            .unwrap()
            .unwrap()
            .fixup()
            .expect("fixup ok");

        assert_eq!(
            result,
            json!({
                "minItems": 1,
                "maxItems": 1,
            })
        )
    }
}
