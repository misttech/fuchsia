// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::subsystems::prelude::*;
use anyhow::Context;
use assembly_config_capabilities::{Config, ConfigValueType};
use assembly_config_schema::platform_settings::power_config::PowerConfig;
use assembly_constants::{BootfsDestination, FileEntry};

pub(crate) struct PowerManagementSubsystem;

impl DefineSubsystemConfiguration<PowerConfig> for PowerManagementSubsystem {
    fn define_configuration(
        context: &ConfigurationContext<'_>,
        config: &PowerConfig,
        builder: &mut dyn ConfigurationBuilder,
    ) -> anyhow::Result<()> {
        if let Some(path) = &config.metrics_logging_config {
            builder
                .package("metrics-logger")
                .config_data(FileEntry { source: path.into(), destination: "config.json".into() })
                .context("Setting metrics-logger config data path")?;
        }

        if let Some(energy_model_config) = &context.board_config.configuration.energy_model {
            builder
                .bootfs()
                .file(FileEntry {
                    source: energy_model_config.into(),
                    destination: BootfsDestination::EnergyModelConfig,
                })
                .context("Adding energy model config file for processor power management")?;
        }

        if let Some(power_manager_config) = &context.board_config.configuration.power_manager {
            builder
                .bootfs()
                .file(FileEntry {
                    source: power_manager_config.into(),
                    destination: BootfsDestination::PowerManagerNodeConfig,
                })
                .context("Adding power_manager config file")?;
        }

        if let Some(system_power_mode_config) =
            &context.board_config.configuration.system_power_mode
        {
            builder
                .bootfs()
                .file(FileEntry {
                    source: system_power_mode_config.into(),
                    destination: BootfsDestination::SystemPowerModeConfig,
                })
                .context("Adding system power mode configuration file")?;
        }

        if let Some(thermal_config) = &context.board_config.configuration.thermal {
            builder
                .bootfs()
                .file(FileEntry {
                    source: thermal_config.into(),
                    destination: BootfsDestination::PowerManagerThermalConfig,
                })
                .context("Adding power_manager's thermal config file")?;
        }

        if *context.feature_set_level != FeatureSetLevel::Embeddable {
            builder.platform_bundle("legacy_power_framework");
        }

        if config.enable_non_hermetic_testing {
            context.ensure_build_type_and_feature_set_level(
                &[BuildType::Eng],
                &[FeatureSetLevel::Bootstrap, FeatureSetLevel::Utility, FeatureSetLevel::Standard],
                "enable_non_hermetic_testing",
            )?;

            builder.platform_bundle("power_framework_broker");
            builder.platform_bundle("power_framework_testing_sag");
            builder.platform_bundle("power_test_platform_drivers");
        }

        if config.suspend_enabled {
            context.ensure_build_type_and_feature_set_level(
                &[BuildType::Eng, BuildType::UserDebug],
                &[FeatureSetLevel::Bootstrap, FeatureSetLevel::Utility, FeatureSetLevel::Standard],
                "suspend_enabled",
            )?;

            builder.platform_bundle("power_framework_broker");

            builder.set_config_capability(
                "fuchsia.power.WaitForSuspendingToken",
                Config::new(
                    ConfigValueType::Bool,
                    context.board_config.provides_feature("fuchsia::suspending_token").into(),
                ),
            )?;

            builder.set_config_capability(
                "fuchsia.power.UseSuspender",
                Config::new(
                    ConfigValueType::Bool,
                    context.board_config.provides_feature("fuchsia::suspender").into(),
                ),
            )?;

            if context.build_type == &BuildType::Eng {
                builder.platform_bundle("topology_test_daemon");
            }

            match context.feature_set_level {
                FeatureSetLevel::Embeddable | FeatureSetLevel::Bootstrap => {}
                FeatureSetLevel::Utility | FeatureSetLevel::Standard => {
                    // Include only when the base package set is available as
                    // these require the core realm, and base package functionality.
                    builder.platform_bundle("power_framework_development_support");
                }
            }

            builder.platform_bundle("power_framework_sag");
        }

        if let Some(cpu_manager_config) = &context.board_config.configuration.cpu_manager {
            context.ensure_build_type_and_feature_set_level(
                &[BuildType::Eng, BuildType::UserDebug, BuildType::User],
                &[FeatureSetLevel::Bootstrap, FeatureSetLevel::Utility, FeatureSetLevel::Standard],
                "cpu_manager",
            )?;

            builder.platform_bundle("cpu_manager");
            builder
                .bootfs()
                .file(FileEntry {
                    source: cpu_manager_config.into(),
                    destination: BootfsDestination::CpuManagerNodeConfig,
                })
                .context("Adding cpu_manager config file")?;
        }

        builder.set_config_capability(
            "fuchsia.power.SuspendEnabled",
            Config::new(ConfigValueType::Bool, config.suspend_enabled.into()),
        )?;

        builder.set_config_capability(
            "fuchsia.power.StoragePowerManagementEnabled",
            Config::new(
                ConfigValueType::Bool,
                serde_json::Value::Bool(
                    config.storage_power_management_enabled
                        && context.board_config.provides_feature("fuchsia::suspending_token"),
                ),
            ),
        )?;

        if let (Some(config), FeatureSetLevel::Standard) =
            (&context.board_config.configuration.power_metrics_recorder, &context.feature_set_level)
        {
            builder.platform_bundle("power_metrics_recorder");
            builder.package("metrics-logger-standalone").config_data(FileEntry {
                source: config.into(),
                destination: "config.json".to_string(),
            })?;
        }

        // Include fake-battery driver through a platform AIB.
        if context.board_config.provides_feature("fuchsia::fake_battery") {
            // We only need this driver feature in the utility / standard feature set levels.
            if *context.feature_set_level == FeatureSetLevel::Standard
                || *context.feature_set_level == FeatureSetLevel::Utility
            {
                builder.platform_bundle("fake_battery_driver");
            }
        }

        // Include fake-power-sensor through a platform AIB.
        if context.board_config.provides_feature("fuchsia::fake_power_sensor")
            && *context.feature_set_level == FeatureSetLevel::Standard
            && *context.build_type == BuildType::Eng
        {
            builder.platform_bundle("fake_power_sensor");
        }

        Ok(())
    }
}
