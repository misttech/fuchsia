// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::parser::DmlBind;
use anyhow::anyhow;
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct AdditionalParentInfo {
    pub parent_name: String,
    pub service_name: String,
    pub transport: String,
    pub optional: bool,
    pub bind: Option<DmlBind>,
}

fn format_bind_val(val: &Value) -> Result<String, anyhow::Error> {
    match val {
        Value::Number(n) => Ok(n.to_string()),
        Value::String(s) => Ok(s.clone()),
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
) -> Result<String, anyhow::Error> {
    let mut content = String::new();
    if !exclude_protocol {
        if let Some(proto) = &bind.protocol {
            content.push_str(&format!("fuchsia.BIND_PROTOCOL == {};\n", proto));
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
    if let Some(rules) = &bind.rules {
        let mut sorted_rules: Vec<_> = rules.iter().collect();
        sorted_rules.sort_unstable_by_key(|&(key, _)| key);
        for (key, val) in sorted_rules {
            content.push_str(&format!("{} == {};\n", key, format_bind_val(val)?));
        }
    }
    Ok(content)
}

fn generate_simple_bind_statements(bind: &DmlBind) -> Result<String, anyhow::Error> {
    generate_simple_bind_statements_excluding(bind, false, false, false, false, false, false)
}

fn get_trigger(alt: &DmlBind) -> Result<Option<String>, anyhow::Error> {
    if alt.protocol.is_some() {
        Ok(Some(format!("fuchsia.BIND_PROTOCOL == {}", alt.protocol.as_ref().unwrap())))
    } else if alt.service.is_some() {
        let svc = alt.service.as_ref().unwrap();
        Ok(Some(format!("fuchsia.Service == \"{}\"", svc)))
    } else if alt.compat.is_some() && !alt.compat.as_ref().unwrap().is_array() {
        match alt.compat.as_ref().unwrap() {
            Value::String(s) => Ok(Some(format!("fuchsia.COMPATIBLE == \"{}\"", s))),
            _ => Ok(None),
        }
    } else if alt.vid.is_some() && !alt.vid.as_ref().unwrap().is_array() {
        Ok(Some(format!(
            "fuchsia.BIND_PLATFORM_DEV_VID == {}",
            format_bind_val(alt.vid.as_ref().unwrap())?
        )))
    } else if alt.pid.is_some() && !alt.pid.as_ref().unwrap().is_array() {
        Ok(Some(format!(
            "fuchsia.BIND_PLATFORM_DEV_PID == {}",
            format_bind_val(alt.pid.as_ref().unwrap())?
        )))
    } else if alt.did.is_some() && !alt.did.as_ref().unwrap().is_array() {
        Ok(Some(format!(
            "fuchsia.BIND_PLATFORM_DEV_DID == {}",
            format_bind_val(alt.did.as_ref().unwrap())?
        )))
    } else {
        Ok(None)
    }
}

fn generate_simple_bind_rules(bind: &DmlBind) -> Result<String, anyhow::Error> {
    let mut content = String::new();
    if let Some(alternatives) = &bind.one_of {
        let mut last_had_trigger = false;
        for (i, alt) in alternatives.iter().enumerate() {
            let trigger = get_trigger(alt)?;
            let has_trigger = trigger.is_some();

            if i == 0 {
                if has_trigger {
                    content.push_str(&format!("if {} {{\n", trigger.unwrap()));
                    last_had_trigger = true;
                } else {
                    return generate_simple_bind_statements(alt);
                }
            } else {
                if has_trigger {
                    content.push_str(&format!("}} else if {} {{\n", trigger.unwrap()));
                    last_had_trigger = true;
                } else {
                    content.push_str("} else {\n");
                    last_had_trigger = false;
                }
            }

            let statements = if has_trigger {
                let mut exclude_protocol = false;
                let mut exclude_service = false;
                let mut exclude_compat = false;
                let mut exclude_vid = false;
                let mut exclude_pid = false;
                let mut exclude_did = false;

                if alt.protocol.is_some() {
                    exclude_protocol = true;
                } else if alt.service.is_some() {
                    exclude_service = true;
                } else if alt.compat.is_some() && !alt.compat.as_ref().unwrap().is_array() {
                    exclude_compat = true;
                } else if alt.vid.is_some() && !alt.vid.as_ref().unwrap().is_array() {
                    exclude_vid = true;
                } else if alt.pid.is_some() && !alt.pid.as_ref().unwrap().is_array() {
                    exclude_pid = true;
                } else if alt.did.is_some() && !alt.did.as_ref().unwrap().is_array() {
                    exclude_did = true;
                }

                generate_simple_bind_statements_excluding(
                    alt,
                    exclude_protocol,
                    exclude_service,
                    exclude_compat,
                    exclude_vid,
                    exclude_pid,
                    exclude_did,
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
        let normalized_name = driver_name.replace("-", "_");
        content.push_str(&format!("composite {};\n\n", normalized_name));
        content.push_str("using fuchsia;\n\n");

        if let Some(primary) = &bind.primary {
            content.push_str(&format!("primary parent \"{}\" {{\n", primary.node));

            if let Some(alternatives) = &primary.one_of {
                for (i, alt) in alternatives.iter().enumerate() {
                    let is_last = i == alternatives.len() - 1;
                    if i == 0 {
                        content.push_str("  if ");
                    } else if is_last {
                        content.push_str("  } else {");
                    } else {
                        content.push_str("  } else if ");
                    }
                    let cond = if alt.compat.is_some() {
                        "fuchsia.BIND_PLATFORM_DEV_DID == fuchsia.platform.BIND_PLATFORM_DEV_DID.DEVICETREE".to_string()
                    } else if let Some(svc) = &alt.service {
                        format!("fuchsia.Service == \"{}\"", svc)
                    } else if let Some(proto) = &alt.protocol {
                        format!("fuchsia.BIND_PROTOCOL == {}", proto)
                    } else {
                        "true".to_string()
                    };
                    if !is_last {
                        content.push_str(&format!("{} {{\n", cond));
                    } else {
                        content.push_str("\n");
                    }

                    if is_last {
                        if alt.compat.is_some() {
                            content.push_str("    fuchsia.BIND_PLATFORM_DEV_DID == fuchsia.platform.BIND_PLATFORM_DEV_DID.DEVICETREE;\n");
                        } else if let Some(svc) = &alt.service {
                            content.push_str(&format!("    fuchsia.Service == \"{}\";\n", svc));
                        } else if let Some(proto) = &alt.protocol {
                            content.push_str(&format!("    fuchsia.BIND_PROTOCOL == {};\n", proto));
                        }
                    }

                    if let Some(compat) = &alt.compat {
                        match compat {
                            Value::Array(arr) => {
                                content.push_str("    accept fuchsia.COMPATIBLE {\n");
                                for v in arr {
                                    if let Some(s) = v.as_str() {
                                        content.push_str(&format!("      \"{}\",\n", s));
                                    }
                                }
                                content.push_str("    }\n");
                            }
                            Value::String(s) => {
                                content
                                    .push_str(&format!("    fuchsia.COMPATIBLE == \"{}\";\n", s));
                            }
                            _ => {}
                        }
                    }

                    if let Some(pci_class) = &alt.pci_class {
                        content
                            .push_str(&format!("    fuchsia.BIND_PCI_CLASS == {};\n", pci_class));
                    }
                    if let Some(pci_subclass) = &alt.pci_subclass {
                        content.push_str(&format!(
                            "    fuchsia.BIND_PCI_SUBCLASS == {};\n",
                            pci_subclass
                        ));
                    }
                    if let Some(pci_interface) = &alt.pci_interface {
                        content.push_str(&format!(
                            "    fuchsia.BIND_PCI_INTERFACE == {};\n",
                            pci_interface
                        ));
                    }

                    if let Some(vid) = &alt.vid {
                        match vid {
                            Value::Array(arr) => {
                                content.push_str("    accept fuchsia.BIND_PLATFORM_DEV_VID {\n");
                                for v in arr {
                                    content.push_str(&format!("      {},\n", format_bind_val(v)?));
                                }
                                content.push_str("    }\n");
                            }
                            _ => {
                                content.push_str(&format!(
                                    "    fuchsia.BIND_PLATFORM_DEV_VID == {};\n",
                                    format_bind_val(vid)?
                                ));
                            }
                        }
                    }
                    if let Some(pid) = &alt.pid {
                        match pid {
                            Value::Array(arr) => {
                                content.push_str("    accept fuchsia.BIND_PLATFORM_DEV_PID {\n");
                                for v in arr {
                                    content.push_str(&format!("      {},\n", format_bind_val(v)?));
                                }
                                content.push_str("    }\n");
                            }
                            _ => {
                                content.push_str(&format!(
                                    "    fuchsia.BIND_PLATFORM_DEV_PID == {};\n",
                                    format_bind_val(pid)?
                                ));
                            }
                        }
                    }
                    if let Some(did) = &alt.did {
                        match did {
                            Value::Array(arr) => {
                                content.push_str("    accept fuchsia.BIND_PLATFORM_DEV_DID {\n");
                                for v in arr {
                                    content.push_str(&format!("      {},\n", format_bind_val(v)?));
                                }
                                content.push_str("    }\n");
                            }
                            _ => {
                                content.push_str(&format!(
                                    "    fuchsia.BIND_PLATFORM_DEV_DID == {};\n",
                                    format_bind_val(did)?
                                ));
                            }
                        }
                    }
                }
                content.push_str("  }\n");
            } else {
                let mut rules = Vec::new();
                if let Some(vid) = &primary.vid {
                    rules.push(format!(
                        "fuchsia.BIND_PLATFORM_DEV_VID == {}",
                        format_bind_val(vid)?
                    ));
                }
                if let Some(pid) = &primary.pid {
                    rules.push(format!(
                        "fuchsia.BIND_PLATFORM_DEV_PID == {}",
                        format_bind_val(pid)?
                    ));
                }
                if let Some(did) = &primary.did {
                    rules.push(format!(
                        "fuchsia.BIND_PLATFORM_DEV_DID == {}",
                        format_bind_val(did)?
                    ));
                }
                if let Some(compat) = &primary.compat {
                    match compat {
                        Value::Array(arr) => {
                            let mut accept_rule = "accept fuchsia.COMPATIBLE {\n".to_string();
                            for v in arr {
                                if let Some(s) = v.as_str() {
                                    accept_rule.push_str(&format!("      \"{}\",\n", s));
                                }
                            }
                            accept_rule.push_str("    }");
                            rules.push(accept_rule);
                        }
                        Value::String(s) => {
                            rules.push(format!("fuchsia.COMPATIBLE == \"{}\"", s));
                        }
                        _ => {}
                    }
                }
                if let Some(proto) = &primary.protocol {
                    rules.push(format!("fuchsia.BIND_PROTOCOL == {}", proto));
                }
                if let Some(svc) = &primary.service {
                    rules.push(format!("fuchsia.Service == \"{}\"", svc));
                }
                if rules.is_empty() {
                    rules.push(
                        "fuchsia.BIND_PROTOCOL == fuchsia.platform.BIND_PROTOCOL.DEVICE"
                            .to_string(),
                    );
                }
                for r in rules {
                    content.push_str(&format!("  {};\n", r));
                }
            }
            content.push_str("}\n\n");
        }

        let primary_node_name = bind.primary.as_ref().map(|p| p.node.as_str());
        let mut grouped_parents = std::collections::BTreeMap::<
            String,
            (Vec<(String, String)>, bool, Option<DmlBind>),
        >::new();
        for parent in additional_parents {
            if Some(parent.parent_name.as_str()) == primary_node_name {
                continue;
            }
            let entry = grouped_parents
                .entry(parent.parent_name.clone())
                .or_insert_with(|| (Vec::new(), true, None));
            entry.0.push((parent.service_name.clone(), parent.transport.clone()));
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
            for (service_name, _transport) in sorted_capabilities {
                if let Some(rule) =
                    crate::workarounds::try_generate_init_step_bind_rule(&service_name)
                {
                    content.push_str(&rule);
                } else {
                    content.push_str(&format!("  fuchsia.Service == \"{}\";\n", service_name));
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
        content.push_str(&generate_simple_bind_rules(bind)?);
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
                service_name: "fuchsia.gpio.Init".to_string(),
                transport: "Driver".to_string(),
                optional: false,
                bind: None,
            },
            AdditionalParentInfo {
                parent_name: "gpio-init".to_string(),
                service_name: "fuchsia.hardware.gpio.Service".to_string(),
                transport: "Driver".to_string(),
                optional: true,
                bind: None,
            },
            AdditionalParentInfo {
                parent_name: "pwm-init".to_string(),
                service_name: "fuchsia.pwm.Init".to_string(),
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
}
