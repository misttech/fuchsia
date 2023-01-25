// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.


use serde_json::json;

use crate::{path::JsonPath, validator::array::Array};

use super::Fixup;

pub struct VariableIntMatrixFixup {
    object: serde_json::Map<String, serde_json::Value>,
}

impl Fixup for VariableIntMatrixFixup {
    fn new(
        _propname: &str,
        value: &serde_json::Value,
        _path: JsonPath,
    ) -> Result<Option<Self>, super::FixupError> {
        if let Some(object) = value.as_object() {
            Ok(Some(VariableIntMatrixFixup {
                object: object.clone(),
            }))
        } else {
            Ok(None)
        }
    }

    fn fixup(mut self) -> Result<serde_json::Value, super::FixupError> {
        let items: Array = serde_json::from_value(self.object.clone().into()).map_err(|e| {
            println!("{:?}", self.object);
            e
        })?;
        if !items.is_matrix() {
            return Ok(self.object.into());
        }

        let inner_dim = items.get_child_dim()?;
        let outer_dim = items.get_dim();

        if outer_dim[0] != outer_dim[1] && inner_dim[0] != inner_dim[1] {
            self.object.remove("items");
            self.object.remove("maxItems");
            self.object.remove("minItems");
            self.object.insert("type".to_owned(), json!("array"));
        }

        Ok(self.object.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_variable_matrix_fixup() {
        let matrix = json!({
            "items": {
                "minItems": 2,
                "maxItems": 3,
            },
            "minItems": 1,
            "maxItems": 4,
        });

        let result = VariableIntMatrixFixup::new("", &matrix, JsonPath::new())
            .expect("Valid schema")
            .unwrap()
            .fixup()
            .expect("Fixup ok");
        assert_eq!(result, json!({"type": "array"}));
    }

    #[test]
    fn test_ignores_nonvariable_matrix() {
        let matrix = json!({
            "items": {
                "minItems": 2,
                "maxItems": 2,
            },
            "minItems": 1,
            "maxItems": 4,
        });
        let result = VariableIntMatrixFixup::new("", &matrix, JsonPath::new())
            .expect("Valid schema")
            .unwrap()
            .fixup()
            .expect("Fixup ok");

        assert_eq!(result, matrix);
    }
}
