// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::subsystems::prelude::*;
use anyhow::Context;
use assembly_config_capabilities::{Config, ConfigValueType};
use assembly_config_schema::product_settings::ProductInfoConfig;
use assembly_constants::FileEntry;

pub(crate) struct HwinfoSubsystem;
impl DefineSubsystemConfiguration<Option<ProductInfoConfig>> for HwinfoSubsystem {
    fn define_configuration(
        context: &ConfigurationContext<'_>,
        config: &Option<ProductInfoConfig>,
        builder: &mut dyn ConfigurationBuilder,
    ) -> anyhow::Result<()> {
        // Set the product information that's returned by the hardware info protocol.
        // When not configured by the product, fall back to the default values in hwinfo.cml.
        let (name, model, manufacturer) = match config {
            Some(hwinfo) => (
                Config::new(ConfigValueType::String { max_size: 40 }, hwinfo.name.clone().into()),
                Config::new(ConfigValueType::String { max_size: 40 }, hwinfo.model.clone().into()),
                Config::new(
                    ConfigValueType::String { max_size: 40 },
                    hwinfo.manufacturer.clone().into(),
                ),
            ),
            None => (Config::new_void(), Config::new_void(), Config::new_void()),
        };
        builder.set_config_capability("fuchsia.hwinfo.ProductName", name)?;
        builder.set_config_capability("fuchsia.hwinfo.ProductModel", model)?;
        builder.set_config_capability("fuchsia.hwinfo.ProductManufacturer", manufacturer)?;

        // Set the board name to be returned by hardware_info.  If the board
        // doesn't provide a separate value, use the name of the board instead.
        let hwinfo_board_name =
            context.board_config.hardware_info.name.as_ref().unwrap_or(&context.board_config.name);
        let contents_value = serde_json::json!({
            "name": hwinfo_board_name,
            "revision": "1"
        });
        let contents =
            serde_json::to_string_pretty(&contents_value).context("Creating BoardInfo data")?;

        let boardinfo_path = context.get_gendir()?.join("board_info.json");
        std::fs::write(&boardinfo_path, contents).context("Writing board_info.json")?;

        builder
            .package("hwinfo")
            .config_data(FileEntry {
                source: boardinfo_path,
                destination: "board_config.json".into(),
            })
            .context("Adding board_config.json to config_data for 'hwinfo'")?;

        Ok(())
    }
}
