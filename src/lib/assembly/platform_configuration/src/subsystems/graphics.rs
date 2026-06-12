// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::subsystems::prelude::*;
use assembly_config_capabilities::{Config, ConfigNestedValueType, ConfigValueType};
use assembly_config_schema::platform_settings::graphics_config::GraphicsConfig;
use assembly_config_schema::platform_settings::ui_config::PlatformUiConfig;
use assembly_constants::BoardFeature;

pub(crate) struct GraphicsSubsystemConfig;
impl DefineSubsystemConfiguration<(&GraphicsConfig, &PlatformUiConfig)>
    for GraphicsSubsystemConfig
{
    fn define_configuration(
        context: &ConfigurationContext<'_>,
        config: &(&GraphicsConfig, &PlatformUiConfig),
        builder: &mut dyn ConfigurationBuilder,
    ) -> anyhow::Result<()> {
        let (graphics_config, ui_config) = *config;
        let virtcon_config = &graphics_config.virtual_console;

        let enable_virtual_console =
            match (context.build_type, context.feature_set_level, virtcon_config.enable) {
                // Use the value if one was specified.
                (_, _, Some(enable_virtual_console)) => enable_virtual_console,
                // If unspecified, virtcon is disabled if it's a user build-type
                (assembly_config_schema::BuildType::User, _, _) => false,
                // If neither of those, disable if we're targeting embeddable as well.
                (_, FeatureSetLevel::Embeddable, _) => false,
                // Otherwise, enable virtcon.
                (_, _, _) => true,
            };

        if enable_virtual_console {
            builder.platform_bundle("virtcon")?;
        }

        if *context.feature_set_level == FeatureSetLevel::Standard
            && context.board_config.provides_feature(BoardFeature::VulkanGpu)
        {
            builder.platform_bundle("vulkan_loader")?;
        }

        if context.board_config.provides_feature(BoardFeature::FakeDisplay) && ui_config.enabled {
            builder.platform_bundle("fake_display_stack_host")?;
        }

        builder.set_config_capability("fuchsia.virtcon.BufferCount", Config::new_void())?;

        if let Some(scheme) = &virtcon_config.color_scheme {
            builder.set_config_capability(
                "fuchsia.virtcon.ColorScheme",
                Config::new(ConfigValueType::String { max_size: 20 }, scheme.to_string().into()),
            )?;
        } else {
            builder.set_config_capability("fuchsia.virtcon.ColorScheme", Config::new_void())?;
        }

        builder.set_config_capability(
            "fuchsia.virtcon.Disable",
            Config::new(ConfigValueType::Bool, (!enable_virtual_console).into()),
        )?;

        if let Some(rotation) = context.board_config.platform.graphics.display.rotation {
            builder.set_config_capability(
                "fuchsia.virtcon.DisplayRotation",
                Config::new(ConfigValueType::Uint32, rotation.into()),
            )?;
        } else {
            builder.set_config_capability("fuchsia.virtcon.DisplayRotation", Config::new_void())?;
        }

        if !virtcon_config.dpi.is_empty() {
            builder.set_config_capability(
                "fuchsia.virtcon.DotsPerInch",
                Config::new(
                    ConfigValueType::Vector {
                        nested_type: ConfigNestedValueType::Uint32,
                        max_count: 10,
                    },
                    virtcon_config.dpi.clone().into(),
                ),
            )?;
        } else {
            builder.set_config_capability("fuchsia.virtcon.DotsPerInch", Config::new_void())?;
        }

        builder.set_config_capability("fuchsia.virtcon.FontSize", Config::new_void())?;
        builder.set_config_capability("fuchsia.virtcon.KeepLogVisible", Config::new_void())?;
        builder.set_config_capability("fuchsia.virtcon.ShowLogo", Config::new_bool(true))?;
        if let Some(keymap) = &virtcon_config.keymap {
            builder.set_config_capability(
                "fuchsia.virtcon.KeyMap",
                Config::new(ConfigValueType::String { max_size: 10 }, keymap.as_str().into()),
            )?;
        } else {
            builder.set_config_capability("fuchsia.virtcon.KeyMap", Config::new_void())?;
        }

        builder.set_config_capability("fuchsia.virtcon.KeyRepeat", Config::new_void())?;

        let rounded_corners = context.board_config.platform.graphics.display.rounded_corners;
        builder.set_config_capability(
            "fuchsia.virtcon.RoundedCorners",
            Config::new(ConfigValueType::Bool, rounded_corners.into()),
        )?;

        builder.set_config_capability("fuchsia.virtcon.ScrollbackRows", Config::new_void())?;

        match context.feature_set_level {
            FeatureSetLevel::Bootstrap | FeatureSetLevel::Embeddable => {
                builder.platform_bundle("display_drivers_boot")?;
            }
            FeatureSetLevel::Utility | FeatureSetLevel::Standard => {
                builder.platform_bundle("display_drivers_base")?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::ConfigurationBuilderImpl;
    use assembly_config_schema::BoardConfig;
    use assembly_config_schema::platform_settings::graphics_config::VirtconConfig;

    #[test]
    fn test_user_default() {
        let context = ConfigurationContext {
            feature_set_level: &FeatureSetLevel::Standard,
            build_type: &BuildType::User,
            ..ConfigurationContext::default_for_tests()
        };
        let config = GraphicsConfig { ..Default::default() };
        let mut builder = ConfigurationBuilderImpl::default();
        GraphicsSubsystemConfig::define_configuration(
            &context,
            &(&config, &PlatformUiConfig { enabled: true, ..Default::default() }),
            &mut builder,
        )
        .unwrap();
        let config = builder.build();
        assert_eq!(config.bundles, ["display_drivers_base".to_string()].into());
    }

    #[test]
    fn test_user_virtcon_disabled() {
        let context = ConfigurationContext {
            feature_set_level: &FeatureSetLevel::Standard,
            build_type: &BuildType::User,
            ..ConfigurationContext::default_for_tests()
        };
        let config = GraphicsConfig {
            virtual_console: VirtconConfig { enable: Some(false), ..Default::default() },
        };
        let mut builder = ConfigurationBuilderImpl::default();
        GraphicsSubsystemConfig::define_configuration(
            &context,
            &(&config, &PlatformUiConfig { enabled: true, ..Default::default() }),
            &mut builder,
        )
        .unwrap();
        let config = builder.build();
        assert_eq!(config.bundles, ["display_drivers_base".to_string()].into());
    }

    #[test]
    fn test_user_virtcon_enabled() {
        let context = ConfigurationContext {
            feature_set_level: &FeatureSetLevel::Standard,
            build_type: &BuildType::User,
            ..ConfigurationContext::default_for_tests()
        };
        let config = GraphicsConfig {
            virtual_console: VirtconConfig { enable: Some(true), ..Default::default() },
        };
        let mut builder = ConfigurationBuilderImpl::default();
        GraphicsSubsystemConfig::define_configuration(
            &context,
            &(&config, &PlatformUiConfig { enabled: true, ..Default::default() }),
            &mut builder,
        )
        .unwrap();
        let config = builder.build();
        assert_eq!(
            config.bundles,
            ["display_drivers_base".to_string(), "virtcon".to_string()].into()
        );
    }

    #[test]
    fn test_fake_display_ui_enabled() {
        let board_config = BoardConfig {
            provided_features: vec!["fuchsia::fake_display".to_string()],
            ..Default::default()
        };
        let context = ConfigurationContext {
            board_config: &board_config,
            ..ConfigurationContext::default_for_tests()
        };
        let config = GraphicsConfig { ..Default::default() };
        let mut builder = ConfigurationBuilderImpl::default();
        GraphicsSubsystemConfig::define_configuration(
            &context,
            &(&config, &PlatformUiConfig { enabled: true, ..Default::default() }),
            &mut builder,
        )
        .unwrap();
        let config = builder.build();
        assert_eq!(
            config.bundles,
            ["display_drivers_base".to_string(), "fake_display_stack_host".to_string()].into()
        );
    }

    #[test]
    fn test_fake_display_ui_disabled() {
        let board_config = BoardConfig {
            provided_features: vec!["fuchsia::fake_display".to_string()],
            ..Default::default()
        };
        let context = ConfigurationContext {
            board_config: &board_config,
            ..ConfigurationContext::default_for_tests()
        };
        let config = GraphicsConfig { ..Default::default() };
        let mut builder = ConfigurationBuilderImpl::default();
        GraphicsSubsystemConfig::define_configuration(
            &context,
            &(&config, &PlatformUiConfig { enabled: false, ..Default::default() }),
            &mut builder,
        )
        .unwrap();
        let config = builder.build();
        assert_eq!(config.bundles, ["display_drivers_base".to_string()].into());
    }
}
