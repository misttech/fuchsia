// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Error, anyhow};
use fidl_fuchsia_audio_device as fadevice;
use fuchsia_audio::Registry;
use fuchsia_audio::device::Type;
use fuchsia_component::client::connect_to_protocol;
use serde::Deserialize;
use std::str::FromStr;

#[derive(Deserialize, Debug)]
struct Config {
    expected_devices: Vec<ExpectedDevice>,
}

#[derive(Deserialize, Debug)]
struct ExpectedDevice {
    device_type: String,
    name: Option<String>,
    manufacturer: Option<String>,
    product: Option<String>,
}

#[fuchsia::test]
// Test Coverage: Audio Device Registry & Drivers (Enumeration)
async fn test_audio_device_enumeration() -> Result<(), Error> {
    // Read config
    let config_path = "/pkg/data/config.json";
    let config_str = std::fs::read_to_string(config_path)
        .context(format!("Failed to read config from {}", config_path))?;
    let config: Config =
        serde_json::from_str(&config_str).context("Failed to parse config.json")?;

    log::info!("Looking for {:?}", config.expected_devices);

    // Try Registry
    let registry_proxy = connect_to_protocol::<fadevice::RegistryMarker>()
        .context("Failed to connect to fuchsia.audio.device.Registry")?;
    let registry = Registry::new(registry_proxy);
    log::info!("Connected to Audio Device Registry");

    // Get all devices
    let infos = registry.device_infos().await;
    let found_devices: Vec<_> = infos.values().cloned().collect();

    log::info!("Found devices:");
    for dev in &found_devices {
        log::info!(
            "  Type: {:?}, Name: {:?}, Manufacturer: {:?}, Product: {:?}",
            dev.device_type(),
            dev.device_name(),
            dev.0.manufacturer,
            dev.0.product
        );
    }

    // Verification Logic
    let mut missing_devices = Vec::new();

    // TODO(https://fxbug.dev/479279117): Extend this to support checking topologies, element types for the devices.

    let total_expected = config.expected_devices.len();
    for (i, expected) in config.expected_devices.into_iter().enumerate() {
        let expected_device_type = Type::from_str(&expected.device_type).map_err(|e| {
            anyhow!("Test config contains invalid device type '{}': {}", expected.device_type, e)
        })?;

        let mut found = false;
        for info in &found_devices {
            let type_match = info.device_type() == expected_device_type;
            if !type_match {
                log::info!(
                    "    Device '{}': type {} does not match expected {}",
                    info.device_name(),
                    info.device_type(),
                    expected_device_type
                );
            }

            let name_match = match &expected.name {
                Some(n) => info.device_name() == n,
                None => true,
            };
            if !name_match {
                log::info!(
                    "    Device '{}': Name does not match expected {:?}",
                    info.device_name(),
                    expected.name,
                );
            }

            let manufacturer_match = match &expected.manufacturer {
                Some(m) => info.0.manufacturer.as_ref().map(|rm| rm == m).unwrap_or(false),
                None => true,
            };
            if !manufacturer_match {
                log::info!(
                    "    Device '{}': Manufacturer {:?} does not match expected {:?}",
                    info.device_name(),
                    info.0.manufacturer,
                    expected.manufacturer,
                );
            }

            let product_match = match &expected.product {
                Some(p) => info.0.product.as_ref().map(|rp| rp == p).unwrap_or(false),
                None => true,
            };
            if !product_match {
                log::info!(
                    "    Device '{}': Product {:?} does not match expected {:?}",
                    info.device_name(),
                    info.0.product,
                    expected.product,
                );
            }

            if type_match && name_match && manufacturer_match && product_match {
                found = true;
                log::info!(
                    "Successfully detected an expected device ({}/{}).",
                    i + 1,
                    total_expected
                );
                log::info!("  Detected: {:?}", info);
                log::info!("  Expected: {:?}", expected);
                break;
            }
        }

        if !found {
            log::error!("Failed to find expected device: {:?}", expected);
            missing_devices.push(expected);
        }
    }

    if !missing_devices.is_empty() {
        panic!("Failed to find the following expected devices: {:?}", missing_devices);
    }

    log::info!("All expected devices found!");
    Ok(())
}
