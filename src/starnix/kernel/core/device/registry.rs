// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::device::kobject::{
    Bus, Class, Device, DeviceMetadata, DeviceSubsystem, UEventAction, UEventContext,
};
use crate::device::kobject_store::KObjectStore;
use crate::fs::devtmpfs::{devtmpfs_create_device, devtmpfs_remove_path};
use crate::fs::sysfs::build_device_directory;
use crate::task::{CurrentTask, Kernel, register_delayed_release};
use crate::vfs::pseudo::simple_directory::SimpleDirectoryMutator;
use crate::vfs::pseudo::stub_empty_file::StubEmptyFile;
use crate::vfs::{FileOps, FsStr, FsString, NamespaceNode};
use starnix_lifecycle::{ObjectReleaser, ReleaserAction};
use starnix_logging::{bug_ref, log_error};
use starnix_sync::{
    DeviceRegistryListenersLock, DeviceRegistryStateLevel, LockDepGuard, LockDepMutex,
    MappedLockDepGuard,
};
use starnix_types::ownership::{Releasable, ReleaseGuard};
use starnix_uapi::as_any::AsAny;
use starnix_uapi::device_id::{DYN_MAJOR_RANGE, DeviceId, MISC_DYNANIC_MINOR_RANGE, MISC_MAJOR};
use starnix_uapi::error;
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::mode;
use starnix_uapi::open_flags::OpenFlags;
use std::collections::btree_map::{BTreeMap, Entry};
use std::ops::{Deref, Range};
use std::sync::Arc;

use dyn_clone::{DynClone, clone_trait_object};

const CHRDEV_MINOR_MAX: u32 = 256;
const BLKDEV_MINOR_MAX: u32 = 2u32.pow(20);

/// The mode or category of the device driver.
#[derive(Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Debug)]
pub enum DeviceMode {
    /// This device is a character device.
    Char,

    /// This device is a block device.
    Block,
}

impl DeviceMode {
    /// The number of minor device numbers available for this device mode.
    fn minor_count(&self) -> u32 {
        match self {
            Self::Char => CHRDEV_MINOR_MAX,
            Self::Block => BLKDEV_MINOR_MAX,
        }
    }

    /// The range of minor device numbers available for this device mode.
    pub fn minor_range(&self) -> Range<u32> {
        0..self.minor_count()
    }
}

pub trait DeviceOps: DynClone + Send + Sync + AsAny + 'static {
    /// Instantiate a FileOps for this device.
    ///
    /// This function is called when userspace opens a file with a `DeviceId`
    /// assigned to this device.
    fn open(
        &self,
        current_task: &CurrentTask,
        devt: DeviceId,
        node: &NamespaceNode,
        flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno>;
}

clone_trait_object!(DeviceOps);

/// Allows directly using a function or closure as an implementation of DeviceOps, avoiding having
/// to write a zero-size struct and an impl for it.
impl<F> DeviceOps for F
where
    F: Clone
        + Send
        + Sync
        + Clone
        + Fn(&CurrentTask, DeviceId, &NamespaceNode, OpenFlags) -> Result<Box<dyn FileOps>, Errno>
        + 'static,
{
    fn open(
        &self,
        current_task: &CurrentTask,
        id: DeviceId,
        node: &NamespaceNode,
        flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        self(current_task, id, node, flags)
    }
}

/// A simple `DeviceOps` function for any device that implements `FileOps + Default`.
pub fn simple_device_ops<T: Default + FileOps + 'static>(
    _current_task: &CurrentTask,
    _id: DeviceId,
    _node: &NamespaceNode,
    _flags: OpenFlags,
) -> Result<Box<dyn FileOps>, Errno> {
    Ok(Box::new(T::default()))
}

/// Keys returned by the registration method for `DeviceListener`s that allows to unregister a
/// listener.
pub type DeviceListenerKey = u64;

/// Listener for device uevents dispatched by [`DeviceRegistry`].
///
/// [`Self::on_device_event`] is called synchronously with the registry's listener lock held so
/// that all listeners observe events in monotonic `seqnum` order. Implementations may query
/// registered devices on [`DeviceRegistry`], but must not register or unregister listeners or
/// dispatch further uevents while handling an event.
pub trait DeviceListener: Send + Sync {
    fn on_device_event(&self, action: UEventAction, device: Device, context: UEventContext);
}

/// Allows directly using a function or closure as an implementation of [`DeviceListener`].
impl<F> DeviceListener for F
where
    F: Fn(UEventAction, Device, UEventContext) + Send + Sync,
{
    fn on_device_event(&self, action: UEventAction, device: Device, context: UEventContext) {
        self(action, device, context);
    }
}

pub struct DeviceOpsWrapper(Box<dyn DeviceOps>);
impl ReleaserAction<DeviceOpsWrapper> for DeviceOpsWrapper {
    fn release(device_ops: ReleaseGuard<DeviceOpsWrapper>) {
        register_delayed_release(device_ops);
    }
}
impl Deref for DeviceOpsWrapper {
    type Target = dyn DeviceOps;
    fn deref(&self) -> &Self::Target {
        self.0.deref()
    }
}
pub type DeviceReleaser = ObjectReleaser<DeviceOpsWrapper, DeviceOpsWrapper>;
pub type DeviceHandle = Arc<DeviceReleaser>;
impl Releasable for DeviceOpsWrapper {
    type Context<'a> = &'a CurrentTask;

    fn release<'a>(self, _context: &'a CurrentTask) {}
}

/// An entry in the `DeviceRegistry`.
struct DeviceEntry {
    /// The name of the device.
    ///
    /// This name is the same as the name of the KObject for the device.
    name: FsString,

    /// The ops used to open the device.
    ops: DeviceHandle,
}

impl DeviceEntry {
    fn new(name: FsString, ops: impl DeviceOps) -> Self {
        Self { name, ops: Arc::new(DeviceOpsWrapper(Box::new(ops)).into()) }
    }
}

/// The devices registered for a given `DeviceMode`.
///
/// Each `DeviceMode` has its own namespace of registered devices.
#[derive(Default)]
struct RegisteredDevices {
    /// The major devices registered for this device mode.
    ///
    /// Typically the devices registered here will add and remove individual devices using the
    /// `add_device` and `remove_device` functions on `DeviceRegistry`.
    ///
    /// A major device registration shadows any minor device registrations for the same major
    /// device number. We might need to reconsider this choice in the future in order to make
    /// the /proc/devices file correctly list major devices such as `misc`.
    majors: BTreeMap<u32, DeviceEntry>,

    /// Individually registered minor devices.
    ///
    /// These devices are registered using the `register_device` function on `DeviceRegistry`.
    minors: BTreeMap<DeviceId, DeviceEntry>,
}

impl RegisteredDevices {
    /// Register a major device.
    ///
    /// Returns `EINVAL` if the major device is already registered.
    fn register_major(&mut self, major: u32, entry: DeviceEntry) -> Result<(), Errno> {
        if let Entry::Vacant(slot) = self.majors.entry(major) {
            slot.insert(entry);
            Ok(())
        } else {
            error!(EINVAL)
        }
    }

    /// Register a minor device.
    ///
    /// Overwrites any existing minor device registered with the given `DeviceId`.
    fn register_minor(&mut self, devt: DeviceId, entry: DeviceEntry) {
        self.minors.insert(devt, entry);
    }

    /// Unregister a minor device.
    fn unregister_minor(&mut self, devt: DeviceId) {
        self.minors.remove(&devt);
    }

    /// Get the ops for a given `DeviceId`.
    ///
    /// If there is a major device registered with the major device number of the
    /// `DeviceId`, the ops for that major device will be returned. Otherwise,
    /// if there is a minor device registered, the ops for that minor device will be
    /// returned. Otherwise, returns `ENODEV`.
    fn get(&self, devt: DeviceId) -> Result<DeviceHandle, Errno> {
        if let Some(major_device) = self.majors.get(&devt.major()) {
            Ok(Arc::clone(&major_device.ops))
        } else if let Some(minor_device) = self.minors.get(&devt) {
            Ok(Arc::clone(&minor_device.ops))
        } else {
            error!(ENODEV)
        }
    }

    /// Returns a list of the registered major device numbers and their names.
    fn list_major_devices(&self) -> Vec<(u32, FsString)> {
        self.majors.iter().map(|(major, entry)| (*major, entry.name.clone())).collect()
    }

    /// Returns a list of the registered minor devices and their names.
    fn list_minor_devices(&self, range: Range<DeviceId>) -> Vec<(DeviceId, FsString)> {
        self.minors.range(range).map(|(devt, entry)| (devt.clone(), entry.name.clone())).collect()
    }
}

/// The registry for devices.
///
/// Devices are specified in file systems with major and minor device numbers, together referred to
/// as a `DeviceId`. When userspace opens one of those files, we look up the `DeviceId` in the
/// device registry to instantiate a file for that device.
///
/// The `DeviceRegistry` also manages the `KObjectStore`, which provides metadata for devices via
/// the sysfs file system, typically mounted at /sys.
pub struct DeviceRegistry {
    /// The KObjects for registered devices.
    pub objects: KObjectStore,

    /// Mutable state for the device registry's major/minor device tables and allocators.
    state: LockDepMutex<DeviceRegistryState, DeviceRegistryStateLevel>,

    /// Listeners notified when device uevents are dispatched by the registry.
    listeners: LockDepMutex<DeviceRegistryListeners, DeviceRegistryListenersLock>,
}
struct DeviceRegistryState {
    /// The registered character devices.
    char_devices: RegisteredDevices,

    /// The registered block devices.
    block_devices: RegisteredDevices,

    /// Some of the misc devices (devices with the `MISC_MAJOR` major number) are dynamically
    /// allocated. This allocator keeps track of which device numbers have been allocated to
    /// such devices.
    misc_chardev_allocator: DeviceIdAllocator,

    /// A range of large major device numbers are reserved for other dynamically allocated
    /// devices. This allocator keeps track of which device numbers have been allocated to
    /// such devices.
    dyn_chardev_allocator: DeviceIdAllocator,

    /// The next anonymous device number to assign to a file system.
    next_anon_minor: u32,
}

#[derive(Default)]
struct DeviceRegistryListeners {
    /// Listeners notified when device uevents are dispatched by the registry.
    listeners: BTreeMap<u64, Box<dyn DeviceListener>>,

    /// The next identifier to use for a listener.
    next_listener_id: u64,

    /// The next event identifier to use when notifying listeners.
    next_event_id: u64,
}

impl DeviceRegistry {
    /// Register a device with the `DeviceRegistry`.
    ///
    /// If you are registering a device that exists in other systems, please check the metadata
    /// for that device in /sys and make sure you use the same properties when calling this
    /// function because these value are visible to userspace.
    ///
    /// For example, a typical device will appear in sysfs at a path like:
    ///
    ///   `/sys/devices/{bus}/{class}/{name}`
    ///
    /// Many common classes have convenient accessors on `DeviceRegistry::objects`.
    ///
    /// To fill out the `DeviceMetadata`, look at the `uevent` file:
    ///
    ///   `/sys/devices/{bus}/{class}/{name}/uevent`
    ///
    /// which as the following format:
    ///
    /// ```
    ///   MAJOR={major-number}
    ///   MINOR={minor-number}
    ///   DEVNAME={devname}
    ///   DEVMODE={devmode}
    /// ```
    ///
    /// Often, the `{name}` and the `{devname}` are the same, but if they are not the same,
    /// please take care to use the correct string in the correct field.
    ///
    /// If the `{major-number}` is 10 and the `{minor-number}` is in the range 52..128, please use
    /// `register_misc_device` instead because these device numbers are dynamically allocated.
    ///
    /// If the `{major-number}` is in the range 234..255, please use `register_dyn_device` instead
    /// because these device are also dynamically allocated.
    ///
    /// If you are unsure which device numbers to use, consult devices.txt:
    ///
    ///   https://www.kernel.org/doc/Documentation/admin-guide/devices.txt
    ///
    /// If you are still unsure, please ask an experienced Starnix contributor rather than make up
    /// a device number.
    ///
    /// Uses [`build_device_directory`] to populate the standard `sysfs` directory for the device.
    /// To populate custom `sysfs` attributes or specify a parent device, use
    /// [`Self::register_device_with_dir`].
    ///
    /// Finally, the `dev_ops` parameter is where you provide the callback for instantiating
    /// your device.
    pub fn register_device(
        &self,
        kernel: &Kernel,
        name: &FsStr,
        metadata: DeviceMetadata,
        class: Class,
        dev_ops: impl DeviceOps,
    ) -> Device {
        self.register_device_with_dir(
            kernel,
            name,
            metadata,
            /* parent = */ None,
            class,
            build_device_directory,
            dev_ops,
        )
    }

    /// Registers a minor device with a custom `sysfs` directory populated by `build_directory`,
    /// beneath `parent`.
    ///
    /// If `parent` is itself a class device then the device directory is created directly within
    /// the parent's directory, otherwise it is created within a `<class>` directory under
    /// `parent`. If `parent` is `None` then `/sys/devices/virtual` is used.
    ///
    /// Dispatches [`UEventAction::Add`] after creating the `sysfs` and `devtmpfs` nodes. The
    /// returned [`Device`] can be unregistered with [`Self::remove_device`].
    ///
    /// See [`Self::register_device`] for an explanation of the other parameters.
    pub fn register_device_with_dir(
        &self,
        kernel: &Kernel,
        name: &FsStr,
        metadata: DeviceMetadata,
        parent: Option<Device>,
        subsystem: impl Into<DeviceSubsystem>,
        build_directory: impl FnOnce(&Device, &SimpleDirectoryMutator),
        dev_ops: impl DeviceOps,
    ) -> Device {
        let entry = DeviceEntry::new(name.into(), dev_ops);
        self.devices(metadata.mode).register_minor(metadata.devt, entry);
        self.add_device(kernel, name, metadata, parent, subsystem, build_directory)
    }

    /// Register a dynamic device in the `MISC_MAJOR` major device number.
    ///
    /// MISC devices (major number 10) with minor numbers in the range 52..128 are dynamically
    /// assigned. Rather than hardcoding registrations with these device numbers, use this
    /// function instead to register the device.
    ///
    /// See `register_device` for an explanation of the parameters.
    pub fn register_misc_device(
        &self,
        kernel: &Kernel,
        name: &FsStr,
        dev_ops: impl DeviceOps,
    ) -> Result<Device, Errno> {
        let devt = self.state.lock().misc_chardev_allocator.allocate()?;
        let metadata = DeviceMetadata::new(name.into(), devt, DeviceMode::Char);
        Ok(self.register_device(kernel, name, metadata, self.objects.misc_class(), dev_ops))
    }

    /// Register a dynamic device with major numbers 234..255.
    ///
    /// Majors device numbers 234..255 are dynamically assigned. Rather than hardcoding
    /// registrations with these device numbers, use this function instead to register the device.
    ///
    /// Note: We do not currently allocate from this entire range because we have mistakenly
    /// hardcoded some device registrations from the dynamic range. Once we fix these registrations
    /// to be dynamic, we should expand to using the full dynamic range.
    ///
    /// See `register_device` for an explanation of the parameters.
    pub fn register_dyn_device(
        &self,
        kernel: &Kernel,
        name: &FsStr,
        class: Class,
        dev_ops: impl DeviceOps,
    ) -> Result<Device, Errno> {
        self.register_dyn_device_with_dir(kernel, name, class, build_device_directory, dev_ops)
    }

    /// Register a dynamic device with a custom directory.
    ///
    /// See `register_device` for an explanation of the parameters.
    pub fn register_dyn_device_with_dir(
        &self,
        kernel: &Kernel,
        name: &FsStr,
        class: Class,
        build_directory: impl FnOnce(&Device, &SimpleDirectoryMutator),
        dev_ops: impl DeviceOps,
    ) -> Result<Device, Errno> {
        self.register_dyn_device_with_devname(kernel, name, name, class, build_directory, dev_ops)
    }

    /// Register a dynamic device with major numbers 234..255.
    ///
    /// Majors device numbers 234..255 are dynamically assigned. Rather than hardcoding
    /// registrations with these device numbers, use this function instead to register the device.
    ///
    /// Note: We do not currently allocate from this entire range because we have mistakenly
    /// hardcoded some device registrations from the dynamic range. Once we fix these registrations
    /// to be dynamic, we should expand to using the full dynamic range.
    ///
    /// See `register_device` for an explanation of the parameters.
    pub fn register_dyn_device_with_devname(
        &self,
        kernel: &Kernel,
        name: &FsStr,
        devname: &FsStr,
        class: Class,
        build_directory: impl FnOnce(&Device, &SimpleDirectoryMutator),
        dev_ops: impl DeviceOps,
    ) -> Result<Device, Errno> {
        let devt = self.state.lock().dyn_chardev_allocator.allocate()?;
        let metadata = DeviceMetadata::new(devname.into(), devt, DeviceMode::Char);
        Ok(self.register_device_with_dir(
            kernel,
            name,
            metadata,
            /* parent = */ None,
            class,
            build_directory,
            dev_ops,
        ))
    }

    /// Register a "silent" dynamic device with major numbers 234..255.
    ///
    /// Only use for devices that should not be registered with the KObjectStore/appear in sysfs.
    /// This is a rare occurrence.
    ///
    /// See `register_dyn_device` for an explanation of dyn devices and of the parameters.
    pub fn register_silent_dyn_device(
        &self,
        name: &FsStr,
        dev_ops: impl DeviceOps,
    ) -> Result<DeviceMetadata, Errno> {
        let devt = self.state.lock().dyn_chardev_allocator.allocate()?;
        let metadata = DeviceMetadata::new(name.into(), devt, DeviceMode::Char);
        let entry = DeviceEntry::new(name.into(), dev_ops);
        self.devices(metadata.mode).register_minor(metadata.devt, entry);
        Ok(metadata)
    }

    /// Directly adds a numbered device to the [`KObjectStore`] and dispatches
    /// [`UEventAction::Add`].
    ///
    /// This function should be used only by devices that have registered an entire major device
    /// number via [`Self::register_major`]. If you want to register a single minor device, use
    /// [`Self::register_device`] or [`Self::register_device_with_dir`] instead.
    ///
    /// The returned [`Device`] can be unregistered with [`Self::remove_device`].
    ///
    /// See [`Self::register_device_with_dir`] for an explanation of the parameters.
    pub fn add_device(
        &self,
        kernel: &Kernel,
        name: &FsStr,
        metadata: DeviceMetadata,
        parent: Option<Device>,
        subsystem: impl Into<DeviceSubsystem>,
        build_directory: impl FnOnce(&Device, &SimpleDirectoryMutator),
    ) -> Device {
        self.devices(metadata.mode).get(metadata.devt).expect("device is registered");
        let device = self.objects.create_device(
            name,
            parent,
            Some(subsystem.into()),
            Some(metadata.clone()),
            build_directory,
        );

        if kernel.features.devtmpfs {
            if let Err(err) = devtmpfs_create_device(kernel, metadata) {
                log_error!("Cannot create device {:?} in devtmpfs ({:?})", device, err);
            }
        }
        self.dispatch_uevent(UEventAction::Add, device.clone());
        device
    }

    /// Adds a network device to the device registry and dispatches [`UEventAction::Add`].
    ///
    /// Network devices do not have a `/dev` node or device number. Instead, in addition to
    /// `DEVPATH` and `SUBSYSTEM=net`, their `uevent` files have the following properties:
    ///
    /// ```
    /// INTERFACE={name}
    /// IFINDEX={index}
    /// ```
    ///
    /// TODO(https://fxbug.dev/297438880): Populate `INTERFACE` and `IFINDEX` in the `uevent` file
    /// and uevent netlink messages. Currently, we only register net devices by name with the
    /// standard `DEVPATH` and `SUBSYSTEM=net` properties, and stub the `address` attribute so that
    /// reads by userspace are tracked.
    ///
    /// The returned [`Device`] can be removed with [`Self::remove_device`].
    pub fn add_net_device(&self, name: &FsStr) -> Device {
        self.add_numberless_device(
            name,
            /* parent = */ None,
            self.objects.net_class(),
            |device, dir| {
                build_device_directory(device, dir);
                dir.entry(
                    "address",
                    StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/452096300")),
                    mode!(IFREG, 0o444),
                );
            },
        )
    }

    fn add_device_without_metadata(
        &self,
        name: &FsStr,
        parent: Option<Device>,
        subsystem: Option<DeviceSubsystem>,
        build_directory: impl FnOnce(&Device, &SimpleDirectoryMutator),
    ) -> Device {
        let device = self.objects.create_device(
            name,
            parent,
            subsystem,
            /* metadata = */ None,
            build_directory,
        );
        self.dispatch_uevent(UEventAction::Add, device.clone());
        device
    }

    /// Directly adds a device without a bus or class subsystem to the [`KObjectStore`], populated
    /// by `build_directory`, and dispatches [`UEventAction::Add`].
    ///
    /// Such devices typically act as structural parents for other devices, e.g. the root of a
    /// subsystem's devices (`/sys/devices/system/cpu`). If `parent` is `Some`, the device
    /// directory is created under `parent`; otherwise it is created directly under
    /// `/sys/devices/<name>`.
    ///
    /// The returned [`Device`] can be removed with [`Self::remove_device`].
    pub fn add_subsystemless_device(
        &self,
        name: &FsStr,
        parent: Option<Device>,
        build_directory: impl FnOnce(&Device, &SimpleDirectoryMutator),
    ) -> Device {
        self.add_device_without_metadata(name, parent, /* subsystem = */ None, build_directory)
    }

    /// Directly adds a bus device to the [`KObjectStore`], populated by `build_directory`, and
    /// dispatches [`UEventAction::Add`].
    ///
    /// If `parent` is `Some`, the device directory is created under `parent`; otherwise it is
    /// created directly under `/sys/devices/<name>`. A symlink is also created under
    /// `/sys/bus/<bus.name>/devices/<name>`.
    ///
    /// The returned [`Device`] can be removed with [`Self::remove_device`].
    pub fn add_bus_device(
        &self,
        name: &FsStr,
        parent: Option<Device>,
        bus: Bus,
        build_directory: impl FnOnce(&Device, &SimpleDirectoryMutator),
    ) -> Device {
        self.add_device_without_metadata(name, parent, Some(bus.into()), build_directory)
    }

    /// Directly adds a class device without a device number to the [`KObjectStore`], populated by
    /// `build_directory`, and dispatches [`UEventAction::Add`].
    ///
    /// This function should be used by class devices that are exposed via `sysfs` or netlink and
    /// do not have a `/dev` device node (and therefore have no major or minor device number).
    ///
    /// If `parent` is `Some` and is itself a class device, the device directory is created under
    /// `<parent>/<name>`; otherwise it is created under `<parent>/<class.name>/<name>` (defaulting
    /// `parent` to `/sys/devices/virtual`). A symlink is also created under
    /// `/sys/class/<class.name>/<name>`.
    ///
    /// The returned [`Device`] can be removed with [`Self::remove_device`].
    pub fn add_numberless_device(
        &self,
        name: &FsStr,
        parent: Option<Device>,
        class: Class,
        build_directory: impl FnOnce(&Device, &SimpleDirectoryMutator),
    ) -> Device {
        self.add_device_without_metadata(name, parent, Some(class.into()), build_directory)
    }

    /// Unregisters `device` from `devtmpfs`, `sysfs`, and the minor device table, recycling any
    /// dynamically allocated [`DeviceId`] before dispatching [`UEventAction::Remove`].
    ///
    /// Any child devices parented under `device` must be removed before `device` itself so
    /// removal uevents are dispatched in child-before-parent order and intermediate class
    /// directories are cleaned up.
    pub fn remove_device(&self, current_task: &CurrentTask, device: Device) {
        if let Some(metadata) = device.metadata() {
            if current_task.kernel().features.devtmpfs {
                if let Err(err) = devtmpfs_remove_path(current_task, metadata.devname.as_ref()) {
                    log_error!("Cannot remove device {:?} ({:?})", device, err);
                }
            }
        }

        self.objects.remove(&device);

        if let Some(metadata) = device.metadata() {
            let mut state = self.state.lock();
            match metadata.mode {
                DeviceMode::Char => {
                    state.char_devices.unregister_minor(metadata.devt);
                    state.misc_chardev_allocator.free(metadata.devt);
                    state.dyn_chardev_allocator.free(metadata.devt);
                }
                DeviceMode::Block => {
                    state.block_devices.unregister_minor(metadata.devt);
                }
            }
        }

        self.dispatch_uevent(UEventAction::Remove, device);
    }

    /// Returns a list of the registered major device numbers for the given `DeviceMode` and their
    /// names.
    pub fn list_major_devices(&self, mode: DeviceMode) -> Vec<(u32, FsString)> {
        self.devices(mode).list_major_devices()
    }

    /// Returns a list of the registered minor devices for the given `DeviceMode` and their names.
    pub fn list_minor_devices(
        &self,
        mode: DeviceMode,
        range: Range<DeviceId>,
    ) -> Vec<(DeviceId, FsString)> {
        self.devices(mode).list_minor_devices(range)
    }

    /// The `RegisteredDevice` object for the given `DeviceMode`.
    fn devices<'a>(&'a self, mode: DeviceMode) -> MappedLockDepGuard<'a, RegisteredDevices> {
        LockDepGuard::map(self.state.lock(), |state| match mode {
            DeviceMode::Char => &mut state.char_devices,
            DeviceMode::Block => &mut state.block_devices,
        })
    }

    /// Register an entire major device number.
    ///
    /// If you register an entire major device, use `add_device` and `remove_device` to manage the
    /// sysfs entiries for your device rather than trying to register and unregister individual
    /// minor devices.
    pub fn register_major<'a>(
        &self,
        name: FsString,
        mode: DeviceMode,
        major: u32,
        dev_ops: impl DeviceOps,
    ) -> Result<(), Errno> {
        let entry = DeviceEntry::new(name, dev_ops);
        self.devices(mode).register_major(major, entry)
    }

    /// Allocate an anonymous device identifier.
    pub fn next_anonymous_dev_id<'a>(&self) -> DeviceId {
        let mut state = self.state.lock();
        let id = DeviceId::new(0, state.next_anon_minor);
        state.next_anon_minor += 1;
        id
    }

    /// Registers a listener for device uevents and returns its [`DeviceListenerKey`].
    pub fn register_listener<'a>(
        &self,
        listener: impl DeviceListener + 'static,
    ) -> DeviceListenerKey {
        let mut listeners = self.listeners.lock();
        let key = listeners.next_listener_id;
        listeners.next_listener_id += 1;
        listeners.listeners.insert(key, Box::new(listener));
        key
    }

    /// Unregisters a listener previously registered via [`Self::register_listener`].
    pub fn unregister_listener<'a>(&self, key: &DeviceListenerKey) {
        self.listeners.lock().listeners.remove(key);
    }

    /// Dispatches a uevent with the next monotonic sequence number for `device` to all registered
    /// listeners.
    pub fn dispatch_uevent<'a>(&self, action: UEventAction, device: Device) {
        let mut listeners = self.listeners.lock();
        let event_id = listeners.next_event_id;
        listeners.next_event_id += 1;
        let context = UEventContext { seqnum: event_id };
        for listener in listeners.listeners.values() {
            listener.on_device_event(action, device.clone(), context);
        }
    }

    /// Instantiate a file for the specified device.
    ///
    /// The device will be looked up in the device registry by `DeviceMode` and `DeviceId`.
    pub fn open_device(
        &self,
        current_task: &CurrentTask,
        node: &NamespaceNode,
        flags: OpenFlags,
        devt: DeviceId,
        mode: DeviceMode,
    ) -> Result<Box<dyn FileOps>, Errno> {
        let dev_ops = self.devices(mode).get(devt)?;
        dev_ops.open(current_task, devt, node, flags)
    }

    pub fn get_device(&self, devt: DeviceId, mode: DeviceMode) -> Result<DeviceHandle, Errno> {
        self.devices(mode).get(devt)
    }
}

impl Default for DeviceRegistry {
    fn default() -> Self {
        let misc_available = vec![DeviceId::new_range(MISC_MAJOR, MISC_DYNANIC_MINOR_RANGE)];
        let dyn_available = DYN_MAJOR_RANGE
            .map(|major| DeviceId::new_range(major, DeviceMode::Char.minor_range()))
            .rev()
            .collect();
        let state = DeviceRegistryState {
            char_devices: Default::default(),
            block_devices: Default::default(),
            misc_chardev_allocator: DeviceIdAllocator::new(misc_available),
            dyn_chardev_allocator: DeviceIdAllocator::new(dyn_available),
            next_anon_minor: 1,
        };
        Self { objects: Default::default(), state: state.into(), listeners: Default::default() }
    }
}

/// An allocator for `DeviceId`
struct DeviceIdAllocator {
    /// Total ranges of device identifiers managed by this allocator.
    ranges: Vec<Range<DeviceId>>,

    /// The available ranges of device types to allocate.
    ///
    /// Devices will be allocated from the back of the vector first.
    freelist: Vec<Range<DeviceId>>,
}

impl DeviceIdAllocator {
    /// Create an allocator for the given ranges of device types.
    ///
    /// The devices will be allocated from the front of the vector first.
    fn new(mut available: Vec<Range<DeviceId>>) -> Self {
        let ranges = available.clone();
        available.reverse();
        Self { ranges, freelist: available }
    }

    /// Allocate a `DeviceId`.
    fn allocate(&mut self) -> Result<DeviceId, Errno> {
        let Some(range) = self.freelist.pop() else {
            return error!(ENOMEM);
        };
        let allocated = range.start;
        if let Some(next) = allocated.next_minor() {
            if next < range.end {
                self.freelist.push(next..range.end);
            }
        }
        Ok(allocated)
    }

    /// Returns a previously allocated number to the pool so it can be reused.
    ///
    /// Does nothing if `id` is outside the ranges managed by this allocator.
    fn free(&mut self, id: DeviceId) {
        if !self.ranges.iter().any(|range| range.contains(&id)) {
            return;
        }
        let Some(next) = id.next_minor() else {
            return;
        };
        if self.freelist.iter().any(|range| range.start <= id && id < range.end) {
            return;
        };
        // Coalesce with any immediately adjacent free ranges to prevent freelist fragmentation
        // across repeated device removals.
        let mut new_range = id..next;
        if let Some(idx) = self.freelist.iter().position(|range| range.end == new_range.start) {
            let prev = self.freelist.remove(idx);
            new_range.start = prev.start;
        }
        if let Some(idx) = self.freelist.iter().position(|range| range.start == new_range.end) {
            self.freelist[idx].start = new_range.start;
        } else {
            self.freelist.push(new_range);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::mem::DevNull;
    use crate::task::{DelayedReleaser, KernelFeatures};
    use crate::testing::*;
    use crate::vfs::*;
    use starnix_uapi::device_id::{INPUT_MAJOR, MEM_MAJOR};

    #[::fuchsia::test]
    async fn registry_fails_to_add_duplicate_device() {
        spawn_kernel_and_run(async |_current_task| {
            let registry = DeviceRegistry::default();
            registry
                .register_major(
                    "mem".into(),
                    DeviceMode::Char,
                    MEM_MAJOR,
                    simple_device_ops::<DevNull>,
                )
                .expect("registers once");
            registry
                .register_major(
                    "random".into(),
                    DeviceMode::Char,
                    123,
                    simple_device_ops::<DevNull>,
                )
                .expect("registers unique");
            registry
                .register_major(
                    "mem".into(),
                    DeviceMode::Char,
                    MEM_MAJOR,
                    simple_device_ops::<DevNull>,
                )
                .expect_err("fail to register duplicate");
        })
        .await;
    }

    #[::fuchsia::test]
    async fn registry_opens_device() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let registry = DeviceRegistry::default();
            registry
                .register_major(
                    "mem".into(),
                    DeviceMode::Char,
                    MEM_MAJOR,
                    simple_device_ops::<DevNull>,
                )
                .expect("registers unique");

            let fs = create_testfs(&kernel);
            let node = create_namespace_node_for_testing(&fs, PanickingFsNode);

            // Fail to open non-existent device.
            assert!(
                registry
                    .open_device(
                        &current_task,
                        &node,
                        OpenFlags::RDONLY,
                        DeviceId::NONE,
                        DeviceMode::Char
                    )
                    .is_err()
            );

            // Fail to open in wrong mode.
            assert!(
                registry
                    .open_device(
                        &current_task,
                        &node,
                        OpenFlags::RDONLY,
                        DeviceId::NULL,
                        DeviceMode::Block
                    )
                    .is_err()
            );

            // Open in correct mode.
            let _ = registry
                .open_device(
                    &current_task,
                    &node,
                    OpenFlags::RDONLY,
                    DeviceId::NULL,
                    DeviceMode::Char,
                )
                .expect("opens device");
        })
        .await;
    }

    #[::fuchsia::test]
    async fn registry_dynamic_misc() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            fn create_test_device(
                _current_task: &CurrentTask,
                _id: DeviceId,
                _node: &NamespaceNode,
                _flags: OpenFlags,
            ) -> Result<Box<dyn FileOps>, Errno> {
                Ok(Box::new(PanickingFile))
            }

            let registry = &kernel.device_registry;
            let device = registry
                .register_dyn_device(
                    kernel,
                    "test-device".into(),
                    registry.objects.block_class(),
                    create_test_device,
                )
                .unwrap();
            let devt = device.metadata().expect("has metadata").devt;
            assert!(DYN_MAJOR_RANGE.contains(&devt.major()));

            let fs = create_testfs(&kernel);
            let node = create_namespace_node_for_testing(&fs, PanickingFsNode);
            let _ = registry
                .open_device(&current_task, &node, OpenFlags::RDONLY, devt, DeviceMode::Char)
                .expect("opens device");
        })
        .await;
    }

    #[::fuchsia::test]
    async fn registery_add_class() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let registry = &kernel.device_registry;
            registry
                .register_major(
                    "input".into(),
                    DeviceMode::Char,
                    INPUT_MAJOR,
                    simple_device_ops::<DevNull>,
                )
                .expect("can register input");

            let input_class = registry.objects.get_or_create_class("input".into());
            registry.add_device(
                kernel,
                "mouse".into(),
                DeviceMetadata::new(
                    "mouse".into(),
                    DeviceId::new(INPUT_MAJOR, 0),
                    DeviceMode::Char,
                ),
                /* parent = */ None,
                input_class,
                build_device_directory,
            );

            assert!(registry.objects.root.lookup("class/input/mouse".into()).is_some());
            assert!(registry.objects.root.lookup("devices/virtual/input/mouse".into()).is_some());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn registry_add_bus() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let registry = &kernel.device_registry;

            let bus = registry.objects.get_or_create_bus("my-bus".into());
            let bus_dev = registry.add_bus_device(
                "my-device".into(),
                /* parent = */ None,
                bus.clone(),
                build_device_directory,
            );
            assert!(registry.objects.root.lookup("bus/my-bus/devices/my-device".into()).is_some());
            assert!(registry.objects.root.lookup("devices/my-device".into()).is_some());

            let class = registry.objects.get_or_create_class("my-class".into());
            let class_dev = registry.add_numberless_device(
                "my-class-dev".into(),
                Some(bus_dev.clone()),
                class,
                build_device_directory,
            );
            assert!(registry.objects.root.lookup("class/my-class/my-class-dev".into()).is_some());
            assert!(
                registry
                    .objects
                    .root
                    .lookup("devices/my-device/my-class/my-class-dev".into())
                    .is_some()
            );

            let sub_class = registry.objects.get_or_create_class("my-subclass".into());
            let child_class_dev = registry.add_numberless_device(
                "my-child-class-dev".into(),
                Some(class_dev.clone()),
                sub_class,
                build_device_directory,
            );
            assert!(
                registry
                    .objects
                    .root
                    .lookup("class/my-subclass/my-child-class-dev".into())
                    .is_some()
            );
            assert!(
                registry
                    .objects
                    .root
                    .lookup("devices/my-device/my-class/my-class-dev/my-child-class-dev".into())
                    .is_some()
            );

            registry.remove_device(&current_task, child_class_dev);
            assert!(
                registry
                    .objects
                    .root
                    .lookup("class/my-subclass/my-child-class-dev".into())
                    .is_none()
            );
            assert!(
                registry
                    .objects
                    .root
                    .lookup("devices/my-device/my-class/my-class-dev/my-child-class-dev".into())
                    .is_none()
            );

            registry.remove_device(&current_task, class_dev);
            assert!(registry.objects.root.lookup("class/my-class/my-class-dev".into()).is_none());
            assert!(
                registry
                    .objects
                    .root
                    .lookup("devices/my-device/my-class/my-class-dev".into())
                    .is_none()
            );
            assert!(registry.objects.root.lookup("devices/my-device/my-class".into()).is_none());

            let bus_dev_check = bus_dev.clone();
            registry.remove_device(&current_task, bus_dev);
            assert!(registry.objects.root.lookup("bus/my-bus/devices/my-device".into()).is_none());
            assert!(registry.objects.root.lookup("devices/my-device".into()).is_none());
            DelayedReleaser::default().apply(&current_task);
            assert!(bus_dev_check.dir().is_none());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn registry_add_root_and_child_bus_device() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let registry = &kernel.device_registry;

            let root_dev = registry.add_subsystemless_device(
                "my-root".into(),
                /* parent = */ None,
                build_device_directory,
            );
            assert!(registry.objects.root.lookup("devices/my-root/uevent".into()).is_some());

            let bus = registry.objects.get_or_create_bus("my-bus".into());
            let child_bus_dev = registry.add_bus_device(
                "my-child".into(),
                Some(root_dev.clone()),
                bus.clone(),
                build_device_directory,
            );
            assert!(
                registry.objects.root.lookup("devices/my-root/my-child/uevent".into()).is_some()
            );
            assert!(registry.objects.root.lookup("bus/my-bus/devices/my-child".into()).is_some());

            let grandchild_bus_dev = registry.add_bus_device(
                "my-grandchild".into(),
                Some(child_bus_dev.clone()),
                bus,
                build_device_directory,
            );
            assert!(
                registry
                    .objects
                    .root
                    .lookup("devices/my-root/my-child/my-grandchild/uevent".into())
                    .is_some()
            );
            assert!(
                registry.objects.root.lookup("bus/my-bus/devices/my-grandchild".into()).is_some()
            );

            registry.remove_device(&current_task, grandchild_bus_dev);
            assert!(
                registry
                    .objects
                    .root
                    .lookup("devices/my-root/my-child/my-grandchild".into())
                    .is_none()
            );
            assert!(
                registry.objects.root.lookup("bus/my-bus/devices/my-grandchild".into()).is_none()
            );

            registry.remove_device(&current_task, child_bus_dev);
            assert!(registry.objects.root.lookup("devices/my-root/my-child".into()).is_none());
            assert!(registry.objects.root.lookup("bus/my-bus/devices/my-child".into()).is_none());

            registry.remove_device(&current_task, root_dev);
            assert!(registry.objects.root.lookup("devices/my-root".into()).is_none());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn registry_remove_device() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let registry = &kernel.device_registry;
            registry
                .register_major(
                    "input".into(),
                    DeviceMode::Char,
                    INPUT_MAJOR,
                    simple_device_ops::<DevNull>,
                )
                .expect("can register input");

            let input_class = registry.objects.get_or_create_class("input".into());
            let mouse_dev = registry.add_device(
                kernel,
                "mouse".into(),
                DeviceMetadata::new(
                    "mouse".into(),
                    DeviceId::new(INPUT_MAJOR, 0),
                    DeviceMode::Char,
                ),
                /* parent = */ None,
                input_class.clone(),
                build_device_directory,
            );
            let keyboard_dev = registry.add_device(
                kernel,
                "keyboard".into(),
                DeviceMetadata::new(
                    "keyboard".into(),
                    DeviceId::new(INPUT_MAJOR, 1),
                    DeviceMode::Char,
                ),
                /* parent = */ None,
                input_class,
                build_device_directory,
            );

            assert!(registry.objects.root.lookup("devices/virtual/input/mouse".into()).is_some());
            assert!(
                registry.objects.root.lookup("devices/virtual/input/keyboard".into()).is_some()
            );
            assert!(registry.objects.root.lookup("class/input/mouse".into()).is_some());
            assert!(registry.objects.root.lookup("class/input/keyboard".into()).is_some());

            registry.remove_device(&current_task, mouse_dev);

            assert!(registry.objects.root.lookup("devices/virtual/input/mouse".into()).is_none());
            assert!(registry.objects.root.lookup("devices/virtual/input".into()).is_some());
            assert!(registry.objects.root.lookup("class/input/mouse".into()).is_none());

            registry.remove_device(&current_task, keyboard_dev.clone());

            assert!(
                registry.objects.root.lookup("devices/virtual/input/keyboard".into()).is_none()
            );
            assert!(registry.objects.root.lookup("devices/virtual/input".into()).is_none());
            assert!(registry.objects.root.lookup("class/input/keyboard".into()).is_none());

            // Removing an already-removed device must not recreate missing parent directories.
            registry.objects.remove(&keyboard_dev);
            assert!(registry.objects.root.lookup("devices/virtual/input".into()).is_none());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn registry_add_and_remove_numberless_device() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let registry = &kernel.device_registry;

            let cooling_device = registry.add_numberless_device(
                "cooling_device0".into(),
                /* parent = */ None,
                registry.objects.thermal_class(),
                build_device_directory,
            );

            assert!(registry.objects.root.lookup("class/thermal/cooling_device0".into()).is_some());
            assert!(
                registry
                    .objects
                    .root
                    .lookup("devices/virtual/thermal/cooling_device0".into())
                    .is_some()
            );

            registry.remove_device(&current_task, cooling_device);

            assert!(registry.objects.root.lookup("class/thermal/cooling_device0".into()).is_none());
            assert!(
                registry
                    .objects
                    .root
                    .lookup("devices/virtual/thermal/cooling_device0".into())
                    .is_none()
            );
            assert!(registry.objects.root.lookup("devices/virtual/thermal".into()).is_none());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn registry_add_and_remove_net_device() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let registry = &kernel.device_registry;

            let wlan0 = registry.add_net_device("wlan0".into());
            assert!(registry.objects.root.lookup("class/net/wlan0".into()).is_some());
            assert!(
                registry.objects.root.lookup("devices/virtual/net/wlan0/address".into()).is_some()
            );
            assert!(
                registry.objects.root.lookup("devices/virtual/net/wlan0/uevent".into()).is_some()
            );

            registry.remove_device(&current_task, wlan0);
            assert!(registry.objects.root.lookup("class/net/wlan0".into()).is_none());
            assert!(registry.objects.root.lookup("devices/virtual/net/wlan0".into()).is_none());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn registry_remove_minor_device() {
        spawn_kernel_with_features_and_run(
            async |current_task| {
                let kernel = current_task.kernel();
                let registry = &kernel.device_registry;

                let dev = registry
                    .register_misc_device(kernel, "test_misc".into(), simple_device_ops::<DevNull>)
                    .expect("register misc device");
                let devt = dev.metadata().expect("metadata").devt;

                assert!(registry.get_device(devt, DeviceMode::Char).is_ok());

                registry.remove_device(&current_task, dev);

                assert_eq!(registry.get_device(devt, DeviceMode::Char).map(|_| ()), error!(ENODEV));

                // Re-registering after removal reuses the freed dynamic misc DeviceId and devtmpfs path.
                let dev2 = registry
                    .register_misc_device(kernel, "test_misc".into(), simple_device_ops::<DevNull>)
                    .expect("re-register misc device");
                assert_eq!(dev2.metadata().expect("metadata").devt, devt);
                registry.remove_device(&current_task, dev2);
            },
            KernelFeatures { devtmpfs: true, ..Default::default() },
        )
        .await;
    }

    #[::fuchsia::test]
    async fn registry_remove_dyn_device_recycles_device_id() {
        spawn_kernel_with_features_and_run(
            async |current_task| {
                let kernel = current_task.kernel();
                let registry = &kernel.device_registry;
                let class = registry.objects.get_or_create_class("test_dyn_class".into());

                let dev1 = registry
                    .register_dyn_device(
                        kernel,
                        "test_dyn_0".into(),
                        class.clone(),
                        simple_device_ops::<DevNull>,
                    )
                    .expect("register first dyn device");
                let dev2 = registry
                    .register_dyn_device(
                        kernel,
                        "test_dyn_1".into(),
                        class.clone(),
                        simple_device_ops::<DevNull>,
                    )
                    .expect("register second dyn device");
                let devt1 = dev1.metadata().expect("metadata").devt;
                let devt2 = dev2.metadata().expect("metadata").devt;
                assert_ne!(devt1, devt2);

                // Free out of order, then in order, verifying range coalescing and reallocation.
                registry.remove_device(&current_task, dev1);
                registry.remove_device(&current_task, dev2);

                let dev1_rebound = registry
                    .register_dyn_device(
                        kernel,
                        "test_dyn_0".into(),
                        class,
                        simple_device_ops::<DevNull>,
                    )
                    .expect("re-register dyn device");
                assert_eq!(dev1_rebound.metadata().expect("metadata").devt, devt1);
                registry.remove_device(&current_task, dev1_rebound);
            },
            KernelFeatures { devtmpfs: true, ..Default::default() },
        )
        .await;
    }

    #[::fuchsia::test]
    async fn registry_listener_can_query_registry_and_devtmpfs_collision_does_not_fail_add() {
        spawn_kernel_with_features_and_run(
            async |current_task| {
                let kernel = current_task.kernel();
                let registry = &kernel.device_registry;
                let observed = Arc::new(starnix_sync::Mutex::new(Vec::new()));
                let listener_observed = observed.clone();
                let kernel_weak = Arc::downgrade(kernel);
                let key = registry.register_listener(
                    move |action: UEventAction, device: Device, _context: UEventContext| {
                        let Some(kernel) = kernel_weak.upgrade() else {
                            return;
                        };
                        if let Some(metadata) = device.metadata() {
                            let is_registered = kernel
                                .device_registry
                                .get_device(metadata.devt, metadata.mode)
                                .is_ok();
                            listener_observed.lock().push((action, is_registered));
                        }
                    },
                );

                // First device creates `/dev/colliding_dev` in `devtmpfs`.
                let dev1 = registry
                    .register_misc_device(
                        kernel,
                        "colliding_dev".into(),
                        simple_device_ops::<DevNull>,
                    )
                    .expect("register first misc device");
                // Second device with a colliding `devtmpfs` name still succeeds and dispatches Add.
                let dev2 = registry
                    .register_misc_device(
                        kernel,
                        "colliding_dev".into(),
                        simple_device_ops::<DevNull>,
                    )
                    .expect("register second misc device despite devtmpfs collision");

                registry.remove_device(&current_task, dev2);
                registry.remove_device(&current_task, dev1);

                assert_eq!(
                    std::mem::take(&mut *observed.lock()),
                    vec![
                        (UEventAction::Add, true),
                        (UEventAction::Add, true),
                        (UEventAction::Remove, false),
                        (UEventAction::Remove, false),
                    ]
                );

                registry.unregister_listener(&key);
            },
            KernelFeatures { devtmpfs: true, ..Default::default() },
        )
        .await;
    }

    #[::fuchsia::test]
    async fn registry_dispatches_uevents_for_bus_and_numberless_devices() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let registry = &kernel.device_registry;
            let events = Arc::new(starnix_sync::Mutex::new(Vec::new()));
            let listener_events = events.clone();
            let root = registry.objects.root.clone();
            let key = registry.register_listener(
                move |action: UEventAction, device: Device, _context: UEventContext| {
                    let path = device.path_from_depth(0);
                    match action {
                        UEventAction::Add => {
                            assert!(
                                root.lookup(path.as_ref()).is_some(),
                                "sysfs entry for {path} must exist when Add uevent is dispatched"
                            );
                        }
                        UEventAction::Remove => {
                            assert!(
                                root.lookup(path.as_ref()).is_none(),
                                "sysfs entry for {path} must be removed before Remove uevent is dispatched"
                            );
                        }
                        _ => {}
                    }
                    listener_events.lock().push((
                        action,
                        path,
                        device.subsystem().map(|s| s.name().to_owned()),
                    ));
                },
            );

            let root_dev = registry.add_subsystemless_device(
                "my-root".into(),
                /* parent = */ None,
                build_device_directory,
            );
            let bus = registry.objects.get_or_create_bus("my-bus".into());
            let bus_dev = registry.add_bus_device(
                "my-bus-dev".into(),
                Some(root_dev.clone()),
                bus,
                build_device_directory,
            );
            let class = registry.objects.get_or_create_class("my-class".into());
            let class_dev = registry.add_numberless_device(
                "my-class-dev".into(),
                /* parent = */ None,
                class,
                build_device_directory,
            );
            let net_dev = registry.add_net_device("wlan0".into());

            assert_eq!(
                std::mem::take(&mut *events.lock()),
                vec![
                    (UEventAction::Add, "devices/my-root".into(), None),
                    (
                        UEventAction::Add,
                        "devices/my-root/my-bus-dev".into(),
                        Some("my-bus".into()),
                    ),
                    (
                        UEventAction::Add,
                        "devices/virtual/my-class/my-class-dev".into(),
                        Some("my-class".into()),
                    ),
                    (UEventAction::Add, "devices/virtual/net/wlan0".into(), Some("net".into())),
                ]
            );

            registry.remove_device(&current_task, net_dev);
            registry.remove_device(&current_task, class_dev);
            registry.remove_device(&current_task, bus_dev);
            registry.remove_device(&current_task, root_dev);

            assert_eq!(
                std::mem::take(&mut *events.lock()),
                vec![
                    (UEventAction::Remove, "devices/virtual/net/wlan0".into(), Some("net".into())),
                    (
                        UEventAction::Remove,
                        "devices/virtual/my-class/my-class-dev".into(),
                        Some("my-class".into()),
                    ),
                    (
                        UEventAction::Remove,
                        "devices/my-root/my-bus-dev".into(),
                        Some("my-bus".into()),
                    ),
                    (UEventAction::Remove, "devices/my-root".into(), None),
                ]
            );

            registry.unregister_listener(&key);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn device_edit_dir() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let registry = &kernel.device_registry;

            let bus = registry.objects.get_or_create_bus("my-bus".into());
            let bus_dev = registry.add_bus_device(
                "my-device".into(),
                /* parent = */ None,
                bus,
                build_device_directory,
            );
            assert!(registry.objects.root.lookup("devices/my-device/uevent".into()).is_some());
            assert!(registry.objects.root.lookup("devices/my-device/extra_attr".into()).is_none());

            bus_dev.edit_dir(kernel, |dir| {
                dir.entry(
                    "extra_attr",
                    StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/560216403")),
                    mode!(IFREG, 0o444),
                );
            });
            assert!(registry.objects.root.lookup("devices/my-device/extra_attr".into()).is_some());

            bus_dev.edit_dir(kernel, |dir| {
                dir.remove("extra_attr".into());
            });
            assert!(registry.objects.root.lookup("devices/my-device/extra_attr".into()).is_none());

            let bus_dev_check = bus_dev.clone();
            registry.remove_device(&current_task, bus_dev);
            DelayedReleaser::default().apply(&current_task);

            // Editing a removed device's directory is a no-op.
            bus_dev_check.edit_dir(kernel, |dir| {
                dir.entry(
                    "extra_attr",
                    StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/560216403")),
                    mode!(IFREG, 0o444),
                );
            });
            assert!(registry.objects.root.lookup("devices/my-device".into()).is_none());
        })
        .await;
    }
}
