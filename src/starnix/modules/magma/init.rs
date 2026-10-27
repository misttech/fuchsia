// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{GpuDeviceConfig, MagmaFile, get_magma_params};
use anyhow::{Error, anyhow, bail};
use bstr::BString;
use starnix_core::device::DeviceOps;
use starnix_core::task::{ArgNameAndValue, CurrentTask, Kernel};
use starnix_core::vfs::{FileOps, NamespaceNode};

use starnix_uapi::device_id::DeviceId;
use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;

const VENDOR_ARM: u16 = 0x13B5;
const VENDOR_INTEL: u16 = 0x8086;

fn parse_supported_vendors(args: &[&str]) -> Result<Vec<u16>, Error> {
    let mut supported_vendors: Option<Vec<u16>> = None;
    for &arg in args {
        match ArgNameAndValue::from(arg) {
            ArgNameAndValue { name: "supported_vendors", value: Some(vendors_arg) } => {
                let vendors = vendors_arg
                    .split(',')
                    .map(|s| {
                        let trimmed =
                            s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
                        u16::from_str_radix(trimmed, 16).map_err(|_| {
                            anyhow!(
                                "supported vendors must be comma-separated hex IDs: 0x1234[,0xabcd]"
                            )
                        })
                    })
                    .collect::<Result<Vec<u16>, Error>>()?;
                supported_vendors = Some(vendors);
            }
            _ => bail!("invalid magma argument: {arg}"),
        }
    }
    Ok(supported_vendors.unwrap_or_else(|| vec![VENDOR_ARM, VENDOR_INTEL]))
}

/// Validates `magma` module configuration `args`.
pub fn validate_args(args: &[&str]) -> Result<(), Error> {
    parse_supported_vendors(args).map(|_| ())
}

/// Returns the kernel command-line parameters for the `magma` module given its configuration `args`.
pub fn cmdline_params(args: &[&str]) -> Result<BString, Error> {
    let vendors = parse_supported_vendors(args)?;
    Ok(get_magma_params(&vendors))
}

/// Initializes the Magma device from module configuration `args`.
pub fn module_init(kernel: &Kernel, args: &[&str]) -> Result<(), Error> {
    let supported_vendors = parse_supported_vendors(args)?;
    let registry = &kernel.device_registry;
    let builder = MagmaDeviceBuilder { supported_vendors };

    registry
        .add_dyn_device(kernel, "magma0".into(), registry.objects.starnix_class(), builder)
        .expect("can add magma0");
    crate::gpu_class_directory::register_gpu_devices(kernel, GpuDeviceConfig::default());
    Ok(())
}

#[derive(Clone)]
struct MagmaDeviceBuilder {
    supported_vendors: Vec<u16>,
}

impl DeviceOps for MagmaDeviceBuilder {
    fn open(
        &self,
        current_task: &CurrentTask,
        id: DeviceId,
        node: &NamespaceNode,
        flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        MagmaFile::new_file(
            current_task,
            id,
            &node.entry.node,
            flags,
            self.supported_vendors.clone(),
        )
    }
}
