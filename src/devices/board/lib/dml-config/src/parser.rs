// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{AggregateEntry, BoardConfig, Device, Iommu, IommuType, ResourceEntry};
use anyhow::{Context, anyhow, bail};
use fdf_fidl::DriverChannel;
use fidl_fuchsia_driver_metadata_common as fdr;
use fidl_next_fuchsia_driver_framework as fdf_framework;
use fidl_next_fuchsia_hardware_platform_bus as fpbus;
use fidl_next_fuchsia_hardware_power as fpower;
use futures::future::try_join_all;
use phf;
use std::collections::{HashMap, HashSet};
use zx;

pub const BIND_PROTOCOL_DEVICE: u32 = 85; // 0x55
pub const BIND_PLATFORM_DEV_VID_GENERIC: u32 = 0;
pub const BIND_PLATFORM_DEV_DID_DEVICETREE: u32 = 50; // 0x32

pub fn property_int(val: u32) -> fdf_framework::NodePropertyValue {
    fdf_framework::NodePropertyValue::IntValue(val)
}

pub fn property_string(val: &str) -> fdf_framework::NodePropertyValue {
    fdf_framework::NodePropertyValue::StringValue(val.to_string())
}

pub fn property_bool(val: bool) -> fdf_framework::NodePropertyValue {
    fdf_framework::NodePropertyValue::BoolValue(val)
}

fn map_interrupt_mode(mode: Option<&str>) -> fpbus::ZirconInterruptMode {
    match mode {
        Some("EdgeLow") => fpbus::ZirconInterruptMode::EdgeLow,
        Some("EdgeHigh") => fpbus::ZirconInterruptMode::EdgeHigh,
        Some("LevelLow") => fpbus::ZirconInterruptMode::LevelLow,
        Some("LevelHigh") => fpbus::ZirconInterruptMode::LevelHigh,
        Some("EdgeBoth") => fpbus::ZirconInterruptMode::EdgeBoth,
        _ => fpbus::ZirconInterruptMode::Default,
    }
}

pub type DriverSpecificMetadata =
    phf::Map<&'static str, &'static [(&'static str, fn() -> anyhow::Result<Vec<u8>>)]>;

pub type DriverSpecificPowerConfigs =
    phf::Map<&'static str, fn() -> anyhow::Result<Vec<fpower::PowerElementConfiguration>>>;

pub fn make_accept_bind_rule(
    key: &str,
    value: fdf_framework::NodePropertyValue,
) -> fdf_framework::BindRule2 {
    fdf_framework::BindRule2 {
        key: key.to_string(),
        condition: fdf_framework::Condition::Accept,
        values: vec![value],
    }
}

pub fn make_property2(
    key: &str,
    value: fdf_framework::NodePropertyValue,
) -> fdf_framework::NodeProperty2 {
    fdf_framework::NodeProperty2 { key: key.to_string(), value }
}

pub fn add_rule_and_property(
    bind_rules: &mut Vec<fdf_framework::BindRule2>,
    properties: &mut Vec<fdf_framework::NodeProperty2>,
    key: &str,
    value: fdf_framework::NodePropertyValue,
) {
    bind_rules.push(make_accept_bind_rule(key, value.clone()));
    properties.push(make_property2(key, value));
}

#[derive(Clone, Copy, Debug)]
pub enum ValueSource {
    /// Value of a given key within the constraint table.
    ConstraintKey(&'static str),

    /// The `name` field of the DML service offer.
    ResourceName,

    /// The target node name (the `to` field) of the DML service offer.
    ResourceNode,

    /// A literal integer value.
    Integer(u32),

    /// The `id` field of the provider (the `from` field) device in the DML.
    ProviderId,
}

#[derive(Clone, Copy, Debug)]
pub enum RuleValueType {
    Integer,
    String,
    Bool,
}

#[derive(Clone, Copy, Debug)]
pub enum Destination {
    /// Create a bind rule of the given key and value.
    BindRules,

    /// Create a node property for the given key and value.
    Properties,

    /// Create a node property and a bind rule for the given key and value.
    Both,
}

/// Defines how to create a node property or bind rule from a given DML service offer.
#[derive(Clone, Copy, Debug)]
pub struct PropertyRule {
    /// Name of key for the node property or bind rule.
    pub bind_key: &'static str,

    /// Candidates for the value of the node property or bind rule.
    ///
    /// Ordered by priority. If the first source exists it will be used, otherwise the next source
    /// is checked and so on.
    pub sources: &'static [ValueSource],

    /// The type of the node property's or bind rule's value.
    pub value_type: RuleValueType,

    /// Where to add `bind_key` and its value in the composite node spec.
    pub destination: Destination,
}

#[derive(Clone, Copy, Debug)]
pub enum TransportType {
    None,
    Zircon,
    Driver,
}

/// Defines how to convert a given DML service offer into a parent node of a composite node spec.
#[derive(Clone, Copy, Debug)]
pub struct ServiceBindConfig {
    /// Which transport type to use.
    pub transport: TransportType,

    /// Rules for creating node properties and bind rules for the parent node.
    pub rules: &'static [PropertyRule],

    /// Candidates for the deduplication key of the parent node.
    ///
    /// Ordered by priority. If the first source exists it will be used, otherwise the next source
    /// is checked and so on.
    pub parent_key_sources: &'static [ValueSource],

    /// If true, adds a `fuchsia.ID` bind rule from the `"id"` constraint when not already set by
    /// `rules`.
    pub bind_id: bool,
}

pub const DEFAULT_SERVICE_BIND_CONFIG: ServiceBindConfig = ServiceBindConfig {
    transport: TransportType::Zircon,
    rules: &[],
    parent_key_sources: &[ValueSource::ResourceName],
    bind_id: false,
};

pub const DEFAULT_ID_SERVICE_BIND_CONFIG: ServiceBindConfig =
    ServiceBindConfig { bind_id: true, ..DEFAULT_SERVICE_BIND_CONFIG };

pub struct DmlParserConfig {
    pub service_configs: phf::Map<&'static str, ServiceBindConfig>,
}

/// A list of instructions for how to convert common DML service offers into parent nodes
/// of composite node specs.
pub static STANDARD_SERVICE_CONFIGS: phf::Map<&'static str, ServiceBindConfig> = phf::phf_map! {
    "fuchsia.clock.Init" => ServiceBindConfig {
        transport: TransportType::None,
        rules: &[PropertyRule {
            bind_key: "fuchsia.BIND_INIT_STEP",
            sources: &[ValueSource::Integer(0x494B4C43)],
            value_type: RuleValueType::Integer,
            destination: Destination::Both,
        }],
        ..DEFAULT_SERVICE_BIND_CONFIG
    },
    "fuchsia.pwm.Init" => ServiceBindConfig {
        transport: TransportType::None,
        rules: &[PropertyRule {
            bind_key: "fuchsia.BIND_INIT_STEP",
            sources: &[ValueSource::Integer(0x004D5750)],
            value_type: RuleValueType::Integer,
            destination: Destination::Both,
        }],
        ..DEFAULT_SERVICE_BIND_CONFIG
    },
    "fuchsia.gpio.Init" => ServiceBindConfig {
        transport: TransportType::None,
        rules: &[PropertyRule {
            bind_key: "fuchsia.BIND_INIT_STEP",
            sources: &[ValueSource::Integer(0x4F495047)],
            value_type: RuleValueType::Integer,
            destination: Destination::Both,
        }],
        ..DEFAULT_SERVICE_BIND_CONFIG
    },
    "fuchsia.hardware.gpu.mali.Service" => ServiceBindConfig {
        transport: TransportType::Driver,
        ..DEFAULT_SERVICE_BIND_CONFIG
    },
    "fuchsia.hardware.platform.device.Service" => ServiceBindConfig {
        rules: &[
            PropertyRule {
                bind_key: "fuchsia.BIND_PROTOCOL",
                sources: &[ValueSource::Integer(BIND_PROTOCOL_DEVICE)],
                value_type: RuleValueType::Integer,
                destination: Destination::Both,
            },
            PropertyRule {
                bind_key: "fuchsia.BIND_PLATFORM_DEV_VID",
                sources: &[ValueSource::ConstraintKey("vid")],
                value_type: RuleValueType::Integer,
                destination: Destination::Both,
            },
            PropertyRule {
                bind_key: "fuchsia.BIND_PLATFORM_DEV_DID",
                sources: &[ValueSource::ConstraintKey("did")],
                value_type: RuleValueType::Integer,
                destination: Destination::Both,
            },
        ],
        ..DEFAULT_SERVICE_BIND_CONFIG
    },
    "fuchsia.hardware.platform.bus.Service" => ServiceBindConfig {
        transport: TransportType::Driver,
        rules: &[
            PropertyRule {
                bind_key: "fuchsia.BIND_PLATFORM_DEV_VID",
                sources: &[ValueSource::ConstraintKey("vid")],
                value_type: RuleValueType::Integer,
                destination: Destination::Both,
            },
            PropertyRule {
                bind_key: "fuchsia.BIND_PLATFORM_DEV_PID",
                sources: &[ValueSource::ConstraintKey("pid")],
                value_type: RuleValueType::Integer,
                destination: Destination::Both,
            },
            PropertyRule {
                bind_key: "fuchsia.BIND_PLATFORM_DEV_DID",
                sources: &[ValueSource::ConstraintKey("did")],
                value_type: RuleValueType::Integer,
                destination: Destination::Both,
            },
        ],
        ..DEFAULT_SERVICE_BIND_CONFIG
    },
    "fuchsia.hardware.pci.Service" => ServiceBindConfig {
        rules: &[
            PropertyRule {
                bind_key: "fuchsia.BIND_PCI_VID",
                sources: &[ValueSource::ConstraintKey("vid")],
                value_type: RuleValueType::Integer,
                destination: Destination::Both,
            },
            PropertyRule {
                bind_key: "fuchsia.BIND_PCI_DID",
                sources: &[ValueSource::ConstraintKey("did")],
                value_type: RuleValueType::Integer,
                destination: Destination::Both,
            },
            PropertyRule {
                bind_key: "fuchsia.BIND_PCI_TOPO",
                sources: &[ValueSource::ConstraintKey("topo")],
                value_type: RuleValueType::Integer,
                destination: Destination::BindRules,
            },
        ],
        ..DEFAULT_SERVICE_BIND_CONFIG
    },
};

pub const DEFAULT_DML_PARSER_CONFIG: DmlParserConfig =
    DmlParserConfig { service_configs: phf::phf_map! {} };

impl Default for DmlParserConfig {
    fn default() -> Self {
        DEFAULT_DML_PARSER_CONFIG
    }
}

fn resolve_value(
    _provider: &str,
    provider_id: u32,
    source: &ValueSource,
    res: &ResourceEntry,
    constraint: &fdr::Dictionary,
) -> Option<ResolvedValue> {
    match source {
        ValueSource::ConstraintKey(key) => {
            if let Some(i) = crate::get_int64(constraint, key) {
                Some(ResolvedValue::Integer(i as u32))
            } else if let Some(s) = crate::get_string(constraint, key) {
                Some(ResolvedValue::String(s))
            } else if let Some(b) = crate::get_bool(constraint, key) {
                Some(ResolvedValue::Bool(b))
            } else {
                None
            }
        }
        ValueSource::ResourceName => res.name.clone().map(ResolvedValue::String),
        ValueSource::ResourceNode => res.node.clone().map(ResolvedValue::String),
        ValueSource::Integer(i) => Some(ResolvedValue::Integer(*i)),
        ValueSource::ProviderId => Some(ResolvedValue::Integer(provider_id)),
    }
}

enum ResolvedValue {
    Integer(u32),
    String(String),
    Bool(bool),
}

fn apply_rule(
    provider: &str,
    provider_id: u32,
    rule: &PropertyRule,
    res: &ResourceEntry,
    constraint: &fdr::Dictionary,
    bind_rules: &mut Vec<fdf_framework::BindRule2>,
    properties: &mut Vec<fdf_framework::NodeProperty2>,
) -> anyhow::Result<()> {
    let resolved = rule
        .sources
        .iter()
        .find_map(|source| resolve_value(provider, provider_id, source, res, constraint));

    let Some(val) = resolved else {
        return Ok(());
    };

    let property_value = match (val, rule.value_type) {
        (ResolvedValue::Integer(i), RuleValueType::Integer) => property_int(i),
        (ResolvedValue::String(s), RuleValueType::String) => property_string(&s),
        (ResolvedValue::Bool(b), RuleValueType::Bool) => property_bool(b),
        (ResolvedValue::String(s), RuleValueType::Integer) => {
            let parsed = if s.starts_with("0x") || s.starts_with("0X") {
                u32::from_str_radix(&s[2..], 16)
            } else {
                s.parse::<u32>()
            };
            match parsed {
                Ok(i) => property_int(i),
                Err(e) => {
                    bail!("Failed to parse string {} as integer: {}", s, e);
                }
            }
        }
        (ResolvedValue::String(s), RuleValueType::Bool) => match s.parse::<bool>() {
            Ok(b) => property_bool(b),
            Err(e) => {
                bail!("Failed to parse string {} as bool: {}", s, e);
            }
        },
        _ => {
            bail!("Type mismatch in rule resolution");
        }
    };

    match rule.destination {
        Destination::BindRules => {
            bind_rules.push(make_accept_bind_rule(rule.bind_key, property_value));
        }
        Destination::Properties => {
            properties.push(make_property2(rule.bind_key, property_value));
        }
        Destination::Both => {
            add_rule_and_property(bind_rules, properties, rule.bind_key, property_value);
        }
    }

    Ok(())
}

pub fn generate_parent_spec_generic(
    provider: &str,
    provider_id: u32,
    service_name: &str,
    res: &ResourceEntry,
    config: &DmlParserConfig,
) -> anyhow::Result<Option<(fdf_framework::ParentSpec2, String)>> {
    let mut bind_rules = Vec::new();
    let mut properties = Vec::new();

    let service_config = config
        .service_configs
        .get(service_name)
        .or_else(|| STANDARD_SERVICE_CONFIGS.get(service_name))
        .unwrap_or(&DEFAULT_ID_SERVICE_BIND_CONFIG);

    match service_config.transport {
        TransportType::Zircon => {
            add_rule_and_property(
                &mut bind_rules,
                &mut properties,
                "fuchsia.Service",
                property_string(service_name),
            );
            properties.push(make_property2(
                service_name,
                property_string(&format!("{service_name}.ZirconTransport")),
            ));
        }
        TransportType::Driver => {
            add_rule_and_property(
                &mut bind_rules,
                &mut properties,
                "fuchsia.Service",
                property_string(service_name),
            );
            properties.push(make_property2(
                service_name,
                property_string(&format!("{service_name}.DriverTransport")),
            ));
        }
        TransportType::None => {}
    }

    let constraint =
        res.constraint.as_ref().ok_or_else(|| anyhow!("constraint is missing in ResourceEntry"))?;

    for rule in service_config.rules {
        apply_rule(provider, provider_id, rule, res, constraint, &mut bind_rules, &mut properties)?;
    }

    if !properties.iter().any(|p| p.key == "fuchsia.NAME") {
        let name_opt = res.name.clone().or_else(|| crate::get_string(constraint, "name"));
        if let Some(name) = name_opt {
            properties.push(make_property2("fuchsia.NAME", property_string(&name)));
        }
    }

    if service_config.bind_id
        && !bind_rules.iter().any(|r| r.key == "fuchsia.ID")
        && let Some(id) = crate::get_uint32(constraint, "id")
    {
        bind_rules.push(make_accept_bind_rule("fuchsia.ID", property_int(id)));
    }

    let resolved_key = service_config.parent_key_sources.iter().find_map(|source| {
        match resolve_value(provider, provider_id, source, res, constraint) {
            Some(ResolvedValue::String(k)) => Some(k),
            _ => None,
        }
    });

    let key = match resolved_key {
        Some(k) => k,
        None => {
            bail!(
                "Failed to resolve parent key for service {}. If this service should be ignored, add it to the config.",
                service_name
            );
        }
    };

    let parent = fdf_framework::ParentSpec2 { bind_rules, properties };
    Ok(Some((parent, key)))
}

fn get_aggregate_id(agg: &AggregateEntry, devices: &[Device], fallback_id: u32) -> u32 {
    let device = devices.iter().find(|d| d.name.as_deref() == agg.provider.as_deref());
    let id = device.and_then(|d| d.id).unwrap_or(fallback_id);
    log::debug!(
        "get_aggregate_id: provider={:?}, device_found={}, device_id={:?}, final_id={}, fallback={}",
        agg.provider,
        device.is_some(),
        device.and_then(|d| d.id),
        id,
        fallback_id
    );
    id
}

pub async fn publish_dml_devices(
    pbus: &fidl_next::Client<fpbus::PlatformBus, DriverChannel>,
    composite_manager: &fidl_next::Client<fdf_framework::CompositeNodeManager, zx::Channel>,
    config: &BoardConfig,
    parser_config: &DmlParserConfig,
    driver_metadata: Option<&DriverSpecificMetadata>,
    driver_power_configs: Option<&DriverSpecificPowerConfigs>,
    enabled_nodes: &[String],
) -> anyhow::Result<()> {
    let mut provider_metadata = HashMap::<String, Vec<fpbus::Metadata>>::new();

    register_iommus(pbus, config.iommus.as_deref()).await.context("Failed to register IOMMUs")?;

    let devices = config
        .devices
        .as_ref()
        .ok_or_else(|| anyhow!("devices field is missing in BoardConfig"))?;

    let disabled_devices: HashSet<&str> = devices
        .iter()
        .filter(|d| crate::is_device_disabled(d, enabled_nodes))
        .filter_map(|d| d.name.as_deref())
        .collect();

    // 1. Generate driver specific metadata for devices in config
    if let Some(drv_meta) = driver_metadata {
        for dev in devices {
            let name = dev.name.as_deref().unwrap_or("");
            if disabled_devices.contains(name) {
                continue;
            }
            if let Some(generators) = drv_meta.get(name) {
                for (metadata_id, gen_fn) in *generators {
                    let data = gen_fn()
                        .with_context(|| format!("Failed to generate metadata for {}", name))?;
                    provider_metadata.entry(name.to_string()).or_default().push(fpbus::Metadata {
                        id: Some(metadata_id.to_string()),
                        data: Some(data),
                        ..Default::default()
                    });
                }
            }
        }
    }

    for (idx, dev) in devices.iter().enumerate() {
        let dev_name = dev.name.as_deref().unwrap_or("");
        if disabled_devices.contains(dev_name) {
            continue;
        }
        if dev.disabled.unwrap_or(false) {
            log::info!("Publishing disabled DML device '{}' due to runtime override", dev_name);
        }
        let instance_id = idx as u32 + 1;
        let mut node = fpbus::Node {
            name: dev.name.clone(),
            vid: Some(0),
            pid: Some(0),
            did: Some(0),
            instance_id: Some(instance_id),
            driver_host: dev.driver_host.clone(),
            ..Default::default()
        };
        node.interrupt_controller_id = dev.interrupt_controller_id;

        if let Some(compatible) = &dev.compatible {
            node.properties = Some(vec![fdf_framework::NodeProperty2 {
                key: "fuchsia.COMPATIBLE".to_string(),
                value: fdf_framework::NodePropertyValue::StringValue(compatible.clone()),
            }]);
            node.did = Some(BIND_PLATFORM_DEV_DID_DEVICETREE);
            node.vid = Some(BIND_PLATFORM_DEV_VID_GENERIC);
        }

        let mut mmio_list = Vec::new();
        let mut irq_list = Vec::new();
        let mut bti_list = Vec::new();
        let mut smc_list = Vec::new();
        let mut boot_metadata_list = Vec::new();

        if let Some(pdev_dict) = crate::pdev_constraints(config, dev.name.as_deref().unwrap_or(""))
        {
            for mmio in crate::mmio_list(pdev_dict) {
                mmio_list.push(fpbus::Mmio {
                    base: Some(mmio.base),
                    length: Some(mmio.length),
                    name: mmio.name,
                    ..Default::default()
                });
            }
            for irq in crate::irq_list(pdev_dict) {
                let irq_spec = match irq.controller {
                    Some(controller_id) => fpbus::IrqSpec::UserspaceIrq(fpbus::UserspaceIrq {
                        irq: irq.number,
                        controller_id,
                    }),
                    None => fpbus::IrqSpec::Irq(irq.number),
                };
                irq_list.push(fpbus::Irq {
                    irq: Some(irq_spec),
                    mode: Some(map_interrupt_mode(irq.mode.as_deref())),
                    name: irq.name.clone(),
                    wake_vector: irq.wake_vector,
                    ..Default::default()
                });
            }
            for bti in crate::bti_list(pdev_dict)? {
                bti_list.push(fpbus::Bti {
                    iommu_id: Some(bti.iommu_id),
                    bti_id: Some(bti.id),
                    name: bti.name,
                    ..Default::default()
                });
            }
            for smc in crate::smc_list(pdev_dict) {
                smc_list.push(fpbus::Smc {
                    service_call_num_base: Some(smc.service_call_num_base),
                    count: Some(smc.count),
                    exclusive: Some(smc.exclusive),
                    name: smc.name,
                    ..Default::default()
                });
            }
            for bm in crate::boot_metadata_list(pdev_dict) {
                boot_metadata_list.push(fpbus::BootMetadata {
                    zbi_type: Some(bm.zbi_type),
                    zbi_extra: bm.zbi_extra.or(Some(0)),
                    ..Default::default()
                });
            }
        }

        if !mmio_list.is_empty() {
            node.mmio = Some(mmio_list);
        }
        if !irq_list.is_empty() {
            node.irq = Some(irq_list);
        }
        if !bti_list.is_empty() {
            node.bti = Some(bti_list);
        }
        if !smc_list.is_empty() {
            node.smc = Some(smc_list);
        }
        if !boot_metadata_list.is_empty() {
            node.boot_metadata = Some(boot_metadata_list);
        }

        let mut metadata_list = Vec::new();

        // Handle static metadata from config
        if let Some(metadata) = &dev.metadata {
            for meta in metadata {
                let data =
                    meta.data.clone().ok_or_else(|| anyhow!("Static metadata missing data"))?;
                metadata_list.push(fpbus::Metadata {
                    id: meta.id.clone(),
                    data: Some(data),
                    ..Default::default()
                });
            }
        }

        if let Some(meta) = provider_metadata.get(dev_name) {
            metadata_list.extend(meta.clone());
        }

        if !metadata_list.is_empty() {
            node.metadata = Some(metadata_list);
        }

        if let Some(gen_fn) = driver_power_configs.and_then(|configs| configs.get(dev_name)) {
            let power_config = gen_fn()
                .with_context(|| format!("Failed to generate power config for {}", dev_name))?;
            if !power_config.is_empty() {
                node.power_config = Some(power_config);
            }
        }

        let mut resource_parents = Vec::new();
        let mut generated_keys = HashSet::new();
        if let Some(aggregates) = &config.aggregates {
            for (agg_idx, agg) in aggregates.iter().enumerate() {
                if agg.provider.as_deref().is_some_and(|p| disabled_devices.contains(p)) {
                    continue;
                }
                if let Some(resources) = &agg.resources {
                    for res in resources {
                        if res.node.as_deref() == dev.name.as_deref() {
                            let parent_and_key = if (agg.service.as_deref()
                                == Some("fuchsia.hardware.platform.device.Service")
                                || agg.service.as_deref()
                                    == Some("fuchsia.hardware.interrupt.ControllerRegistryService"))
                                && dev.compatible.is_some()
                            {
                                None
                            } else {
                                generate_parent_spec_generic(
                                    agg.provider.as_deref().unwrap_or(""),
                                    get_aggregate_id(
                                        agg,
                                        config.devices.as_deref().unwrap_or(&[]),
                                        agg_idx as u32,
                                    ),
                                    agg.service.as_deref().unwrap_or(""),
                                    res,
                                    parser_config,
                                )?
                            };
                            if let Some((parent, key)) = parent_and_key {
                                if generated_keys.insert(key) {
                                    if matches!(
                                        agg.provider.as_deref(),
                                        Some("parent") | Some("pdev")
                                    ) {
                                        resource_parents.insert(0, parent);
                                    } else {
                                        resource_parents.push(parent);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        if resource_parents.is_empty() && dev.driver_host.is_some() {
            node.driver_host = dev.driver_host.clone();
        }

        let mut spec = fdf_framework::CompositeNodeSpec {
            name: dev.name.clone(),
            driver_host: dev.driver_host.clone(),
            ..Default::default()
        };

        let mut parents2 = Vec::new();

        // 1. Generate pdev parent if compatible is present
        if let Some(compatible) = &dev.compatible {
            let pdev_parent = fdf_framework::ParentSpec2 {
                bind_rules: vec![
                    make_accept_bind_rule(
                        "fuchsia.BIND_PROTOCOL",
                        property_int(BIND_PROTOCOL_DEVICE),
                    ),
                    make_accept_bind_rule(
                        "fuchsia.BIND_PLATFORM_DEV_VID",
                        property_int(BIND_PLATFORM_DEV_VID_GENERIC),
                    ),
                    make_accept_bind_rule(
                        "fuchsia.BIND_PLATFORM_DEV_DID",
                        property_int(BIND_PLATFORM_DEV_DID_DEVICETREE),
                    ),
                    make_accept_bind_rule(
                        "fuchsia.BIND_PLATFORM_DEV_INSTANCE_ID",
                        property_int(instance_id),
                    ),
                    make_accept_bind_rule("fuchsia.COMPATIBLE", property_string(compatible)),
                ],
                properties: vec![
                    // TODO(https://fxbug.dev/555962083): Restore `fuchsia.NAME = "pdev"` once all
                    // composite drivers are migrated from `primary parent "devicetree"` to `"pdev"`
                    // (`driver-index` rejects `ParentSpec2.properties` when `fuchsia.NAME` does not
                    // equal the `.bind` primary parent symbol name).
                    make_property2("fuchsia.BIND_PROTOCOL", property_int(BIND_PROTOCOL_DEVICE)),
                    make_property2(
                        "fuchsia.BIND_PLATFORM_DEV_VID",
                        property_int(BIND_PLATFORM_DEV_VID_GENERIC),
                    ),
                    make_property2(
                        "fuchsia.BIND_PLATFORM_DEV_DID",
                        property_int(BIND_PLATFORM_DEV_DID_DEVICETREE),
                    ),
                    make_property2(
                        "fuchsia.BIND_PLATFORM_DEV_INSTANCE_ID",
                        property_int(instance_id),
                    ),
                    make_property2("fuchsia.COMPATIBLE", property_string(compatible)),
                    make_property2(
                        "fuchsia.devicetree.FIRST_COMPATIBLE",
                        property_string(compatible),
                    ),
                    make_property2(
                        "fuchsia.Service",
                        property_string("fuchsia.hardware.platform.device.Service"),
                    ),
                ],
            };
            parents2.push(pdev_parent);
        }

        parents2.extend(resource_parents);

        if !parents2.is_empty() {
            spec.parents2 = Some(parents2);

            log::debug!(
                "DML-CONFIG: Adding composite spec: name={:?}, parents2={:?}",
                spec.name,
                spec.parents2
            );
            composite_manager
                .add_spec_with(spec)
                .await
                .context("AddSpec request failed")?
                .map_err(|e| anyhow!("AddSpec failed: {e:?}"))?;
        }

        if dev.compatible.is_some() {
            log::debug!("DML-CONFIG: Adding node: {}", dev_name);
            pbus.node_add(node)
                .await
                .context("NodeAdd request failed")?
                .context("NodeAdd failed")?;
        }
    }

    Ok(())
}

/// Registers `iommus` with the `pbus`.
async fn register_iommus(
    pbus: &fidl_next::Client<fpbus::PlatformBus, DriverChannel>,
    iommus: Option<&[Iommu]>,
) -> anyhow::Result<()> {
    let Some(iommus) = iommus else {
        return Ok(());
    };

    let futures = iommus
        .iter()
        .map(|iommu| {
            let iommu_name = iommu.name.as_deref().unwrap_or("unnamed");
            let iommu_id = iommu.id.with_context(|| format!("IOMMU {iommu_name:?} missing id"))?;
            let fpbus_iommu = match &iommu.iommu_type {
                Some(IommuType::ArmSmmu(arm_smmu)) => {
                    fpbus::Iommu::ArmSmmu(fpbus::ArmSmmu { base_address: arm_smmu.base_address })
                }
                Some(IommuType::StubIommu(_)) | None => fpbus::Iommu::StubIommu(()),
                _ => {
                    bail!("Unsupported IOMMU type for IOMMU {iommu_name:?}");
                }
            };
            let future = async move {
                log::info!("Registering IOMMU '{iommu_name}' (id: {iommu_id})");
                pbus.register_iommu(iommu_id, &fpbus_iommu)
                    .await
                    .with_context(|| {
                        format!(
                            "Failed to send RegisterIommu FIDL request for IOMMU {iommu_name:?}"
                        )
                    })?
                    .with_context(|| format!("Failed to register IOMMU {iommu_name:?}"))?;
                Ok::<(), anyhow::Error>(())
            };
            Ok(future)
        })
        .collect::<Result<Vec<_>, _>>()?;
    try_join_all(futures).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pci_service_parent_spec() {
        let res = ResourceEntry {
            name: Some("pci".to_string()),
            node: Some("mali-gpu".to_string()),
            constraint: Some(fdr::Dictionary {
                entries: Some(vec![
                    fdr::DictionaryEntry {
                        key: "vid".to_string(),
                        value: fdr::DictionaryValue::Int64(0x13b5),
                    },
                    fdr::DictionaryEntry {
                        key: "did".to_string(),
                        value: fdr::DictionaryValue::Int64(0x7212),
                    },
                    fdr::DictionaryEntry {
                        key: "topo".to_string(),
                        value: fdr::DictionaryValue::Int64(256),
                    },
                ]),
                ..Default::default()
            }),
            ..Default::default()
        };

        let (parent, key) = generate_parent_spec_generic(
            "pcie-c500000",
            0,
            "fuchsia.hardware.pci.Service",
            &res,
            &DEFAULT_DML_PARSER_CONFIG,
        )
        .expect("should succeed")
        .expect("should return parent spec");

        assert_eq!(key, "pci");
        assert!(parent.bind_rules.iter().any(|r| r.key == "fuchsia.BIND_PCI_VID"
            && r.values == vec![fdf_framework::NodePropertyValue::IntValue(0x13b5)]));
        assert!(parent.bind_rules.iter().any(|r| r.key == "fuchsia.BIND_PCI_DID"
            && r.values == vec![fdf_framework::NodePropertyValue::IntValue(0x7212)]));
        assert!(parent.bind_rules.iter().any(|r| r.key == "fuchsia.BIND_PCI_TOPO"
            && r.values == vec![fdf_framework::NodePropertyValue::IntValue(256)]));
        assert!(parent.properties.iter().any(|p| p.key == "fuchsia.BIND_PCI_VID"
            && p.value == fdf_framework::NodePropertyValue::IntValue(0x13b5)));
        assert!(parent.properties.iter().any(|p| p.key == "fuchsia.BIND_PCI_DID"
            && p.value == fdf_framework::NodePropertyValue::IntValue(0x7212)));
        assert!(!parent.properties.iter().any(|p| p.key == "fuchsia.BIND_PCI_TOPO"));
    }
}
