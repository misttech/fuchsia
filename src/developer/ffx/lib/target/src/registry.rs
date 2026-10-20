// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::TargetInfo;
use crate::info::TargetState;
use crate::store::{StoreError, load_json, save_json_atomically};
use addr::TargetAddr;
use ffx_config::EnvironmentContext;
use ffx_config::keys::REGISTERED_TARGETS_CONFIG;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

type Result<T> = std::result::Result<T, RegistryError>;

/// Errors returned when reading or writing the registered targets file.
#[derive(Debug, Error)]
pub enum RegistryError {
    #[error(transparent)]
    Store(#[from] StoreError),

    #[error("could not get shared_data path: {0}")]
    SharedData(String),
}

/// Minimal target device record stored in `$SHARED_DATA/targets.json`.
///
/// This record stores only the fields necessary for resolving symbolic target queries
/// (`nodename`, `id`, and `address`) using a plain string transport address so
/// external test orchestrators can read or author the file directly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredTarget {
    /// Target nodename, if known.
    #[serde(default)]
    pub nodename: Option<String>,
    /// Target identifier (hardware or virtual serial number), if known.
    #[serde(default)]
    pub id: Option<String>,
    /// Primary transport address for the target, formatted as a standard string.
    #[serde(
        serialize_with = "serialize_target_addr",
        deserialize_with = "deserialize_target_addr"
    )]
    pub address: TargetAddr,
}

impl RegisteredTarget {
    /// Constructs a [`RegisteredTarget`] from a [`TargetInfo`] if it is in a connectable
    /// state and has at least one transport address, selecting the highest-priority address.
    pub fn from_target_info(info: &TargetInfo) -> Option<Self> {
        if !matches!(info.target_state, TargetState::Product | TargetState::Unknown) {
            return None;
        }
        let address = info.addresses.iter().cloned().min_by(TargetAddr::compare_by_priority)?;
        Some(Self { nodename: info.nodename.clone(), id: info.serial_number.clone(), address })
    }
}

impl From<RegisteredTarget> for TargetInfo {
    fn from(registered: RegisteredTarget) -> Self {
        Self {
            nodename: registered.nodename,
            addresses: vec![registered.address],
            target_state: TargetState::Product,
            serial_number: registered.id,
            ..Default::default()
        }
    }
}

// Serializes a TargetAddr value as its standard string representation.
fn serialize_target_addr<S>(
    address: &TargetAddr,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error>
where
    S: Serializer,
{
    address.optional_port_str().serialize(serializer)
}

// Deserializes a target address string into a TargetAddr value.
fn deserialize_target_addr<'de, D>(deserializer: D) -> std::result::Result<TargetAddr, D::Error>
where
    D: Deserializer<'de>,
{
    String::deserialize(deserializer)?.parse().map_err(serde::de::Error::custom)
}

/// Returns the path to the registered targets file (`target.registered_targets`,
/// which defaults to `$SHARED_DATA/targets.json`) for the given environment context.
///
/// # Errors
///
/// Returns [`RegistryError::SharedData`] if the configuration path cannot be resolved
/// (such as in strict mode when neither `-c shared_data=<dir>` nor
/// `-c target.registered_targets=<path>` is specified).
pub fn targets_file_path(context: &EnvironmentContext) -> Result<PathBuf> {
    context
        .get::<PathBuf, _>(REGISTERED_TARGETS_CONFIG)
        .map_err(|e| RegistryError::SharedData(e.to_string()))
}

/// Atomically writes discovered connectable targets to `$SHARED_DATA/targets.json`
/// and returns the absolute path of the written file.
///
/// # Errors
///
/// Returns an error if the `shared_data` directory cannot be resolved or if writing and
/// persisting the file fails.
pub fn register_targets(
    context: &EnvironmentContext,
    infos: &[TargetInfo],
) -> std::result::Result<PathBuf, crate::FfxTargetCrateError> {
    let raw_path = targets_file_path(context)?;
    let targets_file = std::path::absolute(&raw_path)
        .map_err(|err| RegistryError::from(StoreError::CreateFile { path: raw_path, err }))?;
    let Some(dir) = targets_file.parent() else {
        return Err(RegistryError::from(StoreError::BadLocation { path: targets_file }).into());
    };
    fs::create_dir_all(dir).map_err(|err| {
        RegistryError::from(StoreError::CreateFile { path: targets_file.clone(), err })
    })?;
    let registered: Vec<RegisteredTarget> =
        infos.iter().filter_map(RegisteredTarget::from_target_info).collect();
    save_json_atomically(&registered, &targets_file).map_err(RegistryError::from)?;
    Ok(targets_file)
}

// Loads registered targets from `$SHARED_DATA/targets.json` and converts them into TargetInfo records.
pub(crate) fn load_registered_targets(path: &Path) -> Result<Vec<TargetInfo>> {
    let registered: Vec<RegisteredTarget> = load_json(path)?;
    Ok(registered.into_iter().map(TargetInfo::from).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffx_config::environment::ExecutableKind;
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    #[fuchsia::test]
    async fn test_register_and_load_registered_targets() {
        let dir = tempdir().unwrap();
        let shared_dir = dir.path().join("shared");
        let test_env = ffx_config::test_env()
            .runtime_config("shared_data", shared_dir.to_str().unwrap())
            .build()
            .unwrap();

        let addr_v4: TargetAddr = "192.168.1.50:8022".parse().unwrap();
        let product_info = TargetInfo {
            nodename: Some("fuchsia-node".to_string()),
            serial_number: Some("04012C12D45E".to_string()),
            addresses: vec![addr_v4.clone(), TargetAddr::UsbCtx(3)],
            target_state: TargetState::Product,
            ..Default::default()
        };
        let fastboot_info = TargetInfo {
            nodename: Some("fastboot-node".to_string()),
            serial_number: Some("FB123".to_string()),
            addresses: vec![addr_v4],
            target_state: TargetState::Fastboot,
            ..Default::default()
        };

        register_targets(&test_env.context, &[product_info, fastboot_info]).unwrap();

        let targets_file = targets_file_path(&test_env.context).unwrap();
        assert_eq!(targets_file, shared_dir.join("targets.json"));
        let loaded = load_registered_targets(&targets_file).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].nodename.as_deref(), Some("fuchsia-node"));
        assert_eq!(loaded[0].serial_number.as_deref(), Some("04012C12D45E"));
        assert_eq!(loaded[0].addresses, vec![TargetAddr::UsbCtx(3)]);
        assert_eq!(loaded[0].target_state, TargetState::Product);
    }

    #[fuchsia::test]
    fn test_load_registered_targets_minimal_infra_json() {
        let dir = tempdir().unwrap();
        let targets_file = dir.path().join("targets.json");
        let raw_json = r#"[
            {
                "id": "04012C12D45E",
                "address": "192.168.1.50:22"
            },
            {
                "nodename": "only-name",
                "address": "10.0.0.2"
            },
            {
                "id": "USB_DEV",
                "address": "usb:cid:4"
            }
        ]"#;
        fs::write(&targets_file, raw_json).unwrap();

        let loaded = load_registered_targets(&targets_file).unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].nodename, None);
        assert_eq!(loaded[0].serial_number.as_deref(), Some("04012C12D45E"));
        assert_eq!(loaded[0].addresses, vec!["192.168.1.50:22".parse::<TargetAddr>().unwrap()]);
        assert_eq!(loaded[1].nodename.as_deref(), Some("only-name"));
        assert_eq!(loaded[1].serial_number, None);
        assert_eq!(loaded[1].addresses, vec!["10.0.0.2:0".parse::<TargetAddr>().unwrap()]);
        assert_eq!(loaded[2].serial_number.as_deref(), Some("USB_DEV"));
        assert_eq!(loaded[2].addresses, vec![TargetAddr::UsbCtx(4)]);
    }

    #[fuchsia::test]
    async fn test_targets_file_path_strict_and_custom_config() {
        let strict_ctx =
            EnvironmentContext::strict(ExecutableKind::Test, ffx_config::ConfigMap::new()).unwrap();
        assert!(matches!(targets_file_path(&strict_ctx), Err(RegistryError::SharedData(_))));

        let config_map = [("shared_data".to_owned(), serde_json::json!("/tmp/strict_shared"))]
            .into_iter()
            .collect();
        let context_with_sd = EnvironmentContext::strict(ExecutableKind::Test, config_map).unwrap();
        assert_eq!(
            targets_file_path(&context_with_sd).unwrap(),
            PathBuf::from("/tmp/strict_shared/targets.json")
        );

        let custom_map = [(
            "target".to_owned(),
            serde_json::json!({"registered_targets": "/tmp/custom/my_targets.json"}),
        )]
        .into_iter()
        .collect();
        let context_with_custom =
            EnvironmentContext::strict(ExecutableKind::Test, custom_map).unwrap();
        assert_eq!(
            targets_file_path(&context_with_custom).unwrap(),
            PathBuf::from("/tmp/custom/my_targets.json")
        );
    }
}
