// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::subsystems::prelude::*;
use assembly_config_capabilities::{Config, ConfigNestedValueType, ConfigValueType};
use assembly_config_schema::platform_settings::starnix_config::PlatformStarnixConfig;
use assembly_config_schema::platform_settings::usb_config::{UsbConfig, UsbPeripheralFunction};
use assembly_constants::BoardFeature;

pub(crate) struct UsbSubsystem;

impl DefineSubsystemConfiguration<(&UsbConfig, &PlatformStarnixConfig)> for UsbSubsystem {
    fn define_configuration(
        context: &ConfigurationContext<'_>,
        (usb, starnix): &(&UsbConfig, &PlatformStarnixConfig),
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
        if context.board_config.provides_feature(BoardFeature::Xhci) {
            builder.platform_bundle("xhci_driver")?;
        }
        if context.board_config.provides_feature(BoardFeature::UsbHost) {
            builder.platform_bundle("usb_bus_driver")?;
            builder.platform_bundle("usb_host_drivers")?;
        }
        if context.board_config.provides_feature(BoardFeature::UsbBus) {
            builder.platform_bundle("usb_bus_driver")?;
        }
        let enable_policy = usb.enable_policy.unwrap_or(
            matches!(
                context.feature_set_level,
                FeatureSetLevel::Utility | FeatureSetLevel::Standard
            ) && matches!(context.build_type, BuildType::UserDebug | BuildType::Eng),
        );

        if context.board_config.provides_feature(BoardFeature::UsbPeripheralSupport) {
            if enable_policy {
                builder.platform_bundle("usb_policy")?;
                if starnix.enabled {
                    builder.platform_bundle("usb_policy_starnix")?;
                }
            }
            for function in usb.peripheral.functions() {
                match (function, context.feature_set_level, context.build_type) {
                    (UsbPeripheralFunction::Adb, _, _) => {
                        builder.platform_bundle("usb_adb_function")?
                    }
                    (
                        UsbPeripheralFunction::Cdc,
                        FeatureSetLevel::Bootstrap | FeatureSetLevel::Embeddable,
                        BuildType::UserDebug | BuildType::Eng,
                    ) => {
                        builder.platform_bundle("usb_cdc_function_boot")?;
                    }
                    (
                        UsbPeripheralFunction::Cdc,
                        FeatureSetLevel::Utility | FeatureSetLevel::Standard,
                        _,
                    ) => {
                        builder.platform_bundle("usb_cdc_function_base")?;
                    }
                    (UsbPeripheralFunction::Fastboot, _, _) => {
                        builder.platform_bundle("fastbootd_usb_support")?
                    }
                    (
                        UsbPeripheralFunction::VsockBridge,
                        FeatureSetLevel::Utility | FeatureSetLevel::Standard,
                        BuildType::UserDebug | BuildType::Eng,
                    ) => {
                        builder.platform_bundle("core_realm_development_access_rcs_usb")?;
                        // Dependency of ^
                        builder.platform_bundle("vsock_service")?;
                    }
                    (UsbPeripheralFunction::Rndis, _, _) => {
                        builder.platform_bundle("usb_rndis_function")?
                    }
                    (
                        UsbPeripheralFunction::Test,
                        FeatureSetLevel::Utility | FeatureSetLevel::Standard,
                        BuildType::Eng,
                    ) => builder.platform_bundle("usb_zero_function_base")?,
                    (UsbPeripheralFunction::Ums, _, _) => {
                        builder.platform_bundle("usb_ums_function")?
                    }
                    _ => (),
                }
            }
        }

        match context.feature_set_level {
            FeatureSetLevel::Bootstrap | FeatureSetLevel::Embeddable => {
                builder.platform_bundle("usb_peripheral_drivers_boot")?;
            }
            FeatureSetLevel::Utility | FeatureSetLevel::Standard => {
                builder.platform_bundle("usb_peripheral_drivers_base")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subsystems::ConfigurationBuilderImpl;
    use assembly_config_schema::board_config::BoardConfig;
    use camino::Utf8Path;
    use tempfile::tempdir;

    fn setup_test_context<'a>(
        board_config: &'a BoardConfig,
        gendir_path: &'a Utf8Path,
    ) -> ConfigurationContext<'a> {
        ConfigurationContext {
            feature_set_level: &FeatureSetLevel::Standard,
            build_type: &BuildType::Eng,
            board_config,
            gendir: gendir_path.to_path_buf(),
            resource_dir: Default::default(),
            developer_only_options: Default::default(),
        }
    }

    #[test]
    fn test_usb_features_usb_bus_and_xhci() {
        let board_config = BoardConfig {
            provided_features: vec![
                BoardFeature::Xhci.as_ref().to_string(),
                BoardFeature::UsbBus.as_ref().to_string(),
            ],
            ..Default::default()
        };
        let gendir = tempdir().unwrap();
        let gendir_path = Utf8Path::from_path(gendir.path()).unwrap();
        let context = setup_test_context(&board_config, gendir_path);
        let usb_config = UsbConfig::default();
        let starnix_config = PlatformStarnixConfig::default();
        let mut builder: ConfigurationBuilderImpl = Default::default();

        let result = UsbSubsystem::define_configuration(
            &context,
            &(&usb_config, &starnix_config),
            &mut builder,
        );
        assert!(result.is_ok());

        let completed = builder.build();
        assert!(completed.bundles.contains("xhci_driver"));
        assert!(completed.bundles.contains("usb_bus_driver"));
        assert!(!completed.bundles.contains("usb_host_drivers"));
    }

    #[test]
    fn test_usb_host_feature_includes_usb_bus() {
        let board_config = BoardConfig {
            provided_features: vec![BoardFeature::UsbHost.as_ref().to_string()],
            ..Default::default()
        };
        let gendir = tempdir().unwrap();
        let gendir_path = Utf8Path::from_path(gendir.path()).unwrap();
        let context = setup_test_context(&board_config, gendir_path);
        let usb_config = UsbConfig::default();
        let starnix_config = PlatformStarnixConfig::default();
        let mut builder: ConfigurationBuilderImpl = Default::default();

        let result = UsbSubsystem::define_configuration(
            &context,
            &(&usb_config, &starnix_config),
            &mut builder,
        );
        assert!(result.is_ok());

        let completed = builder.build();
        assert!(completed.bundles.contains("usb_bus_driver"));
        assert!(completed.bundles.contains("usb_host_drivers"));
        assert!(!completed.bundles.contains("xhci_driver"));
    }
}
