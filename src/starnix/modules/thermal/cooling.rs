// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// TODO: Remove this dead_code annotation once this machinery is used.
#![allow(dead_code)]

use anyhow::{Error, format_err};
use starnix_core::device::kobject::Device;
use starnix_core::fs::sysfs::build_device_directory;
use starnix_core::task::{CurrentTask, Kernel};
use starnix_core::vfs::FsNodeOps;
use starnix_core::vfs::pseudo::simple_directory::SimpleDirectoryMutator;
use starnix_core::vfs::pseudo::simple_file::{BytesFile, BytesFileOps};
use starnix_sync::{Locked, Unlocked};
use starnix_uapi::errors::{Errno, errno};
use starnix_uapi::file_mode::mode;
use std::borrow::Cow;
use std::sync::Arc;

trait CoolingOps: Send + Sync + 'static {
    fn get_max_state(&self) -> u32;
    fn get_state(&self) -> Result<u32, Errno>;
    fn set_state(&self, state: u32) -> Result<(), Errno>;
}

impl<T: CoolingOps> CoolingOps for Arc<T> {
    fn get_max_state(&self) -> u32 {
        self.as_ref().get_max_state()
    }

    fn get_state(&self) -> Result<u32, Errno> {
        self.as_ref().get_state()
    }

    fn set_state(&self, state: u32) -> Result<(), Errno> {
        self.as_ref().set_state(state)
    }
}

struct CoolingDevice<T: CoolingOps> {
    device_id: u32,
    device_type: String,
    ops: T,
}

impl<T: CoolingOps> CoolingDevice<T> {
    fn get_device_name(&self) -> String {
        format!("cooling_device{}", self.device_id)
    }

    fn build_device_dir(self: Arc<Self>, device: &Device, dir: &SimpleDirectoryMutator) {
        build_device_directory(device, dir);
        dir.entry(
            "max_state",
            BytesFile::new_node(format!("{}\n", self.ops.get_max_state()).into_bytes()),
            mode!(IFREG, 0o444),
        );
        dir.entry(
            "type",
            BytesFile::new_node(format!("{}\n", self.device_type).into_bytes()),
            mode!(IFREG, 0o444),
        );
        dir.entry("cur_state", CurStateFile::new_node(self), mode!(IFREG, 0o644));
    }
}

/// Current state file, which proxies integral reads and writes to [`CoolingOps`].
struct CurStateFile<T: CoolingOps> {
    cooling_device: Arc<CoolingDevice<T>>,
}

impl<T: CoolingOps> CurStateFile<T> {
    fn new_node(cooling_device: Arc<CoolingDevice<T>>) -> impl FsNodeOps {
        BytesFile::new_node(Self { cooling_device })
    }
}

impl<T: CoolingOps> BytesFileOps for CurStateFile<T> {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let state = self.cooling_device.ops.get_state()?;
        Ok(format!("{}\n", state).into_bytes().into())
    }

    fn write(&self, _current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        let input = str::from_utf8(&data).map_err(|_| errno!(EINVAL))?;
        let input_num = input.trim().parse().map_err(|_| errno!(EINVAL))?;
        self.cooling_device.ops.set_state(input_num)
    }
}

/// Registrar for cooling devices, which are sequentially numbered.
struct CoolingDeviceRegistrar {
    next_id: u32,
}

impl CoolingDeviceRegistrar {
    fn new() -> Self {
        Self { next_id: 0 }
    }

    fn get_next_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Register a device in the virtual thermal class.
    fn register<T: CoolingOps>(
        &mut self,
        locked: &mut Locked<Unlocked>,
        kernel: &Kernel,
        device_type: String,
        ops: T,
    ) -> Device {
        let device_registry = &kernel.device_registry;
        let device_class = device_registry.objects.virtual_thermal_class();

        let cooling_device =
            Arc::new(CoolingDevice::<T> { device_id: self.get_next_id(), device_type, ops });

        device_registry.add_numberless_device(
            locked,
            cooling_device.get_device_name().as_str().into(),
            device_class,
            |device, dir| cooling_device.build_device_dir(device, dir),
        )
    }
}

/// Initializes the cooling devices specified in the device list.
///
/// Device strings are of the form `type[=param]`. Not all device types support a parameter.
///
/// Supported devices:
/// * `fcc=N`: Fast charge current, where N is the max state.
pub fn cooling_device_init(
    _locked: &mut Locked<Unlocked>,
    _kernel: &Kernel,
    devices: Vec<String>,
) -> Result<(), Error> {
    let mut _registrar = CoolingDeviceRegistrar::new();
    #[allow(clippy::never_loop)]
    for device_spec in devices.into_iter() {
        let (device_type, _device_param) = device_spec
            .split_once('=')
            .map_or_else(|| (device_spec.as_str(), None), |(t, p)| (t, Some(p)));
        match device_type {
            t => {
                return Err(format_err!("Unknown cooling device: {t:?}"));
            }
        };
    }

    Ok(())
}
