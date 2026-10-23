// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fs::fuchsia::RemoteUnixDomainSocket;
use crate::fs::fuchsia::remote_volume::RemoteVolume;
use crate::fs::fuchsia::sync_file::{SyncFence, SyncFile, SyncPoint, Timeline};
use crate::fs::fuchsia::zxio::{zxio_query_events, zxio_wait_async};
use crate::mm::memory::MemoryObject;
use crate::mm::{ProtectionFlags, VMEX_RESOURCE};
use crate::security;
use crate::task::{CurrentTask, EventHandler, Kernel, WaitCanceler, Waiter};
use crate::vfs::buffers::{InputBuffer, OutputBuffer, with_iovec_segments};
use crate::vfs::file_server::serve_file_tagged;
use crate::vfs::fs_args::MountParams;
use crate::vfs::fsverity::FsVerityState;
use crate::vfs::pipe::new_pipe_file;
use crate::vfs::socket::{Socket, SocketFile, SocketHandle, ZxioBackedSocket};
use crate::vfs::{
    Anon, AppendLockWriteGuard, CacheMode, DEFAULT_BYTES_PER_BLOCK, DirEntry, DirectoryEntryType,
    DirentSink, FallocMode, FileHandle, FileObject, FileOps, FileSystem, FileSystemHandle,
    FileSystemOps, FileSystemOptions, FsNode, FsNodeFlags, FsNodeHandle, FsNodeInfo, FsNodeOps,
    FsStr, FsString, LookupVec, MemoryRegularFile, NamespaceNode, RenameContext, SeekTarget,
    StatxFlags, SymlinkTarget, XattrOp, XattrStorage, default_seek, fileops_impl_directory,
    fileops_impl_nonseekable, fileops_impl_noop_sync, fileops_impl_seekable, fs_node_impl_not_dir,
    fs_node_impl_symlink, fs_node_impl_xattr_delegate,
};
use bstr::ByteSlice;
use fidl::endpoints::DiscoverableProtocolMarker as _;
use fidl_fuchsia_io as fio;
use fidl_fuchsia_starnix_binder as fbinder;
use fidl_fuchsia_unknown as funknown;
use fuchsia_runtime::UtcInstant;
use linux_uapi::SYNC_IOC_MAGIC;
use once_cell::sync::OnceCell;
use smallvec::{SmallVec, smallvec};
use starnix_crypt::EncryptionKeyId;
use starnix_logging::{CATEGORY_STARNIX_MM, impossible_error, log_warn, track_stub};
use starnix_sync::{
    DynamicLockDepRwLock, FuchsiaRemoteTargetLock, LockDepReadGuard, LockDepRwLock,
    LockDepWriteGuard,
};
use starnix_syscalls::{SyscallArg, SyscallResult};
use starnix_types::vfs::default_statfs;
use starnix_uapi::auth::{Credentials, FsCred};
use starnix_uapi::device_id::DeviceId;
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::FileMode;
use starnix_uapi::mount_flags::FileSystemFlags;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::vfs::FdEvents;
use starnix_uapi::{
    __kernel_fsid_t, XATTR_NAME_SELINUX, errno, error, from_status_like_fdio, fsverity_descriptor,
    mode, off_t, statfs,
};
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, LazyLock};
use sync_io_client::{RemoteDirectory, RemoteIo, SyncIo, SyncIoDirectory};
use syncio::zxio::{
    ZXIO_OBJECT_TYPE_DATAGRAM_SOCKET, ZXIO_OBJECT_TYPE_NONE, ZXIO_OBJECT_TYPE_PACKET_SOCKET,
    ZXIO_OBJECT_TYPE_RAW_SOCKET, ZXIO_OBJECT_TYPE_STREAM_SOCKET,
    ZXIO_OBJECT_TYPE_SYNCHRONOUS_DATAGRAM_SOCKET,
};
use syncio::{
    AllocateMode, XattrSetMode, Zxio, zxio_fsverity_descriptor_t, zxio_node_attr_has_t,
    zxio_node_attributes_t,
};
use zx::Counter;

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
    current_task: &CurrentTask,
    options: FileSystemOptions,
) -> Result<FileSystemHandle, Errno> {
    let kernel = current_task.kernel();
    let requested_path = std::str::from_utf8(&options.source)
        .map_err(|_| errno!(EINVAL, "source path is not utf8"))?;
    let mut create_flags =
        fio::PERM_READABLE | fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_DIRECTORY;
    if !options.flags.load(Ordering::Relaxed).contains(FileSystemFlags::RDONLY) {
        create_flags |= fio::PERM_WRITABLE;
    }
    let (root_proxy, subdir) = kernel.open_ns_dir(requested_path, create_flags)?;

    let subdir = if subdir.is_empty() { ".".to_string() } else { subdir };
    let mut open_rights = fio::PERM_READABLE;
    if !options.flags.load(Ordering::Relaxed).contains(FileSystemFlags::RDONLY) {
        open_rights |= fio::PERM_WRITABLE;
    }
    let mut subdir_options = options;
    subdir_options.source = subdir.into();
    new_remotefs_in_root(kernel, &root_proxy, subdir_options, open_rights)
}

/// Create a filesystem to access the content of the fuchsia directory available
/// at `options.source` inside `root`.
pub fn new_remotefs_in_root(
    kernel: &Kernel,
    root: &fio::DirectorySynchronousProxy,
    options: FileSystemOptions,
    rights: fio::Flags,
) -> Result<FileSystemHandle, Errno> {
    let root = syncio::directory_open_directory_async(
        root,
        std::str::from_utf8(&options.source)
            .map_err(|_| errno!(EINVAL, "source path is not utf8"))?,
        rights,
    )
    .map_err(|e| errno!(EIO, format!("Failed to open root: {e}")))?;
    RemoteFs::new_fs(kernel, root.into_channel(), options, rights)
}

pub struct RemoteFs<I> {
    // If true, trust the remote file system's IDs (which requires that the remote file system does
    // not span mounts).  This must be true to properly support hard links.  If this is false, the
    // same node can end up having different IDs as it leaves and reenters the node cache.
    // TODO(https://fxbug.dev/42081972): At the time of writing, package directories do not have
    // unique IDs so this *must* be false in that case.
    use_remote_ids: bool,

    root_io: I,

    // The rights used for the root node.
    root_rights: fio::Flags,

    /// Casefold support is only assumed if QueryFilesystem exists and fs_type is Fxfs.
    casefold: bool,

    name: &'static str,
}

impl<I: 'static> RemoteFs<I> {
    /// Returns a reference to a RemoteFs given a reference to a FileSystem.
    ///
    /// # Panics
    ///
    /// This will panic if `fs`'s ops aren't `RemoteFs`, so this should only be called when this is
    /// known to be the case.
    fn from_fs(fs: &FileSystem) -> &RemoteFs<I> {
        if let Some(remote_vol) = fs.downcast_ops::<RemoteVolume<I>>() {
            remote_vol.remotefs()
        } else {
            fs.downcast_ops::<RemoteFs<I>>().unwrap()
        }
    }
}

const REMOTE_FS_MAGIC: u32 = u32::from_be_bytes(*b"f.io");
const DYNAMIC_FS_BYTES_PER_INODE_FALLBACK: u64 = 16384; // 16 KiB
const SYNC_IOC_FILE_INFO: u8 = 4;
const SYNC_IOC_MERGE: u8 = 3;

impl<I: SyncIo> FileSystemOps for RemoteFs<I> {
    fn statfs(&self, _fs: &FileSystem, _current_task: &CurrentTask) -> Result<statfs, Errno> {
        if let Ok(info) = self.root_io.query_filesystem() {
            let (total_blocks, free_blocks) = if info.block_size > 0 {
                (
                    (info.total_bytes / u64::from(info.block_size)).try_into().unwrap_or(i64::MAX),
                    ((info.total_bytes.saturating_sub(info.used_bytes))
                        / u64::from(info.block_size))
                    .try_into()
                    .unwrap_or(i64::MAX),
                )
            } else {
                (0, 0)
            };

            let total_nodes = std::cmp::min(
                info.total_nodes,
                info.total_bytes / DYNAMIC_FS_BYTES_PER_INODE_FALLBACK,
            );
            let free_nodes = total_nodes.saturating_sub(info.used_nodes);

            let fsid = __kernel_fsid_t {
                val: [(info.fs_id & 0xffffffff) as i32, ((info.fs_id >> 32) & 0xffffffff) as i32],
            };

            return Ok(statfs {
                f_type: info.fs_type as i64,
                f_bsize: info.block_size.into(),
                f_blocks: total_blocks,
                f_bfree: free_blocks,
                f_bavail: free_blocks,
                f_files: total_nodes.try_into().unwrap_or(i64::MAX),
                f_ffree: free_nodes.try_into().unwrap_or(i64::MAX),
                f_fsid: fsid,
                f_namelen: info.max_filename_size.try_into().unwrap_or(0),
                f_frsize: info.block_size.into(),
                ..statfs::default()
            });
        }
        Ok(default_statfs(REMOTE_FS_MAGIC))
    }

    fn name(&self) -> &'static FsStr {
        self.name.into()
    }

    fn uses_external_node_ids(&self) -> bool {
        self.use_remote_ids
    }

    fn has_casefold_support(&self) -> bool {
        self.casefold
    }

    fn rename(
        &self,
        _fs: &FileSystem,
        current_task: &CurrentTask,
        context: &mut RenameContext<'_>,
        old_name: &FsStr,
        new_name: &FsStr,
    ) -> Result<(), Errno> {
        let renamed = &context.renamed.node;
        let replaced = context.replaced.map(|r| &r.node);
        let old_parent = &context.old_parent().node;
        let new_parent = &context.new_parent().node;
        let old_parent_info = context.old_parent_info();
        let new_parent_info = context.new_parent_info();
        // Renames should fail if the src or target directory is
        // encrypted and locked.
        old_parent.fail_if_locked(current_task, old_parent_info)?;
        if let Some(info) = new_parent_info {
            new_parent.fail_if_locked(current_task, info)?;
        }

        let Some((old_parent_ops, new_parent_ops)) = old_parent
            .downcast_ops::<RemoteNode<I>>()
            .zip(new_parent.downcast_ops::<RemoteNode<I>>())
        else {
            return error!(EXDEV);
        };

        let mut nodes: SmallVec<[&FsNode; 4]> =
            smallvec![&***old_parent, &***new_parent, &***renamed];
        if let Some(r) = replaced {
            nodes.push(r);
        }

        will_dirty::<I, _, _>(&nodes, || {
            old_parent_ops
                .node
                .io
                .rename(get_name_str(old_name)?, &new_parent_ops.node.io, get_name_str(new_name)?)
                .map_err(|status| match status {
                    zx::Status::BAD_STATE => errno!(EXDEV),
                    zx::Status::ACCESS_DENIED => errno!(ENOKEY),
                    s => map_sync_io_client_error(s),
                })
        })
    }

    fn sync(&self, _fs: &FileSystem, _current_task: &CurrentTask) -> Result<(), Errno> {
        self.root_io.sync().map_err(map_sync_error)
    }

    fn manages_timestamps(&self) -> bool {
        true
    }

    fn reconfigure(
        &self,
        fs: &FileSystem,
        _current_task: &CurrentTask,
        mut flags: FileSystemFlags,
        params: &MountParams,
    ) -> Result<(), Errno> {
        if !params.is_empty() {
            track_stub!(TODO("https://fxbug.dev/322875506"), "MS_REMOUNT: Updating data");
        }
        if !self.root_rights.contains(fio::PERM_WRITABLE) {
            flags |= FileSystemFlags::RDONLY;
        }
        fs.options.flags.store(flags, Ordering::Relaxed);
        Ok(())
    }
}

/// Factory is a helper that creates the appropriate node type when creating a node.  See
/// LookupFactory below for a helper that is specialised for the lookup case.  Nodes are created
/// clean if attributes are returned in `OnRepresentation`, or dirty otherwise.
struct Factory<'a> {
    node_info: &'a mut FsNodeInfo,
    assume_special: bool,
}

impl<'a> Factory<'a> {
    fn new(node_info: &'a mut FsNodeInfo, assume_special: bool) -> Self {
        Self { node_info, assume_special }
    }
}

impl<'a, I: SyncIo> sync_io_client::Factory<I> for Factory<'a> {
    type Result = (Box<dyn FsNodeOps>, u64);

    fn create_node(self, io: I, info: fio::NodeInfo) -> Self::Result {
        let dirty = info.attributes.is_none();
        let attrs = get_attributes(&info.attributes);
        let id = attrs.immutable_attributes.id.unwrap_or(fio::INO_UNKNOWN);
        update_info_from_fidl(
            self.node_info,
            &attrs.mutable_attributes,
            &attrs.immutable_attributes,
        );
        (Box::new(RemoteNode::new(io, dirty)), id)
    }

    fn create_directory(self, io: I, info: fio::DirectoryInfo) -> Self::Result {
        let dirty = info.attributes.is_none();
        let attrs = get_attributes(&info.attributes);
        let id = attrs.immutable_attributes.id.unwrap_or(fio::INO_UNKNOWN);
        update_info_from_fidl(
            self.node_info,
            &attrs.mutable_attributes,
            &attrs.immutable_attributes,
        );
        (Box::new(RemoteNode::new(io, dirty)), id)
    }

    fn create_file(self, io: I, info: fio::FileInfo) -> Self::Result {
        let dirty = info.attributes.is_none();
        let is_special_node = self.assume_special || is_special(&info);
        let attrs = get_attributes(&info.attributes);
        let id = attrs.immutable_attributes.id.unwrap_or(fio::INO_UNKNOWN);
        let ops: Box<dyn FsNodeOps> = if is_special_node {
            Box::new(RemoteSpecialNode { node: BaseNode::new(io, dirty) })
        } else {
            Box::new(RemoteNode::new(io, dirty))
        };
        update_info_from_fidl(
            self.node_info,
            &attrs.mutable_attributes,
            &attrs.immutable_attributes,
        );
        (ops, id)
    }

    fn create_symlink(self, io: I, info: fio::SymlinkInfo) -> Self::Result {
        let dirty = info.attributes.is_none();
        let attrs = get_attributes(&info.attributes);
        let id = attrs.immutable_attributes.id.unwrap_or(fio::INO_UNKNOWN);
        let target = info.target.unwrap_or_default();
        update_info_from_fidl(
            self.node_info,
            &attrs.mutable_attributes,
            &attrs.immutable_attributes,
        );
        (Box::new(RemoteSymlink::new(BaseNode::new(io, dirty), target)), id)
    }
}

/// LookupFactory is an optimised version of Factory which is used only for lookup.  All the
/// functions will create nodes that are initially clean, which works because lookup always requests
/// all attributes.
struct LookupFactory<'a> {
    fs: &'a FileSystemHandle,
    current_task: &'a CurrentTask,
}

impl<'a> LookupFactory<'a> {
    fn new(fs: &'a FileSystemHandle, current_task: &'a CurrentTask) -> Self {
        Self { fs, current_task }
    }

    fn get_node<I: SyncIo>(
        &self,
        io: I,
        attributes: &fio::NodeAttributes2,
        create_ops: impl FnOnce(I) -> Box<dyn FsNodeOps>,
    ) -> Result<FsNodeHandle, Errno> {
        let fs = self.fs;
        let fs_ops = RemoteFs::<I>::from_fs(fs);
        let fio::NodeAttributes2 { mutable_attributes: mutable, immutable_attributes: immutable } =
            attributes;

        let id = immutable.id.unwrap_or(fio::INO_UNKNOWN);
        let node_id = if fs_ops.use_remote_ids {
            if id == fio::INO_UNKNOWN {
                return error!(ENOTSUP);
            }
            id
        } else {
            fs.allocate_ino()
        };

        let node = fs.get_or_create_node(node_id, || {
            let uid = mutable.uid.unwrap_or(0);
            let gid = mutable.gid.unwrap_or(0);
            let owner = FsCred { uid, gid };
            let rdev = DeviceId::from_bits(mutable.rdev.unwrap_or(0));
            let fsverity_enabled = immutable.verity_enabled.unwrap_or(false);
            let protocols = immutable.protocols.unwrap_or(fio::NodeProtocolKinds::empty());
            // fsverity should not be enabled for non-file nodes.
            if fsverity_enabled && !protocols.contains(fio::NodeProtocolKinds::FILE) {
                return error!(EINVAL);
            }

            let ops = create_ops(io);
            let child = FsNode::new_uncached(
                node_id,
                ops,
                fs,
                FsNodeInfo {
                    rdev,
                    ..FsNodeInfo::new(
                        get_mode_from_fidl(mutable, immutable, fs_ops.root_rights),
                        owner,
                    )
                },
                FsNodeFlags::empty(),
            );
            if fsverity_enabled {
                *child.fsverity.lock() = FsVerityState::FsVerity;
            }
            if let Some(fio::SelinuxContext::Data(data)) = mutable.selinux_context.as_ref() {
                security::fs_node_notify_security_context(
                    self.current_task,
                    &child,
                    FsStr::new(data),
                );
            }
            Ok(child)
        })?;

        node.update_info(|info| update_info_from_fidl(info, mutable, immutable));

        Ok(node)
    }
}

impl<'a, I: SyncIo> sync_io_client::Factory<I> for LookupFactory<'a> {
    type Result = Result<FsNodeHandle, Errno>;

    fn create_node(self, io: I, info: fio::NodeInfo) -> Self::Result {
        self.get_node(io, get_attributes(&info.attributes), |io| {
            Box::new(RemoteNode::new(io, false))
        })
    }

    fn create_directory(self, io: I, info: fio::DirectoryInfo) -> Self::Result {
        self.get_node(io, get_attributes(&info.attributes), |io| {
            Box::new(RemoteNode::new(io, false))
        })
    }

    fn create_file(self, io: I, info: fio::FileInfo) -> Self::Result {
        let is_special_node = is_special(&info);
        self.get_node(io, get_attributes(&info.attributes), |io| {
            if is_special_node {
                Box::new(RemoteSpecialNode { node: BaseNode::new(io, false) })
            } else {
                Box::new(RemoteNode::new(io, false))
            }
        })
    }

    fn create_symlink(self, io: I, mut info: fio::SymlinkInfo) -> Self::Result {
        let mut target = info.target.take();
        if target.is_none() {
            return error!(EIO);
        }
        let node = self.get_node(io, get_attributes(&info.attributes), |io| {
            Box::new(RemoteSymlink::new(BaseNode::new(io, false), target.take().unwrap()))
        })?;
        // Encrypted symlinks that use fscrypt can be read as encrypted links when no key is
        // available.  When no key is available, directories will not cache their entries.  When,
        // the key is subsequently provided, the next time the symlink is read, we will come through
        // here, but since the node is cached, `get_or_create_node` will not create a new node
        // which, if we were to do nothing, would mean we'd keep the encrypted value for the target.
        // To address this, if no new node was created, we update the target of the existing node
        // here.  Once the key has been provided, the entry will be cached with the directory and
        // whilst the entry remains cached, `lookup` will not be called.
        if let Some(target) = target
            && let Some(symlink) = node.downcast_ops::<RemoteSymlink<I>>()
        {
            *symlink.target.write() = target.into_boxed_slice();
        }
        Ok(node)
    }
}

// A helper that makes it easy to deal with the rare case where no FIDL attributes are returned.
fn get_attributes(attrs: &Option<fio::NodeAttributes2>) -> &fio::NodeAttributes2 {
    static DEFAULT_NODE_ATTRIBUTES: LazyLock<fio::NodeAttributes2> =
        LazyLock::new(|| fio::NodeAttributes2 {
            mutable_attributes: Default::default(),
            immutable_attributes: Default::default(),
        });
    attrs.as_ref().unwrap_or_else(|| &*DEFAULT_NODE_ATTRIBUTES)
}

impl<I: SyncIo> RemoteFs<I> {
    pub fn new_with_io(
        root_io: I,
        root_rights: fio::Flags,
        name: &'static str,
    ) -> Result<(RemoteFs<I>, Box<dyn FsNodeOps>, FsNodeInfo, u64), Errno> {
        let vfs_type = root_io
            .query_filesystem()
            .ok()
            .and_then(|i| fidl_fuchsia_fs::VfsType::from_primitive(i.fs_type));
        let (use_remote_ids, casefold) = match vfs_type {
            Some(fidl_fuchsia_fs::VfsType::Fxfs) => (true, true),
            Some(fidl_fuchsia_fs::VfsType::Erofs) => (true, false),
            _ => (false, false),
        };

        // The OnRepresentation response will return an initial set of `attrs`.
        let mut node_info = FsNodeInfo::new(mode!(IFDIR, 0o777), FsCred::root());
        let (remote_node, node_id) = root_io
            .open(
                ".",
                fio::Flags::PROTOCOL_DIRECTORY
                    | fio::PERM_READABLE
                    | fio::Flags::PERM_INHERIT_WRITE
                    | fio::Flags::PERM_INHERIT_EXECUTE,
                None,
                NODE_INFO_ATTRIBUTES
                    | fio::NodeAttributesQuery::ID
                    | fio::NodeAttributesQuery::ENCRYPTION_POLICY,
                Factory::new(&mut node_info, false),
            )
            .map_err(map_sync_io_client_error)?;

        Ok((
            RemoteFs { use_remote_ids, root_io, root_rights, casefold, name },
            remote_node,
            node_info,
            node_id,
        ))
    }

    pub(super) fn use_remote_ids(&self) -> bool {
        self.use_remote_ids
    }
}

impl RemoteFs<RemoteIo> {
    pub fn new(
        root: zx::Channel,
        root_rights: fio::Flags,
        name: &'static str,
    ) -> Result<(RemoteFs<RemoteIo>, Box<dyn FsNodeOps>, FsNodeInfo, u64), Errno> {
        let root_io = RemoteIo::new(fio::NodeSynchronousProxy::new(root));
        Self::new_with_io(root_io, root_rights, name)
    }

    pub fn new_fs(
        kernel: &Kernel,
        root: zx::Channel,
        options: FileSystemOptions,
        rights: fio::Flags,
    ) -> Result<FileSystemHandle, Errno> {
        let (remotefs, root_node, info, node_id) = RemoteFs::new(root, rights, "remotefs")?;

        if !rights.contains(fio::PERM_WRITABLE) {
            options.flags.fetch_or(FileSystemFlags::RDONLY, Ordering::Relaxed);
        }
        let use_remote_ids = remotefs.use_remote_ids;
        let fs = FileSystem::new(
            kernel,
            CacheMode::Cached(kernel.fs_cache_config()),
            remotefs,
            options,
        )?;

        let node_id = if use_remote_ids { node_id } else { fs.allocate_ino() };
        fs.create_root_with_info(node_id, root_node, info);

        Ok(fs)
    }
}

/// All nodes compose `BaseNode`.
///
/// NOTE: If new node types are created, the `TryFrom` implementation needs updating below.
struct BaseNode<I> {
    /// The underlying I/O object for this remote node.
    io: I,

    /// The number of active dirty operations on this node and whether the node info is in sync.
    /// See the `will_dirty` function for semantics.
    info_state: InfoState,
}

impl<I> BaseNode<I> {
    fn new(io: I, dirty: bool) -> Self {
        Self { io, info_state: InfoState::new(dirty) }
    }
}

impl<I: SyncIo> BaseNode<I> {
    fn fetch_and_refresh_info<'a>(
        &self,
        info: &'a DynamicLockDepRwLock<FsNodeInfo>,
    ) -> Result<LockDepReadGuard<'a, FsNodeInfo>, Errno> {
        self.info_state.maybe_refresh(
            info,
            |info| {
                let mut query = NODE_INFO_ATTRIBUTES;
                if info.read().pending_time_access_update {
                    query |= fio::NodeAttributesQuery::PENDING_ACCESS_TIME_UPDATE;
                }
                let (mutable, immutable) =
                    self.io.attr_get(query).map_err(map_sync_io_client_error)?;
                let mut info = info.write();
                info.pending_time_access_update = false;
                update_info_from_fidl(&mut info, &mutable, &immutable);
                Ok(LockDepWriteGuard::downgrade(info))
            },
            |info| Ok(info.read()),
        )
    }

    fn update_attributes(&self, info: &FsNodeInfo, has: zxio_node_attr_has_t) -> Result<(), Errno> {
        // Omit updating creation_time. By definition, there shouldn't be a change in creation_time.
        will_dirty(&[self], || {
            let res = self.io.attr_set(fio::MutableNodeAttributes {
                modification_time: has
                    .modification_time
                    .then_some(info.time_modify.into_nanos() as u64),
                access_time: has.access_time.then_some(info.time_access.into_nanos() as u64),
                mode: has.mode.then_some(info.mode.bits()),
                uid: has.uid.then_some(info.uid),
                gid: has.gid.then_some(info.gid),
                rdev: has.rdev.then_some(info.rdev.bits()),
                casefold: has.casefold.then_some(info.casefold),
                encryption_policy: if has.encryption_policy {
                    info.encryption_policy.map(|p| fio::FscryptPolicy {
                        key_identifier: p.key_identifier,
                        flags: fio::FscryptPolicyFlags::from_bits_retain(p.flags.bits()),
                    })
                } else {
                    None
                },
                ..Default::default()
            });
            res.map_err(|status| from_status_like_fdio!(status))
        })
    }
}

impl<'a, I: SyncIo> TryFrom<&'a FsNode> for &'a BaseNode<I> {
    type Error = ();
    fn try_from(value: &'a FsNode) -> Result<&'a BaseNode<I>, ()> {
        value
            .downcast_ops::<RemoteNode<I>>()
            .map(|n| &n.node)
            .or_else(|| value.downcast_ops::<RemoteSpecialNode<I>>().map(|n| &n.node))
            .or_else(|| value.downcast_ops::<RemoteSymlink<I>>().map(|n| &n.node))
            .ok_or(())
    }
}

/// This is the most common type of node.  It is used for files and directories.  Symlinks and
/// special nodes use RemoteSymlink and RemoteSpecialNode respectively.
struct RemoteNode<I> {
    node: BaseNode<I>,

    /// Cached read-only VMO handle shared across all open `RemoteFileObject`s for this inode.
    read_only_memory: OnceCell<Arc<MemoryObject>>,

    /// Cached read/exec VMO handle shared across all open `RemoteFileObject`s for this inode.
    read_exec_memory: OnceCell<Arc<MemoryObject>>,
}

impl<I> RemoteNode<I> {
    fn new(io: I, dirty: bool) -> Self {
        Self {
            node: BaseNode::new(io, dirty),
            read_only_memory: OnceCell::new(),
            read_exec_memory: OnceCell::new(),
        }
    }
}

impl<I: SyncIo> RemoteNode<I> {
    fn get_memory(
        &self,
        file: &FileObject,
        prot: ProtectionFlags,
    ) -> Result<Arc<MemoryObject>, Errno> {
        get_or_init_remote_memory(file, &self.read_only_memory, &self.read_exec_memory, prot, |p| {
            self.node.io.fetch_remote_memory(p)
        })
    }

    /// Shared implementation of `mknod()` and `mkdir()`.
    ///
    /// Creates a child of this directory named `name` with the given `protocol` (file or
    /// directory), supplying its mode, ownership, `extra_attributes`, and (if the file-system is
    /// xattr-labeled) its security context inline with the creation request, so that no separate
    /// `UpdateAttributes` or `SetExtendedAttribute` round-trips are needed afterwards.
    fn create_child(
        &self,
        node: &FsNode,
        current_task: &CurrentTask,
        name: Option<&FsStr>,
        mut node_info: FsNodeInfo,
        create_flags: fio::Flags,
        extra_attributes: fio::MutableNodeAttributes,
    ) -> Result<FsNodeHandle, Errno> {
        let name_str = if let Some(name) = name {
            node.fail_if_locked(current_task, &node.info())?;
            get_name_str(name)?
        } else {
            "."
        };

        let fs = node.fs();
        let fs_ops = RemoteFs::<I>::from_fs(&fs);
        let mode = node_info.mode;

        // Ask the LSM for the label the new node should receive, so that it can be applied by the
        // remote file-system as part of creation.
        let security_xattr =
            security::dentry_init_security(current_task, node, mode, name.unwrap_or_default())?;
        // `fio::MutableNodeAttributes::selinux_context` carries specifically the SELinux label (up
        // to `MAX_SELINUX_CONTEXT_ATTRIBUTE_LEN` bytes); any other or larger security xattr is left
        // to be applied by the VFS via `SetExtendedAttribute`.
        let selinux_context = security_xattr
            .as_ref()
            .filter(|xattr| {
                xattr.name.as_bytes() == XATTR_NAME_SELINUX.to_bytes()
                    && xattr.value.len() <= fio::MAX_SELINUX_CONTEXT_ATTRIBUTE_LEN as usize
            })
            .map(|xattr| xattr.value.clone());

        let (ops, node_id) = will_dirty(&[&self.node], || {
            self.node
                .io
                .open(
                    name_str,
                    create_flags | fio::PERM_READABLE | fio::PERM_WRITABLE,
                    Some(fio::MutableNodeAttributes {
                        mode: Some(mode.bits()),
                        uid: Some(node_info.uid),
                        gid: Some(node_info.gid),
                        selinux_context: selinux_context
                            .clone()
                            .map(|ctx| fio::SelinuxContext::Data(ctx.into())),
                        ..extra_attributes
                    }),
                    NODE_INFO_ATTRIBUTES
                        | fio::NodeAttributesQuery::ID
                        | fio::NodeAttributesQuery::ENCRYPTION_POLICY,
                    Factory::new(&mut node_info, !(mode.is_reg() || mode.is_dir())),
                )
                .map_err(|status| from_status_like_fdio!(status, name_str))
        })?;

        let node_id = if fs_ops.use_remote_ids { node_id } else { fs.allocate_ino() };

        let child = fs.create_node(node_id, ops, node_info);
        if let Some(context) = selinux_context {
            let _ = security::fs_node_notify_security_context(
                current_task,
                &child,
                FsStr::new(&context),
            );
        }
        Ok(child)
    }
}

/// Classified Fuchsia handle representation returned by [`classify_remote_handle`].
enum RemoteHandle {
    Socket(SocketHandle),
    File(RemoteIo),
    Directory(RemoteIo),
    Counter(RemoteCounter),
    Zxio { zxio: Zxio, handle_type: zx::ObjectType, content_size: Option<u64> },
}

/// Inspects `handle` and constructs the corresponding [`RemoteHandle`] variant.
fn classify_remote_handle(
    current_task: &CurrentTask,
    mut handle: zx::NullableHandle,
    creds: Arc<Credentials>,
) -> Result<RemoteHandle, Errno> {
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
                let socket_ops = RemoteUnixDomainSocket::new(queryable.into_channel(), creds)?;
                let socket = Socket::new_with_ops(Box::new(socket_ops))?;
                return Ok(RemoteHandle::Socket(socket));
            }
            FILE_PROTOCOL => {
                let file_proxy = fio::FileSynchronousProxy::from(queryable.into_channel());
                let info =
                    file_proxy.describe(zx::MonotonicInstant::INFINITE).map_err(|_| errno!(EIO))?;
                let io = RemoteIo::with_stream(
                    file_proxy.into_channel().into(),
                    info.stream.unwrap_or_else(|| zx::NullableHandle::invalid().into()),
                );
                return Ok(RemoteHandle::File(io));
            }
            DIRECTORY_PROTOCOL => {
                let io = RemoteIo::new(queryable.into_channel().into());
                return Ok(RemoteHandle::Directory(io));
            }
            _ => {
                handle = queryable.into_channel().into_handle();
                // Fall through for zxio.
            }
        }
    } else if handle_type == zx::ObjectType::COUNTER {
        return Ok(RemoteHandle::Counter(RemoteCounter::new(handle.into())));
    }

    // Otherwise, use zxio based objects.

    // NOTE: If it's a channel, this will repeat the query, which is something we can optimize if we
    // need to.
    let zxio = Zxio::create(handle).map_err(|status| from_status_like_fdio!(status))?;
    let attrs = zxio
        .attr_get(zxio_node_attr_has_t {
            protocols: true,
            content_size: true,
            storage_size: true,
            link_count: true,
            object_type: true,
            ..Default::default()
        })
        .map_err(|status| from_status_like_fdio!(status))?;
    match (handle_type, attrs.object_type) {
        (zx::ObjectType::VMO, _) | (zx::ObjectType::DEBUGLOG, _) | (_, ZXIO_OBJECT_TYPE_NONE) => {
            Ok(RemoteHandle::Zxio {
                zxio,
                handle_type,
                content_size: attrs.has.content_size.then_some(attrs.content_size),
            })
        }
        (zx::ObjectType::SOCKET, _)
        | (_, ZXIO_OBJECT_TYPE_SYNCHRONOUS_DATAGRAM_SOCKET)
        | (_, ZXIO_OBJECT_TYPE_DATAGRAM_SOCKET)
        | (_, ZXIO_OBJECT_TYPE_STREAM_SOCKET)
        | (_, ZXIO_OBJECT_TYPE_RAW_SOCKET)
        | (_, ZXIO_OBJECT_TYPE_PACKET_SOCKET) => {
            let socket_ops = ZxioBackedSocket::new_with_zxio(current_task, zxio);
            let socket = Socket::new_with_ops(Box::new(socket_ops))?;
            Ok(RemoteHandle::Socket(socket))
        }
        _ => error!(ENOTSUP),
    }
}

/// Creates a [`FileHandle`] from a [`zx::NullableHandle`].
///
/// The handle must be a channel, socket, vmo, debuglog, or counter object. If the handle is a
/// channel, then the channel must implement the `fuchsia.unknown/Queryable` protocol. Not all
/// protocols are supported; files, directories, and sockets are, but symlinks are not.
///
/// The resulting object will be owned by root, and will have permissions derived from the `flags`
/// used to open this object. This is not the same as the permissions set if the object was created
/// using Starnix itself. We use this mainly for interfacing with objects created outside of Starnix
/// where these flags represent the desired permissions already.
pub fn new_remote_file(
    current_task: &CurrentTask,
    handle: zx::NullableHandle,
    flags: OpenFlags,
) -> Result<FileHandle, Errno> {
    let creds = current_task.current_creds().clone();
    match classify_remote_handle(current_task, handle, creds)? {
        RemoteHandle::Socket(socket) => {
            SocketFile::from_socket(current_task, socket, flags, /* kernel_private = */ false)
        }
        RemoteHandle::File(io) => {
            new_remote_fs_node_file(current_task, io, /* is_dir = */ false, flags)
        }
        RemoteHandle::Directory(io) => {
            new_remote_fs_node_file(current_task, io, /* is_dir = */ true, flags)
        }
        RemoteHandle::Counter(counter) => {
            Ok(Anon::new_private_file(current_task, Box::new(counter), flags, "[fuchsia:counter]"))
        }
        RemoteHandle::Zxio { zxio, handle_type, content_size } => {
            let size = content_size.map(|s| usize::try_from(s).unwrap_or(usize::MAX)).unwrap_or(0);
            let ops = Box::new(RemoteZxioFileObject::new(zxio));
            let name = match handle_type {
                zx::ObjectType::VMO => "[fuchsia:vmo]",
                zx::ObjectType::DEBUGLOG => "[fuchsia:debuglog]",
                _ => "[fuchsia:remote]",
            };
            Ok(MemoryRegularFile::new_private_file_with_ops(current_task, ops, size, flags, name))
        }
    }
}

/// Unrooted [`FsNodeOps`] implementation for remote Fuchsia channels wrapped via
/// [`new_remote_file`].
struct AnonymousRemoteNode;

impl FsNodeOps for AnonymousRemoteNode {
    fs_node_impl_not_dir!();

    fn create_file_ops(
        &self,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        error!(ENOSYS)
    }
}

/// [`FileSystemOps`] for the kernel-singleton `remotefs` instance hosting unrooted remote nodes.
struct RemoteNullFs;

impl FileSystemOps for RemoteNullFs {
    fn statfs(&self, _fs: &FileSystem, _current_task: &CurrentTask) -> Result<statfs, Errno> {
        Ok(default_statfs(0))
    }

    fn name(&self) -> &'static FsStr {
        "remotefs".into()
    }
}

/// Returns the kernel-singleton `remotefs` [`FileSystemHandle`] used for unrooted remote nodes.
fn remote_null_fs(current_task: &CurrentTask) -> Result<FileSystemHandle, Errno> {
    struct RemoteNullFsHandle {
        fs: FileSystemHandle,
    }

    let kernel = current_task.kernel();
    let fs = kernel
        .expando
        .get_or_init(|| RemoteNullFsHandle {
            fs: FileSystem::new(
                kernel,
                CacheMode::Uncached,
                RemoteNullFs,
                FileSystemOptions::default(),
            )
            .expect("remotefs constructed with valid options"),
        })
        .fs
        .clone();
    security::file_system_resolve_security(current_task, &fs)?;
    Ok(fs)
}

/// Creates a [`FileHandle`] backed by an unrooted [`FsNode`] in [`remote_null_fs`].
fn new_remote_fs_node_file(
    current_task: &CurrentTask,
    io: RemoteIo,
    is_dir: bool,
    flags: OpenFlags,
) -> Result<FileHandle, Errno> {
    let fs = remote_null_fs(current_task)?;
    let is_xattr_labeled = security::fs_is_xattr_labeled(fs.clone());
    let mut query = MODE_ATTRIBUTES | NODE_INFO_ATTRIBUTES;
    if is_xattr_labeled {
        query |= fio::NodeAttributesQuery::SELINUX_CONTEXT;
    }
    let (mutable, mut immutable) = io.attr_get(query).map_err(map_sync_io_client_error)?;
    immutable.protocols.get_or_insert(if is_dir {
        fio::NodeProtocolKinds::DIRECTORY
    } else {
        fio::NodeProtocolKinds::FILE
    });
    let mut rights = fio::Flags::empty();
    if flags.can_read() {
        rights |= fio::PERM_READABLE;
    }
    if flags.can_write() {
        rights |= fio::PERM_WRITABLE;
    }
    let mode = get_mode_from_fidl(&mutable, &immutable, rights);
    let mut info = FsNodeInfo::new(mode, FsCred::root());
    update_info_from_fidl(&mut info, &mutable, &immutable);
    let selinux_context = match mutable.selinux_context {
        Some(fio::SelinuxContext::Data(data)) => Some(data),
        Some(fio::SelinuxContext::UseExtendedAttributes(_)) => {
            io.xattr_get(XATTR_NAME_SELINUX.to_bytes().into()).ok()
        }
        _ => None,
    };
    let ops: Box<dyn FileOps> = if is_dir {
        Box::new(RemoteDirectoryObject::new(RemoteDirectory::new(
            io.into_proxy().into_channel().into(),
        )))
    } else {
        Box::new(AnonymousRemoteFileObject::new(io))
    };

    let node = fs.create_node_and_allocate_node_id(AnonymousRemoteNode, info);
    if let Some(data) = selinux_context {
        security::fs_node_notify_security_context(current_task, &node, FsStr::new(&data));
    }
    let dir_entry = DirEntry::new_unrooted(node);
    security::fs_node_init_with_dentry(current_task, &dir_entry)?;
    FileObject::new(current_task, ops, NamespaceNode::new_anonymous(dir_entry), flags)
}

/// Creates a [`FileOps`] from a [`zx::NullableHandle`].
pub fn new_remote_file_ops(
    current_task: &CurrentTask,
    handle: zx::NullableHandle,
    creds: Arc<Credentials>,
) -> Result<Box<dyn FileOps>, Errno> {
    match classify_remote_handle(current_task, handle, creds)? {
        RemoteHandle::Socket(socket) => Ok(SocketFile::new(socket)),
        RemoteHandle::File(io) => Ok(Box::new(AnonymousRemoteFileObject::new(io))),
        RemoteHandle::Directory(io) => Ok(Box::new(RemoteDirectoryObject::new(
            RemoteDirectory::new(io.into_proxy().into_channel().into()),
        ))),
        RemoteHandle::Counter(counter) => Ok(Box::new(counter)),
        RemoteHandle::Zxio { zxio, .. } => Ok(Box::new(RemoteZxioFileObject::new(zxio))),
    }
}

/// Creates a `pipefs`-backed [`FileHandle`] (`S_IFIFO`) from a [`zx::Socket`].
pub fn create_fuchsia_pipe(
    current_task: &CurrentTask,
    socket: zx::Socket,
    flags: OpenFlags,
) -> Result<FileHandle, Errno> {
    let zxio = Zxio::create(socket.into()).map_err(|status| from_status_like_fdio!(status))?;
    new_pipe_file(current_task, Box::new(RemotePipeObject::new(zxio)), flags)
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
        info.size = attrs.content_size.try_into().unwrap_or(usize::MAX);
    }
    if attrs.has.storage_size {
        info.blocks = usize::try_from(attrs.storage_size)
            .unwrap_or(usize::MAX)
            .div_ceil(DEFAULT_BYTES_PER_BLOCK)
    }
    info.blksize = DEFAULT_BYTES_PER_BLOCK;
    if attrs.has.link_count {
        info.link_count = attrs.link_count.try_into().unwrap_or(usize::MAX);
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
    // The following are only read once so they're not included in `NODE_INFO_ATTRIBUTES`.
    if attrs.has.casefold {
        info.casefold = attrs.casefold;
    }
}

/// Same as `update_info_from_attr` but uses FIDL.
fn update_info_from_fidl(
    info: &mut FsNodeInfo,
    mutable: &fio::MutableNodeAttributes,
    immutable: &fio::ImmutableNodeAttributes,
) {
    if let Some(content_size) = immutable.content_size {
        info.size = content_size.try_into().unwrap_or(usize::MAX);
    }
    if let Some(storage_size) = immutable.storage_size {
        info.blocks =
            usize::try_from(storage_size).unwrap_or(usize::MAX).div_ceil(DEFAULT_BYTES_PER_BLOCK);
    }
    info.blksize = DEFAULT_BYTES_PER_BLOCK;
    if let Some(link_count) = immutable.link_count {
        info.link_count = link_count.try_into().unwrap_or(usize::MAX);
    }
    if let Some(modification_time) = mutable.modification_time {
        info.time_modify = UtcInstant::from_nanos(modification_time.try_into().unwrap_or(i64::MAX));
    }
    if let Some(change_time) = immutable.change_time {
        info.time_status_change =
            UtcInstant::from_nanos(change_time.try_into().unwrap_or(i64::MAX));
    }
    if !info.pending_time_access_update
        && let Some(access_time) = mutable.access_time
    {
        info.time_access = UtcInstant::from_nanos(access_time.try_into().unwrap_or(i64::MAX));
    }
    // The following are only read once so they're not included in `NODE_INFO_ATTRIBUTES`.
    if let Some(casefold) = mutable.casefold {
        info.casefold = casefold;
    }
    if let Some(policy) = mutable.encryption_policy {
        info.encryption_policy = Some(crate::vfs::FscryptNodePolicy {
            key_identifier: policy.key_identifier,
            flags: crate::vfs::FscryptPolicyFlags::from_bits_retain(policy.flags.bits()),
        });
    }
}

/// The attributes we need to request to compute the right mode.
const MODE_ATTRIBUTES: fio::NodeAttributesQuery =
    fio::NodeAttributesQuery::PROTOCOLS.union(fio::NodeAttributesQuery::MODE);

// NOTE: Keep in sync with `MODE_ATTRIBUTES`.
fn get_mode_from_fidl(
    mutable: &fio::MutableNodeAttributes,
    immutable: &fio::ImmutableNodeAttributes,
    rights: fio::Flags,
) -> FileMode {
    let protocols = immutable.protocols.unwrap_or(fio::NodeProtocolKinds::empty());
    if protocols.contains(fio::NodeProtocolKinds::SYMLINK) {
        // We don't set the mode for symbolic links , so we synthesize it instead.
        FileMode::IFLNK | FileMode::ALLOW_ALL
    } else if let Some(mode) = mutable.mode {
        // If the filesystem supports POSIX mode bits, use that directly.
        FileMode::from_bits(mode)
    } else {
        // The filesystem doesn't support the `mode` attribute, so synthesize it from the protocols
        // this node supports, and the rights used to open it.
        let is_directory = protocols.contains(fio::NodeProtocolKinds::DIRECTORY);
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

impl<I: SyncIo> XattrStorage for BaseNode<I> {
    fn get_xattr(&self, name: &FsStr) -> Result<FsString, Errno> {
        Ok(self
            .io
            .xattr_get(name)
            .map_err(|status| match status {
                zx::Status::NOT_FOUND => errno!(ENODATA),
                status => from_status_like_fdio!(status),
            })?
            .into())
    }

    fn set_xattr(&self, name: &FsStr, value: &FsStr, op: XattrOp) -> Result<(), Errno> {
        let mode = match op {
            XattrOp::Set => XattrSetMode::Set,
            XattrOp::Create => XattrSetMode::Create,
            XattrOp::Replace => XattrSetMode::Replace,
        };

        will_dirty(&[self], || {
            self.io.xattr_set(name, value, mode).map_err(|status| match status {
                zx::Status::NOT_FOUND => errno!(ENODATA),
                status => from_status_like_fdio!(status),
            })
        })
    }

    fn remove_xattr(&self, name: &FsStr) -> Result<(), Errno> {
        will_dirty(&[self], || {
            self.io.xattr_remove(name).map_err(|status| match status {
                zx::Status::NOT_FOUND => errno!(ENODATA),
                _ => from_status_like_fdio!(status),
            })
        })
    }

    fn list_xattrs(&self) -> Result<Vec<FsString>, Errno> {
        self.io
            .xattr_list()
            .map(|attrs| attrs.into_iter().map(FsString::new).collect::<Vec<_>>())
            .map_err(map_sync_io_client_error)
    }
}

impl<I: SyncIo> FsNodeOps for RemoteNode<I> {
    fs_node_impl_xattr_delegate!(self, self.node);

    fn create_file_ops(
        &self,
        node: &FsNode,
        current_task: &CurrentTask,
        flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        {
            // It is safe to read the cached node info here because the `encryption_policy` is
            // fetched when the node is first opened, and updated when set. We don't expect this to
            // change out from under Starnix.
            let node_info = node.info();
            if node_info.mode.is_dir() {
                if let Some(policy) = node_info.encryption_policy {
                    if flags.can_write() {
                        // Locked encrypted directories cannot be opened with write access.
                        let crypt_service =
                            node.fs().crypt_service().ok_or_else(|| errno!(ENOKEY))?;
                        if !crypt_service.contains_key(EncryptionKeyId::from(policy.key_identifier))
                        {
                            return error!(ENOKEY);
                        }
                    }
                }
                // For directories we need to clone the connection because we rely on the seek
                // offset.
                return Ok(Box::new(RemoteDirectoryObject::new(
                    self.node.io.open_directory().map_err(map_sync_io_client_error)?,
                )));
            }
        }

        // Locked encrypted files cannot be opened.
        node.fail_if_locked(current_task, &node.info())?;

        // fsverity files cannot be opened in write mode, including while building.
        if flags.can_write() {
            node.fsverity.lock().check_writable()?;
        }

        Ok(Box::new(RemoteFileObject::<I>::default()))
    }

    fn sync(&self, _node: &FsNode, _current_task: &CurrentTask) -> Result<(), Errno> {
        self.node.io.sync().map_err(map_sync_io_client_error)
    }

    fn mknod(
        &self,
        node: &FsNode,
        current_task: &CurrentTask,
        name: &FsStr,
        mode: FileMode,
        dev: DeviceId,
        owner: FsCred,
    ) -> Result<FsNodeHandle, Errno> {
        if !(mode.is_reg() || mode.is_chr() || mode.is_blk() || mode.is_fifo() || mode.is_sock()) {
            return error!(EINVAL, name);
        }
        self.create_child(
            node,
            current_task,
            Some(name),
            FsNodeInfo { rdev: dev, ..FsNodeInfo::new(mode, owner) },
            fio::Flags::FLAG_MUST_CREATE | fio::Flags::PROTOCOL_FILE,
            fio::MutableNodeAttributes { rdev: Some(dev.bits()), ..Default::default() },
        )
    }

    fn mkdir(
        &self,
        node: &FsNode,
        current_task: &CurrentTask,
        name: &FsStr,
        mode: FileMode,
        owner: FsCred,
    ) -> Result<FsNodeHandle, Errno> {
        self.create_child(
            node,
            current_task,
            Some(name),
            FsNodeInfo::new(mode, owner),
            fio::Flags::FLAG_MUST_CREATE | fio::Flags::PROTOCOL_DIRECTORY,
            fio::MutableNodeAttributes::default(),
        )
    }

    fn lookup(
        &self,
        entry: &DirEntry,
        current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<FsNodeHandle, Errno> {
        let name = get_name_str(name)?;

        let fs = entry.node.fs();
        let fs_ops = RemoteFs::<I>::from_fs(&fs);

        let mut query = MODE_ATTRIBUTES
            | NODE_INFO_ATTRIBUTES
            | fio::NodeAttributesQuery::ID
            | fio::NodeAttributesQuery::UID
            | fio::NodeAttributesQuery::GID
            | fio::NodeAttributesQuery::RDEV
            | fio::NodeAttributesQuery::ENCRYPTION_POLICY
            | fio::NodeAttributesQuery::VERITY_ENABLED
            | fio::NodeAttributesQuery::CASEFOLD;

        if security::fs_is_xattr_labeled(fs.clone()) {
            query |= fio::NodeAttributesQuery::SELINUX_CONTEXT;
        }

        self.node
            .io
            .open(name, fs_ops.root_rights, None, query, LookupFactory::new(&fs, current_task))
            .map_err(|status| from_status_like_fdio!(status, name))?
    }

    fn has_lookup_pipelined(&self) -> bool {
        true
    }

    fn lookup_pipelined(
        &self,
        entry: &DirEntry,
        current_task: &CurrentTask,
        names: &[&FsStr],
    ) -> LookupVec<Result<FsNodeHandle, Errno>> {
        let fs = entry.node.fs();
        let fs_ops = RemoteFs::<I>::from_fs(&fs);

        let mut query = MODE_ATTRIBUTES
            | NODE_INFO_ATTRIBUTES
            | fio::NodeAttributesQuery::ID
            | fio::NodeAttributesQuery::UID
            | fio::NodeAttributesQuery::GID
            | fio::NodeAttributesQuery::RDEV
            | fio::NodeAttributesQuery::ENCRYPTION_POLICY
            | fio::NodeAttributesQuery::VERITY_ENABLED
            | fio::NodeAttributesQuery::CASEFOLD;

        if security::fs_is_xattr_labeled(fs.clone()) {
            query |= fio::NodeAttributesQuery::SELINUX_CONTEXT;
        }

        let names_str =
            match names.iter().map(|n| get_name_str(n)).collect::<Result<LookupVec<_>, Errno>>() {
                Ok(names_str) => names_str,
                Err(e) => return vec![Err(e)].into(),
            };

        self.node
            .io
            .open_pipelined(&names_str, fs_ops.root_rights, query, || {
                LookupFactory::new(&fs, current_task)
            })
            .map(|r| r.map_err(|status| from_status_like_fdio!(status)).flatten())
            .collect()
    }

    fn truncate(
        &self,
        _guard: &AppendLockWriteGuard<'_>,
        node: &FsNode,
        current_task: &CurrentTask,
        length: u64,
    ) -> Result<(), Errno> {
        node.fail_if_locked(current_task, &node.info())?;

        let _guard = self.node.info_state.dirty_op_guard(true);

        self.node.io.truncate(length).map_err(|status| from_status_like_fdio!(status))
    }

    fn allocate(
        &self,
        _guard: &AppendLockWriteGuard<'_>,
        node: &FsNode,
        current_task: &CurrentTask,
        mode: FallocMode,
        offset: u64,
        length: u64,
    ) -> Result<(), Errno> {
        match mode {
            FallocMode::Allocate { keep_size } => {
                node.fail_if_locked(current_task, &node.info())?;

                let allocate_mode =
                    if keep_size { AllocateMode::KEEP_SIZE } else { AllocateMode::empty() };

                will_dirty(&[&self.node], || {
                    self.node
                        .io
                        .allocate(offset, length, allocate_mode)
                        .map_err(|status| from_status_like_fdio!(status))
                })?;
                Ok(())
            }
            FallocMode::PunchHole => {
                node.fail_if_locked(current_task, &node.info())?;

                will_dirty(&[&self.node], || {
                    match self.node.io.allocate(
                        offset,
                        length,
                        AllocateMode::PUNCH_HOLE | AllocateMode::KEEP_SIZE,
                    ) {
                        Ok(()) => Ok(()),
                        Err(zx::Status::NOT_SUPPORTED) => Ok(()),
                        Err(status) => Err(from_status_like_fdio!(status)),
                    }
                })?;
                Ok(())
            }
            _ => error!(EINVAL),
        }
    }

    fn fetch_and_refresh_info<'a>(
        &self,
        _node: &FsNode,
        _current_task: &CurrentTask,
        info: &'a DynamicLockDepRwLock<FsNodeInfo>,
        flags: StatxFlags,
    ) -> Result<LockDepReadGuard<'a, FsNodeInfo>, Errno> {
        if flags.contains(StatxFlags::AT_STATX_DONT_SYNC) {
            return Ok(info.read());
        }
        self.node.fetch_and_refresh_info(info)
    }

    fn update_attributes(
        &self,
        _node: &FsNode,
        _current_task: &CurrentTask,
        info: &FsNodeInfo,
        has: zxio_node_attr_has_t,
    ) -> Result<(), Errno> {
        // Attributes of regular remote nodes (files, directory) are valid to update.
        // Their metadata is stored and managed by the underlying Fuchsia filesystem.
        self.node.update_attributes(info, has)
    }

    fn unlink(
        &self,
        node: &FsNode,
        _current_task: &CurrentTask,
        name: &FsStr,
        child: &FsNodeHandle,
    ) -> Result<(), Errno> {
        // We don't care about the child argument because 1. unlinking already takes the parent's
        // children lock, so we don't have to worry about conflicts on this path, and 2. the remote
        // filesystem tracks the link counts so we don't need to update them here.
        let name = get_name_str(name)?;
        will_dirty::<I, _, _>(&[node, child], || {
            self.node
                .io
                .unlink(name, fio::UnlinkFlags::empty())
                .map_err(|status| from_status_like_fdio!(status))
        })
    }

    fn create_symlink(
        &self,
        node: &FsNode,
        current_task: &CurrentTask,
        name: &FsStr,
        target: &FsStr,
        owner: FsCred,
    ) -> Result<FsNodeHandle, Errno> {
        node.fail_if_locked(current_task, &node.info())?;

        let name = get_name_str(name)?;
        let io = will_dirty(&[&self.node], || {
            self.node
                .io
                .create_symlink(name, target)
                .map_err(|status| from_status_like_fdio!(status))
        })?;

        let fs = node.fs();
        let fs_ops = RemoteFs::<I>::from_fs(&fs);

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
            RemoteSymlink::new(BaseNode::new(io, true), target.as_bytes()),
            FsNodeInfo {
                size: target.len(),
                ..FsNodeInfo::new(FileMode::IFLNK | FileMode::ALLOW_ALL, owner)
            },
        ))
    }

    fn create_tmpfile(
        &self,
        node: &FsNode,
        current_task: &CurrentTask,
        mode: FileMode,
        owner: FsCred,
    ) -> Result<FsNodeHandle, Errno> {
        if !mode.is_reg() {
            return error!(EINVAL);
        }

        // `create_tmpfile` is used by O_TMPFILE. Note that
        // <https://man7.org/linux/man-pages/man2/open.2.html> states that if O_EXCL is specified
        // with O_TMPFILE, the temporary file created cannot be linked into the filesystem. Although
        // there exist fuchsia flags `fio::FLAG_TEMPORARY_AS_NOT_LINKABLE`, the starnix vfs already
        // handles this case and makes sure that the created file is not linkable. There is also no
        // way of passing the open flags to this function.
        self.create_child(
            node,
            current_task,
            None,
            FsNodeInfo::new(mode, owner),
            fio::Flags::FLAG_CREATE_AS_UNNAMED_TEMPORARY | fio::Flags::PROTOCOL_FILE,
            fio::MutableNodeAttributes::default(),
        )
    }

    fn link(
        &self,
        node: &FsNode,
        _current_task: &CurrentTask,
        name: &FsStr,
        child: &FsNodeHandle,
    ) -> Result<(), Errno> {
        if !RemoteFs::<I>::from_fs(&node.fs()).use_remote_ids {
            return error!(EPERM);
        }
        let name = get_name_str(name)?;

        will_dirty::<I, _, _>(&[node, child], || {
            if let Some(child) = child.downcast_ops::<RemoteNode<I>>() {
                child.node.io.link_into(&self.node.io, name).map_err(|status| match status {
                    zx::Status::BAD_STATE => errno!(EXDEV),
                    zx::Status::ACCESS_DENIED => errno!(ENOKEY),
                    s => from_status_like_fdio!(s),
                })
            } else if let Some(child) = child.downcast_ops::<RemoteSymlink<I>>() {
                child.node.io.link_into(&self.node.io, name).map_err(|status| match status {
                    zx::Status::BAD_STATE => errno!(EXDEV),
                    zx::Status::ACCESS_DENIED => errno!(ENOKEY),
                    s => from_status_like_fdio!(s),
                })
            } else {
                error!(EXDEV)
            }
        })
    }

    fn forget(self: Box<Self>, _current_task: &CurrentTask, info: FsNodeInfo) -> Result<(), Errno> {
        // Before forgetting this node, update atime if we need to.
        if info.pending_time_access_update {
            self.node
                .io
                .attr_get(fio::NodeAttributesQuery::PENDING_ACCESS_TIME_UPDATE)
                .map_err(|status| from_status_like_fdio!(status))?;
        }
        Ok(())
    }

    fn enable_fsverity(
        &self,
        _node: &FsNode,
        _current_task: &CurrentTask,
        descriptor: &fsverity_descriptor,
    ) -> Result<(), Errno> {
        let descr = zxio_fsverity_descriptor_t {
            hash_algorithm: descriptor.hash_algorithm,
            salt_size: descriptor.salt_size,
            salt: descriptor.salt,
        };
        will_dirty(&[&self.node], || {
            self.node.io.enable_verity(&descr).map_err(|status| from_status_like_fdio!(status))
        })
    }

    fn get_fsverity_descriptor(&self, log_blocksize: u8) -> Result<fsverity_descriptor, Errno> {
        let (_, attrs) = self
            .node
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

    fn get_size(&self, node: &FsNode, current_task: &CurrentTask) -> Result<usize, Errno> {
        if self.node.info_state.is_size_accurate() {
            node.info()
        } else {
            node.fetch_and_refresh_info(current_task)?
        }
        .size
        .try_into()
        .map_err(|_| errno!(EINVAL))
    }
}

struct RemoteSpecialNode<I> {
    node: BaseNode<I>,
}

impl<I: SyncIo> FsNodeOps for RemoteSpecialNode<I> {
    fs_node_impl_not_dir!();
    fs_node_impl_xattr_delegate!(self, self.node);

    fn create_file_ops(
        &self,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        unreachable!("Special nodes cannot be opened.");
    }

    fn update_attributes(
        &self,
        _node: &FsNode,
        _current_task: &CurrentTask,
        info: &FsNodeInfo,
        has: zxio_node_attr_has_t,
    ) -> Result<(), Errno> {
        // Attributes of special remote nodes (sockets, devices, etc.) are valid to update.
        // Their metadata is stored and managed by the underlying Fuchsia filesystem.
        self.node.update_attributes(info, has)
    }
}

struct RemoteDirectoryObject<D>(D);

impl<D> RemoteDirectoryObject<D> {
    fn new(dir: D) -> Self {
        Self(dir)
    }
}

impl<D: SyncIoDirectory> FileOps for RemoteDirectoryObject<D> {
    fileops_impl_directory!();

    fn seek(
        &self,
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
        file: &FileObject,
        _current_task: &CurrentTask,
        sink: &mut dyn DirentSink,
    ) -> Result<(), Errno> {
        match self
            .0
            .readdir(|mut inode_num, entry_type, name| {
                if name == b"." {
                    inode_num = file.name.entry.node.ino;
                } else if name == b".." {
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
        file: &FileObject,
        current_task: &CurrentTask,
    ) -> Result<Option<zx::NullableHandle>, Errno> {
        // If expose a handle to a directory to a Fuchsia component, we trust that it will not
        // modify the directory in a way that will confuse Starnix.
        match self.0.clone_proxy().map_err(map_sync_io_client_error)? {
            Some(proxy) => Ok(Some(proxy.into_channel().into())),
            None => serve_file_tagged(
                current_task,
                file,
                current_task.current_creds().clone(),
                "remote_files",
            )
            .map(|c| Some(c.0.into_channel().into())),
        }
    }
}

fn get_or_init_remote_memory(
    file: &FileObject,
    read_only_memory: &OnceCell<Arc<MemoryObject>>,
    read_exec_memory: &OnceCell<Arc<MemoryObject>>,
    prot: ProtectionFlags,
    fetch: impl Fn(ProtectionFlags) -> Result<Arc<MemoryObject>, Errno>,
) -> Result<Arc<MemoryObject>, Errno> {
    fuchsia_trace::duration!(
        CATEGORY_STARNIX_MM,
        "RemoteFileGetVmo",
        "fs" => std::str::from_utf8(file.fs.name()).unwrap_or("<non-utf8>"),
        "source" => std::str::from_utf8(file.fs.options.source_for_display()).unwrap_or("<non-utf8>"),
        "ino" => file.node().ino,
        "prot" => prot.bits(),
        "cached" => if prot == ProtectionFlags::READ {
            read_only_memory.get().is_some()
        } else if prot == (ProtectionFlags::READ | ProtectionFlags::EXEC) {
            // The executable handle can be derived locally from a cached read-only handle, so
            // either cache being populated means the FIDL round-trip is avoided.
            read_exec_memory.get().is_some() || read_only_memory.get().is_some()
        } else {
            false
        },
    );
    if prot == ProtectionFlags::READ {
        read_only_memory.get_or_try_init(|| fetch(ProtectionFlags::READ)).cloned()
    } else if prot == (ProtectionFlags::READ | ProtectionFlags::EXEC) {
        read_exec_memory
            .get_or_try_init(|| {
                // If a read-only handle has already been fetched, derive the executable handle
                // from it rather than making another GetBackingMemory round-trip. The server is
                // not required to grant ZX_RIGHT_DUPLICATE on the returned VMO, so fall back to
                // fetching if the handle cannot be duplicated.
                if let Some(read_only) = read_only_memory.get()
                    && let Ok(exec_mem) = read_only
                        .duplicate_handle(zx::Rights::SAME_RIGHTS)
                        .and_then(|m| m.replace_as_executable(&VMEX_RESOURCE))
                {
                    return Ok(Arc::new(exec_mem));
                }
                fetch(prot)
            })
            .cloned()
    } else {
        fetch(prot)
    }
}

struct RemoteFileObject<I> {
    was_written: AtomicBool,

    _phantom: std::marker::PhantomData<fn() -> I>,
}

impl<I> Default for RemoteFileObject<I> {
    fn default() -> Self {
        Self { was_written: AtomicBool::new(false), _phantom: std::marker::PhantomData }
    }
}

impl<I: SyncIo> RemoteFileObject<I> {
    /// # Panics
    ///
    /// This will panic if the node's ops are not `RemoteNode`; `AnonymousRemoteFileObject` should
    /// be used if this won't be the case.
    fn remote_node(file: &FileObject) -> &RemoteNode<I> {
        file.node().downcast_ops::<RemoteNode<I>>().unwrap()
    }

    fn io(file: &FileObject) -> &I {
        &Self::remote_node(file).node.io
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

impl<T: SyncIo> RemoteIoExt for T {
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

impl<I: SyncIo> FileOps for RemoteFileObject<I> {
    fileops_impl_seekable!();

    fn flush(&self, file: &FileObject, _current_task: &CurrentTask) {
        if self.was_written.swap(false, Ordering::Relaxed)
            && let Some(node) = file.node().downcast_ops::<RemoteNode<RemoteIo>>()
            && let Ok(proxy) = node.node.io.clone_proxy()
        {
            let _ = proxy.close(zx::MonotonicInstant::INFINITE);
        }
    }

    fn read(
        &self,
        file: &FileObject,
        _current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        Self::io(file).read_to_output_buffer(offset as u64, data)
    }

    fn write(
        &self,
        file: &FileObject,
        _current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        will_dirty::<I, _, _>(&[&***file.node()], || {
            let written = Self::io(file).write_from_input_buffer(offset as u64, data)?;

            // If we increased the file size, we need to update that here so that `NodeInfo::size`
            // is accurate.  This is done so that we can optimize `get_size`.  If the file has been
            // truncated, then the size might not be accurate, but we track that separately and
            // `get_size` will fetch the file size from the remote end in that case.
            if written > 0 {
                self.was_written.store(true, Ordering::Relaxed);
                file.node().update_info(|info| {
                    if offset + written > info.size {
                        info.size = offset + written;
                    }
                });
            }

            Ok(written)
        })
    }

    fn get_memory(
        &self,
        file: &FileObject,
        _current_task: &CurrentTask,
        _length: Option<usize>,
        prot: ProtectionFlags,
    ) -> Result<Arc<MemoryObject>, Errno> {
        Self::remote_node(file).get_memory(file, prot)
    }

    fn to_handle(
        &self,
        file: &FileObject,
        current_task: &CurrentTask,
    ) -> Result<Option<zx::NullableHandle>, Errno> {
        // To avoid cache coherency and security issues, we proxy remote files via the Starnix file
        // server.  This will incur a performance penalty which we can optimize later if we need to.
        serve_file_tagged(current_task, file, current_task.current_creds().clone(), "remote_files")
            .map(|c| Some(c.0.into_channel().into()))
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
        _file: &FileObject,
        _current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        self.io.read_to_output_buffer(offset as u64, data)
    }

    fn write(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        // As this is an anonymous file, there's no point marking the node info dirty because this
        // isn't backed by `RemoteNode` or `RemoteSymlink`.
        self.io.write_from_input_buffer(offset as u64, data)
    }

    fn get_memory(
        &self,
        file: &FileObject,
        _current_task: &CurrentTask,
        _length: Option<usize>,
        prot: ProtectionFlags,
    ) -> Result<Arc<MemoryObject>, Errno> {
        get_or_init_remote_memory(file, &self.read_only_memory, &self.read_exec_memory, prot, |p| {
            self.io.fetch_remote_memory(p)
        })
    }

    fn to_handle(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
    ) -> Result<Option<zx::NullableHandle>, Errno> {
        // This is an anonymous file (not backed by `RemoteNode`).  Any external updates to the
        // file's attributes will not be tracked by Starnix.
        self.io
            .clone_proxy()
            .map_err(map_sync_io_client_error)
            .map(|p| Some(p.into_channel().into()))
    }

    fn sync(&self, _file: &FileObject, _current_task: &CurrentTask) -> Result<(), Errno> {
        self.io.sync().map_err(map_sync_io_client_error)
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
        file: &FileObject,
        _current_task: &CurrentTask,
        _length: Option<usize>,
        prot: ProtectionFlags,
    ) -> Result<Arc<MemoryObject>, Errno> {
        get_or_init_remote_memory(file, &self.read_only_memory, &self.read_exec_memory, prot, |p| {
            self.fetch_remote_memory(p)
        })
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
}

/// Returns true if `file` is a `RemoteFileObject`.
pub fn is_remote_file(file: &FileObject) -> bool {
    file.downcast_file::<RemoteFileObject<RemoteIo>>().is_some()
}

/// [`FileOps`] implementation for Fuchsia stream/datagram pipes backed by [`Zxio`].
struct RemotePipeObject {
    zxio: Zxio,
}

impl RemotePipeObject {
    fn new(zxio: Zxio) -> Self {
        Self { zxio }
    }
}

impl FileOps for RemotePipeObject {
    fileops_impl_nonseekable!();
    fileops_impl_noop_sync!();

    fn read(
        &self,
        file: &FileObject,
        current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        debug_assert!(offset == 0);
        if data.available() == 0 {
            return Ok(0);
        }
        file.blocking_op(current_task, FdEvents::POLLIN | FdEvents::POLLHUP, None, || {
            let read_bytes = with_iovec_segments(data, |iovecs| {
                // SAFETY: The `iovecs` are valid for writing because they come from `OutputBuffer`.
                match unsafe { self.zxio.readv(iovecs) } {
                    Ok(actual) => Ok(actual),
                    Err(zx::Status::PEER_CLOSED | zx::Status::BAD_STATE) => Ok(0),
                    Err(status) => Err(map_stream_error(status)),
                }
            });
            match read_bytes {
                Some(actual) => {
                    let actual = actual?;
                    // SAFETY: We successfully read `actual` bytes directly to the buffer segments.
                    unsafe { data.advance(actual) }?;
                    Ok(actual)
                }
                None => {
                    // Perform the (slower) operation by using an intermediate buffer.
                    let total = data.available();
                    let mut bytes = vec![0u8; total];
                    let actual = match self.zxio.read(&mut bytes) {
                        Ok(actual) => actual,
                        Err(zx::Status::PEER_CLOSED | zx::Status::BAD_STATE) => return Ok(0),
                        Err(status) => return Err(from_status_like_fdio!(status)),
                    };
                    data.write_all(&bytes[0..actual])
                }
            }
        })
    }

    fn write(
        &self,
        file: &FileObject,
        current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        debug_assert!(offset == 0);
        file.blocking_op(current_task, FdEvents::POLLOUT | FdEvents::POLLHUP, None, || {
            let actual = match with_iovec_segments(data, |iovecs| {
                if iovecs.is_empty() {
                    return self.zxio.write(&[]).map_err(|status| from_status_like_fdio!(status));
                }
                // SAFETY: The `iovecs` are valid for reading because they come from `InputBuffer`.
                unsafe { self.zxio.writev(iovecs).map_err(map_stream_error) }
            }) {
                Some(actual) => actual?,
                None => {
                    // Perform the (slower) operation by using an intermediate buffer.
                    let bytes = data.peek_all()?;
                    self.zxio.write(&bytes).map_err(|status| from_status_like_fdio!(status))?
                }
            };
            data.advance(actual)?;
            Ok(actual)
        })
    }

    fn wait_async(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        waiter: &Waiter,
        events: FdEvents,
        handler: EventHandler,
    ) -> Option<WaitCanceler> {
        Some(zxio_wait_async(&self.zxio, waiter, events, handler))
    }

    fn query_events(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
    ) -> Result<FdEvents, Errno> {
        zxio_query_events(&self.zxio)
    }

    fn to_handle(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
    ) -> Result<Option<zx::NullableHandle>, Errno> {
        self.zxio
            .deep_clone()
            .and_then(Zxio::release)
            .map(Some)
            .map_err(|status| from_status_like_fdio!(status))
    }
}

struct RemoteSymlink<I> {
    node: BaseNode<I>,
    target: LockDepRwLock<Box<[u8]>, FuchsiaRemoteTargetLock>,
}

impl<I> RemoteSymlink<I> {
    fn new(node: BaseNode<I>, target: impl Into<Box<[u8]>>) -> Self {
        Self { node, target: LockDepRwLock::new(target.into()) }
    }
}

impl<I: SyncIo> FsNodeOps for RemoteSymlink<I> {
    fs_node_impl_symlink!();
    fs_node_impl_xattr_delegate!(self, self.node);

    fn readlink(
        &self,
        _node: &FsNode,
        _current_task: &CurrentTask,
    ) -> Result<SymlinkTarget, Errno> {
        Ok(SymlinkTarget::Path(FsString::new(self.target.read().to_vec())))
    }

    fn fetch_and_refresh_info<'a>(
        &self,
        _node: &FsNode,
        _current_task: &CurrentTask,
        info: &'a DynamicLockDepRwLock<FsNodeInfo>,
        flags: StatxFlags,
    ) -> Result<LockDepReadGuard<'a, FsNodeInfo>, Errno> {
        if flags.contains(StatxFlags::AT_STATX_DONT_SYNC) {
            return Ok(info.read());
        }
        self.node.fetch_and_refresh_info(info)
    }

    fn forget(self: Box<Self>, _current_task: &CurrentTask, info: FsNodeInfo) -> Result<(), Errno> {
        // Before forgetting this node, update atime if we need to.
        if info.pending_time_access_update {
            self.node
                .io
                .attr_get(fio::NodeAttributesQuery::PENDING_ACCESS_TIME_UPDATE)
                .map_err(|status| from_status_like_fdio!(status))?;
        }
        Ok(())
    }
}

pub struct RemoteCounter {
    counter: Counter,
    koid: std::sync::OnceLock<zx::Koid>,
}

impl RemoteCounter {
    fn new(counter: Counter) -> Self {
        Self { counter, koid: std::sync::OnceLock::new() }
    }

    pub fn duplicate_handle(&self) -> Result<Counter, Errno> {
        self.counter.duplicate_handle(zx::Rights::SAME_RIGHTS).map_err(impossible_error)
    }

    pub fn koid(&self) -> zx::Koid {
        *self.koid.get_or_init(|| self.counter.koid().unwrap())
    }
}

impl FileOps for RemoteCounter {
    fileops_impl_nonseekable!();
    fileops_impl_noop_sync!();

    fn read(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        _data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        error!(ENOTSUP)
    }

    fn write(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        _data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        error!(ENOTSUP)
    }

    fn ioctl(
        &self,
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
            let mut sync_points = Vec::with_capacity(1);
            let counter = self.duplicate_handle()?;
            // For other calls than SYNC_IOC_MERGE, the koid is never used, so we construct it
            // without fetching it, saving a Zircon syscall.
            let sp = if ioctl_number == SYNC_IOC_MERGE {
                SyncPoint::with_koid(Timeline::Hwc, counter.into(), self.koid())
            } else {
                SyncPoint::new(Timeline::Hwc, counter.into())
            };
            sync_points.push(sp);
            let sync_file_name: &[u8; 32] = b"remote counter\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0";
            let sync_file = SyncFile::new(*sync_file_name, SyncFence { sync_points });
            return sync_file.ioctl(file, current_task, request, arg);
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

/// Used to keep track of whether node info is in sync or dirty so that we can avoid communicating
/// exernally if we think the node information is in sync.
// The two top bits are special (see below).  The remaining bits are a count of the number of
// in-flight dirty operations.
struct InfoState(AtomicU32);

impl InfoState {
    /// When this bit is set and the PENDING_REFRESH bit is *not* set, the node information is in
    /// sync with the external node.
    const IN_SYNC: u32 = 0x8000_0000;

    /// When this bit is set in `info_state`, it means the node information is currently being
    /// refreshed.
    const PENDING_REFRESH: u32 = 0x4000_0000;

    /// When this bit is set, it means the node has been truncated and so the size might not be
    /// accurate.
    const TRUNCATED: u32 = 0x2000_0000;

    /// The remaining bits are used to track a count of the number of in-flight dirty operations.
    const COUNT_MASK: u32 = Self::TRUNCATED - 1;

    fn new(dirty: bool) -> Self {
        Self(AtomicU32::new(if dirty { 0 } else { Self::IN_SYNC }))
    }

    /// This guard should be taken whilst an operation that might result in dirty node information
    /// is in flight.  If `for_truncate` is true, this will also set the `TRUNCATED` bit.
    fn dirty_op_guard(&self, for_truncate: bool) -> DirtyOpGuard<'_> {
        // Increment the count indicating a dirty operation is in flight and also clear the
        // `IN_SYNC` bit to indicate the node information will need refreshing from its external
        // source.
        let mut current = self.0.load(Ordering::Relaxed);
        let for_truncate = if for_truncate { Self::TRUNCATED } else { 0 };
        loop {
            assert!(current & Self::COUNT_MASK != Self::COUNT_MASK); // Check overflow
            match self.0.compare_exchange_weak(
                current,
                ((current & !Self::IN_SYNC) + 1) | for_truncate,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(old) => current = old,
            }
        }
        DirtyOpGuard(self)
    }

    /// Calls `refresh` if node information needs to be refreshed, or `not_needed` if node
    /// information does not need refreshing.
    fn maybe_refresh<'a, T: 'a>(
        &self,
        info: &'a DynamicLockDepRwLock<FsNodeInfo>,
        refresh: impl FnOnce(&'a DynamicLockDepRwLock<FsNodeInfo>) -> Result<T, Errno>,
        not_needed: impl FnOnce(&'a DynamicLockDepRwLock<FsNodeInfo>) -> Result<T, Errno>,
    ) -> Result<T, Errno> {
        let mut current = self.0.load(Ordering::Relaxed);

        // If node information is dirty, and there are no pending dirty operations, and there is no
        // other thread currently refreshing node information, we can set the bits indicating that a
        // refresh is pending.  We want to set the `IN_SYNC` bit here in case `will_dirty` runs
        // before we're done.
        //
        // NOTE: Multiple threads can be refreshing at the same time, but only one of them will
        // succeed in setting the `PENDING_REFRESH` bit.
        let mut did_set_pending_refresh = false;
        while current & !Self::TRUNCATED == 0 {
            match self.0.compare_exchange_weak(
                current,
                current | Self::IN_SYNC | Self::PENDING_REFRESH,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    did_set_pending_refresh = true;
                    break;
                }
                Err(old) => current = old,
            }
        }

        // Skip the update if the cached information is in sync and there are no pending dirty
        // operations.  If there's a pending atime update, we'll skip updating that now; it
        // shouldn't be necessary and we can do it later.
        if current == Self::IN_SYNC {
            return not_needed(info);
        }

        let result = refresh(info);

        if did_set_pending_refresh {
            if result.is_ok() {
                // If the TRUNCATED bit was set, we can clear it now so long as no other dirty
                // operations took place.
                if current & Self::TRUNCATED != 0 {
                    // Assuming no other thread has changed the state, this is what we
                    // expect the current value to be.
                    let mut current = Self::TRUNCATED | Self::IN_SYNC | Self::PENDING_REFRESH;
                    while current == Self::TRUNCATED | Self::IN_SYNC | Self::PENDING_REFRESH {
                        match self.0.compare_exchange_weak(
                            current,
                            Self::IN_SYNC,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        ) {
                            Ok(_) => return result,
                            Err(old) => current = old,
                        }
                    }
                    // In this case, we fall through and just clear the PENDING_REFRESH bit, but we
                    // leave the TRUNCATED bit untouched.
                }
                self.0.fetch_and(!Self::PENDING_REFRESH, Ordering::Relaxed);
            } else {
                // If there was an error, we should also clear the IN_SYNC bit to indicate the node
                // information is still dirty.
                self.0.fetch_and(!(Self::IN_SYNC | Self::PENDING_REFRESH), Ordering::Relaxed);
            }
        }

        result
    }

    /// Returns true if the size is accurate.
    fn is_size_accurate(&self) -> bool {
        // The size returned by `get_size` is accurate so long as the file hasn't been truncated.
        // If there are writes currently outstanding, then it's also not safe to return the current
        // size.  To understand why, consider the following scenario:
        //
        //    1. Thread A issues a write.
        //    2. Thread B performs a read which sees the write from thread A.
        //    3. Thread B now tries to seek to the end of the file.  It should be consistent with
        //       the read in #2.
        //
        // #3 needs to see the end-of-file as it is after the write, but it's possible that thread A
        // hasn't updated the size yet even though the write has been completed at the remote end.
        // For that reason, whilst there are potential writes outstanding, we must ask the remote
        // end for the size.
        let state = self.0.load(Ordering::Relaxed);
        state & (Self::TRUNCATED | Self::COUNT_MASK) == 0
    }
}

struct DirtyOpGuard<'a>(&'a InfoState);

impl Drop for DirtyOpGuard<'_> {
    fn drop(&mut self) {
        // Decrement the count we took when we created the guard.
        self.0.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A wrapper to be used around calls that will end up making node info dirty.
fn will_dirty<'a, I: SyncIo, N: TryInto<&'a BaseNode<I>> + Copy, T>(
    nodes: &[N],
    f: impl FnOnce() -> T,
) -> T {
    // We are about to execute an operation that will make the cached information for one or more
    // nodes out of date, and we must deal with races.  If we mark the node as dirty first, another
    // thread could sneak in and refresh the node information before this operation has finished,
    // and then the information would be out of date.  If we only mark the node as dirty afterwards,
    // there is a window between when the operation completes and when we mark the node as dirty
    // where another thread could observe the changes caused by this operation, but still see old
    // node information.  So, the approach we take is to mark the node as dirty before the operation
    // starts, but indicate that this operation is ongoing.  Any threads that try and retrieve node
    // information will fetch fresh information, but, importantly, they'll leave the node marked as
    // dirty.  Once this operation has finished, we'll indicate this operation is no longer
    // in-flight, and then the next time information is refreshed, we'll mark the node information
    // as being in sync.

    let _guards: SmallVec<[_; 4]> = nodes
        .iter()
        .filter_map(|n| N::try_into(*n).ok())
        .map(|n| n.info_state.dirty_op_guard(false))
        .collect();

    f()
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::mm::PAGE_SIZE;
    use crate::task::dynamic_thread_spawner::SpawnRequestBuilder;
    use crate::testing::*;
    use crate::vfs::buffers::{VecInputBuffer, VecOutputBuffer};
    use crate::vfs::socket::{SocketFile, SocketMessageFlags};
    use crate::vfs::{EpollFileObject, LookupContext, Namespace, SymlinkMode, TimeUpdateType};
    use assert_matches::assert_matches;
    use fidl::endpoints::{ServerEnd, create_request_stream};
    use fidl_fuchsia_io as fio;
    use flyweights::FlyByteStr;
    use fuchsia_async as fasync;
    use fuchsia_runtime::UtcDuration;
    use futures::{FutureExt, StreamExt};
    use fxfs_testing::{TestFixture, TestFixtureOptions};
    use starnix_sync::{FsNodeInfoLevel, Mutex};
    use starnix_uapi::auth::Credentials;
    use starnix_uapi::errors::EINVAL;
    use starnix_uapi::file_mode::mode;
    use starnix_uapi::ino_t;
    use starnix_uapi::mount_flags::MountpointFlags;
    use starnix_uapi::open_flags::OpenFlags;
    use starnix_uapi::vfs::{EpollEvent, FdEvents};
    use std::collections::BTreeMap;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
    use storage_device::DeviceHolder;
    use storage_device::fake_device::FakeDevice;

    #[::fuchsia::test]
    async fn test_remote_uds() {
        spawn_kernel_and_run(async |current_task| {
            let (s1, s2) = zx::Socket::create_datagram();
            s2.write(&vec![0]).expect("write");
            let file = new_remote_file(&current_task, s1.into(), OpenFlags::RDWR)
                .expect("new_remote_file");
            assert!(file.node().is_sock());
            let socket_ops = file.downcast_file::<SocketFile>().unwrap();
            let flags = SocketMessageFlags::CTRUNC
                | SocketMessageFlags::TRUNC
                | SocketMessageFlags::NOSIGNAL
                | SocketMessageFlags::CMSG_CLOEXEC;
            let mut buffer = VecOutputBuffer::new(1024);
            let info = socket_ops
                .recvmsg(&current_task, &file, &mut buffer, flags, None)
                .expect("recvmsg");
            assert!(info.ancillary_data.is_empty());
            assert_eq!(info.message_length, 1);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_tree() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_EXECUTABLE;
            let (server, client) = zx::Channel::create();
            fdio::open("/pkg", rights, server).expect("failed to open /pkg");
            let fs = RemoteFs::new_fs(
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
                root.lookup_child(&current_task, &mut context, "nib".into()).err(),
                Some(errno!(ENOENT))
            );
            let mut context = LookupContext::default();
            root.lookup_child(&current_task, &mut context, "lib".into()).unwrap();

            let mut context = LookupContext::default();
            let _test_file = root
                .lookup_child(&current_task, &mut context, "data/tests/hello_starnix".into())
                .unwrap()
                .open(&current_task, OpenFlags::RDONLY)
                .unwrap();
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_blocking_io() {
        spawn_kernel_and_run(async |current_task| {
            let (client, server) = zx::Socket::create_stream();
            let pipe = create_fuchsia_pipe(&current_task, client, OpenFlags::RDWR).unwrap();
            assert!(pipe.node().is_fifo());

            let bytes = [0u8; 64];
            assert_eq!(bytes.len(), server.write(&bytes).unwrap());

            // Spawn a kthread to get the right lock context.
            let bytes_read = pipe.read(&current_task, &mut VecOutputBuffer::new(64)).unwrap();

            assert_eq!(bytes_read, bytes.len());
            assert_eq!(pipe.read(&current_task, &mut VecOutputBuffer::new(0)).unwrap(), 0);

            assert_eq!(
                pipe.write(&current_task, &mut VecInputBuffer::new(&bytes)).unwrap(),
                bytes.len()
            );
            let mut server_buf = [0u8; 64];
            assert_eq!(server.read(&mut server_buf).unwrap(), bytes.len());

            std::mem::drop(server);
            assert_eq!(pipe.read(&current_task, &mut VecOutputBuffer::new(64)).unwrap(), 0);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_poll() {
        spawn_kernel_and_run(async |current_task| {
            let (client, server) = zx::Socket::create_stream();
            let pipe = create_fuchsia_pipe(&current_task, client, OpenFlags::RDWR)
                .expect("create_fuchsia_pipe");
            let server_zxio = Zxio::create(server.into_handle()).expect("Zxio::create");

            assert_eq!(
                pipe.query_events(&current_task),
                Ok(FdEvents::POLLOUT | FdEvents::POLLWRNORM)
            );

            let epoll_object = EpollFileObject::new_file(&current_task);
            let epoll_file = epoll_object.downcast_file::<EpollFileObject>().unwrap();
            let event = EpollEvent::new(FdEvents::POLLIN, 0);
            epoll_file.add(&current_task, &pipe, &epoll_object, event).expect("poll_file.add");

            let fds = epoll_file.wait(&current_task, 1, zx::MonotonicInstant::ZERO).expect("wait");
            assert!(fds.is_empty());

            assert_eq!(server_zxio.write(&[0]).expect("write"), 1);

            assert_eq!(
                pipe.query_events(&current_task),
                Ok(FdEvents::POLLOUT
                    | FdEvents::POLLWRNORM
                    | FdEvents::POLLIN
                    | FdEvents::POLLRDNORM)
            );
            let fds = epoll_file.wait(&current_task, 1, zx::MonotonicInstant::ZERO).expect("wait");
            assert_eq!(fds.len(), 1);

            assert_eq!(pipe.read(&current_task, &mut VecOutputBuffer::new(64)).expect("read"), 1);

            assert_eq!(
                pipe.query_events(&current_task),
                Ok(FdEvents::POLLOUT | FdEvents::POLLWRNORM)
            );
            let fds = epoll_file.wait(&current_task, 1, zx::MonotonicInstant::ZERO).expect("wait");
            assert!(fds.is_empty());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_new_remote_directory() {
        spawn_kernel_and_run(async |current_task| {
            let (server, client) = zx::Channel::create();
            fdio::open("/pkg", fio::PERM_READABLE | fio::PERM_EXECUTABLE, server)
                .expect("failed to open /pkg");

            let fd = new_remote_file(&current_task, client.into(), OpenFlags::RDWR)
                .expect("new_remote_file");
            assert!(fd.node().is_dir());
            assert!(fd.to_handle(&current_task).expect("to_handle").is_some());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_new_remote_file() {
        spawn_kernel_and_run(async |current_task| {
            let (server, client) = zx::Channel::create();
            fdio::open("/pkg/meta/contents", fio::PERM_READABLE, server)
                .expect("failed to open /pkg/meta/contents");

            let fd = new_remote_file(&current_task, client.into(), OpenFlags::RDONLY)
                .expect("new_remote_file");
            assert!(fd.node().is_reg());
            assert!(fd.to_handle(&current_task).expect("to_handle").is_some());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_new_remote_counter() {
        spawn_kernel_and_run(async |current_task| {
            let counter = zx::Counter::create();

            let fd = new_remote_file(&current_task, counter.into(), OpenFlags::RDONLY)
                .expect("new_remote_file");
            assert!(fd.node().is_private());
            assert!(fd.to_handle(&current_task).expect("to_handle").is_some());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_new_remote_vmo() {
        spawn_kernel_and_run(async |current_task| {
            let vmo = zx::Vmo::create(*PAGE_SIZE).expect("Vmo::create");
            let fd = new_remote_file(&current_task, vmo.into(), OpenFlags::RDWR)
                .expect("new_remote_file");
            assert!(fd.node().is_reg());
            assert!(fd.node().is_private());
            assert_eq!(fd.node().info().size, *PAGE_SIZE as usize);
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

        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let fs = RemoteFs::new_fs(
                &kernel,
                client,
                FileSystemOptions { source: FlyByteStr::new(b"/"), ..Default::default() },
                fio::PERM_READABLE | fio::PERM_WRITABLE,
            )
            .expect("new_fs failed");
            let ns = Namespace::new(fs);
            let root = ns.root();
            let symlink_node = root
                .create_symlink(&current_task, LINK_PATH.into(), LINK_TARGET.into())
                .expect("symlink failed");
            assert_matches!(&*symlink_node.entry.node.info(), FsNodeInfo { size: LINK_SIZE, .. });

            let mut context = LookupContext::new(SymlinkMode::NoFollow);
            let child = root
                .lookup_child(&current_task, &mut context, "symlink".into())
                .expect("lookup_child failed");

            match child.readlink(&current_task).expect("readlink failed") {
                SymlinkTarget::Path(path) => assert_eq!(path, LINK_TARGET),
                SymlinkTarget::Node(_) => panic!("readlink returned SymlinkTarget::Node"),
            }
            // Ensure the size stat reports matches what is expected.
            let stat_result = child.entry.node.stat(&current_task).expect("stat failed");
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

        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let fs = RemoteFs::new_fs(
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
                .lookup_child(&current_task, &mut context, "symlink".into())
                .expect("lookup_child failed after remount");

            match child.readlink(&current_task).expect("readlink failed after remount") {
                SymlinkTarget::Path(path) => assert_eq!(path, LINK_TARGET),
                SymlinkTarget::Node(_) => {
                    panic!("readlink returned SymlinkTarget::Node after remount")
                }
            }
            // Ensure the size stat reports matches what is expected.
            let stat_result =
                child.entry.node.stat(&current_task).expect("stat failed after remount");
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
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let creds = Credentials::clone(&current_task.current_creds());
            current_task.set_creds(Credentials { euid: 1, fsuid: 1, egid: 2, fsgid: 2, ..creds });
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
                &kernel,
                client,
                FileSystemOptions { source: FlyByteStr::new(b"/"), ..Default::default() },
                rights,
            )
            .expect("new_fs failed");
            let ns = Namespace::new(fs);
            current_task.fs().set_umask(FileMode::from_bits(0));
            ns.root()
                .create_node(&current_task, "file".into(), FILE_MODE, DeviceId::NONE)
                .expect("create_node failed");
            ns.root()
                .create_node(&current_task, "dir".into(), DIR_MODE, DeviceId::NONE)
                .expect("create_node failed");
            ns.root()
                .create_node(&current_task, "dev".into(), BLK_MODE, DeviceId::RANDOM)
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

        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
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
                .lookup_child(&current_task, &mut context, "file".into())
                .expect("lookup_child failed");
            assert_matches!(
                &*child.entry.node.info(),
                FsNodeInfo { mode: FILE_MODE, uid: 1, gid: 2, rdev: DeviceId::NONE, .. }
            );
            let child = ns
                .root()
                .lookup_child(&current_task, &mut context, "dir".into())
                .expect("lookup_child failed");
            assert_matches!(
                &*child.entry.node.info(),
                FsNodeInfo { mode: DIR_MODE, uid: 1, gid: 2, rdev: DeviceId::NONE, .. }
            );
            let child = ns
                .root()
                .lookup_child(&current_task, &mut context, "dev".into())
                .expect("lookup_child failed");
            assert_matches!(
                &*child.entry.node.info(),
                FsNodeInfo { mode: BLK_MODE, uid: 1, gid: 2, rdev: DeviceId::RANDOM, .. }
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

        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
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
                .create_node(&current_task, "dir".into(), MODE, DeviceId::NONE)
                .expect("create_node failed");
            let sub_dir2 = sub_dir1
                .create_node(&current_task, "dir".into(), MODE, DeviceId::NONE)
                .expect("create_node failed");

            let dir_handle = ns
                .root()
                .entry
                .open_anonymous(&current_task, OpenFlags::RDONLY)
                .expect("open failed");

            #[derive(Default)]
            struct Sink {
                offset: off_t,
                dot_inode_num: u64,
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
                    if name == "." {
                        self.dot_inode_num = inode_num;
                        assert_eq!(entry_type, DirectoryEntryType::DIR);
                    } else if name == ".." {
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
            dir_handle.readdir(&current_task, &mut sink).expect("readdir failed");

            // inode_num for . and .. for the root should be the same as root.
            assert_eq!(sink.dot_inode_num, ns.root().entry.node.ino);
            assert_eq!(sink.dot_dot_inode_num, ns.root().entry.node.ino);

            let dir_handle = sub_dir1
                .entry
                .open_anonymous(&current_task, OpenFlags::RDONLY)
                .expect("open failed");
            let mut sink = Sink::default();
            dir_handle.readdir(&current_task, &mut sink).expect("readdir failed");

            // inode_num for . should be sub_dir1, and .. should be root.
            assert_eq!(sink.dot_inode_num, sub_dir1.entry.node.ino);
            assert_eq!(sink.dot_dot_inode_num, ns.root().entry.node.ino);

            let dir_handle = sub_dir2
                .entry
                .open_anonymous(&current_task, OpenFlags::RDONLY)
                .expect("open failed");
            let mut sink = Sink::default();
            dir_handle.readdir(&current_task, &mut sink).expect("readdir failed");

            // inode_num for . should be sub_dir2, and .. should be sub_dir1.
            assert_eq!(sink.dot_inode_num, sub_dir2.entry.node.ino);
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

        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
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
            root.create_node(&current_task, "fifo".into(), FIFO_MODE, DeviceId::NONE)
                .expect("create_node failed");
            let mut context = LookupContext::new(SymlinkMode::NoFollow);
            let fifo_node = root
                .lookup_child(&current_task, &mut context, "fifo".into())
                .expect("lookup_child failed");

            // Test that we get expected behaviour for RemoteSpecialNode operation, e.g.
            // test that truncate should return EINVAL
            match fifo_node.truncate(&current_task, 0) {
                Ok(_) => {
                    panic!("truncate passed for special node")
                }
                Err(errno) if errno == EINVAL => {}
                Err(e) => {
                    panic!("truncate failed with error {:?}", e)
                }
            };

            // Create regular RemoteNode
            root.create_node(&current_task, "file".into(), REG_MODE, DeviceId::NONE)
                .expect("create_node failed");
            let mut context = LookupContext::new(SymlinkMode::NoFollow);
            let reg_node = root
                .lookup_child(&current_task, &mut context, "file".into())
                .expect("lookup_child failed");

            // We should be able to perform truncate on regular files
            reg_node.truncate(&current_task, 0).expect("truncate failed");
        })
        .await;
        fixture.close().await;
    }

    #[::fuchsia::test]
    async fn test_hard_link() {
        let fixture = TestFixture::new().await;
        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed");

        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
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
                .create_node(&current_task, "file1".into(), mode!(IFREG, 0o666), DeviceId::NONE)
                .expect("create_node failed");
            ns.root()
                .entry
                .node
                .link(&current_task, &ns.root().mount, "file2".into(), &node.entry.node)
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

        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
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
                .lookup_child(&current_task, &mut context, "file1".into())
                .expect("lookup_child failed");
            let child2 = ns
                .root()
                .lookup_child(&current_task, &mut context, "file2".into())
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

        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
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
                .create_node(&current_task, "file".into(), MODE, DeviceId::NONE)
                .expect("create_node failed");
            // Enable verity on the file.
            let desc = fsverity_descriptor {
                version: 1,
                hash_algorithm: 1,
                salt_size: 32,
                log_blocksize: 12,
                ..Default::default()
            };
            file.entry.node.enable_fsverity(current_task, &desc).expect("enable fsverity failed");
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

        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
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
                .lookup_child(&current_task, &mut context, "file".into())
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

        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
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
                .create_node(&current_task, "file".into(), MODE, DeviceId::NONE)
                .expect("create_node failed");
            // Change the mode, this change should persist
            file.entry
                .node
                .chmod(&current_task, &file.mount, MODE | FileMode::ALLOW_ALL)
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

        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
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
                .lookup_child(&current_task, &mut context, "file".into())
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

        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
                &kernel,
                client,
                FileSystemOptions { source: FlyByteStr::new(b"/"), ..Default::default() },
                rights,
            )
            .expect("new_fs failed");

            let statfs = fs.statfs(&current_task).expect("statfs failed");
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

        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
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
            root.create_node(&current_task, "file".into(), REG_MODE, DeviceId::NONE)
                .expect("create_node failed");
            let mut context = LookupContext::new(SymlinkMode::NoFollow);
            let reg_node = root
                .lookup_child(&current_task, &mut context, "file".into())
                .expect("lookup_child failed");

            reg_node
                .entry
                .node
                .fallocate(&current_task, FallocMode::Allocate { keep_size: false }, 0, 20)
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

        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
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
            root.create_node(&current_task, "file".into(), REG_MODE, DeviceId::NONE)
                .expect("create_node failed");
            let mut context = LookupContext::new(SymlinkMode::NoFollow);
            let reg_node = root
                .lookup_child(&current_task, &mut context, "file".into())
                .expect("lookup_child failed");

            reg_node
                .entry
                .node
                .fallocate(&current_task, FallocMode::Allocate { keep_size: false }, 1, u64::MAX)
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

        let last_modified = spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
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
                .create_node(&current_task, "file".into(), MODE, DeviceId::NONE)
                .expect("create_node failed");
            // Write to file (this should update mtime (time_modify))
            let file = child.open(&current_task, OpenFlags::RDWR).expect("open failed");
            // Call `fetch_and_refresh_info(..)` to refresh `time_modify` with the time managed by the
            // underlying filesystem
            let time_before_write = child
                .entry
                .node
                .fetch_and_refresh_info(&current_task)
                .expect("fetch_and_refresh_info failed")
                .time_modify;
            let write_bytes: [u8; 5] = [1, 2, 3, 4, 5];
            let written = file
                .write(&current_task, &mut VecInputBuffer::new(&write_bytes))
                .expect("write failed");
            assert_eq!(written, write_bytes.len());
            let last_modified = child
                .entry
                .node
                .fetch_and_refresh_info(&current_task)
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
        let refreshed_modified_time = spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
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
                .lookup_child(&current_task, &mut context, "file".into())
                .expect("lookup_child failed");
            let last_modified = child
                .entry
                .node
                .fetch_and_refresh_info(&current_task)
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

        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
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
                .create_node(&current_task, "file".into(), MODE, DeviceId::NONE)
                .expect("create_node failed");

            let info_original = child
                .entry
                .node
                .fetch_and_refresh_info(&current_task)
                .expect("fetch_and_refresh_info failed")
                .clone();

            child
                .entry
                .node
                .update_atime_mtime(
                    &current_task,
                    &child.mount,
                    TimeUpdateType::Time(UtcInstant::from_nanos(30)),
                    TimeUpdateType::Omit,
                )
                .expect("update_atime_mtime failed");
            let info_after_update = child
                .entry
                .node
                .fetch_and_refresh_info(&current_task)
                .expect("fetch_and_refresh_info failed")
                .clone();
            assert_eq!(info_after_update.time_modify, info_original.time_modify);
            assert_eq!(info_after_update.time_access, UtcInstant::from_nanos(30));

            child
                .entry
                .node
                .update_atime_mtime(
                    &current_task,
                    &child.mount,
                    TimeUpdateType::Omit,
                    TimeUpdateType::Time(UtcInstant::from_nanos(50)),
                )
                .expect("update_atime_mtime failed");
            let info_after_update2 = child
                .entry
                .node
                .fetch_and_refresh_info(&current_task)
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

        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
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
                .create_node(&current_task, "file".into(), MODE, DeviceId::NONE)
                .expect("create_node failed");
            let file = child.open(&current_task, OpenFlags::RDWR).expect("open failed");
            // Call `fetch_and_refresh_info(..)` to refresh ctime and mtime with the time managed by the
            // underlying filesystem
            let (ctime_before_write, mtime_before_write) = {
                let info = child
                    .entry
                    .node
                    .fetch_and_refresh_info(&current_task)
                    .expect("fetch_and_refresh_info failed");
                (info.time_status_change, info.time_modify)
            };

            // Writing to a file should update ctime and mtime
            let write_bytes: [u8; 5] = [1, 2, 3, 4, 5];
            let written = file
                .write(&current_task, &mut VecInputBuffer::new(&write_bytes))
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
                    .fetch_and_refresh_info(&current_task)
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

        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
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
                    &current_task,
                    "dir".into(),
                    FileMode::ALLOW_ALL.with_type(FileMode::IFDIR),
                    DeviceId::NONE,
                )
                .expect("create_node failed");
            child
                .entry
                .node
                .update_attributes(&current_task, |info| {
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
        let casefold = spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
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
                .lookup_child(&current_task, &mut context, "dir".into())
                .expect("lookup_child failed");
            let casefold = child
                .entry
                .node
                .fetch_and_refresh_info(&current_task)
                .expect("fetch_and_refresh_info failed")
                .casefold;
            casefold
        })
        .await;
        assert!(casefold);

        fixture.close().await;
    }

    #[::fuchsia::test]
    async fn test_pending_access_time() {
        const TEST_FILE: &str = "test_file";

        let fixture = TestFixture::new().await;
        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed");
        let (server, client2) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed");

        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel.clone();

            let atime3 = {
                let fs = RemoteFs::new_fs(
                    &kernel,
                    client,
                    FileSystemOptions {
                        source: FlyByteStr::new(b"/"),
                        flags: FileSystemFlags::empty().into(),
                        ..Default::default()
                    },
                    fio::PERM_READABLE | fio::PERM_WRITABLE,
                )
                .expect("new_fs failed");

                let ns = Namespace::new_with_flags(fs, MountpointFlags::RELATIME);
                let (child, _status) = ns
                    .root()
                    .open_create_node(
                        &current_task,
                        TEST_FILE.into(),
                        FileMode::ALLOW_ALL.with_type(FileMode::IFREG),
                        DeviceId::NONE,
                    )
                    .expect("create_node failed");

                let atime1 = child.entry.node.info().time_access;

                std::thread::sleep(std::time::Duration::from_micros(1));

                let file_handle = child.open(&current_task, OpenFlags::RDWR).expect("open failed");

                file_handle
                    .read(&current_task, &mut VecOutputBuffer::new(10))
                    .expect("read failed");

                // Expect atime to have changed.
                let atime2 = child.entry.node.info().time_access;
                assert!(atime2 > atime1);

                std::thread::sleep(std::time::Duration::from_micros(1));

                file_handle
                    .read(&current_task, &mut VecOutputBuffer::new(10))
                    .expect("read failed");

                // And again.
                let atime3 = child.entry.node.info().time_access;
                assert!(atime3 > atime2);

                atime3
            };

            kernel.delayed_releaser.apply(current_task);

            // After dropping the filesystem, the atime should have been persistently updated.
            let fs = RemoteFs::new_fs(
                &kernel,
                client2,
                FileSystemOptions {
                    source: FlyByteStr::new(b"/"),
                    flags: FileSystemFlags::empty().into(),
                    ..Default::default()
                },
                fio::PERM_READABLE | fio::PERM_WRITABLE,
            )
            .expect("new_fs failed");

            let ns = Namespace::new_with_flags(fs, MountpointFlags::RELATIME);
            let child = ns
                .root()
                .lookup_child(
                    &current_task,
                    &mut LookupContext::new(Default::default()),
                    TEST_FILE.into(),
                )
                .expect("lookup_child failed");

            let atime4 = child.entry.node.info().time_access;

            assert!(atime4 >= atime3);
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

    #[::fuchsia::test]
    async fn test_cached_attribute_refresh_behavior() {
        let (client, mut stream) = create_request_stream::<fio::FileMarker>();
        let barrier = Arc::new(Barrier::new(2));
        let barrier_clone = barrier.clone();
        let get_attrs_count = Arc::new(AtomicU32::new(0));
        let get_attrs_count_clone = get_attrs_count.clone();

        let server_task = fasync::Task::spawn(async move {
            while let Some(Ok(request)) = stream.next().await {
                match request {
                    fio::FileRequest::GetAttributes { query: _, responder } => {
                        get_attrs_count_clone.fetch_add(1, Ordering::SeqCst);
                        let mutable_attrs = fio::MutableNodeAttributes { ..Default::default() };
                        let immutable_attrs = fio::ImmutableNodeAttributes {
                            id: Some(1),
                            link_count: Some(1),
                            ..Default::default()
                        };
                        responder.send(Ok((&mutable_attrs, &immutable_attrs))).unwrap();
                    }
                    fio::FileRequest::Resize { length: _, responder } => {
                        let barrier_clone = barrier_clone.clone();
                        fasync::Task::spawn(async move {
                            barrier_clone.async_wait().await;
                            barrier_clone.async_wait().await;
                            responder.send(Ok(())).unwrap();
                        })
                        .detach();
                    }
                    fio::FileRequest::Close { responder } => {
                        responder.send(Ok(())).unwrap();
                    }
                    _ => panic!("Unexpected request: {:?}", request),
                }
            }
        });

        fasync::unblock(move || {
            let io = RemoteIo::new(client.into_channel().into());
            let node = BaseNode::new(io, false);
            let info = DynamicLockDepRwLock::new::<FsNodeInfoLevel>(FsNodeInfo::default());

            // 1. Initial fetch. Should return cached info immediately.
            assert_eq!(get_attrs_count.load(Ordering::SeqCst), 0);
            {
                let _info = node.fetch_and_refresh_info(&info).expect("fetch failed");
            }
            assert_eq!(get_attrs_count.load(Ordering::SeqCst), 0);

            // 2. Spawn a thread to perform a dirty operation.
            std::thread::scope(|s| {
                s.spawn(|| {
                    will_dirty(&[&node], || {
                        node.io.truncate(0).expect("truncate failed");
                    });
                });

                // Wait for the operation to start.
                barrier.wait();

                // Now the node is dirty. Fetching attributes should trigger a request.
                {
                    let _info = node.fetch_and_refresh_info(&info).expect("fetch failed");
                }
                assert_eq!(get_attrs_count.load(Ordering::SeqCst), 1);

                // A second fetch should trigger another request.
                {
                    let _info = node.fetch_and_refresh_info(&info).expect("fetch failed");
                }
                assert_eq!(get_attrs_count.load(Ordering::SeqCst), 2);

                // Let the operation finish.
                barrier.wait();
            });

            // 3. Operation finished. The next fetch should trigger a request.
            {
                let _info = node.fetch_and_refresh_info(&info).expect("fetch failed");
            }
            assert_eq!(get_attrs_count.load(Ordering::SeqCst), 3);

            // 4. Subsequent fetch should return cached info.
            {
                let _info = node.fetch_and_refresh_info(&info).expect("fetch failed");
            }
            assert_eq!(get_attrs_count.load(Ordering::SeqCst), 3);
        })
        .await;

        server_task.await;
    }

    #[::fuchsia::test]
    async fn test_attribute_refresh_during_concurrent_dirty_operation() {
        let (client, mut stream) = create_request_stream::<fio::FileMarker>();
        let get_attrs_started = Arc::new(Barrier::new(2));
        let get_attrs_started_clone = get_attrs_started.clone();
        let finish_get_attrs = Arc::new(Barrier::new(2));
        let finish_get_attrs_clone = finish_get_attrs.clone();

        let resize_started = Arc::new(Barrier::new(2));
        let resize_started_clone = resize_started.clone();
        let finish_resize = Arc::new(Barrier::new(2));
        let finish_resize_clone = finish_resize.clone();

        let get_attrs_count = Arc::new(AtomicU32::new(0));
        let get_attrs_count_clone = get_attrs_count.clone();

        let server_task = fasync::Task::spawn(async move {
            while let Some(Ok(request)) = stream.next().await {
                match request {
                    fio::FileRequest::GetAttributes { query: _, responder } => {
                        let count = get_attrs_count_clone.fetch_add(1, Ordering::SeqCst);
                        let finish_get_attrs_clone = finish_get_attrs_clone.clone();
                        let get_attrs_started_clone = get_attrs_started_clone.clone();

                        fasync::Task::spawn(async move {
                            if count == 0 {
                                fasync::unblock(move || {
                                    get_attrs_started_clone.wait();
                                    finish_get_attrs_clone.wait();
                                })
                                .await;
                            }
                            let mutable_attrs = fio::MutableNodeAttributes { ..Default::default() };
                            let immutable_attrs = fio::ImmutableNodeAttributes {
                                id: Some(1),
                                link_count: Some(1),
                                ..Default::default()
                            };
                            responder.send(Ok((&mutable_attrs, &immutable_attrs))).unwrap();
                        })
                        .detach();
                    }
                    fio::FileRequest::Resize { length: _, responder } => {
                        let resize_started_clone = resize_started_clone.clone();
                        let finish_resize_clone = finish_resize_clone.clone();
                        fasync::Task::spawn(async move {
                            fasync::unblock(move || {
                                resize_started_clone.wait();
                                finish_resize_clone.wait();
                            })
                            .await;
                            responder.send(Ok(())).unwrap();
                        })
                        .detach();
                    }
                    fio::FileRequest::Close { responder } => {
                        responder.send(Ok(())).unwrap();
                    }
                    _ => panic!("Unexpected request: {:?}", request),
                }
            }
        });

        fasync::unblock(move || {
            let io = RemoteIo::new(client.into_channel().into());
            let node = BaseNode::new(io, true);
            let info = DynamicLockDepRwLock::new::<FsNodeInfoLevel>(FsNodeInfo::default());

            std::thread::scope(|s| {
                // 1. Start Refresh Thread
                let refresh_thread = s.spawn(|| {
                    let _info = node.fetch_and_refresh_info(&info).expect("fetch failed");
                });

                get_attrs_started.wait();

                // 2. Start Dirty Thread
                let dirty_thread = s.spawn(|| {
                    will_dirty(&[&node], || {
                        node.io.truncate(0).expect("truncate failed");
                    });
                });

                resize_started.wait();

                // 3. Allow GetAttributes to finish
                finish_get_attrs.wait();
                refresh_thread.join().unwrap();
                assert_eq!(get_attrs_count.load(Ordering::SeqCst), 1);

                // 4. Refresh #2 (Should fetch because dirty op is in flight)
                {
                    let _info = node.fetch_and_refresh_info(&info).expect("fetch failed");
                }
                assert_eq!(get_attrs_count.load(Ordering::SeqCst), 2);

                // 5. Allow Dirty Op to finish
                finish_resize.wait();
                dirty_thread.join().unwrap();

                // 6. Refresh #3 (Should fetch because dirty op finished, but state was 0)
                {
                    let _info = node.fetch_and_refresh_info(&info).expect("fetch failed");
                }
                assert_eq!(get_attrs_count.load(Ordering::SeqCst), 3);

                // 7. Refresh #4 (Should be cached)
                {
                    let _info = node.fetch_and_refresh_info(&info).expect("fetch failed");
                }
                assert_eq!(get_attrs_count.load(Ordering::SeqCst), 3);
            });
        })
        .await;

        server_task.await;
    }

    #[::fuchsia::test]
    async fn test_update_attributes_invalidates_cache() {
        let (client, mut stream) = create_request_stream::<fio::DirectoryMarker>();
        let get_attrs_count = Arc::new(AtomicU32::new(0));
        let get_attrs_count_clone = get_attrs_count.clone();

        let server_task = fasync::Task::spawn(async move {
            let mut sub_tasks = Vec::new();
            while let Some(Ok(request)) = stream.next().await {
                match request {
                    fio::DirectoryRequest::Open { path, object, flags, .. } => {
                        assert_eq!(path, ".", "Unexpected open() for non-self");
                        let get_attrs_count = get_attrs_count_clone.clone();
                        sub_tasks.push(fasync::Task::spawn(async move {
                            let (mut stream, control_handle) =
                                ServerEnd::<fio::DirectoryMarker>::new(object)
                                    .into_stream_and_control_handle();
                            assert!(flags.contains(fio::Flags::FLAG_SEND_REPRESENTATION));

                            // The Representation provides the initial attributes to cache.
                            let mutable_attributes =
                                fio::MutableNodeAttributes { ..Default::default() };
                            let immutable_attributes = fio::ImmutableNodeAttributes {
                                id: Some(1),
                                link_count: Some(1),
                                ..Default::default()
                            };
                            let info = fio::DirectoryInfo {
                                attributes: Some(fio::NodeAttributes2 {
                                    mutable_attributes,
                                    immutable_attributes,
                                }),
                                ..Default::default()
                            };
                            let _ = control_handle
                                .send_on_representation(fio::Representation::Directory(info));

                            while let Some(Ok(request)) = stream.next().await {
                                match request {
                                    fio::DirectoryRequest::GetAttributes {
                                        query: _,
                                        responder,
                                    } => {
                                        get_attrs_count.fetch_add(1, Ordering::SeqCst);
                                        let mutable_attrs =
                                            fio::MutableNodeAttributes { ..Default::default() };
                                        let immutable_attrs = fio::ImmutableNodeAttributes {
                                            id: Some(1),
                                            link_count: Some(1),
                                            ..Default::default()
                                        };
                                        responder
                                            .send(Ok((&mutable_attrs, &immutable_attrs)))
                                            .unwrap();
                                    }
                                    fio::DirectoryRequest::UpdateAttributes {
                                        payload: _,
                                        responder,
                                    } => {
                                        responder.send(Ok(())).unwrap();
                                    }
                                    fio::DirectoryRequest::Close { responder } => {
                                        responder.send(Ok(())).unwrap();
                                    }
                                    _ => {
                                        panic!("Unexpected request: {:?}", request)
                                    }
                                }
                            }
                        }));
                    }
                    fio::DirectoryRequest::Close { responder } => {
                        responder.send(Ok(())).unwrap();
                    }
                    fio::DirectoryRequest::QueryFilesystem { responder } => {
                        responder.send(0i32, None).unwrap();
                    }
                    _ => panic!("Unexpected request: {:?}", request),
                }
            }

            for sub_task in sub_tasks {
                let _ = sub_task.await;
            }
        });

        spawn_kernel_and_run(async move |current_task| {
            let fs = RemoteFs::new_fs(
                &current_task.kernel(),
                client.into_channel(),
                FileSystemOptions { source: FlyByteStr::new(b"."), ..Default::default() },
                fio::PERM_READABLE | fio::PERM_WRITABLE,
            )
            .expect("failed to mount test remote FS");

            // 1. Initial fetch. Should use cached information from OnRepresentation.
            {
                let _info =
                    fs.root().node.fetch_and_refresh_info(current_task).expect("fetch failed");
            }
            assert_eq!(get_attrs_count.load(Ordering::SeqCst), 0);

            // 2. Update attributes. This should dirty the node.
            fs.root()
                .node
                .update_attributes(current_task, |attrs| {
                    attrs.time_modify += UtcDuration::from_seconds(1);
                    Ok(())
                })
                .expect("update_attributes failed");

            // 3. Fetch again. Should trigger a request.
            {
                let _info =
                    fs.root().node.fetch_and_refresh_info(current_task).expect("fetch failed");
            }
            assert_eq!(get_attrs_count.load(Ordering::SeqCst), 1);

            // 4. Subsequent fetch should use cached information.
            {
                let _info =
                    fs.root().node.fetch_and_refresh_info(current_task).expect("fetch failed");
            }
            assert_eq!(get_attrs_count.load(Ordering::SeqCst), 1);
        })
        .await;

        server_task.await;
    }

    trait AsyncBarrier {
        async fn async_wait(&self);
    }

    impl AsyncBarrier for Arc<Barrier> {
        async fn async_wait(&self) {
            let this = self.clone();
            fasync::unblock(move || this.wait()).await;
        }
    }

    #[derive(Default)]
    struct MockRemoteFs {
        get_attrs_count: AtomicU32,
        get_backing_memory_count: AtomicU32,
        get_xattr_count: AtomicU32,
        set_xattr_count: AtomicU32,
        file_size: AtomicUsize,
        write_offsets: Mutex<Vec<u64>>,
        open_paths: Mutex<Vec<String>>,
        create_selinux_contexts: Mutex<Vec<(String, Option<Vec<u8>>)>>,
        xattrs: Mutex<BTreeMap<Vec<u8>, Vec<u8>>>,
        data: Mutex<Vec<u8>>,
        get_attrs_hook: Mutex<Option<futures::future::BoxFuture<'static, ()>>>,
        write_hook: Mutex<Option<futures::future::BoxFuture<'static, ()>>>,
    }

    impl MockRemoteFs {
        async fn handle_file_requests(
            self: Arc<Self>,
            mut stream: fio::FileRequestStream,
            control_handle: fio::FileControlHandle,
            attributes_query: fio::NodeAttributesQuery,
        ) {
            let size = self.file_size.load(Ordering::SeqCst) as u64;
            let selinux_context =
                if attributes_query.contains(fio::NodeAttributesQuery::SELINUX_CONTEXT) {
                    self.xattrs
                        .lock()
                        .get(b"security.selinux".as_slice())
                        .cloned()
                        .map(fio::SelinuxContext::Data)
                } else {
                    None
                };
            let info = fio::FileInfo {
                attributes: Some(fio::NodeAttributes2 {
                    mutable_attributes: fio::MutableNodeAttributes {
                        selinux_context,
                        ..Default::default()
                    },
                    immutable_attributes: fio::ImmutableNodeAttributes {
                        id: Some(2),
                        link_count: Some(1),
                        content_size: Some(size),
                        storage_size: Some(size),
                        ..Default::default()
                    },
                }),
                ..Default::default()
            };
            let _ = control_handle.send_on_representation(fio::Representation::File(info));
            while let Some(Ok(request)) = stream.next().await {
                match request {
                    fio::FileRequest::GetAttributes { responder, .. } => {
                        // Spawn a separate task so that we can handle concurrent calls.
                        let this = self.clone();
                        fasync::Task::spawn(async move {
                            this.get_attrs_count.fetch_add(1, Ordering::SeqCst);
                            let size = this.file_size.load(Ordering::SeqCst) as u64;
                            let hook = this.get_attrs_hook.lock().take();
                            if let Some(hook) = hook {
                                hook.await;
                            }
                            responder
                                .send(Ok((
                                    &fio::MutableNodeAttributes { ..Default::default() },
                                    &fio::ImmutableNodeAttributes {
                                        id: Some(2),
                                        link_count: Some(1),
                                        content_size: Some(size),
                                        storage_size: Some(size),
                                        ..Default::default()
                                    },
                                )))
                                .unwrap();
                        })
                        .detach();
                    }
                    fio::FileRequest::GetExtendedAttribute { name, responder } => {
                        self.get_xattr_count.fetch_add(1, Ordering::SeqCst);
                        match self.xattrs.lock().get(&name).cloned() {
                            Some(val) => {
                                responder.send(Ok(fio::ExtendedAttributeValue::Bytes(val))).unwrap()
                            }
                            None => responder.send(Err(zx::Status::NOT_FOUND.into_raw())).unwrap(),
                        }
                    }
                    fio::FileRequest::SetExtendedAttribute { name, value, responder, .. } => {
                        self.set_xattr_count.fetch_add(1, Ordering::SeqCst);
                        if let fio::ExtendedAttributeValue::Bytes(bytes) = value {
                            self.xattrs.lock().insert(name, bytes);
                        }
                        responder.send(Ok(())).unwrap();
                    }
                    fio::FileRequest::RemoveExtendedAttribute { name, responder } => {
                        if self.xattrs.lock().remove(&name).is_some() {
                            responder.send(Ok(())).unwrap();
                        } else {
                            responder.send(Err(zx::Status::NOT_FOUND.into_raw())).unwrap();
                        }
                    }
                    fio::FileRequest::ReadAt { count, offset, responder } => {
                        let data = self.data.lock();
                        let start = std::cmp::min(offset as usize, data.len());
                        let end = std::cmp::min(start + count as usize, data.len());
                        responder.send(Ok(&data[start..end])).unwrap();
                    }
                    fio::FileRequest::WriteAt { offset, data, responder, .. } => {
                        // Spawn a separate task so that we can test concurrent writes.
                        let self_clone = Arc::clone(&self);
                        fasync::Task::spawn(async move {
                            self_clone.write_offsets.lock().push(offset);
                            let end = offset as usize + data.len();
                            {
                                let mut mock_data = self_clone.data.lock();
                                if end > mock_data.len() {
                                    mock_data.resize(end, 0);
                                }
                                mock_data[offset as usize..end].copy_from_slice(&data);
                            }
                            let mut current_size = self_clone.file_size.load(Ordering::SeqCst);
                            while end > current_size {
                                match self_clone.file_size.compare_exchange_weak(
                                    current_size,
                                    end,
                                    Ordering::SeqCst,
                                    Ordering::SeqCst,
                                ) {
                                    Ok(_) => break,
                                    Err(actual) => current_size = actual,
                                }
                            }
                            let hook = self_clone.write_hook.lock().take();
                            if let Some(hook) = hook {
                                hook.await;
                            }
                            responder.send(Ok(data.len() as u64)).unwrap();
                        })
                        .detach();
                    }
                    fio::FileRequest::Resize { length, responder, .. } => {
                        self.file_size.store(length as usize, Ordering::SeqCst);
                        responder.send(Ok(())).unwrap();
                    }
                    fio::FileRequest::Seek { origin, offset, responder } => {
                        let new_offset = match origin {
                            fio::SeekOrigin::Start => offset as u64,
                            fio::SeekOrigin::Current => 0,
                            fio::SeekOrigin::End => {
                                (self.file_size.load(Ordering::SeqCst) as i64 + offset) as u64
                            }
                        };
                        responder.send(Ok(new_offset)).unwrap();
                    }
                    fio::FileRequest::GetBackingMemory { flags, responder } => {
                        self.get_backing_memory_count.fetch_add(1, Ordering::SeqCst);
                        let vmo = zx::Vmo::create(4096).unwrap();
                        // Mirror fuchsia.io servers: only grant WRITE if it was requested.
                        let vmo = if flags.contains(fio::VmoFlags::WRITE) {
                            vmo
                        } else {
                            vmo.replace_handle(zx::Rights::VMO_DEFAULT - zx::Rights::WRITE).unwrap()
                        };
                        responder.send(Ok(vmo)).unwrap();
                    }
                    fio::FileRequest::Close { responder } => {
                        responder.send(Ok(())).unwrap();
                    }
                    _ => {}
                }
            }
        }

        fn handle_directory_requests(
            self: Arc<Self>,
            mut stream: fio::DirectoryRequestStream,
            control_handle: fio::DirectoryControlHandle,
            attributes_query: fio::NodeAttributesQuery,
        ) -> futures::future::BoxFuture<'static, ()> {
            async move {
                let selinux_context =
                    if attributes_query.contains(fio::NodeAttributesQuery::SELINUX_CONTEXT) {
                        self.xattrs
                            .lock()
                            .get(b"security.selinux".as_slice())
                            .cloned()
                            .map(fio::SelinuxContext::Data)
                    } else {
                        None
                    };
                let info = fio::DirectoryInfo {
                    attributes: Some(fio::NodeAttributes2 {
                        mutable_attributes: fio::MutableNodeAttributes {
                            selinux_context,
                            ..Default::default()
                        },
                        immutable_attributes: fio::ImmutableNodeAttributes {
                            id: Some(1),
                            link_count: Some(1),
                            protocols: Some(fio::NodeProtocolKinds::DIRECTORY),
                            ..Default::default()
                        },
                    }),
                    ..Default::default()
                };
                let _ = control_handle.send_on_representation(fio::Representation::Directory(info));
                let mut sub_tasks = Vec::new();
                while let Some(Ok(request)) = stream.next().await {
                    match request {
                        fio::DirectoryRequest::Open { path, flags, options, object, .. } => {
                            self.open_paths.lock().push(path.clone());
                            let query = options.attributes.unwrap_or_default();
                            if flags.intersects(
                                fio::Flags::FLAG_MUST_CREATE
                                    | fio::Flags::FLAG_CREATE_AS_UNNAMED_TEMPORARY,
                            ) {
                                let selinux_ctx = options
                                    .create_attributes
                                    .as_ref()
                                    .and_then(|attrs| attrs.selinux_context.as_ref())
                                    .and_then(|ctx| match ctx {
                                        fio::SelinuxContext::Data(d) => Some(d.clone()),
                                        _ => None,
                                    });
                                self.create_selinux_contexts
                                    .lock()
                                    .push((path.clone(), selinux_ctx));
                                let self_clone = Arc::clone(&self);
                                if flags.contains(fio::Flags::PROTOCOL_DIRECTORY) {
                                    sub_tasks.push(fasync::Task::spawn(async move {
                                        let (stream, control_handle) =
                                            ServerEnd::<fio::DirectoryMarker>::new(object)
                                                .into_stream_and_control_handle();
                                        self_clone
                                            .handle_directory_requests(
                                                stream,
                                                control_handle,
                                                query,
                                            )
                                            .await;
                                    }));
                                } else {
                                    sub_tasks.push(fasync::Task::spawn(async move {
                                        let (stream, control_handle) =
                                            ServerEnd::<fio::FileMarker>::new(object)
                                                .into_stream_and_control_handle();
                                        self_clone
                                            .handle_file_requests(stream, control_handle, query)
                                            .await;
                                    }));
                                }
                            } else if path == "file" {
                                let self_clone = Arc::clone(&self);
                                sub_tasks.push(fasync::Task::spawn(async move {
                                    let (stream, control_handle) =
                                        ServerEnd::<fio::FileMarker>::new(object)
                                            .into_stream_and_control_handle();
                                    self_clone
                                        .handle_file_requests(stream, control_handle, query)
                                        .await;
                                }));
                            } else if path == "dir" || path == "dir2" || path == "sub" {
                                let self_clone = Arc::clone(&self);
                                sub_tasks.push(fasync::Task::spawn(async move {
                                    let (stream, control_handle) =
                                        ServerEnd::<fio::DirectoryMarker>::new(object)
                                            .into_stream_and_control_handle();
                                    self_clone
                                        .handle_directory_requests(stream, control_handle, query)
                                        .await;
                                }));
                            } else {
                                let _ = ServerEnd::<fio::NodeMarker>::new(object)
                                    .close_with_epitaph(zx::Status::NOT_FOUND);
                            }
                        }
                        fio::DirectoryRequest::GetAttributes { responder, .. } => {
                            self.get_attrs_count.fetch_add(1, Ordering::SeqCst);
                            responder
                                .send(Ok((
                                    &fio::MutableNodeAttributes { ..Default::default() },
                                    &fio::ImmutableNodeAttributes {
                                        id: Some(1),
                                        link_count: Some(1),
                                        protocols: Some(fio::NodeProtocolKinds::DIRECTORY),
                                        ..Default::default()
                                    },
                                )))
                                .unwrap();
                        }
                        fio::DirectoryRequest::GetExtendedAttribute { name, responder } => {
                            self.get_xattr_count.fetch_add(1, Ordering::SeqCst);
                            match self.xattrs.lock().get(&name).cloned() {
                                Some(val) => responder
                                    .send(Ok(fio::ExtendedAttributeValue::Bytes(val)))
                                    .unwrap(),
                                None => {
                                    responder.send(Err(zx::Status::NOT_FOUND.into_raw())).unwrap()
                                }
                            }
                        }
                        fio::DirectoryRequest::SetExtendedAttribute {
                            name,
                            value,
                            responder,
                            ..
                        } => {
                            self.set_xattr_count.fetch_add(1, Ordering::SeqCst);
                            if let fio::ExtendedAttributeValue::Bytes(bytes) = value {
                                self.xattrs.lock().insert(name, bytes);
                            }
                            responder.send(Ok(())).unwrap();
                        }
                        fio::DirectoryRequest::RemoveExtendedAttribute { name, responder } => {
                            if self.xattrs.lock().remove(&name).is_some() {
                                responder.send(Ok(())).unwrap();
                            } else {
                                responder.send(Err(zx::Status::NOT_FOUND.into_raw())).unwrap();
                            }
                        }
                        fio::DirectoryRequest::Close { responder } => {
                            responder.send(Ok(())).unwrap();
                        }
                        _ => {}
                    }
                }
                for task in sub_tasks {
                    let _ = task.await;
                }
            }
            .boxed()
        }

        async fn run(self: Arc<Self>, mut stream: fio::DirectoryRequestStream) {
            let mut sub_tasks = Vec::new();
            while let Some(Ok(request)) = stream.next().await {
                match request {
                    fio::DirectoryRequest::Open { path, options, object, .. } => {
                        if path == "." {
                            let query = options.attributes.unwrap_or_default();
                            let self_clone = Arc::clone(&self);
                            sub_tasks.push(fasync::Task::spawn(async move {
                                let (stream, control_handle) =
                                    ServerEnd::<fio::DirectoryMarker>::new(object)
                                        .into_stream_and_control_handle();
                                self_clone
                                    .handle_directory_requests(stream, control_handle, query)
                                    .await;
                            }));
                        }
                    }
                    fio::DirectoryRequest::Close { responder } => {
                        responder.send(Ok(())).unwrap();
                    }
                    fio::DirectoryRequest::QueryFilesystem { responder } => {
                        responder.send(0i32, None).unwrap();
                    }
                    _ => {}
                }
            }
            for sub_task in sub_tasks {
                let _ = sub_task.await;
            }
        }
    }

    #[::fuchsia::test]
    async fn test_mknod_and_mkdir_pass_selinux_context_inline() {
        let (client, stream) = create_request_stream::<fio::DirectoryMarker>();
        let state = Arc::new(MockRemoteFs::default());

        let server_task = fasync::Task::spawn(Arc::clone(&state).run(stream));

        const TEST_POLICY: &[u8] = include_bytes!(
            "../../../../lib/selinux/testdata/composite_policies/compiled/bounded_transition_policy"
        );

        spawn_kernel_with_selinux_and_run(async move |current_task, security_server| {
            security_server.set_enforcing(false);
            security_server.load_policy(TEST_POLICY.to_vec()).expect("policy load failed");
            security::selinuxfs_policy_loaded(current_task);

            let fs = RemoteFs::new_fs(
                &current_task.kernel(),
                client.into_channel(),
                FileSystemOptions { source: FlyByteStr::new(b"."), ..Default::default() },
                fio::PERM_READABLE | fio::PERM_WRITABLE,
            )
            .expect("failed to mount test remote FS");
            security::file_system_resolve_security(current_task, &fs).expect("resolve fs security");

            let ns = Namespace::new(fs);
            let root = ns.root();

            let file_node = root
                .create_node(current_task, "new_file".into(), mode!(IFREG, 0o644), DeviceId::NONE)
                .expect("create_node file failed");
            {
                let _info = file_node
                    .entry
                    .node
                    .fetch_and_refresh_info(current_task)
                    .expect("fetch file info");
            }

            let dir_node = root
                .create_node(current_task, "new_dir".into(), mode!(IFDIR, 0o755), DeviceId::NONE)
                .expect("create_node dir failed");
            {
                let _info = dir_node
                    .entry
                    .node
                    .fetch_and_refresh_info(current_task)
                    .expect("fetch dir info");
            }

            let tmp_node = root
                .create_tmpfile(current_task, mode!(IFREG, 0o644), OpenFlags::RDWR)
                .expect("create_tmpfile failed");
            {
                let _info = tmp_node
                    .entry
                    .node
                    .fetch_and_refresh_info(current_task)
                    .expect("fetch tmpfile info");
            }

            // mknod, mkdir, and create_tmpfile should all pass selinux_context inline in
            // create_attributes, with 0 SetExtendedAttribute RPCs and 0 GetAttributes RPCs!
            let created = state.create_selinux_contexts.lock().clone();
            assert_eq!(created.len(), 3);
            assert_eq!(created[0].0, "new_file");
            assert!(created[0].1.is_some(), "expected inline selinux_context for new_file");
            assert_eq!(created[1].0, "new_dir");
            assert!(created[1].1.is_some(), "expected inline selinux_context for new_dir");
            assert_eq!(created[2].0, ".");
            assert!(created[2].1.is_some(), "expected inline selinux_context for tmpfile");
            assert_eq!(state.set_xattr_count.load(Ordering::SeqCst), 0);
            assert_eq!(state.get_attrs_count.load(Ordering::SeqCst), 0);
        })
        .await;

        server_task.await;
    }

    #[::fuchsia::test]
    async fn test_mknod_and_mkdir_do_not_trigger_get_attributes() {
        let (client, stream) = create_request_stream::<fio::DirectoryMarker>();
        let state = Arc::new(MockRemoteFs::default());

        let server_task = fasync::Task::spawn(Arc::clone(&state).run(stream));

        spawn_kernel_and_run(async move |current_task| {
            let fs = RemoteFs::new_fs(
                &current_task.kernel(),
                client.into_channel(),
                FileSystemOptions { source: FlyByteStr::new(b"."), ..Default::default() },
                fio::PERM_READABLE | fio::PERM_WRITABLE,
            )
            .expect("failed to mount test remote FS");

            let ns = Namespace::new(fs);
            let root = ns.root();

            // 1. Fetching attributes on the freshly mounted root directory should not trigger GetAttributes.
            {
                let _info =
                    root.entry.node.fetch_and_refresh_info(current_task).expect("fetch root info");
            }
            assert_eq!(state.get_attrs_count.load(Ordering::SeqCst), 0);

            // 2. Creating a regular file (mknod) and fetching its attributes should not trigger GetAttributes.
            let file_node = root
                .create_node(current_task, "new_file".into(), mode!(IFREG, 0o644), DeviceId::NONE)
                .expect("create_node file failed");
            {
                let _info = file_node
                    .entry
                    .node
                    .fetch_and_refresh_info(current_task)
                    .expect("fetch file info");
            }
            assert_eq!(state.get_attrs_count.load(Ordering::SeqCst), 0);

            // 3. Creating a directory (mkdir) and fetching its attributes should not trigger GetAttributes.
            let dir_node = root
                .create_node(current_task, "new_dir".into(), mode!(IFDIR, 0o755), DeviceId::NONE)
                .expect("create_node dir failed");
            {
                let _info = dir_node
                    .entry
                    .node
                    .fetch_and_refresh_info(current_task)
                    .expect("fetch dir info");
            }
            assert_eq!(state.get_attrs_count.load(Ordering::SeqCst), 0);

            // 4. Creating an unnamed temporary file (create_tmpfile) and fetching its attributes should not trigger GetAttributes.
            let tmp_node = root
                .create_tmpfile(current_task, mode!(IFREG, 0o644), OpenFlags::RDWR)
                .expect("create_tmpfile failed");
            {
                let _info = tmp_node
                    .entry
                    .node
                    .fetch_and_refresh_info(current_task)
                    .expect("fetch tmpfile info");
            }
            assert_eq!(state.get_attrs_count.load(Ordering::SeqCst), 0);
        })
        .await;

        server_task.await;
    }

    #[::fuchsia::test]
    async fn test_lookup_open_counts() {
        let (client, stream) = create_request_stream::<fio::DirectoryMarker>();
        let state = Arc::new(MockRemoteFs::default());

        let server_task = fasync::Task::spawn(Arc::clone(&state).run(stream));

        spawn_kernel_and_run(async move |current_task| {
            let fs = RemoteFs::new_fs(
                &current_task.kernel(),
                client.into_channel(),
                FileSystemOptions { source: FlyByteStr::new(b"."), ..Default::default() },
                fio::PERM_READABLE | fio::PERM_WRITABLE,
            )
            .expect("failed to mount test remote FS");

            let ns = Namespace::new(fs);
            let root = ns.root();

            // 1. Non-existent component_lookup sends only 1 Open request (not 2).
            let mut context = LookupContext::default();
            assert_eq!(
                root.lookup_child(current_task, &mut context, "nonexistent".into())
                    .expect_err("lookup should fail with ENOENT"),
                errno!(ENOENT)
            );
            assert_eq!(*state.open_paths.lock(), vec!["nonexistent"]);
            state.open_paths.lock().clear();

            // 2. Multi-component get_children_pipelined sends 1 pipelined Open per component.
            let results = root.entry.get_children_pipelined(
                current_task,
                &root.mount,
                &["dir".into(), "sub".into(), "file".into()],
            );
            assert_eq!(results.len(), 3);
            for res in &results {
                assert!(res.is_ok(), "expected Ok, got {res:?}");
            }
            assert_eq!(*state.open_paths.lock(), vec!["dir", "sub", "file"]);
            state.open_paths.lock().clear();

            // 3. Pipelined lookup encountering ENOENT does not send duplicate Open requests.
            let results = root.entry.get_children_pipelined(
                current_task,
                &root.mount,
                &["dir2".into(), "missing".into()],
            );
            assert_eq!(results.len(), 2);
            assert!(results[0].is_ok());
            assert_eq!(results[1].as_ref().expect_err("expected ENOENT"), &errno!(ENOENT));
            assert_eq!(*state.open_paths.lock(), vec!["dir2", "missing"]);
            state.open_paths.lock().clear();

            // 4. Re-querying a previously resolved path is served from the cache and sends no Open
            // requests.
            let results = root.entry.get_children_pipelined(
                current_task,
                &root.mount,
                &["dir".into(), "sub".into(), "file".into()],
            );
            assert_eq!(results.len(), 3);
            for res in &results {
                assert!(res.is_ok(), "expected Ok, got {res:?}");
            }
            assert_eq!(*state.open_paths.lock(), Vec::<String>::new());
        })
        .await;

        server_task.await;
    }

    #[::fuchsia::test]
    async fn test_get_size_uses_cache_unless_truncated() {
        let (client, stream) = create_request_stream::<fio::DirectoryMarker>();
        let state = Arc::new(MockRemoteFs::default());

        let server_task = fasync::Task::spawn(Arc::clone(&state).run(stream));

        spawn_kernel_and_run(async move |current_task| {
            let fs = RemoteFs::new_fs(
                &current_task.kernel(),
                client.into_channel(),
                FileSystemOptions { source: FlyByteStr::new(b"."), ..Default::default() },
                fio::PERM_READABLE | fio::PERM_WRITABLE,
            )
            .expect("failed to mount test remote FS");

            let ns = Namespace::new(fs);
            let root = ns.root();

            let mut context = LookupContext::default();
            let file_node = root
                .lookup_child(current_task, &mut context, "file".into())
                .expect("lookup failed");

            // 1. Initial get_size.
            assert_eq!(state.get_attrs_count.load(Ordering::SeqCst), 0);
            {
                let _size = file_node.entry.node.get_size(current_task).expect("get_size failed");
            }
            assert_eq!(state.get_attrs_count.load(Ordering::SeqCst), 0);

            // 2. Open in append mode and write.
            let file_handle = file_node
                .open(current_task, OpenFlags::RDWR | OpenFlags::APPEND)
                .expect("open failed");

            {
                let mut data = VecInputBuffer::new(b"foo");
                let written = file_handle.write(current_task, &mut data).expect("write failed");
                assert_eq!(written, 3);
            }
            assert_eq!(file_node.entry.node.get_size(current_task).expect("get_size failed"), 3);
            assert_eq!(state.get_attrs_count.load(Ordering::SeqCst), 0);
            assert_eq!(*state.write_offsets.lock(), vec![0]);

            // 3. Truncate. This should invalidate the cache.
            file_node
                .entry
                .node
                .truncate(current_task, &file_node.mount, 0)
                .expect("truncate failed");
            assert_eq!(state.file_size.load(Ordering::SeqCst), 0);

            // 4. get_size again. Should trigger a request.
            {
                let size = file_node.entry.node.get_size(current_task).expect("get_size failed");
                assert_eq!(size, 0);
            }
            assert_eq!(state.get_attrs_count.load(Ordering::SeqCst), 1);

            // 5. Append again. It should append at offset 0.
            {
                let mut data = VecInputBuffer::new(b"bar");
                let written = file_handle.write(current_task, &mut data).expect("write failed");
                assert_eq!(written, 3);
            }
            assert_eq!(file_node.entry.node.get_size(current_task).expect("get_size failed"), 3);
            // write calls seek(End, 0) which calls get_size, which uses the cache if it was just
            // refreshed.
            assert_eq!(state.get_attrs_count.load(Ordering::SeqCst), 1);
            assert_eq!(*state.write_offsets.lock(), vec![0, 0]);

            // 6. Truncate to 10 and append.
            file_node
                .entry
                .node
                .truncate(current_task, &file_node.mount, 10)
                .expect("truncate failed");
            {
                let mut data = VecInputBuffer::new(b"baz");
                let written = file_handle.write(current_task, &mut data).expect("write failed");
                assert_eq!(written, 3);
            }
            // write calls seek(End, 0). Since truncate was called, cache is invalid.
            assert_eq!(state.get_attrs_count.load(Ordering::SeqCst), 2);
            assert_eq!(file_node.entry.node.get_size(current_task).expect("get_size failed"), 13);
            assert_eq!(*state.write_offsets.lock(), vec![0, 0, 10]);
        })
        .await;

        server_task.await;
    }

    #[::fuchsia::test]
    async fn test_get_size_during_refresh_after_truncate() {
        let (client, stream) = create_request_stream::<fio::DirectoryMarker>();
        let state = Arc::new(MockRemoteFs::default());

        let server_task = fasync::Task::spawn(Arc::clone(&state).run(stream));

        spawn_kernel_and_run(async move |current_task| {
            let fs = RemoteFs::new_fs(
                &current_task.kernel(),
                client.into_channel(),
                FileSystemOptions { source: FlyByteStr::new(b"."), ..Default::default() },
                fio::PERM_READABLE | fio::PERM_WRITABLE,
            )
            .expect("failed to mount test remote FS");

            let ns = Namespace::new(fs);
            let root = ns.root();

            let mut context = LookupContext::default();
            let file_node = root
                .lookup_child(current_task, &mut context, "file".into())
                .expect("lookup failed");

            // Fill cache.
            assert_eq!(file_node.entry.node.get_size(current_task).expect("get_size failed"), 0);

            // Truncate to 10.
            file_node
                .entry
                .node
                .truncate(current_task, &file_node.mount, 10)
                .expect("truncate failed");

            // Set barrier to pause GetAttributes.
            let barrier = Arc::new(Barrier::new(2));
            {
                let barrier = barrier.clone();
                *state.get_attrs_hook.lock() = Some(Box::pin(async move {
                    barrier.async_wait().await;
                    barrier.async_wait().await;
                }));
            }

            // Spawn thread to call get_size. It will pause when it hits the first barrier.
            let file_node_clone = file_node.clone();
            let (result1, request) = SpawnRequestBuilder::new()
                .with_sync_closure(move |current_task| {
                    let size =
                        file_node_clone.entry.node.get_size(current_task).expect("get_size failed");
                    assert_eq!(size, 10);
                })
                .build_with_async_result();
            current_task.kernel().kthreads.spawner().spawn_from_request(request);

            // Wait for the first barrier to be reached.
            barrier.async_wait().await;

            // Set up the next request so it unblocks the first request.
            *state.get_attrs_hook.lock() =
                Some(Box::pin(async move { barrier.async_wait().await }));

            // Another get_size call should not use cached size (0) while refresh is in progress.
            let (result2, request) = SpawnRequestBuilder::new()
                .with_sync_closure(move |current_task| {
                    let size =
                        file_node.entry.node.get_size(current_task).expect("get_size failed");
                    assert_eq!(size, 10);
                })
                .build_with_async_result();
            current_task.kernel().kthreads.spawner().spawn_from_request(request);

            result1.await.unwrap();
            result2.await.unwrap();
        })
        .await;

        server_task.await;
    }

    #[::fuchsia::test]
    async fn test_get_size_during_outstanding_write() {
        let (client, stream) = create_request_stream::<fio::DirectoryMarker>();
        let state = Arc::new(MockRemoteFs::default());

        let server_task = fasync::Task::spawn(Arc::clone(&state).run(stream));

        spawn_kernel_and_run(async move |current_task| {
            let fs = RemoteFs::new_fs(
                &current_task.kernel(),
                client.into_channel(),
                FileSystemOptions { source: FlyByteStr::new(b"."), ..Default::default() },
                fio::PERM_READABLE | fio::PERM_WRITABLE,
            )
            .expect("failed to mount test remote FS");

            let ns = Namespace::new(fs);
            let root = ns.root();

            let mut context = LookupContext::default();
            let file_node = root
                .lookup_child(current_task, &mut context, "file".into())
                .expect("lookup failed");

            // Open the file.
            let file_handle = file_node.open(current_task, OpenFlags::RDWR).expect("open failed");

            // Set hook to stall the write response.
            let barrier = Arc::new(Barrier::new(2));
            {
                let barrier = barrier.clone();
                *state.write_hook.lock() = Some(Box::pin(async move {
                    barrier.async_wait().await;
                    barrier.async_wait().await;
                }));
            }

            // Start write in another thread.
            let file_handle_clone = file_handle.clone();
            current_task.kernel().kthreads.spawner().spawn_from_request(
                SpawnRequestBuilder::new()
                    .with_sync_closure(move |current_task| {
                        let mut data = VecInputBuffer::new(b"hello");
                        file_handle_clone.write(current_task, &mut data).expect("write failed");
                    })
                    .build(),
            );

            // Wait until the mock has processed the write and hit the hook.
            barrier.async_wait().await;

            // On this thread, verify that a read sees the new data.
            {
                let mut data = VecOutputBuffer::new(5);
                let read = file_handle.read_at(current_task, 0, &mut data).expect("read failed");
                assert_eq!(read, 5);
                assert_eq!(data.data(), b"hello");
            }

            // Now call get_size and it should see the correct size.
            let size = file_node.entry.node.get_size(current_task).expect("get_size failed");
            assert_eq!(
                size, 5,
                "get_size should return the updated size even if a write is outstanding"
            );

            // Unblock the write.
            barrier.async_wait().await;
        })
        .await;

        server_task.await;
    }

    #[test]
    fn test_info_state_initial_state() {
        let state = InfoState::new(true); // dirty
        assert_eq!(state.0.load(Ordering::Relaxed), 0);

        let state = InfoState::new(false); // in sync
        assert_eq!(state.0.load(Ordering::Relaxed), InfoState::IN_SYNC);
    }

    #[test]
    fn test_info_state_dirty_op_guard() {
        let state = InfoState::new(false);
        {
            let _guard = state.dirty_op_guard(false);
            assert_eq!(state.0.load(Ordering::Relaxed), 1); // IN_SYNC bit cleared, count 1
            assert!(!state.is_size_accurate());
        }
        assert_eq!(state.0.load(Ordering::Relaxed), 0);
        assert!(state.is_size_accurate());

        {
            let _guard = state.dirty_op_guard(true);
            assert_eq!(state.0.load(Ordering::Relaxed), InfoState::TRUNCATED | 1);
            assert!(!state.is_size_accurate());
        }
        assert_eq!(state.0.load(Ordering::Relaxed), InfoState::TRUNCATED);
        assert!(!state.is_size_accurate());

        {
            let _guard1 = state.dirty_op_guard(true);
            let _guard2 = state.dirty_op_guard(true);
            assert_eq!(state.0.load(Ordering::Relaxed), InfoState::TRUNCATED | 2);
            assert!(!state.is_size_accurate());
        }
        assert_eq!(state.0.load(Ordering::Relaxed), InfoState::TRUNCATED);
        assert!(!state.is_size_accurate());
    }

    #[test]
    fn test_info_state_refresh_clears_truncated() {
        let state = InfoState::new(true);
        // Set TRUNCATED bit.
        {
            let _guard = state.dirty_op_guard(true);
        }
        assert_eq!(state.0.load(Ordering::Relaxed), InfoState::TRUNCATED);

        let info = DynamicLockDepRwLock::new::<FsNodeInfoLevel>(FsNodeInfo::default());
        state.maybe_refresh(&info, |_| Ok(()), |_| unreachable!()).unwrap();

        assert_eq!(state.0.load(Ordering::Relaxed), InfoState::IN_SYNC);
        assert!(state.is_size_accurate());
    }

    #[test]
    fn test_info_state_maybe_refresh_success() {
        let state = InfoState::new(true);
        let info = DynamicLockDepRwLock::new::<FsNodeInfoLevel>(FsNodeInfo::default());

        let res = state.maybe_refresh(&info, |_| Ok(42), |_| unreachable!());
        assert_eq!(res.unwrap(), 42);
        assert_eq!(state.0.load(Ordering::Relaxed), InfoState::IN_SYNC);
    }

    #[test]
    fn test_info_state_maybe_refresh_error() {
        let state = InfoState::new(true);
        let info = DynamicLockDepRwLock::new::<FsNodeInfoLevel>(FsNodeInfo::default());

        let res: Result<u32, Errno> =
            state.maybe_refresh(&info, |_| error!(EIO), |_| unreachable!());
        assert!(res.is_err());
        assert_eq!(state.0.load(Ordering::Relaxed), 0); // Still dirty
    }

    #[test]
    fn test_info_state_maybe_refresh_not_needed() {
        let state = InfoState::new(false); // in sync
        let info = DynamicLockDepRwLock::new::<FsNodeInfoLevel>(FsNodeInfo::default());
        let res = state.maybe_refresh(&info, |_| unreachable!(), |_| Ok(123));
        assert_eq!(res.unwrap(), 123);
    }

    #[test]
    fn test_info_state_concurrent_dirty_op_during_refresh() {
        let state = InfoState::new(true);
        let info = DynamicLockDepRwLock::new::<FsNodeInfoLevel>(FsNodeInfo::default());

        state
            .maybe_refresh(
                &info,
                |_| {
                    // Simulate a dirty op starting while refresh is in progress
                    let _guard = state.dirty_op_guard(false);
                    assert_eq!(state.0.load(Ordering::Relaxed), InfoState::PENDING_REFRESH | 1);
                    Ok(())
                },
                |_| unreachable!(),
            )
            .unwrap();

        assert_eq!(state.0.load(Ordering::Relaxed), 0);
    }

    #[::fuchsia::test]
    async fn test_sync() {
        let fixture = TestFixture::new().await;
        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed");

        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
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
            root.create_node(&current_task, "file".into(), REG_MODE, DeviceId::NONE)
                .expect("create_node failed");
            let mut context = LookupContext::new(SymlinkMode::NoFollow);
            let reg_node = root
                .lookup_child(&current_task, &mut context, "file".into())
                .expect("lookup_child failed");

            // sync should delegate to zxio and succeed
            reg_node
                .entry
                .node
                .ops()
                .sync(&reg_node.entry.node, &current_task)
                .expect("sync failed");
        })
        .await;
        fixture.close().await;
    }

    #[::fuchsia::test]
    async fn test_msync_propagates_to_fxfs() {
        use crate::mm::MemoryAccessor;
        use crate::mm::syscalls::{sys_mmap, sys_msync};
        use crate::vfs::FdFlags;
        use starnix_uapi::user_address::UserAddress;
        use starnix_uapi::{MAP_SHARED, MS_SYNC, PROT_READ, PROT_WRITE};

        // Counter to track Fxfs transactions
        let commit_count = Arc::new(AtomicUsize::new(0));
        let commit_count_clone = commit_count.clone();

        let (mut hooks, fs_hooks) = fxfs_testing::Hooks::new();
        hooks.set_pre_commit(move |_transaction| {
            commit_count_clone.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });

        // Open fixture with hooks
        let fixture = TestFixture::open(
            DeviceHolder::new(FakeDevice::new(1024 * 1024, 512)),
            TestFixtureOptions {
                format: true,
                as_blob: false,
                encrypted: true,
                hooks: Some(fs_hooks),
                ..Default::default()
            },
        )
        .await;

        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone channel");

        spawn_kernel_and_run(async move |current_task| {
            // Setup RemoteFs
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
                current_task.kernel(),
                client,
                FileSystemOptions { source: FlyByteStr::new(b"/test"), ..Default::default() },
                rights,
            )
            .expect("new_fs");
            let ns = Namespace::new(fs);
            let root = ns.root();

            // Create and Open a file
            let node = root
                .create_node(&current_task, "test_file".into(), mode!(IFREG, 0o666), DeviceId::NONE)
                .expect("create_node");
            let file_handle = node.open(&current_task, OpenFlags::RDWR).expect("open");
            let fd = current_task.add_file(file_handle, FdFlags::empty()).expect("add file");

            // Do mmap
            let len = *PAGE_SIZE as usize * 4;
            let mmap_addr = sys_mmap(
                current_task,
                UserAddress::default(),
                len,
                PROT_READ | PROT_WRITE,
                MAP_SHARED,
                fd,
                0,
            )
            .expect("mmap");

            // Modify memory (multiple pages)
            for i in 0..4 {
                let data = [0xAAu8; 1];
                current_task
                    .write_memory((mmap_addr + (i * *PAGE_SIZE as usize)).unwrap(), &data)
                    .expect("write memory");
            }

            // Capture commit count before msync
            let commits_before_msync = commit_count.load(Ordering::SeqCst);

            // invoke msync()
            sys_msync(current_task, mmap_addr, len, MS_SYNC).expect("msync");

            // Verify msync results
            let final_commits = commit_count.load(Ordering::SeqCst);
            assert!(
                final_commits > commits_before_msync,
                "msync should trigger Fxfs transaction. commits: {} -> {}",
                commits_before_msync,
                final_commits
            );
        })
        .await;

        fixture.close().await;
    }

    #[::fuchsia::test]
    async fn test_get_size() {
        let fixture = TestFixture::new().await;
        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed");

        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
                &kernel,
                client,
                FileSystemOptions { source: FlyByteStr::new(b"/"), ..Default::default() },
                rights,
            )
            .expect("new_fs failed");
            let ns = Namespace::new(fs);
            let root = ns.root();

            const REG_MODE: FileMode = FileMode::from_bits(FileMode::IFREG.bits() | 0o666);
            let node = root
                .create_node(&current_task, "file".into(), REG_MODE, DeviceId::NONE)
                .expect("create_node failed");
            let file = node.open(&current_task, OpenFlags::RDWR).expect("open failed");

            // Initial size should be 0.
            assert_eq!(node.entry.node.get_size(&current_task).expect("get_size failed"), 0);

            // Write some data.
            let mut data = VecInputBuffer::new(b"hello");
            file.write(&current_task, &mut data).expect("write failed");

            // Size should be 5.
            assert_eq!(node.entry.node.get_size(&current_task).expect("get_size failed"), 5);

            // Truncate to 10.
            node.truncate(&current_task, 10).expect("truncate failed");

            // Size should be 10.
            assert_eq!(node.entry.node.get_size(&current_task).expect("get_size failed"), 10);

            // Truncate to 3.
            node.truncate(&current_task, 3).expect("truncate failed");

            // Size should be 3.
            assert_eq!(node.entry.node.get_size(&current_task).expect("get_size failed"), 3);
        })
        .await;
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_remote_fs_casefold_not_supported_on_non_fxfs() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_EXECUTABLE;
            let (server, client) = zx::Channel::create();
            fdio::open("/pkg", rights, server).expect("failed to open /pkg");
            let fs = RemoteFs::new_fs(
                &kernel,
                client,
                FileSystemOptions { source: FlyByteStr::new(b"/pkg"), ..Default::default() },
                rights,
            )
            .unwrap();
            let ns = Namespace::new(fs);
            let root = ns.root();

            assert!(!root.entry.node.fs().has_casefold_support());
            assert_eq!(
                root.entry.node.update_attributes(&current_task, |info| {
                    info.casefold = true;
                    Ok(())
                }),
                error!(ENOTSUP)
            );
            assert_eq!(root.entry.set_casefold(&current_task, true), error!(ENOTSUP));
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_remote_fs_casefold_supported_on_fxfs() {
        let fixture = TestFixture::new().await;
        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed");

        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
            let fs = RemoteFs::new_fs(
                &kernel,
                client,
                FileSystemOptions { source: FlyByteStr::new(b"/"), ..Default::default() },
                rights,
            )
            .expect("new_fs failed");
            let ns = Namespace::new(fs);
            let root = ns.root();

            assert!(root.entry.node.fs().has_casefold_support());

            // Casefold can be enabled on an empty directory.
            assert_eq!(root.entry.set_casefold(&current_task, true), Ok(()));
            assert!(root.entry.node.info().casefold);

            // Enabling casefold when already enabled is idempotent.
            assert_eq!(root.entry.set_casefold(&current_task, true), Ok(()));

            // Adding a child makes the directory non-empty.
            root.create_node(&current_task, "child".into(), FileMode::IFREG, DeviceId::NONE)
                .expect("create child");

            // Toggling casefold on a non-empty directory returns ENOTEMPTY.
            assert_eq!(root.entry.set_casefold(&current_task, false), error!(ENOTEMPTY));
        })
        .await;

        fixture.close().await;
    }

    #[::fuchsia::test]
    async fn test_close_flushes_written_file() {
        use crate::vfs::FdFlags;

        let commits = Arc::new(AtomicUsize::new(0));
        let commits_clone = commits.clone();
        let (mut hooks, fs_hooks) = fxfs_testing::Hooks::new();
        hooks.set_pre_commit(move |_transaction| {
            commits_clone.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        let fixture = TestFixture::open(
            DeviceHolder::new(FakeDevice::new(16384, 512)),
            TestFixtureOptions { hooks: Some(fs_hooks), ..Default::default() },
        )
        .await;
        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed");

        spawn_kernel_and_run(async move |current_task| {
            let fs = RemoteFs::new_fs(
                current_task.kernel(),
                client,
                FileSystemOptions { source: FlyByteStr::new(b"/"), ..Default::default() },
                fio::PERM_READABLE | fio::PERM_WRITABLE,
            )
            .expect("new_fs failed");
            let ns = Namespace::new(fs);
            let file = ns
                .root()
                .create_node(&current_task, "file".into(), mode!(IFREG, 0o666), DeviceId::NONE)
                .expect("create_node failed")
                .open(&current_task, OpenFlags::RDWR)
                .expect("open failed");
            file.write(&current_task, &mut VecInputBuffer::new(b"hello")).expect("write failed");
            let fd = current_task.add_file(file, FdFlags::empty()).expect("add_file failed");

            // close(2), without fsync, should make Fxfs flush (commit) the file's data.
            let commits_before_close = commits.load(Ordering::SeqCst);
            current_task.files().close(fd).expect("close failed");
            current_task.trigger_delayed_releaser();
            assert!(commits.load(Ordering::SeqCst) > commits_before_close);
        })
        .await;
        fixture.close().await;
    }

    #[derive(Clone, Default)]
    struct MockSyncDirectory;

    impl SyncIoDirectory for MockSyncDirectory {
        fn seek(&self, _new_index: u64) -> Result<u64, zx::Status> {
            Ok(0)
        }
        fn readdir<B, S: FnMut(u64, fio::DirentType, &[u8]) -> ControlFlow<B, ()>>(
            &self,
            _sink: S,
        ) -> Result<Option<B>, zx::Status> {
            Ok(None)
        }
        fn sync(&self) -> Result<(), zx::Status> {
            Ok(())
        }
        fn clone_proxy(&self) -> Result<Option<fio::DirectorySynchronousProxy>, zx::Status> {
            Ok(None)
        }
    }

    #[derive(Clone, Default)]
    struct MockSyncIo {
        id: u64,
    }

    impl SyncIo for MockSyncIo {
        type Directory = MockSyncDirectory;

        fn attr_get(
            &self,
            _query: fio::NodeAttributesQuery,
        ) -> Result<(fio::MutableNodeAttributes, fio::ImmutableNodeAttributes), zx::Status>
        {
            Ok((
                fio::MutableNodeAttributes {
                    mode: Some(FileMode::IFDIR.bits() | 0o755),
                    ..Default::default()
                },
                fio::ImmutableNodeAttributes {
                    id: Some(self.id),
                    protocols: Some(fio::NodeProtocolKinds::DIRECTORY),
                    ..Default::default()
                },
            ))
        }

        fn attr_set(&self, _attributes: fio::MutableNodeAttributes) -> Result<(), zx::Status> {
            Ok(())
        }

        fn query_filesystem(&self) -> Result<fio::FilesystemInfo, zx::Status> {
            Ok(fio::FilesystemInfo {
                total_bytes: 1024 * 1024,
                used_bytes: 4096,
                total_nodes: 1000,
                used_nodes: 10,
                free_shared_pool_bytes: 0,
                fs_id: 42,
                block_size: 4096,
                max_filename_size: 255,
                fs_type: fidl_fuchsia_fs::VfsType::Fxfs.into_primitive(),
                padding: 0,
                name: [0; 32],
            })
        }

        fn open<F: sync_io_client::Factory<Self>>(
            &self,
            _path: &str,
            _flags: fio::Flags,
            _attributes: Option<fio::MutableNodeAttributes>,
            _query: fio::NodeAttributesQuery,
            factory: F,
        ) -> Result<F::Result, zx::Status> {
            Ok(factory.create_directory(
                MockSyncIo { id: self.id },
                fio::DirectoryInfo {
                    attributes: Some(fio::NodeAttributes2 {
                        mutable_attributes: fio::MutableNodeAttributes {
                            mode: Some(FileMode::IFDIR.bits() | 0o755),
                            ..Default::default()
                        },
                        immutable_attributes: fio::ImmutableNodeAttributes {
                            id: Some(self.id),
                            protocols: Some(fio::NodeProtocolKinds::DIRECTORY),
                            ..Default::default()
                        },
                    }),
                    ..Default::default()
                },
            ))
        }

        fn open_pipelined<F: sync_io_client::Factory<Self>>(
            &self,
            _paths: &[&str],
            _flags: fio::Flags,
            _query: fio::NodeAttributesQuery,
            _create_factory: impl FnMut() -> F,
        ) -> impl Iterator<Item = Result<F::Result, zx::Status>> {
            std::iter::empty()
        }

        fn link_into(&self, _target_dir: &Self, _name: &str) -> Result<(), zx::Status> {
            Err(zx::Status::NOT_SUPPORTED)
        }

        fn unlink(&self, _name: &str, _flags: fio::UnlinkFlags) -> Result<(), zx::Status> {
            Err(zx::Status::NOT_SUPPORTED)
        }

        fn rename(
            &self,
            _old_path: &str,
            _new_dir: &Self,
            _new_path: &str,
        ) -> Result<(), zx::Status> {
            Err(zx::Status::NOT_SUPPORTED)
        }

        fn create_symlink(&self, _name: &str, _target: &[u8]) -> Result<Self, zx::Status> {
            Err(zx::Status::NOT_SUPPORTED)
        }

        fn read_partial(&self, _offset: u64, _max: usize) -> Result<(Vec<u8>, bool), zx::Status> {
            Err(zx::Status::NOT_SUPPORTED)
        }

        fn write(&self, _offset: u64, _data: &[u8]) -> Result<usize, zx::Status> {
            Err(zx::Status::NOT_SUPPORTED)
        }

        fn supports_vectored(&self) -> bool {
            false
        }

        unsafe fn readv(
            &self,
            _offset: u64,
            _iovecs: &mut [zx::sys::zx_iovec_t],
        ) -> Result<usize, zx::Status> {
            Err(zx::Status::NOT_SUPPORTED)
        }

        fn writev(
            &self,
            _offset: u64,
            _iovecs: &[zx::sys::zx_iovec_t],
        ) -> Result<usize, zx::Status> {
            Err(zx::Status::NOT_SUPPORTED)
        }

        fn truncate(&self, _length: u64) -> Result<(), zx::Status> {
            Err(zx::Status::NOT_SUPPORTED)
        }

        fn vmo_get(&self, _flags: zx::VmarFlags) -> Result<zx::Vmo, zx::Status> {
            Err(zx::Status::NOT_SUPPORTED)
        }

        fn sync(&self) -> Result<(), zx::Status> {
            Ok(())
        }

        fn enable_verity(
            &self,
            _descriptor: &zxio_fsverity_descriptor_t,
        ) -> Result<(), zx::Status> {
            Err(zx::Status::NOT_SUPPORTED)
        }

        fn allocate(
            &self,
            _offset: u64,
            _length: u64,
            _mode: syncio::AllocateMode,
        ) -> Result<(), zx::Status> {
            Err(zx::Status::NOT_SUPPORTED)
        }

        fn xattr_get(&self, _name: &[u8]) -> Result<Vec<u8>, zx::Status> {
            Err(zx::Status::NOT_SUPPORTED)
        }

        fn xattr_set(
            &self,
            _name: &[u8],
            _value: &[u8],
            _mode: syncio::XattrSetMode,
        ) -> Result<(), zx::Status> {
            Err(zx::Status::NOT_SUPPORTED)
        }

        fn xattr_remove(&self, _name: &[u8]) -> Result<(), zx::Status> {
            Err(zx::Status::NOT_SUPPORTED)
        }

        fn xattr_list(&self) -> Result<Vec<Vec<u8>>, zx::Status> {
            Ok(vec![])
        }

        fn open_directory(&self) -> Result<Self::Directory, zx::Status> {
            Ok(MockSyncDirectory)
        }
    }

    #[::fuchsia::test]
    async fn test_remote_fs_new_with_mock_sync_io() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let mock_io = MockSyncIo { id: 1001 };
            let (remotefs, root_node, info, node_id) = RemoteFs::new_with_io(
                mock_io,
                fio::PERM_READABLE | fio::PERM_WRITABLE,
                "mock_remotefs",
            )
            .expect("new_with_io failed");
            assert_eq!(node_id, 1001);
            assert!(remotefs.use_remote_ids());
            assert_eq!(remotefs.name(), "mock_remotefs");

            let fs = FileSystem::new(
                kernel,
                CacheMode::Uncached,
                remotefs,
                FileSystemOptions::default(),
            )
            .expect("FileSystem::new failed");
            fs.create_root_with_info(node_id, root_node, info);

            let root_entry = fs.root();
            let file =
                root_entry.open_anonymous(&current_task, OpenFlags::RDONLY).expect("open failed");
            let stat = fs.statfs(&current_task).expect("statfs failed");
            assert_eq!(stat.f_bsize as u64, 4096);
            assert_eq!(stat.f_blocks, 256); // 1024 * 1024 / 4096

            // Check to_handle falls back when clone_proxy returns None
            let handle = file.to_handle(&current_task).expect("to_handle failed");
            assert!(handle.is_some());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_get_memory_cached_across_opens_and_exec() {
        let (client, stream) = create_request_stream::<fio::DirectoryMarker>();
        let state = Arc::new(MockRemoteFs::default());

        let server_task = fasync::Task::spawn(Arc::clone(&state).run(stream));

        spawn_kernel_and_run(async move |current_task| {
            let fs = RemoteFs::new_fs(
                &current_task.kernel(),
                client.into_channel(),
                FileSystemOptions { source: FlyByteStr::new(b"."), ..Default::default() },
                fio::PERM_READABLE | fio::PERM_WRITABLE,
            )
            .expect("failed to mount test remote FS");

            let ns = Namespace::new(fs);
            let root = ns.root();

            let mut context = LookupContext::default();
            let file_node = root
                .lookup_child(current_task, &mut context, "file".into())
                .expect("lookup failed");

            let file1 = file_node.open(current_task, OpenFlags::RDONLY).expect("open 1 failed");
            assert_eq!(state.get_backing_memory_count.load(Ordering::SeqCst), 0);

            let ro_mem1 = file1
                .get_memory(current_task, None, ProtectionFlags::READ)
                .expect("get_memory READ failed");
            assert_eq!(state.get_backing_memory_count.load(Ordering::SeqCst), 1);

            // Requesting READ | EXEC should derive the executable VMO from the cached READ VMO
            // without making another GetBackingMemory FIDL call.
            let rx_mem1 = file1
                .get_memory(current_task, None, ProtectionFlags::READ | ProtectionFlags::EXEC)
                .expect("get_memory READ|EXEC failed");
            assert_eq!(state.get_backing_memory_count.load(Ordering::SeqCst), 1);
            assert!(rx_mem1.get_rights().contains(zx::Rights::EXECUTE));
            assert!(!rx_mem1.get_rights().contains(zx::Rights::WRITE));

            drop(file1);

            // Opening the same file node again should reuse the cached VMOs on RemoteNode.
            let file2 = file_node.open(current_task, OpenFlags::RDONLY).expect("open 2 failed");
            let ro_mem2 = file2
                .get_memory(current_task, None, ProtectionFlags::READ)
                .expect("get_memory READ 2 failed");
            let rx_mem2 = file2
                .get_memory(current_task, None, ProtectionFlags::READ | ProtectionFlags::EXEC)
                .expect("get_memory READ|EXEC 2 failed");
            assert_eq!(state.get_backing_memory_count.load(Ordering::SeqCst), 1);
            assert!(Arc::ptr_eq(&ro_mem1, &ro_mem2));
            assert!(Arc::ptr_eq(&rx_mem1, &rx_mem2));
        })
        .await;

        server_task.await;
    }

    #[::fuchsia::test]
    async fn test_get_memory_exec_first_fetches_directly() {
        let (client, stream) = create_request_stream::<fio::DirectoryMarker>();
        let state = Arc::new(MockRemoteFs::default());

        let server_task = fasync::Task::spawn(Arc::clone(&state).run(stream));

        spawn_kernel_and_run(async move |current_task| {
            let fs = RemoteFs::new_fs(
                &current_task.kernel(),
                client.into_channel(),
                FileSystemOptions { source: FlyByteStr::new(b"."), ..Default::default() },
                fio::PERM_READABLE | fio::PERM_WRITABLE,
            )
            .expect("failed to mount test remote FS");

            let ns = Namespace::new(fs);
            let root = ns.root();

            let mut context = LookupContext::default();
            let file_node = root
                .lookup_child(current_task, &mut context, "file".into())
                .expect("lookup failed");
            let file = file_node.open(current_task, OpenFlags::RDONLY).expect("open failed");

            // With no cached READ VMO to derive from, READ | EXEC is fetched directly and does
            // not populate the READ cache (so exec-only files hold a single VMO handle).
            let rx_mem = file
                .get_memory(current_task, None, ProtectionFlags::READ | ProtectionFlags::EXEC)
                .expect("get_memory READ|EXEC failed");
            assert_eq!(state.get_backing_memory_count.load(Ordering::SeqCst), 1);
            assert!(rx_mem.get_rights().contains(zx::Rights::EXECUTE));
            assert!(!rx_mem.get_rights().contains(zx::Rights::WRITE));

            let ro_mem = file
                .get_memory(current_task, None, ProtectionFlags::READ)
                .expect("get_memory READ failed");
            assert_eq!(state.get_backing_memory_count.load(Ordering::SeqCst), 2);
            assert!(!ro_mem.get_rights().contains(zx::Rights::EXECUTE));

            // Both handles are now cached on the node.
            let rx_mem2 = file
                .get_memory(current_task, None, ProtectionFlags::READ | ProtectionFlags::EXEC)
                .expect("get_memory READ|EXEC 2 failed");
            let ro_mem2 = file
                .get_memory(current_task, None, ProtectionFlags::READ)
                .expect("get_memory READ 2 failed");
            assert_eq!(state.get_backing_memory_count.load(Ordering::SeqCst), 2);
            assert!(Arc::ptr_eq(&rx_mem, &rx_mem2));
            assert!(Arc::ptr_eq(&ro_mem, &ro_mem2));
        })
        .await;

        server_task.await;
    }
}
