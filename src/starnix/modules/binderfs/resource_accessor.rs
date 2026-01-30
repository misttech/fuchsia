// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {fidl_fuchsia_posix as fposix, fidl_fuchsia_starnix_binder as fbinder, zx};

use starnix_core::device::mem::new_null_file;
use starnix_core::fs::fuchsia::new_remote_file;
use starnix_core::mm::MemoryAccessor;

use starnix_core::task::{CurrentTask, FullCredentials, Task};
use starnix_core::vfs::{FdFlags, FdNumber, FileHandle};
use starnix_logging::{log_trace, log_warn, with_zx_name};
use starnix_sync::{Locked, ResourceAccessorLevel};
use starnix_types::convert::IntoFidl;
use starnix_uapi::errors::{Errno, errno, errno_from_code, error};

use starnix_uapi::user_address::UserAddress;
use std::cell::Cell;
use std::mem::MaybeUninit;
use std::sync::Arc;

pub struct RemoteIoctl {
    pub ioctl_writes: Cell<Vec<fbinder::IoctlWrite>>,
    pub vmo: zx::Vmo,
}

/// Abstraction for accessing resources of a given process, whether it is a current process or a
/// remote one.
pub trait ResourceAccessor: std::fmt::Debug {
    // File related methods.
    fn close_files(&self, fds: Vec<FdNumber>) -> Result<(), Errno>;
    fn get_files_with_flags(
        &self,
        locked: &mut Locked<ResourceAccessorLevel>,
        current_task: &CurrentTask,
        fds: Vec<FdNumber>,
    ) -> Result<Vec<(FileHandle, FdFlags)>, Errno>;
    fn add_files_with_flags(
        &self,
        locked: &mut Locked<ResourceAccessorLevel>,
        current_task: &CurrentTask,
        files: Vec<(FileHandle, FdFlags)>,
        add_action: &mut dyn FnMut(FdNumber),
    ) -> Result<Vec<FdNumber>, Errno>;

    // Convenience method to allow passing a MemoryAccessor as a parameter.
    fn as_memory_accessor(&self) -> Option<&dyn MemoryAccessor>;
}

/// Return the `ResourceAccessor` to use to access the resources of `task`. If
/// `remote_resource_accessor` is not empty, the task is remote, and it should be used instead.
pub fn get_resource_accessor<'a>(
    task: &'a dyn ResourceAccessor,
    remote_resource_accessor: &'a Option<Arc<RemoteResourceAccessor>>,
) -> &'a dyn ResourceAccessor {
    if let Some(resource_accessor) = remote_resource_accessor {
        resource_accessor.as_ref()
    } else {
        task
    }
}

pub struct RemoteResourceAccessor {
    pub process: zx::Process,
    pub process_accessor: fbinder::ProcessAccessorSynchronousProxy,
    pub remote_creds: FullCredentials,
}

impl RemoteResourceAccessor {
    fn run_file_request(
        &self,
        request: fbinder::FileRequest,
    ) -> Result<fbinder::FileResponse, Errno> {
        let result = self
            .process_accessor
            .file_request(request, zx::MonotonicInstant::INFINITE)
            .map_err(|_| errno!(ENOENT))?;
        result.map_err(|e| errno_from_code!(e.into_primitive() as i16))
    }

    fn with_remote_creds<F, T>(&self, current_task: &CurrentTask, f: F) -> Result<T, Errno>
    where
        F: FnOnce() -> Result<T, Errno>,
    {
        current_task.override_creds(self.remote_creds.clone(), f)
    }
}

impl std::fmt::Debug for RemoteResourceAccessor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteResourceAccessor").finish()
    }
}

pub struct RemoteMemoryAccessor<'b> {
    pub remote_resource_accessor: Arc<RemoteResourceAccessor>,
    pub remote_ioctl: &'b RemoteIoctl,
}

impl<'b> RemoteMemoryAccessor<'b> {
    fn map_fidl_posix_errno(e: fposix::Errno) -> Errno {
        errno_from_code!(e.into_primitive() as i16)
    }
}

// The maximal size of buffers that zircon supports for process_{read|write}_memory.
const MAX_PROCESS_READ_WRITE_MEMORY_BUFFER_SIZE: usize = 64 * 1024 * 1024;

impl<'b> MemoryAccessor for RemoteMemoryAccessor<'b> {
    fn read_memory<'a>(
        &self,
        addr: UserAddress,
        mut unread_bytes: &'a mut [MaybeUninit<u8>],
    ) -> Result<&'a mut [u8], Errno> {
        let mut addr = addr.ptr();
        let unread_bytes_ptr = unread_bytes.as_mut_ptr();
        let unread_bytes_len = unread_bytes.len();
        while !unread_bytes.is_empty() {
            let len = std::cmp::min(unread_bytes.len(), MAX_PROCESS_READ_WRITE_MEMORY_BUFFER_SIZE);
            let (read_bytes, _unread_bytes) = self
                .remote_resource_accessor
                .process
                .read_memory_uninit(addr, &mut unread_bytes[..len])
                .map_err(|_| errno!(EINVAL))?;
            let bytes_count = read_bytes.len();
            // bytes_count can be less than len when:
            // - there is a fault
            // - the reading is done across 2 mappings
            // To detect this, this only fails when nothing could be read. Otherwise, a new
            // read will be issued with the remaining of the buffer, and a fault will be
            // detected when no byte can be read.
            if bytes_count == 0 {
                return error!(EFAULT);
            }
            addr += bytes_count;
            // Note that we can't use `_unread_bytes` because it does not extend
            // to the end of `unread_bytes`. We pass `unread_bytes[..len]` to
            // `read_memory_uninit` so the returned unread bytes would be
            // `unread_bytes[bytes_count..len]` vs. `unread_bytes[bytes_count..]`
            // which is what we want.
            unread_bytes = &mut unread_bytes[bytes_count..];
        }

        debug_assert_eq!(unread_bytes.len(), 0);
        // SAFETY: [MaybeUninit<T>] and [T] have the same layout. All bytes have been
        // initialized.
        let bytes = unsafe {
            std::slice::from_raw_parts_mut(unread_bytes_ptr as *mut u8, unread_bytes_len)
        };
        Ok(bytes)
    }

    fn read_memory_partial_until_null_byte<'a>(
        &self,
        _addr: UserAddress,
        _bytes: &'a mut [MaybeUninit<u8>],
    ) -> Result<&'a mut [u8], Errno> {
        error!(ENOTSUP)
    }

    fn read_memory_partial<'a>(
        &self,
        _addr: UserAddress,
        _bytes: &'a mut [MaybeUninit<u8>],
    ) -> Result<&'a mut [u8], Errno> {
        error!(ENOTSUP)
    }

    fn write_memory(&self, addr: UserAddress, bytes: &[u8]) -> Result<usize, Errno> {
        // No bytes to write.
        if bytes.is_empty() {
            return Ok(0);
        }
        // Writes are returned through ioctl, if there is space.
        let mut ioctl_writes = self.remote_ioctl.ioctl_writes.take();
        if ioctl_writes.len() < fbinder::MAX_IOCTL_WRITE_COUNT as usize {
            let last = ioctl_writes.last().unwrap_or(&fbinder::IoctlWrite {
                address: 0,
                offset: 0,
                length: 0,
            });
            let offset = last.offset + last.length;
            ioctl_writes.push(fbinder::IoctlWrite {
                address: addr.ptr() as u64,
                offset,
                length: bytes.len() as u64,
            });
            self.remote_ioctl.ioctl_writes.set(ioctl_writes);
            self.remote_ioctl.vmo.write(bytes, offset).map_err(|_| errno!(ENOENT))?;
            return Ok(bytes.len());
        }
        self.remote_ioctl.ioctl_writes.set(ioctl_writes);
        // Otherwise use ProcessAccessor to write to the process.
        if bytes.len() <= fbinder::MAX_WRITE_BYTES as usize {
            self.remote_resource_accessor.process_accessor.write_bytes(
                addr.ptr() as u64,
                bytes,
                zx::MonotonicInstant::INFINITE,
            )
        } else {
            let vmo = with_zx_name(
                zx::Vmo::create(bytes.len() as u64).map_err(|_| errno!(EINVAL))?,
                b"starnix:device_binder",
            );
            vmo.write(bytes, 0).map_err(|_| errno!(EFAULT))?;
            vmo.set_content_size(&(bytes.len() as u64)).map_err(|_| errno!(EINVAL))?;
            self.remote_resource_accessor.process_accessor.write_memory(
                addr.ptr() as u64,
                vmo,
                zx::MonotonicInstant::INFINITE,
            )
        }
        .map_err(|_| errno!(ENOENT))?
        .map_err(Self::map_fidl_posix_errno)?;
        Ok(bytes.len())
    }

    fn write_memory_partial(&self, _addr: UserAddress, _bytes: &[u8]) -> Result<usize, Errno> {
        error!(ENOTSUP)
    }

    fn zero(&self, _addr: UserAddress, _length: usize) -> Result<usize, Errno> {
        error!(ENOTSUP)
    }
}

impl ResourceAccessor for RemoteResourceAccessor {
    fn close_files(&self, fds: Vec<FdNumber>) -> Result<(), Errno> {
        for chunk in fds.chunks(fbinder::MAX_REQUEST_COUNT as usize) {
            self.run_file_request(fbinder::FileRequest {
                close_requests: Some(chunk.into_iter().map(|fd| fd.raw()).collect()),
                ..Default::default()
            })?;
        }
        Ok(())
    }

    fn get_files_with_flags(
        &self,
        locked: &mut Locked<ResourceAccessorLevel>,
        current_task: &CurrentTask,
        fds: Vec<FdNumber>,
    ) -> Result<Vec<(FileHandle, FdFlags)>, Errno> {
        let num_fds = fds.len();
        let mut files = Vec::with_capacity(num_fds);

        self.with_remote_creds(current_task, || {
            for chunk in fds.chunks(fbinder::MAX_REQUEST_COUNT as usize) {
                let response = self.run_file_request(fbinder::FileRequest {
                    get_requests: Some(chunk.into_iter().map(|fd| fd.raw()).collect()),
                    ..Default::default()
                })?;
                for fbinder::FileHandle { file, flags, .. } in
                    response.get_responses.into_iter().flatten()
                {
                    let Some(flags) = flags else {
                        log_warn!("Incorrect response to file request. Missing flags.");
                        return error!(ENOENT);
                    };
                    let file = if let Some(file) = file {
                        new_remote_file(locked, current_task, file, flags.into_fidl())?
                    } else {
                        new_null_file(locked, current_task, flags.into_fidl())
                    };
                    files.push((file, FdFlags::empty()));
                }
            }

            if files.len() != num_fds { error!(ENOENT) } else { Ok(files) }
        })
    }

    fn add_files_with_flags(
        &self,
        _locked: &mut Locked<ResourceAccessorLevel>,
        current_task: &CurrentTask,
        files: Vec<(FileHandle, FdFlags)>,
        add_action: &mut dyn FnMut(FdNumber),
    ) -> Result<Vec<FdNumber>, Errno> {
        let num_files = files.len();
        let mut fds = Vec::with_capacity(num_files);

        self.with_remote_creds(current_task, || {
            for chunk in files.chunks(fbinder::MAX_REQUEST_COUNT as usize) {
                let mut handles = Vec::with_capacity(chunk.len());
                for (file, _) in chunk.into_iter() {
                    handles.push(fbinder::FileHandle {
                        file: file.to_handle(current_task)?,
                        // NOTE: We do not pass flags when adding files to a Fuchsia process as:
                        //   1. The flags are already set on the underlying fuchsia.io file.
                        //   2. There is only one flag (append) that you can set after the fact.
                        ..fbinder::FileHandle::default()
                    });
                }
                let response = self.run_file_request(fbinder::FileRequest {
                    add_requests: Some(handles),
                    ..Default::default()
                })?;
                for fd in
                    response.add_responses.into_iter().flatten().map(|fd| FdNumber::from_raw(fd))
                {
                    add_action(fd);
                    fds.push(fd);
                }
            }

            if fds.len() != num_files { error!(ENOENT) } else { Ok(fds) }
        })
    }

    fn as_memory_accessor(&self) -> Option<&dyn MemoryAccessor> {
        None
    }
}

/// Implementation of `ResourceAccessor` for a local client represented as a `CurrentTask`.
impl ResourceAccessor for CurrentTask {
    fn close_files(&self, fds: Vec<FdNumber>) -> Result<(), Errno> {
        for fd in fds {
            log_trace!("Closing fd {:?}", fd);
            self.files.close(fd)?;
        }
        Ok(())
    }

    fn get_files_with_flags(
        &self,
        _locked: &mut Locked<ResourceAccessorLevel>,
        _current_task: &CurrentTask,
        fds: Vec<FdNumber>,
    ) -> Result<Vec<(FileHandle, FdFlags)>, Errno> {
        let mut files = Vec::with_capacity(fds.len());
        for fd in fds {
            log_trace!("Getting file {:?} with flags", fd);
            // TODO: Should we allow O_PATH here?
            files.push(self.files.get_allowing_opath_with_flags(fd)?);
        }
        Ok(files)
    }

    fn add_files_with_flags(
        &self,
        locked: &mut Locked<ResourceAccessorLevel>,
        current_task: &CurrentTask,
        files: Vec<(FileHandle, FdFlags)>,
        add_action: &mut dyn FnMut(FdNumber),
    ) -> Result<Vec<FdNumber>, Errno> {
        let mut fds = Vec::with_capacity(files.len());
        for (file, flags) in files {
            log_trace!("Adding file {:?} with flags {:?}", file, flags);
            let fd = self.files.add(locked, current_task, file, flags)?;
            add_action(fd);
            fds.push(fd);
        }
        Ok(fds)
    }

    fn as_memory_accessor(&self) -> Option<&dyn MemoryAccessor> {
        Some(self)
    }
}

/// Implementation of `ResourceAccessor` for a local client represented as a `Task`.
impl ResourceAccessor for Task {
    fn close_files(&self, fds: Vec<FdNumber>) -> Result<(), Errno> {
        for fd in fds {
            log_trace!("Closing fd {:?}", fd);
            self.files.close(fd)?;
        }
        Ok(())
    }

    fn get_files_with_flags(
        &self,
        _locked: &mut Locked<ResourceAccessorLevel>,
        _current_task: &CurrentTask,
        fds: Vec<FdNumber>,
    ) -> Result<Vec<(FileHandle, FdFlags)>, Errno> {
        let mut files = Vec::with_capacity(fds.len());
        for fd in fds {
            log_trace!("Getting file {:?} with flags", fd);
            // TODO: Should we allow O_PATH here?
            files.push(self.files.get_allowing_opath_with_flags(fd)?);
        }
        Ok(files)
    }

    fn add_files_with_flags(
        &self,
        locked: &mut Locked<ResourceAccessorLevel>,
        current_task: &CurrentTask,
        files: Vec<(FileHandle, FdFlags)>,
        add_action: &mut dyn FnMut(FdNumber),
    ) -> Result<Vec<FdNumber>, Errno> {
        let mut fds = Vec::with_capacity(files.len());
        for (file, flags) in files {
            log_trace!("Adding file {:?} with flags {:?}", file, flags);
            let fd = self.files.add(locked, current_task, file, flags)?;
            add_action(fd);
            fds.push(fd);
        }
        Ok(fds)
    }

    fn as_memory_accessor(&self) -> Option<&dyn MemoryAccessor> {
        Some(self)
    }
}
