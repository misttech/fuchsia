// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::device::UsbDevice;
use starnix_core::device::kobject::Device;
use starnix_core::fs::sysfs::build_device_directory;
use starnix_core::task::CurrentTask;
use starnix_core::vfs::pseudo::simple_directory::SimpleDirectoryMutator;
use starnix_core::vfs::pseudo::simple_file::{BytesFile, BytesFileOps};
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::mode;
use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::Ordering;

struct ConfigurationValueFile {
    usb_device: Arc<UsbDevice>,
}

impl BytesFileOps for ConfigurationValueFile {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let val = self.usb_device.active_config.load(Ordering::Relaxed);
        Ok(format!("{}\n", val).into_bytes().into())
    }
}

pub(crate) fn build_usb_device_directory(
    device: &Device,
    dir: &SimpleDirectoryMutator,
    usb_device: &Arc<UsbDevice>,
) {
    build_device_directory(device, dir);

    let desc = &usb_device.device_descriptor;
    let id_vendor = u16::from_le(desc.idVendor);
    let id_product = u16::from_le(desc.idProduct);
    let bcd_device = u16::from_le(desc.bcdDevice);
    let b_device_class = desc.bDeviceClass;
    let b_device_sub_class = desc.bDeviceSubClass;
    let b_device_protocol = desc.bDeviceProtocol;
    let b_num_configurations = desc.bNumConfigurations;

    dir.entry(
        "idVendor",
        BytesFile::new_node(format!("{:04x}\n", id_vendor).into_bytes()),
        mode!(IFREG, 0o444),
    );
    dir.entry(
        "idProduct",
        BytesFile::new_node(format!("{:04x}\n", id_product).into_bytes()),
        mode!(IFREG, 0o444),
    );
    dir.entry(
        "bcdDevice",
        BytesFile::new_node(format!("{:04x}\n", bcd_device).into_bytes()),
        mode!(IFREG, 0o444),
    );
    dir.entry(
        "bDeviceClass",
        BytesFile::new_node(format!("{:02x}\n", b_device_class).into_bytes()),
        mode!(IFREG, 0o444),
    );
    dir.entry(
        "bDeviceSubClass",
        BytesFile::new_node(format!("{:02x}\n", b_device_sub_class).into_bytes()),
        mode!(IFREG, 0o444),
    );
    dir.entry(
        "bDeviceProtocol",
        BytesFile::new_node(format!("{:02x}\n", b_device_protocol).into_bytes()),
        mode!(IFREG, 0o444),
    );
    dir.entry(
        "bNumConfigurations",
        BytesFile::new_node(format!("{}\n", b_num_configurations).into_bytes()),
        mode!(IFREG, 0o444),
    );
    dir.entry(
        "bConfigurationValue",
        BytesFile::new_node(ConfigurationValueFile { usb_device: Arc::clone(usb_device) }),
        mode!(IFREG, 0o444),
    );
    dir.entry(
        "busnum",
        BytesFile::new_node(format!("{}\n", usb_device.bus_num).into_bytes()),
        mode!(IFREG, 0o444),
    );
    dir.entry(
        "devnum",
        BytesFile::new_node(format!("{}\n", usb_device.dev_num).into_bytes()),
        mode!(IFREG, 0o444),
    );
    dir.entry(
        "speed",
        BytesFile::new_node(usb_device.speed_string().as_bytes().to_vec()),
        mode!(IFREG, 0o444),
    );
    dir.entry(
        "descriptors",
        BytesFile::new_node(usb_device.raw_descriptors.clone()),
        mode!(IFREG, 0o444),
    );
}
