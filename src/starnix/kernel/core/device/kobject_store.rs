// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::device::DeviceMode;
use crate::device::kobject::{Bus, Class, Device, DeviceMetadata, Subsystem};
use crate::fs::sysfs::{build_device_directory, get_sysfs, register_cpu_devices};
use crate::task::Kernel;
use crate::vfs::pseudo::simple_directory::{SimpleDirectory, SimpleDirectoryMutator};
use crate::vfs::pseudo::stub_empty_file::StubEmptyFile;
use crate::vfs::{FileSystemHandle, FsStr, FsString};
use starnix_logging::bug_ref;
use starnix_uapi::file_mode::mode;
use std::sync::{Arc, OnceLock};

/// Owner of all the KObjects in sysfs.
///
/// Holds strong references to the KObjects that are visible in sysfs. These
/// objects are organized into hierarchies that make it easier to implement sysfs.
pub struct KObjectStore {
    /// Root of the sysfs hierarchy.
    pub root: Arc<SimpleDirectory>,

    /// Sysfs filesystem in which the KObjects are stored.
    fs: OnceLock<FileSystemHandle>,

    /// Root `/sys/devices/virtual` device.
    virtual_device: OnceLock<Device>,

    /// Root `/sys/devices/platform` device.
    platform_device: OnceLock<Device>,

    /// Root `/sys/devices/system` device.
    system_device: OnceLock<Device>,

    /// Root `/sys/devices/platform/soc` bus device.
    soc_device: OnceLock<Device>,
}

impl KObjectStore {
    pub fn init(&self, kernel: &Kernel) {
        self.fs.set(get_sysfs(kernel)).unwrap();
        self.register_initial_devices(kernel);
    }

    fn register_initial_devices(&self, kernel: &Kernel) {
        let registry = &kernel.device_registry;

        // Generic Linux device and class registrations.
        register_cpu_devices(kernel);
        let _ = self.power_supply_class();

        // TODO(https://fxbug.dev/452096300): Register backing_dev_info devices from actual
        // superblocks instead of a hardcoded 0:80 stub.
        registry.add_numberless_device(
            "0:80".into(),
            /* parent = */ None,
            self.get_or_create_class("bdi".into()),
            |device, dir| {
                build_device_directory(device, dir);
                add_stub_attributes(dir, &["max_ratio", "read_ahead_kb"]);
            },
        );

        // TODO(https://fxbug.dev/452096300): Replace hardcoded board/SoC stub devices with dynamic
        // device configuration.
        // Eagerly create `/sys/devices/platform/soc` and `/sys/bus/platform/devices/soc`.
        let _ = self.soc_device();

        // TODO(https://fxbug.dev/452096300): Stub SoC identification device (`/sys/devices/soc0`),
        // attached to the `soc` bus.
        registry.add_bus_device(
            "soc0".into(),
            /* parent = */ None,
            self.get_or_create_bus("soc".into()),
            |device, dir| {
                build_device_directory(device, dir);
                add_stub_attributes(dir, &["revision", "serial_number"]);
            },
        );
    }

    pub(super) fn fs(&self) -> &FileSystemHandle {
        self.fs.get().expect("sysfs should be initialized")
    }

    /// Device bus used for platform devices (`/sys/bus/platform`).
    pub fn platform_bus(&self) -> Bus {
        self.get_or_create_bus("platform".into())
    }

    /// Root device used for virtual devices (`/sys/devices/virtual`).
    pub fn virtual_device(&self) -> Device {
        self.virtual_device
            .get_or_init(|| {
                self.create_device(
                    "virtual".into(),
                    /* parent = */ None,
                    /* subsystem = */ None,
                    /* metadata = */ None,
                    |_, _| {},
                )
            })
            .clone()
    }

    /// Root device used for platform devices (`/sys/devices/platform`).
    pub fn platform_device(&self) -> Device {
        self.platform_device
            .get_or_init(|| {
                self.create_device(
                    "platform".into(),
                    /* parent = */ None,
                    /* subsystem = */ None,
                    /* metadata = */ None,
                    build_device_directory,
                )
            })
            .clone()
    }

    /// Root device used for system devices (`/sys/devices/system`).
    ///
    /// Subsystems such as `cpu` place their root device beneath this device.
    pub fn system_device(&self) -> Device {
        self.system_device
            .get_or_init(|| {
                self.create_device(
                    "system".into(),
                    /* parent = */ None,
                    /* subsystem = */ None,
                    /* metadata = */ None,
                    |_, _| {},
                )
            })
            .clone()
    }

    /// Bus device used for SoC peripheral devices (`/sys/devices/platform/soc`).
    pub fn soc_device(&self) -> Device {
        // TODO(https://fxbug.dev/452096300): Replace hardcoded SoC bus device with dynamic board
        // configuration.
        self.soc_device
            .get_or_init(|| {
                self.create_device(
                    "soc".into(),
                    Some(self.platform_device()),
                    Some(self.platform_bus().into()),
                    /* metadata = */ None,
                    build_device_directory,
                )
            })
            .clone()
    }

    /// Device class used for block devices.
    pub fn block_class(&self) -> Class {
        self.get_or_create_class("block".into())
    }

    /// Device class used for thermal devices.
    pub fn thermal_class(&self) -> Class {
        self.get_or_create_class("thermal".into())
    }

    /// Device class used for powercap devices.
    pub fn powercap_class(&self) -> Class {
        self.get_or_create_class("powercap".into())
    }

    /// Device class used for power-supply devices (`/sys/class/power_supply`).
    pub fn power_supply_class(&self) -> Class {
        self.get_or_create_class("power_supply".into())
    }

    /// Device class used for graphics devices.
    pub fn graphics_class(&self) -> Class {
        self.get_or_create_class("graphics".into())
    }

    /// Device class used for input devices.
    pub fn input_class(&self) -> Class {
        self.get_or_create_class("input".into())
    }

    /// Device class used for mem devices.
    pub fn mem_class(&self) -> Class {
        self.get_or_create_class("mem".into())
    }

    /// Device class used for net devices.
    pub fn net_class(&self) -> Class {
        self.get_or_create_class("net".into())
    }

    /// Device class used for misc devices.
    pub fn misc_class(&self) -> Class {
        self.get_or_create_class("misc".into())
    }

    /// Device class used for tty devices.
    pub fn tty_class(&self) -> Class {
        self.get_or_create_class("tty".into())
    }

    /// Device class used for dma_heap devices.
    pub fn dma_heap_class(&self) -> Class {
        self.get_or_create_class("dma_heap".into())
    }

    /// Incorrect device class.
    ///
    /// This class exposes the name "starnix" to userspace, which is incorrect. Instead, devices
    /// should use a class that represents their usage rather than their implementation.
    ///
    /// This class exists because a number of devices incorrectly use this class. We should fix
    /// those devices to report their proper class.
    pub fn starnix_class(&self) -> Class {
        self.get_or_create_class("starnix".into())
    }

    /// Real-time clock class.
    ///
    /// Becomes `/sys/class/rtc/...`.
    pub fn rtc_class(&self) -> Class {
        self.get_or_create_class("rtc".into())
    }

    fn ensure_dir(&self, path: &[&FsStr]) -> Arc<SimpleDirectory> {
        let fs = self.fs();
        let mut dir = self.root.clone();
        for component in path {
            dir = dir.subdir(fs, component, 0o755);
        }
        dir
    }

    fn lookup_dir(&self, path: &[&FsStr]) -> Option<Arc<SimpleDirectory>> {
        let mut dir = self.root.clone();
        for component in path {
            dir = dir.get_dir(component)?;
        }
        Some(dir)
    }

    /// Get a bus by name.
    ///
    /// If the bus does not exist, this function will create it.
    pub fn get_or_create_bus(&self, name: &FsStr) -> Bus {
        let name = name.to_owned();
        let devices = self.ensure_dir(&[b"bus".into(), name.as_ref(), b"devices".into()]);
        Bus::new(name, devices)
    }

    /// Get a class by name.
    ///
    /// If the class does not exist, this function will create it.
    pub fn get_or_create_class(&self, name: &FsStr) -> Class {
        let name = name.to_owned();
        let devices = self.ensure_dir(&[b"class".into(), name.as_ref()]);
        Class::new(name, devices)
    }

    pub fn class_with_dir(
        &self,
        name: &FsStr,
        build_directory: impl FnOnce(&SimpleDirectoryMutator),
    ) -> Class {
        let class = self.get_or_create_class(name);
        class.devices().edit(self.fs(), build_directory);
        class
    }

    /// Create a device and add that device to the store.
    ///
    /// Rather than use this function directly, you should register your device with the
    /// [`DeviceRegistry`](crate::device::DeviceRegistry). The
    /// [`DeviceRegistry`](crate::device::DeviceRegistry) will create the KObject for the device as
    /// part of the registration process.
    ///
    /// If you create the device yourself, userspace will not be able to instantiate the
    /// device because the [`DeviceId`](starnix_uapi::device_id::DeviceId) will not be registered
    /// with the [`DeviceRegistry`](crate::device::DeviceRegistry).
    pub(super) fn create_device(
        &self,
        name: &FsStr,
        parent: Option<Device>,
        subsystem: Option<Subsystem>,
        metadata: Option<DeviceMetadata>,
        build_directory: impl FnOnce(&Device, &SimpleDirectoryMutator),
    ) -> Device {
        let parent = match (parent, &subsystem) {
            (None, Some(Subsystem::Class(_))) => Some(self.virtual_device()),
            (parent, _) => parent,
        };
        let dir = if let Some(glue_dir) =
            Device::glue_dir_name_for(parent.as_ref(), subsystem.as_ref())
        {
            let parent_dir = parent
                .as_ref()
                .and_then(Device::dir)
                .expect("parent device directory exists in sysfs");
            parent_dir.nested_subdir(self.fs(), glue_dir, 0o755, name, 0o755)
        } else if let Some(parent) = parent.as_ref() {
            let parent_dir = parent.dir().expect("parent device directory exists in sysfs");
            parent_dir.subdir(self.fs(), name, 0o755)
        } else {
            self.ensure_dir(&[b"devices".into()]).subdir(self.fs(), name, 0o755)
        };
        let device = Device::new(name.to_owned(), parent, subsystem, metadata, dir.clone());
        dir.edit(self.fs(), |mutator| {
            build_directory(&device, mutator);
        });
        self.add(&device);
        device
    }

    fn add(&self, device: &Device) {
        let name = device.name();

        // Insert the newly created device into various views.
        match device.subsystem() {
            Some(Subsystem::Bus(bus)) => {
                bus.devices().edit(self.fs(), |dir| {
                    dir.symlink(name, device.path_from_depth(3).as_ref());
                });
            }
            Some(Subsystem::Class(class)) => {
                class.devices().edit(self.fs(), |dir| {
                    dir.symlink(name, device.path_from_depth(2).as_ref());
                });
            }
            None => {}
        }

        if let Some(metadata) = device.metadata() {
            let device_number = FsString::from(metadata.devt.to_string());
            let dev_subdir: &FsStr = match metadata.mode {
                DeviceMode::Block => {
                    self.ensure_dir(&[b"block".into()]).edit(self.fs(), |dir| {
                        dir.symlink(name, device.path_from_depth(1).as_ref());
                    });
                    b"block".into()
                }
                DeviceMode::Char => b"char".into(),
            };
            self.ensure_dir(&[b"dev".into(), dev_subdir]).edit(self.fs(), |dir| {
                dir.symlink(device_number.as_ref(), device.path_from_depth(2).as_ref());
            });
        }
    }

    /// Destroy a device.
    ///
    /// This function removes the KObject for the device from the store.
    ///
    /// Most clients hold weak references to KObjects, which means those references will become
    /// invalid shortly after this function is called.
    pub(super) fn remove(&self, device: &Device) {
        let name = device.name();
        // Remove the device from its views in the reverse order in which it was added.
        if let Some(metadata) = device.metadata() {
            let device_number: FsString = metadata.devt.to_string().into();
            let dev_subdir: &FsStr = match metadata.mode {
                DeviceMode::Block => {
                    if let Some(block_dir) = self.lookup_dir(&[b"block".into()]) {
                        block_dir.remove(name);
                    }
                    b"block".into()
                }
                DeviceMode::Char => b"char".into(),
            };
            if let Some(dev_dir) = self.lookup_dir(&[b"dev".into(), dev_subdir]) {
                dev_dir.remove(device_number.as_ref());
            }
        }
        match device.subsystem() {
            Some(Subsystem::Bus(bus)) => {
                bus.devices().remove(name);
            }
            Some(Subsystem::Class(class)) => {
                class.devices().remove(name);
            }
            None => {}
        }
        // Finally, remove the device from the object store.
        if let Some(glue_dir) = device.glue_dir_name() {
            if let Some(parent_dir) = device.parent().and_then(Device::dir) {
                parent_dir.remove_from_subdir_if_empty(glue_dir, name);
            }
        } else if let Some(parent) = device.parent() {
            if let Some(parent_dir) = parent.dir() {
                parent_dir.remove(name);
            }
        } else if let Some(devices_dir) = self.lookup_dir(&[b"devices".into()]) {
            devices_dir.remove(name);
        }
    }
}

impl Default for KObjectStore {
    fn default() -> Self {
        Self {
            root: SimpleDirectory::new(),
            fs: OnceLock::new(),
            virtual_device: OnceLock::new(),
            platform_device: OnceLock::new(),
            system_device: OnceLock::new(),
            soc_device: OnceLock::new(),
        }
    }
}

/// Adds read-only [`StubEmptyFile`] attributes named `names` to `dir`.
///
/// Reads of these attributes are tracked to identify which unimplemented sysfs attributes are used
/// by userspace.
fn add_stub_attributes(dir: &SimpleDirectoryMutator, names: &[&str]) {
    for name in names {
        dir.entry(
            name,
            StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/452096300")),
            mode!(IFREG, 0o444),
        );
    }
}
