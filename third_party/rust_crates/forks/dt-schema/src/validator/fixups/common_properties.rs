// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use std::collections::HashSet;

use fancy_regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::path::JsonPath;

use super::Fixup;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct NodeWithProperties {
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    properties: serde_json::Map<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pattern_properties: serde_json::Map<String, serde_json::Value>,

    #[serde(skip_serializing_if = "Option::is_none")]
    unevaluated_properties: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    additional_properties: Option<serde_json::Value>,

    #[serde(flatten)]
    etc: serde_json::Map<String, serde_json::Value>,
}

impl NodeWithProperties {
    fn has_properties(&self) -> bool {
        !self.properties.is_empty()
            || !self.pattern_properties.is_empty()
            || self.unevaluated_properties.is_some()
    }

    fn is_incomplete_schema(&self) -> bool {
        self.unevaluated_properties
            .as_ref()
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            || self
                .additional_properties
                .as_ref()
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
    }
}

/// Add missing node properties to schemas.
/// This includes adding phandle, status, secure-status and $nodename unconditionally,
/// adding pinctrl properties if needed and also adding assigned-clock properties if needed.
pub struct CommonPropertiesFixup {
    node: NodeWithProperties,
}

impl Fixup for CommonPropertiesFixup {
    fn new(
        _propname: &str,
        value: &serde_json::Value,
        _path: JsonPath,
    ) -> Result<Option<Self>, super::FixupError> {
        if !value.is_object() {
            return Ok(None);
        }
        let value: NodeWithProperties = serde_json::from_value(value.clone())?;
        if !value.has_properties() || value.is_incomplete_schema() {
            Ok(None)
        } else {
            Ok(Some(CommonPropertiesFixup { node: value }))
        }
    }

    fn fixup(mut self) -> Result<serde_json::Value, super::FixupError> {
        for value in ["phandle", "status", "secure-status", "$nodename"] {
            self.node.properties.entry(value).or_insert(json!(true));
        }

        let key_pinctrl_regex = Regex::new("^pinctrl-[0-9]").unwrap();
        let all_keys: HashSet<String> = self
            .node
            .properties
            .keys()
            .chain(self.node.pattern_properties.keys())
            .cloned()
            .collect();

        if !all_keys
            .iter()
            .any(|key| key_pinctrl_regex.is_match(key).unwrap())
        {
            self.node
                .properties
                .entry("pinctrl-names")
                .or_insert(json!(true));
            self.node
                .pattern_properties
                .insert("pinctrl-[0-9]+".to_owned(), json!(true));
        }

        if all_keys.contains("clocks") && !all_keys.contains("assigned-clocks") {
            self.node
                .properties
                .insert("assigned-clocks".to_owned(), json!(true));
            self.node
                .properties
                .insert("assigned-clock-rates".to_owned(), json!(true));
            self.node
                .properties
                .insert("assigned-clock-parents".to_owned(), json!(true));
        }

        Ok(serde_json::to_value(self.node)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_incomplete() {
        let schema = json!({
            "properties": {
                "clocks": 10,
            },
            "additionalProperties": true,
        });

        assert!(CommonPropertiesFixup::new("", &schema, JsonPath::new())
            .expect("No errors")
            .is_none());
    }

    #[test]
    fn test_no_properties() {
        let schema = json!({});

        assert!(CommonPropertiesFixup::new("", &schema, JsonPath::new())
            .expect("No errors")
            .is_none());
    }

    #[test]
    fn test_basic_properties() {
        let schema = json!({"properties": { "example": true }});

        let result = CommonPropertiesFixup::new("", &schema, JsonPath::new())
            .expect("No errors")
            .expect("Applies to schema")
            .fixup()
            .expect("Fixup OK");

        assert_eq!(
            result,
            json!({
                "properties": {
                    "example": true,
                    "phandle": true,
                    "status": true,
                    "secure-status": true,
                    "$nodename": true,
                    "pinctrl-names": true,
                },
                "patternProperties": {
                    "pinctrl-[0-9]+": true,
                }
            })
        );
    }

    #[test]
    fn test_basic_properties_not_overwritten() {
        let schema = json!({"properties": {
            "$nodename": {"const": "foo"}
        }});

        let result = CommonPropertiesFixup::new("", &schema, JsonPath::new())
            .expect("No errors")
            .expect("Applies to schema")
            .fixup()
            .expect("Fixup OK");

        assert_eq!(
            result,
            json!({
                "properties": {
                    "phandle": true,
                    "status": true,
                    "secure-status": true,
                    "$nodename": {"const": "foo"},
                    "pinctrl-names": true,
                },
                "patternProperties": {
                    "pinctrl-[0-9]+": true,
                }
            })
        );
    }

    #[test]
    fn test_adds_assigned_clocks() {
        let schema = json!({"properties": {
            "clocks": {"maxItems": 2}
        }});

        let result = CommonPropertiesFixup::new("", &schema, JsonPath::new())
            .expect("No errors")
            .expect("Applies to schema")
            .fixup()
            .expect("Fixup OK");

        assert_eq!(
            result,
            json!({
                "properties": {
                    "clocks": {"maxItems": 2},
                    "phandle": true,
                    "status": true,
                    "secure-status": true,
                    "$nodename": true,
                    "pinctrl-names": true,
                    "assigned-clocks": true,
                    "assigned-clock-rates": true,
                    "assigned-clock-parents": true
                },
                "patternProperties": {
                    "pinctrl-[0-9]+": true,
                }
            })
        );
    }

    #[test]
    fn test_explicit_pinctrl() {
        let schema = json!({"properties": {
            "pinctrl-0": true,
            "pinctrl-1": true,
        }});

        let result = CommonPropertiesFixup::new("", &schema, JsonPath::new())
            .expect("No errors")
            .expect("Applies to schema")
            .fixup()
            .expect("Fixup OK");

        assert_eq!(
            result,
            json!({
                "properties": {
                    "phandle": true,
                    "status": true,
                    "secure-status": true,
                    "$nodename": true,
                    "pinctrl-0": true,
                    "pinctrl-1": true,
                }
            })
        );
    }
}
