// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Error, anyhow, bail};
use fidl_fuchsia_hardware_usb_peripheral as peripheral;
use serde::{Deserialize, Serialize};
use std::fs;

pub const MAX_FUNCTIONS: usize = 8;
pub const MAX_FUNCTION_NAME_LEN: usize = 32;

// Communication Class (CDC / RNDIS)
pub const USB_CLASS_COMM: u8 = 0x02;
pub const USB_CDC_SUBCLASS_ACM: u8 = 0x02;
pub const USB_CDC_SUBCLASS_ETHERNET: u8 = 0x06;
pub const USB_CDC_PROTOCOL_NONE: u8 = 0x00;
pub const USB_PROTOCOL_RNDIS: u8 = 0xff;

// Mass Storage Class (UMS)
pub const USB_CLASS_MSC: u8 = 0x08;
pub const USB_SUBCLASS_MSC_SCSI: u8 = 0x06;
pub const USB_PROTOCOL_MSC_BULK_ONLY: u8 = 0x50;

// Vendor Specific Class (Test, ADB, VSOCK)
pub const USB_CLASS_VENDOR: u8 = 0xff;
pub const USB_SUBCLASS_TEST: u8 = 0x00;
pub const USB_PROTOCOL_TEST: u8 = 0x00;
pub const USB_SUBCLASS_ADB: u8 = 0x42;
pub const USB_PROTOCOL_ADB: u8 = 0x01;
pub const USB_SUBCLASS_VSOCK: u8 = 0x43;
pub const USB_PROTOCOL_VSOCK: u8 = 0x00;

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct UsbConfigJson {
    pub functions: Vec<String>,
}

pub fn function_name_to_descriptor(name: &str) -> Result<peripheral::FunctionDescriptor, Error> {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "cdc" | "cdc_eth" | "cdc-eth" => Ok(peripheral::FunctionDescriptor {
            interface_class: USB_CLASS_COMM,
            interface_subclass: USB_CDC_SUBCLASS_ETHERNET,
            interface_protocol: USB_CDC_PROTOCOL_NONE,
        }),
        "rndis" => Ok(peripheral::FunctionDescriptor {
            interface_class: USB_CLASS_COMM,
            interface_subclass: USB_CDC_SUBCLASS_ACM,
            interface_protocol: USB_PROTOCOL_RNDIS,
        }),
        "ums" | "mass_storage" => Ok(peripheral::FunctionDescriptor {
            interface_class: USB_CLASS_MSC,
            interface_subclass: USB_SUBCLASS_MSC_SCSI,
            interface_protocol: USB_PROTOCOL_MSC_BULK_ONLY,
        }),
        "test" | "usb_zero" | "usb-zero" | "zero" => Ok(peripheral::FunctionDescriptor {
            interface_class: USB_CLASS_VENDOR,
            interface_subclass: USB_SUBCLASS_TEST,
            interface_protocol: USB_PROTOCOL_TEST,
        }),
        "adb" => Ok(peripheral::FunctionDescriptor {
            interface_class: USB_CLASS_VENDOR,
            interface_subclass: USB_SUBCLASS_ADB,
            interface_protocol: USB_PROTOCOL_ADB,
        }),
        "vsock" | "ffx" => Ok(peripheral::FunctionDescriptor {
            interface_class: USB_CLASS_VENDOR,
            interface_subclass: USB_SUBCLASS_VSOCK,
            interface_protocol: USB_PROTOCOL_VSOCK,
        }),
        _ => {
            if let Some(spec) = lower.strip_prefix("custom_") {
                let mut parts = spec.split('_');
                if let (Some(cls_str), Some(sub_str), Some(proto_str), None) =
                    (parts.next(), parts.next(), parts.next(), parts.next())
                {
                    fn parse_hex(s: &str) -> Result<u8, Error> {
                        let hex_str = s.strip_prefix("0x").unwrap_or(s);
                        u8::from_str_radix(hex_str, 16)
                            .with_context(|| format!("Invalid hex byte '{}'", s))
                    }
                    let cls = parse_hex(cls_str)?;
                    let sub = parse_hex(sub_str)?;
                    let proto = parse_hex(proto_str)?;
                    return Ok(peripheral::FunctionDescriptor {
                        interface_class: cls,
                        interface_subclass: sub,
                        interface_protocol: proto,
                    });
                }
                bail!("Invalid custom function descriptor format: {}", name);
            }
            Err(anyhow!("Unknown USB function: {}", name))
        }
    }
}

pub fn descriptor_to_function_name(desc: &peripheral::FunctionDescriptor) -> String {
    match (desc.interface_class, desc.interface_subclass, desc.interface_protocol) {
        (USB_CLASS_COMM, USB_CDC_SUBCLASS_ETHERNET, USB_CDC_PROTOCOL_NONE) => "cdc".to_string(),
        (USB_CLASS_COMM, USB_CDC_SUBCLASS_ACM, USB_PROTOCOL_RNDIS) => "rndis".to_string(),
        (USB_CLASS_MSC, USB_SUBCLASS_MSC_SCSI, USB_PROTOCOL_MSC_BULK_ONLY) => "ums".to_string(),
        (USB_CLASS_VENDOR, USB_SUBCLASS_TEST, USB_PROTOCOL_TEST) => "test".to_string(),
        (USB_CLASS_VENDOR, USB_SUBCLASS_ADB, USB_PROTOCOL_ADB) => "adb".to_string(),
        (USB_CLASS_VENDOR, USB_SUBCLASS_VSOCK, USB_PROTOCOL_VSOCK) => "vsock".to_string(),
        (cls, sub, proto) => format!("custom_0x{:02x}_0x{:02x}_0x{:02x}", cls, sub, proto),
    }
}

fn validate_config(config: UsbConfigJson) -> Result<UsbConfigJson, Error> {
    if config.functions.is_empty() {
        bail!("Configuration must specify at least one USB function");
    }
    if config.functions.len() > MAX_FUNCTIONS {
        bail!(
            "Configuration exceeds maximum of {} USB functions (got {})",
            MAX_FUNCTIONS,
            config.functions.len()
        );
    }
    if let Some(func) = config.functions.iter().find(|f| f.len() > MAX_FUNCTION_NAME_LEN) {
        bail!(
            "Function name '{}' exceeds maximum allowed length of {} bytes",
            func,
            MAX_FUNCTION_NAME_LEN
        );
    }
    Ok(config)
}

pub fn load_config_input(input: &str) -> Result<UsbConfigJson, Error> {
    let input = input.trim();
    if input.is_empty() {
        bail!("Configuration input cannot be empty");
    }

    let config = if input.starts_with('{') {
        serde_json::from_str::<UsbConfigJson>(input).context("Failed to parse JSON string")?
    } else if input.ends_with(".json")
        || input.contains('/')
        || std::path::Path::new(input).is_file()
    {
        let content = fs::read_to_string(input)
            .with_context(|| format!("Failed to read JSON config file '{}'", input))?;
        serde_json::from_str::<UsbConfigJson>(&content)
            .with_context(|| format!("Failed to parse JSON file '{}'", input))?
    } else {
        // Treat as comma-separated function list
        let functions: Vec<String> =
            input.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        UsbConfigJson { functions }
    };

    validate_config(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_config_json_string() -> Result<(), Error> {
        let json_str = r#"{"functions": ["cdc", "test"]}"#;
        let config = load_config_input(json_str)?;
        assert_eq!(config.functions, vec!["cdc", "test"]);
        Ok(())
    }

    #[test]
    fn test_load_config_comma_separated() -> Result<(), Error> {
        let input = " cdc, test, rndis ";
        let config = load_config_input(input)?;
        assert_eq!(config.functions, vec!["cdc", "test", "rndis"]);
        Ok(())
    }

    #[test]
    fn test_load_config_comma_separated_with_empty_elements() -> Result<(), Error> {
        let input = "cdc,,test, ";
        let config = load_config_input(input)?;
        assert_eq!(config.functions, vec!["cdc", "test"]);
        Ok(())
    }

    #[test]
    fn test_empty_config_rejected() {
        assert!(load_config_input("").is_err());
        assert!(load_config_input("   ").is_err());
        assert!(load_config_input(",,,").is_err());
    }

    #[test]
    fn test_bounds_validation_too_many_functions() {
        let functions = vec!["f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9"].join(",");
        assert!(load_config_input(&functions).is_err());
    }

    #[test]
    fn test_bounds_validation_name_too_long() {
        let long_name = "a".repeat(33);
        assert!(load_config_input(&long_name).is_err());
    }

    #[test]
    fn test_standard_functions_mapping() -> Result<(), Error> {
        let test_cases = [
            ("cdc", USB_CLASS_COMM, USB_CDC_SUBCLASS_ETHERNET, USB_CDC_PROTOCOL_NONE),
            ("cdc_eth", USB_CLASS_COMM, USB_CDC_SUBCLASS_ETHERNET, USB_CDC_PROTOCOL_NONE),
            ("cdc-eth", USB_CLASS_COMM, USB_CDC_SUBCLASS_ETHERNET, USB_CDC_PROTOCOL_NONE),
            ("rndis", USB_CLASS_COMM, USB_CDC_SUBCLASS_ACM, USB_PROTOCOL_RNDIS),
            ("ums", USB_CLASS_MSC, USB_SUBCLASS_MSC_SCSI, USB_PROTOCOL_MSC_BULK_ONLY),
            ("mass_storage", USB_CLASS_MSC, USB_SUBCLASS_MSC_SCSI, USB_PROTOCOL_MSC_BULK_ONLY),
            ("test", USB_CLASS_VENDOR, USB_SUBCLASS_TEST, USB_PROTOCOL_TEST),
            ("usb_zero", USB_CLASS_VENDOR, USB_SUBCLASS_TEST, USB_PROTOCOL_TEST),
            ("usb-zero", USB_CLASS_VENDOR, USB_SUBCLASS_TEST, USB_PROTOCOL_TEST),
            ("zero", USB_CLASS_VENDOR, USB_SUBCLASS_TEST, USB_PROTOCOL_TEST),
            ("adb", USB_CLASS_VENDOR, USB_SUBCLASS_ADB, USB_PROTOCOL_ADB),
            ("vsock", USB_CLASS_VENDOR, USB_SUBCLASS_VSOCK, USB_PROTOCOL_VSOCK),
            ("ffx", USB_CLASS_VENDOR, USB_SUBCLASS_VSOCK, USB_PROTOCOL_VSOCK),
        ];

        for (name, cls, sub, proto) in test_cases {
            let desc = function_name_to_descriptor(name)?;
            assert_eq!(desc.interface_class, cls);
            assert_eq!(desc.interface_subclass, sub);
            assert_eq!(desc.interface_protocol, proto);
        }
        Ok(())
    }

    #[test]
    fn test_case_insensitivity() -> Result<(), Error> {
        let desc = function_name_to_descriptor("CDC")?;
        assert_eq!(desc.interface_class, USB_CLASS_COMM);

        let desc = function_name_to_descriptor("Adb")?;
        assert_eq!(desc.interface_subclass, USB_SUBCLASS_ADB);

        let desc = function_name_to_descriptor("CUSTOM_0X01_0X02_0X03")?;
        assert_eq!(desc.interface_class, 0x01);
        assert_eq!(desc.interface_subclass, 0x02);
        assert_eq!(desc.interface_protocol, 0x03);
        Ok(())
    }

    #[test]
    fn test_custom_hex_parsing_and_roundtrip() -> Result<(), Error> {
        let name = "custom_0x12_0x34_0x56";
        let desc = function_name_to_descriptor(name)?;
        assert_eq!(desc.interface_class, 0x12);
        assert_eq!(desc.interface_subclass, 0x34);
        assert_eq!(desc.interface_protocol, 0x56);

        let roundtrip_name = descriptor_to_function_name(&desc);
        assert_eq!(roundtrip_name, name);
        Ok(())
    }

    #[test]
    fn test_invalid_names() {
        assert!(function_name_to_descriptor("invalid_fn").is_err());
        assert!(function_name_to_descriptor("custom_0x12_0x34").is_err());
        assert!(function_name_to_descriptor("custom_0xzz_0x12_0x34").is_err());
        assert!(function_name_to_descriptor("custom_0x100_0x12_0x34").is_err());
    }
}
