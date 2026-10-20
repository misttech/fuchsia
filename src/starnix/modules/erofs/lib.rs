// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! EROFS (Enhanced Read-Only File System) module for Starnix.
//!
//! Supports two mounting modes:
//! 1. **In-process driver-mapped mode**: When the backing file supports `FileOps::get_file_token`
//!    (e.g., an EROFS image stored in an Fxblob package blob, mounted directly or via a loop
//!    device / `dm-verity`) and `fuchsia.storage.mapping.FileMapper` is available, Starnix parses
//!    EROFS metadata in-process using `erofs::ErofsFilesystem` and creates pager-backed file VMOs
//!    through a child `MapperSession` (`erofs::VolumeMapper`). This avoids per-operation FIDL
//!    round-trips to an external filesystem component while keeping `zx::Pager` out of
//!    `starnix_kernel` so container suspend cannot freeze pager threads and deadlock.
//! 2. **Out-of-process component fallback**: When `FileMapper` is unavailable, the source file does
//!    not provide a page-aligned `FileToken`, or `FileMapper.OpenSession` fails, Starnix falls back
//!    to the out-of-process `fuchsia.erofs.Erofs` component (`Erofs.Serve`) and mounts the served
//!    directory channel via `RemoteFs`.

#![recursion_limit = "256"]

use erofs::readers::VmoReader;
use erofs::{
    DirectoryEntry as ErofsDirEntry, DirectoryNode, ErofsFilesystem, FileNode, FileType, Node,
    NodeInner, SymlinkNode, VolumeMapper,
};
use fidl_fuchsia_erofs::{ErofsMarker, ErofsServeRequest};
use fidl_fuchsia_io as fio;
use fidl_fuchsia_storage_mapping::FileMapperMarker;
use fuchsia_component::client::connect_to_protocol_sync;
use fuchsia_runtime::UtcInstant;
use starnix_core::fs::fuchsia::RemoteFs;
use starnix_core::mm::ProtectionFlags;
use starnix_core::mm::memory::MemoryObject;
use starnix_core::task::CurrentTask;
use starnix_core::vfs::fs_args::MountParams;
use starnix_core::vfs::{
    CacheMode, DEFAULT_BYTES_PER_BLOCK, DirEntry, DirectoryEntryType, DirentSink, FileObject,
    FileOps, FileSystem, FileSystemHandle, FileSystemOps, FileSystemOptions, FsNode, FsNodeFlags,
    FsNodeHandle, FsNodeInfo, FsNodeOps, FsStr, FsString, MemoryRegularFile, SeekTarget,
    SymlinkTarget, ValueOrSize, default_seek, emit_dotdot, fileops_impl_directory,
    fileops_impl_noop_sync, fs_node_impl_dir_readonly, fs_node_impl_not_dir, fs_node_impl_symlink,
};
use starnix_logging::{impossible_error, log_error, log_info, log_warn};
use starnix_types::vfs::default_statfs;
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::FileMode;
use starnix_uapi::mount_flags::FileSystemFlags;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::{
    EROFS_SUPER_MAGIC_V1, errno, error, from_status_like_fdio, ino_t, off_t, statfs,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use zx::{MonotonicInstant, VmoChildOptions};

/// Caches whether the `fuchsia.storage.mapping.FileMapper` protocol is routed and active.
///
/// Because `connect_to_protocol_sync` only pipelines the channel open to `/svc`, an unrouted
/// protocol manifests as `PEER_CLOSED` on the first `FileMapper.OpenSession` call. Clearing this
/// flag on transport error avoids repeated failed connection attempts on subsequent mounts, while
/// per-file application errors (`Result::Err(zx_status)`) fall back without disabling `FileMapper`
/// globally.
static FILE_MAPPER_AVAILABLE: AtomicBool = AtomicBool::new(true);

/// Number of synthetic directory entries (`"."` and `".."`) emitted by `emit_dotdot` before
/// reading entries from `ErofsFilesystem`.
const DOT_DOTDOT_OFFSET: off_t = 2;

/// Number of directory entries fetched per `ErofsFilesystem::read_directory` batch.
const READDIR_BATCH_SIZE: usize = 16;

/// Starnix `FileSystemOps` implementation for an in-process EROFS volume.
struct ErofsFileSystem {
    /// Parsed EROFS filesystem metadata and reader over the image VMO.
    fs: Arc<ErofsFilesystem>,
    /// Driver-backed child `MapperSession` used to create pager-backed VMOs for regular files.
    mapper: VolumeMapper,
}

impl FileSystemOps for ErofsFileSystem {
    fn name(&self) -> &'static FsStr {
        "erofs".into()
    }

    fn uses_external_node_ids(&self) -> bool {
        true
    }

    fn statfs(&self, _fs: &FileSystem, _current_task: &CurrentTask) -> Result<statfs, Errno> {
        let mut stat = default_statfs(EROFS_SUPER_MAGIC_V1);
        let block_size = self.fs.block_size();
        stat.f_bsize = block_size as i64;
        stat.f_frsize = block_size as i64;
        // EROFS is strictly read-only, so free block/inode counts remain 0 from `default_statfs`.
        stat.f_blocks = (self.fs.total_bytes() / block_size).try_into().unwrap_or(i64::MAX);
        stat.f_files = self.fs.total_inodes().try_into().unwrap_or(i64::MAX);
        Ok(stat)
    }

    fn reconfigure(
        &self,
        fs: &FileSystem,
        _current_task: &CurrentTask,
        new_flags: FileSystemFlags,
        _params: &MountParams,
    ) -> Result<(), Errno> {
        if !new_flags.contains(FileSystemFlags::RDONLY) {
            return error!(EROFS);
        }
        fs.options.flags.store(new_flags, Ordering::Relaxed);
        Ok(())
    }
}

/// Converts on-disk EROFS inode metadata (`NodeInner`) into Starnix's `FsNodeInfo`.
fn fs_node_info(node: &NodeInner, block_size: u64) -> FsNodeInfo {
    // EROFS only stores a single timestamp (mtime / build time) per inode, so all three POSIX
    // timestamps are initialized to `mtime_ns`.
    let time = UtcInstant::from_nanos(node.mtime_ns() as i64);
    let storage_size = node.storage_size(block_size);
    FsNodeInfo {
        mode: FileMode::from_bits(node.mode().into()),
        link_count: node.link_count() as usize,
        uid: node.uid(),
        gid: node.gid(),
        size: node.size() as usize,
        // `st_blocks` is always reported in 512-byte (`DEFAULT_BYTES_PER_BLOCK`) units.
        blksize: DEFAULT_BYTES_PER_BLOCK,
        blocks: storage_size.div_ceil(DEFAULT_BYTES_PER_BLOCK as u64) as usize,
        time_status_change: time,
        time_access: time,
        time_modify: time,
        ..Default::default()
    }
}

/// Reads a single extended attribute `name` from `erofs_node`, mapping a missing attribute to
/// `ENODATA`.
fn get_node_xattr(
    node: &FsNode,
    erofs_node: &NodeInner,
    name: &FsStr,
) -> Result<ValueOrSize<FsString>, Errno> {
    let fs = node.fs();
    let fs_ops = fs.downcast_ops::<ErofsFileSystem>().unwrap();
    match fs_ops.fs.get_xattr(erofs_node, name.as_ref()).map_err(|e| errno!(EIO, e))? {
        Some(val) => Ok(FsString::from(val).into()),
        None => error!(ENODATA),
    }
}

/// Lists all extended attribute names on `erofs_node`.
fn list_node_xattrs(
    node: &FsNode,
    erofs_node: &NodeInner,
) -> Result<ValueOrSize<Vec<FsString>>, Errno> {
    let fs = node.fs();
    let fs_ops = fs.downcast_ops::<ErofsFileSystem>().unwrap();
    let names = fs_ops.fs.list_xattrs(erofs_node).map_err(|e| errno!(EIO, e))?;
    Ok(names.into_iter().map(FsString::from).collect::<Vec<_>>().into())
}

/// `FsNodeOps` implementation for an EROFS directory inode.
struct ErofsDirectory {
    node: DirectoryNode,
}

impl FsNodeOps for ErofsDirectory {
    fs_node_impl_dir_readonly!();

    fn create_file_ops(
        &self,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        Ok(Box::new(ErofsDirFileObject { node: self.node.clone() }))
    }

    fn lookup(
        &self,
        entry: &DirEntry,
        _current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<FsNodeHandle, Errno> {
        // The `erofs` parser validates directory entry names as UTF-8, so any non-UTF-8 lookup
        // name cannot match an existing entry.
        let name_str = std::str::from_utf8(name.as_ref()).map_err(|_| errno!(ENOENT))?;
        let fs = entry.node.fs();
        let fs_ops = fs.downcast_ops::<ErofsFileSystem>().unwrap();
        let child_node = fs_ops
            .fs
            .lookup(&self.node, name_str)
            .map_err(|e| errno!(EIO, e))?
            .ok_or_else(|| errno!(ENOENT, name))?;
        // EROFS 64-bit node IDs (`nid`) uniquely identify inodes within the volume and are used
        // directly as Starnix inode numbers (`ino_t`).
        let nid = child_node.nid() as ino_t;
        fs.get_or_create_node(nid, || {
            let info = fs_node_info(&child_node, fs_ops.fs.block_size());
            let ops: Box<dyn FsNodeOps> = match child_node {
                Node::Directory(dir) => Box::new(ErofsDirectory { node: dir }),
                Node::File(file) => Box::new(ErofsFile { node: file, memory: OnceLock::new() }),
                Node::Symlink(symlink) => {
                    Box::new(ErofsSymlink { node: symlink, target: OnceLock::new() })
                }
            };
            Ok(FsNode::new_uncached(nid, ops, &fs, info, FsNodeFlags::empty()))
        })
    }

    fn get_xattr(
        &self,
        node: &FsNode,
        _current_task: &CurrentTask,
        name: &FsStr,
        _max_size: usize,
    ) -> Result<ValueOrSize<FsString>, Errno> {
        get_node_xattr(node, &self.node, name)
    }

    fn list_xattrs(
        &self,
        node: &FsNode,
        _current_task: &CurrentTask,
        _max_size: usize,
    ) -> Result<ValueOrSize<Vec<FsString>>, Errno> {
        list_node_xattrs(node, &self.node)
    }
}

/// `FileOps` implementation for an open EROFS directory descriptor.
struct ErofsDirFileObject {
    node: DirectoryNode,
}

impl FileOps for ErofsDirFileObject {
    fileops_impl_directory!();
    fileops_impl_noop_sync!();

    fn seek(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        current_offset: off_t,
        target: SeekTarget,
    ) -> Result<off_t, Errno> {
        default_seek(current_offset, target, || error!(EINVAL))
    }

    fn readdir(
        &self,
        file: &FileObject,
        _current_task: &CurrentTask,
        sink: &mut dyn DirentSink,
    ) -> Result<(), Errno> {
        // `emit_dotdot` emits `"."` at `sink.offset() == 0` and `".."` at `sink.offset() == 1`
        // (resolving `".."` via the VFS `DirEntry` parent across mount boundaries), leaving
        // `sink.offset() >= DOT_DOTDOT_OFFSET`.
        emit_dotdot(file, sink)?;

        let fs = file.node().fs();
        let fs_ops = fs.downcast_ops::<ErofsFileSystem>().unwrap();
        let mut entry_offset = (sink.offset() - DOT_DOTDOT_OFFSET) as usize;
        let mut buffer = vec![ErofsDirEntry::default(); READDIR_BATCH_SIZE];

        loop {
            let filled = fs_ops
                .fs
                .read_directory(&self.node, entry_offset, &mut buffer)
                .map_err(|e| errno!(EIO, e))?;
            for i in 0..filled {
                let entry = &buffer[i];
                let next_entry_offset = entry_offset + i + 1;
                // On-disk EROFS directories include `"."` and `".."` entries, which were already
                // emitted by `emit_dotdot` above. Skip them while still advancing
                // `next_entry_offset` so subsequent entries map to stable offsets.
                if entry.name == "." || entry.name == ".." {
                    continue;
                }
                let entry_type = match entry.file_type {
                    FileType::RegFile => DirectoryEntryType::REG,
                    FileType::Dir => DirectoryEntryType::DIR,
                    FileType::ChrDev => DirectoryEntryType::CHR,
                    FileType::BlkDev => DirectoryEntryType::BLK,
                    FileType::Fifo => DirectoryEntryType::FIFO,
                    FileType::Sock => DirectoryEntryType::SOCK,
                    FileType::Symlink => DirectoryEntryType::LNK,
                    FileType::Unknown => DirectoryEntryType::UNKNOWN,
                };
                sink.add(
                    entry.nid as ino_t,
                    next_entry_offset as off_t + DOT_DOTDOT_OFFSET,
                    entry_type,
                    entry.name.as_bytes().into(),
                )?;
            }
            if filled < buffer.len() {
                break;
            }
            entry_offset += filled;
        }
        Ok(())
    }
}

/// `FsNodeOps` implementation for an EROFS regular file inode.
struct ErofsFile {
    node: FileNode,
    /// Lazily initialized `MemoryObject` wrapping the file's VMO, created on the first `open`
    /// (`create_file_ops`) rather than during `lookup`/`stat`.
    memory: OnceLock<Arc<MemoryObject>>,
}

impl FsNodeOps for ErofsFile {
    fs_node_impl_not_dir!();

    fn create_file_ops(
        &self,
        node: &FsNode,
        _current_task: &CurrentTask,
        flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        if flags.can_write() {
            return error!(EROFS);
        }
        let memory = if let Some(memory) = self.memory.get() {
            memory.clone()
        } else {
            let fs = node.fs();
            let fs_ops = fs.downcast_ops::<ErofsFileSystem>().unwrap();
            let vmo = fs_ops
                .mapper
                .get_or_create_vmo(&fs_ops.fs, &self.node)
                .map_err(|status| from_status_like_fdio!(status))?;
            let memory = Arc::new(MemoryObject::from(vmo).with_zx_name(b"starnix:erofs"));
            // If another thread raced to initialize `self.memory`, use the winning `MemoryObject`.
            let _ = self.memory.set(memory);
            self.memory.get().unwrap().clone()
        };
        Ok(Box::new(MemoryRegularFile::new(memory)))
    }

    fn get_xattr(
        &self,
        node: &FsNode,
        _current_task: &CurrentTask,
        name: &FsStr,
        _max_size: usize,
    ) -> Result<ValueOrSize<FsString>, Errno> {
        get_node_xattr(node, &self.node, name)
    }

    fn list_xattrs(
        &self,
        node: &FsNode,
        _current_task: &CurrentTask,
        _max_size: usize,
    ) -> Result<ValueOrSize<Vec<FsString>>, Errno> {
        list_node_xattrs(node, &self.node)
    }
}

/// `FsNodeOps` implementation for an EROFS symbolic link inode.
struct ErofsSymlink {
    node: SymlinkNode,
    /// Cached symlink target string, populated on the first `readlink` call.
    target: OnceLock<FsString>,
}

impl FsNodeOps for ErofsSymlink {
    fs_node_impl_symlink!();

    fn readlink(&self, node: &FsNode, _current_task: &CurrentTask) -> Result<SymlinkTarget, Errno> {
        if let Some(target) = self.target.get() {
            return Ok(SymlinkTarget::Path(target.clone()));
        }
        let fs = node.fs();
        let fs_ops = fs.downcast_ops::<ErofsFileSystem>().unwrap();
        let target: FsString =
            fs_ops.fs.read_symlink(&self.node).map_err(|e| errno!(EIO, e))?.into();
        let _ = self.target.set(target.clone());
        Ok(SymlinkTarget::Path(target))
    }

    fn get_xattr(
        &self,
        node: &FsNode,
        _current_task: &CurrentTask,
        name: &FsStr,
        _max_size: usize,
    ) -> Result<ValueOrSize<FsString>, Errno> {
        get_node_xattr(node, &self.node, name)
    }

    fn list_xattrs(
        &self,
        node: &FsNode,
        _current_task: &CurrentTask,
        _max_size: usize,
    ) -> Result<ValueOrSize<Vec<FsString>>, Errno> {
        list_node_xattrs(node, &self.node)
    }
}

/// Mounts an EROFS filesystem from `options.source` (with optional page-aligned `fsoffset` mount
/// parameter), using in-process driver mapping via `fuchsia.storage.mapping.FileMapper` when
/// available and falling back to the out-of-process `fuchsia.erofs.Erofs` component otherwise.
pub fn new_fs(
    current_task: &CurrentTask,
    options: FileSystemOptions,
) -> Result<FileSystemHandle, Errno> {
    let kernel = current_task.kernel();

    // EROFS is strictly read-only.
    if !options.flags.load(Ordering::Relaxed).contains(FileSystemFlags::RDONLY) {
        return Err(errno!(EROFS));
    }

    let fsoffset = options.params.get_as::<u64>(b"fsoffset")?;
    let page_size = zx::system_get_page_size() as u64;

    let source_device = current_task.open_file(options.source.as_ref(), OpenFlags::RDONLY)?;
    let memory = source_device.get_memory(current_task, None, ProtectionFlags::READ)?;
    let backing_vmo = memory
        .as_vmo()
        .ok_or_else(|| errno!(EINVAL))?
        .duplicate_handle(zx::Rights::SAME_RIGHTS)
        .map_err(impossible_error)?;

    // If `fsoffset` is non-zero, create a `SLICE` child VMO starting at `fsoffset` so that offset
    // 0 of `backing_vmo` corresponds to the start of the EROFS image for both `VmoReader` and the
    // `Erofs.Serve` fallback.
    let backing_vmo = match fsoffset {
        None => {
            log_info!("mounting erofs image source={:?}", options.source);
            backing_vmo
        }
        Some(fsoffset) if fsoffset > 0 => {
            if fsoffset % page_size != 0 {
                log_warn!("fsoffset {} is not page-aligned (page size {})", fsoffset, page_size);
                return Err(errno!(EINVAL));
            }
            let vmo_size = backing_vmo.get_size().map_err(impossible_error)?;
            let clone_len =
                vmo_size.checked_sub(fsoffset).filter(|&len| len > 0).ok_or_else(|| {
                    log_warn!("fsoffset {} exceeds or equals vmo size {}", fsoffset, vmo_size);
                    errno!(EINVAL)
                })?;
            log_info!("mounting erofs image with fsoffset={}, len={}", fsoffset, clone_len);
            backing_vmo.create_child(VmoChildOptions::SLICE, fsoffset, clone_len).map_err(|e| {
                log_error!("Failed to create child VMO slice for fsoffset {}: {:?}", fsoffset, e);
                errno!(EINVAL)
            })?
        }
        Some(_) => {
            log_info!("mounting erofs image with fsoffset of zero");
            backing_vmo
        }
    };

    // Fast path: if the source device provides a page-aligned `FileToken` for the underlying
    // Fuchsia blob and `FileMapper` is available, open a child `MapperSession` and serve EROFS
    // in-process.
    if FILE_MAPPER_AVAILABLE.load(Ordering::Relaxed) {
        if let Ok(file_token) = source_device
            .get_file_token(current_task)
            .and_then(|t| t.add_offset(fsoffset.unwrap_or(0)))
            .ok()
            .filter(|t| t.offset % page_size == 0)
            .ok_or(())
        {
            if let Ok(file_mapper) = connect_to_protocol_sync::<FileMapperMarker>() {
                let token = file_token.token;
                let file_offset = file_token.offset;
                let keep_alive = file_token.keep_alive;
                let mapper_res =
                    VolumeMapper::open_with_session(file_offset, |mapping_vmo, session_server| {
                        file_mapper
                            .open_session(
                                token,
                                mapping_vmo,
                                session_server,
                                MonotonicInstant::INFINITE,
                            )
                            .map_err(|_| {
                                FILE_MAPPER_AVAILABLE.store(false, Ordering::Relaxed);
                                zx::Status::PEER_CLOSED
                            })?
                            .map_err(zx::Status::err_from_raw)
                    });
                // `keep_alive` keeps the `fuchsia.io/Node.GetToken` token registered in the server's
                // VFS token registry until `FileMapper.OpenSession` has resolved it to a `VmoBlob`
                // and opened the child `MapperSession`.
                drop(keep_alive);
                if let Ok(mapper) = mapper_res {
                    let reader = Arc::new(
                        VmoReader::new(Arc::new(backing_vmo)).map_err(|e| errno!(EIO, e))?,
                    );
                    let erofs_fs =
                        Arc::new(ErofsFilesystem::new(reader).map_err(|e| errno!(EINVAL, e))?);
                    let root_node = erofs_fs.root_node();
                    let root_nid = root_node.nid() as ino_t;
                    let root_info = fs_node_info(&root_node, erofs_fs.block_size());
                    let fs_ops = ErofsFileSystem { fs: erofs_fs, mapper };
                    let fs = FileSystem::new(
                        kernel,
                        CacheMode::Cached(kernel.fs_cache_config()),
                        fs_ops,
                        options,
                    )?;
                    fs.create_root_with_info(
                        root_nid,
                        ErofsDirectory { node: root_node },
                        root_info,
                    );
                    return Ok(fs);
                }
            } else {
                FILE_MAPPER_AVAILABLE.store(false, Ordering::Relaxed);
            }
        }
    }

    // Fallback path: delegate serving the EROFS image to the out-of-process `fuchsia.erofs.Erofs`
    // component and mount the resulting `fuchsia.io/Directory` channel as a `RemoteFs`.
    let erofs_provider = connect_to_protocol_sync::<ErofsMarker>().map_err(|_| errno!(ENOENT))?;

    let (root_client, root_server) = fidl::endpoints::create_endpoints();
    erofs_provider
        .serve(
            ErofsServeRequest {
                backing_vmo: Some(backing_vmo),
                root: Some(root_server),
                ..Default::default()
            },
            MonotonicInstant::INFINITE,
        )
        .map_err(|e| {
            log_error!("FIDL error on Erofs.Serve: {e:?}");
            errno!(EIO)
        })?
        .map_err(|e| {
            log_error!("Erofs.Serve failed: {e:?}");
            from_status_like_fdio!(zx::Status::err_from_raw(e))
        })?;

    let (remotefs, root_node, info, node_id) =
        RemoteFs::new(root_client.into_channel(), fio::PERM_READABLE, "erofs")?;

    let fs =
        FileSystem::new(kernel, CacheMode::Cached(kernel.fs_cache_config()), remotefs, options)?;

    fs.create_root_with_info(node_id, root_node, info);

    Ok(fs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl_fuchsia_storage_block as fblock;
    use futures::TryStreamExt as _;
    use starnix_core::testing::spawn_kernel_and_run;
    use starnix_core::vfs::buffers::VecOutputBuffer;
    use starnix_core::vfs::{LookupContext, Namespace, NamespaceNode, SymlinkMode};
    use std::sync::atomic::AtomicUsize;

    struct TestDirentSink {
        offset: off_t,
        max_entries: Option<usize>,
        entries: Vec<(String, DirectoryEntryType)>,
    }

    impl TestDirentSink {
        fn new() -> Self {
            Self { offset: 0, max_entries: None, entries: Vec::new() }
        }

        fn with_limit(offset: off_t, max_entries: usize) -> Self {
            Self { offset, max_entries: Some(max_entries), entries: Vec::new() }
        }
    }

    impl DirentSink for TestDirentSink {
        fn add(
            &mut self,
            _inode_num: ino_t,
            offset: off_t,
            entry_type: DirectoryEntryType,
            name: &FsStr,
        ) -> Result<(), Errno> {
            if let Some(max) = self.max_entries {
                if self.entries.len() >= max {
                    return error!(ENOSPC);
                }
            }
            self.offset = offset;
            self.entries.push((String::from_utf8_lossy(name.as_ref()).into_owned(), entry_type));
            Ok(())
        }

        fn offset(&self) -> off_t {
            self.offset
        }
    }

    fn mount_test_erofs(
        current_task: &CurrentTask,
        create_vmo_count: Arc<AtomicUsize>,
        fail_create_vmo: bool,
    ) -> (FileSystemHandle, Arc<Namespace>) {
        mount_test_erofs_from_path(
            current_task,
            "/pkg/data/simple.erofs",
            create_vmo_count,
            fail_create_vmo,
        )
    }

    fn mount_test_erofs_from_path(
        current_task: &CurrentTask,
        image_path: &str,
        create_vmo_count: Arc<AtomicUsize>,
        fail_create_vmo: bool,
    ) -> (FileSystemHandle, Arc<Namespace>) {
        let erofs_bytes = std::fs::read(image_path).expect("read erofs image");
        let expected_file1 = std::fs::read("/pkg/data/simple/file1").expect("read file1");
        let expected_photosynthesis =
            std::fs::read("/pkg/data/simple/photosynthesis").expect("read photosynthesis");

        let mapper = VolumeMapper::open_with_session(4096, move |_mapping_vmo, server_end| {
            std::thread::spawn(move || {
                let mut executor = fuchsia_async::LocalExecutor::default();
                executor.run_singlethreaded(async move {
                    let mut stream = server_end.into_stream();
                    while let Ok(Some(req)) = stream.try_next().await {
                        match req {
                            fblock::MapperSessionRequest::CreateVmo { size, responder, .. } => {
                                create_vmo_count.fetch_add(1, Ordering::Relaxed);
                                if fail_create_vmo {
                                    let _ = responder.send(Err(zx::Status::IO.into_raw()));
                                    continue;
                                }
                                let vmo = zx::Vmo::create(size).unwrap();
                                if size == expected_file1.len() as u64 {
                                    vmo.write(&expected_file1, 0).unwrap();
                                } else if size == expected_photosynthesis.len() as u64 {
                                    vmo.write(&expected_photosynthesis, 0).unwrap();
                                }
                                let _ = responder.send(Ok(vmo));
                            }
                            fblock::MapperSessionRequest::CloseVmo { .. } => {}
                            fblock::MapperSessionRequest::Close { responder } => {
                                let _ = responder.send(Ok(()));
                                break;
                            }
                            _ => unreachable!(),
                        }
                    }
                });
            });
            Ok(())
        })
        .expect("open_with_session");

        let vmo = zx::Vmo::create(erofs_bytes.len() as u64).unwrap();
        vmo.write(&erofs_bytes, 0).unwrap();
        let reader = Arc::new(VmoReader::new(Arc::new(vmo)).unwrap());
        let erofs_fs = Arc::new(ErofsFilesystem::new(reader).unwrap());

        let kernel = current_task.kernel();
        let root_node = erofs_fs.root_node();
        let root_nid = root_node.nid() as ino_t;
        let root_info = fs_node_info(&root_node, erofs_fs.block_size());
        let fs_ops = ErofsFileSystem { fs: erofs_fs, mapper };
        let fs = FileSystem::new(
            kernel,
            CacheMode::Cached(kernel.fs_cache_config()),
            fs_ops,
            FileSystemOptions::default(),
        )
        .expect("FileSystem::new");
        fs.create_root_with_info(root_nid, ErofsDirectory { node: root_node }, root_info);
        let ns = Namespace::new(fs.clone());
        (fs, ns)
    }

    fn lookup_node(
        current_task: &CurrentTask,
        parent: &NamespaceNode,
        name: &[u8],
    ) -> NamespaceNode {
        let mut context = LookupContext::new(SymlinkMode::NoFollow);
        parent.lookup_child(current_task, &mut context, name.into()).expect("lookup_child")
    }

    #[::fuchsia::test]
    async fn test_statfs_and_node_metadata() {
        spawn_kernel_and_run(async move |current_task| {
            let expected_file1 = std::fs::read("/pkg/data/simple/file1").expect("read file1");
            let (fs, ns) = mount_test_erofs(current_task, Arc::new(AtomicUsize::new(0)), false);

            let stat = fs.statfs(current_task).expect("statfs");
            assert_eq!(stat.f_type, EROFS_SUPER_MAGIC_V1 as i64);
            assert_eq!(stat.f_bsize, 4096);
            assert!(stat.f_blocks > 0);
            assert!(stat.f_files > 0);

            // Remounting without RDONLY must fail with EROFS, whereas keeping RDONLY succeeds.
            assert_eq!(
                fs.reconfigure(current_task, FileSystemFlags::empty(), &MountParams::default()),
                error!(EROFS)
            );
            assert!(
                fs.reconfigure(current_task, FileSystemFlags::RDONLY, &MountParams::default())
                    .is_ok()
            );

            // `new_fs` without RDONLY must fail with EROFS.
            assert_eq!(
                new_fs(current_task, FileSystemOptions::default()).map(|_| ()),
                error!(EROFS)
            );

            // Create an 8192-byte backing file in TmpFs to test `new_fs` `fsoffset` validation.
            let img_file = current_task
                .open_file_at(
                    starnix_core::vfs::FdNumber::AT_FDCWD,
                    "/image.bin".into(),
                    OpenFlags::CREAT | OpenFlags::RDWR,
                    FileMode::from_bits(0o644),
                    starnix_uapi::vfs::ResolveFlags::empty(),
                )
                .expect("create /image.bin");
            img_file
                .node()
                .truncate(current_task, &current_task.fs().root().mount, 8192)
                .expect("truncate /image.bin");
            for bad_param in [b"fsoffset=100".as_slice(), b"fsoffset=8192", b"fsoffset=16384"] {
                let options = FileSystemOptions {
                    source: "/image.bin".into(),
                    flags: FileSystemFlags::RDONLY.into(),
                    params: MountParams::parse(bad_param.into()).expect("parse MountParams"),
                };
                assert_eq!(new_fs(current_task, options).map(|_| ()), error!(EINVAL));
            }

            let root = ns.root();
            {
                let root_info = root.entry.node.info();
                assert!(root_info.mode.is_dir());
                assert!(root_info.link_count >= 2);
            }

            let file1_entry = lookup_node(current_task, &root, b"file1");
            {
                let file1_info = file1_entry.entry.node.info();
                assert!(file1_info.mode.is_reg());
                assert_eq!(file1_info.size, expected_file1.len());
                assert_eq!(file1_info.blksize, DEFAULT_BYTES_PER_BLOCK);
                assert_eq!(
                    file1_info.blocks,
                    expected_file1.len().div_ceil(DEFAULT_BYTES_PER_BLOCK)
                );
            }

            let symlink_entry = lookup_node(current_task, &root, b"symlink_to_file1");
            assert!(symlink_entry.entry.node.info().mode.is_lnk());
            let SymlinkTarget::Path(target) =
                symlink_entry.readlink(current_task).expect("readlink")
            else {
                panic!("expected SymlinkTarget::Path");
            };
            assert_eq!(target, "file1");
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_directory_readdir_pagination_and_seek() {
        spawn_kernel_and_run(async move |current_task| {
            let (_fs, ns) = mount_test_erofs(current_task, Arc::new(AtomicUsize::new(0)), false);
            let root = ns.root();

            // 1. Root directory readdir
            let root_dir = root.open(current_task, OpenFlags::RDONLY).expect("open root");
            let mut sink = TestDirentSink::new();
            root_dir.readdir(current_task, &mut sink).expect("readdir root");
            let names: Vec<&str> = sink.entries.iter().map(|(n, _)| n.as_str()).collect();
            assert_eq!(
                names,
                vec![
                    ".",
                    "..",
                    "file1",
                    "large_dir",
                    "mixed_compression",
                    "photosynthesis",
                    "quantum",
                    "symlink_to_file1",
                ]
            );

            // 2. Directory seek (Set, Cur, and negative offset error)
            assert_eq!(root_dir.seek(current_task, SeekTarget::Set(0)).unwrap(), 0);
            assert_eq!(root_dir.seek(current_task, SeekTarget::Cur(3)).unwrap(), 3);
            assert_eq!(root_dir.seek(current_task, SeekTarget::Set(-1)), error!(EINVAL));

            // 3. Multi-batch readdir on `large_dir` (> 16 entries across multiple blocks)
            let large_dir_node = lookup_node(current_task, &root, b"large_dir");
            let large_dir =
                large_dir_node.open(current_task, OpenFlags::RDONLY).expect("open large_dir");
            let mut full_sink = TestDirentSink::new();
            large_dir.readdir(current_task, &mut full_sink).expect("readdir large_dir");
            assert!(
                full_sink.entries.len() > 32,
                "expected > 32 entries in large_dir, got {}",
                full_sink.entries.len()
            );
            assert_eq!(full_sink.entries[0], (".".to_string(), DirectoryEntryType::DIR));
            assert_eq!(full_sink.entries[1], ("..".to_string(), DirectoryEntryType::DIR));
            for (name, kind) in &full_sink.entries[2..] {
                assert!(name.starts_with("file_number_"), "unexpected entry: {name}");
                assert_eq!(*kind, DirectoryEntryType::REG);
            }

            // 4. Resuming readdir after partial-buffer ENOSPC (5 entries at a time)
            let mut paged_entries = Vec::new();
            let mut offset = 0;
            loop {
                let mut page_sink = TestDirentSink::with_limit(offset, 5);
                let res = large_dir.readdir(current_task, &mut page_sink);
                let count = page_sink.entries.len();
                offset = page_sink.offset();
                paged_entries.extend(page_sink.entries);
                if res.is_ok() {
                    break;
                }
                assert_eq!(res, error!(ENOSPC));
                assert_eq!(count, 5);
            }
            assert_eq!(paged_entries, full_sink.entries);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_file_read_get_memory_and_caching() {
        spawn_kernel_and_run(async move |current_task| {
            let expected_file1 = std::fs::read("/pkg/data/simple/file1").expect("read file1");
            let expected_photosynthesis =
                std::fs::read("/pkg/data/simple/photosynthesis").expect("read photosynthesis");
            let create_vmo_count = Arc::new(AtomicUsize::new(0));
            let (_fs, ns) = mount_test_erofs(current_task, create_vmo_count.clone(), false);
            let root = ns.root();

            // Lookup alone must not create any VMOs.
            let file1_entry = lookup_node(current_task, &root, b"file1");
            assert_eq!(create_vmo_count.load(Ordering::Relaxed), 0);

            // Opening for write must fail with EROFS.
            assert_eq!(file1_entry.open(current_task, OpenFlags::RDWR).unwrap_err(), errno!(EROFS));
            assert_eq!(create_vmo_count.load(Ordering::Relaxed), 0);

            // First open creates the VMO via VolumeMapper.
            let file1 = file1_entry.open(current_task, OpenFlags::RDONLY).expect("open file1");
            assert_eq!(create_vmo_count.load(Ordering::Relaxed), 1);

            // Second open reuses the cached MemoryObject without another CreateVmo RPC.
            let _file1_again =
                file1_entry.open(current_task, OpenFlags::RDONLY).expect("re-open file1");
            assert_eq!(create_vmo_count.load(Ordering::Relaxed), 1);

            let mut buf = VecOutputBuffer::new(expected_file1.len() + 64);
            let n = file1.read(current_task, &mut buf).expect("read file1");
            assert_eq!(n, expected_file1.len());
            assert_eq!(buf.data(), expected_file1.as_slice());

            // Test get_memory (READ, READ|EXEC, and WRITE rejected with EACCES).
            let mem_ro =
                file1.get_memory(current_task, None, ProtectionFlags::READ).expect("get_memory RO");
            assert_eq!(mem_ro.read_to_vec(0, expected_file1.len() as u64).unwrap(), expected_file1);
            let mem_exec = file1
                .get_memory(current_task, None, ProtectionFlags::READ | ProtectionFlags::EXEC)
                .expect("get_memory EXEC");
            assert!(mem_exec.get_rights().contains(zx::Rights::EXECUTE));
            assert_eq!(
                file1.get_memory(current_task, None, ProtectionFlags::WRITE).unwrap_err(),
                errno!(EACCES)
            );

            // Test multi-block photosynthesis file.
            let photo_file = lookup_node(current_task, &root, b"photosynthesis")
                .open(current_task, OpenFlags::RDONLY)
                .expect("open photosynthesis");
            assert_eq!(create_vmo_count.load(Ordering::Relaxed), 2);
            let mut buf = VecOutputBuffer::new(expected_photosynthesis.len() + 64);
            let n = photo_file.read(current_task, &mut buf).expect("read photosynthesis");
            assert_eq!(n, expected_photosynthesis.len());
            assert_eq!(buf.data(), expected_photosynthesis.as_slice());

            // Test compressed file (`mixed_compression` in `simple_lz4.erofs`), which is
            // decompressed in-process into an anonymous VMO without calling
            // `MapperSession.CreateVmo`.
            let expected_mixed = std::fs::read("/pkg/data/simple/mixed_compression")
                .expect("read mixed_compression");
            let lz4_vmo_count = Arc::new(AtomicUsize::new(0));
            let (_lz4_fs, lz4_ns) = mount_test_erofs_from_path(
                current_task,
                "/pkg/data/simple_lz4.erofs",
                lz4_vmo_count.clone(),
                false,
            );
            let mixed_file = lookup_node(current_task, &lz4_ns.root(), b"mixed_compression")
                .open(current_task, OpenFlags::RDONLY)
                .expect("open mixed_compression");
            assert_eq!(lz4_vmo_count.load(Ordering::Relaxed), 0);
            let mut buf = VecOutputBuffer::new(expected_mixed.len() + 64);
            let n = mixed_file.read(current_task, &mut buf).expect("read mixed_compression");
            assert_eq!(n, expected_mixed.len());
            assert_eq!(buf.data(), expected_mixed.as_slice());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_xattrs_and_error_paths() {
        spawn_kernel_and_run(async move |current_task| {
            let (_fs, ns) = mount_test_erofs(current_task, Arc::new(AtomicUsize::new(0)), false);
            let root = ns.root();

            // Non-existent and invalid UTF-8 lookups must return ENOENT.
            let mut context = LookupContext::new(SymlinkMode::NoFollow);
            assert_eq!(
                root.lookup_child(current_task, &mut context, "does_not_exist".into()).unwrap_err(),
                errno!(ENOENT)
            );
            let mut context = LookupContext::new(SymlinkMode::NoFollow);
            assert_eq!(
                root.lookup_child(current_task, &mut context, b"\xff\xfe".into()).unwrap_err(),
                errno!(ENOENT)
            );

            // File xattrs: present and absent (ENODATA).
            let file1_node = &lookup_node(current_task, &root, b"file1").entry.node;
            let ValueOrSize::Value(flavor) = file1_node
                .ops()
                .get_xattr(file1_node, current_task, "user.flavor".into(), usize::MAX)
                .expect("get_xattr user.flavor")
            else {
                panic!("expected Value");
            };
            assert_eq!(flavor, "vanilla");
            assert_eq!(
                file1_node
                    .ops()
                    .get_xattr(file1_node, current_task, "user.missing".into(), usize::MAX)
                    .unwrap_err(),
                errno!(ENODATA)
            );

            let ValueOrSize::Value(mut xattr_list) = file1_node
                .ops()
                .list_xattrs(file1_node, current_task, usize::MAX)
                .expect("list_xattrs")
            else {
                panic!("expected Value");
            };
            xattr_list.sort();
            assert_eq!(
                xattr_list,
                vec![
                    FsString::from("security.selinux"),
                    FsString::from("user.flavor"),
                    FsString::from("user.security"),
                    FsString::from("user.shared"),
                ]
            );

            // Directory and symlink xattr calls.
            let root_node = &root.entry.node;
            assert!(root_node.ops().list_xattrs(root_node, current_task, usize::MAX).is_ok());
            assert_eq!(
                root_node
                    .ops()
                    .get_xattr(root_node, current_task, "user.missing".into(), usize::MAX)
                    .unwrap_err(),
                errno!(ENODATA)
            );

            let symlink_node = &lookup_node(current_task, &root, b"symlink_to_file1").entry.node;
            assert!(symlink_node.ops().list_xattrs(symlink_node, current_task, usize::MAX).is_ok());
            assert_eq!(
                symlink_node
                    .ops()
                    .get_xattr(symlink_node, current_task, "user.missing".into(), usize::MAX)
                    .unwrap_err(),
                errno!(ENODATA)
            );

            // MapperSession.CreateVmo failure propagation when opening a file.
            let (_bad_fs, bad_ns) =
                mount_test_erofs(current_task, Arc::new(AtomicUsize::new(0)), true);
            let bad_file1 = lookup_node(current_task, &bad_ns.root(), b"file1");
            assert_eq!(bad_file1.open(current_task, OpenFlags::RDONLY).unwrap_err(), errno!(EIO));
        })
        .await;
    }
}
