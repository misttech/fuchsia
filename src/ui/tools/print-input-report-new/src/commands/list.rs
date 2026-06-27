// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Result};
use argh::FromArgs;
use fidl_fuchsia_input_report as fidl_input_report;
use fidl_fuchsia_io as fio;
use fuchsia_component::client::connect_to_named_protocol_at_dir_root;
use prettytable::Table;
use serde::{Serialize, Serializer};
use std::str::FromStr;

use crate::common::SERVICE_DIR;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Table,
    Csv,
}

impl FromStr for OutputFormat {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "table" => Ok(OutputFormat::Table),
            "csv" => Ok(OutputFormat::Csv),
            _ => Err(format!("Invalid output format: {}", s)),
        }
    }
}

#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "list")]
/// List input devices.
pub struct ListArgs {
    #[argh(option, default = "OutputFormat::Table")]
    /// output format: csv or table (default: table)
    pub output: OutputFormat,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct DeviceInfo {
    instance: String,
    #[serde(serialize_with = "to_hex")]
    vendor_id: u32,
    #[serde(serialize_with = "to_hex")]
    product_id: u32,
    serial_number: Option<String>,
    manufacturer_name: Option<String>,
    product_name: Option<String>,
}

fn to_hex<S>(val: &u32, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&format!("0x{:04x}", val))
}

pub async fn run(args: ListArgs) -> Result<()> {
    let service_dir =
        fuchsia_fs::directory::open_in_namespace(SERVICE_DIR, fio::Flags::PROTOCOL_DIRECTORY)
            .context(format!("Failed to open {}", SERVICE_DIR))?;
    let output = list_devices_to_string(&service_dir, args.output).await?;
    println!("{}", output);
    Ok(())
}

/// Fetches the info of all input device instances from `service_dir`, and
/// returns it as a formatted String.
async fn list_devices_to_string(
    service_dir: &fio::DirectoryProxy,
    output_format: OutputFormat,
) -> Result<String> {
    let mut devices = Vec::new();

    let entries = fuchsia_fs::directory::readdir(service_dir).await?;
    for entry in entries {
        let instance_name = entry.name;
        match get_device_info(service_dir, instance_name.clone()).await {
            Ok(device_info) => devices.push(device_info),
            Err(e) => eprintln!("Error reading device {}: {}", instance_name, e),
        }
    }

    // Generate CSV data in memory
    let mut writer = csv::Writer::from_writer(vec![]);
    for device_info in devices {
        writer.serialize(device_info)?;
    }
    let csv_bytes: Vec<u8> = writer.into_inner()?;
    let csv_string = String::from_utf8(csv_bytes)?;

    match output_format {
        OutputFormat::Csv => Ok(csv_string),
        OutputFormat::Table => {
            let mut table = Table::from_csv_string(&csv_string)
                .context("Failed to parse internal CSV string")?;
            table.set_format(*prettytable::format::consts::FORMAT_NO_BORDER_LINE_SEPARATOR);

            let mut table_bytes: Vec<u8> = vec![];
            table.print(&mut table_bytes)?;
            let table_string = String::from_utf8(table_bytes)?;
            Ok(table_string)
        }
    }
}

async fn get_device_info(
    service_dir: &fio::DirectoryProxy,
    instance: String,
) -> Result<DeviceInfo> {
    let device_path = format!("{}/input_device", instance);
    let device = connect_to_named_protocol_at_dir_root::<fidl_input_report::InputDeviceMarker>(
        service_dir,
        &device_path,
    )
    .context(format!("Failed to connect to {}", device_path))?;

    let descriptor = device
        .get_descriptor()
        .await
        .context(format!("Failed to get descriptor for {}", instance))?;

    let device_info = descriptor.device_information.expect("device_information is required.");

    Ok(DeviceInfo {
        instance,
        vendor_id: device_info.vendor_id.expect("vendor_id is required."),
        product_id: device_info.product_id.expect("product_id is required."),
        serial_number: device_info.serial_number,
        manufacturer_name: device_info.manufacturer_name,
        product_name: device_info.product_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl_fuchsia_input_report::{
        DeviceDescriptor, DeviceInformation, InputDeviceRequest, InputDeviceRequestStream,
    };
    use futures::TryStreamExt;
    use googletest::prelude::*;
    use std::sync::Arc;

    async fn handle_input_device_request(
        mut stream: InputDeviceRequestStream,
        device_info: DeviceInformation,
    ) {
        while let Ok(Some(request)) = stream.try_next().await {
            match request {
                InputDeviceRequest::GetDescriptor { responder } => {
                    let descriptor = DeviceDescriptor {
                        device_information: Some(device_info.clone()),
                        ..Default::default()
                    };
                    responder.send(&descriptor).unwrap();
                }
                _ => panic!("Unsupported"),
            }
        }
    }

    /// Serves a fake InputDevice that always returns the given `device_info`
    /// when asked for it.
    fn serve_fake_input_device(device_info: DeviceInformation) -> Arc<vfs::service::Service> {
        vfs::service::host(move |stream: InputDeviceRequestStream| {
            let device_info = device_info.clone();
            async move {
                handle_input_device_request(stream, device_info).await;
            }
        })
    }

    /// Creates a fake service directory with two input devices.
    /// Only the GetDescriptor method is supported on the devices for now.
    fn setup_fake_service_directory() -> fio::DirectoryProxy {
        let service_dir = vfs::pseudo_directory! {
            "instance1" => vfs::pseudo_directory! {
                "input_device" => serve_fake_input_device(DeviceInformation {
                    vendor_id: Some(0x1234),
                    product_id: Some(0x5678),
                    manufacturer_name: Some("Manuf1".to_string()),
                    product_name: Some("Prod1".to_string()),
                    serial_number: Some("Ser1".to_string()),
                    ..Default::default()
                }),
            },
            "instance2" => vfs::pseudo_directory! {
                "input_device" => serve_fake_input_device(DeviceInformation {
                    vendor_id: Some(0xaaaa),
                    product_id: Some(0xbbbb),
                    manufacturer_name: Some("Manuf2".to_string()),
                    product_name: Some("Prod2".to_string()),
                    serial_number: Some("Ser2".to_string()),
                    ..Default::default()
                }),
            }
        };
        vfs::directory::serve_read_only(service_dir, vfs::execution_scope::ExecutionScope::new())
    }

    #[gtest]
    #[fuchsia::test]
    async fn test_list_devices_to_string_csv() {
        let dir_proxy = setup_fake_service_directory();

        let csv_output = list_devices_to_string(&dir_proxy, OutputFormat::Csv).await.unwrap();
        let csv_lines: Vec<&str> = csv_output.lines().collect();
        assert_that!(csv_lines, len(eq(3)));

        // CSV header
        expect_eq!(
            csv_lines[0],
            "Instance,VendorId,ProductId,SerialNumber,ManufacturerName,ProductName"
        );

        // CSV rows
        expect_that!(
            csv_lines,
            contains_each![
                eq(&"instance1,0x1234,0x5678,Ser1,Manuf1,Prod1"),
                eq(&"instance2,0xaaaa,0xbbbb,Ser2,Manuf2,Prod2"),
            ]
        );
    }

    #[gtest]
    #[fuchsia::test]
    async fn test_list_devices_to_string_table() {
        let dir_proxy = setup_fake_service_directory();

        let table_output = list_devices_to_string(&dir_proxy, OutputFormat::Table).await.unwrap();

        let lines: Vec<&str> = table_output.lines().map(|line| line.trim()).collect();
        assert_that!(lines, len(eq(3)));
        expect_eq!(
            lines[0],
            "Instance  | VendorId | ProductId | SerialNumber | ManufacturerName | ProductName"
        );
        expect_that!(
            lines,
            contains_each![
                eq(&"instance1 | 0x1234   | 0x5678    | Ser1         | Manuf1           | Prod1"),
                eq(&"instance2 | 0xaaaa   | 0xbbbb    | Ser2         | Manuf2           | Prod2"),
            ]
        );
    }
}
