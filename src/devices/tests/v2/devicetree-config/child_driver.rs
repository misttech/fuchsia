// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fdf_component::{Driver, DriverContext, driver_register};
use fidl_fuchsia_driver_metadata as fmetadata;
use log::info;
use serde::Deserialize;
use zx::Status;

#[derive(Deserialize, Debug, PartialEq)]
struct MyConfig {
    string_prop: String,
    int_prop: i64,
    nested: NestedConfig,
}

#[derive(Deserialize, Debug, PartialEq)]
struct NestedConfig {
    prop: String,
}

struct ChildDriver;

driver_register!(ChildDriver);

impl Driver for ChildDriver {
    const NAME: &str = "devicetree-config-child";

    async fn start(context: DriverContext) -> Result<Self, Status> {
        log::info!("Child driver starting");

        // Connect to PlatformDevice service (default instance)
        let service = context
            .incoming
            .service::<fidl_fuchsia_hardware_platform_device::ServiceProxy>()
            .connect()
            .map_err(|e| {
                log::error!("Failed to connect to service: {:?}", e);
                Status::INTERNAL
            })?;
        let pdev = service.connect_to_device().map_err(|e| {
            log::error!("Failed to connect to device: {:?}", e);
            Status::INTERNAL
        })?;

        let result =
            pdev.get_metadata("fuchsia.driver.metadata.Dictionary").await.map_err(|e| {
                log::error!("Failed to get metadata: {:?}", e);
                Status::INTERNAL
            })?;
        let bytes = result.map_err(|s| {
            log::error!("Metadata result error: {:?}", s);
            Status::from_raw(s)
        })?;
        let dict: fmetadata::Dictionary = fidl::unpersist(&bytes).map_err(|e| {
            log::error!("Failed to unpersist dictionary: {:?}", e);
            Status::INTERNAL
        })?;

        log::info!("Dict: {:?}", dict);

        let config: MyConfig = fdf_metadata::from_dictionary(dict).map_err(|e| {
            log::error!("Failed to deserialize config: {:?}", e);
            Status::INTERNAL
        })?;

        log::info!("Config: {:?}", config);

        assert_eq!(config.string_prop, "hello");
        assert_eq!(config.int_prop, 0x12345678);
        assert_eq!(config.nested.prop, "world");

        log::info!("Child driver verified config successfully!");

        Ok(ChildDriver)
    }

    async fn stop(&self) {
        info!("Child driver stopping");
    }
}
