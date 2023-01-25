// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.


use std::collections::HashMap;

use serde_json::json;

use crate::{path::JsonPath, validator::util::IsSchema};

use super::{Fixup, FixupError};

/// Translate a "reg" property schema into a more fully formed schema that identifies the number
/// of items in each reg "entry" as well as the total number of reg entries.
pub struct RegFixup {
    object: serde_json::Map<String, serde_json::Value>,
}

impl Fixup for RegFixup {
    fn new(
        propname: &str,
        value: &serde_json::Value,
        path: JsonPath,
    ) -> Result<Option<Self>, super::FixupError> {
        if propname != "reg" {
            return Ok(None);
        }

        if let Some(object) = value.as_object() {
            Ok(Some(Self {
                object: object.clone(),
            }))
        } else {
            Err(FixupError::UnexpectedSchemaError(
                "reg should be an object, but it was not".to_owned(),
                path,
                value.clone(),
            ))
        }
    }

    fn fixup(mut self) -> Result<serde_json::Value, super::FixupError> {
        let map = self
            .object
            .get("items")
            .and_then(|v| match v {
                serde_json::Value::Array(array) => array.get(0),
                other => Some(other),
            })
            .and_then(|v| v.as_object())
            .unwrap_or(&self.object);
        if !map.is_int_schema() {
            return Ok(self.object.into());
        }

        let mut object: HashMap<String, serde_json::Value> = HashMap::new();
        object.extend(
            map.iter()
                .filter(|(k, _)| {
                    let k = k.as_str();
                    k == "const" || k == "enum" || k == "minimum" || k == "maximum"
                })
                .map(|(k, v)| (k.clone(), v.clone())),
        );

        self.object
            .insert("items".to_owned(), json!([{ "items": object }]));

        Ok(self.object.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_reg_fixup_basic() {
        let reg = json!({
            "items": {
                "minimum": 10,
                "maximum": 12,
            }
        });

        let result = RegFixup::new("reg", &reg, JsonPath::new())
            .expect("reg schema valid")
            .unwrap()
            .fixup()
            .expect("fixup succeeds");

        assert_eq!(
            result,
            json!({
                "items": [{
                    "items": {"minimum": 10, "maximum": 12},
                }]
            })
        );
    }

    #[test]
    fn test_reg_fixup_list() {
        let reg = json!({
            "items": [{"enum": [2, 4]}]
        });

        let result = RegFixup::new("reg", &reg, JsonPath::new())
            .expect("reg schema valid")
            .unwrap()
            .fixup()
            .expect("fixup succeeds");
        assert_eq!(
            result,
            json!({
                "items": [{"items": {"enum": [2, 4]}}]
            })
        )
    }
}
