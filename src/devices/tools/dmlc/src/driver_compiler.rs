// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::bind_generator::{AdditionalParentInfo, generate_bind_file};
use crate::cml_generator::de_duplicate_use_entries;
use crate::parser::*;
use crate::{CompileDriverArgs, cpp_generator};
use anyhow::Context;
use serde_json::Value;
use std::path::Path;

pub fn compile_driver(args: &CompileDriverArgs, year: &str) -> Result<(), anyhow::Error> {
    let driver_dml = load_driver_dml(Path::new(&args.input_file))?;
    let driver_name = &driver_dml.name;

    // Find the schema for this driver
    let mut schema_def = None;
    for cap_val in &driver_dml.capabilities {
        let Some(obj) = cap_val.as_object() else {
            continue;
        };
        if !obj.contains_key("metadata") {
            continue;
        }
        let cap: DriverCapability = serde_json::from_value(cap_val.clone()).context(
            "Failed to deserialize DriverCapability from capability containing 'metadata' key",
        )?;
        let Some(meta) = cap.metadata else {
            continue;
        };
        if meta.schema.is_some() {
            schema_def = Some(meta);
            break;
        }
    }
    if let Some(schema_def) = schema_def {
        let parsed_schema = parse_json_schema(schema_def.schema.as_ref().unwrap(), &schema_def.id)?;
        let namespace = args.namespace.as_deref().unwrap_or(driver_name);
        let (h_code, cc_code) =
            cpp_generator::generate_cpp_parser(&parsed_schema, driver_name, namespace, year)?;
        if let Some(h_output) = &args.h_output {
            std::fs::write(h_output, h_code).context("Failed to write header file")?;
        }
        if let Some(cc_output) = &args.cc_output {
            std::fs::write(cc_output, cc_code).context("Failed to write source file")?;
        }
    } else {
        if let Some(h_output) = &args.h_output {
            std::fs::write(h_output, "").context("Failed to write stub header file")?;
        }
        if let Some(cc_output) = &args.cc_output {
            std::fs::write(cc_output, "").context("Failed to write stub source file")?;
        }
    }

    let mut bind_config = DmlBind::default();
    let mut has_explicit_bind_block = false;
    if let Some(obj) = driver_dml.program.as_object() {
        if let Some(bind_val) = obj.get("requirements").or_else(|| obj.get("bind")) {
            bind_config = serde_json::from_value(bind_val.clone())
                .context("Failed to parse structured 'requirements' block in DML program")?;
            has_explicit_bind_block = true;
        }
    }

    let mut additional_parents = Vec::new();
    let mut cleaned_use_entries = Vec::new();
    let mut primary_use_entry = None;

    for entry_val in &driver_dml.use_entries {
        let mut entry = entry_val.clone();
        if let Some(obj) = entry.as_object_mut() {
            let optional = obj
                .get("availability")
                .and_then(|v| v.as_str())
                .map(|s| s == "optional")
                .unwrap_or(false);
            let primary = obj.remove("primary").and_then(|v| v.as_bool()).unwrap_or(false);
            let transport_val = obj.remove("transport");
            let bind_val = obj.remove("requirements").or_else(|| obj.remove("bind"));
            let generate_bind_rule = obj
                .remove("generate_bind_rule")
                .or_else(|| obj.remove("generate_bind_rules"))
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let parent_val = obj.remove("name").or_else(|| obj.remove("instance_name"));
            if let Some(parent_val) = parent_val {
                if generate_bind_rule && let Some(parent_name) = parent_val.as_str() {
                    let service_name = obj
                        .get("service")
                        .or_else(|| obj.get("protocol"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());

                    if service_name.is_none() && bind_val.is_none() {
                        return Err(anyhow::anyhow!(
                            "use entry with 'name' or 'instance_name' must specify a 'service', 'protocol', or 'bind'"
                        ));
                    }

                    let transport = transport_val
                        .and_then(|t| t.as_str().map(|s| s.to_string()))
                        .unwrap_or_else(|| "Zircon".to_string());

                    let bind = bind_val
                        .map(|v| serde_json::from_value::<DmlBind>(v))
                        .transpose()
                        .context("Failed to parse 'bind' block in use entry")?;

                    if primary {
                        if has_explicit_bind_block {
                            return Err(anyhow::anyhow!(
                                "Cannot specify 'primary: true' in 'use' entry when 'program.requirements' is also present"
                            ));
                        }
                        if primary_use_entry.is_some() {
                            return Err(anyhow::anyhow!(
                                "Cannot specify 'primary: true' in multiple 'use' entries"
                            ));
                        }
                        primary_use_entry = Some((
                            parent_name.to_string(),
                            service_name.clone(),
                            obj.get("service").is_some(), // is_service
                            transport.clone(),
                            bind.clone(),
                        ));
                    }

                    additional_parents.push(AdditionalParentInfo {
                        parent_name: parent_name.to_string(),
                        service_name,
                        transport,
                        optional,
                        bind,
                    });
                }
            }
            let is_init_step = obj
                .get("service")
                .and_then(|v| v.as_str())
                .map(|s| crate::workarounds::try_generate_init_step_bind_rule(s).is_some())
                .unwrap_or(false);
            let is_bind_protocol = obj
                .get("protocol")
                .or_else(|| obj.get("service"))
                .and_then(|v| v.as_str())
                .map(|s| s.contains(".BIND_PROTOCOL."))
                .unwrap_or(false);
            let has_cml_fields = obj.iter().any(|(k, _)| {
                ![
                    "availability",
                    "bind",
                    "requirements",
                    "primary",
                    "name",
                    "instance_name",
                    "transport",
                    "generate_bind_rule",
                    "generate_bind_rules",
                ]
                .contains(&k.as_str())
            });
            if !is_init_step && !is_bind_protocol && has_cml_fields && !obj.is_empty() {
                cleaned_use_entries.push(entry);
            }
        } else {
            cleaned_use_entries.push(entry_val.clone());
        }
    }

    if let Some((parent_name, service_or_proto, is_service, transport, bind)) = primary_use_entry {
        let (service, protocol) = if let Some(sop) = service_or_proto {
            if is_service { (Some(sop), None) } else { (None, Some(sop)) }
        } else {
            (None, None)
        };

        let mut primary_bind = BindPrimary {
            node: parent_name,
            compat: None,
            vid: None,
            pid: None,
            did: None,
            protocol,
            service,
            transport: Some(transport),
            one_of: None,
        };

        if let Some(b) = bind {
            if b.composite_name.is_some() {
                bind_config.composite_name = b.composite_name.clone();
            }
            primary_bind.compat = b.compat;
            primary_bind.vid = b.vid;
            primary_bind.pid = b.pid;
            primary_bind.did = b.did;
            primary_bind.one_of = b.one_of;
        }

        bind_config.primary = Some(primary_bind);
    }

    let is_composite = bind_config.primary.is_some() || !additional_parents.is_empty();
    if is_composite && has_explicit_bind_block {
        return Err(anyhow::anyhow!(
            "Composite drivers cannot use 'program.requirements'. Move bind rules to the corresponding 'use' entry."
        ));
    }

    let final_use_entries = de_duplicate_use_entries(&cleaned_use_entries);

    // Generate CML if requested
    if let Some(cml_output) = &args.cml_output {
        let mut program_val = driver_dml.program.clone();
        if let Some(obj) = program_val.as_object_mut() {
            obj.remove("requirements");
            obj.remove("bind");
            obj.remove("bind_rules");
            if !obj.contains_key("runner") {
                obj.insert("runner".to_string(), Value::String("driver".to_string()));
            }
            if !obj.contains_key("binary") && !obj.contains_key("compat") {
                obj.insert(
                    "binary".to_string(),
                    Value::String(format!("driver/{}.so", driver_name)),
                );
            }
            if !obj.contains_key("bind") {
                obj.insert(
                    "bind".to_string(),
                    Value::String(format!("meta/bind/{}.bindbc", driver_name)),
                );
            }
        }

        let expose = driver_dml.expose.clone();
        let mut capabilities = driver_dml.capabilities.clone();

        // We need to clean up metadata capability from capabilities before writing to CML
        for cap in &mut capabilities {
            if let Some(obj) = cap.as_object_mut() {
                obj.remove("metadata");
            }
        }

        let mut includes =
            vec!["inspect/client.shard.cml".to_string(), "syslog/client.shard.cml".to_string()];
        includes.extend(driver_dml.include.clone());

        let mut cml = serde_json::json!({
            "include": includes,
            "program": program_val,
            "capabilities": capabilities,
            "use": final_use_entries,
            "expose": expose
        });
        if let Some(config) = &driver_dml.config {
            if let Some(cml_obj) = cml.as_object_mut() {
                cml_obj.insert("config".to_string(), config.clone());
            }
        }

        let cml_code = serde_json::to_string_pretty(&cml)?;
        let json5_code = crate::cml_generator::json_to_json5(&cml_code);
        let header = format!(
            r#"// Copyright {year} The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// WARNING: THIS FILE IS GENERATED BY dmlc. DO NOT EDIT.

"#,
        );
        let final_cml_code = format!("{}{}", header, json5_code);
        std::fs::write(cml_output, final_cml_code).context("Failed to write CML file")?;
    }

    // Generate Bind if requested
    if let Some(bind_output) = &args.bind_output {
        let bind_code = generate_bind_file(driver_name, &bind_config, &additional_parents, year)?;
        std::fs::write(bind_output, bind_code).context("Failed to write bind file")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_bind_rule_false() {
        let temp_dir = std::env::temp_dir().join("test_temp_generate_bind_rule_false");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let dml_content = r#"{
            name: "sample_driver",
            program: {
                bind: {
                    protocol: "fuchsia.platform.BIND_PROTOCOL.DEVICE",
                },
            },
            use: [
                {
                    service: "fuchsia.boot.metadata.PartitionMap",
                    name: "partition_map",
                    generate_bind_rule: false,
                },
            ],
        }"#;

        let dml_path = temp_dir.join("sample.dml");
        std::fs::write(&dml_path, dml_content).unwrap();

        let bind_path = temp_dir.join("sample.bind");
        let cml_path = temp_dir.join("sample.cml");

        let args = CompileDriverArgs {
            input_file: dml_path.to_str().unwrap().to_string(),
            h_output: None,
            cc_output: None,
            cml_output: Some(cml_path.to_str().unwrap().to_string()),
            bind_output: Some(bind_path.to_str().unwrap().to_string()),
            namespace: None,
        };

        compile_driver(&args, "2026").unwrap();

        let bind_content = std::fs::read_to_string(&bind_path).unwrap();
        assert!(!bind_content.contains("composite"));
        assert!(!bind_content.contains("partition_map"));
        assert!(
            bind_content
                .contains("fuchsia.BIND_PROTOCOL == fuchsia.platform.BIND_PROTOCOL.DEVICE;")
        );

        let cml_content = std::fs::read_to_string(&cml_path).unwrap();
        assert!(cml_content.contains("fuchsia.boot.metadata.PartitionMap"));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_generate_bind_rule_true_default() {
        let temp_dir = std::env::temp_dir().join("test_temp_generate_bind_rule_true");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let dml_content = r#"{
            name: "sample_composite",
            use: [
                {
                    service: "fuchsia.hardware.platform.device.Service",
                    name: "pdev",
                    primary: true,
                    bind: {
                        compat: "sample,hardware",
                    },
                },
                {
                    service: "fuchsia.hardware.gpio.Service",
                    name: "gpio-reset",
                },
            ],
        }"#;

        let dml_path = temp_dir.join("composite.dml");
        std::fs::write(&dml_path, dml_content).unwrap();

        let bind_path = temp_dir.join("composite.bind");

        let args = CompileDriverArgs {
            input_file: dml_path.to_str().unwrap().to_string(),
            h_output: None,
            cc_output: None,
            cml_output: None,
            bind_output: Some(bind_path.to_str().unwrap().to_string()),
            namespace: None,
        };

        compile_driver(&args, "2026").unwrap();

        let bind_content = std::fs::read_to_string(&bind_path).unwrap();
        assert!(bind_content.contains("composite sample_composite;"));
        assert!(bind_content.contains("primary parent \"pdev\""));
        assert!(bind_content.contains("parent \"gpio-reset\""));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_init_step_excluded_from_cml() {
        let temp_dir = std::env::temp_dir().join("test_temp_init_step_excluded");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let dml_content = r#"{
            name: "buttons_sample",
            use: [
                {
                    service: "fuchsia.hardware.platform.device.Service",
                    name: "pdev",
                    primary: true,
                    bind: {
                        compat: "sample,buttons",
                    },
                },
                {
                    service: "fuchsia.gpio.Init",
                    name: "gpio-init",
                    availability: "optional",
                },
                {
                    service: "fuchsia.hardware.gpio.Service",
                    name: "mic-mute",
                    availability: "optional",
                },
            ],
        }"#;

        let dml_path = temp_dir.join("sample.dml");
        std::fs::write(&dml_path, dml_content).unwrap();

        let bind_path = temp_dir.join("sample.bind");
        let cml_path = temp_dir.join("sample.cml");

        let args = CompileDriverArgs {
            input_file: dml_path.to_str().unwrap().to_string(),
            h_output: None,
            cc_output: None,
            cml_output: Some(cml_path.to_str().unwrap().to_string()),
            bind_output: Some(bind_path.to_str().unwrap().to_string()),
            namespace: None,
        };

        compile_driver(&args, "2026").unwrap();

        let bind_content = std::fs::read_to_string(&bind_path).unwrap();
        assert!(bind_content.contains("optional parent \"gpio-init\""));
        assert!(
            bind_content.contains("fuchsia.BIND_INIT_STEP == fuchsia.gpio.BIND_INIT_STEP.GPIO;")
        );

        let cml_content = std::fs::read_to_string(&cml_path).unwrap();
        assert!(!cml_content.contains("fuchsia.gpio.Init"));
        assert!(cml_content.contains("fuchsia.hardware.gpio.Service"));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_primary_use_entry_without_service_or_protocol() {
        let temp_dir = std::env::temp_dir().join("test_temp_primary_no_service");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let dml_content = r#"{
            name: "tas27xx_sample",
            use: [
                {
                    name: "tas27xx",
                    primary: true,
                    bind: {
                        compat: "ti,tas27xx",
                    },
                },
                {
                    service: "fuchsia.hardware.gpio.Service",
                    name: "gpio-reset",
                },
            ],
        }"#;

        let dml_path = temp_dir.join("sample.dml");
        std::fs::write(&dml_path, dml_content).unwrap();

        let bind_path = temp_dir.join("sample.bind");
        let cml_path = temp_dir.join("sample.cml");

        let args = CompileDriverArgs {
            input_file: dml_path.to_str().unwrap().to_string(),
            h_output: None,
            cc_output: None,
            cml_output: Some(cml_path.to_str().unwrap().to_string()),
            bind_output: Some(bind_path.to_str().unwrap().to_string()),
            namespace: None,
        };

        compile_driver(&args, "2026").unwrap();

        let bind_content = std::fs::read_to_string(&bind_path).unwrap();
        assert!(bind_content.contains("primary parent \"tas27xx\""));
        assert!(bind_content.contains("fuchsia.COMPATIBLE == \"ti,tas27xx\";"));
        assert!(!bind_content.contains("fuchsia.BIND_PROTOCOL == ;"));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
