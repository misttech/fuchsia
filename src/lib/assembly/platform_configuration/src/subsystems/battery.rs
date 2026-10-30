// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::subsystems::prelude::*;
use assembly_config_schema::platform_settings::battery_config::BatteryConfig;

pub(crate) struct BatterySubsystemConfig;
impl DefineSubsystemConfiguration<BatteryConfig> for BatterySubsystemConfig {
    fn define_configuration(
        context: &ConfigurationContext<'_>,
        config: &BatteryConfig,
        builder: &mut dyn ConfigurationBuilder,
    ) -> anyhow::Result<()> {
        if config.enabled {
            builder.platform_bundle("battery_manager")?;
        }

        // Include the batteryutil CLI in engineering builds when battery
        // support is enabled by the product or configured by the board.
        let has_battery_support =
            config.enabled || context.board_config.configuration.battery_manager.is_some();
        if has_battery_support
            && *context.build_type == BuildType::Eng
            && matches!(
                context.feature_set_level,
                FeatureSetLevel::Bootstrap | FeatureSetLevel::Utility | FeatureSetLevel::Standard
            )
        {
            builder.platform_bundle("batteryutil")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::ConfigurationBuilderImpl;
    use assembly_config_schema::{BoardConfig, BoardProvidedConfig};

    #[test]
    fn test_battery_disabled_by_default() {
        let context = ConfigurationContext {
            feature_set_level: &FeatureSetLevel::Standard,
            build_type: &BuildType::Eng,
            ..ConfigurationContext::default_for_tests()
        };
        let mut builder = ConfigurationBuilderImpl::default();
        BatterySubsystemConfig::define_configuration(
            &context,
            &BatteryConfig::default(),
            &mut builder,
        )
        .unwrap();
        let config = builder.build();
        assert!(!config.bundles.contains("battery_manager"));
        assert!(!config.bundles.contains("batteryutil"));
    }

    #[test]
    fn test_battery_enabled_by_product() {
        for (build_type, expect_batteryutil) in
            [(BuildType::Eng, true), (BuildType::UserDebug, false), (BuildType::User, false)]
        {
            let context = ConfigurationContext {
                feature_set_level: &FeatureSetLevel::Standard,
                build_type: &build_type,
                ..ConfigurationContext::default_for_tests()
            };
            let mut builder = ConfigurationBuilderImpl::default();
            BatterySubsystemConfig::define_configuration(
                &context,
                &BatteryConfig { enabled: true },
                &mut builder,
            )
            .unwrap();
            let config = builder.build();
            assert!(config.bundles.contains("battery_manager"));
            assert_eq!(
                config.bundles.contains("batteryutil"),
                expect_batteryutil,
                "{build_type:?}"
            );
        }
    }

    #[test]
    fn test_batteryutil_enabled_by_board_config() {
        let board_with_battery = BoardConfig {
            name: "Test Battery Board".into(),
            configuration: BoardProvidedConfig {
                battery_manager: Some("config/battery.json".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        for (feature_set_level, build_type, expected) in [
            (FeatureSetLevel::Bootstrap, BuildType::Eng, true),
            (FeatureSetLevel::Utility, BuildType::Eng, true),
            (FeatureSetLevel::Standard, BuildType::Eng, true),
            (FeatureSetLevel::Embeddable, BuildType::Eng, false),
            (FeatureSetLevel::Standard, BuildType::UserDebug, false),
            (FeatureSetLevel::Standard, BuildType::User, false),
        ] {
            let context = ConfigurationContext {
                feature_set_level: &feature_set_level,
                build_type: &build_type,
                board_config: &board_with_battery,
                ..ConfigurationContext::default_for_tests()
            };
            let mut builder = ConfigurationBuilderImpl::default();
            BatterySubsystemConfig::define_configuration(
                &context,
                &BatteryConfig::default(),
                &mut builder,
            )
            .unwrap();
            let config = builder.build();
            assert!(!config.bundles.contains("battery_manager"));
            assert_eq!(
                config.bundles.contains("batteryutil"),
                expected,
                "{feature_set_level:?}, {build_type:?}"
            );
        }
    }
}
