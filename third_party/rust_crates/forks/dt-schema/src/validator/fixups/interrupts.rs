// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.


use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::path::JsonPath;

use super::Fixup;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
struct InterruptProps {
    #[serde(skip_serializing_if = "Option::is_none")]
    interrupts: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    interrupts_extended: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    interrupt_controller: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    interrupt_parent: Option<serde_json::Value>,

    #[serde(flatten)]
    etc: serde_json::Value,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Interrupt {
    properties: InterruptProps,
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    required: HashSet<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    one_of: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    all_of: Vec<serde_json::Value>,

    #[serde(flatten)]
    etc: serde_json::Value,
}

/// |InterruptsFixup| does two things:
/// 1. Allows any node with "interrupts" or "interrupt-controller" properties to have an "interrupt-parent"
/// 2. Allows any node with "interrupts" to have "interrupts-extended".
pub struct InterruptsFixup {
    interrupt: Interrupt,
}

impl Fixup for InterruptsFixup {
    fn new(
        _propname: &str,
        value: &serde_json::Value,
        _path: JsonPath,
    ) -> Result<Option<Self>, super::FixupError> {
        if value
            .as_object()
            .and_then(|v| v.get("properties"))
            .is_some()
        {
            Ok(Some(InterruptsFixup {
                interrupt: serde_json::from_value(value.clone())?,
            }))
        } else {
            Ok(None)
        }
    }

    fn fixup(mut self) -> Result<serde_json::Value, super::FixupError> {
        let props = &mut self.interrupt.properties;

        // Any node with interrupts can have 'interrupt-parent'.
        if props
            .interrupts
            .as_ref()
            .or(props.interrupt_controller.as_ref())
            .is_some()
            && props.interrupt_parent.is_none()
        {
            props.interrupt_parent = Some(json!(true));
        }

        // Any node with 'interrupts' can also have 'interrupts-extended'.
        match (
            props.interrupts.as_ref(),
            props.interrupts_extended.as_ref(),
        ) {
            (None, _) => return Ok(serde_json::to_value(self.interrupt)?),
            (Some(interrupts), None) => props.interrupts_extended = Some(interrupts.clone()),
            (Some(_), Some(_)) => {}
        }

        if self.interrupt.required.remove("interrupts") {
            let required = vec![
                json!({"required": ["interrupts"]}),
                json!({"required": ["interrupts-extended"]}),
            ];
            if !self.interrupt.one_of.is_empty() {
                if self.interrupt.all_of.is_empty() {
                    self.interrupt.all_of.push(json!({ "oneOf": required }));
                } else {
                    self.interrupt.all_of = vec![json!({ "oneOf": required })];
                }
            } else {
                self.interrupt.one_of = required;
            }
        }
        Ok(serde_json::to_value(self.interrupt)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_interrupt_controller() {
        let schema = json!({
            "properties": {
                "interrupt-controller": true,
            }
        });

        let result = InterruptsFixup::new("", &schema, JsonPath::new())
            .expect("schema ok")
            .expect("fixup applies")
            .fixup()
            .expect("fixup ok");
        assert_eq!(
            result,
            json!({
                "properties": {
                    "interrupt-controller": true,
                    "interrupt-parent": true,
                }
            })
        )
    }

    #[test]
    fn test_interrupts() {
        let schema = json!({
            "properties": {
                "interrupts": {"items": [{"description": "IRQ 1"}]},
            }
        });

        let result = InterruptsFixup::new("", &schema, JsonPath::new())
            .expect("schema ok")
            .expect("fixup applies")
            .fixup()
            .expect("fixup ok");
        assert_eq!(
            result,
            json!({
                "properties": {
                    "interrupt-parent": true,
                    "interrupts": {"items": [{"description": "IRQ 1"}]},
                    "interrupts-extended": {"items": [{"description": "IRQ 1"}]},
                }
            })
        )
    }

    #[test]
    fn test_interrupts_required() {
        let schema = json!({
            "properties": {
                "interrupts": {"items": [{"description": "IRQ 1"}]},
            },
            "required": ["interrupts"]
        });

        let result = InterruptsFixup::new("", &schema, JsonPath::new())
            .expect("schema ok")
            .expect("fixup applies")
            .fixup()
            .expect("fixup ok");
        assert_eq!(
            result,
            json!({
                "properties": {
                    "interrupt-parent": true,
                    "interrupts": {"items": [{"description": "IRQ 1"}]},
                    "interrupts-extended": {"items": [{"description": "IRQ 1"}]},
                },
                "oneOf": [
                    {"required": ["interrupts"]},
                    {"required": ["interrupts-extended"]}
                ]
            })
        )
    }

    #[test]
    fn test_interrupts_required_already_oneof() {
        let schema = json!({
            "properties": {
                "interrupts": {"items": [{"description": "IRQ 1"}]},
            },
            "required": ["interrupts"],
            "oneOf": [{"$nodename": "hello"}, {"$nodename": "hello2"}]
        });

        let result = InterruptsFixup::new("", &schema, JsonPath::new())
            .expect("schema ok")
            .expect("fixup applies")
            .fixup()
            .expect("fixup ok");
        assert_eq!(
            result,
            json!({
                "properties": {
                    "interrupt-parent": true,
                    "interrupts": {"items": [{"description": "IRQ 1"}]},
                    "interrupts-extended": {"items": [{"description": "IRQ 1"}]},
                },
                "oneOf": [{"$nodename": "hello"}, {"$nodename": "hello2"}],
                "allOf": [
                    {
                        "oneOf": [
                            {"required": ["interrupts"]},
                            {"required": ["interrupts-extended"]}
                        ]
                    },
                ]
            })
        )
    }
}
