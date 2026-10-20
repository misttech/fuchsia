// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_hardware_usb as fhardware_usb;
use starnix_core::task::CurrentTask;
use starnix_core::vfs::buffers::{InputBuffer, OutputBuffer};
use starnix_core::vfs::{FileObject, FileOps, fileops_impl_noop_sync, fileops_impl_seekable};
use starnix_syscalls::{SyscallArg, SyscallResult};
use starnix_uapi::errors::Errno;
use starnix_uapi::{
    error, usb_device_descriptor, usb_device_speed_USB_SPEED_FULL, usb_device_speed_USB_SPEED_HIGH,
    usb_device_speed_USB_SPEED_LOW, usb_device_speed_USB_SPEED_SUPER,
    usb_device_speed_USB_SPEED_SUPER_PLUS, usb_device_speed_USB_SPEED_UNKNOWN,
    usb_device_speed_USB_SPEED_WIRELESS,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

pub(crate) const USB_SPEED_UNKNOWN: u32 = usb_device_speed_USB_SPEED_UNKNOWN;
pub(crate) const USB_SPEED_LOW: u32 = usb_device_speed_USB_SPEED_LOW;
pub(crate) const USB_SPEED_FULL: u32 = usb_device_speed_USB_SPEED_FULL;
pub(crate) const USB_SPEED_HIGH: u32 = usb_device_speed_USB_SPEED_HIGH;
pub(crate) const USB_SPEED_WIRELESS: u32 = usb_device_speed_USB_SPEED_WIRELESS;
pub(crate) const USB_SPEED_SUPER: u32 = usb_device_speed_USB_SPEED_SUPER;
pub(crate) const USB_SPEED_SUPER_PLUS: u32 = usb_device_speed_USB_SPEED_SUPER_PLUS;

pub(crate) struct UsbDevice {
    pub(crate) bus_num: u16,
    pub(crate) dev_num: u16,
    pub(crate) speed: u32,
    pub(crate) device_descriptor: usb_device_descriptor,
    #[expect(dead_code)]
    pub(crate) config_descriptors: Vec<u8>,
    pub(crate) raw_descriptors: Vec<u8>,
    pub(crate) active_config: AtomicU8,
    pub(crate) sync_proxy: Option<Arc<fhardware_usb::UsbSynchronousProxy>>,
    pub(crate) disconnected: AtomicBool,
}

impl UsbDevice {
    pub(crate) fn new(
        bus_num: u16,
        dev_num: u16,
        speed: u32,
        device_descriptor: usb_device_descriptor,
        config_descriptors: Vec<u8>,
    ) -> Self {
        let mut raw_descriptors = Vec::new();
        let desc_bytes = zerocopy::IntoBytes::as_bytes(&device_descriptor);
        raw_descriptors.extend_from_slice(desc_bytes);
        raw_descriptors.extend_from_slice(&config_descriptors);

        Self {
            bus_num,
            dev_num,
            speed,
            device_descriptor,
            config_descriptors,
            raw_descriptors,
            active_config: AtomicU8::new(1),
            sync_proxy: None,
            disconnected: AtomicBool::new(false),
        }
    }

    /// Computes the Linux USB device minor number per `usb_dev_minor`:
    /// `(bus - 1) * 128 + (dev_num - 1)`.
    pub(crate) fn minor(&self) -> u32 {
        debug_assert!(self.bus_num >= 1 && self.dev_num >= 1);
        (u32::from(self.bus_num).saturating_sub(1) * 128)
            + u32::from(self.dev_num).saturating_sub(1)
    }

    pub(crate) fn speed_string(&self) -> &'static str {
        match self.speed {
            USB_SPEED_LOW => "1.5\n",
            USB_SPEED_FULL => "12\n",
            USB_SPEED_HIGH => "480\n",
            USB_SPEED_WIRELESS => "480\n",
            USB_SPEED_SUPER => "5000\n",
            USB_SPEED_SUPER_PLUS => "10000\n",
            _ => "unknown\n",
        }
    }

    pub(crate) fn on_disconnect(&self) {
        self.disconnected.store(true, Ordering::Release);
    }
}

pub(crate) struct UsbDeviceFile {
    pub(crate) device: Arc<UsbDevice>,
}

impl UsbDeviceFile {
    pub(crate) fn new(device: Arc<UsbDevice>) -> Self {
        Self { device }
    }
}

impl FileOps for UsbDeviceFile {
    fileops_impl_seekable!();
    fileops_impl_noop_sync!();

    fn read(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        if self.device.disconnected.load(Ordering::Acquire) {
            return error!(ENODEV);
        }
        let descriptors = &self.device.raw_descriptors;
        if offset >= descriptors.len() {
            return Ok(0);
        }
        data.write(&descriptors[offset..])
    }

    fn write(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        _data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        error!(EINVAL)
    }

    fn ioctl(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _request: u32,
        _arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        if self.device.disconnected.load(Ordering::Acquire) {
            return error!(ENODEV);
        }
        error!(ENOTTY)
    }
}
