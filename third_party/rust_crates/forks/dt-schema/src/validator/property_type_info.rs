// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use fancy_regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;

use crate::path::JsonPath;

use super::{
    array::Array, error::ValidatorError, property_type::PropertyType, GeneratedPropertyType, Schema,
};

#[derive(Deserialize, Debug, Serialize)]
#[serde(untagged)]
pub enum SchemaType {
    Single(String),
    Many(Vec<String>),
}

impl SchemaType {
    pub fn is_exactly(&self, want: &str) -> bool {
        match self {
            SchemaType::Single(v) => v == want,
            SchemaType::Many(_) => false,
        }
    }
}

#[derive(Deserialize, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
/// Represents the structured data we need to determine the type of a schema.
pub struct PropertyTypeInfo {
    r#type: Option<SchemaType>,

    #[serde(rename = "$ref")]
    dollar_ref: Option<String>,

    #[serde(flatten)]
    array: Array,
    dim: Option<Vec<Vec<u32>>>,

    // These are actually used by |Schema| to recurse.
    // TODO(simonshields): consider making Schema just poke |etc| directly.
    pub one_of: Option<Vec<serde_json::Value>>,
    pub any_of: Option<Vec<serde_json::Value>>,
    pub all_of: Option<Vec<serde_json::Value>>,

    #[serde(flatten)]
    pub etc: HashMap<String, serde_json::Value>,
}

impl PropertyTypeInfo {
    /// Determine type information represented by this |PropertyTypeInfo|.
    #[tracing::instrument(level = "debug", skip(self, schema), fields(path=%path))]
    pub fn extract_type(
        &self,
        schema: &Schema,
        name: &str,
        is_pattern: bool,
        path: JsonPath,
    ) -> Result<Option<GeneratedPropertyType>, ValidatorError> {
        if name.starts_with('$') {
            return Ok(None);
        }

        let type_re = Regex::new(
            "(flag|u?int(8|16|32|64)(-(array|matrix))?|string(-array)?|phandle(-array)?)",
        )
        .unwrap();
        // Determine type.
        let prop_type = if self
            .r#type
            .as_ref()
            .map(|x| x.is_exactly("object"))
            .unwrap_or(false)
        {
            Some(PropertyType::Node)
        } else if let Ok(Some(result)) =
            type_re.find(&self.dollar_ref.clone().unwrap_or_else(|| "".to_owned()))
        {
            // Try and guess the reference type based on |type_re|.
            tracing::debug!("Type found based on $ref");
            Some(PropertyType::from_str(result.as_str())?)
        } else if self
            .r#type
            .as_ref()
            .map(|x| x.is_exactly("boolean"))
            .unwrap_or(false)
        {
            Some(PropertyType::Flag)
        } else if self.array.has_items() {
            if self.array.is_string_schema() {
                Some(PropertyType::StringArray)
            } else if self.array.is_uint32_matrix(name) {
                Some(PropertyType::Uint32Matrix)
            } else {
                None
            }
        } else if Regex::new("\\.yaml#?$")
            .unwrap()
            .is_match(&self.dollar_ref.clone().unwrap_or_else(|| "na".to_owned()))?
        {
            // Looks like a reference to another schema type.
            Some(PropertyType::Node)
        } else {
            None
        };

        tracing::debug!(name = name, "Found type type={:?}", prop_type);

        // If this is a matrix, determine its dimensions.
        let dim = match prop_type.map(|v| v.is_matrix()) {
            Some(true) => Some([self.array.get_dim(), self.array.get_child_dim()?].into()),
            _ => None,
        };

        let new_prop = GeneratedPropertyType {
            r#type: prop_type,
            id: vec![schema.spec_id().clone().ok_or(ValidatorError::ExpectedKey(
                "spec_id".to_owned(),
                JsonPath::new(),
            ))?]
            .into_iter()
            .collect(),
            dim,
            regex: if is_pattern {
                Some(Regex::new(name)?)
            } else {
                None
            },
        };

        Ok(Some(new_prop))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    fn fake_schema() -> Schema {
        Schema::from_value(
            json!({
                "$id": "fake-test-schema",
                "properties": {
                    "my_reference_property": {
                        "$ref": "#/properties/my_other_prop",
                    },
                    "my_other_prop": {
                        "type": "boolean"
                    },
                    "my_external_ref": {
                        "$ref": "types.yaml#/definitions/string-array"
                    },
                    "implicit_string_array": {
                        "items": {
                            "enum": ["a", "b"]
                        }
                    },
                    "implicit_u32_matrix-bits": {
                        "items": {
                            "items": [{}]
                        }
                    },
                    "other_schema_ref": {
                        "$ref": "my-type.yaml"
                    }
                }

            }),
            "test".to_owned(),
        )
        .unwrap()
    }

    fn get_property_decl(schema: &Schema, name: &str) -> PropertyTypeInfo {
        serde_json::from_value(schema.properties().get(name).unwrap().clone()).unwrap()
    }

    #[test]
    fn test_external_ref() {
        let schema = fake_schema();
        let val = get_property_decl(&schema, "my_external_ref")
            .extract_type(&schema, "my_external_ref", false, JsonPath::new())
            .unwrap();
        assert_eq!(val.map(|v| v.r#type), Some(Some(PropertyType::StringArray)));
    }

    #[test]
    fn test_implicit_string_array() {
        let schema = fake_schema();
        let val = get_property_decl(&schema, "implicit_string_array")
            .extract_type(&schema, "implicit_string_array", false, JsonPath::new())
            .unwrap();
        assert_eq!(val.map(|v| v.r#type), Some(Some(PropertyType::StringArray)));
    }

    #[test]
    fn test_implicit_u32_matrix() {
        let schema = fake_schema();
        let val = get_property_decl(&schema, "implicit_u32_matrix-bits")
            .extract_type(&schema, "implicit_u32_matrix-bits", false, JsonPath::new())
            .unwrap();
        assert_eq!(
            val.map(|v| v.r#type),
            Some(Some(PropertyType::Uint32Matrix))
        );
    }

    #[test]
    fn test_other_schema_ref() {
        let schema = fake_schema();
        let val = get_property_decl(&schema, "other_schema_ref")
            .extract_type(&schema, "other_schema_ref", false, JsonPath::new())
            .unwrap();
        assert_eq!(val.map(|v| v.r#type), Some(Some(PropertyType::Node)));
    }
}
