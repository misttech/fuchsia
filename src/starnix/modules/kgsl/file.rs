// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fdio::service_connect;
use kgsl_libmagma::{Device, initialize_logging};
use kgsl_strings::{ioctl_kgsl, kgsl_prop};
use magma::MAGMA_QUERY_VENDOR_ID;
use starnix_core::mm::MemoryAccessorExt;
use starnix_core::task::CurrentTask;
use starnix_core::vfs::{FileObject, FileOps, FsNode};
use starnix_core::{fileops_impl_dataless, fileops_impl_nonseekable, fileops_impl_noop_sync};
use starnix_logging::{log_error, log_info, log_warn};
use starnix_sync::{Locked, Unlocked};
use starnix_syscalls::{SUCCESS, SyscallArg, SyscallResult};
use starnix_uapi::device_type::DeviceType;
use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::user_address::{UserAddress, UserRef};
use starnix_uapi::{
    IOCTL_KGSL_DEVICE_GETPROPERTY, KGSL_PROP_DEVICE_INFO, errno, error, kgsl_device_getproperty,
    kgsl_devinfo,
};
use std::sync::Once;

pub struct KgslFile {
    _device: Device,
}

impl KgslFile {
    pub fn init() {
        match Self::init_magma_logging() {
            Ok(()) => log_info!("kgsl: magma logging enabled"),
            Err(()) => log_warn!("kgsl: magma logging failed to initialize"),
        };
    }

    fn init_magma_logging() -> Result<(), ()> {
        let (client, server) = zx::Channel::create();
        service_connect("/svc/fuchsia.logger.LogSink", server).map_err(|_| ())?;
        return initialize_logging(client);
    }

    fn import_device(path: &str) -> Result<Device, zx::Status> {
        let (client, server) = zx::Channel::create();
        service_connect(&path, server)?;
        let device = Device::from_channel(client).map_err(|_| zx::Status::INTERNAL)?;
        let vendor_id =
            device.query_value(MAGMA_QUERY_VENDOR_ID).map_err(|_| zx::Status::INTERNAL)?;
        log_info!("kgsl: magma device at {} is vendor {:#04x}", path, vendor_id);
        Ok(device)
    }

    pub fn new_file(
        _current_task: &CurrentTask,
        _dev: DeviceType,
        _node: &FsNode,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            Self::init();
        });
        let mut devices = std::fs::read_dir("/svc/fuchsia.gpu.magma.Service")
            .map_err(|_| errno!(ENXIO))?
            .filter_map(|x| x.ok())
            .filter_map(|entry| entry.path().join("device").into_os_string().into_string().ok())
            .filter_map(|path| Self::import_device(&path).ok());
        let device = devices.next().ok_or_else(|| errno!(ENXIO))?;
        Ok(Box::new(Self { _device: device }))
    }

    fn kgsl_device_getproperty(
        &self,
        current_task: &CurrentTask,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        let params = current_task.read_object(UserRef::<kgsl_device_getproperty>::from(arg))?;
        let result = UserRef::from(UserAddress::from(params.value));

        match params.type_ {
            KGSL_PROP_DEVICE_INFO => {
                const PLACEHOLDER_DEVICE_ID: u32 = 42;
                let devinfo =
                    kgsl_devinfo { device_id: PLACEHOLDER_DEVICE_ID, ..Default::default() };
                current_task.write_object(result, &devinfo)?;
                Ok(SUCCESS)
            }
            _ => {
                log_error!("kgsl: unimplemented GetProperty type {}", kgsl_prop(params.type_));
                error!(ENOTSUP)
            }
        }
    }
}

impl Drop for KgslFile {
    fn drop(&mut self) {}
}

impl FileOps for KgslFile {
    fileops_impl_dataless!();
    fileops_impl_nonseekable!();
    fileops_impl_noop_sync!();

    fn ioctl(
        &self,
        _locked: &mut Locked<Unlocked>,
        _file: &FileObject,
        current_task: &CurrentTask,
        request: u32,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        // Special ioctl to signal container to use kgsl.
        // TODO(b/429239527): remove after transitioned
        const IOCTL_KGSL_ENABLE: u32 = 42;
        if request == IOCTL_KGSL_ENABLE {
            if cfg!(not(feature = "starnix-kgsl-enable")) {
                log_info!("kgsl: suppressing further use of kgsl");
                return error!(ENXIO);
            }
            return Ok(SUCCESS);
        }
        match request {
            IOCTL_KGSL_DEVICE_GETPROPERTY => self.kgsl_device_getproperty(current_task, arg),
            _ => {
                log_error!("kgsl: unimplemented ioctl {}", ioctl_kgsl(request));
                error!(ENOTSUP)
            }
        }
    }
}
