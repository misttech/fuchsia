// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{MagmaFile, get_magma_params};
use anyhow::{Error, anyhow};
use argh::FromArgs;
use bstr::BString;
use starnix_core::device::DeviceOps;
use starnix_core::task::{CurrentTask, Kernel};
use starnix_core::vfs::{FileOps, NamespaceNode};

use starnix_uapi::device_id::DeviceId;
use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;

const VENDOR_ARM: u16 = 0x13B5;
const VENDOR_INTEL: u16 = 0x8086;

/// Configuration options for the `magma` module.
#[derive(FromArgs, Debug, Default)]
struct MagmaArgs {
    /// comma-separated list of 16-bit hex vendor IDs (e.g. "0x13B5,0x8086")
    #[argh(option)]
    supported_vendors: Option<String>,
}

fn parse_supported_vendors(args: &[&str]) -> Result<Vec<u16>, Error> {
    let parsed = MagmaArgs::from_args(&["magma"], args).map_err(|e| anyhow!("{}", e.output))?;
    match parsed.supported_vendors {
        Some(vendors_arg) => vendors_arg
            .split(',')
            .map(|s| {
                let trimmed = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
                u16::from_str_radix(trimmed, 16).map_err(|_| {
                    anyhow!("supported vendors must be comma-separated hex IDs: 0x1234[,0xabcd]")
                })
            })
            .collect(),
        None => Ok(vec![VENDOR_ARM, VENDOR_INTEL]),
    }
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
        .register_dyn_device(kernel, "magma0".into(), registry.objects.starnix_class(), builder)
        .expect("can register magma0");
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
