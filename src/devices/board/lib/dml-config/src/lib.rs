// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Context;
use fidl_fuchsia_board_dml_config_common as fbdc;
use fidl_fuchsia_driver_metadata_common as fdr;

// Re-export FIDL types for convenience
pub use fbdc::{
    AggregateEntry, ArmSmmu, BoardConfig, Device, Iommu, IommuType, ResourceEntry, StaticMetadata,
    StubIommu,
};

#[derive(Debug, Clone, Default)]
pub struct Mmio {
    pub name: Option<String>,
    pub base: u64,
    pub length: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Irq {
    pub name: Option<String>,
    pub number: u32,
    pub mode: Option<String>,
    pub wake_vector: Option<bool>,
    pub controller: Option<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct Bti {
    pub id: u32,
    pub name: Option<String>,
    pub iommu_id: u32,
}

#[derive(Debug, Clone, Default)]
pub struct Smc {
    pub service_call_num_base: u32,
    pub count: u32,
    pub exclusive: bool,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct BootMetadata {
    pub zbi_type: u32,
    pub zbi_extra: Option<u32>,
}

// Dictionary Lookup Helpers
pub fn find_value<'a>(dict: &'a fdr::Dictionary, key: &str) -> Option<&'a fdr::DictionaryValue> {
    dict.entries.as_ref()?.iter().find(|e| e.key == key).map(|e| &e.value)
}

pub fn get_int64(dict: &fdr::Dictionary, key: &str) -> Option<i64> {
    match find_value(dict, key)? {
        fdr::DictionaryValue::Int64(i) => Some(*i),
        _ => None,
    }
}

pub fn get_uint32(dict: &fdr::Dictionary, key: &str) -> Option<u32> {
    get_int64(dict, key).and_then(|i| u32::try_from(i).ok())
}

pub fn get_uint64(dict: &fdr::Dictionary, key: &str) -> Option<u64> {
    get_int64(dict, key).map(|i| i as u64)
}

pub fn get_string(dict: &fdr::Dictionary, key: &str) -> Option<String> {
    match find_value(dict, key)? {
        fdr::DictionaryValue::Str(s) => Some(s.clone()),
        _ => None,
    }
}

pub fn get_bool(dict: &fdr::Dictionary, key: &str) -> Option<bool> {
    match find_value(dict, key)? {
        fdr::DictionaryValue::Boolean(b) => Some(*b),
        _ => None,
    }
}

// Resource Parsing Helpers
pub fn mmio_list(dict: &fdr::Dictionary) -> Vec<Mmio> {
    let mut list = Vec::new();
    for i in 0.. {
        let prefix = format!("mmio.{}", i);
        let base = get_uint64(dict, &format!("{}.base", prefix))
            .or_else(|| get_uint64(dict, &format!("{}.address", prefix)));
        if let Some(base) = base {
            let length = get_uint64(dict, &format!("{}.length", prefix))
                .or_else(|| get_uint64(dict, &format!("{}.size", prefix)))
                .unwrap_or(0);
            let name = get_string(dict, &format!("{}.name", prefix));
            list.push(Mmio { name, base, length });
        } else {
            break;
        }
    }
    list
}

pub fn irq_list(dict: &fdr::Dictionary) -> Vec<Irq> {
    let mut list = Vec::new();
    for i in 0.. {
        let prefix = format!("interrupts.{}", i);
        if let Some(number) = get_uint32(dict, &format!("{}.number", prefix)) {
            let name = get_string(dict, &format!("{}.name", prefix));
            let mode = get_string(dict, &format!("{}.mode", prefix));
            let wake_vector = get_bool(dict, &format!("{}.wake_vector", prefix));
            let controller = get_uint32(dict, &format!("{}.controller", prefix));
            list.push(Irq { name, number, mode, wake_vector, controller });
        } else {
            break;
        }
    }
    list
}

pub fn bti_list(dict: &fdr::Dictionary) -> anyhow::Result<Vec<Bti>> {
    let mut list = Vec::new();
    let entries = dict.entries.as_deref().unwrap_or(&[]);
    for i in 0.. {
        let prefix = format!("btis.{i}.");
        if !entries.iter().any(|e| e.key.starts_with(&prefix)) {
            break;
        }

        let id = get_uint32(dict, &format!("btis.{i}.id"))
            .with_context(|| format!("BTI at index {i} is missing required \"id\""))?;
        let iommu_id = get_uint32(dict, &format!("btis.{i}.iommu_id"))
            .with_context(|| format!("BTI with ID {id} at index {i} is missing \"iommu_id\""))?;
        let name = get_string(dict, &format!("btis.{i}.name"));
        list.push(Bti { id, name, iommu_id });
    }
    Ok(list)
}

pub fn smc_list(dict: &fdr::Dictionary) -> Vec<Smc> {
    let mut list = Vec::new();
    for i in 0.. {
        let prefix = format!("smcs.{}", i);
        if let Some(service_call_num_base) =
            get_uint32(dict, &format!("{}.service_call_num_base", prefix))
        {
            let count = get_uint32(dict, &format!("{}.count", prefix)).unwrap_or(0);
            let exclusive = get_bool(dict, &format!("{}.exclusive", prefix)).unwrap_or(false);
            let name = get_string(dict, &format!("{}.name", prefix));
            list.push(Smc { service_call_num_base, count, exclusive, name });
        } else {
            break;
        }
    }
    list
}

pub fn boot_metadata_list(dict: &fdr::Dictionary) -> Vec<BootMetadata> {
    let mut list = Vec::new();
    for i in 0.. {
        let prefix = format!("boot_metadata.{}", i);
        if let Some(zbi_type) = get_uint32(dict, &format!("{}.zbi_type", prefix)) {
            let zbi_extra = get_uint32(dict, &format!("{}.zbi_extra", prefix));
            list.push(BootMetadata { zbi_type, zbi_extra });
        } else {
            break;
        }
    }
    list
}

pub fn pdev_constraints<'a>(
    config: &'a BoardConfig,
    node_name: &str,
) -> Option<&'a fdr::Dictionary> {
    config
        .aggregates
        .as_ref()?
        .iter()
        .find(|agg| {
            agg.provider.as_deref() == Some("pdev")
                && agg.service.as_deref() == Some("fuchsia.hardware.platform.device.Service")
        })
        .and_then(|agg| {
            agg.resources
                .as_ref()?
                .iter()
                .find(|res| res.node.as_deref() == Some(node_name))
                .and_then(|res| res.constraint.as_ref())
        })
}

pub fn is_node_force_enabled(dev_name: &str, enabled_nodes: &[String]) -> bool {
    enabled_nodes.iter().any(|n| {
        n == dev_name
            || n.trim_start_matches('/').replace('@', "-") == dev_name
            || n.rsplit('/').next().unwrap_or(n).replace('@', "-") == dev_name
    })
}

pub fn is_device_disabled(dev: &Device, enabled_nodes: &[String]) -> bool {
    if !dev.disabled.unwrap_or(false) {
        return false;
    }
    let name = dev.name.as_deref().unwrap_or("");
    !is_node_force_enabled(name, enabled_nodes)
}

#[cfg(target_os = "fuchsia")]
pub use board_structured_config::Config as StructuredConfig;

#[cfg(target_os = "fuchsia")]
pub mod parser;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_device_disabled_and_runtime_override() {
        let dev_enabled = Device { name: Some("dev_a".to_string()), ..Default::default() };
        let dev_explicit_enabled =
            Device { name: Some("dev_b".to_string()), disabled: Some(false), ..Default::default() };
        let dev_disabled = Device {
            name: Some("pcie-c500000".to_string()),
            disabled: Some(true),
            ..Default::default()
        };

        assert!(!is_device_disabled(&dev_enabled, &[]));
        assert!(!is_device_disabled(&dev_explicit_enabled, &[]));
        assert!(is_device_disabled(&dev_disabled, &[]));

        // Runtime override by exact DML node name
        assert!(!is_device_disabled(&dev_disabled, &["pcie-c500000".to_string()]));

        // Runtime override by devicetree path format (e.g. "/pcie@c500000")
        assert!(!is_device_disabled(&dev_disabled, &["/pcie@c500000".to_string()]));

        // Unrelated enabled_nodes entry leaves it disabled
        assert!(is_device_disabled(&dev_disabled, &["other-node".to_string()]));
    }

    #[test]
    fn test_bti_list() {
        let dict = fdr::Dictionary {
            entries: Some(vec![
                fdr::DictionaryEntry {
                    key: "btis.0.id".to_string(),
                    value: fdr::DictionaryValue::Int64(1),
                },
                fdr::DictionaryEntry {
                    key: "btis.0.iommu_id".to_string(),
                    value: fdr::DictionaryValue::Int64(10),
                },
                fdr::DictionaryEntry {
                    key: "btis.0.name".to_string(),
                    value: fdr::DictionaryValue::Str("dma_bti".to_string()),
                },
                fdr::DictionaryEntry {
                    key: "btis.1.id".to_string(),
                    value: fdr::DictionaryValue::Int64(2),
                },
                fdr::DictionaryEntry {
                    key: "btis.1.iommu_id".to_string(),
                    value: fdr::DictionaryValue::Int64(0),
                },
            ]),
            ..Default::default()
        };

        let btis = bti_list(&dict).unwrap();
        assert_eq!(btis.len(), 2);
        assert_eq!(btis[0].id, 1);
        assert_eq!(btis[0].name.as_deref(), Some("dma_bti"));
        assert_eq!(btis[0].iommu_id, 10);
        assert_eq!(btis[1].id, 2);
        assert_eq!(btis[1].name, None);
        assert_eq!(btis[1].iommu_id, 0);

        let dict_missing_iommu = fdr::Dictionary {
            entries: Some(vec![fdr::DictionaryEntry {
                key: "btis.0.id".to_string(),
                value: fdr::DictionaryValue::Int64(1),
            }]),
            ..Default::default()
        };
        assert!(bti_list(&dict_missing_iommu).is_err());

        let dict_missing_id = fdr::Dictionary {
            entries: Some(vec![
                fdr::DictionaryEntry {
                    key: "btis.0.name".to_string(),
                    value: fdr::DictionaryValue::Str("dma_bti".to_string()),
                },
                fdr::DictionaryEntry {
                    key: "btis.0.iommu_id".to_string(),
                    value: fdr::DictionaryValue::Int64(10),
                },
            ]),
            ..Default::default()
        };
        assert!(bti_list(&dict_missing_id).is_err());
    }
}
