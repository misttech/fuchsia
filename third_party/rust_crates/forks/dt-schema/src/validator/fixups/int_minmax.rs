// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use serde_json::json;

use crate::{
    path::JsonPath,
    validator::{array::Array, util::ContainsExt},
};

use super::{items_size::ItemsSizeFixup, Fixup, FixupError};

/// Convert an int array defined by "minItems" and "maxItems" to a matrix.
pub struct IntMinMaxToMatrixFixup {
    object: serde_json::Value,
    path: JsonPath,
}

impl IntMinMaxToMatrixFixup {
    // Actually perform the fixup. Split like this to make the borrow-checker happy.
    fn do_fixup_on_object(
        map: &mut serde_json::Map<String, serde_json::Value>,
        path: JsonPath,
    ) -> Result<(), FixupError> {
        if map.get_mut("items").and_then(|v| v.as_array()).is_some() {
            return Ok(());
        }

        let items: Array = serde_json::from_value(map.clone().into())?;
        if items.is_matrix() {
            return Ok(());
        }

        if map.get("maxItems").and_then(|v| v.as_u64()) == Some(1) {
            return Ok(());
        }

        let mut tmp_schema = serde_json::Map::<String, serde_json::Value>::new();
        let min_items = if let Some(min) = map.remove("minItems") {
            tmp_schema.insert("minItems".to_owned(), min.clone());
            min.as_u64().unwrap_or(0)
        } else {
            0
        };
        if let Some(max) = map.remove("maxItems") {
            tmp_schema.insert("maxItems".to_owned(), max);
        }

        if !tmp_schema.is_empty() {
            let mut vec = vec![json!({"items": [tmp_schema.clone()]})];

            tmp_schema.insert("items".to_owned(), json!({"maxItems": 1}));
            if min_items == 1 {
                tmp_schema.insert("minItems".to_owned(), json!(2));
            }
            vec.push(tmp_schema.into());

            // We added "oneOf", so we need to manually do this fixup.
            let value = serde_json::to_value(vec)?;
            let size_fixup = ItemsSizeFixup::new("", &value, path.extend("oneOf"))?
                .map(|v| v.fixup())
                .ok_or(FixupError::UnexpectedSchemaError(
                    "item size fixup should operate after minmax fixup".to_owned(),
                    path.extend("oneOf"),
                    value,
                ))??;
            map.insert("oneOf".to_owned(), size_fixup);
        }

        Ok(())
    }
}

impl Fixup for IntMinMaxToMatrixFixup {
    fn new(
        propname: &str,
        value: &serde_json::Value,
        path: JsonPath,
    ) -> Result<Option<Self>, super::FixupError> {
        let items: Array = serde_json::from_value(value.clone())?;
        if !items.is_int_array_schema(propname) {
            return Ok(None);
        }

        let value = value.clone();
        match value.as_object() {
            Some(_) => {}
            None => {
                return Ok(None);
            }
        };

        Ok(Some(IntMinMaxToMatrixFixup {
            object: value,
            path,
        }))
    }

    fn fixup(mut self) -> Result<serde_json::Value, super::FixupError> {
        // Find the actual map we want to modify.
        let map = self.object.as_object_mut().unwrap();
        if let Some(all_of) = map.get_mut("allOf").and_then(|v| v.as_array_mut()) {
            for (i, item) in all_of.iter_mut().enumerate() {
                if item
                    .as_object_mut()
                    .map(|v| v.contains_any(&["minItems", "maxItems"]))
                    .unwrap_or(false)
                {
                    Self::do_fixup_on_object(
                        item.as_object_mut().unwrap(),
                        self.path.extend_array_index("allOf", i),
                    )?;
                    break;
                }
            }
        } else {
            Self::do_fixup_on_object(map, self.path)?;
        };

        Ok(self.object)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROP_NAME: &str = "test-percent";
    #[test]
    fn test_simple() {
        let schema = json!({
            "minItems": 2,
            "maxItems": 3,
        });

        let result = IntMinMaxToMatrixFixup::new(PROP_NAME, &schema, JsonPath::new())
            .expect("schema OK")
            .expect("fixup applies")
            .fixup()
            .expect("fixup OK");

        assert_eq!(
            result,
            json!({
                "oneOf": [
                    {
                        "items": [
                            {
                                "minItems": 2,
                                "maxItems": 3,
                            }
                        ],
                        "minItems": 1,
                        "maxItems": 1,
                        "type": "array",
                    },
                    {
                        "items": {
                            "maxItems": 1,
                            "minItems": 1,
                        },
                        "minItems": 2,
                        "maxItems": 3,
                        "type": "array",
                    }
                ]
            })
        );
    }

    #[test]
    fn test_preserves_allof() {
        let schema = json!({
            "allOf": [
                true,
                {"minItems": 1, "maxItems": 4}
            ]
        });

        let result = IntMinMaxToMatrixFixup::new(PROP_NAME, &schema, JsonPath::new())
            .expect("schema OK")
            .expect("fixup applies")
            .fixup()
            .expect("fixup OK");

        assert_eq!(
            result,
            json!({
                "allOf": [
                    true,
                    {
                        "oneOf": [
                            {"minItems": 1, "maxItems": 1, "items": [{"maxItems": 4, "minItems": 1}], "type": "array"},
                            {"minItems": 2, "maxItems": 4, "items": {"minItems": 1, "maxItems": 1}, "type": "array"},
                        ]
                    }
                ]
            })
        )
    }
}
