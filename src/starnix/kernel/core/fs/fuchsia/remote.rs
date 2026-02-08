// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fs::fuchsia::RemoteUnixDomainSocket;
use crate::fs::fuchsia::remote_volume::RemoteVolume;
use crate::fs::fuchsia::sync_file::{SyncFence, SyncFile, SyncPoint, Timeline};
use crate::mm::memory::MemoryObject;
use crate::mm::{ProtectionFlags, VMEX_RESOURCE};
use crate::security;
use crate::task::{CurrentTask, FullCredentials, Kernel};
use crate::vfs::buffers::{InputBuffer, OutputBuffer, with_iovec_segments};
use crate::vfs::fsverity::FsVerityState;
use crate::vfs::socket::{Socket, SocketFile, ZxioBackedSocket};
use crate::vfs::{
    Anon, AppendLockGuard, CacheMode, DEFAULT_BYTES_PER_BLOCK, DirectoryEntryType, DirentSink,
    FallocMode, FileHandle, FileObject, FileOps, FileSystem, FileSystemHandle, FileSystemOps,
    FileSystemOptions, FsNode, FsNodeHandle, FsNodeInfo, FsNodeOps, FsStr, FsString, SeekTarget,
    SymlinkTarget, XattrOp, XattrStorage, default_ioctl, default_seek, fileops_impl_directory,
    fileops_impl_nonseekable, fileops_impl_noop_sync, fileops_impl_seekable, fs_node_impl_not_dir,
    fs_node_impl_symlink, fs_node_impl_xattr_delegate,
};
use bstr::ByteSlice;
use fidl::endpoints::{DiscoverableProtocolMarker as _, SynchronousProxy as _};
use fuchsia_runtime::UtcInstant;
use linux_uapi::SYNC_IOC_MAGIC;
use once_cell::sync::OnceCell;
use starnix_crypt::EncryptionKeyId;
use starnix_logging::{CATEGORY_STARNIX_MM, impossible_error, log_warn, trace_duration};
use starnix_sync::{
    FileOpsCore, LockEqualOrBefore, Locked, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard,
    Unlocked,
};
use starnix_syscalls::{SyscallArg, SyscallResult};
use starnix_types::vfs::default_statfs;
use starnix_uapi::auth::FsCred;
use starnix_uapi::device_type::DeviceType;
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::FileMode;
use starnix_uapi::mount_flags::MountFlags;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::{
    __kernel_fsid_t, errno, error, from_status_like_fdio, fsverity_descriptor, ino_t, mode, off_t,
    statfs,
};
use std::ops::Range;
use std::sync::Arc;
use syncio::zxio::{
    ZXIO_NODE_PROTOCOL_DIRECTORY, ZXIO_NODE_PROTOCOL_FILE, ZXIO_NODE_PROTOCOL_SYMLINK,
    ZXIO_OBJECT_TYPE_DATAGRAM_SOCKET, ZXIO_OBJECT_TYPE_NONE, ZXIO_OBJECT_TYPE_PACKET_SOCKET,
    ZXIO_OBJECT_TYPE_RAW_SOCKET, ZXIO_OBJECT_TYPE_STREAM_SOCKET,
    ZXIO_OBJECT_TYPE_SYNCHRONOUS_DATAGRAM_SOCKET, zxio_node_attr,
};
use syncio::{
    AllocateMode, XattrSetMode, Zxio, zxio_fsverity_descriptor_t, zxio_node_attr_has_t,
    zxio_node_attributes_t,
};
use zerocopy::FromBytes;
use zx::{Counter, HandleBased as _};
use {
    fidl_fuchsia_io as fio, fidl_fuchsia_starnix_binder as fbinder,
    fidl_fuchsia_unknown as funknown,
};

fn zxio_attr_from_fidl_attributes(in_attr: &fio::NodeAttributes2) -> zxio_node_attributes_t {
    zxio_attr_from_fidl(&in_attr.mutable_attributes, &in_attr.immutable_attributes)
}

/// Converts FIDL attributes to `zxio_node_attributes_t`.
///
/// NOTE: This does not work for _all_ attributes e.g. fsverity options and root hash.
fn zxio_attr_from_fidl(
    mutable: &fio::MutableNodeAttributes,
    immutable: &fio::ImmutableNodeAttributes,
) -> zxio_node_attributes_t {
    let mut out_attr = zxio_node_attributes_t::default();
    if let Some(protocols) = immutable.protocols {
        out_attr.protocols = protocols.bits();
        out_attr.has.protocols = true;
    }
    if let Some(abilities) = immutable.abilities {
        out_attr.abilities = abilities.bits();
        out_attr.has.abilities = true;
    }
    if let Some(id) = immutable.id {
        out_attr.id = id;
        out_attr.has.id = true;
    }
    if let Some(content_size) = immutable.content_size {
        out_attr.content_size = content_size;
        out_attr.has.content_size = true;
    }
    if let Some(storage_size) = immutable.storage_size {
        out_attr.storage_size = storage_size;
        out_attr.has.storage_size = true;
    }
    if let Some(link_count) = immutable.link_count {
        out_attr.link_count = link_count;
        out_attr.has.link_count = true;
    }
    if let Some(creation_time) = mutable.creation_time {
        out_attr.creation_time = creation_time;
        out_attr.has.creation_time = true;
    }
    if let Some(modification_time) = mutable.modification_time {
        out_attr.modification_time = modification_time;
        out_attr.has.modification_time = true;
    }
    if let Some(access_time) = mutable.access_time {
        out_attr.access_time = access_time;
        out_attr.has.access_time = true;
    }
    if let Some(mode) = mutable.mode {
        out_attr.mode = mode;
        out_attr.has.mode = true;
    }
    if let Some(uid) = mutable.uid {
        out_attr.uid = uid;
        out_attr.has.uid = true;
    }
    if let Some(gid) = mutable.gid {
        out_attr.gid = gid;
        out_attr.has.gid = true;
    }
    if let Some(rdev) = mutable.rdev {
        out_attr.rdev = rdev;
        out_attr.has.rdev = true;
    }
    if let Some(change_time) = immutable.change_time {
        out_attr.change_time = change_time;
        out_attr.has.change_time = true;
    }
    if let Some(casefold) = mutable.casefold {
        out_attr.casefold = casefold;
        out_attr.has.casefold = true;
    }
    if let Some(verity_enabled) = immutable.verity_enabled {
        out_attr.fsverity_enabled = verity_enabled;
        out_attr.has.fsverity_enabled = true;
    }
    if let Some(wrapping_key_id) = mutable.wrapping_key_id {
        out_attr.wrapping_key_id = wrapping_key_id;
        out_attr.has.wrapping_key_id = true;
    }
    out_attr
}

fn create_with_on_representation(
    proxy: fio::NodeSynchronousProxy,
    assume_special: bool,
) -> Result<(Box<dyn FsNodeOps>, zxio_node_attributes_t, Option<fio::SelinuxContext>), zx::Status> {
    match proxy.wait_for_event(zx::MonotonicInstant::INFINITE) {
        Ok(fio::NodeEvent::OnRepresentation { mut payload }) => {
            let (ops, attrs): (Box<dyn FsNodeOps>, _) = match &mut payload {
                fio::Representation::Node(info) => {
                    (Box::new(RemoteNode::new(RemoteIo::new(proxy))), &mut info.attributes)
                }
                fio::Representation::Directory(info) => {
                    (Box::new(RemoteNode::new(RemoteIo::new(proxy))), &mut info.attributes)
                }
                fio::Representation::File(info) => {
                    if assume_special || is_special(info) {
                        (
                            Box::new(RemoteSpecialNode { io: RemoteIo::new(proxy) }),
                            &mut info.attributes,
                        )
                    } else {
                        (
                            Box::new(RemoteNode::new(RemoteIo {
                                proxy,
                                stream: info
                                    .stream
                                    .take()
                                    .unwrap_or_else(|| zx::NullableHandle::invalid().into()),
                            })),
                            &mut info.attributes,
                        )
                    }
                }
                fio::Representation::Symlink(info) => {
                    let Some(target) = info.target.take() else {
                        return Err(zx::Status::IO);
                    };
                    (
                        Box::new(RemoteSymlink::new(RemoteIo::new(proxy), target)),
                        &mut info.attributes,
                    )
                }
                _ => return Err(zx::Status::NOT_SUPPORTED),
            };
            let (attrs, context) = attrs
                .as_mut()
                .map(|a| {
                    (zxio_attr_from_fidl_attributes(a), a.mutable_attributes.selinux_context.take())
                })
                .unwrap_or_default();
            Ok((ops, attrs, context))
        }
        Ok(_) => Err(zx::Status::IO),
        Err(e) => Err(zx::Status::from_raw(match e {
            fidl::Error::ClientChannelClosed { status, .. } => status.into_raw(),
            _ => zx::sys::ZX_ERR_IO,
        })),
    }
}

fn is_special(file_info: &fio::FileInfo) -> bool {
    matches!(
        file_info,
        fio::FileInfo {
            attributes:
                Some(fio::NodeAttributes2 {
                    mutable_attributes: fio::MutableNodeAttributes { mode: Some(mode), .. },
                    ..
                }),
            ..
        } if {
            let mode = FileMode::from_bits(*mode);
            mode.is_chr() || mode.is_blk() || mode.is_fifo() || mode.is_sock()
        }
    )
}

#[derive(Debug)]
struct RemoteIo {
    proxy: fio::NodeSynchronousProxy,
    // NOTE: This can be invalid if the remote end did not return a stream in which case
    // file I/O will use FIDL (slow).
    stream: zx::Stream,
}

impl RemoteIo {
    fn new(proxy: fio::NodeSynchronousProxy) -> Self {
        Self { proxy, stream: zx::NullableHandle::invalid().into() }
    }

    fn fetch_remote_memory(&self, prot: ProtectionFlags) -> Result<Arc<MemoryObject>, Errno> {
        let without_exec = self
            .vmo_get(prot.to_vmar_flags() - zx::VmarFlags::PERM_EXECUTE)
            .map_err(|status| from_status_like_fdio!(status))?;
        let all_flags = if prot.contains(ProtectionFlags::EXEC) {
            without_exec.replace_as_executable(&VMEX_RESOURCE).map_err(impossible_error)?
        } else {
            without_exec
        };
        Ok(Arc::new(MemoryObject::from(all_flags)))
    }

    fn cast_proxy<T: From<zx::Channel> + Into<zx::NullableHandle>>(&self) -> zx::Unowned<'_, T> {
        zx::Unowned::new(self.proxy.as_channel())
    }

    fn attr_get(
        &self,
        query: fio::NodeAttributesQuery,
    ) -> Result<(fio::MutableNodeAttributes, fio::ImmutableNodeAttributes), zx::Status> {
        self.proxy
            .get_attributes(query, zx::MonotonicInstant::INFINITE)
            .map_err(|_| zx::Status::IO)?
            .map_err(zx::Status::from_raw)
    }

    fn attr_get_zxio(
        &self,
        query: fio::NodeAttributesQuery,
    ) -> Result<zxio_node_attributes_t, zx::Status> {
        self.attr_get(query).map(|(m, i)| zxio_attr_from_fidl(&m, &i))
    }

    fn attr_set(&self, attributes: fio::MutableNodeAttributes) -> Result<(), zx::Status> {
        self.proxy
            .update_attributes(&attributes, zx::MonotonicInstant::INFINITE)
            .map_err(|_| zx::Status::IO)?
            .map_err(zx::Status::from_raw)?;
        Ok(())
    }

    fn open(
        &self,
        path: &str,
        flags: fio::Flags,
        create_attributes: Option<fio::MutableNodeAttributes>,
        query: fio::NodeAttributesQuery,
        assume_special: bool,
    ) -> Result<(Box<dyn FsNodeOps>, zxio_node_attributes_t, Option<fio::SelinuxContext>), zx::Status>
    {
        let (client_end, server_end) = zx::Channel::create();
        let dir_proxy = self.cast_proxy::<fio::DirectorySynchronousProxy>();
        dir_proxy
            .open(
                path,
                flags | fio::Flags::FLAG_SEND_REPRESENTATION,
                &fio::Options {
                    attributes: (!query.is_empty()).then_some(query),
                    create_attributes,
                    ..Default::default()
                },
                server_end,
            )
            .map_err(|_| zx::Status::IO)?;
        create_with_on_representation(client_end.into(), assume_special)
    }

    fn read_at(&self, offset: u64, buffer: &mut dyn OutputBuffer) -> Result<usize, Errno> {
        if self.stream.is_invalid_handle() {
            let total = buffer.available();
            let file_proxy = self.cast_proxy::<fio::FileSynchronousProxy>();
            let mut total_read = 0;
            while total_read < total {
                let chunk_size = std::cmp::min((total - total_read) as u64, fio::MAX_TRANSFER_SIZE);
                let result = file_proxy
                    .read_at(chunk_size, offset + total_read as u64, zx::MonotonicInstant::INFINITE)
                    .map_err(|_| errno!(EIO))?
                    .map_err(|s| from_status_like_fdio!(zx::Status::from_raw(s)))?;
                if result.is_empty() {
                    break;
                }
                let len = buffer.write(&result)?;
                total_read += len;
                if (len as u64) < chunk_size {
                    break;
                }
            }
            Ok(total_read)
        } else {
            let read_bytes = with_iovec_segments(buffer, |iovecs| {
                // SAFETY: The iovecs are valid for writing because they come from OutputBuffer.
                unsafe {
                    self.stream
                        .readv_at(zx::StreamReadOptions::empty(), offset as u64, iovecs)
                        .map_err(map_stream_error)
                }
            });

            match read_bytes {
                Some(actual) => {
                    let actual = actual?;
                    // SAFETY: we successfully read `actual` bytes directly to the user's buffer
                    // segments.
                    unsafe { buffer.advance(actual) }?;
                    Ok(actual)
                }
                None => {
                    // Perform the (slower) operation by using an intermediate buffer.
                    let total = buffer.available();
                    let mut bytes = vec![0u8; total];
                    // Use readv_at with a single iovec for the fallback
                    let mut iovec = zx::sys::zx_iovec_t {
                        buffer: bytes.as_mut_ptr() as *mut _,
                        capacity: bytes.len(),
                    };
                    // SAFETY: iovec points to valid mutable buffer.
                    let actual = unsafe {
                        self.stream.readv_at(
                            zx::StreamReadOptions::empty(),
                            offset as u64,
                            std::slice::from_mut(&mut iovec),
                        )
                    }
                    .map_err(map_stream_error)?;
                    buffer.write_all(&bytes[0..actual])
                }
            }
        }
    }

    fn write_at(&self, offset: u64, buffer: &mut dyn InputBuffer) -> Result<usize, Errno> {
        if self.stream.is_invalid_handle() {
            let file_proxy = self.cast_proxy::<fio::FileSynchronousProxy>();
            let write_bytes =
                with_iovec_segments(buffer, |iovecs: &mut [zx::sys::zx_iovec_t]| {
                    let mut total_written = 0;
                    for iovec in iovecs {
                        if iovec.capacity == 0 {
                            continue;
                        }
                        // SAFETY: iovec.buffer is assumed valid and we read from it.
                        let buf =
                            unsafe { std::slice::from_raw_parts(iovec.buffer, iovec.capacity) };
                        for chunk in buf.chunks(fio::MAX_TRANSFER_SIZE as usize) {
                            let actual = file_proxy
                                .write_at(
                                    chunk,
                                    offset + total_written as u64,
                                    zx::MonotonicInstant::INFINITE,
                                )
                                .map_err(|_| errno!(EIO))?
                                .map_err(|status| {
                                    from_status_like_fdio!(zx::Status::from_raw(status))
                                })? as usize;
                            total_written += actual;
                            if actual < chunk.len() {
                                return Ok(total_written);
                            }
                        }
                    }
                    Ok(total_written)
                });

            match write_bytes {
                Some(actual) => {
                    let actual = actual?;
                    buffer.advance(actual)?;
                    Ok(actual)
                }
                None => {
                    // Perform the (slower) operation by using an intermediate buffer.
                    let total = buffer.available();
                    let mut total_written = 0;
                    let tx_buf_size = std::cmp::min(fio::MAX_TRANSFER_SIZE as usize, total);
                    let mut tx_buf = Vec::with_capacity(tx_buf_size);
                    while total_written < total {
                        tx_buf.clear();
                        let len = buffer.peek(&mut tx_buf.spare_capacity_mut()[..tx_buf_size])?;
                        // SAFETY: `peek` read `len` bytes into `tx_buf`.
                        unsafe {
                            tx_buf.set_len(len);
                        }
                        let actual = file_proxy
                            .write_at(
                                &tx_buf,
                                offset + total_written as u64,
                                zx::MonotonicInstant::INFINITE,
                            )
                            .map_err(|_| errno!(EIO))?
                            .map_err(|status| {
                                from_status_like_fdio!(zx::Status::from_raw(status))
                            })?;
                        total_written += actual as usize;
                        buffer.advance(actual as usize)?;
                        if actual < fio::MAX_TRANSFER_SIZE {
                            break;
                        }
                    }
                    Ok(total_written)
                }
            }
        } else {
            let write_bytes = with_iovec_segments(buffer, |iovecs| {
                self.stream
                    .writev_at(zx::StreamWriteOptions::empty(), offset as u64, &iovecs)
                    .map_err(map_stream_error)
            });

            match write_bytes {
                Some(actual) => {
                    let actual = actual?;
                    buffer.advance(actual)?;
                    Ok(actual)
                }
                None => {
                    // Perform the (slower) operation by using an intermediate buffer.
                    let bytes = buffer.peek_all()?;
                    // Use writev_at with a single iovec for the fallback
                    let iovec =
                        zx::sys::zx_iovec_t { buffer: bytes.as_ptr(), capacity: bytes.len() };
                    let actual = self
                        .stream
                        .writev_at(zx::StreamWriteOptions::empty(), offset as u64, &[iovec])
                        .map_err(map_stream_error)?;
                    buffer.advance(actual)?;
                    Ok(actual)
                }
            }
        }
    }

    fn truncate(&self, length: u64) -> Result<(), zx::Status> {
        self.cast_proxy::<fio::FileSynchronousProxy>()
            .resize(length, zx::MonotonicInstant::INFINITE)
            .map_err(|_| zx::Status::IO)?
            .map_err(zx::Status::from_raw)?;
        Ok(())
    }

    fn vmo_get(&self, flags: zx::VmarFlags) -> Result<zx::Vmo, zx::Status> {
        let mut fio_flags = fio::VmoFlags::empty();
        if flags.contains(zx::VmarFlags::PERM_READ) {
            fio_flags |= fio::VmoFlags::READ;
        }
        if flags.contains(zx::VmarFlags::PERM_WRITE) {
            fio_flags |= fio::VmoFlags::WRITE;
        }
        if flags.contains(zx::VmarFlags::PERM_EXECUTE) {
            fio_flags |= fio::VmoFlags::EXECUTE;
        }
        let file_proxy = self.cast_proxy::<fio::FileSynchronousProxy>();
        let vmo = file_proxy
            .get_backing_memory(fio_flags, zx::MonotonicInstant::INFINITE)
            .map_err(|_| zx::Status::IO)?
            .map_err(zx::Status::from_raw)?;
        Ok(vmo)
    }

    fn sync(&self) -> Result<(), Errno> {
        self.proxy
            .sync(zx::MonotonicInstant::INFINITE)
            .map_err(|_| errno!(EIO))?
            .map_err(|status| map_sync_error(zx::Status::from_raw(status)))
    }

    fn close_and_update_access_time(self) -> Result<(), zx::Status> {
        let _ = self.proxy.get_attributes(
            fio::NodeAttributesQuery::PENDING_ACCESS_TIME_UPDATE,
            zx::MonotonicInstant::INFINITE_PAST,
        );
        Ok(())
    }

    fn clone_proxy(&self) -> Result<fio::NodeSynchronousProxy, zx::Status> {
        let (client_end, server_end) = zx::Channel::create();
        self.proxy.clone(server_end.into()).map_err(|_| zx::Status::IO)?;
        Ok(client_end.into())
    }

    fn link_into(&self, target_dir: &Self, name: &str) -> Result<(), zx::Status> {
        let target_dir_proxy = target_dir.cast_proxy::<fio::DirectorySynchronousProxy>();
        let (status, token) = target_dir_proxy
            .get_token(zx::MonotonicInstant::INFINITE)
            .map_err(|_| zx::Status::IO)?;
        zx::Status::ok(status)?;
        let token = token.ok_or(zx::Status::NOT_SUPPORTED)?;

        // Linkable::LinkInto is a separate protocol.
        let linkable_proxy = self.cast_proxy::<fio::LinkableSynchronousProxy>();

        linkable_proxy
            .link_into(token.into(), name, zx::MonotonicInstant::INFINITE)
            .map_err(|_| zx::Status::IO)?
            .map_err(zx::Status::from_raw)?;
        Ok(())
    }

    fn unlink(&self, name: &str, flags: fio::UnlinkFlags) -> Result<(), zx::Status> {
        let options = fio::UnlinkOptions { flags: Some(flags), ..Default::default() };
        let dir_proxy = self.cast_proxy::<fio::DirectorySynchronousProxy>();
        dir_proxy
            .unlink(name, &options, zx::MonotonicInstant::INFINITE)
            .map_err(|_| zx::Status::IO)?
            .map_err(zx::Status::from_raw)?;
        Ok(())
    }

    fn rename(
        &self,
        old_path: &str,
        new_directory: &Self,
        new_path: &str,
    ) -> Result<(), zx::Status> {
        let new_dir_proxy = new_directory.cast_proxy::<fio::DirectorySynchronousProxy>();
        let (status, token) =
            new_dir_proxy.get_token(zx::MonotonicInstant::INFINITE).map_err(|_| zx::Status::IO)?;
        zx::Status::ok(status)?;
        let token = token.ok_or(zx::Status::NOT_SUPPORTED)?;
        let dir_proxy = self.cast_proxy::<fio::DirectorySynchronousProxy>();
        dir_proxy
            .rename(old_path, token.into(), new_path, zx::MonotonicInstant::INFINITE)
            .map_err(|_| zx::Status::IO)?
            .map_err(zx::Status::from_raw)?;
        Ok(())
    }

    fn create_symlink(&self, name: &str, target: &[u8]) -> Result<RemoteIo, zx::Status> {
        let dir_proxy = self.cast_proxy::<fio::DirectorySynchronousProxy>();
        let (client_end, server_end) = zx::Channel::create();
        dir_proxy
            .create_symlink(name, target, Some(server_end.into()), zx::MonotonicInstant::INFINITE)
            .map_err(|_| zx::Status::IO)?
            .map_err(zx::Status::from_raw)?;
        Ok(RemoteIo::new(client_end.into()))
    }

    fn enable_verity(&self, descriptor: &zxio_fsverity_descriptor_t) -> Result<(), zx::Status> {
        let file_proxy = self.cast_proxy::<fio::FileSynchronousProxy>();
        let options = fio::VerificationOptions {
            hash_algorithm: Some(match descriptor.hash_algorithm {
                1 => fio::HashAlgorithm::Sha256,
                2 => fio::HashAlgorithm::Sha512,
                _ => return Err(zx::Status::INVALID_ARGS),
            }),
            salt: Some(descriptor.salt[..descriptor.salt_size as usize].to_vec()),
            ..Default::default()
        };
        file_proxy
            .enable_verity(&options, zx::MonotonicInstant::INFINITE)
            .map_err(|_| zx::Status::IO)?
            .map_err(zx::Status::from_raw)?;
        Ok(())
    }

    fn allocate(&self, offset: u64, len: u64, mode: AllocateMode) -> Result<(), zx::Status> {
        let file_proxy = self.cast_proxy::<fio::FileSynchronousProxy>();
        let mut fio_mode = fio::AllocateMode::empty();
        if mode.contains(AllocateMode::KEEP_SIZE) {
            fio_mode |= fio::AllocateMode::KEEP_SIZE;
        }
        if mode.contains(AllocateMode::UNSHARE_RANGE) {
            fio_mode |= fio::AllocateMode::UNSHARE_RANGE;
        }
        if mode.contains(AllocateMode::PUNCH_HOLE) {
            fio_mode |= fio::AllocateMode::PUNCH_HOLE;
        }
        if mode.contains(AllocateMode::COLLAPSE_RANGE) {
            fio_mode |= fio::AllocateMode::COLLAPSE_RANGE;
        }
        if mode.contains(AllocateMode::ZERO_RANGE) {
            fio_mode |= fio::AllocateMode::ZERO_RANGE;
        }
        if mode.contains(AllocateMode::INSERT_RANGE) {
            fio_mode |= fio::AllocateMode::INSERT_RANGE;
        }
        file_proxy
            .allocate(offset, len, fio_mode, zx::MonotonicInstant::INFINITE)
            .map_err(|_| zx::Status::IO)?
            .map_err(zx::Status::from_raw)?;
        Ok(())
    }

    fn xattr_get(&self, name: &FsStr) -> Result<Vec<u8>, zx::Status> {
        let name_str = std::str::from_utf8(name.as_ref()).map_err(|_| zx::Status::INVALID_ARGS)?;
        let result = self
            .proxy
            .get_extended_attribute(name_str.as_bytes(), zx::MonotonicInstant::INFINITE)
            .map_err(|_| zx::Status::IO)?
            .map_err(zx::Status::from_raw)?;
        match result {
            fio::ExtendedAttributeValue::Bytes(bytes) => Ok(bytes),
            fio::ExtendedAttributeValue::Buffer(vmo) => {
                let size = vmo.get_content_size()?;
                vmo.read_to_vec(0, size)
            }
            _ => Err(zx::Status::NOT_SUPPORTED),
        }
    }

    fn xattr_set(
        &self,
        name: &FsStr,
        value: &FsStr,
        mode: syncio::XattrSetMode,
    ) -> Result<(), zx::Status> {
        let val = fio::ExtendedAttributeValue::Bytes(value.to_vec());
        let fidl_mode = match mode {
            syncio::XattrSetMode::Set => fio::SetExtendedAttributeMode::Set,
            syncio::XattrSetMode::Create => fio::SetExtendedAttributeMode::Create,
            syncio::XattrSetMode::Replace => fio::SetExtendedAttributeMode::Replace,
        };
        self.proxy
            .set_extended_attribute(name.as_bytes(), val, fidl_mode, zx::MonotonicInstant::INFINITE)
            .map_err(|_| zx::Status::IO)?
            .map_err(zx::Status::from_raw)?;
        Ok(())
    }

    fn xattr_remove(&self, name: &FsStr) -> Result<(), zx::Status> {
        let name_str = std::str::from_utf8(name.as_ref()).map_err(|_| zx::Status::INVALID_ARGS)?;
        self.proxy
            .remove_extended_attribute(name_str.as_bytes(), zx::MonotonicInstant::INFINITE)
            .map_err(|_| zx::Status::IO)?
            .map_err(zx::Status::from_raw)?;
        Ok(())
    }

    fn xattr_list(&self) -> Result<Vec<Vec<u8>>, zx::Status> {
        let (client_end, server_end) = zx::Channel::create();
        self.proxy.list_extended_attributes(server_end.into()).map_err(|_| zx::Status::IO)?;
        let iterator = fio::ExtendedAttributeIteratorSynchronousProxy::new(client_end);
        let mut all_attrs = vec![];
        loop {
            let (attributes, last) = iterator
                .get_next(zx::MonotonicInstant::INFINITE)
                .map_err(|_| zx::Status::IO)?
                .map_err(zx::Status::from_raw)?;
            all_attrs.extend(attributes);
            if last {
                break;
            }
        }
        Ok(all_attrs)
    }

    fn to_handle(&self) -> Result<Option<zx::NullableHandle>, Errno> {
        let (client_end, server_end) = zx::Channel::create();
        self.proxy.clone(server_end.into()).map_err(|_| errno!(EIO))?;
        Ok(Some(client_end.into_handle()))
    }
}

pub fn new_remote_fs(
    locked: &mut Locked<Unlocked>,
    current_task: &CurrentTask,
    options: FileSystemOptions,
) -> Result<FileSystemHandle, Errno> {
    let kernel = current_task.kernel();
    let requested_path = std::str::from_utf8(&options.source)
        .map_err(|_| errno!(EINVAL, "source path is not utf8"))?;
    let mut create_flags =
        fio::PERM_READABLE | fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_DIRECTORY;
    if !options.flags.contains(MountFlags::RDONLY) {
        create_flags |= fio::PERM_WRITABLE;
    }
    let (root_proxy, subdir) = kernel.open_ns_dir(requested_path, create_flags)?;

    let subdir = if subdir.is_empty() { ".".to_string() } else { subdir };
    let mut open_rights = fio::PERM_READABLE;
    if !options.flags.contains(MountFlags::RDONLY) {
        open_rights |= fio::PERM_WRITABLE;
    }
    let mut subdir_options = options;
    subdir_options.source = subdir.into();
    create_remotefs_filesystem(locked, kernel, &root_proxy, subdir_options, open_rights)
}

/// Create a filesystem to access the content of the fuchsia directory available at `fs_src` inside
/// `pkg`.
pub fn create_remotefs_filesystem<L>(
    locked: &mut Locked<L>,
    kernel: &Kernel,
    root: &fio::DirectorySynchronousProxy,
    options: FileSystemOptions,
    rights: fio::Flags,
) -> Result<FileSystemHandle, Errno>
where
    L: LockEqualOrBefore<FileOpsCore>,
{
    let root = syncio::directory_open_directory_async(
        root,
        std::str::from_utf8(&options.source)
            .map_err(|_| errno!(EINVAL, "source path is not utf8"))?,
        rights,
    )
    .map_err(|e| errno!(EIO, format!("Failed to open root: {e}")))?;
    RemoteFs::new_fs(locked, kernel, root.into_channel(), options, rights)
}

pub struct RemoteFs {
    // If true, trust the remote file system's IDs (which requires that the remote file system does
    // not span mounts).  This must be true to properly support hard links.  If this is false, the
    // same node can end up having different IDs as it leaves and reenters the node cache.
    // TODO(https://fxbug.dev/42081972): At the time of writing, package directories do not have
    // unique IDs so this *must* be false in that case.
    use_remote_ids: bool,

    root_proxy: fio::DirectorySynchronousProxy,

    // The rights used for the root node.
    root_rights: fio::Flags,
}

impl RemoteFs {
    /// Returns a reference to a RemoteFs given a reference to a FileSystem.
    ///
    /// # Panics
    ///
    /// This will panic if `fs`'s ops aren't `RemoteFs`, so this should only be called when this is
    /// known to be the case.
    fn from_fs(fs: &FileSystem) -> &RemoteFs {
        if let Some(remote_vol) = fs.downcast_ops::<RemoteVolume>() {
            remote_vol.remotefs()
        } else {
            fs.downcast_ops::<RemoteFs>().unwrap()
        }
    }
}

const REMOTE_FS_MAGIC: u32 = u32::from_be_bytes(*b"f.io");
const SYNC_IOC_FILE_INFO: u8 = 4;
const SYNC_IOC_MERGE: u8 = 3;

impl FileSystemOps for RemoteFs {
    fn statfs(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _fs: &FileSystem,
        _current_task: &CurrentTask,
    ) -> Result<statfs, Errno> {
        let (status, info) = self
            .root_proxy
            .query_filesystem(zx::MonotonicInstant::INFINITE)
            .map_err(|_| errno!(EIO))?;
        // Not all remote filesystems support `QueryFilesystem`, many return ZX_ERR_NOT_SUPPORTED.
        if status == 0 {
            if let Some(info) = info {
                let (total_blocks, free_blocks) = if info.block_size > 0 {
                    (
                        (info.total_bytes / u64::from(info.block_size))
                            .try_into()
                            .unwrap_or(i64::MAX),
                        ((info.total_bytes.saturating_sub(info.used_bytes))
                            / u64::from(info.block_size))
                        .try_into()
                        .unwrap_or(i64::MAX),
                    )
                } else {
                    (0, 0)
                };

                let fsid = __kernel_fsid_t {
                    val: [
                        (info.fs_id & 0xffffffff) as i32,
                        ((info.fs_id >> 32) & 0xffffffff) as i32,
                    ],
                };

                return Ok(statfs {
                    f_type: info.fs_type as i64,
                    f_bsize: info.block_size.into(),
                    f_blocks: total_blocks,
                    f_bfree: free_blocks,
                    f_bavail: free_blocks,
                    f_files: info.total_nodes.try_into().unwrap_or(i64::MAX),
                    f_ffree: (info.total_nodes.saturating_sub(info.used_nodes))
                        .try_into()
                        .unwrap_or(i64::MAX),
                    f_fsid: fsid,
                    f_namelen: info.max_filename_size.try_into().unwrap_or(0),
                    f_frsize: info.block_size.into(),
                    ..statfs::default()
                });
            }
        }
        Ok(default_statfs(REMOTE_FS_MAGIC))
    }

    fn name(&self) -> &'static FsStr {
        "remotefs".into()
    }

    fn uses_external_node_ids(&self) -> bool {
        self.use_remote_ids
    }

    fn rename(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _fs: &FileSystem,
        current_task: &CurrentTask,
        old_parent: &FsNodeHandle,
        old_name: &FsStr,
        new_parent: &FsNodeHandle,
        new_name: &FsStr,
        _renamed: &FsNodeHandle,
        _replaced: Option<&FsNodeHandle>,
    ) -> Result<(), Errno> {
        // Renames should fail if the src or target directory is encrypted and locked.
        old_parent.fail_if_locked(current_task)?;
        new_parent.fail_if_locked(current_task)?;

        let Some(old_parent) = old_parent.downcast_ops::<RemoteNode>() else {
            return error!(EXDEV);
        };
        let Some(new_parent) = new_parent.downcast_ops::<RemoteNode>() else {
            return error!(EXDEV);
        };
        old_parent
            .io
            .rename(get_name_str(old_name)?, &new_parent.io, get_name_str(new_name)?)
            .map_err(|status| from_status_like_fdio!(status))
    }

    fn manages_timestamps(&self) -> bool {
        true
    }
}

impl RemoteFs {
    pub(super) fn new(
        root: zx::Channel,
        root_rights: fio::Flags,
    ) -> Result<(RemoteFs, Box<dyn FsNodeOps>, u64, Option<[u8; 16]>), Errno> {
        let (client_end, server_end) = zx::Channel::create();
        let root_proxy = fio::DirectorySynchronousProxy::new(root);
        root_proxy
            .open(
                ".",
                fio::Flags::PROTOCOL_DIRECTORY
                    | fio::PERM_READABLE
                    | fio::Flags::PERM_INHERIT_WRITE
                    | fio::Flags::PERM_INHERIT_EXECUTE
                    | fio::Flags::FLAG_SEND_REPRESENTATION,
                &fio::Options {
                    attributes: Some(
                        fio::NodeAttributesQuery::ID | fio::NodeAttributesQuery::WRAPPING_KEY_ID,
                    ),
                    ..Default::default()
                },
                server_end,
            )
            .map_err(|_| errno!(EIO))?;
        // Use remote IDs if the filesystem is Fxfs which we know will give us unique IDs.  Hard
        // links need to resolve to the same underlying FsNode, so we can only support hard links if
        // the remote file system will give us unique IDs.  The IDs are also used as the key in
        // caches, so we can't use remote IDs if the remote filesystem is not guaranteed to provide
        // unique IDs, or if the remote filesystem spans multiple filesystems.
        let (status, info) =
            root_proxy.query_filesystem(zx::MonotonicInstant::INFINITE).map_err(|_| errno!(EIO))?;

        // Be tolerant of errors here; many filesystems return `ZX_ERR_NOT_SUPPORTED`.
        let use_remote_ids = status == 0
            && info
                .map(|i| i.fs_type == fidl_fuchsia_fs::VfsType::Fxfs.into_primitive())
                .unwrap_or(false);

        let (remote_node, attrs, _) = create_with_on_representation(client_end.into(), false)
            .map_err(|s| from_status_like_fdio!(s))?;
        Ok((
            RemoteFs { use_remote_ids, root_proxy, root_rights },
            remote_node,
            attrs.id,
            attrs.has.wrapping_key_id.then_some(attrs.wrapping_key_id),
        ))
    }

    pub fn new_fs<L>(
        locked: &mut Locked<L>,
        kernel: &Kernel,
        root: zx::Channel,
        mut options: FileSystemOptions,
        rights: fio::Flags,
    ) -> Result<FileSystemHandle, Errno>
    where
        L: LockEqualOrBefore<FileOpsCore>,
    {
        let (remotefs, root_node, node_id, wrapping_key_id) = RemoteFs::new(root, rights)?;

        if !rights.contains(fio::PERM_WRITABLE) {
            options.flags |= MountFlags::RDONLY;
        }
        let use_remote_ids = remotefs.use_remote_ids;
        let fs = FileSystem::new(
            locked,
            kernel,
            CacheMode::Cached(kernel.fs_cache_config()),
            remotefs,
            options,
        )?;

        let info =
            FsNodeInfo { wrapping_key_id, ..FsNodeInfo::new(mode!(IFDIR, 0o777), FsCred::root()) };

        if use_remote_ids {
            fs.create_root_with_info(node_id, root_node, info);
        } else {
            let root_ino = fs.allocate_ino();
            fs.create_root_with_info(root_ino, root_node, info);
        }

        Ok(fs)
    }

    pub(super) fn use_remote_ids(&self) -> bool {
        self.use_remote_ids
    }
}

struct RemoteNode {
    /// The underlying I/O object for this remote node.
    io: RemoteIo,
}

impl RemoteNode {
    fn new(io: RemoteIo) -> Self {
        Self { io }
    }
}

/// Creates a file handle from a zx::NullableHandle.
///
/// The handle must be a channel, socket, vmo or debuglog object.  If the handle is a channel, then
/// the channel must implement the `fuchsia.unknown/Queryable` protocol.  Not all protocols are
/// supported; files and directories are, but symlinks are not.
///
/// The resulting object will be owned by root, and will have permissions derived from the `flags`
/// used to open this object. This is not the same as the permissions set if the object was created
/// using Starnix itself. We use this mainly for interfacing with objects created outside of Starnix
/// where these flags represent the desired permissions already.
pub fn new_remote_file<L>(
    locked: &mut Locked<L>,
    current_task: &CurrentTask,
    handle: zx::NullableHandle,
    flags: OpenFlags,
) -> Result<FileHandle, Errno>
where
    L: LockEqualOrBefore<FileOpsCore>,
{
    let remote_creds = current_task.full_current_creds();
    let (attrs, ops) = remote_file_attrs_and_ops(current_task, handle.into(), remote_creds)?;
    let mut rights = fio::Flags::empty();
    if flags.can_read() {
        rights |= fio::PERM_READABLE;
    }
    if flags.can_write() {
        rights |= fio::PERM_WRITABLE;
    }
    let mode = get_mode(&attrs, rights);
    // TODO: https://fxbug.dev/407611229 - Give these nodes valid labels.
    let mut info = FsNodeInfo::new(mode, FsCred::root());
    update_info_from_attrs(&mut info, &attrs);
    Ok(Anon::new_private_file_extended(locked, current_task, ops, flags, "[fuchsia:remote]", info))
}

/// Creates a FileOps from a zx::NullableHandle.
///
/// The handle must satisfy the same requirements as `new_remote_file`.
pub fn new_remote_file_ops(
    current_task: &CurrentTask,
    handle: zx::NullableHandle,
    creds: FullCredentials,
) -> Result<Box<dyn FileOps>, Errno> {
    let (_, ops) = remote_file_attrs_and_ops(current_task, handle, creds)?;
    Ok(ops)
}

fn remote_file_attrs_and_ops(
    current_task: &CurrentTask,
    mut handle: zx::NullableHandle,
    remote_creds: FullCredentials,
) -> Result<(zxio_node_attr, Box<dyn FileOps>), Errno> {
    let handle_type =
        handle.basic_info().map_err(|status| from_status_like_fdio!(status))?.object_type;

    if handle_type == zx::ObjectType::CHANNEL {
        let channel = zx::Channel::from(handle);
        let queryable = funknown::QueryableSynchronousProxy::new(channel);
        let protocol = queryable.query(zx::MonotonicInstant::INFINITE).map_err(|_| errno!(EIO))?;
        const UNIX_DOMAIN_SOCKET_PROTOCOL: &[u8] =
            fbinder::UnixDomainSocketMarker::PROTOCOL_NAME.as_bytes();
        const FILE_PROTOCOL: &[u8] = fio::FileMarker::PROTOCOL_NAME.as_bytes();
        const DIRECTORY_PROTOCOL: &[u8] = fio::DirectoryMarker::PROTOCOL_NAME.as_bytes();
        match &protocol[..] {
            UNIX_DOMAIN_SOCKET_PROTOCOL => {
                let socket_ops =
                    RemoteUnixDomainSocket::new(queryable.into_channel(), remote_creds)?;
                let socket = Socket::new_with_ops(Box::new(socket_ops))?;
                let file_ops = SocketFile::new(socket);
                let attr = zxio_node_attr {
                    has: zxio_node_attr_has_t { mode: true, ..zxio_node_attr_has_t::default() },
                    mode: 0o777 | FileMode::IFSOCK.bits(),
                    ..zxio_node_attr::default()
                };
                return Ok((attr, file_ops));
            }
            FILE_PROTOCOL => {
                let file_proxy = fio::FileSynchronousProxy::from(queryable.into_channel());
                let info =
                    file_proxy.describe(zx::MonotonicInstant::INFINITE).map_err(|_| errno!(EIO))?;
                let io = RemoteIo {
                    proxy: file_proxy.into_channel().into(),
                    stream: info.stream.unwrap_or_else(|| zx::NullableHandle::invalid().into()),
                };
                let attr = io
                    .attr_get_zxio(MODE_ATTRIBUTES | NODE_INFO_ATTRIBUTES)
                    .map_err(|status| from_status_like_fdio!(status))?;
                return Ok((attr, Box::new(AnonymousRemoteFileObject::new(io))));
            }
            DIRECTORY_PROTOCOL => {
                let io = RemoteIo::new(queryable.into_channel().into());
                let attr = io
                    .attr_get_zxio(MODE_ATTRIBUTES | NODE_INFO_ATTRIBUTES)
                    .map_err(|status| from_status_like_fdio!(status))?;
                return Ok((
                    attr,
                    Box::new(RemoteDirectoryObject::new(io.proxy.into_channel().into())),
                ));
            }
            _ => {
                handle = queryable.into_channel().into_handle();
                // Fall through for zxio.
            }
        }
    } else if handle_type == zx::ObjectType::COUNTER {
        let attr = zxio_node_attr::default();
        let file_ops = Box::new(RemoteCounter::new(handle.into()));
        return Ok((attr, file_ops));
    }

    // Otherwise, use zxio based objects.

    // NOTE: If it's a channel, this will repeat the query, which is something we can optimize if we
    // need to.
    let zxio = Zxio::create(handle).map_err(|status| from_status_like_fdio!(status))?;
    let mut attrs = zxio
        .attr_get(zxio_node_attr_has_t {
            protocols: true,
            content_size: true,
            storage_size: true,
            link_count: true,
            object_type: true,
            ..Default::default()
        })
        .map_err(|status| from_status_like_fdio!(status))?;
    let ops: Box<dyn FileOps> = match (handle_type, attrs.object_type) {
        (zx::ObjectType::VMO, _) | (zx::ObjectType::DEBUGLOG, _) | (_, ZXIO_OBJECT_TYPE_NONE) => {
            Box::new(RemoteZxioFileObject::new(zxio))
        }
        (zx::ObjectType::SOCKET, _)
        | (_, ZXIO_OBJECT_TYPE_SYNCHRONOUS_DATAGRAM_SOCKET)
        | (_, ZXIO_OBJECT_TYPE_DATAGRAM_SOCKET)
        | (_, ZXIO_OBJECT_TYPE_STREAM_SOCKET)
        | (_, ZXIO_OBJECT_TYPE_RAW_SOCKET)
        | (_, ZXIO_OBJECT_TYPE_PACKET_SOCKET) => {
            let socket_ops = ZxioBackedSocket::new_with_zxio(current_task, zxio);
            let socket = Socket::new_with_ops(Box::new(socket_ops))?;
            attrs.has.mode = true;
            attrs.mode = FileMode::IFSOCK.bits();
            SocketFile::new(socket)
        }
        _ => return error!(ENOTSUP),
    };
    Ok((attrs, ops))
}

pub fn create_fuchsia_pipe<L>(
    locked: &mut Locked<L>,
    current_task: &CurrentTask,
    socket: zx::Socket,
    flags: OpenFlags,
) -> Result<FileHandle, Errno>
where
    L: LockEqualOrBefore<FileOpsCore>,
{
    new_remote_file(locked, current_task, socket.into(), flags)
}

fn fetch_and_refresh_info_impl<'a>(
    io: &RemoteIo,
    info: &'a RwLock<FsNodeInfo>,
) -> Result<RwLockReadGuard<'a, FsNodeInfo>, Errno> {
    let mut query = NODE_INFO_ATTRIBUTES;
    if info.read().pending_time_access_update {
        query |= fio::NodeAttributesQuery::PENDING_ACCESS_TIME_UPDATE;
    }
    let attrs = io.attr_get_zxio(query).map_err(|status| from_status_like_fdio!(status))?;
    let mut info = info.write();
    update_info_from_attrs(&mut info, &attrs);
    info.pending_time_access_update = false;
    Ok(RwLockWriteGuard::downgrade(info))
}

// This only needs to include attributes that can be out of date.  There are other attributes that
// we read when we first look up the node (see `lookup`).
const NODE_INFO_ATTRIBUTES: fio::NodeAttributesQuery = fio::NodeAttributesQuery::CONTENT_SIZE
    .union(fio::NodeAttributesQuery::STORAGE_SIZE)
    .union(fio::NodeAttributesQuery::LINK_COUNT)
    .union(fio::NodeAttributesQuery::MODIFICATION_TIME)
    .union(fio::NodeAttributesQuery::CHANGE_TIME)
    .union(fio::NodeAttributesQuery::ACCESS_TIME);

/// Updates info from attrs if they are set.
///
// Keep in sync with `NODE_INFO_ATTRIBUTES`.
pub(super) fn update_info_from_attrs(info: &mut FsNodeInfo, attrs: &zxio_node_attributes_t) {
    // TODO - store these in FsNodeState and convert on fstat
    if attrs.has.content_size {
        info.size = attrs.content_size.try_into().unwrap_or(std::usize::MAX);
    }
    if attrs.has.storage_size {
        info.blocks = usize::try_from(attrs.storage_size)
            .unwrap_or(std::usize::MAX)
            .div_ceil(DEFAULT_BYTES_PER_BLOCK)
    }
    info.blksize = DEFAULT_BYTES_PER_BLOCK;
    if attrs.has.link_count {
        info.link_count = attrs.link_count.try_into().unwrap_or(std::usize::MAX);
    }
    if attrs.has.modification_time {
        info.time_modify =
            UtcInstant::from_nanos(attrs.modification_time.try_into().unwrap_or(i64::MAX));
    }
    if attrs.has.change_time {
        info.time_status_change =
            UtcInstant::from_nanos(attrs.change_time.try_into().unwrap_or(i64::MAX));
    }
    if attrs.has.access_time {
        info.time_access = UtcInstant::from_nanos(attrs.access_time.try_into().unwrap_or(i64::MAX));
    }
}

const MODE_ATTRIBUTES: fio::NodeAttributesQuery =
    fio::NodeAttributesQuery::PROTOCOLS.union(fio::NodeAttributesQuery::MODE);

// NOTE: Keep in sync with `MODE_ATTRIBUTES`.
fn get_mode(attrs: &zxio_node_attributes_t, rights: fio::Flags) -> FileMode {
    if attrs.protocols & ZXIO_NODE_PROTOCOL_SYMLINK != 0 {
        // We don't set the mode for symbolic links , so we synthesize it instead.
        FileMode::IFLNK | FileMode::ALLOW_ALL
    } else if attrs.has.mode {
        // If the filesystem supports POSIX mode bits, use that directly.
        FileMode::from_bits(attrs.mode)
    } else {
        // The filesystem doesn't support the `mode` attribute, so synthesize it from the protocols
        // this node supports, and the rights used to open it.
        let is_directory =
            attrs.protocols & ZXIO_NODE_PROTOCOL_DIRECTORY == ZXIO_NODE_PROTOCOL_DIRECTORY;
        let mode = if is_directory { FileMode::IFDIR } else { FileMode::IFREG };
        let mut permissions = FileMode::EMPTY;
        if rights.contains(fio::PERM_READABLE) {
            permissions |= FileMode::IRUSR;
        }
        if rights.contains(fio::PERM_WRITABLE) {
            permissions |= FileMode::IWUSR;
        }
        if rights.contains(fio::PERM_EXECUTABLE) {
            permissions |= FileMode::IXUSR;
        }
        // Make sure the same permissions are granted to user, group, and other.
        permissions |= FileMode::from_bits((permissions.bits() >> 3) | (permissions.bits() >> 6));
        mode | permissions
    }
}

fn get_name_str<'a>(name_bytes: &'a FsStr) -> Result<&'a str, Errno> {
    std::str::from_utf8(name_bytes.as_ref()).map_err(|_| {
        log_warn!("bad utf8 in pathname! remote filesystems can't handle this");
        errno!(EINVAL)
    })
}

impl XattrStorage for RemoteIo {
    fn get_xattr(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        name: &FsStr,
    ) -> Result<FsString, Errno> {
        Ok(self
            .xattr_get(name)
            .map_err(|status| match status {
                zx::Status::NOT_FOUND => errno!(ENODATA),
                status => from_status_like_fdio!(status),
            })?
            .into())
    }

    fn set_xattr(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        name: &FsStr,
        value: &FsStr,
        op: XattrOp,
    ) -> Result<(), Errno> {
        let mode = match op {
            XattrOp::Set => XattrSetMode::Set,
            XattrOp::Create => XattrSetMode::Create,
            XattrOp::Replace => XattrSetMode::Replace,
        };

        self.xattr_set(name, value, mode).map_err(|status| match status {
            zx::Status::NOT_FOUND => errno!(ENODATA),
            status => from_status_like_fdio!(status),
        })
    }

    fn remove_xattr(&self, _locked: &mut Locked<FileOpsCore>, name: &FsStr) -> Result<(), Errno> {
        self.xattr_remove(name).map_err(|status| match status {
            zx::Status::NOT_FOUND => errno!(ENODATA),
            _ => from_status_like_fdio!(status),
        })
    }

    fn list_xattrs(&self, _locked: &mut Locked<FileOpsCore>) -> Result<Vec<FsString>, Errno> {
        self.xattr_list()
            .map(|attrs| attrs.into_iter().map(FsString::new).collect::<Vec<_>>())
            .map_err(|status| from_status_like_fdio!(status))
    }
}

impl FsNodeOps for RemoteNode {
    fs_node_impl_xattr_delegate!(self, self.io);

    fn create_file_ops(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        node: &FsNode,
        current_task: &CurrentTask,
        flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        {
            // It is safe to read the cached node info here because the `wrapping_key_id` is
            // fetched when the node is first opened, and updated when set. We don't expect this to
            // change out from under Starnix.
            let node_info = node.info();
            if node_info.mode.is_dir() {
                if let Some(wrapping_key_id) = node_info.wrapping_key_id {
                    if flags.can_write() {
                        // Locked encrypted directories cannot be opened with write access.
                        let crypt_service =
                            node.fs().crypt_service().ok_or_else(|| errno!(ENOKEY))?;
                        if !crypt_service.contains_key(EncryptionKeyId::from(wrapping_key_id)) {
                            return error!(ENOKEY);
                        }
                    }
                }
                // For directories we need to clone the connection because we rely on the seek
                // offset.
                return Ok(Box::new(RemoteDirectoryObject::new(
                    self.io
                        .clone_proxy()
                        .map(|p| p.into_channel().into())
                        .map_err(|status| from_status_like_fdio!(status))?,
                )));
            }
        }

        // Locked encrypted files cannot be opened.
        node.fail_if_locked(current_task)?;

        // fsverity files cannot be opened in write mode, including while building.
        if flags.can_write() {
            node.fsverity.lock().check_writable()?;
        }

        Ok(Box::new(RemoteFileObject::default()))
    }

    fn mknod(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        node: &FsNode,
        current_task: &CurrentTask,
        name: &FsStr,
        mode: FileMode,
        dev: DeviceType,
        owner: FsCred,
    ) -> Result<FsNodeHandle, Errno> {
        node.fail_if_locked(current_task)?;
        let name = get_name_str(name)?;

        let fs = node.fs();
        let fs_ops = RemoteFs::from_fs(&fs);

        if !(mode.is_reg() || mode.is_chr() || mode.is_blk() || mode.is_fifo() || mode.is_sock()) {
            return error!(EINVAL, name);
        }
        let (ops, attrs, _) = self
            .io
            .open(
                name,
                fio::Flags::FLAG_MUST_CREATE
                    | fio::Flags::PROTOCOL_FILE
                    | fio::PERM_READABLE
                    | fio::PERM_WRITABLE,
                Some(fio::MutableNodeAttributes {
                    mode: Some(mode.bits()),
                    uid: Some(owner.uid),
                    gid: Some(owner.gid),
                    rdev: Some(dev.bits()),
                    ..Default::default()
                }),
                fio::NodeAttributesQuery::ID | fio::NodeAttributesQuery::WRAPPING_KEY_ID,
                !mode.is_reg(),
            )
            .map_err(|status| from_status_like_fdio!(status, name))?;

        let node_id = if fs_ops.use_remote_ids { attrs.id } else { fs.allocate_ino() };

        let mut node_info = FsNodeInfo { rdev: dev, ..FsNodeInfo::new(mode, owner) };
        if attrs.has.wrapping_key_id {
            node_info.wrapping_key_id = Some(attrs.wrapping_key_id);
        }

        let child = fs.create_node(node_id, ops, node_info);
        Ok(child)
    }

    fn mkdir(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        node: &FsNode,
        current_task: &CurrentTask,
        name: &FsStr,
        mode: FileMode,
        owner: FsCred,
    ) -> Result<FsNodeHandle, Errno> {
        node.fail_if_locked(current_task)?;
        let name = get_name_str(name)?;

        let fs = node.fs();
        let fs_ops = RemoteFs::from_fs(&fs);

        let mut node_id;
        let (ops, attrs, _) = self
            .io
            .open(
                name,
                fio::Flags::FLAG_MUST_CREATE
                    | fio::Flags::PROTOCOL_DIRECTORY
                    | fio::PERM_READABLE
                    | fio::PERM_WRITABLE,
                Some(fio::MutableNodeAttributes {
                    mode: Some(mode.bits()),
                    uid: Some(owner.uid),
                    gid: Some(owner.gid),
                    ..Default::default()
                }),
                fio::NodeAttributesQuery::ID | fio::NodeAttributesQuery::WRAPPING_KEY_ID,
                false,
            )
            .map_err(|status| from_status_like_fdio!(status, name))?;
        node_id = attrs.id;

        if !fs_ops.use_remote_ids {
            node_id = fs.allocate_ino();
        }

        let mut node_info = FsNodeInfo::new(mode, owner);
        if attrs.has.wrapping_key_id {
            node_info.wrapping_key_id = Some(attrs.wrapping_key_id);
        }

        let child = fs.create_node(node_id, ops, node_info);
        Ok(child)
    }

    fn lookup(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        node: &FsNode,
        current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<FsNodeHandle, Errno> {
        let name = get_name_str(name)?;

        let fs = node.fs();
        let fs_ops = RemoteFs::from_fs(&fs);

        let mut query = MODE_ATTRIBUTES
            | NODE_INFO_ATTRIBUTES
            | fio::NodeAttributesQuery::ID
            | fio::NodeAttributesQuery::UID
            | fio::NodeAttributesQuery::GID
            | fio::NodeAttributesQuery::RDEV
            | fio::NodeAttributesQuery::WRAPPING_KEY_ID
            | fio::NodeAttributesQuery::VERITY_ENABLED
            | fio::NodeAttributesQuery::CASEFOLD;

        if security::fs_is_xattr_labeled(node.fs()) {
            query |= fio::NodeAttributesQuery::SELINUX_CONTEXT;
        }
        let (ops, attrs, context) = self
            .io
            .open(name, fs_ops.root_rights, None, query, false)
            .map_err(|status| from_status_like_fdio!(status, name))?;
        let node_id = if fs_ops.use_remote_ids {
            if attrs.id == fio::INO_UNKNOWN {
                return error!(ENOTSUP);
            }
            attrs.id
        } else {
            fs.allocate_ino()
        };
        let owner = FsCred { uid: attrs.uid, gid: attrs.gid };
        let rdev = DeviceType::from_bits(attrs.rdev);
        let fsverity_enabled = attrs.fsverity_enabled;
        // fsverity should not be enabled for non-file nodes.
        if fsverity_enabled && (attrs.protocols & ZXIO_NODE_PROTOCOL_FILE == 0) {
            return error!(EINVAL);
        }
        let casefold = attrs.casefold;
        let time_modify =
            UtcInstant::from_nanos(attrs.modification_time.try_into().unwrap_or(i64::MAX));
        let time_status_change =
            UtcInstant::from_nanos(attrs.change_time.try_into().unwrap_or(i64::MAX));
        let time_access = UtcInstant::from_nanos(attrs.access_time.try_into().unwrap_or(i64::MAX));

        let mut ops = Some(ops);
        let node = fs.get_or_create_node(node_id, || {
            let child = FsNode::new_uncached(
                node_id,
                ops.take().unwrap(),
                &fs,
                FsNodeInfo {
                    rdev,
                    casefold,
                    time_status_change,
                    time_modify,
                    time_access,
                    wrapping_key_id: attrs.has.wrapping_key_id.then_some(attrs.wrapping_key_id),
                    ..FsNodeInfo::new(get_mode(&attrs, fs_ops.root_rights), owner)
                },
            );
            if fsverity_enabled {
                *child.fsverity.lock() = FsVerityState::FsVerity;
            }
            // This is valid to fail if we're using mount point labelling or the provided context
            // string is invalid.
            if let Some(fio::SelinuxContext::Data(data)) = &context {
                let _ = security::fs_node_notify_security_context(
                    current_task,
                    &child,
                    FsStr::new(&data),
                );
            }
            Ok(child)
        })?;

        // Encrypted symlinks that use fscrypt can be read as encrypted links when no key is
        // available.  When no key is available, directories will not cache their entries.  When,
        // the key is subsequently provided, the next time the symlink is read, we will come through
        // here, but since the node is cached, `get_or_create_node` will not create a new node
        // which, if we were to do nothing, would mean we'd keep the encrypted value for the target.
        // To address this, if no new node was created, we update the target of the existing node
        // here.  Once the key has been provided, the entry will be cached with the directory and
        // whilst the entry remains cached, `lookup` will not be called.
        if let Some(ops) = ops
            && let Some(new_symlink) = ops.as_any().downcast_ref::<RemoteSymlink>()
            && let Some(symlink) = node.downcast_ops::<RemoteSymlink>()
        {
            *symlink.target.write() = std::mem::take(&mut new_symlink.target.write());
        }

        Ok(node)
    }

    fn truncate(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _guard: &AppendLockGuard<'_>,
        node: &FsNode,
        current_task: &CurrentTask,
        length: u64,
    ) -> Result<(), Errno> {
        node.fail_if_locked(current_task)?;
        self.io.truncate(length).map_err(|status| from_status_like_fdio!(status))
    }

    fn allocate(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _guard: &AppendLockGuard<'_>,
        node: &FsNode,
        current_task: &CurrentTask,
        mode: FallocMode,
        offset: u64,
        length: u64,
    ) -> Result<(), Errno> {
        match mode {
            FallocMode::Allocate { keep_size: false } => {
                node.fail_if_locked(current_task)?;
                self.io
                    .allocate(offset, length, AllocateMode::empty())
                    .map_err(|status| from_status_like_fdio!(status))?;
                Ok(())
            }
            _ => error!(EINVAL),
        }
    }

    fn fetch_and_refresh_info<'a>(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _node: &FsNode,
        _current_task: &CurrentTask,
        info: &'a RwLock<FsNodeInfo>,
    ) -> Result<RwLockReadGuard<'a, FsNodeInfo>, Errno> {
        fetch_and_refresh_info_impl(&self.io, info)
    }

    fn update_attributes(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _current_task: &CurrentTask,
        info: &FsNodeInfo,
        has: zxio_node_attr_has_t,
    ) -> Result<(), Errno> {
        // Omit updating creation_time. By definition, there shouldn't be a change in creation_time.
        self.io
            .attr_set(fio::MutableNodeAttributes {
                modification_time: has
                    .modification_time
                    .then_some(info.time_modify.into_nanos() as u64),
                access_time: has.access_time.then_some(info.time_access.into_nanos() as u64),
                mode: has.mode.then_some(info.mode.bits()),
                uid: has.uid.then_some(info.uid),
                gid: has.gid.then_some(info.gid),
                rdev: has.rdev.then_some(info.rdev.bits()),
                casefold: has.casefold.then_some(info.casefold),
                wrapping_key_id: if has.wrapping_key_id { info.wrapping_key_id } else { None },
                ..Default::default()
            })
            .map_err(|status| from_status_like_fdio!(status))
    }

    fn unlink(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _node: &FsNode,
        _current_task: &CurrentTask,
        name: &FsStr,
        _child: &FsNodeHandle,
    ) -> Result<(), Errno> {
        // We don't care about the _child argument because 1. unlinking already takes the parent's
        // children lock, so we don't have to worry about conflicts on this path, and 2. the remote
        // filesystem tracks the link counts so we don't need to update them here.
        let name = get_name_str(name)?;
        self.io
            .unlink(name, fio::UnlinkFlags::empty())
            .map_err(|status| from_status_like_fdio!(status))
    }

    fn create_symlink(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        node: &FsNode,
        current_task: &CurrentTask,
        name: &FsStr,
        target: &FsStr,
        owner: FsCred,
    ) -> Result<FsNodeHandle, Errno> {
        node.fail_if_locked(current_task)?;

        let name = get_name_str(name)?;
        let io = self
            .io
            .create_symlink(name, target)
            .map_err(|status| from_status_like_fdio!(status))?;

        let fs = node.fs();
        let fs_ops = RemoteFs::from_fs(&fs);

        let node_id = if fs_ops.use_remote_ids {
            io.attr_get(fio::NodeAttributesQuery::ID)
                .map_err(|status| from_status_like_fdio!(status))?
                .1
                .id
                .unwrap_or_default()
        } else {
            fs.allocate_ino()
        };
        Ok(fs.create_node(
            node_id,
            RemoteSymlink::new(io, target.as_bytes()),
            FsNodeInfo {
                size: target.len(),
                ..FsNodeInfo::new(FileMode::IFLNK | FileMode::ALLOW_ALL, owner)
            },
        ))
    }

    fn create_tmpfile(
        &self,
        node: &FsNode,
        _current_task: &CurrentTask,
        mode: FileMode,
        owner: FsCred,
    ) -> Result<FsNodeHandle, Errno> {
        let fs = node.fs();
        let fs_ops = RemoteFs::from_fs(&fs);

        let mut node_id;
        if !mode.is_reg() {
            return error!(EINVAL);
        }
        // `create_tmpfile` is used by O_TMPFILE. Note that
        // <https://man7.org/linux/man-pages/man2/open.2.html> states that if O_EXCL is specified
        // with O_TMPFILE, the temporary file created cannot be linked into the filesystem. Although
        // there exist fuchsia flags `fio::FLAG_TEMPORARY_AS_NOT_LINKABLE`, the starnix vfs already
        // handles this case and makes sure that the created file is not linkable. There is also no
        // current way of passing the open flags to this function.
        let (ops, attrs, _) = self
            .io
            .open(
                ".",
                fio::Flags::PROTOCOL_FILE
                    | fio::Flags::FLAG_CREATE_AS_UNNAMED_TEMPORARY
                    | fio::PERM_READABLE
                    | fio::PERM_WRITABLE,
                Some(fio::MutableNodeAttributes {
                    mode: Some(mode.bits()),
                    uid: Some(owner.uid),
                    gid: Some(owner.gid),
                    ..Default::default()
                }),
                fio::NodeAttributesQuery::ID,
                false,
            )
            .map_err(|status| from_status_like_fdio!(status))?;
        node_id = attrs.id;

        if !fs_ops.use_remote_ids {
            node_id = fs.allocate_ino();
        }
        Ok(fs.create_node(node_id, ops, FsNodeInfo::new(mode, owner)))
    }

    fn link(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        node: &FsNode,
        _current_task: &CurrentTask,
        name: &FsStr,
        child: &FsNodeHandle,
    ) -> Result<(), Errno> {
        if !RemoteFs::from_fs(&node.fs()).use_remote_ids {
            return error!(EPERM);
        }
        let name = get_name_str(name)?;
        if let Some(child) = child.downcast_ops::<RemoteNode>() {
            child.io.link_into(&self.io, name).map_err(|status| match status {
                zx::Status::BAD_STATE => errno!(EXDEV),
                zx::Status::ACCESS_DENIED => errno!(ENOKEY),
                s => from_status_like_fdio!(s),
            })
        } else if let Some(child) = child.downcast_ops::<RemoteSymlink>() {
            child.io.link_into(&self.io, name).map_err(|status| match status {
                zx::Status::BAD_STATE => errno!(EXDEV),
                zx::Status::ACCESS_DENIED => errno!(ENOKEY),
                s => from_status_like_fdio!(s),
            })
        } else {
            error!(EXDEV)
        }
    }

    fn forget(
        self: Box<Self>,
        _locked: &mut Locked<FileOpsCore>,
        _current_task: &CurrentTask,
        info: FsNodeInfo,
    ) -> Result<(), Errno> {
        // Before forgetting this node, update atime if we need to.
        if info.pending_time_access_update {
            self.io
                .close_and_update_access_time()
                .map_err(|status| from_status_like_fdio!(status))?;
        }
        Ok(())
    }

    fn enable_fsverity(&self, descriptor: &fsverity_descriptor) -> Result<(), Errno> {
        let descr = zxio_fsverity_descriptor_t {
            hash_algorithm: descriptor.hash_algorithm,
            salt_size: descriptor.salt_size,
            salt: descriptor.salt,
        };
        self.io.enable_verity(&descr).map_err(|status| from_status_like_fdio!(status))
    }

    fn get_fsverity_descriptor(&self, log_blocksize: u8) -> Result<fsverity_descriptor, Errno> {
        let (_, attrs) = self
            .io
            .attr_get(
                fio::NodeAttributesQuery::CONTENT_SIZE
                    | fio::NodeAttributesQuery::OPTIONS
                    | fio::NodeAttributesQuery::ROOT_HASH,
            )
            .map_err(|status| from_status_like_fdio!(status))?;
        let fio::ImmutableNodeAttributes {
            content_size: Some(data_size),
            options:
                Some(fio::VerificationOptions {
                    hash_algorithm: Some(hash_algorithm),
                    salt: Some(salt),
                    ..
                }),
            root_hash: Some(root_hash),
            ..
        } = attrs
        else {
            return error!(ENODATA);
        };
        let mut descriptor = fsverity_descriptor {
            version: 1,
            hash_algorithm: hash_algorithm.into_primitive(),
            log_blocksize,
            __reserved_0x04: 0u32,
            data_size,
            ..Default::default()
        };
        if salt.len() > std::mem::size_of_val(&descriptor.salt)
            || root_hash.len() > std::mem::size_of_val(&descriptor.root_hash)
        {
            return error!(EIO);
        }
        descriptor.salt_size = salt.len() as u8;
        descriptor.salt[..salt.len()].copy_from_slice(&salt);
        descriptor.root_hash[..root_hash.len()].copy_from_slice(&root_hash);
        Ok(descriptor)
    }
}

struct RemoteSpecialNode {
    io: RemoteIo,
}

impl FsNodeOps for RemoteSpecialNode {
    fs_node_impl_not_dir!();
    fs_node_impl_xattr_delegate!(self, self.io);

    fn create_file_ops(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        unreachable!("Special nodes cannot be opened.");
    }
}

#[derive(Default)]
enum Entry {
    // Indicates no more entries.
    #[default]
    None,

    Some {
        ino: ino_t,
        entry_type: DirectoryEntryType,
        name: Range<usize>,
    },

    // Indicates dot-dot should be synthesized.
    DotDot,
}

impl Entry {
    fn take(&mut self) -> Entry {
        std::mem::replace(self, Entry::None)
    }
}

struct RemoteDirectoryObject {
    proxy: fio::DirectorySynchronousProxy,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Buffer contains the last batch of entries read from the remote end.
    buffer: Vec<u8>,

    /// Position in the buffer for the next entry.
    offset: usize,

    /// If the last attempt to write to the sink failed, this contains the entry that is pending to
    /// be added. This is also used to synthesize dot-dot.
    pending_entry: Entry,
}

impl State {
    fn name(&self, range: Range<usize>) -> &FsStr {
        FsStr::new(&self.buffer[range])
    }

    /// Returns the next dir entry. If no more entries are found, returns None.  Returns an error if
    /// the iterator fails.
    fn next(&mut self, proxy: &fio::DirectorySynchronousProxy) -> Result<Entry, Errno> {
        let mut next_dirent = || {
            if self.offset >= self.buffer.len() {
                match proxy.read_dirents(fio::MAX_BUF, zx::MonotonicInstant::INFINITE) {
                    Ok((status, dirents)) => {
                        zx::Status::ok(status).map_err(|s| from_status_like_fdio!(s))?;
                        if dirents.is_empty() {
                            return Ok(None);
                        }
                        self.buffer = dirents;
                        self.offset = 0;
                    }
                    Err(_) => return error!(EIO),
                }
            }

            #[repr(C, packed)]
            #[derive(FromBytes)]
            struct DirectoryEntry {
                ino: u64,
                name_len: u8,
                entry_type: u8,
            }

            let Some((ino, name, entry_type)) =
                DirectoryEntry::read_from_prefix(&self.buffer[self.offset..]).ok().and_then(
                    |(DirectoryEntry { ino, name_len, entry_type }, remainder)| {
                        let name_len = name_len as usize;
                        let name_start = self.offset + std::mem::size_of::<DirectoryEntry>();
                        (remainder.len() >= name_len).then_some((
                            ino,
                            name_start..name_start + name_len,
                            entry_type,
                        ))
                    },
                )
            else {
                // Truncated entry.
                return Ok(None);
            };

            self.offset = name.end;

            Ok(Some(Entry::Some {
                ino,
                entry_type: match entry_type {
                    4 => DirectoryEntryType::DIR,
                    8 => DirectoryEntryType::REG,
                    10 => DirectoryEntryType::LNK,
                    _ => DirectoryEntryType::UNKNOWN,
                },
                name,
            }))
        };

        let mut next = self.pending_entry.take();
        if let Entry::None = next {
            next = next_dirent()?.unwrap_or(Entry::None);
        }
        // We only want to synthesize .. if . exists because the . and .. entries get removed if the
        // directory is unlinked, so if the remote filesystem has removed ., we know to omit the
        // .. entry.
        match &next {
            Entry::Some { name, .. } if self.name(name.clone()) == "." => {
                self.pending_entry = Entry::DotDot;
            }
            _ => {}
        }
        Ok(next)
    }
}

impl RemoteDirectoryObject {
    fn new(proxy: fio::DirectorySynchronousProxy) -> Self {
        Self { proxy, state: Mutex::default() }
    }

    fn rewind(&self) -> Result<(), zx::Status> {
        let mut state = self.state.lock();
        state.pending_entry = Entry::None;
        let status =
            self.proxy.rewind(zx::MonotonicInstant::INFINITE).map_err(|_| zx::Status::IO)?;
        zx::Status::ok(status)?;
        state.buffer.clear();
        state.offset = 0;
        Ok(())
    }
}

impl FileOps for RemoteDirectoryObject {
    fileops_impl_directory!();

    fn seek(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        current_offset: off_t,
        target: SeekTarget,
    ) -> Result<off_t, Errno> {
        let new_offset = default_seek(current_offset, target, || error!(EINVAL))?;
        let mut iterator_position = current_offset;

        if new_offset < iterator_position {
            // Our iterator only goes forward, so reset it here.  Note: we *must* rewind it rather
            // than just create a new iterator because the remote end maintains the offset.
            self.rewind().map_err(|status| from_status_like_fdio!(status))?;
            iterator_position = 0;
        }

        // Advance the iterator to catch up with the offset.
        let mut state = self.state.lock();
        for i in iterator_position..new_offset {
            match state.next(&self.proxy) {
                Ok(Entry::Some { .. } | Entry::DotDot) => {}
                Ok(Entry::None) => break, // No more entries.
                Err(_) => {
                    // In order to keep the offset and the iterator in sync, set the new offset
                    // to be as far as we could get.
                    // Note that failing the seek here would also cause the iterator and the
                    // offset to not be in sync, because the iterator has already moved from
                    // where it was.
                    return Ok(i);
                }
            }
        }

        Ok(new_offset)
    }

    fn readdir(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        file: &FileObject,
        _current_task: &CurrentTask,
        sink: &mut dyn DirentSink,
    ) -> Result<(), Errno> {
        let mut state = self.state.lock();
        loop {
            let entry = state.next(&self.proxy)?;
            if let Err(e) = match &entry {
                Entry::Some { ino, entry_type, name } => {
                    sink.add(*ino, sink.offset() + 1, *entry_type, state.name(name.clone()))
                }
                Entry::DotDot => {
                    let inode_num = if let Some(parent) = file.name.parent_within_mount() {
                        parent.node.ino
                    } else {
                        // For the root .. should have the same inode number as .
                        file.name.entry.node.ino
                    };
                    sink.add(inode_num, sink.offset() + 1, DirectoryEntryType::DIR, "..".into())
                }
                Entry::None => break,
            } {
                state.pending_entry = entry;
                return Err(e);
            }
        }
        Ok(())
    }

    fn sync(&self, _file: &FileObject, _current_task: &CurrentTask) -> Result<(), Errno> {
        self.proxy
            .sync(zx::MonotonicInstant::INFINITE)
            .map_err(|_| errno!(EIO))?
            .map_err(|status| map_sync_error(zx::Status::from_raw(status)))
    }

    fn to_handle(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
    ) -> Result<Option<zx::NullableHandle>, Errno> {
        let (client_end, server_end) = zx::Channel::create();
        self.proxy.clone(server_end.into()).map_err(|_| errno!(EIO))?;
        Ok(Some(client_end.into_handle()))
    }
}

#[derive(Default)]
pub struct RemoteFileObject {
    /// Cached read-only VMO handle.
    read_only_memory: OnceCell<Arc<MemoryObject>>,

    /// Cached read/exec VMO handle.
    read_exec_memory: OnceCell<Arc<MemoryObject>>,
}

impl RemoteFileObject {
    /// # Panics
    ///
    /// This will panic if the node's ops are not `RemoteNode`; `AnonymousRemoteFileObject` should
    /// be used if this won't be the case.
    fn io(file: &FileObject) -> &RemoteIo {
        &file.node().downcast_ops::<RemoteNode>().unwrap().io
    }
}

impl FileOps for RemoteFileObject {
    fileops_impl_seekable!();

    fn read(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        file: &FileObject,
        _current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        Self::io(file).read_at(offset as u64, data)
    }

    fn write(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        file: &FileObject,
        _current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        Self::io(file).write_at(offset as u64, data)
    }

    fn get_memory(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        file: &FileObject,
        _current_task: &CurrentTask,
        _length: Option<usize>,
        prot: ProtectionFlags,
    ) -> Result<Arc<MemoryObject>, Errno> {
        trace_duration!(CATEGORY_STARNIX_MM, "RemoteFileGetVmo");
        let memory_cache = if prot == (ProtectionFlags::READ | ProtectionFlags::EXEC) {
            Some(&self.read_exec_memory)
        } else if prot == ProtectionFlags::READ {
            Some(&self.read_only_memory)
        } else {
            None
        };

        let io = Self::io(file);

        memory_cache
            .map(|c| c.get_or_try_init(|| io.fetch_remote_memory(prot)).cloned())
            .unwrap_or_else(|| io.fetch_remote_memory(prot))
    }

    fn to_handle(
        &self,
        file: &FileObject,
        _current_task: &CurrentTask,
    ) -> Result<Option<zx::NullableHandle>, Errno> {
        Self::io(file).to_handle()
    }

    fn sync(&self, file: &FileObject, _current_task: &CurrentTask) -> Result<(), Errno> {
        Self::io(file).sync()
    }

    fn ioctl(
        &self,
        locked: &mut Locked<Unlocked>,
        file: &FileObject,
        current_task: &CurrentTask,
        request: u32,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        default_ioctl(file, locked, current_task, request, arg)
    }
}

/// A file object that is not attached to a `RemoteFs`, which means it stores its own `RemoteIo`.
pub struct AnonymousRemoteFileObject {
    io: RemoteIo,

    /// Cached read-only VMO handle.
    read_only_memory: OnceCell<Arc<MemoryObject>>,

    /// Cached read/exec VMO handle.
    read_exec_memory: OnceCell<Arc<MemoryObject>>,
}

impl AnonymousRemoteFileObject {
    fn new(io: RemoteIo) -> Self {
        Self { io, read_only_memory: Default::default(), read_exec_memory: Default::default() }
    }
}

impl FileOps for AnonymousRemoteFileObject {
    fileops_impl_seekable!();

    fn read(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        self.io.read_at(offset as u64, data)
    }

    fn write(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        self.io.write_at(offset as u64, data)
    }

    fn get_memory(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _length: Option<usize>,
        prot: ProtectionFlags,
    ) -> Result<Arc<MemoryObject>, Errno> {
        trace_duration!(CATEGORY_STARNIX_MM, "RemoteFileGetVmo");
        let memory_cache = if prot == (ProtectionFlags::READ | ProtectionFlags::EXEC) {
            Some(&self.read_exec_memory)
        } else if prot == ProtectionFlags::READ {
            Some(&self.read_only_memory)
        } else {
            None
        };

        memory_cache
            .map(|c| c.get_or_try_init(|| self.io.fetch_remote_memory(prot)).cloned())
            .unwrap_or_else(|| self.io.fetch_remote_memory(prot))
    }

    fn to_handle(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
    ) -> Result<Option<zx::NullableHandle>, Errno> {
        self.io.to_handle()
    }

    fn sync(&self, _file: &FileObject, _current_task: &CurrentTask) -> Result<(), Errno> {
        self.io.sync()
    }

    fn ioctl(
        &self,
        locked: &mut Locked<Unlocked>,
        file: &FileObject,
        current_task: &CurrentTask,
        request: u32,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        default_ioctl(file, locked, current_task, request, arg)
    }
}

pub struct RemoteZxioFileObject {
    /// The underlying Zircon I/O object.  This is shared, so we must take care not to use any
    /// stateful methods on the underlying object (reading and writing is fine).
    zxio: Zxio,

    /// Cached read-only VMO handle.
    read_only_memory: OnceCell<Arc<MemoryObject>>,

    /// Cached read/exec VMO handle.
    read_exec_memory: OnceCell<Arc<MemoryObject>>,
}

impl RemoteZxioFileObject {
    fn new(zxio: Zxio) -> RemoteZxioFileObject {
        RemoteZxioFileObject {
            zxio,
            read_only_memory: Default::default(),
            read_exec_memory: Default::default(),
        }
    }

    fn fetch_remote_memory(&self, prot: ProtectionFlags) -> Result<Arc<MemoryObject>, Errno> {
        let without_exec = self
            .zxio
            .vmo_get(prot.to_vmar_flags() - zx::VmarFlags::PERM_EXECUTE)
            .map_err(|status| from_status_like_fdio!(status))?;
        let all_flags = if prot.contains(ProtectionFlags::EXEC) {
            without_exec.replace_as_executable(&VMEX_RESOURCE).map_err(impossible_error)?
        } else {
            without_exec
        };
        Ok(Arc::new(MemoryObject::from(all_flags)))
    }
}

impl FileOps for RemoteZxioFileObject {
    fileops_impl_seekable!();

    fn read(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        let offset = offset as u64;
        let read_bytes = with_iovec_segments::<_, syncio::zxio::zx_iovec, _>(data, |iovecs| {
            // SAFETY: The iovecs are valid for writing because they come from OutputBuffer.
            unsafe { self.zxio.readv_at(offset, iovecs).map_err(map_stream_error) }
        });

        match read_bytes {
            Some(actual) => {
                let actual = actual?;
                // SAFETY: we successfully read `actual` bytes
                // directly to the user's buffer segments.
                unsafe { data.advance(actual) }?;
                Ok(actual)
            }
            None => {
                // Perform the (slower) operation by using an intermediate buffer.
                let total = data.available();
                let mut bytes = vec![0u8; total];
                let actual = self
                    .zxio
                    .read_at(offset, &mut bytes)
                    .map_err(|status| from_status_like_fdio!(status))?;
                data.write_all(&bytes[0..actual])
            }
        }
    }

    fn write(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        let offset = offset as u64;
        let write_bytes = with_iovec_segments::<_, syncio::zxio::zx_iovec, _>(data, |iovecs| {
            // SAFETY: The iovecs are valid for reading because they come from InputBuffer.
            unsafe { self.zxio.writev_at(offset, iovecs).map_err(map_stream_error) }
        });

        match write_bytes {
            Some(actual) => {
                let actual = actual?;
                data.advance(actual)?;
                Ok(actual)
            }
            None => {
                // Perform the (slower) operation by using an intermediate buffer.
                let bytes = data.peek_all()?;
                let actual = self
                    .zxio
                    .write_at(offset, &bytes)
                    .map_err(|status| from_status_like_fdio!(status))?;
                data.advance(actual)?;
                Ok(actual)
            }
        }
    }

    fn get_memory(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _length: Option<usize>,
        prot: ProtectionFlags,
    ) -> Result<Arc<MemoryObject>, Errno> {
        trace_duration!(CATEGORY_STARNIX_MM, "RemoteFileGetVmo");
        let memory_cache = if prot == (ProtectionFlags::READ | ProtectionFlags::EXEC) {
            Some(&self.read_exec_memory)
        } else if prot == ProtectionFlags::READ {
            Some(&self.read_only_memory)
        } else {
            None
        };

        memory_cache
            .map(|c| c.get_or_try_init(|| self.fetch_remote_memory(prot)).cloned())
            .unwrap_or_else(|| self.fetch_remote_memory(prot))
    }

    fn to_handle(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
    ) -> Result<Option<zx::NullableHandle>, Errno> {
        self.zxio
            .clone_handle()
            .map(|h| Some(h.into()))
            .map_err(|status| from_status_like_fdio!(status))
    }

    fn sync(&self, _file: &FileObject, _current_task: &CurrentTask) -> Result<(), Errno> {
        self.zxio.sync().map_err(map_sync_error)
    }

    fn ioctl(
        &self,
        locked: &mut Locked<Unlocked>,
        file: &FileObject,
        current_task: &CurrentTask,
        request: u32,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        default_ioctl(file, locked, current_task, request, arg)
    }
}

struct RemoteSymlink {
    io: RemoteIo,
    target: RwLock<Box<[u8]>>,
}

impl RemoteSymlink {
    fn new(io: RemoteIo, target: impl Into<Box<[u8]>>) -> Self {
        Self { io, target: RwLock::new(target.into()) }
    }
}

impl FsNodeOps for RemoteSymlink {
    fs_node_impl_symlink!();
    fs_node_impl_xattr_delegate!(self, self.io);

    fn readlink(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _node: &FsNode,
        _current_task: &CurrentTask,
    ) -> Result<SymlinkTarget, Errno> {
        Ok(SymlinkTarget::Path(FsString::new(self.target.read().to_vec())))
    }

    fn fetch_and_refresh_info<'a>(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _node: &FsNode,
        _current_task: &CurrentTask,
        info: &'a RwLock<FsNodeInfo>,
    ) -> Result<RwLockReadGuard<'a, FsNodeInfo>, Errno> {
        fetch_and_refresh_info_impl(&self.io, info)
    }

    fn forget(
        self: Box<Self>,
        _locked: &mut Locked<FileOpsCore>,
        _current_task: &CurrentTask,
        info: FsNodeInfo,
    ) -> Result<(), Errno> {
        // Before forgetting this node, update atime if we need to.
        if info.pending_time_access_update {
            self.io
                .close_and_update_access_time()
                .map_err(|status| from_status_like_fdio!(status))?;
        }
        Ok(())
    }
}

pub struct RemoteCounter {
    counter: Counter,
}

impl RemoteCounter {
    fn new(counter: Counter) -> Self {
        Self { counter }
    }

    pub fn duplicate_handle(&self) -> Result<Counter, Errno> {
        self.counter.duplicate_handle(zx::Rights::SAME_RIGHTS).map_err(impossible_error)
    }
}

impl FileOps for RemoteCounter {
    fileops_impl_nonseekable!();
    fileops_impl_noop_sync!();

    fn read(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        _data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        error!(ENOTSUP)
    }

    fn write(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        _data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        error!(ENOTSUP)
    }

    fn ioctl(
        &self,
        locked: &mut Locked<Unlocked>,
        file: &FileObject,
        current_task: &CurrentTask,
        request: u32,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        let ioctl_type = (request >> 8) as u8;
        let ioctl_number = request as u8;
        if ioctl_type == SYNC_IOC_MAGIC
            && (ioctl_number == SYNC_IOC_FILE_INFO || ioctl_number == SYNC_IOC_MERGE)
        {
            let mut sync_points: Vec<SyncPoint> = vec![];
            let counter = self.duplicate_handle()?;
            sync_points.push(SyncPoint::new(Timeline::Hwc, counter.into()));
            let sync_file_name: &[u8; 32] = b"remote counter\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0";
            let sync_file = SyncFile::new(*sync_file_name, SyncFence { sync_points });
            return sync_file.ioctl(locked, file, current_task, request, arg);
        }

        error!(EINVAL)
    }
}

#[track_caller]
fn map_sync_error(status: zx::Status) -> Errno {
    match status {
        zx::Status::NO_RESOURCES | zx::Status::NO_MEMORY | zx::Status::NO_SPACE => {
            errno!(ENOSPC)
        }
        zx::Status::INVALID_ARGS | zx::Status::NOT_FILE => errno!(EINVAL),
        zx::Status::BAD_HANDLE => errno!(EBADFD),
        zx::Status::NOT_SUPPORTED => errno!(ENOTSUP),
        zx::Status::INTERRUPTED_RETRY => errno!(EINTR),
        _ => errno!(EIO),
    }
}

#[track_caller]
fn map_stream_error(status: zx::Status) -> Errno {
    match status {
        // zx::Stream may return invalid args or not found error because of invalid zx_iovec buffer
        // pointers.
        zx::Status::INVALID_ARGS | zx::Status::NOT_FOUND => errno!(EFAULT),
        status => from_status_like_fdio!(status),
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::mm::PAGE_SIZE;
    use crate::testing::*;
    use crate::vfs::buffers::{VecInputBuffer, VecOutputBuffer};
    use crate::vfs::socket::{SocketFile, SocketMessageFlags};
    use crate::vfs::{EpollFileObject, LookupContext, Namespace, SymlinkMode, TimeUpdateType};
    use assert_matches::assert_matches;
    use fidl::endpoints::create_request_stream;
    use flyweights::FlyByteStr;
    use futures::StreamExt;
    use fxfs_testing::{TestFixture, TestFixtureOptions};
    use starnix_uapi::auth::Credentials;
    use starnix_uapi::errors::EINVAL;
    use starnix_uapi::file_mode::{AccessCheck, mode};
    use starnix_uapi::open_flags::OpenFlags;
    use starnix_uapi::vfs::{EpollEvent, FdEvents};
    use zx::HandleBased;
    use {fidl_fuchsia_io as fio, fuchsia_async as fasync};

    #[::fuchsia::test]
    async fn test_remote_uds() {
        spawn_kernel_and_run(async |locked, current_task| {
            let (s1, s2) = zx::Socket::create_datagram();
            s2.write(&vec![0]).expect("write");
            let file = new_remote_file(locked, &current_task, s1.into(), OpenFlags::RDWR)
                .expect("new_remote_file");
            assert!(file.node().is_sock());
            let socket_ops = file.downcast_file::<SocketFile>().unwrap();
            let flags = SocketMessageFlags::CTRUNC
                | SocketMessageFlags::TRUNC
                | SocketMessageFlags::NOSIGNAL
                | SocketMessageFlags::CMSG_CLOEXEC;
            let mut buffer = VecOutputBuffer::new(1024);
            let info = socket_ops
                .recvmsg(locked, &current_task, &file, &mut buffer, flags, None)
                .expect("recvmsg");
            assert!(info.ancillary_data.is_empty());
            assert_eq!(info.message_length, 1);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_tree() {
        spawn_kernel_and_run(async |locked, current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_EXECUTABLE;
            let (server, client) = zx::Channel::create();
            fdio::open("/pkg", rights, server).expect("failed to open /pkg");
            let fs = RemoteFs::new_fs(
                locked,
                &kernel,
                client,
                FileSystemOptions { source: FlyByteStr::new(b"/pkg"), ..Default::default() },
                rights,
            )
            .unwrap();
            let ns = Namespace::new(fs);
            let root = ns.root();
            let mut context = LookupContext::default();
            assert_eq!(
                root.lookup_child(locked, &current_task, &mut context, "nib".into()).err(),
                Some(errno!(ENOENT))
            );
            let mut context = LookupContext::default();
            root.lookup_child(locked, &current_task, &mut context, "lib".into()).unwrap();

            let mut context = LookupContext::default();
            let _test_file = root
                .lookup_child(
                    locked,
                    &current_task,
                    &mut context,
                    "data/tests/hello_starnix".into(),
                )
                .unwrap()
                .open(locked, &current_task, OpenFlags::RDONLY, AccessCheck::default())
                .unwrap();
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_blocking_io() {
        spawn_kernel_and_run(async |locked, current_task| {
            let (client, server) = zx::Socket::create_stream();
            let pipe = create_fuchsia_pipe(locked, &current_task, client, OpenFlags::RDWR).unwrap();

            let bytes = [0u8; 64];
            assert_eq!(bytes.len(), server.write(&bytes).unwrap());

            // Spawn a kthread to get the right lock context.
            let bytes_read =
                pipe.read(locked, &current_task, &mut VecOutputBuffer::new(64)).unwrap();

            assert_eq!(bytes_read, bytes.len());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_poll() {
        spawn_kernel_and_run(async |locked, current_task| {
            let (client, server) = zx::Socket::create_stream();
            let pipe = create_fuchsia_pipe(locked, &current_task, client, OpenFlags::RDWR)
                .expect("create_fuchsia_pipe");
            let server_zxio = Zxio::create(server.into_handle()).expect("Zxio::create");

            assert_eq!(
                pipe.query_events(locked, &current_task),
                Ok(FdEvents::POLLOUT | FdEvents::POLLWRNORM)
            );

            let epoll_object = EpollFileObject::new_file(locked, &current_task);
            let epoll_file = epoll_object.downcast_file::<EpollFileObject>().unwrap();
            let event = EpollEvent::new(FdEvents::POLLIN, 0);
            epoll_file
                .add(locked, &current_task, &pipe, &epoll_object, event)
                .expect("poll_file.add");

            let fds = epoll_file
                .wait(locked, &current_task, 1, zx::MonotonicInstant::ZERO)
                .expect("wait");
            assert!(fds.is_empty());

            assert_eq!(server_zxio.write(&[0]).expect("write"), 1);

            assert_eq!(
                pipe.query_events(locked, &current_task),
                Ok(FdEvents::POLLOUT
                    | FdEvents::POLLWRNORM
                    | FdEvents::POLLIN
                    | FdEvents::POLLRDNORM)
            );
            let fds = epoll_file
                .wait(locked, &current_task, 1, zx::MonotonicInstant::ZERO)
                .expect("wait");
            assert_eq!(fds.len(), 1);

            assert_eq!(
                pipe.read(locked, &current_task, &mut VecOutputBuffer::new(64)).expect("read"),
                1
            );

            assert_eq!(
                pipe.query_events(locked, &current_task),
                Ok(FdEvents::POLLOUT | FdEvents::POLLWRNORM)
            );
            let fds = epoll_file
                .wait(locked, &current_task, 1, zx::MonotonicInstant::ZERO)
                .expect("wait");
            assert!(fds.is_empty());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_new_remote_directory() {
        spawn_kernel_and_run(async |locked, current_task| {
            let (server, client) = zx::Channel::create();
            fdio::open("/pkg", fio::PERM_READABLE | fio::PERM_EXECUTABLE, server)
                .expect("failed to open /pkg");

            let fd = new_remote_file(locked, &current_task, client.into(), OpenFlags::RDWR)
                .expect("new_remote_file");
            assert!(fd.node().is_dir());
            assert!(fd.to_handle(&current_task).expect("to_handle").is_some());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_new_remote_file() {
        spawn_kernel_and_run(async |locked, current_task| {
            let (server, client) = zx::Channel::create();
            fdio::open("/pkg/meta/contents", fio::PERM_READABLE, server)
                .expect("failed to open /pkg/meta/contents");

            let fd = new_remote_file(locked, &current_task, client.into(), OpenFlags::RDONLY)
                .expect("new_remote_file");
            assert!(!fd.node().is_dir());
            assert!(fd.to_handle(&current_task).expect("to_handle").is_some());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_new_remote_counter() {
        spawn_kernel_and_run(async |locked, current_task| {
            let counter = zx::Counter::create();

            let fd = new_remote_file(locked, &current_task, counter.into(), OpenFlags::RDONLY)
                .expect("new_remote_file");
            assert!(fd.to_handle(&current_task).expect("to_handle").is_some());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_new_remote_vmo() {
        spawn_kernel_and_run(async |locked, current_task| {
            let vmo = zx::Vmo::create(*PAGE_SIZE).expect("Vmo::create");
            let fd = new_remote_file(locked, &current_task, vmo.into(), OpenFlags::RDWR)
                .expect("new_remote_file");
            assert!(!fd.node().is_dir());
            assert!(fd.to_handle(&current_task).expect("to_handle").is_some());
        })
        .await;
    }

    #[::fuchsia::test(threads = 2)]
    async fn test_symlink() {
        let fixture = TestFixture::new().await;
        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed");

        const LINK_PATH: &'static str = "symlink";
        const LINK_TARGET: &'static str = "私は「UTF8」です";
        // We expect the reported size of the symlink to be the length of the target, in bytes,
        // *without* a null terminator. Most Linux systems assume UTF-8 encoding.
        const LINK_SIZE: usize = 22;
        assert_eq!(LINK_SIZE, LINK_TARGET.len());

        spawn_kernel_and_run(async move |locked, current_task| {
            let kernel = current_task.kernel();
            let fs = RemoteFs::new_fs(
                locked,
                &kernel,
                client,
                FileSystemOptions { source: FlyByteStr::new(b"/"), ..Default::default() },
                fio::PERM_READABLE | fio::PERM_WRITABLE,
            )
            .expect("new_fs failed");
            let ns = Namespace::new(fs);
            let root = ns.root();
            let symlink_node = root
                .create_symlink(locked, &current_task, LINK_PATH.into(), LINK_TARGET.into())
                .expect("symlink failed");
            assert_matches!(&*symlink_node.entry.node.info(), FsNodeInfo { size: LINK_SIZE, .. });

            let mut context = LookupContext::new(SymlinkMode::NoFollow);
            let child = root
                .lookup_child(locked, &current_task, &mut context, "symlink".into())
                .expect("lookup_child failed");

            match child.readlink(locked, &current_task).expect("readlink failed") {
                SymlinkTarget::Path(path) => assert_eq!(path, LINK_TARGET),
                SymlinkTarget::Node(_) => panic!("readlink returned SymlinkTarget::Node"),
            }
            // Ensure the size stat reports matches what is expected.
            let stat_result = child.entry.node.stat(locked, &current_task).expect("stat failed");
            assert_eq!(stat_result.st_size as usize, LINK_SIZE);
        })
        .await;

        // Simulate a second run to ensure the symlink was persisted correctly.
        let fixture = TestFixture::open(
            fixture.close().await,
            TestFixtureOptions { format: false, ..Default::default() },
        )
        .await;
        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed after remount");

        spawn_kernel_and_run(async move |locked, current_task| {
            let kernel = current_task.kernel();
            let fs = RemoteFs::new_fs(
                locked,
                &kernel,
                client,
                FileSystemOptions { source: FlyByteStr::new(b"/"), ..Default::default() },
                fio::PERM_READABLE | fio::PERM_WRITABLE,
            )
            .expect("new_fs failed after remount");
            let ns = Namespace::new(fs);
            let root = ns.root();
            let mut context = LookupContext::new(SymlinkMode::NoFollow);
            let child = root
                .lookup_child(locked, &current_task, &mut context, "symlink".into())
                .expect("lookup_child failed after remount");

            match child.readlink(locked, &current_task).expect("readlink failed after remount") {
                SymlinkTarget::Path(path) => assert_eq!(path, LINK_TARGET),
                SymlinkTarget::Node(_) => {
                    panic!("readlink returned SymlinkTarget::Node after remount")
                }
            }
            // Ensure the size stat reports matches what is expected.
            let stat_result =
                child.entry.node.stat(locked, &current_task).expect("stat failed after remount");
            assert_eq!(stat_result.st_size as usize, LINK_SIZE);
        })
        .await;

        fixture.close().await;
    }

    #[::fuchsia::test]
    async fn test_mode_uid_gid_and_dev_persists() {
        const FILE_MODE: FileMode = mode!(IFREG, 0o467);
        const DIR_MODE: FileMode = mode!(IFDIR, 0o647);
        const BLK_MODE: FileMode = mode!(IFBLK, 0o746);

        let fixture = TestFixture::new().await;
        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed");

        // Simulate a first run of starnix.
        spawn_kernel_and_run(async move |locked, current_task| {
            let kernel = current_task.kernel();
            let creds = Credentials::clone(&current_task.current_creds());
            current_task.set_creds(Credentials { euid: 1, fsuid: 1, egid: 2, fsgid: 2, ..creds });
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
                locked,
                &kernel,
                client,
                FileSystemOptions { source: FlyByteStr::new(b"/"), ..Default::default() },
                rights,
            )
            .expect("new_fs failed");
            let ns = Namespace::new(fs);
            current_task.fs().set_umask(FileMode::from_bits(0));
            ns.root()
                .create_node(locked, &current_task, "file".into(), FILE_MODE, DeviceType::NONE)
                .expect("create_node failed");
            ns.root()
                .create_node(locked, &current_task, "dir".into(), DIR_MODE, DeviceType::NONE)
                .expect("create_node failed");
            ns.root()
                .create_node(locked, &current_task, "dev".into(), BLK_MODE, DeviceType::RANDOM)
                .expect("create_node failed");
        })
        .await;

        // Simulate a second run.
        let fixture = TestFixture::open(
            fixture.close().await,
            TestFixtureOptions { format: false, ..Default::default() },
        )
        .await;

        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed");

        spawn_kernel_and_run(async move |locked, current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
                locked,
                &kernel,
                client,
                FileSystemOptions { source: FlyByteStr::new(b"/"), ..Default::default() },
                rights,
            )
            .expect("new_fs failed");
            let ns = Namespace::new(fs);
            let mut context = LookupContext::new(SymlinkMode::NoFollow);
            let child = ns
                .root()
                .lookup_child(locked, &current_task, &mut context, "file".into())
                .expect("lookup_child failed");
            assert_matches!(
                &*child.entry.node.info(),
                FsNodeInfo { mode: FILE_MODE, uid: 1, gid: 2, rdev: DeviceType::NONE, .. }
            );
            let child = ns
                .root()
                .lookup_child(locked, &current_task, &mut context, "dir".into())
                .expect("lookup_child failed");
            assert_matches!(
                &*child.entry.node.info(),
                FsNodeInfo { mode: DIR_MODE, uid: 1, gid: 2, rdev: DeviceType::NONE, .. }
            );
            let child = ns
                .root()
                .lookup_child(locked, &current_task, &mut context, "dev".into())
                .expect("lookup_child failed");
            assert_matches!(
                &*child.entry.node.info(),
                FsNodeInfo { mode: BLK_MODE, uid: 1, gid: 2, rdev: DeviceType::RANDOM, .. }
            );
        })
        .await;
        fixture.close().await;
    }

    #[::fuchsia::test]
    async fn test_dot_dot_inode_numbers() {
        let fixture = TestFixture::new().await;
        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed");

        const MODE: FileMode = FileMode::from_bits(FileMode::IFDIR.bits() | 0o777);

        spawn_kernel_and_run(async |locked, current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
                locked,
                &kernel,
                client,
                FileSystemOptions { source: FlyByteStr::new(b"/"), ..Default::default() },
                rights,
            )
            .expect("new_fs failed");
            let ns = Namespace::new(fs);
            current_task.fs().set_umask(FileMode::from_bits(0));
            let sub_dir1 = ns
                .root()
                .create_node(locked, &current_task, "dir".into(), MODE, DeviceType::NONE)
                .expect("create_node failed");
            let sub_dir2 = sub_dir1
                .create_node(locked, &current_task, "dir".into(), MODE, DeviceType::NONE)
                .expect("create_node failed");

            let dir_handle = ns
                .root()
                .entry
                .open_anonymous(locked, &current_task, OpenFlags::RDONLY)
                .expect("open failed");

            #[derive(Default)]
            struct Sink {
                offset: off_t,
                dot_dot_inode_num: u64,
            }
            impl DirentSink for Sink {
                fn add(
                    &mut self,
                    inode_num: ino_t,
                    offset: off_t,
                    entry_type: DirectoryEntryType,
                    name: &FsStr,
                ) -> Result<(), Errno> {
                    if name == ".." {
                        self.dot_dot_inode_num = inode_num;
                        assert_eq!(entry_type, DirectoryEntryType::DIR);
                    }
                    self.offset = offset;
                    Ok(())
                }
                fn offset(&self) -> off_t {
                    self.offset
                }
            }
            let mut sink = Sink::default();
            dir_handle.readdir(locked, &current_task, &mut sink).expect("readdir failed");

            // inode_num for .. for the root should be the same as root.
            assert_eq!(sink.dot_dot_inode_num, ns.root().entry.node.ino);

            let dir_handle = sub_dir1
                .entry
                .open_anonymous(locked, &current_task, OpenFlags::RDONLY)
                .expect("open failed");
            let mut sink = Sink::default();
            dir_handle.readdir(locked, &current_task, &mut sink).expect("readdir failed");

            // inode_num for .. for the first sub directory should be the same as root.
            assert_eq!(sink.dot_dot_inode_num, ns.root().entry.node.ino);

            let dir_handle = sub_dir2
                .entry
                .open_anonymous(locked, &current_task, OpenFlags::RDONLY)
                .expect("open failed");
            let mut sink = Sink::default();
            dir_handle.readdir(locked, &current_task, &mut sink).expect("readdir failed");

            // inode_num for .. for the second subdir should be the first subdir.
            assert_eq!(sink.dot_dot_inode_num, sub_dir1.entry.node.ino);
        })
        .await;
        fixture.close().await;
    }

    #[::fuchsia::test]
    async fn test_remote_special_node() {
        let fixture = TestFixture::new().await;
        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed");

        const FIFO_MODE: FileMode = FileMode::from_bits(FileMode::IFIFO.bits() | 0o777);
        const REG_MODE: FileMode = FileMode::from_bits(FileMode::IFREG.bits());

        spawn_kernel_and_run(async |locked, current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
                locked,
                &kernel,
                client,
                FileSystemOptions { source: FlyByteStr::new(b"/"), ..Default::default() },
                rights,
            )
            .expect("new_fs failed");
            let ns = Namespace::new(fs);
            current_task.fs().set_umask(FileMode::from_bits(0));
            let root = ns.root();

            // Create RemoteSpecialNode (e.g. FIFO)
            root.create_node(locked, &current_task, "fifo".into(), FIFO_MODE, DeviceType::NONE)
                .expect("create_node failed");
            let mut context = LookupContext::new(SymlinkMode::NoFollow);
            let fifo_node = root
                .lookup_child(locked, &current_task, &mut context, "fifo".into())
                .expect("lookup_child failed");

            // Test that we get expected behaviour for RemoteSpecialNode operation, e.g.
            // test that truncate should return EINVAL
            match fifo_node.truncate(locked, &current_task, 0) {
                Ok(_) => {
                    panic!("truncate passed for special node")
                }
                Err(errno) if errno == EINVAL => {}
                Err(e) => {
                    panic!("truncate failed with error {:?}", e)
                }
            };

            // Create regular RemoteNode
            root.create_node(locked, &current_task, "file".into(), REG_MODE, DeviceType::NONE)
                .expect("create_node failed");
            let mut context = LookupContext::new(SymlinkMode::NoFollow);
            let reg_node = root
                .lookup_child(locked, &current_task, &mut context, "file".into())
                .expect("lookup_child failed");

            // We should be able to perform truncate on regular files
            reg_node.truncate(locked, &current_task, 0).expect("truncate failed");
        })
        .await;
        fixture.close().await;
    }

    #[::fuchsia::test]
    async fn test_hard_link() {
        let fixture = TestFixture::new().await;
        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed");

        spawn_kernel_and_run(async move |locked, current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
                locked,
                &kernel,
                client,
                FileSystemOptions { source: FlyByteStr::new(b"/"), ..Default::default() },
                rights,
            )
            .expect("new_fs failed");
            let ns = Namespace::new(fs);
            current_task.fs().set_umask(FileMode::from_bits(0));
            let node = ns
                .root()
                .create_node(
                    locked,
                    &current_task,
                    "file1".into(),
                    mode!(IFREG, 0o666),
                    DeviceType::NONE,
                )
                .expect("create_node failed");
            ns.root()
                .entry
                .node
                .link(locked, &current_task, &ns.root().mount, "file2".into(), &node.entry.node)
                .expect("link failed");
        })
        .await;

        let fixture = TestFixture::open(
            fixture.close().await,
            TestFixtureOptions { format: false, ..Default::default() },
        )
        .await;

        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed");

        spawn_kernel_and_run(async move |locked, current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
                locked,
                &kernel,
                client,
                FileSystemOptions { source: FlyByteStr::new(b"/"), ..Default::default() },
                rights,
            )
            .expect("new_fs failed");
            let ns = Namespace::new(fs);
            let mut context = LookupContext::new(SymlinkMode::NoFollow);
            let child1 = ns
                .root()
                .lookup_child(locked, &current_task, &mut context, "file1".into())
                .expect("lookup_child failed");
            let child2 = ns
                .root()
                .lookup_child(locked, &current_task, &mut context, "file2".into())
                .expect("lookup_child failed");
            assert!(Arc::ptr_eq(&child1.entry.node, &child2.entry.node));
        })
        .await;
        fixture.close().await;
    }

    #[::fuchsia::test]
    async fn test_lookup_on_fsverity_enabled_file() {
        let fixture = TestFixture::new().await;
        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed");

        const MODE: FileMode = FileMode::from_bits(FileMode::IFREG.bits() | 0o467);

        spawn_kernel_and_run(async move |locked, current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
                locked,
                &kernel,
                client,
                FileSystemOptions { source: FlyByteStr::new(b"/"), ..Default::default() },
                rights,
            )
            .expect("new_fs failed");
            let ns = Namespace::new(fs);
            current_task.fs().set_umask(FileMode::from_bits(0));
            let file = ns
                .root()
                .create_node(locked, &current_task, "file".into(), MODE, DeviceType::NONE)
                .expect("create_node failed");
            // Enable verity on the file.
            let desc = fsverity_descriptor {
                version: 1,
                hash_algorithm: 1,
                salt_size: 32,
                log_blocksize: 12,
                ..Default::default()
            };
            file.entry.node.ops().enable_fsverity(&desc).expect("enable fsverity failed");
        })
        .await;

        // Tear down the kernel and open the file again. The file should no longer be cached.
        // Test that lookup works as expected for an fsverity-enabled file.
        let fixture = TestFixture::open(
            fixture.close().await,
            TestFixtureOptions { format: false, ..Default::default() },
        )
        .await;
        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed");

        spawn_kernel_and_run(async move |locked, current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
                locked,
                &kernel,
                client,
                FileSystemOptions { source: FlyByteStr::new(b"/"), ..Default::default() },
                rights,
            )
            .expect("new_fs failed");
            let ns = Namespace::new(fs);
            let mut context = LookupContext::new(SymlinkMode::NoFollow);
            let _child = ns
                .root()
                .lookup_child(locked, &current_task, &mut context, "file".into())
                .expect("lookup_child failed");
        })
        .await;
        fixture.close().await;
    }

    #[::fuchsia::test]
    async fn test_update_attributes_persists() {
        let fixture = TestFixture::new().await;
        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed");

        const MODE: FileMode = FileMode::from_bits(FileMode::IFREG.bits() | 0o467);

        spawn_kernel_and_run(async move |locked, current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
                locked,
                &kernel,
                client,
                FileSystemOptions { source: FlyByteStr::new(b"/"), ..Default::default() },
                rights,
            )
            .expect("new_fs failed");
            let ns = Namespace::new(fs);
            current_task.fs().set_umask(FileMode::from_bits(0));
            let file = ns
                .root()
                .create_node(locked, &current_task, "file".into(), MODE, DeviceType::NONE)
                .expect("create_node failed");
            // Change the mode, this change should persist
            file.entry
                .node
                .chmod(locked, &current_task, &file.mount, MODE | FileMode::ALLOW_ALL)
                .expect("chmod failed");
        })
        .await;

        // Tear down the kernel and open the file again. Check that changes persisted.
        let fixture = TestFixture::open(
            fixture.close().await,
            TestFixtureOptions { format: false, ..Default::default() },
        )
        .await;
        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed");

        spawn_kernel_and_run(async move |locked, current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
                locked,
                &kernel,
                client,
                FileSystemOptions { source: FlyByteStr::new(b"/"), ..Default::default() },
                rights,
            )
            .expect("new_fs failed");
            let ns = Namespace::new(fs);
            let mut context = LookupContext::new(SymlinkMode::NoFollow);
            let child = ns
                .root()
                .lookup_child(locked, &current_task, &mut context, "file".into())
                .expect("lookup_child failed");
            assert_eq!(child.entry.node.info().mode, MODE | FileMode::ALLOW_ALL);
        })
        .await;
        fixture.close().await;
    }

    #[::fuchsia::test]
    async fn test_statfs() {
        let fixture = TestFixture::new().await;
        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed");

        spawn_kernel_and_run(async move |locked, current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
                locked,
                &kernel,
                client,
                FileSystemOptions { source: FlyByteStr::new(b"/"), ..Default::default() },
                rights,
            )
            .expect("new_fs failed");

            let statfs = fs.statfs(locked, &current_task).expect("statfs failed");
            assert!(statfs.f_type != 0);
            assert!(statfs.f_bsize > 0);
            assert!(statfs.f_blocks > 0);
            assert!(statfs.f_bfree > 0 && statfs.f_bfree <= statfs.f_blocks);
            assert!(statfs.f_files > 0);
            assert!(statfs.f_ffree > 0 && statfs.f_ffree <= statfs.f_files);
            assert!(statfs.f_fsid.val[0] != 0 || statfs.f_fsid.val[1] != 0);
            assert!(statfs.f_namelen > 0);
            assert!(statfs.f_frsize > 0);
        })
        .await;

        fixture.close().await;
    }

    #[::fuchsia::test]
    async fn test_allocate() {
        let fixture = TestFixture::new().await;
        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed");

        spawn_kernel_and_run(async move |locked, current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
                locked,
                &kernel,
                client,
                FileSystemOptions { source: FlyByteStr::new(b"/"), ..Default::default() },
                rights,
            )
            .expect("new_fs failed");
            let ns = Namespace::new(fs);
            current_task.fs().set_umask(FileMode::from_bits(0));
            let root = ns.root();

            const REG_MODE: FileMode = FileMode::from_bits(FileMode::IFREG.bits());
            root.create_node(locked, &current_task, "file".into(), REG_MODE, DeviceType::NONE)
                .expect("create_node failed");
            let mut context = LookupContext::new(SymlinkMode::NoFollow);
            let reg_node = root
                .lookup_child(locked, &current_task, &mut context, "file".into())
                .expect("lookup_child failed");

            reg_node
                .entry
                .node
                .fallocate(locked, &current_task, FallocMode::Allocate { keep_size: false }, 0, 20)
                .expect("truncate failed");
        })
        .await;
        fixture.close().await;
    }

    #[::fuchsia::test]
    async fn test_allocate_overflow() {
        let fixture = TestFixture::new().await;
        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed");

        spawn_kernel_and_run(async move |locked, current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
                locked,
                &kernel,
                client,
                FileSystemOptions { source: FlyByteStr::new(b"/"), ..Default::default() },
                rights,
            )
            .expect("new_fs failed");
            let ns = Namespace::new(fs);
            current_task.fs().set_umask(FileMode::from_bits(0));
            let root = ns.root();

            const REG_MODE: FileMode = FileMode::from_bits(FileMode::IFREG.bits());
            root.create_node(locked, &current_task, "file".into(), REG_MODE, DeviceType::NONE)
                .expect("create_node failed");
            let mut context = LookupContext::new(SymlinkMode::NoFollow);
            let reg_node = root
                .lookup_child(locked, &current_task, &mut context, "file".into())
                .expect("lookup_child failed");

            reg_node
                .entry
                .node
                .fallocate(
                    locked,
                    &current_task,
                    FallocMode::Allocate { keep_size: false },
                    1,
                    u64::MAX,
                )
                .expect_err("truncate unexpectedly passed");
        })
        .await;
        fixture.close().await;
    }

    #[::fuchsia::test]
    async fn test_time_modify_persists() {
        let fixture = TestFixture::new().await;
        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed");

        const MODE: FileMode = FileMode::from_bits(FileMode::IFREG.bits() | 0o467);

        let last_modified = spawn_kernel_and_run(async move |locked, current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
                locked,
                &kernel,
                client,
                FileSystemOptions { source: FlyByteStr::new(b"/"), ..Default::default() },
                rights,
            )
            .expect("new_fs failed");
            let ns: Arc<Namespace> = Namespace::new(fs);
            current_task.fs().set_umask(FileMode::from_bits(0));
            let child = ns
                .root()
                .create_node(locked, &current_task, "file".into(), MODE, DeviceType::NONE)
                .expect("create_node failed");
            // Write to file (this should update mtime (time_modify))
            let file = child
                .open(locked, &current_task, OpenFlags::RDWR, AccessCheck::default())
                .expect("open failed");
            // Call `fetch_and_refresh_info(..)` to refresh `time_modify` with the time managed by the
            // underlying filesystem
            let time_before_write = child
                .entry
                .node
                .fetch_and_refresh_info(locked, &current_task)
                .expect("fetch_and_refresh_info failed")
                .time_modify;
            let write_bytes: [u8; 5] = [1, 2, 3, 4, 5];
            let written = file
                .write(locked, &current_task, &mut VecInputBuffer::new(&write_bytes))
                .expect("write failed");
            assert_eq!(written, write_bytes.len());
            let last_modified = child
                .entry
                .node
                .fetch_and_refresh_info(locked, &current_task)
                .expect("fetch_and_refresh_info failed")
                .time_modify;
            assert!(last_modified > time_before_write);
            last_modified
        })
        .await;

        // Tear down the kernel and open the file again. Check that modification time is when we
        // last modified the contents of the file
        let fixture = TestFixture::open(
            fixture.close().await,
            TestFixtureOptions { format: false, ..Default::default() },
        )
        .await;
        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed");
        let refreshed_modified_time = spawn_kernel_and_run(async move |locked, current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
                locked,
                &kernel,
                client,
                FileSystemOptions { source: FlyByteStr::new(b"/"), ..Default::default() },
                rights,
            )
            .expect("new_fs failed");
            let ns = Namespace::new(fs);
            let mut context = LookupContext::new(SymlinkMode::NoFollow);
            let child = ns
                .root()
                .lookup_child(locked, &current_task, &mut context, "file".into())
                .expect("lookup_child failed");
            let last_modified = child
                .entry
                .node
                .fetch_and_refresh_info(locked, &current_task)
                .expect("fetch_and_refresh_info failed")
                .time_modify;
            last_modified
        })
        .await;
        assert_eq!(last_modified, refreshed_modified_time);

        fixture.close().await;
    }

    #[::fuchsia::test]
    async fn test_update_atime_mtime() {
        let fixture = TestFixture::new().await;
        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed");

        const MODE: FileMode = FileMode::from_bits(FileMode::IFREG.bits() | 0o467);

        spawn_kernel_and_run(async move |locked, current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
                locked,
                &kernel,
                client,
                FileSystemOptions { source: FlyByteStr::new(b"/"), ..Default::default() },
                rights,
            )
            .expect("new_fs failed");
            let ns: Arc<Namespace> = Namespace::new(fs);
            current_task.fs().set_umask(FileMode::from_bits(0));
            let child = ns
                .root()
                .create_node(locked, &current_task, "file".into(), MODE, DeviceType::NONE)
                .expect("create_node failed");

            let info_original = child
                .entry
                .node
                .fetch_and_refresh_info(locked, &current_task)
                .expect("fetch_and_refresh_info failed")
                .clone();

            child
                .entry
                .node
                .update_atime_mtime(
                    locked,
                    &current_task,
                    &child.mount,
                    TimeUpdateType::Time(UtcInstant::from_nanos(30)),
                    TimeUpdateType::Omit,
                )
                .expect("update_atime_mtime failed");
            let info_after_update = child
                .entry
                .node
                .fetch_and_refresh_info(locked, &current_task)
                .expect("fetch_and_refresh_info failed")
                .clone();
            assert_eq!(info_after_update.time_modify, info_original.time_modify);
            assert_eq!(info_after_update.time_access, UtcInstant::from_nanos(30));

            child
                .entry
                .node
                .update_atime_mtime(
                    locked,
                    &current_task,
                    &child.mount,
                    TimeUpdateType::Omit,
                    TimeUpdateType::Time(UtcInstant::from_nanos(50)),
                )
                .expect("update_atime_mtime failed");
            let info_after_update2 = child
                .entry
                .node
                .fetch_and_refresh_info(locked, &current_task)
                .expect("fetch_and_refresh_info failed")
                .clone();
            assert_eq!(info_after_update2.time_modify, UtcInstant::from_nanos(50));
            assert_eq!(info_after_update2.time_access, UtcInstant::from_nanos(30));
        })
        .await;
        fixture.close().await;
    }

    #[::fuchsia::test]
    async fn test_write_updates_mtime_ctime() {
        let fixture = TestFixture::new().await;
        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed");

        const MODE: FileMode = FileMode::from_bits(FileMode::IFREG.bits() | 0o467);

        spawn_kernel_and_run(async move |locked, current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
                locked,
                &kernel,
                client,
                FileSystemOptions { source: FlyByteStr::new(b"/"), ..Default::default() },
                rights,
            )
            .expect("new_fs failed");
            let ns: Arc<Namespace> = Namespace::new(fs);
            current_task.fs().set_umask(FileMode::from_bits(0));
            let child = ns
                .root()
                .create_node(locked, &current_task, "file".into(), MODE, DeviceType::NONE)
                .expect("create_node failed");
            let file = child
                .open(locked, &current_task, OpenFlags::RDWR, AccessCheck::default())
                .expect("open failed");
            // Call `fetch_and_refresh_info(..)` to refresh ctime and mtime with the time managed by the
            // underlying filesystem
            let (ctime_before_write, mtime_before_write) = {
                let info = child
                    .entry
                    .node
                    .fetch_and_refresh_info(locked, &current_task)
                    .expect("fetch_and_refresh_info failed");
                (info.time_status_change, info.time_modify)
            };

            // Writing to a file should update ctime and mtime
            let write_bytes: [u8; 5] = [1, 2, 3, 4, 5];
            let written = file
                .write(locked, &current_task, &mut VecInputBuffer::new(&write_bytes))
                .expect("write failed");
            assert_eq!(written, write_bytes.len());

            // As Fxfs, the underlying filesystem in this test, can manage file timestamps,
            // we should not see an update in mtime and ctime without first refreshing the node with
            // the metadata from Fxfs.
            let (ctime_after_write_no_refresh, mtime_after_write_no_refresh) = {
                let info = child.entry.node.info();
                (info.time_status_change, info.time_modify)
            };
            assert_eq!(ctime_after_write_no_refresh, ctime_before_write);
            assert_eq!(mtime_after_write_no_refresh, mtime_before_write);

            // Refresh information, we should see `info` with mtime and ctime from the remote
            // filesystem (assume this is true if the new timestamp values are greater than the ones
            // without the refresh).
            let (ctime_after_write_refresh, mtime_after_write_refresh) = {
                let info = child
                    .entry
                    .node
                    .fetch_and_refresh_info(locked, &current_task)
                    .expect("fetch_and_refresh_info failed");
                (info.time_status_change, info.time_modify)
            };
            assert_eq!(ctime_after_write_refresh, mtime_after_write_refresh);
            assert!(ctime_after_write_refresh > ctime_after_write_no_refresh);
        })
        .await;
        fixture.close().await;
    }

    #[::fuchsia::test]
    async fn test_casefold_persists() {
        let fixture = TestFixture::new().await;
        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed");

        spawn_kernel_and_run(async move |locked, current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
                locked,
                &kernel,
                client,
                FileSystemOptions { source: FlyByteStr::new(b"/"), ..Default::default() },
                rights,
            )
            .expect("new_fs failed");
            let ns: Arc<Namespace> = Namespace::new(fs);
            let child = ns
                .root()
                .create_node(
                    locked,
                    &current_task,
                    "dir".into(),
                    FileMode::ALLOW_ALL.with_type(FileMode::IFDIR),
                    DeviceType::NONE,
                )
                .expect("create_node failed");
            child
                .entry
                .node
                .update_attributes(locked, &current_task, |info| {
                    info.casefold = true;
                    Ok(())
                })
                .expect("enable casefold")
        })
        .await;

        // Tear down the kernel and open the dir again. Check that casefold is preserved.
        let fixture = TestFixture::open(
            fixture.close().await,
            TestFixtureOptions { format: false, ..Default::default() },
        )
        .await;
        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed");
        let casefold = spawn_kernel_and_run(async move |locked, current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
                locked,
                &kernel,
                client,
                FileSystemOptions { source: FlyByteStr::new(b"/"), ..Default::default() },
                rights,
            )
            .expect("new_fs failed");
            let ns = Namespace::new(fs);
            let mut context = LookupContext::new(SymlinkMode::NoFollow);
            let child = ns
                .root()
                .lookup_child(locked, &current_task, &mut context, "dir".into())
                .expect("lookup_child failed");
            let casefold = child
                .entry
                .node
                .fetch_and_refresh_info(locked, &current_task)
                .expect("fetch_and_refresh_info failed")
                .casefold;
            casefold
        })
        .await;
        assert!(casefold);

        fixture.close().await;
    }

    #[::fuchsia::test]
    async fn test_update_time_access_persists() {
        const TEST_FILE: &str = "test_file";

        let fixture = TestFixture::new().await;
        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed");
        // Set up file.
        let info_after_read = spawn_kernel_and_run(async move |locked, current_task| {
            let kernel = current_task.kernel();
            let fs = RemoteFs::new_fs(
                locked,
                &kernel,
                client,
                FileSystemOptions {
                    source: FlyByteStr::new(b"/"),
                    flags: MountFlags::RELATIME,
                    ..Default::default()
                },
                fio::PERM_READABLE | fio::PERM_WRITABLE,
            )
            .expect("new_fs failed");
            let ns = Namespace::new_with_flags(fs, MountFlags::RELATIME);
            let child = ns
                .root()
                .open_create_node(
                    locked,
                    &current_task,
                    TEST_FILE.into(),
                    FileMode::ALLOW_ALL.with_type(FileMode::IFREG),
                    DeviceType::NONE,
                    OpenFlags::empty(),
                )
                .expect("create_node failed");

            let file_handle = child
                .open(locked, &current_task, OpenFlags::RDWR, AccessCheck::default())
                .expect("open failed");

            // Expect atime to be updated as this is the first file access since the
            // last file modification or status change.
            file_handle
                .read(locked, &current_task, &mut VecOutputBuffer::new(10))
                .expect("read failed");

            // Call `fetch_and_refresh_info` to persist atime update.
            let info_after_read = child
                .entry
                .node
                .fetch_and_refresh_info(locked, &current_task)
                .expect("fetch_and_refresh_info failed")
                .clone();

            info_after_read
        })
        .await;

        // Tear down the kernel and open the file again. The file should no longer be cached.
        let fixture = TestFixture::open(
            fixture.close().await,
            TestFixtureOptions { format: false, ..Default::default() },
        )
        .await;

        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed");

        spawn_kernel_and_run(async move |locked, current_task| {
            let kernel = current_task.kernel();
            let fs = RemoteFs::new_fs(
                locked,
                &kernel,
                client,
                FileSystemOptions {
                    source: FlyByteStr::new(b"/"),
                    flags: MountFlags::RELATIME,
                    ..Default::default()
                },
                fio::PERM_READABLE | fio::PERM_WRITABLE,
            )
            .expect("new_fs failed");
            let ns = Namespace::new_with_flags(fs, MountFlags::RELATIME);
            let mut context = LookupContext::new(SymlinkMode::NoFollow);
            let child = ns
                .root()
                .lookup_child(locked, &current_task, &mut context, TEST_FILE.into())
                .expect("lookup_child failed");

            // Get info - this should be refreshed with info that was persisted before
            // we tore down the kernel.
            let persisted_info = child
                .entry
                .node
                .fetch_and_refresh_info(locked, &current_task)
                .expect("fetch_and_refresh_info failed")
                .clone();
            assert_eq!(info_after_read.time_access, persisted_info.time_access);
        })
        .await;
        fixture.close().await;
    }

    #[::fuchsia::test]
    async fn test_pending_access_time_updates() {
        const TEST_FILE: &str = "test_file";

        let fixture = TestFixture::new().await;
        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed");

        spawn_kernel_and_run(async move |locked, current_task| {
            let kernel = current_task.kernel.clone();
            let fs = RemoteFs::new_fs(
                locked,
                &kernel,
                client,
                FileSystemOptions {
                    source: FlyByteStr::new(b"/"),
                    flags: MountFlags::RELATIME,
                    ..Default::default()
                },
                fio::PERM_READABLE | fio::PERM_WRITABLE,
            )
            .expect("new_fs failed");

            let ns = Namespace::new_with_flags(fs, MountFlags::RELATIME);
            let child = ns
                .root()
                .open_create_node(
                    locked,
                    &current_task,
                    TEST_FILE.into(),
                    FileMode::ALLOW_ALL.with_type(FileMode::IFREG),
                    DeviceType::NONE,
                    OpenFlags::empty(),
                )
                .expect("create_node failed");

            let file_handle = child
                .open(locked, &current_task, OpenFlags::RDWR, AccessCheck::default())
                .expect("open failed");

            // Expect atime to be updated as this is the first file access since the last
            // file modification or status change.
            file_handle
                .read(locked, &current_task, &mut VecOutputBuffer::new(10))
                .expect("read failed");

            let atime_after_first_read = child
                .entry
                .node
                .fetch_and_refresh_info(locked, &current_task)
                .expect("fetch_and_refresh_info failed")
                .time_access;

            // Read again (this read will not trigger a persistent atime update if
            // filesystem was mounted with atime)
            file_handle
                .read(locked, &current_task, &mut VecOutputBuffer::new(10))
                .expect("read failed");

            let atime_after_second_read = child
                .entry
                .node
                .fetch_and_refresh_info(locked, &current_task)
                .expect("fetch_and_refresh_info failed")
                .time_access;
            assert_eq!(atime_after_first_read, atime_after_second_read);

            // Do another operation that will update ctime and/or mtime but not atime.
            let write_bytes: [u8; 5] = [1, 2, 3, 4, 5];
            let _written = file_handle
                .write(locked, &current_task, &mut VecInputBuffer::new(&write_bytes))
                .expect("write failed");

            // Read again (atime should be updated).
            file_handle
                .read(locked, &current_task, &mut VecOutputBuffer::new(10))
                .expect("read failed");

            assert!(
                atime_after_second_read
                    < child
                        .entry
                        .node
                        .fetch_and_refresh_info(locked, &current_task)
                        .expect("fetch_and_refresh_info failed")
                        .time_access
            );
        })
        .await;
        fixture.close().await;
    }

    #[::fuchsia::test]
    async fn test_read_at_chunking() {
        use futures::StreamExt;
        let (client, mut stream) = create_request_stream::<fio::FileMarker>();
        let content = vec![0xAB; (fio::MAX_TRANSFER_SIZE + 100) as usize];
        let content_clone = content.clone();

        let _server_task = fasync::Task::spawn(async move {
            while let Some(Ok(request)) = stream.next().await {
                match request {
                    fio::FileRequest::ReadAt { count, offset, responder } => {
                        let start = offset as usize;
                        let end = std::cmp::min(start + count as usize, content_clone.len());
                        let data = if start < content_clone.len() {
                            &content_clone[start..end]
                        } else {
                            &[]
                        };
                        responder.send(Ok(data)).unwrap();
                    }
                    _ => panic!("Unexpected request: {:?}", request),
                }
            }
        });

        fasync::unblock(move || {
            let io = RemoteIo::new(client.into_channel().into());
            let mut buffer = VecOutputBuffer::new(content.len());
            assert_eq!(io.read_at(0, &mut buffer).expect("read_at failed"), content.len());
            assert_eq!(buffer.data(), content.as_slice());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_write_at_chunking() {
        let (client, mut stream) = create_request_stream::<fio::FileMarker>();
        let content = vec![0xCD; (fio::MAX_TRANSFER_SIZE + 100) as usize];
        let content2 = content.clone();

        let server_task = fasync::Task::spawn(async move {
            let mut written = vec![0; content2.len()];
            while let Some(Ok(request)) = stream.next().await {
                match request {
                    fio::FileRequest::WriteAt { offset, data, responder, .. } => {
                        let offset = offset as usize;
                        written[offset..offset + data.len()].copy_from_slice(&data);
                        responder.send(Ok(data.len() as u64)).unwrap();
                    }
                    _ => panic!("Unexpected request: {:?}", request),
                }
            }
            assert_eq!(written, content2);
        });

        fasync::unblock(move || {
            let io = RemoteIo::new(client.into_channel().into());
            let mut buffer = VecInputBuffer::new(&content);
            assert_eq!(io.write_at(0, &mut buffer).expect("write_at failed"), content.len());
        })
        .await;

        server_task.await;
    }

    #[::fuchsia::test]
    async fn test_large_directory() {
        use futures::StreamExt;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (client, mut stream) = create_request_stream::<fio::DirectoryMarker>();
        let num_entries = 2000;
        let request_count = Arc::new(AtomicUsize::new(0));
        let request_count_clone = request_count.clone();

        fasync::Task::spawn(async move {
            let mut sent_count = 0;
            while let Some(Ok(request)) = stream.next().await {
                match request {
                    fio::DirectoryRequest::Query { responder } => {
                        let _ = responder.send(fio::DirectoryMarker::PROTOCOL_NAME.as_bytes());
                    }
                    fio::DirectoryRequest::GetAttributes { query: _, responder } => {
                        let mut mutable_attributes = fio::MutableNodeAttributes {
                            mode: Some(mode!(IFDIR, 0o777).bits()),
                            ..Default::default()
                        };
                        let mut immutable_attributes = fio::ImmutableNodeAttributes {
                            protocols: Some(fio::NodeProtocolKinds::DIRECTORY),
                            ..Default::default()
                        };
                        let _ = responder
                            .send(Ok((&mut mutable_attributes, &mut immutable_attributes)));
                    }
                    fio::DirectoryRequest::ReadDirents { max_bytes, responder } => {
                        request_count_clone.fetch_add(1, Ordering::Relaxed);
                        let mut buffer = Vec::new();
                        while sent_count < num_entries {
                            let name = format!("file_{}", sent_count);
                            let name_bytes = name.as_bytes();
                            let entry_size = 10 + name_bytes.len();
                            if buffer.len() + entry_size > max_bytes as usize {
                                break;
                            }
                            buffer.extend_from_slice(&(sent_count as u64 + 1).to_le_bytes());
                            buffer.push(name_bytes.len() as u8);
                            buffer.push(fio::DirentType::File.into_primitive());
                            buffer.extend_from_slice(name_bytes);
                            sent_count += 1;
                        }
                        let _ = responder.send(0, &buffer);
                    }
                    fio::DirectoryRequest::Rewind { responder } => {
                        sent_count = 0;
                        let _ = responder.send(0);
                    }
                    fio::DirectoryRequest::Close { responder } => {
                        let _ = responder.send(Ok(()));
                    }
                    _ => panic!("Unexpected request: {:?}", request),
                }
            }
        })
        .detach();

        spawn_kernel_and_run(async move |locked, current_task| {
            let file = new_remote_file(locked, &current_task, client.into(), OpenFlags::RDONLY)
                .expect("new_remote_file");
            #[derive(Default)]
            struct Sink {
                count: usize,
                offset: off_t,
            }
            impl DirentSink for Sink {
                fn add(
                    &mut self,
                    _inode_num: ino_t,
                    offset: off_t,
                    _entry_type: DirectoryEntryType,
                    _name: &FsStr,
                ) -> Result<(), Errno> {
                    self.count += 1;
                    self.offset = offset;
                    Ok(())
                }
                fn offset(&self) -> off_t {
                    self.offset
                }
            }
            let mut sink = Sink::default();
            file.readdir(locked, &current_task, &mut sink).expect("readdir");
            assert_eq!(sink.count, num_entries);
            assert!(request_count.load(Ordering::Relaxed) > 1);
        })
        .await;
    }
}
