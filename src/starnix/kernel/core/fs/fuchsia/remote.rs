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
use fidl::endpoints::DiscoverableProtocolMarker as _;
use fuchsia_runtime::UtcInstant;
use linux_uapi::SYNC_IOC_MAGIC;
use once_cell::sync::OnceCell;
use starnix_crypt::EncryptionKeyId;
use starnix_logging::{CATEGORY_STARNIX_MM, impossible_error, log_warn, trace_duration};
use starnix_sync::{
    FileOpsCore, LockEqualOrBefore, Locked, RwLock, RwLockReadGuard, RwLockWriteGuard, Unlocked,
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
    __kernel_fsid_t, errno, error, from_status_like_fdio, fsverity_descriptor, mode, off_t, statfs,
};
use std::ops::ControlFlow;
use std::sync::Arc;
use sync_io_client::{RemoteIo, create_with_on_representation};
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
use zx::{Counter, HandleBased as _};
use {
    fidl_fuchsia_io as fio, fidl_fuchsia_starnix_binder as fbinder,
    fidl_fuchsia_unknown as funknown,
};

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
    new_remotefs_in_root(locked, kernel, &root_proxy, subdir_options, open_rights)
}

/// Create a filesystem to access the content of the fuchsia directory available
/// at `options.source` inside `root`.
pub fn new_remotefs_in_root<L>(
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
            .map_err(map_sync_io_client_error)
    }

    fn sync(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _fs: &FileSystem,
        _current_task: &CurrentTask,
    ) -> Result<(), Errno> {
        self.root_proxy
            .sync(zx::MonotonicInstant::INFINITE)
            .map_err(|_| errno!(EIO))?
            .map_err(|status| map_sync_error(zx::Status::from_raw(status)))
    }

    fn manages_timestamps(&self) -> bool {
        true
    }
}

struct Factory {
    assume_special: bool,
}

impl sync_io_client::Factory for Factory {
    type Result = Box<dyn FsNodeOps>;

    fn create_node(self, io: RemoteIo) -> Self::Result {
        Box::new(RemoteNode::new(io))
    }

    fn create_directory(self, io: RemoteIo) -> Self::Result {
        Box::new(RemoteNode::new(io))
    }

    fn create_file(self, io: RemoteIo, info: &fio::FileInfo) -> Self::Result {
        if self.assume_special || is_special(info) {
            Box::new(RemoteSpecialNode { io })
        } else {
            Box::new(RemoteNode::new(io))
        }
    }

    fn create_symlink(self, io: RemoteIo, target: Vec<u8>) -> Self::Result {
        Box::new(RemoteSymlink::new(io, target))
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

        let (remote_node, attrs, _) =
            create_with_on_representation(client_end.into(), Factory { assume_special: false })
                .map_err(map_sync_io_client_error)?;
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
                let io = RemoteIo::with_stream(
                    file_proxy.into_channel().into(),
                    info.stream.unwrap_or_else(|| zx::NullableHandle::invalid().into()),
                );
                let attr = io
                    .attr_get_zxio(MODE_ATTRIBUTES | NODE_INFO_ATTRIBUTES)
                    .map_err(map_sync_io_client_error)?;
                return Ok((attr, Box::new(AnonymousRemoteFileObject::new(io))));
            }
            DIRECTORY_PROTOCOL => {
                let io = RemoteIo::new(queryable.into_channel().into());
                let attr = io
                    .attr_get_zxio(MODE_ATTRIBUTES | NODE_INFO_ATTRIBUTES)
                    .map_err(map_sync_io_client_error)?;
                return Ok((
                    attr,
                    Box::new(RemoteDirectoryObject::new(io.into_proxy().into_channel().into())),
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
    let attrs = io.attr_get_zxio(query).map_err(map_sync_io_client_error)?;
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
            .map_err(map_sync_io_client_error)
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
                        .map_err(map_sync_io_client_error)?,
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
                Factory { assume_special: !mode.is_reg() },
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
                Factory { assume_special: false },
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
            .open(name, fs_ops.root_rights, None, query, Factory { assume_special: false })
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
                Factory { assume_special: false },
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

struct RemoteDirectoryObject(sync_io_client::RemoteDirectory);

impl RemoteDirectoryObject {
    fn new(proxy: fio::DirectorySynchronousProxy) -> Self {
        Self(sync_io_client::RemoteDirectory::new(proxy))
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
        Ok(self
            .0
            .seek(default_seek(current_offset, target, || error!(EINVAL))? as u64)
            .map_err(map_sync_io_client_error)? as i64)
    }

    fn readdir(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        file: &FileObject,
        _current_task: &CurrentTask,
        sink: &mut dyn DirentSink,
    ) -> Result<(), Errno> {
        match self
            .0
            .readdir(|mut inode_num, entry_type, name| {
                if name == b".." {
                    inode_num = if let Some(parent) = file.name.parent_within_mount() {
                        parent.node.ino
                    } else {
                        // For the root .. should have the same inode number as .
                        file.name.entry.node.ino
                    };
                }
                let entry_type = match entry_type {
                    fio::DirentType::Directory => DirectoryEntryType::DIR,
                    fio::DirentType::File => DirectoryEntryType::REG,
                    fio::DirentType::Symlink => DirectoryEntryType::LNK,
                    _ => DirectoryEntryType::UNKNOWN,
                };
                match sink.add(inode_num, sink.offset() + 1, entry_type, name.into()) {
                    Ok(()) => ControlFlow::Continue(()),
                    Err(e) => ControlFlow::Break(e),
                }
            })
            .map_err(map_sync_io_client_error)?
        {
            None => Ok(()),
            Some(e) => Err(e),
        }
    }

    fn sync(&self, _file: &FileObject, _current_task: &CurrentTask) -> Result<(), Errno> {
        self.0.sync().map_err(map_sync_error)
    }

    fn to_handle(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
    ) -> Result<Option<zx::NullableHandle>, Errno> {
        self.0
            .clone_proxy()
            .map_err(map_sync_io_client_error)
            .map(|p| Some(p.into_channel().into()))
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

trait RemoteIoExt {
    fn read_to_output_buffer(
        &self,
        offset: u64,
        buffer: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno>;
    fn write_from_input_buffer(
        &self,
        offset: u64,
        buffer: &mut dyn InputBuffer,
    ) -> Result<usize, Errno>;
    fn fetch_remote_memory(&self, prot: ProtectionFlags) -> Result<Arc<MemoryObject>, Errno>;
}

impl RemoteIoExt for RemoteIo {
    fn read_to_output_buffer(
        &self,
        offset: u64,
        buffer: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        if self.supports_vectored()
            && let Some(actual) = with_iovec_segments(buffer, |iovecs| {
                // SAFETY: The iovecs are known to point to userspace, so any damage we do here is
                // limited to userspace.  Zircon will catch faults and return an error.
                unsafe { self.readv(offset, iovecs).map_err(map_stream_error) }
            })
        {
            let actual = actual?;
            // SAFETY: we successfully read `actual` bytes directly to the user's buffer
            // segments.
            unsafe { buffer.advance(actual) }?;
            Ok(actual)
        } else {
            self.read(
                offset,
                buffer.available(),
                |data| buffer.write(&data),
                map_sync_io_client_error,
            )
        }
    }

    fn write_from_input_buffer(
        &self,
        offset: u64,
        buffer: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        let actual = if self.supports_vectored()
            && let Some(actual) = with_iovec_segments(buffer, |iovecs| {
                self.writev(offset, iovecs).map_err(map_stream_error)
            }) {
            actual?
        } else {
            self.write(offset as u64, &buffer.peek_all()?).map_err(map_sync_io_client_error)?
        };
        buffer.advance(actual)?;
        Ok(actual)
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
        Self::io(file).read_to_output_buffer(offset as u64, data)
    }

    fn write(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        file: &FileObject,
        _current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        Self::io(file).write_from_input_buffer(offset as u64, data)
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
        Self::io(file)
            .clone_proxy()
            .map_err(map_sync_io_client_error)
            .map(|p| Some(p.into_channel().into()))
    }

    fn sync(&self, file: &FileObject, _current_task: &CurrentTask) -> Result<(), Errno> {
        Self::io(file).sync().map_err(map_sync_io_client_error)
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
        self.io.read_to_output_buffer(offset as u64, data)
    }

    fn write(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        self.io.write_from_input_buffer(offset as u64, data)
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
        self.io
            .clone_proxy()
            .map_err(map_sync_io_client_error)
            .map(|p| Some(p.into_channel().into()))
    }

    fn sync(&self, _file: &FileObject, _current_task: &CurrentTask) -> Result<(), Errno> {
        self.io.sync().map_err(map_sync_io_client_error)
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
        self.zxio.clone_handle().map(Some).map_err(|status| from_status_like_fdio!(status))
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

#[track_caller]
fn map_sync_io_client_error(status: zx::Status) -> Errno {
    from_status_like_fdio!(status)
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
    use starnix_uapi::ino_t;
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
    async fn test_read_chunking() {
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
            assert_eq!(
                io.read_to_output_buffer(0, &mut buffer).expect("read_at failed"),
                content.len()
            );
            assert_eq!(buffer.data(), content.as_slice());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_write_chunking() {
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
            assert_eq!(
                io.write_from_input_buffer(0, &mut buffer).expect("write_at failed"),
                content.len()
            );
        })
        .await;

        server_task.await;
    }
}
