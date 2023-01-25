// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.


use serde_json::json;

use crate::path::JsonPath;

use super::{Fixup, FixupError};

pub struct EmptyItemsRemovalFixup {
    object: serde_json::Map<String, serde_json::Value>,
    path: JsonPath,
}

impl EmptyItemsRemovalFixup {
    fn fixup_one_object(
        obj: &mut serde_json::Map<String, serde_json::Value>,
        path: JsonPath,
    ) -> Result<(), FixupError> {
        let fixed_item_length = match obj.get_mut("items") {
            Some(serde_json::Value::Array(items)) => {
                let mut has_value = false;
                for (i, item) in items
                    .iter_mut()
                    .enumerate()
                    .filter_map(|(k, v)| v.as_object_mut().map(|v| (k, v)))
                {
                    item.remove("description");
                    Self::fixup_one_object(item, path.extend_array_index("items", i))?;
                    if !item.is_empty() {
                        // We found a value, so we're not going to remove the "items" child from this array.
                        has_value = true;
                        break;
                    }
                }

                if has_value {
                    None
                } else {
                    Some(items.len())
                }
            }
            Some(serde_json::Value::Object(o)) => {
                Self::fixup_one_object(o, path.extend("items"))?;
                None
            }
            None => return Ok(()),
            _ => {
                return Err(FixupError::UnexpectedSchemaError(
                    "items should be array or object".to_owned(),
                    path.extend("items"),
                    obj.clone().into(),
                ))
            }
        };

        if let Some(length) = fixed_item_length {
            obj.entry("type").or_insert(json!("array"));
            obj.entry("maxItems").or_insert(json!(length));
            obj.entry("minItems").or_insert(json!(length));
            obj.remove("items");
        }

        Ok(())
    }
}

impl Fixup for EmptyItemsRemovalFixup {
    fn new(
        _propname: &str,
        value: &serde_json::Value,
        path: JsonPath,
    ) -> Result<Option<Self>, super::FixupError> {
        let map_ref = value.as_object().ok_or(FixupError::UnexpectedSchemaError(
            "EmptyItemsRemovalFixup expects an object".to_owned(),
            path.clone(),
            value.clone(),
        ))?;

        if !map_ref.contains_key("items") {
            Ok(None)
        } else {
            Ok(Some(EmptyItemsRemovalFixup {
                object: map_ref.clone(),
                path,
            }))
        }
    }

    fn fixup(mut self) -> Result<serde_json::Value, super::FixupError> {
        Self::fixup_one_object(&mut self.object, self.path)?;
        Ok(self.object.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_remove_empty_items() {
        let data = json!({
            "items": [{
                "items": [{
                    "description": "wow, some items",
                }]
            },
            {"items": []},
            {}
            ]
        });

        let result = EmptyItemsRemovalFixup::new("", &data, JsonPath::new())
            .expect("schema ok")
            .unwrap()
            .fixup()
            .expect("fixup ok");
        assert_eq!(
            result,
            json!({"items": [
                {"minItems": 1, "maxItems": 1, "type": "array"},
                {"items": []},
                {}
            ]})
        );
    }
}
