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
use anyhow::{Context as _, Error};
use block_client::{BufferSlice, MutableBufferSlice, RemoteBlockClientSync};
use fidl::endpoints::ClientEnd;
use fidl_fuchsia_hardware_block_volume::VolumeMarker;
use starnix_sync::{FileOpsCore, LockEqualOrBefore, Locked, Mutex, Unlocked};
use starnix_syscalls::{SUCCESS, SyscallArg, SyscallResult};
use starnix_uapi::device_type::{BLOCK_EXTENDED_MAJOR, DeviceType};
use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::user_address::UserRef;
use starnix_uapi::{BLKGETSIZE, BLKGETSIZE64, errno, from_status_like_fdio, off_t};
use std::collections::btree_map::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

pub struct RemoteBlockDeviceVmo {
    backing_memory: MemoryObject,
    backing_memory_size: usize,
    block_size: u32,
}

/// A block device. This can be backed by a VMO or a Fuchsia block client depending on how it was
/// configured.
/// TODO(https://fxbug.dev/407091711): Remove VMO-backed block devices.
pub enum RemoteBlockDevice {
    Vmo(Arc<RemoteBlockDeviceVmo>),
    Fuchsia(Arc<RemoteBlockClientSync>),
}

const BLOCK_SIZE: u32 = 512;

impl RemoteBlockDevice {
    pub fn read(&self, offset: u64, buf: &mut [u8]) -> Result<(), Error> {
        match self {
            RemoteBlockDevice::Vmo(device) => Ok(device.backing_memory.read(buf, offset)?),
            RemoteBlockDevice::Fuchsia(block_client) => block_client
                .read_at(MutableBufferSlice::Memory(buf), offset as u64)
                .context("read_at failed"),
        }
    }

    fn new_from_vmo<L>(
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
        let device = Arc::new(Self::Vmo(Arc::new(RemoteBlockDeviceVmo {
            backing_memory,
            backing_memory_size,
            block_size: BLOCK_SIZE,
        })));
        let device_weak = Arc::<RemoteBlockDevice>::downgrade(&device);
        registry.add_device(
            locked,
            current_task,
            device_name.as_ref(),
            DeviceMetadata::new(
                device_name.clone(),
                DeviceType::new(BLOCK_EXTENDED_MAJOR, minor),
                DeviceMode::Block,
            ),
            virtual_block_class,
            |device, dir| build_block_device_directory(device, device_weak, dir),
        )?;
        Ok(device)
    }

    fn new<L>(
        locked: &mut Locked<L>,
        current_task: &CurrentTask,
        minor: u32,
        name: &str,
        block: ClientEnd<VolumeMarker>,
    ) -> Result<Arc<Self>, Errno>
    where
        L: LockEqualOrBefore<FileOpsCore>,
    {
        let kernel = current_task.kernel();
        let registry = &kernel.device_registry;
        let device_name = FsString::from(name);
        let virtual_block_class = registry.objects.virtual_block_class();
        let block_client = RemoteBlockClientSync::new(block.into_channel().into())
            .map_err(|status| from_status_like_fdio!(status))?;
        let device = Arc::new(Self::Fuchsia(Arc::new(block_client)));
        let device_weak = Arc::<RemoteBlockDevice>::downgrade(&device);
        registry.add_device(
            locked,
            current_task,
            device_name.as_ref(),
            DeviceMetadata::new(
                device_name.clone(),
                DeviceType::new(BLOCK_EXTENDED_MAJOR, minor),
                DeviceMode::Block,
            ),
            virtual_block_class,
            |device, dir| build_block_device_directory(device, device_weak, dir),
        )?;
        Ok(device)
    }

    pub fn create_file_ops(&self) -> Box<dyn FileOps> {
        match self {
            RemoteBlockDevice::Vmo(device) => {
                Box::new(VmoBlockDeviceFile { device: device.clone() })
            }
            RemoteBlockDevice::Fuchsia(block_client) => {
                Box::new(RemoteBlockDeviceFile { block_client: block_client.clone() })
            }
        }
    }
}

impl BlockDeviceInfo for RemoteBlockDevice {
    fn size(&self) -> Result<usize, Errno> {
        match self {
            RemoteBlockDevice::Vmo(device) => Ok(device.backing_memory.get_size() as usize),
            RemoteBlockDevice::Fuchsia(block_client) => (block_client.block_count() as usize)
                .checked_mul(block_client.block_size() as usize)
                .ok_or_else(|| errno!(EINVAL)),
        }
    }
}

struct VmoBlockDeviceFile {
    device: Arc<RemoteBlockDeviceVmo>,
}

impl FileOps for VmoBlockDeviceFile {
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

struct RemoteBlockDeviceFile {
    block_client: Arc<RemoteBlockClientSync>,
}

impl RemoteBlockDeviceFile {
    fn size(&self) -> Result<usize, Errno> {
        (self.block_client.block_count() as usize)
            .checked_mul(self.block_client.block_size() as usize)
            .ok_or_else(|| errno!(EINVAL))
    }
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
        default_seek(current_offset, target, || self.size()?.try_into().map_err(|_| errno!(EINVAL)))
    }

    fn read(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        mut offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        let size = self.size()?;
        let block_size = self.block_client.block_size() as usize;
        const MAX_CHUNK_SIZE: usize = 32 * 1024; // 32KB

        let mut total_read = 0;
        while data.available() > 0 && offset < size {
            let chunk_len = std::cmp::min(data.available(), MAX_CHUNK_SIZE);
            let chunk_len = std::cmp::min(chunk_len, size - offset);
            if chunk_len == 0 {
                break;
            }

            let aligned_offset = offset - offset % block_size;
            let end_offset = offset + chunk_len;
            let aligned_end_offset = std::cmp::min(
                end_offset.checked_next_multiple_of(block_size).ok_or_else(|| errno!(EINVAL))?,
                size,
            );
            let aligned_data_length = aligned_end_offset - aligned_offset;

            let mut read_data = vec![0u8; aligned_data_length];
            self.block_client
                .read_at(MutableBufferSlice::Memory(&mut read_data), aligned_offset as u64)
                .map_err(|status| from_status_like_fdio!(status))?;

            let read_offset = offset - aligned_offset;
            let read_end = read_offset + chunk_len;
            let bytes_written = data.write(&read_data[read_offset..read_end])?;

            offset += bytes_written;
            total_read += bytes_written;

            if bytes_written < chunk_len {
                break;
            }
        }
        Ok(total_read)
    }

    fn write(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        mut offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        let size = self.size()?;
        let block_size = self.block_client.block_size() as usize;
        const MAX_CHUNK_SIZE: usize = 32 * 1024; // 32KB

        let mut total_written = 0;
        while data.available() > 0 && offset < size {
            let chunk_len = std::cmp::min(data.available(), MAX_CHUNK_SIZE);
            let chunk_len = std::cmp::min(chunk_len, size - offset);
            if chunk_len == 0 {
                break;
            }

            let aligned_offset = offset - offset % block_size;
            let end_offset = offset + chunk_len;
            let aligned_end_offset = std::cmp::min(
                end_offset.checked_next_multiple_of(block_size).ok_or_else(|| errno!(EINVAL))?,
                size,
            );
            let aligned_data_length = aligned_end_offset - aligned_offset;

            let mut write_buf = vec![0u8; aligned_data_length];

            // Read-Modify-Write: If the write is not block-aligned at the start or end,
            // we need to read the existing data for the first and/or last block to preserve it.
            let head_unaligned = offset > aligned_offset;
            let tail_unaligned = end_offset < aligned_end_offset;

            if head_unaligned {
                self.block_client
                    .read_at(
                        MutableBufferSlice::Memory(&mut write_buf[..block_size]),
                        aligned_offset as u64,
                    )
                    .map_err(|status| from_status_like_fdio!(status))?;
            }

            if tail_unaligned {
                let last_block_start = aligned_data_length - block_size;
                // If we already read the first block and it's the same as the last block, don't
                // read again.
                if !head_unaligned || last_block_start > 0 {
                    self.block_client
                        .read_at(
                            MutableBufferSlice::Memory(&mut write_buf[last_block_start..]),
                            (aligned_offset + last_block_start) as u64,
                        )
                        .map_err(|status| from_status_like_fdio!(status))?;
                }
            }

            let write_offset = offset - aligned_offset;
            let write_slice = &mut write_buf[write_offset..write_offset + chunk_len];
            // SAFETY: We are writing to a buffer of u8, which is always initialized.
            // We can safely cast &mut [u8] to &mut [MaybeUninit<u8>].
            let write_slice_uninit = unsafe {
                std::slice::from_raw_parts_mut(
                    write_slice.as_mut_ptr() as *mut std::mem::MaybeUninit<u8>,
                    write_slice.len(),
                )
            };
            let bytes_read = data.read(write_slice_uninit)?;

            self.block_client
                .write_at(BufferSlice::Memory(&write_buf), aligned_offset as u64)
                .map_err(|status| from_status_like_fdio!(status))?;

            offset += bytes_read;
            total_written += bytes_read;

            if bytes_read < chunk_len {
                break;
            }
        }
        Ok(total_written)
    }

    fn sync(&self, _file: &FileObject, _current_task: &CurrentTask) -> Result<(), Errno> {
        self.block_client.flush().map_err(|status| from_status_like_fdio!(status))
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
                let size = self.block_client.block_count();
                current_task.write_object(user_size, &size)?;
                Ok(SUCCESS)
            }
            BLKGETSIZE64 => {
                let user_size = UserRef::<u64>::from(arg);
                let size = self.size()? as u64;
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
            BLOCK_EXTENDED_MAJOR,
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
    pub fn create_vmo_block_device<L>(
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
        let backing_memory = MemoryObject::from(zx::Vmo::create(initial_size)?)
            .with_zx_name(b"starnix:remote_block_device");
        let minor = self.next_minor.fetch_add(1, Ordering::Relaxed);
        let device =
            RemoteBlockDevice::new_from_vmo(locked, current_task, minor, name, backing_memory)?;
        if let Some(callback) = self.device_added_fn.get() {
            callback(name, minor, &device);
        }
        devices.insert(minor, device);
        Ok(())
    }

    pub fn create_remote_block_device<L>(
        &self,
        locked: &mut Locked<L>,
        current_task: &CurrentTask,
        name: &str,
        block: ClientEnd<VolumeMarker>,
    ) -> Result<(), Error>
    where
        L: LockEqualOrBefore<FileOpsCore>,
    {
        let mut devices = self.devices.lock();
        let minor = self.next_minor.fetch_add(1, Ordering::Relaxed);
        let device = RemoteBlockDevice::new(locked, current_task, minor, name, block)?;
        devices.insert(minor, device);
        Ok(())
    }

    pub fn open(&self, minor: u32) -> Result<Arc<RemoteBlockDevice>, Errno> {
        self.devices.lock().get(&minor).ok_or_else(|| errno!(ENODEV)).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{anon_test_file, map_object_anywhere, spawn_kernel_and_run};
    use crate::vfs::{SeekTarget, VecInputBuffer, VecOutputBuffer};
    use starnix_uapi::open_flags::OpenFlags;
    use starnix_uapi::{BLKGETSIZE, BLKGETSIZE64};
    use vmo_backed_block_server::{VmoBackedServer, VmoBackedServerTestingExt};
    use zerocopy::FromBytes as _;

    #[::fuchsia::test]
    async fn test_vmo_block_device_registry() {
        spawn_kernel_and_run(async |locked, current_task| {
            let kernel = current_task.kernel();
            remote_block_device_init(locked, &current_task);
            let registry = kernel.remote_block_device_registry.clone();

            registry
                .create_vmo_block_device(locked, &current_task, "test", 1024)
                .expect("create_vmo_block_device failed.");

            let device = registry.open(0).expect("open failed.");
            let file =
                anon_test_file(locked, &current_task, device.create_file_ops(), OpenFlags::RDWR);

            let arg_addr = map_object_anywhere(locked, &current_task, &0u64);
            let mut arg = [0u8; 8];

            file.ioctl(locked, &current_task, BLKGETSIZE64, arg_addr.into()).expect("ioctl failed");
            current_task.read_memory_to_slice(arg_addr, &mut arg).unwrap();
            assert_eq!(u64::read_from_bytes(&arg).unwrap(), 1024);

            file.ioctl(locked, &current_task, BLKGETSIZE, arg_addr.into()).expect("ioctl failed");
            current_task.read_memory_to_slice(arg_addr, &mut arg).unwrap();
            assert_eq!(u64::read_from_bytes(&arg).unwrap(), 2);

            let mut buf = VecOutputBuffer::new(512);
            file.read(locked, &current_task, &mut buf).expect("read failed.");
            assert_eq!(buf.data(), &[0u8; 512]);

            let mut buf = VecInputBuffer::from(vec![1u8; 512]);
            file.seek(locked, &current_task, SeekTarget::Set(0)).expect("seek failed");
            file.write(locked, &current_task, &mut buf).expect("write failed.");

            let mut buf = VecOutputBuffer::new(512);
            file.seek(locked, &current_task, SeekTarget::Set(0)).expect("seek failed");
            file.read(locked, &current_task, &mut buf).expect("read failed.");
            assert_eq!(buf.data(), &[1u8; 512]);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_vmo_read_write_past_eof() {
        spawn_kernel_and_run(async |locked, current_task| {
            let kernel = current_task.kernel();
            remote_block_device_init(locked, &current_task);
            let registry = kernel.remote_block_device_registry.clone();

            registry
                .create_vmo_block_device(locked, &current_task, "test", 1024)
                .expect("create_vmo_block_device failed.");

            let device = registry.open(0).expect("open failed.");
            let file =
                anon_test_file(locked, &current_task, device.create_file_ops(), OpenFlags::RDWR);

            file.seek(locked, &current_task, SeekTarget::End(0)).expect("seek failed");
            let mut buf = VecOutputBuffer::new(512);
            assert_eq!(file.read(locked, &current_task, &mut buf).expect("read failed."), 0);

            let mut buf = VecInputBuffer::from(vec![1u8; 512]);
            assert_eq!(file.write(locked, &current_task, &mut buf).expect("write failed."), 0);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_remote_block_device_registry() {
        spawn_kernel_and_run(async |locked, current_task| {
            let kernel = current_task.kernel();
            remote_block_device_init(locked, &current_task);
            let registry = kernel.remote_block_device_registry.clone();
            let server = Arc::new(VmoBackedServer::new(2, 512, &[0u8; 1024]));
            let (client, server_end) = fidl::endpoints::create_endpoints::<VolumeMarker>();
            std::thread::spawn(move || {
                let mut executor = fuchsia_async::LocalExecutor::default();
                executor.run_singlethreaded(async move {
                    use fidl::endpoints::RequestStream;
                    server.serve(server_end.into_stream().cast_stream()).await.unwrap();
                });
            });

            registry
                .create_remote_block_device(locked, &current_task, "test", client)
                .expect("create_remote_block_device failed.");

            let device = registry.open(0).expect("open failed.");
            let file =
                anon_test_file(locked, &current_task, device.create_file_ops(), OpenFlags::RDWR);

            let arg_addr = map_object_anywhere(locked, &current_task, &0u64);
            let mut arg = [0u8; 8];

            file.ioctl(locked, &current_task, BLKGETSIZE64, arg_addr.into()).expect("ioctl failed");
            current_task.read_memory_to_slice(arg_addr, &mut arg).unwrap();
            assert_eq!(u64::read_from_bytes(&arg).unwrap(), 1024);

            file.ioctl(locked, &current_task, BLKGETSIZE, arg_addr.into()).expect("ioctl failed");
            current_task.read_memory_to_slice(arg_addr, &mut arg).unwrap();
            assert_eq!(u64::read_from_bytes(&arg).unwrap(), 2);

            // Deliberately read with a non-block-aligned buffer size. These reads come from
            // uncontrolled sources so we need to be able to handle the alignment ourselves.
            let mut buf = VecOutputBuffer::new(256);
            file.read(locked, &current_task, &mut buf).expect("read failed.");
            assert_eq!(buf.data(), &[0u8; 256]);

            let mut buf = VecInputBuffer::from(vec![1u8; 256]);
            file.seek(locked, &current_task, SeekTarget::Set(0)).expect("seek failed");
            file.write(locked, &current_task, &mut buf).expect("write failed.");

            let mut buf = VecOutputBuffer::new(256);
            file.seek(locked, &current_task, SeekTarget::Set(0)).expect("seek failed");
            file.read(locked, &current_task, &mut buf).expect("read failed.");
            assert_eq!(buf.data(), &[1u8; 256]);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_read_write_past_eof() {
        spawn_kernel_and_run(async |locked, current_task| {
            let kernel = current_task.kernel();
            remote_block_device_init(locked, &current_task);
            let registry = kernel.remote_block_device_registry.clone();
            let server = Arc::new(VmoBackedServer::new(2, 512, &[0u8; 1024]));
            let (client, server_end) = fidl::endpoints::create_endpoints::<VolumeMarker>();
            std::thread::spawn(move || {
                let mut executor = fuchsia_async::LocalExecutor::default();
                executor.run_singlethreaded(async move {
                    use fidl::endpoints::RequestStream;
                    server.serve(server_end.into_stream().cast_stream()).await.unwrap();
                });
            });

            registry
                .create_remote_block_device(locked, &current_task, "test", client)
                .expect("create_remote_block_device failed.");

            let device = registry.open(0).expect("open failed.");
            let file =
                anon_test_file(locked, &current_task, device.create_file_ops(), OpenFlags::RDWR);

            file.seek(locked, &current_task, SeekTarget::End(0)).expect("seek failed");
            let mut buf = VecOutputBuffer::new(512);
            assert_eq!(file.read(locked, &current_task, &mut buf).expect("read failed."), 0);

            let mut buf = VecInputBuffer::from(vec![1u8; 512]);
            assert_eq!(file.write(locked, &current_task, &mut buf).expect("write failed."), 0);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_unaligned_read_write_spanning_blocks() {
        spawn_kernel_and_run(async |locked, current_task| {
            let kernel = current_task.kernel();
            remote_block_device_init(locked, &current_task);
            let registry = kernel.remote_block_device_registry.clone();
            // 3 blocks of 512 bytes = 1536 bytes
            let server = Arc::new(VmoBackedServer::new(3, 512, &[0u8; 1536]));
            let (client, server_end) = fidl::endpoints::create_endpoints::<VolumeMarker>();
            std::thread::spawn(move || {
                let mut executor = fuchsia_async::LocalExecutor::default();
                executor.run_singlethreaded(async move {
                    use fidl::endpoints::RequestStream;
                    server.serve(server_end.into_stream().cast_stream()).await.unwrap();
                });
            });

            registry
                .create_remote_block_device(locked, &current_task, "test", client)
                .expect("create_remote_block_device failed.");

            let device = registry.open(0).expect("open failed.");
            let file =
                anon_test_file(locked, &current_task, device.create_file_ops(), OpenFlags::RDWR);

            // Write spanning across block boundaries (e.g., from 510 to 1026)
            // Start at 510 (2 bytes before end of 1st block)
            // Write 516 bytes (2 bytes in 1st block, 512 bytes in 2nd block, 2 bytes in 3rd block)
            let mut buf = VecInputBuffer::from(vec![0xAAu8; 516]);
            file.seek(locked, &current_task, SeekTarget::Set(510)).expect("seek failed");
            assert_eq!(file.write(locked, &current_task, &mut buf).expect("write failed."), 516);

            // Read back the data
            let mut buf = VecOutputBuffer::new(516);
            file.seek(locked, &current_task, SeekTarget::Set(510)).expect("seek failed");
            assert_eq!(file.read(locked, &current_task, &mut buf).expect("read failed."), 516);
            assert_eq!(buf.data(), &[0xAAu8; 516]);

            // Verify surrounding data is still 0
            let mut buf = VecOutputBuffer::new(1);
            file.seek(locked, &current_task, SeekTarget::Set(509)).expect("seek failed");
            assert_eq!(file.read(locked, &current_task, &mut buf).expect("read failed."), 1);
            assert_eq!(buf.data(), &[0u8]);

            let mut buf = VecOutputBuffer::new(1);
            file.seek(locked, &current_task, SeekTarget::Set(1026)).expect("seek failed");
            assert_eq!(file.read(locked, &current_task, &mut buf).expect("read failed."), 1);
            assert_eq!(buf.data(), &[0u8]);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_exact_eof_boundary() {
        spawn_kernel_and_run(async |locked, current_task| {
            let kernel = current_task.kernel();
            remote_block_device_init(locked, &current_task);
            let registry = kernel.remote_block_device_registry.clone();
            // 2 blocks of 512 bytes = 1024 bytes
            let server = Arc::new(VmoBackedServer::new(2, 512, &[0u8; 1024]));
            let (client, server_end) = fidl::endpoints::create_endpoints::<VolumeMarker>();
            std::thread::spawn(move || {
                let mut executor = fuchsia_async::LocalExecutor::default();
                executor.run_singlethreaded(async move {
                    use fidl::endpoints::RequestStream;
                    server.serve(server_end.into_stream().cast_stream()).await.unwrap();
                });
            });

            registry
                .create_remote_block_device(locked, &current_task, "test", client)
                .expect("create_remote_block_device failed.");

            let device = registry.open(0).expect("open failed.");
            let file =
                anon_test_file(locked, &current_task, device.create_file_ops(), OpenFlags::RDWR);

            // Write ending exactly at EOF (1024)
            // Start at 1020, write 4 bytes
            let mut buf = VecInputBuffer::from(vec![0xBBu8; 4]);
            file.seek(locked, &current_task, SeekTarget::Set(1020)).expect("seek failed");
            assert_eq!(file.write(locked, &current_task, &mut buf).expect("write failed."), 4);

            // Read back
            let mut buf = VecOutputBuffer::new(4);
            file.seek(locked, &current_task, SeekTarget::Set(1020)).expect("seek failed");
            assert_eq!(file.read(locked, &current_task, &mut buf).expect("read failed."), 4);
            assert_eq!(buf.data(), &[0xBBu8; 4]);

            // Try to read past EOF from 1020 (request 5 bytes)
            let mut buf = VecOutputBuffer::new(5);
            file.seek(locked, &current_task, SeekTarget::Set(1020)).expect("seek failed");
            assert_eq!(file.read(locked, &current_task, &mut buf).expect("read failed."), 4);
            assert_eq!(buf.data()[..4], [0xBBu8; 4]);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_rmw_preserves_data() {
        spawn_kernel_and_run(async |locked, current_task| {
            let kernel = current_task.kernel();
            remote_block_device_init(locked, &current_task);
            let registry = kernel.remote_block_device_registry.clone();
            // 3 blocks of 512 bytes = 1536 bytes
            // Initialize with a known pattern (0xFF)
            let server = Arc::new(VmoBackedServer::new(3, 512, &[0xFFu8; 1536]));
            let (client, server_end) = fidl::endpoints::create_endpoints::<VolumeMarker>();
            std::thread::spawn(move || {
                let mut executor = fuchsia_async::LocalExecutor::default();
                executor.run_singlethreaded(async move {
                    use fidl::endpoints::RequestStream;
                    server.serve(server_end.into_stream().cast_stream()).await.unwrap();
                });
            });

            registry
                .create_remote_block_device(locked, &current_task, "test", client)
                .expect("create_remote_block_device failed.");

            let device = registry.open(0).expect("open failed.");
            let file =
                anon_test_file(locked, &current_task, device.create_file_ops(), OpenFlags::RDWR);

            // Write a small chunk in the middle of the second block (offset 600, length 100)
            // Block 1 is 512-1024. 600 is inside.
            // This should trigger RMW for the second block.
            let mut buf = VecInputBuffer::from(vec![0xAAu8; 100]);
            file.seek(locked, &current_task, SeekTarget::Set(600)).expect("seek failed");
            assert_eq!(file.write(locked, &current_task, &mut buf).expect("write failed."), 100);

            // Read back the entire second block to verify
            let mut buf = VecOutputBuffer::new(512);
            file.seek(locked, &current_task, SeekTarget::Set(512)).expect("seek failed");
            assert_eq!(file.read(locked, &current_task, &mut buf).expect("read failed."), 512);
            let data = buf.data();

            // 512 to 600 (88 bytes) should be 0xFF
            assert_eq!(&data[0..88], &[0xFFu8; 88]);
            // 600 to 700 (100 bytes) should be 0xAA
            assert_eq!(&data[88..188], &[0xAAu8; 100]);
            // 700 to 1024 (324 bytes) should be 0xFF
            assert_eq!(&data[188..512], &[0xFFu8; 324]);
        })
        .await;
    }
}
