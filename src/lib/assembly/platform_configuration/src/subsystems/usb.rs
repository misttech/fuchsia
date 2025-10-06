// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::subsystems::prelude::*;
use assembly_config_capabilities::{Config, ConfigNestedValueType, ConfigValueType};
use assembly_config_schema::platform_settings::usb_config::{UsbConfig, UsbPeripheralFunction};

pub(crate) struct UsbSubsystem;

impl DefineSubsystemConfiguration<UsbConfig> for UsbSubsystem {
    fn define_configuration(
        context: &ConfigurationContext<'_>,
        usb: &UsbConfig,
        builder: &mut dyn ConfigurationBuilder,
    ) -> anyhow::Result<()> {
        let usb_peripheral_functions: Vec<String> =
            usb.peripheral.functions().iter().map(|x| x.to_string()).collect();

        builder.set_config_capability(
            "fuchsia.usb.PeripheralConfig.Functions",
            Config::new(
                ConfigValueType::Vector {
                    nested_type: ConfigNestedValueType::String { max_size: 32 },
                    max_count: 8,
                },
                usb_peripheral_functions.into(),
            ),
        )?;

        // Include xHCI driver through a platform AIB.
        if context.board_config.provides_feature("fuchsia::xhci") {
            builder.platform_bundle("xhci_driver");
        }
        if context.board_config.provides_feature("fuchsia::usb_host") {
            builder.platform_bundle("usb_host_drivers");
        }
        if context.board_config.provides_feature("fuchsia::usb_peripheral_support") {
            for function in usb.peripheral.functions() {
                match (function, context.feature_set_level, context.build_type) {
                    (UsbPeripheralFunction::Adb, _, _) => {
                        builder.platform_bundle("usb_adb_function")
                    }
                    (
                        UsbPeripheralFunction::Cdc,
                        FeatureSetLevel::Bootstrap | FeatureSetLevel::Embeddable,
                        BuildType::UserDebug | BuildType::Eng,
                    ) => {
                        builder.platform_bundle("usb_cdc_function_boot");
                    }
                    (
                        UsbPeripheralFunction::Cdc,
                        FeatureSetLevel::Utility | FeatureSetLevel::Standard,
                        _,
                    ) => {
                        builder.platform_bundle("usb_cdc_function_base");
                    }
                    (UsbPeripheralFunction::Fastboot, _, _) => {
                        builder.platform_bundle("fastbootd_usb_support")
                    }
                    (
                        UsbPeripheralFunction::VsockBridge,
                        FeatureSetLevel::Utility | FeatureSetLevel::Standard,
                        BuildType::UserDebug | BuildType::Eng,
                    ) => {
                        builder.platform_bundle("core_realm_development_access_rcs_usb");
                        // Dependency of ^
                        builder.platform_bundle("vsock_service");
                    }
                    (UsbPeripheralFunction::Rndis, _, _) => {
                        builder.platform_bundle("usb_rndis_function")
                    }
                    (UsbPeripheralFunction::Test, _, _) => {
                        anyhow::bail!(
                            "Product requested the \"test\" USB peripheral function which has no associated AIB"
                        )
                    }
                    (UsbPeripheralFunction::Ums, _, _) => {
                        builder.platform_bundle("usb_ums_function")
                    }
                    _ => (),
                }
            }
        }

        match context.feature_set_level {
            FeatureSetLevel::Bootstrap | FeatureSetLevel::Embeddable => {
                builder.platform_bundle("usb_peripheral_drivers_boot");
            }
            FeatureSetLevel::Utility | FeatureSetLevel::Standard => {
                builder.platform_bundle("usb_peripheral_drivers_base");
            }
        }
        Ok(())
    }
}
