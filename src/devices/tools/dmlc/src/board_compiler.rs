// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::CompileBoardArgs;
use crate::bind_generator::generate_bind_file;
use crate::cml_generator::generate_board_cml_file;
use crate::parser::*;
use anyhow::Context;
use dml_config as fbdc;
use fidl_fuchsia_driver_metadata_common as fdr;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
struct LocalResourceEntry {
    node: String,
    constraint: Value,
    name: Option<String>,
    service: String,
}

/// Strips the leading '#' from a node/component reference name if present.
/// In DML/CML, child nodes are often referenced with a leading '#'.
fn strip_hash(s: &str) -> String {
    if let Some(stripped) = s.strip_prefix('#') { stripped.to_string() } else { s.to_string() }
}

fn normalize_config_id(id: &str) -> String {
    id.trim().trim_start_matches(['#', '/']).to_string()
}

/// Computes a deterministic, non-zero 32-bit FNV-1a hash identifying an offer from `provider` to
/// `to_name` with `name`.
pub fn compute_global_id(provider: &str, to_name: &str, name: &str) -> u32 {
    let mut hash: u32 = 0x811c9dc5;
    for b in format!("{}:{}:{}", provider, to_name, name).bytes() {
        hash ^= b as u32;
        hash = hash.wrapping_mul(0x01000193);
    }
    if hash == 0 { 1 } else { hash }
}

/// Returns the index of the device with the given name in the `devices` vector,
/// creating a new entry if it doesn't exist.
///
/// The returned index is arbitrary and used solely for local lookup and tracking
/// within the compiler. It does not correspond to the device's runtime ID (which is
/// assigned separately). The order of devices in the vector is determined by the
/// order they are processed, which matches their appearance in the DML input.
fn get_or_create_device_idx(
    devices: &mut Vec<fbdc::Device>,
    name: &str,
    url: Option<String>,
) -> usize {
    if let Some(idx) = devices.iter().position(|d| d.name.as_deref() == Some(name)) {
        if url.is_some() && devices[idx].url.is_none() {
            devices[idx].url = url;
        }
        return idx;
    }
    let dev = fbdc::Device { name: Some(name.to_string()), url, ..Default::default() };
    devices.push(dev);
    devices.len() - 1
}

/// Allocates a sequential, non-zero interrupt controller ID.
fn allocate_controller_id(next_id: &mut u32) -> u32 {
    let allocated = *next_id;
    *next_id = next_id.checked_add(1).expect("Exhausted interrupt controller IDs");
    allocated
}

fn resolve_single_irq_controller(
    irq_obj: &mut serde_json::Map<String, Value>,
    devices: &[fbdc::Device],
) -> Result<(), anyhow::Error> {
    if let Some(ctrl_val) = irq_obj.get("controller") {
        match ctrl_val {
            Value::String(ctrl_str) => {
                let target_node = strip_hash(ctrl_str);
                if target_node.is_empty() {
                    anyhow::bail!(
                        "Empty controller reference '{}' in interrupt constraint",
                        ctrl_str
                    );
                }
                let dev_idx = devices
                    .iter()
                    .position(|d| d.name.as_deref() == Some(&target_node))
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "Referenced interrupt controller device '{}' not found in board devices",
                            target_node
                        )
                    })?;
                let controller_id = devices[dev_idx].interrupt_controller_id.ok_or_else(|| {
                    anyhow::anyhow!(
                        "Referenced interrupt controller device '{}' does not have a 'fuchsia.hardware.interrupt.ControllerRegistryService' offer from 'parent'",
                        target_node
                    )
                })?;
                irq_obj.insert("controller".to_string(), Value::Number(controller_id.into()));
            }
            Value::Number(_) => {
                anyhow::bail!(
                    "Manual integer controller IDs are not supported in interrupt constraints, use string reference (e.g. \"#<node_name>\")"
                );
            }
            other => {
                anyhow::bail!(
                    "Invalid controller value in interrupt constraint: expected string reference (e.g. \"#<node_name>\"), got {:?}",
                    other
                );
            }
        }
    }
    Ok(())
}

fn resolve_interrupt_controllers(
    constraint_val: &mut Value,
    devices: &[fbdc::Device],
) -> Result<(), anyhow::Error> {
    let Some(obj) = constraint_val.as_object_mut() else {
        return Ok(());
    };

    if let Some(Value::Array(interrupts)) = obj.get_mut("interrupts") {
        for irq_val in interrupts.iter_mut() {
            if let Some(irq_obj) = irq_val.as_object_mut() {
                resolve_single_irq_controller(irq_obj, devices)?;
            }
        }
    }

    if let Some(Value::Object(irq_obj)) = obj.get_mut("interrupt") {
        resolve_single_irq_controller(irq_obj, devices)?;
    }

    Ok(())
}

fn flatten_value(
    val: &Value,
    prefix: &str,
    entries: &mut Vec<fdr::DictionaryEntry>,
) -> Result<(), anyhow::Error> {
    match val {
        Value::Null => {}
        Value::Bool(b) => {
            entries.push(fdr::DictionaryEntry {
                key: prefix.to_string(),
                value: fdr::DictionaryValue::Boolean(*b),
            });
        }
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                entries.push(fdr::DictionaryEntry {
                    key: prefix.to_string(),
                    value: fdr::DictionaryValue::Int64(i),
                });
            } else if let Some(u) = n.as_u64() {
                entries.push(fdr::DictionaryEntry {
                    key: prefix.to_string(),
                    value: fdr::DictionaryValue::Int64(u as i64),
                });
            } else {
                anyhow::bail!("Unsupported number type: {}", n);
            }
        }
        Value::String(s) => {
            entries.push(fdr::DictionaryEntry {
                key: prefix.to_string(),
                value: fdr::DictionaryValue::Str(s.clone()),
            });
        }
        Value::Array(arr) => {
            if arr.is_empty() {
                entries.push(fdr::DictionaryEntry {
                    key: format!("{}._count", prefix),
                    value: fdr::DictionaryValue::Int64(0),
                });
            } else {
                let first = &arr[0];
                match first {
                    Value::Number(_) => {
                        let mut vec = Vec::new();
                        for item in arr {
                            if let Value::Number(n) = item {
                                if let Some(i) = n.as_i64() {
                                    vec.push(i);
                                } else if let Some(u) = n.as_u64() {
                                    vec.push(u as i64);
                                } else {
                                    anyhow::bail!(
                                        "Unsupported number in metadata: {} (only 64-bit integers are supported)",
                                        n
                                    );
                                }
                            } else {
                                anyhow::bail!(
                                    "Heterogeneous array element: expected number, found: {:?}",
                                    item
                                );
                            }
                        }
                        entries.push(fdr::DictionaryEntry {
                            key: prefix.to_string(),
                            value: fdr::DictionaryValue::Int64Vec(vec),
                        });
                    }
                    Value::String(_) => {
                        let mut vec = Vec::new();
                        for item in arr {
                            if let Value::String(s) = item {
                                vec.push(s.clone());
                            } else {
                                anyhow::bail!(
                                    "Heterogeneous array element: expected string, found: {:?}",
                                    item
                                );
                            }
                        }
                        entries.push(fdr::DictionaryEntry {
                            key: prefix.to_string(),
                            value: fdr::DictionaryValue::StrVec(vec),
                        });
                    }
                    Value::Object(_) => {
                        entries.push(fdr::DictionaryEntry {
                            key: format!("{}._count", prefix),
                            value: fdr::DictionaryValue::Int64(arr.len() as i64),
                        });
                        for (idx, item) in arr.iter().enumerate() {
                            if !item.is_object() {
                                anyhow::bail!(
                                    "Heterogeneous array element: expected object, found: {:?}",
                                    item
                                );
                            }
                            let child_prefix = format!("{}.{}", prefix, idx);
                            flatten_value(item, &child_prefix, entries)?;
                        }
                    }
                    _ => anyhow::bail!("Unsupported array element type: {:?}", first),
                }
            }
        }
        Value::Object(obj) => {
            // Empty objects (e.g. `direct: {}`) are used to represent unit variants in FIDL
            // unions or empty marker structs. We emit them as a boolean `true` entry to indicate
            // presence.
            if obj.is_empty() && !prefix.is_empty() {
                entries.push(fdr::DictionaryEntry {
                    key: prefix.to_string(),
                    value: fdr::DictionaryValue::Boolean(true),
                });
            } else {
                // Non-empty objects are recursively flattened using dot-separated keys.
                for (key, child_val) in obj {
                    let child_prefix =
                        if prefix.is_empty() { key.clone() } else { format!("{}.{}", prefix, key) };
                    flatten_value(child_val, &child_prefix, entries)?;
                }
            }
        }
    }
    Ok(())
}

fn build_generic_metadata_value(
    mapping: &MetadataMapping,
    provider_id: u32,
    resources: &[LocalResourceEntry],
) -> Result<Value, anyhow::Error> {
    let mut root_obj = serde_json::Map::new();

    root_obj.insert(
        crate::workarounds::provider_id_key().to_string(),
        Value::Number(provider_id.into()),
    );

    for agg in &mapping.aggregations {
        root_obj.insert(agg.field.clone(), Value::Array(Vec::new()));
    }

    for agg in &mapping.aggregations {
        let mut arr = Vec::new();

        for res in resources {
            if res.service == agg.service {
                let mut val = res.constraint.clone();

                if let Some(obj) = val.as_object_mut() {
                    if !obj.contains_key("name") {
                        let name_to_insert = if !agg.use_node_name {
                            res.name.as_deref().unwrap_or(res.node.as_str())
                        } else {
                            res.node.as_str()
                        };
                        obj.insert("name".to_string(), Value::String(name_to_insert.to_string()));
                    }
                }
                arr.push(val);
            }
        }
        crate::workarounds::deduplicate_metadata_resources(&mapping.metadata_id, &mut arr);
        root_obj.insert(agg.field.clone(), Value::Array(arr));
    }

    Ok(Value::Object(root_obj))
}

fn resolve_offer_config_id(
    offer: &DmlOffer,
    to_name: &str,
    driver_config_map: &HashMap<String, String>,
) -> Option<String> {
    if let Some(config_id) = &offer.driver_config {
        return Some(normalize_config_id(config_id));
    }
    if let Some(meta) = &offer.metadata {
        if let Some(id_str) = meta.id() {
            return Some(normalize_config_id(id_str));
        }
        return driver_config_map.get(to_name).cloned();
    }
    if offer.service.is_none() && (!offer.extra.is_empty() || offer.properties.is_some()) {
        return driver_config_map.get(to_name).cloned();
    }
    if offer.properties.is_some() && offer.service.is_some() {
        return offer.service.as_ref().map(|s| normalize_config_id(s));
    }
    None
}

fn extract_offer_config_val(offer: &DmlOffer) -> Value {
    if let Some(meta) = &offer.metadata {
        if let Some(data) = meta.extra.get("data") {
            return data.clone();
        }
        if let Some(props) = meta.extra.get("properties") {
            return props.clone();
        }
        if !meta.extra.is_empty() {
            return Value::Object(meta.extra.clone().into_iter().collect());
        }
        return Value::Object(serde_json::Map::new());
    }
    if let Some(props) = &offer.properties {
        return props.clone();
    }
    if !offer.extra.is_empty() {
        return Value::Object(offer.extra.clone().into_iter().collect());
    }
    Value::Object(serde_json::Map::new())
}

fn process_offer_driver_config(
    offer: &DmlOffer,
    to_name: &str,
    driver_config_map: &HashMap<String, String>,
    device: &mut fbdc::Device,
) -> Result<(), anyhow::Error> {
    let Some(config_id) = resolve_offer_config_id(offer, to_name, driver_config_map) else {
        return Ok(());
    };

    let config_val = extract_offer_config_val(offer);
    let mut entries = Vec::new();
    flatten_value(&config_val, "", &mut entries)?;
    let dictionary = fdr::Dictionary { entries: Some(entries), ..Default::default() };
    let serialized_bytes =
        fidl::persist(&dictionary).context("Failed to serialize Dictionary to FIDL")?;

    let dev_metadata = device.metadata.get_or_insert_with(Vec::new);
    if let Some(existing) = dev_metadata.iter_mut().find(|m| m.id.as_deref() == Some(&config_id)) {
        existing.data = Some(serialized_bytes);
    } else {
        dev_metadata.push(fbdc::StaticMetadata {
            id: Some(config_id),
            data: Some(serialized_bytes),
            ..Default::default()
        });
    }

    Ok(())
}

fn process_service_offer(
    offer: &DmlOffer,
    to_name: &str,
    devices: &[fbdc::Device],
    iommu_map: &IommuMap,
    auto_incrementer: &mut crate::workarounds::AutoIncrementer,
    aggregates_list: &mut Vec<((String, String), Vec<LocalResourceEntry>)>,
) -> Result<(), anyhow::Error> {
    let Some(service_name) = &offer.service else {
        return Ok(());
    };

    let from = offer
        .from
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("'from' is missing in service offer for '{}'", to_name))?;

    let provider = if from == "parent" { "pdev".to_string() } else { strip_hash(from) };

    let mut constraint_val =
        offer.constraints.clone().unwrap_or_else(|| Value::Object(serde_json::Map::new()));

    auto_incrementer.apply(service_name, &provider, &mut constraint_val)?;
    resolve_interrupt_controllers(&mut constraint_val, devices)?;
    resolve_bti_iommus(&mut constraint_val, iommu_map)?;

    let global_id = compute_global_id(&provider, to_name, offer.name.as_deref().unwrap_or(""));
    if let Some(obj) = constraint_val.as_object_mut()
        && !obj.contains_key("id")
    {
        obj.insert("id".to_string(), Value::Number(global_id.into()));
    }

    let entry = LocalResourceEntry {
        node: to_name.to_string(),
        constraint: constraint_val,
        name: offer.name.clone(),
        service: service_name.clone(),
    };
    let key = (provider, service_name.clone());
    if let Some(existing) = aggregates_list.iter_mut().find(|(k, _)| *k == key) {
        existing.1.push(entry);
    } else {
        aggregates_list.push((key, vec![entry]));
    }

    Ok(())
}

/// Replaces the IOMMU references in the BTI constraints of `constraint_val`
/// with IOMMU IDs using `iommu_map`.
///
/// If a BTI constraint does not specify an IOMMU then it is assigned the IOMMU
/// ID 0.
///
/// ```json
/// // Constraints before.
/// {
///   "btis": [
///     {
///       "id": 0,
///       "iommu": "#foo"
///     },
///     {
///       "id": 1
///     }
///   ],
///   ...
/// }
///
/// // Constraints after.
/// {
///   "btis": [
///     {
///       "id": 0,
///
///       // Assuming the ID of IOMMU "foo" is 2.
///       "iommu_id": 2
///     },
///     {
///       "id": 1,
///       "iommu_id": 0
///     }
///   ],
///   ...
/// }
/// ```
///
/// Returns an error if:
/// - A BTI constraint defines `iommu_id`.
/// - An IOMMU reference does not start with `#`.
/// - A referenced IOMMU is not found in `iommu_map`.
fn resolve_bti_iommus(
    constraint_val: &mut Value,
    iommu_map: &IommuMap,
) -> Result<(), anyhow::Error> {
    let Some(obj) = constraint_val.as_object_mut() else {
        return Ok(());
    };

    if let Some(Value::Array(btis)) = obj.get_mut("btis") {
        for bti_val in btis.iter_mut() {
            if let Some(bti_obj) = bti_val.as_object_mut() {
                resolve_single_bti_iommu(bti_obj, iommu_map)?;
            }
        }
    }

    Ok(())
}

/// Replaces the IOMMU reference in the BTI constraint `bti_obj` with an IOMMU
/// ID using `iommu_map`.
///
/// If the BTI constraint does not specify an IOMMU then it is assigned the
/// IOMMU ID 0.
///
/// ```json
/// // BTI constraint before.
/// {
///   "id": 0,
///   "iommu": "#foo"
/// }
///
/// // BTI constraint after.
/// {
///   "id": 0,
///
///   // Assuming the ID of IOMMU "foo" is 2.
///   "iommu_id": 2
/// }
/// ```
///
/// Returns an error if:
/// - `bti_obj` defines `iommu_id`.
/// - The IOMMU reference does not start with `#`.
/// - A referenced IOMMU is not found in `iommu_map`.
fn resolve_single_bti_iommu(
    bti_obj: &mut serde_json::Map<String, Value>,
    iommu_map: &IommuMap,
) -> Result<(), anyhow::Error> {
    if bti_obj.contains_key("iommu_id") {
        anyhow::bail!(
            "Explicit \"iommu_id\" definition not allowed: Use `iommu: #<iommu-name>` instead"
        );
    }

    match bti_obj.remove("iommu") {
        Some(Value::String(iommu_reference)) => {
            let iommu_name = iommu_reference.strip_prefix('#').with_context(|| {
                format!(
                    "IOMMU reference {iommu_reference:?} in BTI constraint must start with '#' (e.g. \"#{iommu_reference}\")"
                )
            })?;
            let iommu_id = iommu_map.get_id(iommu_name).with_context(|| {
                format!("Referenced IOMMU {iommu_name:?} not found in declared iommus")
            })?;
            bti_obj.insert("iommu_id".to_string(), Value::Number(iommu_id.into()));
        }
        Some(other) => {
            anyhow::bail!(
                "Invalid iommu value in BTI constraint: expected string reference starting with '#', got {:?}",
                other
            );
        }
        None => {
            // Default to platform bus built-in stub IOMMU (ID 0).
            bti_obj.insert("iommu_id".to_string(), Value::Number(0.into()));
        }
    }

    Ok(())
}

pub fn compile_board(args: &CompileBoardArgs, year: &str) -> Result<(), anyhow::Error> {
    if args.out_dir.is_none()
        && (args.fidl_output.is_none() || args.bind_output.is_none() || args.cml_output.is_none())
    {
        return Err(anyhow::anyhow!(
            "Either out_dir or all of (fidl_output, bind_output, cml_output) must be specified"
        ));
    }

    let input_path = Path::new(&args.input_file);
    let out_dir = args.out_dir.as_deref().map(Path::new);

    if let Some(dir) = out_dir {
        std::fs::create_dir_all(dir)?;
    }
    if let Some(path) = &args.fidl_output {
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
    }
    if let Some(path) = &args.bind_output {
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
    }
    if let Some(path) = &args.cml_output {
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let board_dml = load_dml_file_root(input_path)?;

    // Build a mapping from driver DML names and child device names to their config IDs.
    // 1. Initial mapping: Load each driver DML file and map the driver's DML name to its primary
    // configuration ID
    // 2. Child device mapping: In the child processing loop below, extract the driver name from
    //    each child's component URL and insert a mapping from `child.name` to that `config_id`.
    // This enables subsequent offer processing to resolve the config ID for a target device
    // even when an offer does not specify an explicit `config` or metadata ID.
    let mut driver_config_map: HashMap<String, String> = HashMap::new();
    for dml_path in &args.driver_dml {
        let driver_dml = load_driver_dml(Path::new(dml_path))?;
        if let Some(config) = driver_dml.driver_configs.first() {
            driver_config_map.insert(driver_dml.name, config.driver_config.clone());
        }
    }

    let mut devices = Vec::new();
    let mut aggregates = Vec::new();

    // Process children
    for child in &board_dml.children {
        let idx = get_or_create_device_idx(&mut devices, &child.name, child.url.clone());
        devices[idx].compatible = child.compatible.clone();
        devices[idx].id = child.id;
        devices[idx].disabled = child.disabled;
        devices[idx].driver_host = child.driver_host.clone();
        // Map the child device name to its driver's config ID in `driver_config_map`.
        if let Some(url) = &child.url {
            // Extract driver name from component URL (e.g. "fuchsia-pkg://.../buttons#meta/buttons.cm" -> "buttons").
            let driver_name =
                url.split('#').next().and_then(|u| u.split('/').next_back()).unwrap_or("");
            if let Some(config_id) = driver_config_map.get(driver_name) {
                driver_config_map.insert(child.name.clone(), config_id.clone());
            }
        }

        if !child.metadata.is_empty() {
            let new_meta: Vec<_> = child
                .metadata
                .iter()
                .map(|m| fbdc::StaticMetadata {
                    id: Some(m.id.clone()),
                    data: m.data.clone(),
                    ..Default::default()
                })
                .collect();
            let existing = devices[idx].metadata.get_or_insert_with(Vec::new);
            for new_item in new_meta {
                if existing.iter().any(|m| m.id == new_item.id) {
                    anyhow::bail!(
                        "Device '{}' has duplicate metadata entry for ID '{}'",
                        child.name,
                        new_item.id.as_ref().unwrap()
                    );
                }
                existing.push(new_item);
            }
        }
    }

    let mut auto_incrementer =
        crate::workarounds::AutoIncrementer::new(&board_dml.metadata_mappings);
    let mut aggregates_list = Vec::<((String, String), Vec<LocalResourceEntry>)>::new();
    let mut next_controller_id: u32 = 1;

    // Allocate interrupt controller IDs for devices receiving ControllerRegistryService from parent
    for offer in &board_dml.offer {
        if offer.service.as_deref() == Some("fuchsia.hardware.interrupt.ControllerRegistryService")
        {
            let to_name = strip_hash(&offer.to);
            let from = offer.from.as_deref().ok_or_else(|| {
                anyhow::anyhow!("'from' is missing in service offer for '{}'", to_name)
            })?;
            if from != "parent" {
                anyhow::bail!(
                    "fuchsia.hardware.interrupt.ControllerRegistryService offer to '{}' must come from 'parent', found '{}'",
                    to_name,
                    from
                );
            }
            if offer.name.as_deref().map(|s| s.is_empty()).unwrap_or(true) {
                anyhow::bail!(
                    "'name' is missing in fuchsia.hardware.interrupt.ControllerRegistryService offer for '{}'",
                    to_name
                );
            }
            let dev_idx = devices
                .iter()
                .position(|d| d.name.as_deref() == Some(&to_name))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Target device '{}' for ControllerRegistryService offer not found in board devices",
                        to_name
                    )
                })?;
            if devices[dev_idx].interrupt_controller_id.is_none() {
                let allocated = allocate_controller_id(&mut next_controller_id);
                devices[dev_idx].interrupt_controller_id = Some(allocated);
            }
        }
    }

    let (iommus, iommu_map) = process_iommus(&board_dml.iommus)?;

    // Process offers
    for offer in &board_dml.offer {
        if offer.service.as_deref() == Some("fuchsia.hardware.interrupt.ControllerRegistryService")
        {
            continue;
        }
        let to_name = strip_hash(&offer.to);
        let dev_idx = get_or_create_device_idx(&mut devices, &to_name, None);
        process_offer_driver_config(offer, &to_name, &driver_config_map, &mut devices[dev_idx])?;
        process_service_offer(
            offer,
            &to_name,
            &devices,
            &iommu_map,
            &mut auto_incrementer,
            &mut aggregates_list,
        )?;
    }

    for ((provider, service), resources) in &aggregates_list {
        let mut unique_resources = Vec::new();
        for res in resources {
            if !unique_resources.iter().any(|r: &LocalResourceEntry| {
                r.node == res.node && r.constraint == res.constraint && r.name == res.name
            }) {
                unique_resources.push(res.clone());
            }
        }

        let mut fidl_resources = Vec::new();
        for res in unique_resources {
            let mut constraint_entries = Vec::new();
            flatten_value(&res.constraint, "", &mut constraint_entries)?;
            let constraint_dict =
                fdr::Dictionary { entries: Some(constraint_entries), ..Default::default() };
            fidl_resources.push(fbdc::ResourceEntry {
                node: Some(res.node.clone()),
                constraint: Some(constraint_dict),
                name: res.name.clone(),
                ..Default::default()
            });
        }

        aggregates.push(fbdc::AggregateEntry {
            provider: Some(provider.clone()),
            service: Some(service.clone()),
            resources: Some(fidl_resources),
            ..Default::default()
        });
    }

    // 4. Serialize aggregated metadata to FIDL and attach to provider devices
    let mut service_to_metadata_id = std::collections::HashMap::new();
    for mapping in &board_dml.metadata_mappings {
        for agg in &mapping.aggregations {
            service_to_metadata_id.insert(agg.service.clone(), mapping.metadata_id.clone());
        }
    }

    let mut metadata_map = BTreeMap::<(String, String), Vec<LocalResourceEntry>>::new();
    for ((provider, service), resources) in &aggregates_list {
        if service == "fuchsia.hardware.platform.device.Service" {
            continue;
        }
        if let Some(metadata_id) = service_to_metadata_id.get(service.as_str()) {
            metadata_map
                .entry((provider.clone(), metadata_id.to_string()))
                .or_default()
                .extend(resources.clone());
        }
    }

    for ((provider, metadata_id), resources) in metadata_map {
        let dev_idx = get_or_create_device_idx(&mut devices, &provider, None);
        let provider_id = devices[dev_idx].id.unwrap_or(0);

        let mapping =
            board_dml.metadata_mappings.iter().find(|m| m.metadata_id == metadata_id).ok_or_else(
                || {
                    anyhow::anyhow!(
                        "No metadata mapping found in board DML for metadata ID: {}",
                        metadata_id
                    )
                },
            )?;

        let root_val = build_generic_metadata_value(mapping, provider_id, &resources)?;
        let mut entries = Vec::new();
        flatten_value(&root_val, "", &mut entries)?;
        let dictionary = fdr::Dictionary { entries: Some(entries), ..Default::default() };
        let serialized_bytes =
            fidl::persist(&dictionary).context("Failed to serialize Dictionary to FIDL")?;

        // Attach to provider device
        let metadata = devices[dev_idx].metadata.get_or_insert_with(Vec::new);
        metadata.push(fbdc::StaticMetadata {
            id: Some(metadata_id.clone()),
            data: Some(serialized_bytes),
            ..Default::default()
        });
    }

    // 5. Serialize BoardConfig to FIDL and write to file
    let board_config = fbdc::BoardConfig {
        devices: Some(devices),
        aggregates: Some(aggregates),
        iommus: Some(iommus),
        ..Default::default()
    };
    let serialized_board_config =
        fidl::persist(&board_config).context("Failed to serialize BoardConfig to FIDL")?;

    let out_config_path = match &args.fidl_output {
        Some(path) => PathBuf::from(path),
        None => {
            out_dir.ok_or_else(|| anyhow::anyhow!("out_dir is missing"))?.join("board-config.fidl")
        }
    };
    let mut file = File::create(out_config_path)?;
    file.write_all(&serialized_board_config)?;

    // 6. Generate Bind and CML files for the board driver
    let board_name = board_dml.name.clone().unwrap_or_else(|| "board".to_string());

    let empty_bind = DmlBind::default();
    let bind_config = board_dml.program.bind.as_ref().unwrap_or(&empty_bind);
    let bind_code = generate_bind_file(&board_name, bind_config, &[], year)?;
    let cml_code = generate_board_cml_file(
        &board_name,
        &board_dml.program,
        &board_dml.use_entries,
        &board_dml.capabilities,
        &board_dml.expose,
    )?;

    let bind_output_path = match &args.bind_output {
        Some(path) => PathBuf::from(path),
        None => out_dir
            .ok_or_else(|| anyhow::anyhow!("out_dir is missing"))?
            .join(format!("{}-dml.bind", board_name)),
    };
    let cml_output_path = match &args.cml_output {
        Some(path) => PathBuf::from(path),
        None => out_dir
            .ok_or_else(|| anyhow::anyhow!("out_dir is missing"))?
            .join(format!("{}-dml.cml", board_name)),
    };

    std::fs::write(bind_output_path, bind_code).context("Failed to write generated bind file")?;
    let header = format!(
        r#"// Copyright {year} The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// WARNING: THIS FILE IS GENERATED BY dmlc. DO NOT EDIT.

"#,
    );
    let final_cml_code = format!("{}{}", header, cml_code);
    std::fs::write(cml_output_path, final_cml_code)
        .context("Failed to write generated cml file")?;
    Ok(())
}

/// Validates IDs of IOMMUs in `dml_iommus`, ensuring there are no duplicate
/// names or IDs. Generates unique IDs for IOMMUs in `dml_iommus` that do not
/// explicitly define an ID.
///
/// Returns:
/// - A `Vec<fbdc::Iommu>` for [`BoardConfig`](fbdc::BoardConfig).
/// - An [`IommuMap`] providing bidirectional lookup between IOMMU names and IDs.
fn process_iommus(dml_iommus: &[DmlIommu]) -> Result<(Vec<fbdc::Iommu>, IommuMap), anyhow::Error> {
    let mut iommu_map = IommuMap::with_capacity(dml_iommus.len());

    // Fill the map with the IOMMU IDs explicitly defined in the DML.
    for iommu in dml_iommus {
        if let Some(id) = iommu.id {
            if id == 0 {
                anyhow::bail!(
                    "IOMMU {} has ID 0 which is reserved for the platform bus stub IOMMU",
                    iommu.name
                );
            }
            iommu_map.insert(iommu.name.clone(), id)?;
        }
    }

    let mut next_iommu_id: u32 = 1;
    let mut iommus = Vec::<fbdc::Iommu>::with_capacity(dml_iommus.len());
    for iommu in dml_iommus {
        let iommu_id = if let Some(id) = iommu.id {
            id
        } else {
            // Generate a unique IOMMU ID.
            while iommu_map.contains_id(next_iommu_id) {
                next_iommu_id = next_iommu_id.checked_add(1).context("Exhausted IOMMU IDs")?;
            }
            let id = next_iommu_id;
            iommu_map.insert(iommu.name.clone(), id)?;
            next_iommu_id = next_iommu_id.checked_add(1).context("Exhausted IOMMU IDs")?;
            id
        };

        let iommu_type = match (&iommu.arm_smmu, &iommu.stub_iommu) {
            (Some(arm), None) => {
                fbdc::IommuType::ArmSmmu(fbdc::ArmSmmu { base_address: arm.base_address })
            }
            (None, Some(_)) => fbdc::IommuType::StubIommu(fbdc::StubIommu {}),
            (Some(_), Some(_)) => {
                anyhow::bail!(
                    "IOMMU '{}' cannot specify both 'arm_smmu' and 'stub_iommu'",
                    iommu.name
                );
            }
            (None, None) => {
                anyhow::bail!(
                    "IOMMU '{}' must specify either 'arm_smmu' or 'stub_iommu'",
                    iommu.name
                );
            }
        };

        iommus.push(fbdc::Iommu {
            name: Some(iommu.name.clone()),
            id: Some(iommu_id),
            iommu_type: Some(iommu_type),
            ..Default::default()
        });
    }

    Ok((iommus, iommu_map))
}

/// Bidirectional map between IOMMU names and IDs.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct IommuMap {
    name_to_id: HashMap<String, u32>,
    id_to_name: HashMap<u32, String>,
}

impl IommuMap {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            name_to_id: HashMap::with_capacity(capacity),
            id_to_name: HashMap::with_capacity(capacity),
        }
    }

    /// Inserts a name-to-id mapping, ensuring both name and ID are unique.
    pub fn insert(&mut self, name: String, id: u32) -> Result<(), anyhow::Error> {
        if name.starts_with('#') {
            // '#' is reserved for references. An IOMMU name is not a reference.
            anyhow::bail!("IOMMU name {:?} cannot start with '#'", name);
        }
        if let Some(other) = self.id_to_name.get(&id) {
            anyhow::bail!("IOMMUs {:?} and {:?} have the same ID {}", name, other, id);
        }
        if self.name_to_id.contains_key(&name) {
            anyhow::bail!("Multiple IOMMUs have the same name {:?}", name);
        }
        self.name_to_id.insert(name.clone(), id);
        self.id_to_name.insert(id, name);
        Ok(())
    }

    pub fn get_id(&self, name: &str) -> Option<u32> {
        self.name_to_id.get(name).copied()
    }

    #[allow(dead_code)]
    pub fn get_name(&self, id: u32) -> Option<&str> {
        self.id_to_name.get(&id).map(String::as_str)
    }

    pub fn contains_id(&self, id: u32) -> bool {
        self.id_to_name.contains_key(&id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn flatten(val: &Value) -> BTreeMap<String, fdr::DictionaryValue> {
        let mut entries = Vec::new();
        flatten_value(val, "", &mut entries).unwrap();
        entries.into_iter().map(|e| (e.key, e.value)).collect()
    }

    #[test]
    fn test_flatten_value_primitives() {
        let val = serde_json::json!({
            "a_bool": true,
            "an_int": 42,
            "a_str": "hello",
            "large_uint": 18446744073709551615u64
        });
        let flat = flatten(&val);
        assert_eq!(flat.len(), 4);
        assert_eq!(flat.get("a_bool"), Some(&fdr::DictionaryValue::Boolean(true)));
        assert_eq!(flat.get("an_int"), Some(&fdr::DictionaryValue::Int64(42)));
        assert_eq!(flat.get("a_str"), Some(&fdr::DictionaryValue::Str("hello".to_string())));
        assert_eq!(flat.get("large_uint"), Some(&fdr::DictionaryValue::Int64(-1)));
    }

    #[test]
    fn test_flatten_value_arrays() {
        let val = serde_json::json!({
            "int_arr": [1, 2, 3],
            "str_arr": ["x", "y"],
            "empty_arr": [],
            "large_uint_arr": [18446744073709551615u64]
        });
        let flat = flatten(&val);
        assert_eq!(flat.len(), 4);
        assert_eq!(flat.get("int_arr"), Some(&fdr::DictionaryValue::Int64Vec(vec![1, 2, 3])));
        assert_eq!(
            flat.get("str_arr"),
            Some(&fdr::DictionaryValue::StrVec(vec!["x".to_string(), "y".to_string()]))
        );
        assert_eq!(flat.get("empty_arr._count"), Some(&fdr::DictionaryValue::Int64(0)));
        assert_eq!(flat.get("large_uint_arr"), Some(&fdr::DictionaryValue::Int64Vec(vec![-1])));
    }

    #[test]
    fn test_flatten_value_nested() {
        let val = serde_json::json!({
            "outer": {
                "inner_bool": false,
                "nested_obj": {
                    "val": "leaf"
                }
            }
        });
        let flat = flatten(&val);
        assert_eq!(flat.len(), 2);
        assert_eq!(flat.get("outer.inner_bool"), Some(&fdr::DictionaryValue::Boolean(false)));
        assert_eq!(
            flat.get("outer.nested_obj.val"),
            Some(&fdr::DictionaryValue::Str("leaf".to_string()))
        );
    }

    #[test]
    fn test_flatten_value_obj_array() {
        let val = serde_json::json!({
            "arr": [
                { "id": 1, "name": "first" },
                { "id": 2, "name": "second" }
            ]
        });
        let flat = flatten(&val);
        assert_eq!(flat.len(), 5);
        assert_eq!(flat.get("arr._count"), Some(&fdr::DictionaryValue::Int64(2)));
        assert_eq!(flat.get("arr.0.id"), Some(&fdr::DictionaryValue::Int64(1)));
        assert_eq!(flat.get("arr.0.name"), Some(&fdr::DictionaryValue::Str("first".to_string())));
        assert_eq!(flat.get("arr.1.id"), Some(&fdr::DictionaryValue::Int64(2)));
        assert_eq!(flat.get("arr.1.name"), Some(&fdr::DictionaryValue::Str("second".to_string())));
    }

    #[test]
    fn test_flatten_value_heterogeneous_array_error() {
        let val = serde_json::json!({
            "arr": [1, "string"]
        });
        let mut entries = Vec::new();
        let res = flatten_value(&val, "", &mut entries);
        assert!(res.is_err());
        let err_msg = format!("{}", res.err().unwrap());
        assert!(
            err_msg.contains("Heterogeneous array element"),
            "Expected heterogeneous error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_flatten_value_float_error() {
        let val = serde_json::json!({
            "arr": [1.2, 2.3]
        });
        let mut entries = Vec::new();
        let res = flatten_value(&val, "", &mut entries);
        assert!(res.is_err());
        let err_msg = format!("{}", res.err().unwrap());
        assert!(
            err_msg.contains("Unsupported number in metadata"),
            "Expected float error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_flatten_value_heterogeneous_obj_array_error() {
        let val = serde_json::json!({
            "arr": [
                { "id": 1 },
                "not_an_object"
            ]
        });
        let mut entries = Vec::new();
        let res = flatten_value(&val, "", &mut entries);
        assert!(res.is_err());
        let err_msg = format!("{}", res.err().unwrap());
        assert!(
            err_msg.contains("Heterogeneous array element"),
            "Expected heterogeneous error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_duplicate_metadata_id_error() {
        let temp_dir = std::env::temp_dir().join("test_temp_dup_metadata");
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");
        let inc_file = temp_dir.join("inc.dml");

        // inc.dml defines a device with some metadata
        fs::write(
            &inc_file,
            r#"{
                "children": [
                    {
                        "name": "my_device",
                        "metadata": [
                            {
                                "id": "my_metadata_id",
                                "data": [1, 2, 3]
                            }
                        ]
                    }
                ]
            }"#,
        )
        .unwrap();

        // main.dml includes inc.dml and re-defines the same device with the same metadata ID
        fs::write(
            &main_file,
            r#"{
                "include": ["inc.dml"],
                "children": [
                    {
                        "name": "my_device",
                        "metadata": [
                            {
                                "id": "my_metadata_id",
                                "data": [4, 5, 6]
                            }
                        ]
                    }
                ]
            }"#,
        )
        .unwrap();

        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: Some(temp_dir.to_str().unwrap().to_string()),
            fidl_output: None,
            bind_output: None,
            cml_output: None,
            driver_dml: vec![],
        };

        let res = compile_board(&args, "2026");

        // Clean up
        let _ = fs::remove_file(&main_file);
        let _ = fs::remove_file(&inc_file);
        let _ = fs::remove_dir(&temp_dir);

        assert!(res.is_err());
        let err_msg = format!("{}", res.err().unwrap());
        assert!(
            err_msg.contains("has duplicate metadata entry for ID"),
            "Expected duplicate metadata ID error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_auto_increment_non_object_constraints_error() {
        let temp_dir = std::env::temp_dir().join("test_temp_autoincrement");
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");

        fs::write(
            &main_file,
            r#"{
                "name": "test_board",
                "metadata_mappings": [
                    {
                        "metadata_id": "fuchsia.hardware.gpio.Metadata",
                        "aggregations": [
                            {
                                "service": "fuchsia.hardware.gpio.Service",
                                "field": "pin",
                                "auto_increment": "pin"
                            }
                        ]
                    }
                ],
                "offer": [
                    {
                        "from": "parent",
                        "to": "gpio",
                        "service": "fuchsia.hardware.gpio.Service",
                        "constraints": "not_an_object"
                    }
                ]
            }"#,
        )
        .unwrap();

        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: Some(temp_dir.to_str().unwrap().to_string()),
            fidl_output: None,
            bind_output: None,
            cml_output: None,
            driver_dml: vec![],
        };

        let res = compile_board(&args, "2026");

        // Clean up
        let _ = fs::remove_file(&main_file);
        let _ = fs::remove_dir(&temp_dir);

        assert!(res.is_err());
        let err_msg = format!("{}", res.err().unwrap());
        assert!(
            err_msg.contains("Constraints must be a JSON object when auto-incrementing"),
            "Expected auto-increment constraints error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_large_uint64_round_trip() {
        let val = serde_json::json!({
            "large_uint": 18446744073709551615u64
        });
        let mut entries = Vec::new();
        flatten_value(&val, "", &mut entries).unwrap();
        let dict = fdr::Dictionary { entries: Some(entries), ..Default::default() };

        let retrieved = fbdc::get_uint64(&dict, "large_uint").unwrap();
        assert_eq!(retrieved, 18446744073709551615u64);
    }
    #[test]
    fn test_compute_global_id() {
        let id1 = compute_global_id("pdev", "to1", "gpio-pin-1");
        let id2 = compute_global_id("pdev", "to1", "gpio-pin-1");
        let id3 = compute_global_id("pdev", "to1", "gpio-pin-2");
        let id4 = compute_global_id("pdev", "to2", "gpio-pin-1");
        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
        assert_ne!(id1, id4);
        assert_ne!(id1, 0);
    }
    #[test]
    fn test_compile_board_minimal() {
        let temp_dir = std::env::temp_dir().join("test_temp_compile_board_minimal");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");
        fs::write(
            &main_file,
            r##"{
                "name": "test_board",
                "children": [
                    {
                        "name": "clock",
                        "url": "fuchsia-pkg://fuchsia.com/clock#meta/clock.cm",
                        "compatible": "test,clock"
                    }
                ],
                "offer": [
                    {
                        "from": "parent",
                        "to": "#clock",
                        "service": "fuchsia.hardware.platform.device.Service"
                    }
                ]
            }"##,
        )
        .unwrap();

        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: Some(temp_dir.to_str().unwrap().to_string()),
            fidl_output: None,
            bind_output: None,
            cml_output: None,
            driver_dml: vec![],
        };

        let res = compile_board(&args, "2026");
        assert!(res.is_ok(), "compile_board failed: {:?}", res.err());

        let fidl_path = temp_dir.join("board-config.fidl");
        let bind_path = temp_dir.join("test_board-dml.bind");
        let cml_path = temp_dir.join("test_board-dml.cml");

        assert!(fidl_path.exists(), "Expected board-config.fidl to exist");
        assert!(bind_path.exists(), "Expected test_board-dml.bind to exist");
        assert!(cml_path.exists(), "Expected test_board-dml.cml to exist");

        // Clean up
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_interrupt_controller_id() {
        let temp_dir = std::env::temp_dir().join("test_temp_intr_ctrl");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");
        fs::write(
            &main_file,
            r##"{
                "name": "test_board",
                "children": [
                    {
                        "name": "gia_node",
                        "compatible": "google,level-gia"
                    },
                    {
                        "name": "regular_node",
                        "compatible": "google,regular"
                    },
                    {
                        "name": "gia_node",
                        "metadata": [
                            { "id": "test.meta", "data": [1, 2] }
                        ]
                    }
                ],
                "offers": [
                    {
                        "name": "pdev",
                        "from": "parent",
                        "to": "#gia_node",
                        "service": "fuchsia.hardware.interrupt.ControllerRegistryService"
                    },
                    {
                        "from": "parent",
                        "to": "regular_node",
                        "service": "fuchsia.hardware.platform.device.Service",
                        "constraints": {
                            "interrupts": [
                                {
                                    "number": 10,
                                    "mode": "LevelHigh",
                                    "controller": "#gia_node"
                                },
                                {
                                    "number": 20,
                                    "mode": "EdgeHigh"
                                }
                            ]
                        }
                    }
                ]
            }"##,
        )
        .unwrap();

        let fidl_out = temp_dir.join("board-config.fidl");
        let bind_out = temp_dir.join("board.bind");
        let cml_out = temp_dir.join("board.cml");

        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: None,
            fidl_output: Some(fidl_out.to_str().unwrap().to_string()),
            bind_output: Some(bind_out.to_str().unwrap().to_string()),
            cml_output: Some(cml_out.to_str().unwrap().to_string()),
            driver_dml: vec![],
        };

        compile_board(&args, "2026").unwrap();

        let fidl_bytes = fs::read(&fidl_out).unwrap();
        let board_config: fbdc::BoardConfig = fidl::unpersist(&fidl_bytes).unwrap();
        let devices = board_config.devices.as_ref().unwrap();
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].name.as_deref(), Some("gia_node"));
        assert_eq!(devices[0].interrupt_controller_id, Some(1));
        assert_eq!(devices[1].name.as_deref(), Some("regular_node"));
        assert_eq!(devices[1].interrupt_controller_id, None);

        // Verify that IRQ constraints with controller IDs are parsed properly
        let pdev_dict = fbdc::pdev_constraints(&board_config, "regular_node").unwrap();
        let irqs = fbdc::irq_list(pdev_dict);
        assert_eq!(irqs.len(), 2);
        assert_eq!(irqs[0].number, 10);
        assert_eq!(irqs[0].mode.as_deref(), Some("LevelHigh"));
        assert_eq!(irqs[0].controller, Some(1));

        assert_eq!(irqs[1].number, 20);
        assert_eq!(irqs[1].mode.as_deref(), Some("EdgeHigh"));
        assert_eq!(irqs[1].controller, None);

        // Clean up
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_interrupt_controller_reference() {
        let temp_dir = std::env::temp_dir().join("test_temp_intr_ctrl_ref");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");
        fs::write(
            &main_file,
            r##"{
                "name": "test_board",
                "children": [
                    {
                        "name": "auto_gia_1",
                        "compatible": "google,level-gia"
                    },
                    {
                        "name": "auto_gia_2",
                        "compatible": "google,level-gia"
                    },
                    {
                        "name": "auto_gia_3",
                        "compatible": "google,level-gia"
                    },
                    {
                        "name": "unreferenced_node",
                        "compatible": "google,unreferenced"
                    },
                    {
                        "name": "client_node",
                        "compatible": "google,client"
                    }
                ],
                "offers": [
                    {
                        "name": "pdev",
                        "from": "parent",
                        "to": "#auto_gia_1",
                        "service": "fuchsia.hardware.interrupt.ControllerRegistryService"
                    },
                    {
                        "name": "pdev",
                        "from": "parent",
                        "to": "#auto_gia_2",
                        "service": "fuchsia.hardware.interrupt.ControllerRegistryService"
                    },
                    {
                        "name": "pdev",
                        "from": "parent",
                        "to": "#auto_gia_3",
                        "service": "fuchsia.hardware.interrupt.ControllerRegistryService"
                    },
                    {
                        "from": "parent",
                        "to": "client_node",
                        "service": "fuchsia.hardware.platform.device.Service",
                        "constraints": {
                            "interrupts": [
                                {
                                    "number": 10,
                                    "mode": "LevelHigh",
                                    "controller": "#auto_gia_1"
                                },
                                {
                                    "number": 11,
                                    "mode": "LevelHigh",
                                    "controller": "#auto_gia_1"
                                },
                                {
                                    "number": 20,
                                    "mode": "EdgeHigh",
                                    "controller": "#auto_gia_2"
                                },
                                {
                                    "number": 30,
                                    "mode": "LevelHigh",
                                    "controller": "auto_gia_3"
                                },
                                {
                                    "number": 40
                                }
                            ]
                        }
                    }
                ]
            }"##,
        )
        .unwrap();

        let fidl_out = temp_dir.join("board-config.fidl");
        let bind_out = temp_dir.join("board.bind");
        let cml_out = temp_dir.join("board.cml");

        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: None,
            fidl_output: Some(fidl_out.to_str().unwrap().to_string()),
            bind_output: Some(bind_out.to_str().unwrap().to_string()),
            cml_output: Some(cml_out.to_str().unwrap().to_string()),
            driver_dml: vec![],
        };

        compile_board(&args, "2026").unwrap();

        let fidl_bytes = fs::read(&fidl_out).unwrap();
        let board_config: fbdc::BoardConfig = fidl::unpersist(&fidl_bytes).unwrap();
        let devices = board_config.devices.as_ref().unwrap();

        let find_dev =
            |name: &str| devices.iter().find(|d| d.name.as_deref() == Some(name)).unwrap();
        assert_eq!(find_dev("auto_gia_1").interrupt_controller_id, Some(1));
        assert_eq!(find_dev("auto_gia_2").interrupt_controller_id, Some(2));
        assert_eq!(find_dev("auto_gia_3").interrupt_controller_id, Some(3));
        assert_eq!(find_dev("unreferenced_node").interrupt_controller_id, None);
        assert_eq!(find_dev("client_node").interrupt_controller_id, None);

        let pdev_dict = fbdc::pdev_constraints(&board_config, "client_node").unwrap();
        let irqs = fbdc::irq_list(pdev_dict);
        assert_eq!(irqs.len(), 5);

        // First reference to #auto_gia_1 gets allocated ID 1
        assert_eq!(irqs[0].number, 10);
        assert_eq!(irqs[0].controller, Some(1));

        // Second reference to #auto_gia_1 reuses ID 1
        assert_eq!(irqs[1].number, 11);
        assert_eq!(irqs[1].controller, Some(1));

        // Reference to auto_gia_2 gets allocated ID 2
        assert_eq!(irqs[2].number, 20);
        assert_eq!(irqs[2].controller, Some(2));

        // Reference to auto_gia_3 (without leading #) gets allocated ID 3
        assert_eq!(irqs[3].number, 30);
        assert_eq!(irqs[3].controller, Some(3));

        // No controller specified
        assert_eq!(irqs[4].number, 40);
        assert_eq!(irqs[4].controller, None);

        // Clean up
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_interrupt_controller_empty_reference_error() {
        let temp_dir = std::env::temp_dir().join("test_temp_intr_ctrl_empty_ref");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");
        fs::write(
            &main_file,
            r##"{
                "name": "test_board",
                "children": [
                    { "name": "node_a" }
                ],
                "offers": [
                    {
                        "from": "parent",
                        "to": "node_a",
                        "service": "fuchsia.hardware.platform.device.Service",
                        "constraints": {
                            "interrupts": [
                                { "number": 1, "controller": "#" }
                            ]
                        }
                    }
                ]
            }"##,
        )
        .unwrap();

        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: Some(temp_dir.to_str().unwrap().to_string()),
            fidl_output: None,
            bind_output: None,
            cml_output: None,
            driver_dml: vec![],
        };

        let res = compile_board(&args, "2026");
        let _ = fs::remove_dir_all(&temp_dir);

        assert!(res.is_err());
        let err_msg = format!("{}", res.err().unwrap());
        assert!(
            err_msg.contains("Empty controller reference"),
            "Expected empty controller reference error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_interrupt_controller_invalid_type_error() {
        let temp_dir = std::env::temp_dir().join("test_temp_intr_ctrl_invalid_type");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");
        fs::write(
            &main_file,
            r#"{
                "name": "test_board",
                "children": [
                    { "name": "node_a" }
                ],
                "offers": [
                    {
                        "from": "parent",
                        "to": "node_a",
                        "service": "fuchsia.hardware.platform.device.Service",
                        "constraints": {
                            "interrupts": [
                                { "number": 1, "controller": [1, 2] }
                            ]
                        }
                    }
                ]
            }"#,
        )
        .unwrap();

        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: Some(temp_dir.to_str().unwrap().to_string()),
            fidl_output: None,
            bind_output: None,
            cml_output: None,
            driver_dml: vec![],
        };

        let res = compile_board(&args, "2026");
        let _ = fs::remove_dir_all(&temp_dir);

        assert!(res.is_err());
        let err_msg = format!("{}", res.err().unwrap());
        assert!(
            err_msg.contains("Invalid controller value"),
            "Expected invalid controller value error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_interrupt_controller_nonexistent_reference_error() {
        let temp_dir = std::env::temp_dir().join("test_temp_intr_ctrl_nonexistent");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");
        fs::write(
            &main_file,
            r##"{
                "name": "test_board",
                "children": [
                    { "name": "node_a" }
                ],
                "offers": [
                    {
                        "from": "parent",
                        "to": "node_a",
                        "service": "fuchsia.hardware.platform.device.Service",
                        "constraints": {
                            "interrupts": [
                                { "number": 1, "controller": "#nonexistent_node" }
                            ]
                        }
                    }
                ]
            }"##,
        )
        .unwrap();

        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: Some(temp_dir.to_str().unwrap().to_string()),
            fidl_output: None,
            bind_output: None,
            cml_output: None,
            driver_dml: vec![],
        };

        let res = compile_board(&args, "2026");
        let _ = fs::remove_dir_all(&temp_dir);

        assert!(res.is_err());
        let err_msg = format!("{}", res.err().unwrap());
        assert!(
            err_msg.contains("Referenced interrupt controller device 'nonexistent_node' not found in board devices"),
            "Expected nonexistent device error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_interrupt_controller_raw_integer_rejected() {
        let temp_dir = std::env::temp_dir().join("test_temp_intr_ctrl_integer");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");
        fs::write(
            &main_file,
            r#"{
                "name": "test_board",
                "children": [
                    { "name": "node_a" }
                ],
                "offers": [
                    {
                        "from": "parent",
                        "to": "node_a",
                        "service": "fuchsia.hardware.platform.device.Service",
                        "constraints": {
                            "interrupts": [
                                { "number": 1, "controller": 42 }
                            ]
                        }
                    }
                ]
            }"#,
        )
        .unwrap();

        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: Some(temp_dir.to_str().unwrap().to_string()),
            fidl_output: None,
            bind_output: None,
            cml_output: None,
            driver_dml: vec![],
        };

        let res = compile_board(&args, "2026");
        let _ = fs::remove_dir_all(&temp_dir);

        assert!(res.is_err());
        let err_msg = format!("{}", res.err().unwrap());
        assert!(
            err_msg.contains("Manual integer controller IDs are not supported"),
            "Expected manual integer controller IDs not supported error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_interrupt_controller_zero_rejected() {
        let temp_dir = std::env::temp_dir().join("test_temp_intr_ctrl_zero");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");
        fs::write(
            &main_file,
            r#"{
                "name": "test_board",
                "children": [
                    { "name": "node_a" }
                ],
                "offers": [
                    {
                        "from": "parent",
                        "to": "node_a",
                        "service": "fuchsia.hardware.platform.device.Service",
                        "constraints": {
                            "interrupts": [
                                { "number": 1, "controller": 0 }
                            ]
                        }
                    }
                ]
            }"#,
        )
        .unwrap();

        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: Some(temp_dir.to_str().unwrap().to_string()),
            fidl_output: None,
            bind_output: None,
            cml_output: None,
            driver_dml: vec![],
        };

        let res = compile_board(&args, "2026");
        let _ = fs::remove_dir_all(&temp_dir);

        assert!(res.is_err());
        let err_msg = format!("{}", res.err().unwrap());
        assert!(
            err_msg.contains("Manual integer controller IDs are not supported"),
            "Expected manual integer controller IDs not supported error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_interrupt_controller_empty_string_rejected() {
        let temp_dir = std::env::temp_dir().join("test_temp_intr_ctrl_empty_str");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");
        fs::write(
            &main_file,
            r#"{
                "name": "test_board",
                "children": [
                    { "name": "node_a" }
                ],
                "offers": [
                    {
                        "from": "parent",
                        "to": "node_a",
                        "service": "fuchsia.hardware.platform.device.Service",
                        "constraints": {
                            "interrupts": [
                                { "number": 1, "controller": "" }
                            ]
                        }
                    }
                ]
            }"#,
        )
        .unwrap();

        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: Some(temp_dir.to_str().unwrap().to_string()),
            fidl_output: None,
            bind_output: None,
            cml_output: None,
            driver_dml: vec![],
        };

        let res = compile_board(&args, "2026");
        let _ = fs::remove_dir_all(&temp_dir);

        assert!(res.is_err());
        let err_msg = format!("{}", res.err().unwrap());
        assert!(
            err_msg.contains("Empty controller reference"),
            "Expected empty controller reference error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_interrupt_controller_singular_object() {
        let temp_dir = std::env::temp_dir().join("test_temp_intr_ctrl_singular");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");
        fs::write(
            &main_file,
            r##"{
                "name": "test_board",
                "children": [
                    { "name": "ctrl_node" },
                    { "name": "client_node" }
                ],
                "offers": [
                    {
                        "name": "pdev",
                        "from": "parent",
                        "to": "#ctrl_node",
                        "service": "fuchsia.hardware.interrupt.ControllerRegistryService"
                    },
                    {
                        "from": "parent",
                        "to": "client_node",
                        "service": "fuchsia.hardware.platform.device.Service",
                        "constraints": {
                            "interrupt": {
                                "number": 42,
                                "controller": "#ctrl_node"
                            }
                        }
                    }
                ]
            }"##,
        )
        .unwrap();

        let fidl_out = temp_dir.join("board-config.fidl");
        let bind_out = temp_dir.join("board.bind");
        let cml_out = temp_dir.join("board.cml");
        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: None,
            fidl_output: Some(fidl_out.to_str().unwrap().to_string()),
            bind_output: Some(bind_out.to_str().unwrap().to_string()),
            cml_output: Some(cml_out.to_str().unwrap().to_string()),
            driver_dml: vec![],
        };

        compile_board(&args, "2026").unwrap();

        let fidl_bytes = fs::read(&fidl_out).unwrap();
        let board_config: fbdc::BoardConfig = fidl::unpersist(&fidl_bytes).unwrap();
        let devices = board_config.devices.as_ref().unwrap();

        let ctrl = devices.iter().find(|d| d.name.as_deref() == Some("ctrl_node")).unwrap();
        assert_eq!(ctrl.interrupt_controller_id, Some(1));

        let client = devices.iter().find(|d| d.name.as_deref() == Some("client_node")).unwrap();
        assert_eq!(client.interrupt_controller_id, None);

        let pdev_dict = fbdc::pdev_constraints(&board_config, "client_node").unwrap();
        let irq_ctrl = fbdc::get_uint32(pdev_dict, "interrupt.controller");
        assert_eq!(irq_ctrl, Some(1));
        let irq_num = fbdc::get_uint32(pdev_dict, "interrupt.number");
        assert_eq!(irq_num, Some(42));

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_interrupt_controller_multiple_offers_same_controller() {
        let temp_dir = std::env::temp_dir().join("test_temp_intr_ctrl_multi_offer");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");
        fs::write(
            &main_file,
            r##"{
                "name": "test_board",
                "children": [
                    { "name": "controller_x" },
                    { "name": "controller_y" },
                    { "name": "client_a" },
                    { "name": "client_b" }
                ],
                "offers": [
                    {
                        "name": "pdev",
                        "from": "parent",
                        "to": "#controller_x",
                        "service": "fuchsia.hardware.interrupt.ControllerRegistryService"
                    },
                    {
                        "name": "pdev",
                        "from": "parent",
                        "to": "#controller_y",
                        "service": "fuchsia.hardware.interrupt.ControllerRegistryService"
                    },
                    {
                        "from": "parent",
                        "to": "client_a",
                        "service": "fuchsia.hardware.platform.device.Service",
                        "constraints": {
                            "interrupts": [
                                { "number": 1, "controller": "#controller_x" }
                            ]
                        }
                    },
                    {
                        "from": "parent",
                        "to": "client_b",
                        "service": "fuchsia.hardware.platform.device.Service",
                        "constraints": {
                            "interrupts": [
                                { "number": 2, "controller": "#controller_y" },
                                { "number": 3, "controller": "#controller_x" }
                            ]
                        }
                    }
                ]
            }"##,
        )
        .unwrap();

        let fidl_out = temp_dir.join("board-config.fidl");
        let bind_out = temp_dir.join("board.bind");
        let cml_out = temp_dir.join("board.cml");

        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: None,
            fidl_output: Some(fidl_out.to_str().unwrap().to_string()),
            bind_output: Some(bind_out.to_str().unwrap().to_string()),
            cml_output: Some(cml_out.to_str().unwrap().to_string()),
            driver_dml: vec![],
        };

        compile_board(&args, "2026").unwrap();

        let fidl_bytes = fs::read(&fidl_out).unwrap();
        let board_config: fbdc::BoardConfig = fidl::unpersist(&fidl_bytes).unwrap();
        let devices = board_config.devices.as_ref().unwrap();

        let find_dev =
            |name: &str| devices.iter().find(|d| d.name.as_deref() == Some(name)).unwrap();
        // controller_x is offered first -> ID 1
        assert_eq!(find_dev("controller_x").interrupt_controller_id, Some(1));
        // controller_y is offered next -> ID 2
        assert_eq!(find_dev("controller_y").interrupt_controller_id, Some(2));

        let pdev_a = fbdc::pdev_constraints(&board_config, "client_a").unwrap();
        let irqs_a = fbdc::irq_list(pdev_a);
        assert_eq!(irqs_a.len(), 1);
        assert_eq!(irqs_a[0].controller, Some(1));

        let pdev_b = fbdc::pdev_constraints(&board_config, "client_b").unwrap();
        let irqs_b = fbdc::irq_list(pdev_b);
        assert_eq!(irqs_b.len(), 2);
        assert_eq!(irqs_b[0].controller, Some(2));
        assert_eq!(irqs_b[1].controller, Some(1));

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_interrupt_controller_missing_registry_offer_error() {
        let temp_dir = std::env::temp_dir().join("test_temp_intr_ctrl_missing_registry");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");
        fs::write(
            &main_file,
            r##"{
                "name": "test_board",
                "children": [
                    { "name": "ctrl_node" },
                    { "name": "client_node" }
                ],
                "offers": [
                    {
                        "from": "parent",
                        "to": "client_node",
                        "service": "fuchsia.hardware.platform.device.Service",
                        "constraints": {
                            "interrupts": [
                                { "number": 1, "controller": "#ctrl_node" }
                            ]
                        }
                    }
                ]
            }"##,
        )
        .unwrap();

        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: Some(temp_dir.to_str().unwrap().to_string()),
            fidl_output: None,
            bind_output: None,
            cml_output: None,
            driver_dml: vec![],
        };

        let res = compile_board(&args, "2026");
        let _ = fs::remove_dir_all(&temp_dir);

        assert!(res.is_err());
        let err_msg = format!("{}", res.err().unwrap());
        assert!(
            err_msg.contains("does not have a 'fuchsia.hardware.interrupt.ControllerRegistryService' offer from 'parent'"),
            "Expected missing ControllerRegistryService offer error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_interrupt_controller_registry_offer_wrong_source_error() {
        let temp_dir = std::env::temp_dir().join("test_temp_intr_ctrl_wrong_source");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");
        fs::write(
            &main_file,
            r##"{
                "name": "test_board",
                "children": [
                    { "name": "other_node" },
                    { "name": "ctrl_node" }
                ],
                "offers": [
                    {
                        "name": "pdev",
                        "from": "#other_node",
                        "to": "#ctrl_node",
                        "service": "fuchsia.hardware.interrupt.ControllerRegistryService"
                    }
                ]
            }"##,
        )
        .unwrap();

        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: Some(temp_dir.to_str().unwrap().to_string()),
            fidl_output: None,
            bind_output: None,
            cml_output: None,
            driver_dml: vec![],
        };

        let res = compile_board(&args, "2026");
        let _ = fs::remove_dir_all(&temp_dir);

        assert!(res.is_err());
        let err_msg = format!("{}", res.err().unwrap());
        assert!(
            err_msg.contains("must come from 'parent'"),
            "Expected wrong source error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_interrupt_controller_registry_offer_missing_name_error() {
        let temp_dir = std::env::temp_dir().join("test_temp_intr_ctrl_missing_name");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");
        fs::write(
            &main_file,
            r##"{
                "name": "test_board",
                "children": [
                    { "name": "ctrl_node" }
                ],
                "offers": [
                    {
                        "from": "parent",
                        "to": "#ctrl_node",
                        "service": "fuchsia.hardware.interrupt.ControllerRegistryService"
                    }
                ]
            }"##,
        )
        .unwrap();

        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: Some(temp_dir.to_str().unwrap().to_string()),
            fidl_output: None,
            bind_output: None,
            cml_output: None,
            driver_dml: vec![],
        };

        let res = compile_board(&args, "2026");
        let _ = fs::remove_dir_all(&temp_dir);

        assert!(res.is_err());
        let err_msg = format!("{}", res.err().unwrap());
        assert!(
            err_msg.contains(
                "'name' is missing in fuchsia.hardware.interrupt.ControllerRegistryService offer"
            ),
            "Expected missing name error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_service_offer_name_and_constraint_name() {
        let temp_dir = std::env::temp_dir().join("test_temp_offer_name_constraint");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");
        fs::write(
            &main_file,
            r##"{
                "name": "test_board",
                "children": [
                    {
                        "name": "pmic_node",
                        "compatible": "fuchsia,test-pmic"
                    },
                    {
                        "name": "dpu_node",
                        "compatible": "fuchsia,test-display"
                    }
                ],
                "offers": [
                    {
                        "from": "#pmic_node",
                        "to": "#dpu_node",
                        "service": "fuchsia.hardware.vreg.Service",
                        "name": "test-vreg",
                        "constraints": {
                            "name": "regulator_1"
                        }
                    }
                ]
            }"##,
        )
        .unwrap();

        let fidl_out = temp_dir.join("board-config.fidl");
        let bind_out = temp_dir.join("board.bind");
        let cml_out = temp_dir.join("board.cml");

        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: None,
            fidl_output: Some(fidl_out.to_str().unwrap().to_string()),
            bind_output: Some(bind_out.to_str().unwrap().to_string()),
            cml_output: Some(cml_out.to_str().unwrap().to_string()),
            driver_dml: vec![],
        };

        compile_board(&args, "2026").unwrap();

        let fidl_bytes = fs::read(&fidl_out).unwrap();
        let board_config: fbdc::BoardConfig = fidl::unpersist(&fidl_bytes).unwrap();
        let aggregates = board_config.aggregates.as_ref().unwrap();
        assert_eq!(aggregates.len(), 1);
        assert_eq!(aggregates[0].provider.as_deref(), Some("pmic_node"));
        assert_eq!(aggregates[0].service.as_deref(), Some("fuchsia.hardware.vreg.Service"));

        let resources = aggregates[0].resources.as_ref().unwrap();
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].name.as_deref(), Some("test-vreg"));
        assert_eq!(resources[0].node.as_deref(), Some("dpu_node"));

        let constraint_entries =
            resources[0].constraint.as_ref().unwrap().entries.as_ref().unwrap();
        let name_entry = constraint_entries.iter().find(|e| e.key == "name").unwrap();
        assert_eq!(name_entry.value, fdr::DictionaryValue::Str("regulator_1".to_string()));

        let expected_id = compute_global_id("pmic_node", "dpu_node", "test-vreg") as i64;
        let id_entry = constraint_entries.iter().find(|e| e.key == "id").unwrap();
        assert_eq!(id_entry.value, fdr::DictionaryValue::Int64(expected_id));

        // Clean up
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_gpio_service_offer_id() {
        let temp_dir = std::env::temp_dir().join("test_temp_gpio_offer_global_id");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");
        fs::write(
            &main_file,
            r##"{
                "name": "test_board",
                "children": [
                    {
                        "name": "gpio_controller",
                        "compatible": "fuchsia,test-gpio"
                    },
                    {
                        "name": "buttons_node",
                        "compatible": "fuchsia,test-buttons"
                    }
                ],
                "offers": [
                    {
                        "from": "#gpio_controller",
                        "to": "#buttons_node",
                        "service": "fuchsia.hardware.gpio.Service",
                        "name": "test-pin",
                        "constraints": {
                            "pin": 51,
                            "name": "test-pin"
                        }
                    }
                ]
            }"##,
        )
        .unwrap();

        let fidl_out = temp_dir.join("board-config.fidl");
        let bind_out = temp_dir.join("board.bind");
        let cml_out = temp_dir.join("board.cml");

        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: None,
            fidl_output: Some(fidl_out.to_str().unwrap().to_string()),
            bind_output: Some(bind_out.to_str().unwrap().to_string()),
            cml_output: Some(cml_out.to_str().unwrap().to_string()),
            driver_dml: vec![],
        };

        compile_board(&args, "2026").unwrap();

        let fidl_bytes = fs::read(&fidl_out).unwrap();
        let board_config: fbdc::BoardConfig = fidl::unpersist(&fidl_bytes).unwrap();
        let aggregates = board_config.aggregates.as_ref().unwrap();
        assert_eq!(aggregates.len(), 1);
        assert_eq!(aggregates[0].provider.as_deref(), Some("gpio_controller"));
        assert_eq!(aggregates[0].service.as_deref(), Some("fuchsia.hardware.gpio.Service"));

        let resources = aggregates[0].resources.as_ref().unwrap();
        assert_eq!(resources.len(), 1);

        let constraint_entries =
            resources[0].constraint.as_ref().unwrap().entries.as_ref().unwrap();

        let expected_id = compute_global_id("gpio_controller", "buttons_node", "test-pin") as i64;
        let id_entry = constraint_entries.iter().find(|e| e.key == "id").unwrap();
        assert_eq!(id_entry.value, fdr::DictionaryValue::Int64(expected_id));

        // Clean up
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_service_offer_preserves_explicit_id() {
        let temp_dir = std::env::temp_dir().join("test_temp_service_offer_preserves_explicit_id");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");
        fs::write(
            &main_file,
            r##"{
                "name": "test_board",
                "children": [
                    {
                        "name": "pmic_node",
                        "compatible": "fuchsia,test-pmic"
                    },
                    {
                        "name": "dpu_node",
                        "compatible": "fuchsia,test-display"
                    }
                ],
                "offers": [
                    {
                        "from": "#pmic_node",
                        "to": "#dpu_node",
                        "service": "fuchsia.hardware.vreg.Service",
                        "name": "test-vreg",
                        "constraints": {
                            "name": "regulator_1",
                            "id": 999
                        }
                    }
                ]
            }"##,
        )
        .unwrap();

        let fidl_out = temp_dir.join("board-config.fidl");
        let bind_out = temp_dir.join("board.bind");
        let cml_out = temp_dir.join("board.cml");

        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: None,
            fidl_output: Some(fidl_out.to_str().unwrap().to_string()),
            bind_output: Some(bind_out.to_str().unwrap().to_string()),
            cml_output: Some(cml_out.to_str().unwrap().to_string()),
            driver_dml: vec![],
        };

        compile_board(&args, "2026").unwrap();

        let fidl_bytes = fs::read(&fidl_out).unwrap();
        let board_config: fbdc::BoardConfig = fidl::unpersist(&fidl_bytes).unwrap();
        let aggregates = board_config.aggregates.as_ref().unwrap();
        assert_eq!(aggregates.len(), 1);

        let resources = aggregates[0].resources.as_ref().unwrap();
        assert_eq!(resources.len(), 1);

        let constraint_entries =
            resources[0].constraint.as_ref().unwrap().entries.as_ref().unwrap();

        let id_entry = constraint_entries.iter().find(|e| e.key == "id").unwrap();
        assert_eq!(id_entry.value, fdr::DictionaryValue::Int64(999));

        // Clean up
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_duplicate_gpio_pin_enforces_single_consumer_in_metadata() {
        let temp_dir = std::env::temp_dir().join("test_temp_duplicate_gpio_pin_single_consumer");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");
        fs::write(
            &main_file,
            r##"{
                "name": "test_board",
                "children": [
                    {
                        "name": "gpio_controller",
                        "compatible": "fuchsia,test-gpio"
                    },
                    {
                        "name": "consumer_a",
                        "compatible": "fuchsia,test-a"
                    },
                    {
                        "name": "consumer_b",
                        "compatible": "fuchsia,test-b"
                    }
                ],
                "offers": [
                    {
                        "from": "#gpio_controller",
                        "to": "#consumer_a",
                        "service": "fuchsia.hardware.gpio.Service",
                        "name": "reset-pin",
                        "constraints": {
                            "pin": 51
                        }
                    },
                    {
                        "from": "#gpio_controller",
                        "to": "#consumer_b",
                        "service": "fuchsia.hardware.gpio.Service",
                        "name": "reset-pin",
                        "constraints": {
                            "pin": 51
                        }
                    }
                ],
                "metadata_mappings": [
                    {
                        "metadata_id": "fuchsia.hardware.pinimpl.Metadata",
                        "aggregations": [
                            {
                                "service": "fuchsia.hardware.gpio.Service",
                                "field": "pins"
                            }
                        ]
                    }
                ]
            }"##,
        )
        .unwrap();

        let fidl_out = temp_dir.join("board-config.fidl");
        let bind_out = temp_dir.join("board.bind");
        let cml_out = temp_dir.join("board.cml");

        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: None,
            fidl_output: Some(fidl_out.to_str().unwrap().to_string()),
            bind_output: Some(bind_out.to_str().unwrap().to_string()),
            cml_output: Some(cml_out.to_str().unwrap().to_string()),
            driver_dml: vec![],
        };

        compile_board(&args, "2026").unwrap();

        let fidl_bytes = fs::read(&fidl_out).unwrap();
        let board_config: fbdc::BoardConfig = fidl::unpersist(&fidl_bytes).unwrap();
        let aggregates = board_config.aggregates.as_ref().unwrap();
        assert_eq!(aggregates.len(), 1);

        let resources = aggregates[0].resources.as_ref().unwrap();
        assert_eq!(resources.len(), 2);

        let id_a = resources[0]
            .constraint
            .as_ref()
            .unwrap()
            .entries
            .as_ref()
            .unwrap()
            .iter()
            .find(|e| e.key == "id")
            .unwrap();
        let id_b = resources[1]
            .constraint
            .as_ref()
            .unwrap()
            .entries
            .as_ref()
            .unwrap()
            .iter()
            .find(|e| e.key == "id")
            .unwrap();
        // Distinct consumers receive distinct IDs, and deduplicate_metadata_resources keeps only
        // the first pin entry (`id_a`), ensuring only `consumer_a` can bind to the pin.
        assert_ne!(id_a.value, id_b.value);

        let gpio_dev = board_config
            .devices
            .as_ref()
            .unwrap()
            .iter()
            .find(|d| d.name.as_deref() == Some("gpio_controller"))
            .unwrap();
        let meta_bytes = gpio_dev.metadata.as_ref().unwrap()[0].data.as_ref().unwrap();
        let meta_dict: fdr::Dictionary = fidl::unpersist(meta_bytes).unwrap();
        let meta_entries = meta_dict.entries.as_ref().unwrap();
        let pins_count = meta_entries.iter().find(|e| e.key == "pins._count").unwrap();
        assert_eq!(pins_count.value, fdr::DictionaryValue::Int64(1));
        let pin_0_id = meta_entries.iter().find(|e| e.key == "pins.0.id").unwrap();
        assert_eq!(pin_0_id.value, id_a.value);

        let _ = fs::remove_dir_all(&temp_dir);
    }
    #[test]
    fn test_compile_board_offer_metadata() {
        use std::collections::HashMap;
        let temp_dir = std::env::temp_dir().join("test_temp_offer_metadata");
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");
        let fidl_out = temp_dir.join("board-config.fidl");
        let bind_out = temp_dir.join("board.bind");
        let cml_out = temp_dir.join("board.cml");

        fs::write(
            &main_file,
            r##"{
                "name": "test_board",
                "children": [
                    {
                        "name": "gpio-buttons",
                        "url": "fuchsia-pkg://fuchsia.com/buttons#meta/buttons.cm",
                        "compatible": "fuchsia,gpio-buttons"
                    }
                ],
                "offers": [
                    {
                        "service": "fuchsia.hardware.gpio.Service",
                        "name": "power",
                        "from": "#gpio-controller-ff634400",
                        "to": "#gpio-buttons",
                        "constraints": {
                            "pin": 92,
                            "name": "power"
                        }
                    },
                    {
                        "driver_config": "fuchsia.buttons.GpioButtonsMetadata",
                        "from": "#gpio-controller-ff634400",
                        "to": "#gpio-buttons",
                        "buttons": [
                            {
                                "type": {
                                    "direct": {}
                                 },
                                "gpio_a_index": 0,
                                "id": "POWER"
                            }
                        ],
                        "gpios": [
                            {
                                "type": {
                                    "interrupt": {}
                                },
                                "flags": 128
                            }
                        ]
                    }
                ]
            }"##,
        )
        .unwrap();

        let driver_file = temp_dir.join("buttons.dml");
        fs::write(
            &driver_file,
            r##"{
                "name": "buttons",
                "capabilities": [
                    {
                        "service": "fuchsia.hardware.buttons.Service"
                    },
                    {
                        "driver_config": "fuchsia.buttons.GpioButtonsMetadata",
                        "properties": {
                            "buttons": {
                                "type": "vector",
                                "max_count": 10,
                                "element": { "type": "object" }
                            },
                            "gpios": {
                                "type": "vector",
                                "max_count": 10,
                                "element": { "type": "object" }
                            }
                        }
                    }
                ]
            }"##,
        )
        .unwrap();

        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: None,
            fidl_output: Some(fidl_out.to_str().unwrap().to_string()),
            bind_output: Some(bind_out.to_str().unwrap().to_string()),
            cml_output: Some(cml_out.to_str().unwrap().to_string()),
            driver_dml: vec![driver_file.to_str().unwrap().to_string()],
        };

        compile_board(&args, "2026").unwrap();

        let bytes = fs::read(&fidl_out).unwrap();
        let board_config = fidl::unpersist::<fbdc::BoardConfig>(&bytes).unwrap();
        let devices = board_config.devices.unwrap();
        let gpio_buttons =
            devices.iter().find(|d| d.name.as_deref() == Some("gpio-buttons")).unwrap();
        let metadata = gpio_buttons.metadata.as_ref().unwrap();
        assert_eq!(metadata.len(), 1);
        assert_eq!(metadata[0].id.as_deref(), Some("fuchsia.buttons.GpioButtonsMetadata"));
        let dict = fidl::unpersist::<fdr::Dictionary>(metadata[0].data.as_ref().unwrap()).unwrap();
        let flat =
            dict.entries.unwrap().into_iter().map(|e| (e.key, e.value)).collect::<HashMap<_, _>>();
        assert_eq!(flat.get("buttons._count"), Some(&fdr::DictionaryValue::Int64(1)));
        assert_eq!(flat.get("buttons.0.id"), Some(&fdr::DictionaryValue::Str("POWER".to_string())));
        assert_eq!(flat.get("buttons.0.gpio_a_index"), Some(&fdr::DictionaryValue::Int64(0)));
        assert_eq!(flat.get("buttons.0.type.direct"), Some(&fdr::DictionaryValue::Boolean(true)));
        assert_eq!(flat.get("gpios._count"), Some(&fdr::DictionaryValue::Int64(1)));
        assert_eq!(flat.get("gpios.0.flags"), Some(&fdr::DictionaryValue::Int64(128)));
        assert_eq!(flat.get("gpios.0.type.interrupt"), Some(&fdr::DictionaryValue::Boolean(true)));

        // Clean up
        let _ = fs::remove_file(&main_file);
        let _ = fs::remove_file(&driver_file);
        let _ = fs::remove_file(&fidl_out);
        let _ = fs::remove_file(&bind_out);
        let _ = fs::remove_file(&cml_out);
        let _ = fs::remove_dir(&temp_dir);
    }

    #[test]
    fn test_disabled_device_configuration() {
        let temp_dir = std::env::temp_dir().join("test_temp_disabled_device");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");
        fs::write(
            &main_file,
            r##"{
                "name": "test_board",
                "children": [
                    {
                        "name": "node_disabled",
                        "compatible": "fuchsia,disabled",
                        "disabled": true
                    },
                    {
                        "name": "node_explicit_false",
                        "compatible": "fuchsia,explicit-false",
                        "disabled": false
                    },
                    {
                        "name": "node_enabled",
                        "compatible": "fuchsia,enabled"
                    }
                ],
                "offers": [
                    {
                        "from": "parent",
                        "service": "fuchsia.hardware.platform.device.Service",
                        "name": "pdev",
                        "to": "#node_disabled"
                    },
                    {
                        "from": "parent",
                        "service": "fuchsia.hardware.platform.device.Service",
                        "name": "pdev",
                        "to": "#node_offer_only"
                    }
                ]
            }"##,
        )
        .unwrap();

        let fidl_out = temp_dir.join("board-config.fidl");
        let bind_out = temp_dir.join("board.bind");
        let cml_out = temp_dir.join("board.cml");

        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: None,
            fidl_output: Some(fidl_out.to_str().unwrap().to_string()),
            bind_output: Some(bind_out.to_str().unwrap().to_string()),
            cml_output: Some(cml_out.to_str().unwrap().to_string()),
            driver_dml: vec![],
        };

        compile_board(&args, "2026").unwrap();

        let fidl_bytes = fs::read(&fidl_out).unwrap();
        let board_config: fbdc::BoardConfig = fidl::unpersist(&fidl_bytes).unwrap();
        let devices = board_config.devices.as_ref().unwrap();

        let disabled = devices.iter().find(|d| d.name.as_deref() == Some("node_disabled")).unwrap();
        assert_eq!(disabled.disabled, Some(true));

        let explicit_false =
            devices.iter().find(|d| d.name.as_deref() == Some("node_explicit_false")).unwrap();
        assert_eq!(explicit_false.disabled, Some(false));

        let normal = devices.iter().find(|d| d.name.as_deref() == Some("node_enabled")).unwrap();
        assert_eq!(normal.disabled, None);

        let offer_only =
            devices.iter().find(|d| d.name.as_deref() == Some("node_offer_only")).unwrap();
        assert_eq!(offer_only.disabled, None);

        // Verify that offers to disabled devices are preserved in aggregates so they can be
        // enabled at runtime via `fuchsia.driver.devicetree.EnabledNodes`.
        let aggregates = board_config.aggregates.as_ref().unwrap();
        let has_disabled_resource = aggregates.iter().any(|agg| {
            agg.resources
                .as_ref()
                .is_some_and(|res| res.iter().any(|r| r.node.as_deref() == Some("node_disabled")))
        });
        assert!(has_disabled_resource);

        // Verify runtime override helper behavior with `enabled_nodes`.
        assert!(fbdc::is_device_disabled(disabled, &[]));
        assert!(!fbdc::is_device_disabled(disabled, &["node_disabled".to_string()]));
        assert!(!fbdc::is_device_disabled(disabled, &["/node_disabled".to_string()]));
        assert!(fbdc::is_node_force_enabled("pcie-c500000", &["/pcie@c500000".to_string()]));
        assert!(!fbdc::is_device_disabled(explicit_false, &[]));
        assert!(!fbdc::is_device_disabled(normal, &[]));

        // Clean up
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_iommu_declaration_and_bti_resolution() {
        let temp_dir = std::env::temp_dir().join("test_temp_iommu_resolution");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");
        fs::write(
            &main_file,
            r##"{
                "name": "test_board",
                "iommus": [
                    {
                        "name": "arm-smmu",
                        "arm_smmu": {
                            "base_address": 0x0c600001
                        }
                    },
                    {
                        "name": "stub-iommu",
                        "stub_iommu": {}
                    }
                ],
                "children": [
                    {
                        "name": "child_dev",
                        "compatible": "fuchsia,test"
                    }
                ],
                "offers": [
                    {
                        "from": "parent",
                        "service": "fuchsia.hardware.platform.device.Service",
                        "name": "pdev",
                        "to": "#child_dev",
                        "constraints": {
                            "btis": [
                                {
                                    "iommu": "#arm-smmu",
                                    "id": 1,
                                    "name": "bti_0"
                                },
                                {
                                    "iommu": "#arm-smmu",
                                    "id": 2,
                                    "name": "bti_1"
                                },
                                {
                                    "iommu": "#stub-iommu",
                                    "id": 3,
                                    "name": "bti_stub"
                                }
                            ]
                        }
                    }
                ]
            }"##,
        )
        .unwrap();

        let fidl_out = temp_dir.join("board-config.fidl");
        let bind_out = temp_dir.join("board.bind");
        let cml_out = temp_dir.join("board.cml");

        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: None,
            fidl_output: Some(fidl_out.to_str().unwrap().to_string()),
            bind_output: Some(bind_out.to_str().unwrap().to_string()),
            cml_output: Some(cml_out.to_str().unwrap().to_string()),
            driver_dml: vec![],
        };

        compile_board(&args, "2026").unwrap();

        let fidl_bytes = fs::read(&fidl_out).unwrap();
        let board_config: fbdc::BoardConfig = fidl::unpersist(&fidl_bytes).unwrap();

        let iommus = board_config.iommus.as_ref().expect("iommus should be present");
        assert_eq!(iommus.len(), 2);

        let arm_smmu = iommus.iter().find(|i| i.name.as_deref() == Some("arm-smmu")).unwrap();
        // ID 1 was autogenerated since 0 is reserved
        assert_eq!(arm_smmu.id, Some(1));
        match &arm_smmu.iommu_type {
            Some(fbdc::IommuType::ArmSmmu(arm)) => {
                assert_eq!(arm.base_address, 0x0c600001);
            }
            other => panic!("Expected ArmSmmu, got {:?}", other),
        }

        let stub = iommus.iter().find(|i| i.name.as_deref() == Some("stub-iommu")).unwrap();
        // ID 2 was autogenerated sequentially
        assert_eq!(stub.id, Some(2));
        assert!(matches!(stub.iommu_type, Some(fbdc::IommuType::StubIommu(_))));

        let aggregates = board_config.aggregates.as_ref().unwrap();
        let pdev_agg = aggregates.iter().find(|a| a.provider.as_deref() == Some("pdev")).unwrap();
        let resource = &pdev_agg.resources.as_ref().unwrap()[0];
        let dict = resource.constraint.as_ref().unwrap();

        let btis = fbdc::bti_list(dict).unwrap();
        assert_eq!(btis.len(), 3);
        assert_eq!(btis[0].id, 1);
        assert_eq!(btis[0].iommu_id, 1);
        assert_eq!(btis[0].name.as_deref(), Some("bti_0"));

        assert_eq!(btis[1].id, 2);
        assert_eq!(btis[1].iommu_id, 1);
        assert_eq!(btis[1].name.as_deref(), Some("bti_1"));

        assert_eq!(btis[2].id, 3);
        assert_eq!(btis[2].iommu_id, 2);
        assert_eq!(btis[2].name.as_deref(), Some("bti_stub"));

        // Clean up
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_bti_omitted_iommu_defaults_to_zero() {
        let temp_dir = std::env::temp_dir().join("test_temp_bti_omitted_iommu");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");
        fs::write(
            &main_file,
            r##"{
                "name": "test_board",
                "children": [
                    {
                        "name": "child_dev",
                        "compatible": "fuchsia,test"
                    }
                ],
                "offers": [
                    {
                        "from": "parent",
                        "service": "fuchsia.hardware.platform.device.Service",
                        "name": "pdev",
                        "to": "#child_dev",
                        "constraints": {
                            "btis": [
                                {
                                    "id": 1,
                                    "name": "bti_default"
                                },
                                {
                                    "id": 2,
                                    "name": "bti_second"
                                }
                            ]
                        }
                    }
                ]
            }"##,
        )
        .unwrap();

        let fidl_out = temp_dir.join("board-config.fidl");
        let bind_out = temp_dir.join("board.bind");
        let cml_out = temp_dir.join("board.cml");

        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: None,
            fidl_output: Some(fidl_out.to_str().unwrap().to_string()),
            bind_output: Some(bind_out.to_str().unwrap().to_string()),
            cml_output: Some(cml_out.to_str().unwrap().to_string()),
            driver_dml: vec![],
        };

        compile_board(&args, "2026").unwrap();

        let fidl_bytes = fs::read(&fidl_out).unwrap();
        let board_config: fbdc::BoardConfig = fidl::unpersist(&fidl_bytes).unwrap();

        let aggregates = board_config.aggregates.as_ref().unwrap();
        let pdev_agg = aggregates.iter().find(|a| a.provider.as_deref() == Some("pdev")).unwrap();
        let resource = &pdev_agg.resources.as_ref().unwrap()[0];
        let dict = resource.constraint.as_ref().unwrap();

        let btis = fbdc::bti_list(dict).unwrap();
        assert_eq!(btis.len(), 2);
        assert_eq!(btis[0].id, 1);
        assert_eq!(btis[0].iommu_id, 0);
        assert_eq!(btis[0].name.as_deref(), Some("bti_default"));

        assert_eq!(btis[1].id, 2);
        assert_eq!(btis[1].iommu_id, 0);
        assert_eq!(btis[1].name.as_deref(), Some("bti_second"));

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_bti_manual_iommu_id_rejected() {
        let temp_dir = std::env::temp_dir().join("test_temp_bti_manual_iommu_id");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");
        fs::write(
            &main_file,
            r##"{
                "name": "test_board",
                "children": [
                    { "name": "child_dev" }
                ],
                "offers": [
                    {
                        "from": "parent",
                        "service": "fuchsia.hardware.platform.device.Service",
                        "to": "#child_dev",
                        "constraints": {
                            "btis": [
                                {
                                    "id": 1,
                                    "iommu_id": 0
                                }
                            ]
                        }
                    }
                ]
            }"##,
        )
        .unwrap();

        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: Some(temp_dir.to_str().unwrap().to_string()),
            fidl_output: None,
            bind_output: None,
            cml_output: None,
            driver_dml: vec![],
        };

        let res = compile_board(&args, "2026");
        let _ = fs::remove_dir_all(&temp_dir);

        assert!(res.is_err());
        let err_msg = format!("{}", res.err().unwrap());
        assert!(
            err_msg.contains("Explicit \"iommu_id\" definition not allowed"),
            "Expected manual iommu_id error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_bti_reference_missing_hash_rejected() {
        let temp_dir = std::env::temp_dir().join("test_temp_bti_missing_hash");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");
        fs::write(
            &main_file,
            r##"{
                "name": "test_board",
                "iommus": [
                    {
                        "name": "arm-smmu",
                        "arm_smmu": { "base_address": 0x0c600001 }
                    }
                ],
                "children": [
                    { "name": "child_dev" }
                ],
                "offers": [
                    {
                        "from": "parent",
                        "service": "fuchsia.hardware.platform.device.Service",
                        "to": "#child_dev",
                        "constraints": {
                            "btis": [
                                {
                                    "id": 1,
                                    "iommu": "arm-smmu"
                                }
                            ]
                        }
                    }
                ]
            }"##,
        )
        .unwrap();

        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: Some(temp_dir.to_str().unwrap().to_string()),
            fidl_output: None,
            bind_output: None,
            cml_output: None,
            driver_dml: vec![],
        };

        let res = compile_board(&args, "2026");
        let _ = fs::remove_dir_all(&temp_dir);

        assert!(res.is_err());
        let err_msg = format!("{}", res.err().unwrap());
        assert!(
            err_msg.contains("must start with '#'"),
            "Expected missing '#' error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_bti_empty_reference_rejected() {
        let temp_dir = std::env::temp_dir().join("test_temp_bti_empty_ref");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");
        fs::write(
            &main_file,
            r##"{
                "name": "test_board",
                "children": [
                    { "name": "child_dev" }
                ],
                "offers": [
                    {
                        "from": "parent",
                        "service": "fuchsia.hardware.platform.device.Service",
                        "to": "#child_dev",
                        "constraints": {
                            "btis": [
                                {
                                    "id": 1,
                                    "iommu": "#"
                                }
                            ]
                        }
                    }
                ]
            }"##,
        )
        .unwrap();

        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: Some(temp_dir.to_str().unwrap().to_string()),
            fidl_output: None,
            bind_output: None,
            cml_output: None,
            driver_dml: vec![],
        };

        let res = compile_board(&args, "2026");
        let _ = fs::remove_dir_all(&temp_dir);

        assert!(res.is_err());
        let err_msg = format!("{}", res.err().unwrap());
        assert!(
            err_msg.contains("Referenced IOMMU \"\" not found in declared iommus"),
            "Expected empty reference error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_bti_numeric_iommu_rejected() {
        let temp_dir = std::env::temp_dir().join("test_temp_bti_numeric_iommu");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");
        fs::write(
            &main_file,
            r##"{
                "name": "test_board",
                "children": [
                    { "name": "child_dev" }
                ],
                "offers": [
                    {
                        "from": "parent",
                        "service": "fuchsia.hardware.platform.device.Service",
                        "to": "#child_dev",
                        "constraints": {
                            "btis": [
                                {
                                    "id": 1,
                                    "iommu": 42
                                }
                            ]
                        }
                    }
                ]
            }"##,
        )
        .unwrap();

        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: Some(temp_dir.to_str().unwrap().to_string()),
            fidl_output: None,
            bind_output: None,
            cml_output: None,
            driver_dml: vec![],
        };

        let res = compile_board(&args, "2026");
        let _ = fs::remove_dir_all(&temp_dir);

        assert!(res.is_err());
        let err_msg = format!("{}", res.err().unwrap());
        assert!(
            err_msg.contains("expected string reference starting with '#'"),
            "Expected expected string reference error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_bti_undeclared_reference_rejected() {
        let temp_dir = std::env::temp_dir().join("test_temp_bti_undeclared_ref");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");
        fs::write(
            &main_file,
            r##"{
                "name": "test_board",
                "children": [
                    { "name": "child_dev" }
                ],
                "offers": [
                    {
                        "from": "parent",
                        "service": "fuchsia.hardware.platform.device.Service",
                        "to": "#child_dev",
                        "constraints": {
                            "btis": [
                                {
                                    "id": 1,
                                    "iommu": "#unknown-smmu"
                                }
                            ]
                        }
                    }
                ]
            }"##,
        )
        .unwrap();

        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: Some(temp_dir.to_str().unwrap().to_string()),
            fidl_output: None,
            bind_output: None,
            cml_output: None,
            driver_dml: vec![],
        };

        let res = compile_board(&args, "2026");
        let _ = fs::remove_dir_all(&temp_dir);

        assert!(res.is_err());
        let err_msg = format!("{}", res.err().unwrap());
        assert!(
            err_msg.contains("Referenced IOMMU \"unknown-smmu\" not found in declared iommus"),
            "Expected undeclared IOMMU error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_iommu_map_name_starts_with_hash_error() {
        let mut map = IommuMap::default();
        let res = map.insert("#arm-smmu".to_string(), 1);
        assert!(res.is_err());
        let err_msg = format!("{}", res.err().unwrap());
        assert!(err_msg.contains("IOMMU name \"#arm-smmu\" cannot start with '#'"));
    }

    #[test]
    fn test_iommu_definition_with_hash_name_error() {
        let temp_dir = std::env::temp_dir().join("test_temp_iommu_hash_name");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");
        fs::write(
            &main_file,
            r##"{
                "name": "test_board",
                "iommus": [
                    {
                        "name": "#arm-smmu",
                        "arm_smmu": { "base_address": 0x0c600001 }
                    }
                ]
            }"##,
        )
        .unwrap();

        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: Some(temp_dir.to_str().unwrap().to_string()),
            fidl_output: None,
            bind_output: None,
            cml_output: None,
            driver_dml: vec![],
        };

        let res = compile_board(&args, "2026");
        let _ = fs::remove_dir_all(&temp_dir);

        assert!(res.is_err());
        let err_msg = format!("{}", res.err().unwrap());
        assert!(
            err_msg.contains("cannot start with '#'"),
            "Expected hash name error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_iommu_missing_type_error() {
        let temp_dir = std::env::temp_dir().join("test_temp_iommu_missing_type");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");
        fs::write(
            &main_file,
            r##"{
                "name": "test_board",
                "iommus": [
                    {
                        "name": "arm-smmu"
                    }
                ]
            }"##,
        )
        .unwrap();

        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: Some(temp_dir.to_str().unwrap().to_string()),
            fidl_output: None,
            bind_output: None,
            cml_output: None,
            driver_dml: vec![],
        };

        let res = compile_board(&args, "2026");
        let _ = fs::remove_dir_all(&temp_dir);

        assert!(res.is_err());
        let err_msg = format!("{}", res.err().unwrap());
        assert!(
            err_msg.contains("must specify either 'arm_smmu' or 'stub_iommu'"),
            "Expected missing type error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_iommu_both_types_error() {
        let temp_dir = std::env::temp_dir().join("test_temp_iommu_both_types");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let main_file = temp_dir.join("main.dml");
        fs::write(
            &main_file,
            r##"{
                "name": "test_board",
                "iommus": [
                    {
                        "name": "arm-smmu",
                        "arm_smmu": { "base_address": 0x0c600001 },
                        "stub_iommu": {}
                    }
                ]
            }"##,
        )
        .unwrap();

        let args = CompileBoardArgs {
            input_file: main_file.to_str().unwrap().to_string(),
            out_dir: Some(temp_dir.to_str().unwrap().to_string()),
            fidl_output: None,
            bind_output: None,
            cml_output: None,
            driver_dml: vec![],
        };

        let res = compile_board(&args, "2026");
        let _ = fs::remove_dir_all(&temp_dir);

        assert!(res.is_err());
        let err_msg = format!("{}", res.err().unwrap());
        assert!(
            err_msg.contains("cannot specify both 'arm_smmu' and 'stub_iommu'"),
            "Expected both types error, got: {}",
            err_msg
        );
    }
}
