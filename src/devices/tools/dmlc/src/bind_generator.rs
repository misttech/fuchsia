// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::parser::DmlBind;
use anyhow::anyhow;
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct AdditionalParentInfo {
    pub parent_name: String,
    pub service_name: Option<String>,
    pub banjo_name: Option<String>,
    pub transport: String,
    pub optional: bool,
    pub bind: Option<DmlBind>,
}

fn format_bind_val(val: &Value) -> Result<String, anyhow::Error> {
    match val {
        Value::Number(n) => Ok(n.to_string()),
        Value::String(s) => {
            if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
                Ok(s.clone())
            } else if s.contains('.') || s.starts_with("0x") || s.starts_with("0X") {
                Ok(s.clone())
            } else {
                Ok(format!("\"{}\"", s))
            }
        }
        Value::Bool(b) => Ok(b.to_string()),
        _ => Err(anyhow!("Invalid bind value type: {} (expected string, number or bool)", val)),
    }
}

fn generate_simple_bind_statements_excluding(
    bind: &DmlBind,
    exclude_protocol: bool,
    exclude_service: bool,
    exclude_compat: bool,
    exclude_vid: bool,
    exclude_pid: bool,
    exclude_did: bool,
    exclude_rule: Option<(&str, &Value)>,
) -> Result<String, anyhow::Error> {
    let mut content = String::new();
    if !exclude_protocol {
        if let Some(proto) = &bind.protocol {
            content.push_str(&format!("fuchsia.BIND_PROTOCOL == {};\n", proto));
        } else if let Some(banjo) = &bind.banjo {
            content.push_str(&format!("fuchsia.BIND_PROTOCOL == {};\n", banjo));
        }
    }
    if !exclude_service {
        if let Some(svc) = &bind.service {
            content.push_str(&format!("fuchsia.Service == \"{}\";\n", svc));
        }
    }
    if !exclude_vid {
        if let Some(vid) = &bind.vid {
            match vid {
                Value::Array(arr) => {
                    content.push_str("accept fuchsia.BIND_PLATFORM_DEV_VID {\n");
                    for v in arr {
                        content.push_str(&format!("  {},\n", format_bind_val(v)?));
                    }
                    content.push_str("}\n");
                }
                _ => {
                    content.push_str(&format!(
                        "fuchsia.BIND_PLATFORM_DEV_VID == {};\n",
                        format_bind_val(vid)?
                    ));
                }
            }
        }
    }
    if !exclude_pid {
        if let Some(pid) = &bind.pid {
            match pid {
                Value::Array(arr) => {
                    content.push_str("accept fuchsia.BIND_PLATFORM_DEV_PID {\n");
                    for v in arr {
                        content.push_str(&format!("  {},\n", format_bind_val(v)?));
                    }
                    content.push_str("}\n");
                }
                _ => {
                    content.push_str(&format!(
                        "fuchsia.BIND_PLATFORM_DEV_PID == {};\n",
                        format_bind_val(pid)?
                    ));
                }
            }
        }
    }
    if !exclude_did {
        if let Some(did) = &bind.did {
            match did {
                Value::Array(arr) => {
                    content.push_str("accept fuchsia.BIND_PLATFORM_DEV_DID {\n");
                    for v in arr {
                        content.push_str(&format!("  {},\n", format_bind_val(v)?));
                    }
                    content.push_str("}\n");
                }
                _ => {
                    content.push_str(&format!(
                        "fuchsia.BIND_PLATFORM_DEV_DID == {};\n",
                        format_bind_val(did)?
                    ));
                }
            }
        }
    }
    if !exclude_compat {
        if let Some(compat) = &bind.compat {
            match compat {
                Value::Array(arr) => {
                    content.push_str("accept fuchsia.COMPATIBLE {\n");
                    for v in arr {
                        if let Some(s) = v.as_str() {
                            content.push_str(&format!("  \"{}\",\n", s));
                        }
                    }
                    content.push_str("}\n");
                }
                Value::String(s) => {
                    content.push_str(&format!("fuchsia.COMPATIBLE == \"{}\";\n", s));
                }
                _ => {}
            }
        }
    }
    if let Some(pci_class) = &bind.pci_class {
        content.push_str(&format!("fuchsia.BIND_PCI_CLASS == {};\n", pci_class));
    }
    if let Some(pci_subclass) = &bind.pci_subclass {
        content.push_str(&format!("fuchsia.BIND_PCI_SUBCLASS == {};\n", pci_subclass));
    }
    if let Some(pci_interface) = &bind.pci_interface {
        content.push_str(&format!("fuchsia.BIND_PCI_INTERFACE == {};\n", pci_interface));
    }
    if let Some(rules) = &bind.rules {
        let mut sorted_rules: Vec<_> = rules.iter().collect();
        sorted_rules.sort_unstable_by_key(|&(key, _)| key);
        for (key, val) in sorted_rules {
            if let Some((ex_key, ex_val)) = exclude_rule {
                if key == ex_key && val == ex_val {
                    continue;
                }
            }
            match val {
                Value::Array(arr) => {
                    content.push_str(&format!("accept {} {{\n", key));
                    for v in arr {
                        content.push_str(&format!("  {},\n", format_bind_val(v)?));
                    }
                    content.push_str("}\n");
                }
                Value::Object(map) => {
                    if let Some(neq_val) = map.get("neq") {
                        content.push_str(&format!("{} != {};\n", key, format_bind_val(neq_val)?));
                    } else {
                        return Err(anyhow!(
                            "Unsupported rule object operator for '{}': {:?} (expected 'neq')",
                            key,
                            map
                        ));
                    }
                }
                _ => {
                    content.push_str(&format!("{} == {};\n", key, format_bind_val(val)?));
                }
            }
        }
    }
    Ok(content)
}

fn generate_simple_bind_statements(bind: &DmlBind) -> Result<String, anyhow::Error> {
    generate_simple_bind_statements_excluding(bind, false, false, false, false, false, false, None)
}

enum TriggerKind {
    Protocol(String),
    Service(String),
    Compat(String),
    Vid(String),
    Pid(String),
    Did(String),
    Rule(String, Value, String),
}

impl TriggerKind {
    fn condition_str(&self) -> &str {
        match self {
            TriggerKind::Protocol(s)
            | TriggerKind::Service(s)
            | TriggerKind::Compat(s)
            | TriggerKind::Vid(s)
            | TriggerKind::Pid(s)
            | TriggerKind::Did(s)
            | TriggerKind::Rule(_, _, s) => s,
        }
    }
}

fn get_trigger(alt: &DmlBind) -> Result<Option<TriggerKind>, anyhow::Error> {
    if let Some(rules) = &alt.rules {
        if let Some(autobind_val) = rules.get("fuchsia.BIND_AUTOBIND") {
            if !autobind_val.is_object() && !autobind_val.is_array() {
                return Ok(Some(TriggerKind::Rule(
                    "fuchsia.BIND_AUTOBIND".to_string(),
                    autobind_val.clone(),
                    format!("fuchsia.BIND_AUTOBIND == {}", format_bind_val(autobind_val)?),
                )));
            }
        }
        if let Some(acpi_bus) = rules.get("fuchsia.BIND_ACPI_BUS_TYPE") {
            if !acpi_bus.is_object() && !acpi_bus.is_array() {
                return Ok(Some(TriggerKind::Rule(
                    "fuchsia.BIND_ACPI_BUS_TYPE".to_string(),
                    acpi_bus.clone(),
                    format!("fuchsia.BIND_ACPI_BUS_TYPE == {}", format_bind_val(acpi_bus)?),
                )));
            }
        }
    }
    if let Some(compat) = &alt.compat
        && !compat.is_array()
    {
        match compat {
            Value::String(s) => {
                return Ok(Some(TriggerKind::Compat(format!("fuchsia.COMPATIBLE == \"{}\"", s))));
            }
            _ => {}
        }
    }
    if let Some(rules) = &alt.rules {
        if let Some(hid) = rules.get("fuchsia.acpi.HID") {
            if !hid.is_object() && !hid.is_array() {
                return Ok(Some(TriggerKind::Rule(
                    "fuchsia.acpi.HID".to_string(),
                    hid.clone(),
                    format!("fuchsia.acpi.HID == {}", format_bind_val(hid)?),
                )));
            }
        }
    }
    if let Some(proto) = &alt.protocol {
        Ok(Some(TriggerKind::Protocol(format!("fuchsia.BIND_PROTOCOL == {}", proto))))
    } else if let Some(banjo) = &alt.banjo {
        Ok(Some(TriggerKind::Protocol(format!("fuchsia.BIND_PROTOCOL == {}", banjo))))
    } else if let Some(svc) = &alt.service {
        Ok(Some(TriggerKind::Service(format!("fuchsia.Service == \"{}\"", svc))))
    } else if let Some(vid) = &alt.vid
        && !vid.is_array()
    {
        Ok(Some(TriggerKind::Vid(format!(
            "fuchsia.BIND_PLATFORM_DEV_VID == {}",
            format_bind_val(vid)?
        ))))
    } else if let Some(pid) = &alt.pid
        && !pid.is_array()
    {
        Ok(Some(TriggerKind::Pid(format!(
            "fuchsia.BIND_PLATFORM_DEV_PID == {}",
            format_bind_val(pid)?
        ))))
    } else if let Some(did) = &alt.did
        && !did.is_array()
    {
        Ok(Some(TriggerKind::Did(format!(
            "fuchsia.BIND_PLATFORM_DEV_DID == {}",
            format_bind_val(did)?
        ))))
    } else if let Some(rules) = &alt.rules {
        if let Some(name_val) = rules.get("fuchsia.NAME") {
            if !name_val.is_object() && !name_val.is_array() {
                return Ok(Some(TriggerKind::Rule(
                    "fuchsia.NAME".to_string(),
                    name_val.clone(),
                    format!("fuchsia.NAME == {}", format_bind_val(name_val)?),
                )));
            }
        }
        Ok(None)
    } else {
        Ok(None)
    }
}

fn generate_simple_bind_rules(bind: &DmlBind) -> Result<String, anyhow::Error> {
    let mut content = String::new();
    if let Some(alternatives) = &bind.one_of {
        if let Some(proto) = &bind.protocol {
            content.push_str(&format!("fuchsia.BIND_PROTOCOL == {};\n", proto));
        } else if let Some(banjo) = &bind.banjo {
            content.push_str(&format!("fuchsia.BIND_PROTOCOL == {};\n", banjo));
        }
        if let Some(svc) = &bind.service {
            content.push_str(&format!("fuchsia.Service == \"{}\";\n", svc));
        }
        if let Some(vid) = &bind.vid {
            content.push_str(&format!(
                "fuchsia.BIND_PLATFORM_DEV_VID == {};\n",
                format_bind_val(vid)?
            ));
        }
        if let Some(pid) = &bind.pid {
            content.push_str(&format!(
                "fuchsia.BIND_PLATFORM_DEV_PID == {};\n",
                format_bind_val(pid)?
            ));
        }
        if let Some(did) = &bind.did {
            content.push_str(&format!(
                "fuchsia.BIND_PLATFORM_DEV_DID == {};\n",
                format_bind_val(did)?
            ));
        }

        let mut last_had_trigger = false;
        for (i, alt) in alternatives.iter().enumerate() {
            let trigger_opt = get_trigger(alt)?;
            let has_trigger = trigger_opt.is_some();

            if i == 0 {
                if let Some(trigger) = &trigger_opt {
                    content.push_str(&format!("if {} {{\n", trigger.condition_str()));
                    last_had_trigger = true;
                } else {
                    return generate_simple_bind_statements(alt);
                }
            } else {
                if let Some(trigger) = &trigger_opt {
                    content.push_str(&format!("}} else if {} {{\n", trigger.condition_str()));
                    last_had_trigger = true;
                } else {
                    content.push_str("} else {\n");
                    last_had_trigger = false;
                }
            }

            let statements = if let Some(trigger) = &trigger_opt {
                let mut exclude_protocol = false;
                let mut exclude_service = false;
                let mut exclude_compat = false;
                let mut exclude_vid = false;
                let mut exclude_pid = false;
                let mut exclude_did = false;
                let mut ex_rule_ref = None;

                match trigger {
                    TriggerKind::Protocol(_) => exclude_protocol = true,
                    TriggerKind::Service(_) => exclude_service = true,
                    TriggerKind::Compat(_) => exclude_compat = true,
                    TriggerKind::Vid(_) => exclude_vid = true,
                    TriggerKind::Pid(_) => exclude_pid = true,
                    TriggerKind::Did(_) => exclude_did = true,
                    TriggerKind::Rule(k, v, _) => ex_rule_ref = Some((k.as_str(), v)),
                }

                generate_simple_bind_statements_excluding(
                    alt,
                    exclude_protocol,
                    exclude_service,
                    exclude_compat,
                    exclude_vid,
                    exclude_pid,
                    exclude_did,
                    ex_rule_ref,
                )?
            } else {
                generate_simple_bind_statements(alt)?
            };

            if statements.trim().is_empty() {
                content.push_str("    true;\n");
            } else {
                for line in statements.lines() {
                    if !line.trim().is_empty() {
                        content.push_str(&format!("    {}\n", line));
                    }
                }
            }

            if !has_trigger {
                break;
            }
        }
        if last_had_trigger {
            content.push_str("} else {\n    false;\n}\n");
        } else {
            content.push_str("}\n");
        }
    } else {
        content.push_str(&generate_simple_bind_statements(bind)?);
    }
    Ok(content)
}

pub fn generate_bind_file(
    driver_name: &str,
    bind: &DmlBind,
    additional_parents: &[AdditionalParentInfo],
    year: &str,
) -> Result<String, anyhow::Error> {
    let mut content = String::new();

    let is_composite = bind.primary.is_some() || !additional_parents.is_empty();

    if is_composite {
        let comp_name = bind.composite_name.as_deref().unwrap_or(driver_name).replace("-", "_");
        content.push_str(&format!("composite {};\n\n", comp_name));
        content.push_str("using fuchsia;\n\n");

        if let Some(primary) = &bind.primary {
            content.push_str(&format!("primary parent \"{}\" {{\n", primary.node));

            let dml_bind = DmlBind {
                protocol: primary.protocol.clone(),
                service: primary.service.clone(),
                banjo: primary.banjo.clone(),
                transport: primary.transport.clone(),
                vid: primary.vid.clone(),
                pid: primary.pid.clone(),
                did: primary.did.clone(),
                compat: primary.compat.clone(),
                primary: None,
                one_of: primary.one_of.clone(),
                rules: primary.rules.clone(),
                pci_class: primary.pci_class.clone(),
                pci_subclass: primary.pci_subclass.clone(),
                pci_interface: primary.pci_interface.clone(),
                composite_name: None,
            };
            let rules_str = generate_simple_bind_rules(&dml_bind)?;
            if rules_str.trim().is_empty() {
                content.push_str(
                    "  fuchsia.BIND_PROTOCOL == fuchsia.platform.BIND_PROTOCOL.DEVICE;\n",
                );
            } else {
                for line in rules_str.lines() {
                    if !line.trim().is_empty() {
                        content.push_str(&format!("  {}\n", line));
                    }
                }
            }
            content.push_str("}\n\n");
        }

        let primary_node_name = bind.primary.as_ref().map(|p| p.node.as_str());
        let mut grouped_parents = std::collections::BTreeMap::<
            String,
            (Vec<(Option<String>, Option<String>, String)>, bool, Option<DmlBind>),
        >::new();
        for parent in additional_parents {
            if Some(parent.parent_name.as_str()) == primary_node_name {
                continue;
            }
            let entry = grouped_parents
                .entry(parent.parent_name.clone())
                .or_insert_with(|| (Vec::new(), true, None));
            entry.0.push((
                parent.service_name.clone(),
                parent.banjo_name.clone(),
                parent.transport.clone(),
            ));
            entry.1 = entry.1 && parent.optional;
            if parent.bind.is_some() && entry.2.is_none() {
                entry.2 = parent.bind.clone();
            }
        }

        for (parent_name, (capabilities, optional, parent_bind)) in grouped_parents {
            let prefix = if optional { "optional " } else { "" };
            content.push_str(&format!("{}parent \"{}\" {{\n", prefix, parent_name));
            let mut sorted_capabilities: Vec<_> = capabilities.iter().collect();
            sorted_capabilities.sort_unstable();
            for (service_name, banjo_name, _transport) in sorted_capabilities {
                if let Some(banjo_name) = banjo_name {
                    content.push_str(&format!("  fuchsia.BIND_PROTOCOL == {};\n", banjo_name));
                }
                if let Some(service_name) = service_name {
                    if let Some(rule) =
                        crate::workarounds::try_generate_init_step_bind_rule(&service_name)
                    {
                        content.push_str(&rule);
                    } else {
                        content.push_str(&format!("  fuchsia.Service == \"{}\";\n", service_name));
                    }
                }
            }
            if let Some(bind_rules) = parent_bind {
                let rules_str = generate_simple_bind_rules(&bind_rules)?;
                for line in rules_str.lines() {
                    if !line.trim().is_empty() {
                        content.push_str(&format!("  {}\n", line));
                    }
                }
            }
            content.push_str("}\n\n");
        }
    } else {
        let rules_str = generate_simple_bind_rules(bind)?;
        if rules_str.trim().is_empty() {
            content.push_str("true;\n");
        } else {
            content.push_str(&rules_str);
        }
    }
    let mut header = format!(
        r#"// Copyright {} The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// WARNING: THIS FILE IS GENERATED BY dmlc. DO NOT EDIT.

"#,
        year
    );
    if !is_composite && content.contains("fuchsia.Service") {
        header.push_str("using fuchsia;\n\n");
    }
    content.insert_str(0, &header);
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_bind_file_composite_grouping() {
        let bind = DmlBind {
            primary: Some(crate::parser::BindPrimary {
                node: "pdev".to_string(),
                compat: Some(Value::String("fuchsia,gpio-buttons".to_string())),
                ..Default::default()
            }),
            ..Default::default()
        };

        let additional_parents = vec![
            AdditionalParentInfo {
                parent_name: "gpio-init".to_string(),
                service_name: Some("fuchsia.gpio.Init".to_string()),
                banjo_name: None,
                transport: "Driver".to_string(),
                optional: false,
                bind: None,
            },
            AdditionalParentInfo {
                parent_name: "gpio-init".to_string(),
                service_name: Some("fuchsia.hardware.gpio.Service".to_string()),
                banjo_name: None,
                transport: "Driver".to_string(),
                optional: true,
                bind: None,
            },
            AdditionalParentInfo {
                parent_name: "pwm-init".to_string(),
                service_name: Some("fuchsia.pwm.Init".to_string()),
                banjo_name: None,
                transport: "Driver".to_string(),
                optional: true,
                bind: None,
            },
        ];

        let content = generate_bind_file("buttons", &bind, &additional_parents, "2026").unwrap();

        assert!(content.contains("using fuchsia;"));

        let expected_gpio_init = "parent \"gpio-init\" {\n  fuchsia.BIND_INIT_STEP == fuchsia.gpio.BIND_INIT_STEP.GPIO;\n  fuchsia.Service == \"fuchsia.hardware.gpio.Service\";\n}";
        let expected_pwm_init = "optional parent \"pwm-init\" {\n  fuchsia.BIND_INIT_STEP == fuchsia.pwm.BIND_INIT_STEP.PWM;\n}";

        assert!(
            content.contains(expected_gpio_init),
            "Expected:\n{}\n\nGot:\n{}",
            expected_gpio_init,
            content
        );
        assert!(
            content.contains(expected_pwm_init),
            "Expected:\n{}\n\nGot:\n{}",
            expected_pwm_init,
            content
        );
    }

    #[test]
    fn test_generate_bind_file_simple_one_of() {
        let bind = DmlBind {
            one_of: Some(vec![
                DmlBind { vid: Some(Value::Number(125.into())), ..Default::default() },
                DmlBind {
                    compat: Some(Value::String("fuchsia,my-compat".to_string())),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };

        let content = generate_bind_file("my_driver", &bind, &[], "2025").unwrap();

        assert!(content.contains("// Copyright 2025 The Fuchsia Authors. All rights reserved."));
        let expected = "if fuchsia.BIND_PLATFORM_DEV_VID == 125 {\n    true;\n} else if fuchsia.COMPATIBLE == \"fuchsia,my-compat\" {\n    true;\n} else {\n    false;\n}";
        assert!(content.contains(expected), "Expected:\n{}\n\nGot:\n{}", expected, content);
    }

    #[test]
    fn test_generate_bind_file_simple_one_of_array_compat() {
        let bind = DmlBind {
            one_of: Some(vec![
                DmlBind { vid: Some(Value::Number(125.into())), ..Default::default() },
                DmlBind {
                    compat: Some(Value::Array(vec![Value::String(
                        "fuchsia,my-compat".to_string(),
                    )])),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };

        let content = generate_bind_file("my_driver", &bind, &[], "2026").unwrap();

        assert!(content.contains("// Copyright 2026 The Fuchsia Authors. All rights reserved."));
        let expected = "if fuchsia.BIND_PLATFORM_DEV_VID == 125 {\n    true;\n} else {\n    accept fuchsia.COMPATIBLE {\n      \"fuchsia,my-compat\",\n    }\n}";
        assert!(content.contains(expected), "Expected:\n{}\n\nGot:\n{}", expected, content);
    }

    #[test]
    fn test_generate_bind_file_name_branching_and_fallback_else() {
        let mut rules_adc0 = std::collections::HashMap::new();
        rules_adc0.insert("fuchsia.NAME".to_string(), Value::String("adc-0".to_string()));

        let mut rules_chan = std::collections::HashMap::new();
        rules_chan.insert("fuchsia.adc.CHANNEL".to_string(), Value::Number(0.into()));

        let bind = DmlBind {
            one_of: Some(vec![
                DmlBind { rules: Some(rules_adc0), ..Default::default() },
                DmlBind { rules: Some(rules_chan), ..Default::default() },
            ]),
            ..Default::default()
        };

        let content = generate_bind_file("aml_thermistor", &bind, &[], "2026").unwrap();
        assert!(content.contains(
            "if fuchsia.NAME == \"adc-0\" {\n    true;\n} else {\n    fuchsia.adc.CHANNEL == 0;\n}"
        ));
    }

    #[test]
    fn test_generate_bind_file_composite_name_override() {
        let bind = DmlBind {
            composite_name: Some("custom_composite_name".to_string()),
            primary: Some(crate::parser::BindPrimary {
                node: "pdev".to_string(),
                compat: Some(Value::String("fuchsia,custom".to_string())),
                ..Default::default()
            }),
            ..Default::default()
        };

        let content = generate_bind_file("original_driver_name", &bind, &[], "2026").unwrap();
        assert!(content.contains("composite custom_composite_name;\n"));
    }

    #[test]
    fn test_generate_bind_file_non_equality_rule() {
        let mut rules = std::collections::HashMap::new();
        let mut neq_map = serde_json::Map::new();
        neq_map.insert("neq".to_string(), Value::Number(1.into()));
        rules.insert("fuchsia.BIND_COMPOSITE".to_string(), Value::Object(neq_map));

        let bind = DmlBind { rules: Some(rules), ..Default::default() };

        let content = generate_bind_file("serial", &bind, &[], "2026").unwrap();
        assert!(content.contains("fuchsia.BIND_COMPOSITE != 1;\n"));
    }

    #[test]
    fn test_generate_bind_file_empty_bind() {
        let bind = DmlBind::default();
        let content = generate_bind_file("driver_serve_fidl", &bind, &[], "2026").unwrap();
        assert!(content.contains("// Copyright 2026 The Fuchsia Authors. All rights reserved."));
        assert!(content.contains("true;\n"), "Expected true; in content:\n{}", content);
    }

    #[test]
    fn test_generate_bind_file_banjo_capability() {
        let bind = DmlBind {
            primary: Some(crate::parser::BindPrimary {
                node: "pdev".to_string(),
                banjo: Some("fuchsia.platform.BIND_PROTOCOL.DEVICE".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let additional = vec![AdditionalParentInfo {
            parent_name: "gpio".to_string(),
            service_name: None,
            banjo_name: Some("fuchsia.gpio.BIND_PROTOCOL.DEVICE".to_string()),
            transport: "Banjo".to_string(),
            optional: false,
            bind: None,
        }];

        let content = generate_bind_file("my_driver", &bind, &additional, "2026").unwrap();
        assert!(content.contains("primary parent \"pdev\" {\n  fuchsia.BIND_PROTOCOL == fuchsia.platform.BIND_PROTOCOL.DEVICE;\n}"));
        assert!(content.contains(
            "parent \"gpio\" {\n  fuchsia.BIND_PROTOCOL == fuchsia.gpio.BIND_PROTOCOL.DEVICE;\n}"
        ));
    }
}
