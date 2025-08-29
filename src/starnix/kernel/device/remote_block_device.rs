// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::device::DeviceMode;
use crate::device::kobject::DeviceMetadata;
use crate::fs::sysfs::{BlockDeviceInfo, build_block_device_directory};
use crate::mm::memory::MemoryObject;
use crate::mm::{MemoryAccessorExt, ProtectionFlags};
use crate::task::CurrentTask;
use crate::vfs::buffers::{InputBuffer, OutputBuffer};
use crate::vfs::{
    FileObject, FileOps, FsString, NamespaceNode, SeekTarget, default_ioctl, default_seek,
};
use anyhow::Error;
use starnix_sync::{FileOpsCore, LockEqualOrBefore, Locked, Mutex, Unlocked};
use starnix_syscalls::{SUCCESS, SyscallArg, SyscallResult};
use starnix_uapi::device_type::{DeviceType, REMOTE_BLOCK_MAJOR};
use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::user_address::UserRef;
use starnix_uapi::{BLKGETSIZE, BLKGETSIZE64, errno, from_status_like_fdio, off_t};
use std::collections::btree_map::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

/// A block device which is backed by a VMO.  Notably, the contents of the device are not persistent
/// across reboots.
#[derive(Debug)]
pub struct RemoteBlockDevice {
    name: String,
    backing_memory: MemoryObject,
    backing_memory_size: usize,
    block_size: u32,
}

const BLOCK_SIZE: u32 = 512;

impl RemoteBlockDevice {
    pub fn read(&self, offset: u64, buf: &mut [u8]) -> Result<(), Error> {
        Ok(self.backing_memory.read(buf, offset)?)
    }

    fn new<L>(
        locked: &mut Locked<L>,
        current_task: &CurrentTask,
        minor: u32,
        name: &str,
        backing_memory: MemoryObject,
    ) -> Result<Arc<Self>, Errno>
    where
        L: LockEqualOrBefore<FileOpsCore>,
    {
        let kernel = current_task.kernel();
        let registry = &kernel.device_registry;
        let device_name = FsString::from(format!("remoteblk-{name}"));
        let virtual_block_class = registry.objects.virtual_block_class();
        let backing_memory_size = backing_memory.get_content_size() as usize;
        let device = Arc::new(Self {
            name: name.to_owned(),
            backing_memory,
            backing_memory_size,
            block_size: BLOCK_SIZE,
        });
        let device_weak = Arc::<RemoteBlockDevice>::downgrade(&device);
        registry.add_device(
            locked,
            current_task,
            device_name.as_ref(),
            DeviceMetadata::new(
                device_name.clone(),
                DeviceType::new(REMOTE_BLOCK_MAJOR, minor),
                DeviceMode::Block,
            ),
            virtual_block_class,
            |device, dir| build_block_device_directory(device, device_weak, dir),
        )?;
        Ok(device)
    }

    pub fn create_file_ops(self: &Arc<Self>) -> Box<dyn FileOps> {
        Box::new(RemoteBlockDeviceFile { device: self.clone() })
    }
}

impl BlockDeviceInfo for RemoteBlockDevice {
    fn size(&self) -> Result<usize, Errno> {
        Ok(self.backing_memory.get_size() as usize)
    }
}

struct RemoteBlockDeviceFile {
    device: Arc<RemoteBlockDevice>,
}

impl FileOps for RemoteBlockDeviceFile {
    fn has_persistent_offsets(&self) -> bool {
        true
    }

    fn is_seekable(&self) -> bool {
        true
    }

    // Manually implement seek, because default_eof_offset uses st_size (which is not used for block
    // devices).
    fn seek(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        current_offset: off_t,
        target: SeekTarget,
    ) -> Result<off_t, Errno> {
        default_seek(current_offset, target, || {
            self.device.backing_memory_size.try_into().map_err(|_| errno!(EINVAL))
        })
    }

    fn read(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        mut offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        data.write_each(&mut move |buf| {
            let buflen = buf.len();
            let buf = &mut buf
                [..std::cmp::min(self.device.backing_memory_size.saturating_sub(offset), buflen)];
            if !buf.is_empty() {
                self.device
                    .backing_memory
                    .read_uninit(buf, offset as u64)
                    .map_err(|status| from_status_like_fdio!(status))?;
                offset = offset.checked_add(buf.len()).ok_or_else(|| errno!(EINVAL))?;
            }
            Ok(buf.len())
        })
    }

    fn write(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        mut offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        data.read_each(&mut move |buf| {
            let to_write =
                std::cmp::min(self.device.backing_memory_size.saturating_sub(offset), buf.len());
            self.device
                .backing_memory
                .write(&buf[..to_write], offset as u64)
                .map_err(|status| from_status_like_fdio!(status))?;
            offset = offset.checked_add(to_write).ok_or_else(|| errno!(EINVAL))?;
            Ok(to_write)
        })
    }

    fn sync(&self, _file: &FileObject, _current_task: &CurrentTask) -> Result<(), Errno> {
        Ok(())
    }

    fn get_memory(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        requested_length: Option<usize>,
        _prot: ProtectionFlags,
    ) -> Result<Arc<MemoryObject>, Errno> {
        let slice_len =
            std::cmp::min(self.device.backing_memory_size, requested_length.unwrap_or(usize::MAX))
                as u64;
        self.device
            .backing_memory
            .create_child(zx::VmoChildOptions::SLICE, 0, slice_len)
            .map(Arc::new)
            .map_err(|status| from_status_like_fdio!(status))
    }

    fn ioctl(
        &self,
        locked: &mut Locked<Unlocked>,
        file: &FileObject,
        current_task: &CurrentTask,
        request: u32,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        match request {
            BLKGETSIZE => {
                let user_size = UserRef::<u64>::from(arg);
                let size =
                    (self.device.backing_memory_size / self.device.block_size as usize) as u64;
                current_task.write_object(user_size, &size)?;
                Ok(SUCCESS)
            }
            BLKGETSIZE64 => {
                let user_size = UserRef::<u64>::from(arg);
                let size = self.device.backing_memory_size as u64;
                current_task.write_object(user_size, &size)?;
                Ok(SUCCESS)
            }
            _ => default_ioctl(file, locked, current_task, request, arg),
        }
    }
}

fn open_remote_block_device(
    _locked: &mut Locked<FileOpsCore>,
    current_task: &CurrentTask,
    id: DeviceType,
    _node: &NamespaceNode,
    _flags: OpenFlags,
) -> Result<Box<dyn FileOps>, Errno> {
    Ok(current_task.kernel().remote_block_device_registry.open(id.minor())?.create_file_ops())
}

pub fn remote_block_device_init(locked: &mut Locked<Unlocked>, current_task: &CurrentTask) {
    current_task
        .kernel()
        .device_registry
        .register_major(
            locked,
            "remote-block".into(),
            DeviceMode::Block,
            REMOTE_BLOCK_MAJOR,
            open_remote_block_device,
        )
        .expect("remote block device register failed.");
}

#[derive(Default)]
pub struct RemoteBlockDeviceRegistry {
    devices: Mutex<BTreeMap<u32, Arc<RemoteBlockDevice>>>,
    next_minor: AtomicU32,
    device_added_fn: OnceLock<RemoteBlockDeviceAddedFn>,
}

/// Arguments are (name, minor, device).
pub type RemoteBlockDeviceAddedFn = Box<dyn Fn(&str, u32, &Arc<RemoteBlockDevice>) + Send + Sync>;

impl RemoteBlockDeviceRegistry {
    /// Registers a callback to be invoked for each new device.  Only one callback can be registered.
    pub fn on_device_added(&self, callback: RemoteBlockDeviceAddedFn) {
        self.device_added_fn.set(callback).map_err(|_| ()).expect("Callback already set");
    }

    /// Creates a new block device called `name` if absent.  Does nothing if the device already
    /// exists.
    pub fn create_remote_block_device_if_absent<L>(
        &self,
        locked: &mut Locked<L>,
        current_task: &CurrentTask,
        name: &str,
        initial_size: u64,
    ) -> Result<(), Error>
    where
        L: LockEqualOrBefore<FileOpsCore>,
    {
        let mut devices = self.devices.lock();
        if devices.values().find(|dev| &dev.name == name).is_some() {
            return Ok(());
        }

        let backing_memory = MemoryObject::from(zx::Vmo::create(initial_size)?)
            .with_zx_name(b"starnix:remote_block_device");
        let minor = self.next_minor.fetch_add(1, Ordering::Relaxed);
        let device = RemoteBlockDevice::new(locked, current_task, minor, name, backing_memory)?;
        if let Some(callback) = self.device_added_fn.get() {
            callback(name, minor, &device);
        }
        devices.insert(minor, device);
        Ok(())
    }

    pub fn open(&self, minor: u32) -> Result<Arc<RemoteBlockDevice>, Errno> {
        self.devices.lock().get(&minor).ok_or_else(|| errno!(ENODEV)).cloned()
    }
}
