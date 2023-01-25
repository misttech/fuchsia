// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use serde_json::json;

use crate::path::JsonPath;

use super::{Fixup, FixupError};

/// Fix pre-201909 schemas to be compatible with 201909.
/// The only transformation is splitting "dependencies" into "dependentRequired" and "dependentSchemas" per
/// https://json-schema.org/understanding-json-schema/reference/conditionals.html (see "draft-specific info").
pub struct Fixup201909 {
    map: serde_json::Map<String, serde_json::Value>,
    path: JsonPath,
}

impl Fixup for Fixup201909 {
    fn new(
        _propname: &str,
        value: &serde_json::Value,
        path: JsonPath,
    ) -> Result<Option<Self>, super::FixupError> {
        match value {
            serde_json::Value::Object(o) => {
                if o.contains_key("dependencies") {
                    Ok(Some(Fixup201909 {
                        map: o.clone(),
                        path,
                    }))
                } else {
                    Ok(None)
                }
            }
            _ => Ok(None),
        }
    }

    fn fixup(mut self) -> Result<serde_json::Value, super::FixupError> {
        let value = self.map.remove("dependencies").unwrap();
        let dependencies = match value {
            serde_json::Value::Object(o) => o,
            _ => {
                return Err(FixupError::UnexpectedSchemaError(
                    "dependencies should be a map".to_owned(),
                    self.path.extend("dependencies"),
                    value.clone(),
                ));
            }
        };

        for (k, v) in dependencies.into_iter() {
            match v {
                serde_json::Value::Array(array) => {
                    let dependent_required = self.map.entry("dependentRequired");
                    dependent_required
                        .or_insert(json!({}))
                        .as_object_mut()
                        .unwrap()
                        .insert(k, array.into());
                }
                value => {
                    let dependent_schemas = self.map.entry("dependentSchemas");
                    dependent_schemas
                        .or_insert(json!({}))
                        .as_object_mut()
                        .unwrap()
                        .insert(k, value);
                }
            }
        }

        Ok(self.map.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_translate_old_schema() {
        let old_value = json!({
            "dependencies": {
                "propertyOne": ["propertyTwo"],
                "propertyTwo": {
                    "propertyThree": {"type": "string"}
                }
            }
        });

        let result = Fixup201909::new("", &old_value, JsonPath::new())
            .expect("Schema ok")
            .unwrap()
            .fixup()
            .expect("Fixup ok");
        assert_eq!(
            result,
            json!({
                "dependentRequired": {"propertyOne": ["propertyTwo"]},
                "dependentSchemas": {"propertyTwo": {
                    "propertyThree": {"type": "string"}
                }}
            })
        );
    }
}
