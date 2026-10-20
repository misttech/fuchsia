// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::device::DeviceMode;
use crate::task::CurrentTask;
use crate::vfs::buffers::{InputBuffer, OutputBuffer};
use crate::vfs::pseudo::simple_directory::SimpleDirectory;
use crate::vfs::{
    FileObject, FileOps, FsNode, FsNodeOps, FsStr, FsString, PathBuilder, fileops_impl_noop_sync,
    fileops_impl_seekable, fs_node_impl_not_dir,
};
use derivative::Derivative;
use starnix_logging::track_stub;
use starnix_rcu::{RcuHashMap, RcuReadScope};
use starnix_uapi::device_id::DeviceId;
use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::{errno, error};
use std::sync::{Arc, Weak};

/// Higher-level view of a device.
///
/// Groups devices based on what they do, rather than how they are connected.
#[derive(Clone, Derivative)]
#[derivative(Debug)]
pub struct Class {
    name: FsString,
    #[derivative(Debug = "ignore")]
    devices: Arc<SimpleDirectory>,
}

impl Class {
    pub(super) fn new(name: FsString, devices: Arc<SimpleDirectory>) -> Self {
        Self { name, devices }
    }

    pub fn name(&self) -> &FsStr {
        self.name.as_ref()
    }

    pub(super) fn devices(&self) -> &Arc<SimpleDirectory> {
        &self.devices
    }
}

/// Identifies how devices are connected to the processor.
#[derive(Clone, Derivative)]
#[derivative(Debug)]
pub struct Bus {
    name: FsString,
    #[derivative(Debug = "ignore")]
    devices: Arc<SimpleDirectory>,
}

impl Bus {
    pub(super) fn new(name: FsString, devices: Arc<SimpleDirectory>) -> Self {
        Self { name, devices }
    }

    pub fn name(&self) -> &FsStr {
        self.name.as_ref()
    }

    pub(super) fn devices(&self) -> &Arc<SimpleDirectory> {
        &self.devices
    }
}

/// Subsystem with which a [`Device`] is associated.
#[derive(Clone, Debug)]
pub enum DeviceSubsystem {
    Bus(Bus),
    Class(Class),
}

pub(super) type Subsystem = DeviceSubsystem;

impl DeviceSubsystem {
    pub(super) fn name(&self) -> &FsStr {
        match self {
            Self::Bus(bus) => bus.name(),
            Self::Class(class) => class.name(),
        }
    }
}

impl From<Bus> for DeviceSubsystem {
    fn from(bus: Bus) -> Self {
        Self::Bus(bus)
    }
}

impl From<Class> for DeviceSubsystem {
    fn from(class: Class) -> Self {
        Self::Class(class)
    }
}

pub type UEventProperties = Vec<(FsString, FsString)>;

/// Device node in the `/sys/devices` hierarchy that can act as a parent for other devices.
#[derive(Clone, Derivative)]
#[derivative(Debug)]
pub struct Device {
    name: FsString,
    parent: Option<Arc<Device>>,
    subsystem: Option<Subsystem>,
    metadata: Option<DeviceMetadata>,
    /// Weak reference to the device's directory in `/sys/devices/...`.
    ///
    /// [`KObjectStore`](super::kobject_store::KObjectStore) strongly owns live device directories
    /// via the sysfs root directory hierarchy. Storing a weak reference here avoids an `Arc` cycle
    /// with the directory's `"uevent"` [`UEventFsNode`], which holds a clone of [`Device`].
    #[derivative(Debug = "ignore")]
    dir: Weak<SimpleDirectory>,
}

impl Device {
    pub(super) fn new(
        name: FsString,
        parent: Option<Device>,
        subsystem: Option<Subsystem>,
        metadata: Option<DeviceMetadata>,
        dir: Arc<SimpleDirectory>,
    ) -> Self {
        debug_assert!(parent.is_some() || !matches!(subsystem, Some(Subsystem::Class(_))));
        Self { name, parent: parent.map(Arc::new), subsystem, metadata, dir: Arc::downgrade(&dir) }
    }

    pub fn name(&self) -> &FsStr {
        self.name.as_ref()
    }

    pub fn parent(&self) -> Option<&Device> {
        self.parent.as_deref()
    }

    pub fn metadata(&self) -> Option<&DeviceMetadata> {
        self.metadata.as_ref()
    }

    pub(super) fn subsystem(&self) -> Option<&Subsystem> {
        self.subsystem.as_ref()
    }

    pub fn dir(&self) -> Option<Arc<SimpleDirectory>> {
        self.dir.upgrade()
    }

    /// Intermediate `<class>` directory name inserted under `parent` for `subsystem`, if any.
    pub(super) fn glue_dir_name_for<'a>(
        parent: Option<&Device>,
        subsystem: Option<&'a Subsystem>,
    ) -> Option<&'a FsStr> {
        let (Some(parent), Some(Subsystem::Class(class))) = (parent, subsystem) else {
            return None;
        };
        (!matches!(parent.subsystem(), Some(Subsystem::Class(_)))).then(|| class.name())
    }

    /// Intermediate `<class>` directory name inserted between [`Self::parent`] and this device, if
    /// any.
    pub(super) fn glue_dir_name(&self) -> Option<&FsStr> {
        Self::glue_dir_name_for(self.parent(), self.subsystem())
    }

    /// Relative path to the device from a directory `depth` levels below the sysfs root.
    pub fn path_from_depth(&self, depth: usize) -> FsString {
        let mut builder = PathBuilder::new();
        for current in std::iter::successors(Some(self), |dev| dev.parent()) {
            builder.prepend_element(current.name());
            if let Some(glue_dir) = current.glue_dir_name() {
                builder.prepend_element(glue_dir);
            }
        }
        builder.prepend_element(b"devices".into());
        for _ in 0..depth {
            builder.prepend_element(b"..".into());
        }
        builder.build_relative()
    }

    pub fn uevent_properties(&self, separator: char) -> FsString {
        let props = self.get_uevent_properties_list();
        flatten_uevent_properties(props, separator)
    }

    pub fn get_uevent_properties_list(&self) -> UEventProperties {
        let mut props = vec![];

        // TODO(https://fxbug.dev/42078277): Pass the synthetic UUID when available.
        // Otherwise, default as "0".
        let path = self.path_from_depth(0);

        let mut devpath = vec![b'/'];
        devpath.extend_from_slice(path.as_ref());

        props.push((b"DEVPATH".into(), devpath.into()));
        if let Some(subsystem) = &self.subsystem {
            props.push((b"SUBSYSTEM".into(), subsystem.name().to_owned()));
        }

        if let Some(metadata) = &self.metadata {
            props.push((b"DEVNAME".into(), metadata.devname.clone()));
            props.push((b"SYNTH_UUID".into(), b"0".into()));
            props.push((b"MAJOR".into(), metadata.devt.major().to_string().into()));
            props.push((b"MINOR".into(), metadata.devt.minor().to_string().into()));
            let scope = RcuReadScope::new();
            for (key, value) in metadata.properties.iter(&scope) {
                props.push((key.clone(), value.clone()));
            }
        }

        props
    }
}

pub fn flatten_uevent_properties(props: UEventProperties, separator: char) -> FsString {
    let mut result = vec![];
    let sep = separator as u8;
    for (key, value) in props {
        result.extend_from_slice(key.as_ref());
        result.push(b'=');
        result.extend_from_slice(value.as_ref());
        result.push(sep);
    }
    result.into()
}

#[derive(Clone, Debug)]
pub struct DeviceMetadata {
    /// Name of the device in /dev.
    ///
    /// Also appears in sysfs via uevent.
    pub devname: FsString,
    pub devt: DeviceId,
    pub mode: DeviceMode,
    pub properties: Arc<RcuHashMap<FsString, FsString>>,
}

impl DeviceMetadata {
    pub fn new(devname: FsString, devt: DeviceId, mode: DeviceMode) -> Self {
        Self { devname, devt, mode, properties: Arc::new(RcuHashMap::default()) }
    }

    pub fn with_devtype(self, devtype: impl Into<FsString>) -> Self {
        self.properties.insert(b"DEVTYPE".into(), devtype.into());
        self
    }
}

pub struct UEventFsNode {
    device: Device,
}

impl UEventFsNode {
    pub fn new(device: Device) -> Self {
        Self { device }
    }
}

impl FsNodeOps for UEventFsNode {
    fs_node_impl_not_dir!();

    fn create_file_ops(
        &self,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        Ok(Box::new(UEventFile::new(self.device.clone())))
    }
}

struct UEventFile {
    device: Device,
}

impl UEventFile {
    pub fn new(device: Device) -> Self {
        Self { device }
    }

    fn parse_commands(data: &[u8]) -> Vec<&[u8]> {
        data.split(|&c| c == b'\0' || c == b'\n').collect()
    }
}

impl FileOps for UEventFile {
    fileops_impl_seekable!();
    fileops_impl_noop_sync!();

    fn read(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        let content = self.device.uevent_properties('\n');
        let content_bytes: &[u8] = content.as_ref();
        data.write(content_bytes.get(offset..).ok_or_else(|| errno!(EINVAL))?)
    }

    fn write(
        &self,
        _file: &FileObject,
        current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        if offset != 0 {
            return error!(EINVAL);
        }
        let content = data.read_all()?;
        for command in Self::parse_commands(&content) {
            // Ignore empty lines.
            if command == b"" {
                continue;
            }

            match UEventAction::try_from(command) {
                Ok(c) => {
                    current_task.kernel().device_registry.dispatch_uevent(c, self.device.clone())
                }
                Err(e) => {
                    track_stub!(TODO("https://fxbug.dev/297435061"), "synthetic uevent variables");
                    return Err(e);
                }
            }
        }
        Ok(content.len())
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum UEventAction {
    Add,
    Remove,
    Change,
    Move,
    Online,
    Offline,
    Bind,
    Unbind,
}

impl std::fmt::Display for UEventAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UEventAction::Add => write!(f, "add"),
            UEventAction::Remove => write!(f, "remove"),
            UEventAction::Change => write!(f, "change"),
            UEventAction::Move => write!(f, "move"),
            UEventAction::Online => write!(f, "online"),
            UEventAction::Offline => write!(f, "offline"),
            UEventAction::Bind => write!(f, "bind"),
            UEventAction::Unbind => write!(f, "unbind"),
        }
    }
}

impl TryFrom<&[u8]> for UEventAction {
    type Error = Errno;

    fn try_from(action: &[u8]) -> Result<Self, Self::Error> {
        match action {
            b"add" => Ok(UEventAction::Add),
            b"remove" => Ok(UEventAction::Remove),
            b"change" => Ok(UEventAction::Change),
            b"move" => Ok(UEventAction::Move),
            b"online" => Ok(UEventAction::Online),
            b"offline" => Ok(UEventAction::Offline),
            b"bind" => Ok(UEventAction::Bind),
            b"unbind" => Ok(UEventAction::Unbind),
            _ => error!(EINVAL),
        }
    }
}

#[derive(Copy, Clone)]
pub struct UEventContext {
    pub seqnum: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::pseudo::simple_directory::SimpleDirectory;
    use starnix_uapi::device_id::DeviceId;

    #[test]
    fn test_uevent_properties() {
        let dir = SimpleDirectory::new();
        let devices = SimpleDirectory::new();
        let bus = Bus::new("bus".into(), devices.clone());
        let parent = Device::new(
            "bus_dev".into(),
            /* parent = */ None,
            Some(bus.into()),
            /* metadata = */ None,
            dir.clone(),
        );
        let class = Class::new("class".into(), devices);
        let device = Device::new(
            "device".into(),
            Some(parent),
            Some(class.into()),
            Some(
                DeviceMetadata::new("devname".into(), DeviceId::new(1, 2), DeviceMode::Char)
                    .with_devtype("disk"),
            ),
            dir,
        );

        assert_eq!(
            device.uevent_properties('\n'),
            b"DEVPATH=/devices/bus_dev/class/device\n\
             SUBSYSTEM=class\n\
             DEVNAME=devname\n\
             SYNTH_UUID=0\n\
             MAJOR=1\n\
             MINOR=2\n\
             DEVTYPE=disk\n"
        );
    }

    #[test]
    fn test_uevent_properties_no_devtype() {
        let dir = SimpleDirectory::new();
        let devices = SimpleDirectory::new();
        let bus = Bus::new("bus".into(), devices.clone());
        let parent = Device::new(
            "bus_dev".into(),
            /* parent = */ None,
            Some(bus.into()),
            /* metadata = */ None,
            dir.clone(),
        );
        let class = Class::new("class".into(), devices);
        let device = Device::new(
            "device".into(),
            Some(parent),
            Some(class.into()),
            Some(DeviceMetadata::new("devname".into(), DeviceId::new(1, 2), DeviceMode::Char)),
            dir,
        );

        assert_eq!(
            device.uevent_properties('\n'),
            b"DEVPATH=/devices/bus_dev/class/device\n\
             SUBSYSTEM=class\n\
             DEVNAME=devname\n\
             SYNTH_UUID=0\n\
             MAJOR=1\n\
             MINOR=2\n"
        );
    }

    #[::fuchsia::test]
    fn test_root_bus_and_nested_class_device_uevent_properties() {
        let dir = SimpleDirectory::new();
        let root_device = Device::new(
            "platform".into(),
            /* parent = */ None,
            /* subsystem = */ None,
            /* metadata = */ None,
            dir.clone(),
        );
        assert_eq!(root_device.path_from_depth(0), b"devices/platform");
        assert_eq!(root_device.uevent_properties('\n'), b"DEVPATH=/devices/platform\n");

        let platform_bus = Bus::new("platform".into(), SimpleDirectory::new());
        let soc_device = Device::new(
            "soc".into(),
            Some(root_device),
            Some(platform_bus.clone().into()),
            /* metadata = */ None,
            dir.clone(),
        );
        assert_eq!(soc_device.path_from_depth(0), b"devices/platform/soc");
        assert_eq!(soc_device.path_from_depth(3), b"../../../devices/platform/soc");
        assert_eq!(
            soc_device.uevent_properties('\n'),
            b"DEVPATH=/devices/platform/soc\n\
             SUBSYSTEM=platform\n"
        );

        let drm_class = Class::new("drm".into(), SimpleDirectory::new());
        let card0 = Device::new(
            "card0".into(),
            Some(soc_device),
            Some(drm_class.clone().into()),
            /* metadata = */ None,
            dir.clone(),
        );
        assert_eq!(card0.path_from_depth(0), b"devices/platform/soc/drm/card0");

        let connector = Device::new(
            "sde-conn-0-DSI-1".into(),
            Some(card0.clone()),
            Some(drm_class.into()),
            /* metadata = */ None,
            dir.clone(),
        );
        assert_eq!(
            connector.path_from_depth(0),
            b"devices/platform/soc/drm/card0/sde-conn-0-DSI-1"
        );
        assert_eq!(
            connector.uevent_properties('\n'),
            b"DEVPATH=/devices/platform/soc/drm/card0/sde-conn-0-DSI-1\n\
             SUBSYSTEM=drm\n"
        );

        // Bus device parented under a class device does not insert a glue directory,
        // while a class device parented under that bus device does insert its class directory.
        let child_bus_dev = Device::new(
            "aux_dev".into(),
            Some(card0),
            Some(platform_bus.into()),
            /* metadata = */ None,
            dir.clone(),
        );
        assert_eq!(child_bus_dev.path_from_depth(0), b"devices/platform/soc/drm/card0/aux_dev");

        let wakeup_class = Class::new("wakeup".into(), SimpleDirectory::new());
        let grandchild_class_dev = Device::new(
            "wakeup0".into(),
            Some(child_bus_dev),
            Some(wakeup_class.into()),
            /* metadata = */ None,
            dir,
        );
        assert_eq!(
            grandchild_class_dev.path_from_depth(0),
            b"devices/platform/soc/drm/card0/aux_dev/wakeup/wakeup0"
        );
    }

    #[::fuchsia::test]
    fn test_get_uevent_properties_list() {
        let virtual_device = Device::new(
            "virtual".into(),
            /* parent = */ None,
            /* subsystem = */ None,
            /* metadata = */ None,
            SimpleDirectory::new(),
        );
        let class = Class::new("android_usb".into(), SimpleDirectory::new());
        let metadata =
            DeviceMetadata::new("android0".into(), DeviceId::new(1, 2), DeviceMode::Char);
        let device = Device::new(
            "android0".into(),
            Some(virtual_device),
            Some(class.into()),
            Some(metadata),
            SimpleDirectory::new(),
        );

        let props = device.get_uevent_properties_list();

        // Now we have metadata, so we expect more properties (DEVNAME, SYNTH_UUID, MAJOR, MINOR).
        // Original count was 2 (DEVPATH, SUBSYSTEM).
        // Now we add: DEVNAME, SYNTH_UUID, MAJOR, MINOR. Total 6.
        assert_eq!(props.len(), 6);
        assert_eq!(props[0], ("DEVPATH".into(), "/devices/virtual/android_usb/android0".into()));
        assert_eq!(props[1], ("SUBSYSTEM".into(), "android_usb".into()));

        let properties = &device.metadata().unwrap().properties;
        properties.insert("USB_STATE".into(), "CONNECTED".into());
        properties.insert("ABC".into(), "XYZ".into());
        properties.insert("FOO".into(), "BAR".into());

        let mut props = device.get_uevent_properties_list();

        assert_eq!(props.len(), 9);
        // The properties from the metadata HashMap are in non-deterministic order.
        // Sort them by key to make assertions deterministic.
        props[6..].sort();
        assert_eq!(props[6], ("ABC".into(), "XYZ".into()));
        assert_eq!(props[7], ("FOO".into(), "BAR".into()));
        assert_eq!(props[8], ("USB_STATE".into(), "CONNECTED".into()));
    }
}
