// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! FFX plugin for examining product bundles, which are distributable containers for a product's
//! images and packages, and can be used to emulate, flash, or update a product.

use anyhow::{Result, bail};
use camino::Utf8Path;
use ffx_writer::{SimpleWriter, ToolIO as _};
use fho::{FfxMain, FfxTool};
use product_bundle::{ProductBundle, ProductBundleV2};
use sdk_metadata::VirtualDevice;
use serde_json::to_string_pretty;
use std::io::{stdin, stdout};
use structured_ui::{Notice, Presentation, TableRows};

mod args;
pub use args::ShowCommand;

/// `ffx product show` sub-command.
#[derive(FfxTool)]
pub struct ProductShowTool {
    #[command]
    pub cmd: ShowCommand,
}

/// Create a product bundle.
impl FfxMain for ProductShowTool {
    type Writer = SimpleWriter;

    type Error = ::fho::Error;

    async fn main(self, mut writer: Self::Writer) -> fho::Result<()> {
        let mut input = stdin();
        let mut output = stdout();
        let mut err_out = writer.stderr();
        let ui = structured_ui::TextUi::new(&mut input, &mut output, &mut err_out);
        pb_show_impl(&ui, &self.cmd).await.map_err(Into::into)
    }
}

async fn pb_show_impl<I>(ui: &I, cmd: &ShowCommand) -> Result<()>
where
    I: structured_ui::Interface,
{
    log::debug!("pb_show");
    if !product_bundle::is_gcs_uri(&cmd.product_bundle_path) && !cmd.product_bundle_path.exists() {
        let mut note = Notice::builder();
        note.title(format!("File does not exist: {}", cmd.product_bundle_path));
        ui.present(&Presentation::Notice(note)).expect("Problem presenting the note.");
        return Ok(());
    }
    let product_bundle = ProductBundle::try_load_from(&cmd.product_bundle_path)?;

    if cmd.devices {
        if let Err(e) = list_virtual_devices(&cmd.product_bundle_path, &product_bundle, ui).await {
            let mut note = Notice::builder();
            note.title(format!("{:?}", e));
            ui.present(&Presentation::Notice(note)).expect("Problem presenting the note.");
            return Ok(());
        }
    }
    if let Some(device_name) = &cmd.device {
        if let Err(e) =
            virtual_device_details(&cmd.product_bundle_path, &product_bundle, ui, device_name).await
        {
            let mut note = Notice::builder();
            note.title(format!("{:?}", e));
            ui.present(&Presentation::Notice(note)).expect("Problem presenting the note.");
            return Ok(());
        }
    }
    Ok(())
}

async fn retrieve_v2_virtual_devices(
    product_bundle_source: &Utf8Path,
    product_bundle: &ProductBundleV2,
) -> Result<Vec<VirtualDevice>> {
    let path = product_bundle.get_virtual_devices_path();
    product_bundle::load_virtual_devices(product_bundle_source, &path)
}

/// Given a product bundle, print the names and descriptions of all of the
/// virtual device specifications linked to that product bundle.
pub async fn list_virtual_devices<I>(
    product_bundle_source: &Utf8Path,
    product_bundle: &ProductBundle,
    ui: &I,
) -> Result<()>
where
    I: structured_ui::Interface,
{
    let virtual_devices = match product_bundle {
        ProductBundle::V2(product_bundle) => {
            retrieve_v2_virtual_devices(product_bundle_source, product_bundle).await?
        }
    };
    let mut table = TableRows::builder();
    for entry in virtual_devices {
        match entry {
            VirtualDevice::V1(v) => {
                table.row(vec![
                    v.name,
                    v.description.unwrap_or_else(|| "No description.".to_string()),
                ]);
            }
        }
    }
    ui.present(&Presentation::Table(table.clone()))?;
    Ok(())
}

/// Given a device name, print the json-formatted contents of the virtual
/// device which matches that name. If no such device exists, return an error.
pub async fn virtual_device_details<I>(
    product_bundle_source: &Utf8Path,
    product_bundle: &ProductBundle,
    ui: &I,
    device_name: &str,
) -> Result<()>
where
    I: structured_ui::Interface,
{
    let virtual_devices = match product_bundle {
        ProductBundle::V2(product_bundle) => {
            retrieve_v2_virtual_devices(product_bundle_source, product_bundle).await?
        }
    };
    let selected = virtual_devices.iter().find(|v| v.name() == device_name);
    if selected.is_none() {
        bail!("Couldn't find a virtual device named {}", device_name);
    }
    let mut notice = Notice::builder();
    let text = to_string_pretty(&selected)?;
    notice.message(text);
    ui.present(&Presentation::Notice(notice))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use assembly_partitions_config::PartitionsConfig;
    use serde_json::json;
    use std::fs::File;
    use std::io::Write;
    use structured_ui::MockUi;
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    const VIRTUAL_DEVICE_VALID: &str =
        include_str!("../../../../../../../build/sdk/meta/test_data/virtual_device.json");

    #[fuchsia::test]
    async fn test_pb_show_local_dir_and_zip() {
        let tmp = tempfile::tempdir().unwrap();
        let pb_dir = Utf8Path::from_path(tmp.path()).unwrap().join("pb_dir");
        std::fs::create_dir_all(&pb_dir).unwrap();

        let vd_manifest =
            json!({"recommended": "device", "device_paths": {"device": "device.json"}});
        std::fs::write(
            pb_dir.join("virtual_device_manifest.json"),
            serde_json::to_vec(&vd_manifest).unwrap(),
        )
        .unwrap();
        std::fs::write(pb_dir.join("device.json"), VIRTUAL_DEVICE_VALID.as_bytes()).unwrap();

        let pb = ProductBundle::V2(ProductBundleV2 {
            product_name: "show-test".to_string(),
            product_version: "1".to_string(),
            partitions: PartitionsConfig::default(),
            sdk_version: "1".to_string(),
            system_a: None,
            system_b: None,
            system_r: None,
            platform_tools_a: vec![],
            platform_tools_b: vec![],
            platform_tools_r: vec![],
            repositories: vec![],
            update_package_hash: None,
            virtual_devices_path: Some(pb_dir.join("virtual_device_manifest.json")),
            release_info: None,
        });
        pb.write(&pb_dir).unwrap();

        let zip_path = Utf8Path::from_path(tmp.path()).unwrap().join("pb.zip");
        {
            let mut zip = ZipWriter::new(File::create(&zip_path).unwrap());
            let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
            zip.start_file("product_bundle/virtual_device_manifest.json", opts).unwrap();
            zip.write_all(&serde_json::to_vec(&vd_manifest).unwrap()).unwrap();
            zip.start_file("product_bundle/device.json", opts).unwrap();
            zip.write_all(VIRTUAL_DEVICE_VALID.as_bytes()).unwrap();
            zip.start_file("product_bundle/product_bundle.json", opts).unwrap();
            zip.write_all(&std::fs::read(pb_dir.join("product_bundle.json")).unwrap()).unwrap();
            zip.finish().unwrap();
        }

        for path in [pb_dir, zip_path] {
            let ui = MockUi::new();
            let cmd = ShowCommand {
                devices: true,
                device: Some("device".to_string()),
                product_bundle_path: path,
            };
            pb_show_impl(&ui, &cmd).await.unwrap();
        }
    }
}
