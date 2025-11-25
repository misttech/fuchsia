// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::mm::ProtectionFlags;
use crate::task::{CurrentTask, FullCredentials, Kernel, LockedAndTask};
use crate::vfs::buffers::{VecInputBuffer, VecOutputBuffer};
use crate::vfs::{
    DirectoryEntryType, DirentSink, FileHandle, FileObject, FsStr, FsString, LookupContext,
    NamespaceNode, RenameFlags, SeekTarget, UnlinkKind,
};
use fidl::HandleBased;
use fidl::endpoints::{ClientEnd, ServerEnd};
use fidl_fuchsia_io as fio;
use fuchsia_runtime::UtcInstant;
use futures::future::BoxFuture;
use itertools::Either;
use starnix_logging::{log_error, track_stub};
use starnix_sync::{Locked, Unlocked};
use starnix_types::convert::IntoFidl as _;
use starnix_uapi::device_type::DeviceType;
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::{AccessCheck, FileMode};
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::vfs::ResolveFlags;
use starnix_uapi::{errno, error, from_status_like_fdio, ino_t, off_t};
use std::sync::Arc;
use vfs::directory::mutable::connection::MutableConnection;
use vfs::directory::{self};
use vfs::{
    ObjectRequestRef, ProtocolsExt, ToObjectRequest, attributes, execution_scope, file, path,
};

/// Returns a handle implementing a fuchsia.io.Node delegating to the given `file`.
pub fn serve_file(
    current_task: &CurrentTask,
    file: &FileObject,
    credentials: FullCredentials,
) -> Result<(ClientEnd<fio::NodeMarker>, execution_scope::ExecutionScope), Errno> {
    let (client_end, server_end) = fidl::endpoints::create_endpoints::<fio::NodeMarker>();
    let scope = serve_file_at(server_end, current_task, file, credentials)?;
    Ok((client_end, scope))
}

pub fn serve_file_at(
    server_end: ServerEnd<fio::NodeMarker>,
    current_task: &CurrentTask,
    file: &FileObject,
    credentials: FullCredentials,
) -> Result<execution_scope::ExecutionScope, Errno> {
    let kernel = current_task.kernel();
    let open_flags = file.flags();
    let starnix_file =
        StarnixNodeConnection::new(&kernel, file.weak_handle.upgrade().unwrap(), credentials);
    let scope = execution_scope::ExecutionScope::new();
    kernel.kthreads.spawn_future({
        let scope = scope.clone();
        async move || {
            let fidl_flags: fio::OpenFlags = open_flags.into_fidl();
            if starnix_file.is_dir() {
                fidl_flags.to_object_request(server_end).handle(|object_request| {
                    object_request.take().create_connection_sync::<MutableConnection<_>, _>(
                        scope.clone(),
                        starnix_file,
                        fidl_flags,
                    );
                    Ok(())
                });
            } else {
                fidl_flags.to_object_request(server_end).handle(|object_request| {
                    object_request.take().create_connection_sync::<file::RawIoConnection<_>, _>(
                        scope.clone(),
                        starnix_file,
                        fidl_flags,
                    );
                    Ok(())
                });
            }
            scope.wait().await;
        }
    });
    Ok(scope)
}

#[async_trait::async_trait(?Send)]
trait Work: Send + 'static {
    async fn run(
        self: Box<Self>,
        locked: &mut Locked<Unlocked>,
        current_task: &CurrentTask,
        file: &FileHandle,
    );
}

struct EitherSender<R>(Either<futures::channel::oneshot::Sender<R>, std::sync::mpsc::Sender<R>>);

impl<R> From<futures::channel::oneshot::Sender<R>> for EitherSender<R> {
    fn from(v: futures::channel::oneshot::Sender<R>) -> Self {
        Self(Either::Left(v))
    }
}

impl<R> From<std::sync::mpsc::Sender<R>> for EitherSender<R> {
    fn from(v: std::sync::mpsc::Sender<R>) -> Self {
        Self(Either::Right(v))
    }
}

impl<R> EitherSender<R> {
    async fn send(self, r: R) {
        match self.0 {
            Either::Left(s) => {
                let _ = s.send(r);
            }
            Either::Right(s) => {
                let _ = s.send(r);
            }
        }
    }
}

struct WorkWrapper<R, F>
where
    R: Send + 'static,
    F: AsyncFnOnce(&mut Locked<Unlocked>, &CurrentTask, &FileHandle) -> R + Send + 'static,
{
    f: F,
    sender: EitherSender<R>,
}

#[async_trait::async_trait(?Send)]
impl<R, F> Work for WorkWrapper<R, F>
where
    R: Send + 'static,
    F: AsyncFnOnce(&mut Locked<Unlocked>, &CurrentTask, &FileHandle) -> R + Send + 'static,
{
    async fn run(
        self: Box<Self>,
        locked: &mut Locked<Unlocked>,
        current_task: &CurrentTask,
        file: &FileHandle,
    ) {
        let f: F = self.f;
        let r = f(locked, current_task, file).await;
        self.sender.send(r).await;
    }
}

async fn handle_file(
    locked_and_task: LockedAndTask<'_>,
    credentials: FullCredentials,
    file: FileHandle,
    receiver: std::sync::mpsc::Receiver<Box<dyn Work>>,
) {
    // Run with the correct credentials
    locked_and_task
        .current_task()
        .override_creds_async(
            |temp_creds| *temp_creds = credentials,
            async || {
                // Reopen file object to not share state with the given FileObject.
                let file = match file.name.open(
                    &mut locked_and_task.unlocked(),
                    locked_and_task.current_task(),
                    file.flags(),
                    AccessCheck::skip(),
                ) {
                    Ok(file) => file,
                    Err(e) => {
                        log_error!("Unable to reopen file: {e:?}");
                        return;
                    }
                };
                while let Ok(w) = receiver.recv() {
                    w.run(&mut locked_and_task.unlocked(), locked_and_task.current_task(), &file)
                        .await;
                }
            },
        )
        .await;
}

fn to_open_flags(flags: &impl ProtocolsExt) -> OpenFlags {
    let rights = flags.rights().unwrap_or_default();
    let mut open_flags = if rights.contains(fio::Operations::WRITE_BYTES) {
        if rights.contains(fio::Operations::READ_BYTES) {
            OpenFlags::RDWR
        } else {
            OpenFlags::WRONLY
        }
    } else {
        OpenFlags::RDONLY
    };

    if flags.create_directory() {
        open_flags |= OpenFlags::DIRECTORY;
    }

    match flags.creation_mode() {
        vfs::CreationMode::Always => open_flags |= OpenFlags::CREAT | OpenFlags::EXCL,
        vfs::CreationMode::AllowExisting => open_flags |= OpenFlags::CREAT,
        vfs::CreationMode::UnnamedTemporary => open_flags |= OpenFlags::TMPFILE,
        vfs::CreationMode::UnlinkableUnnamedTemporary => {
            open_flags |= OpenFlags::TMPFILE | OpenFlags::EXCL
        }
        vfs::CreationMode::Never => {}
    };

    if flags.is_truncate() {
        open_flags |= OpenFlags::TRUNC;
    }

    if flags.is_append() {
        open_flags |= OpenFlags::APPEND;
    }

    open_flags
}

/// A representation of `file` for the rust vfs.
///
/// This struct implements the following trait from the rust vfs library:
/// - directory::entry_container::Directory
/// - directory::entry_container::MutableDirectory
/// - file::File
/// - file::RawFileIoConnection
///
/// Each method is delegated back to the starnix vfs, using `task` as the current task. Blocking
/// methods are run from the kernel dynamic thread spawner so that the async dispatched do not
/// block on these.
/// All vfs operations should be done using `credentials`.
#[derive(Clone)]
struct StarnixNodeConnection {
    is_dir: bool,
    credentials: FullCredentials,
    work_sender: std::sync::mpsc::Sender<Box<dyn Work>>,
}

fn lookup_parent(
    locked: &mut Locked<Unlocked>,
    current_task: &CurrentTask,
    file: &FileObject,
    path: path::Path,
) -> Result<(NamespaceNode, FsString), Errno> {
    let (node, name) = current_task.lookup_parent(
        locked,
        &mut LookupContext::default(),
        &file.name,
        path.as_str().into(),
    )?;
    Ok((node, name.to_owned()))
}

impl StarnixNodeConnection {
    fn new(kernel: &Kernel, file: FileHandle, credentials: FullCredentials) -> Arc<Self> {
        let (work_sender, receiver) = std::sync::mpsc::channel();
        let is_dir = file.node().is_dir();
        kernel.kthreads.spawn_async({
            let credentials = credentials.clone();
            async move |locked_and_task| {
                handle_file(locked_and_task, credentials, file, receiver).await;
            }
        });
        Arc::new(Self { is_dir, credentials, work_sender })
    }

    fn spawn_task<R, E, F>(&self, f: F) -> Result<R, Errno>
    where
        R: Send + 'static,
        E: Send + 'static,
        F: AsyncFnOnce(&mut Locked<Unlocked>, &CurrentTask, &FileHandle) -> Result<R, E>
            + Send
            + 'static,
        Errno: From<E>,
    {
        let (sender, receiver) = std::sync::mpsc::channel();
        self.work_sender
            .send(Box::new(WorkWrapper { f, sender: sender.into() }))
            .map_err(|_| errno!(EIO))?;
        Ok(receiver.recv().map_err(|_| errno!(EIO))??)
    }

    async fn spawn_task_async<R, E, F>(&self, f: F) -> Result<R, Errno>
    where
        R: Send + 'static,
        E: Send + 'static,
        F: AsyncFnOnce(&mut Locked<Unlocked>, &CurrentTask, &FileHandle) -> Result<R, E>
            + Send
            + 'static,
        Errno: From<E>,
    {
        let (sender, receiver) = futures::channel::oneshot::channel();
        self.work_sender
            .send(Box::new(WorkWrapper { f, sender: sender.into() }))
            .map_err(|_| errno!(EIO))?;
        Ok(receiver.await.map_err(|_| errno!(EIO))??)
    }

    fn is_dir(&self) -> bool {
        self.is_dir
    }

    fn lookup_parent(&self, path: path::Path) -> Result<(NamespaceNode, FsString), Errno> {
        self.spawn_task(async move |locked, current_task, file| {
            lookup_parent(locked, current_task, file, path)
        })
    }

    /// Reopen the current `StarnixNodeConnection` with the given `OpenFlags`. The new file will not share
    /// state. It is equivalent to opening the same file, not dup'ing the file descriptor.
    fn reopen(&self, flags: &impl ProtocolsExt) -> Result<Arc<Self>, Errno> {
        let credentials = self.credentials.clone();
        let flags = to_open_flags(flags);
        self.spawn_task(async move |locked, current_task, file| {
            let file = file.name.open(locked, current_task, flags, AccessCheck::default())?;
            Ok(StarnixNodeConnection::new(&current_task.kernel(), file, credentials))
        })
    }

    /// Implementation of `vfs::directory::entry_container::Directory::directory_read_dirents`.
    fn directory_read_dirents<'a>(
        &'a self,
        pos: &'a directory::traversal_position::TraversalPosition,
        sink: Box<dyn directory::dirents_sink::Sink>,
    ) -> Result<
        (
            directory::traversal_position::TraversalPosition,
            Box<dyn directory::dirents_sink::Sealed>,
        ),
        Errno,
    > {
        let pos = pos.clone();
        self.spawn_task(async move |locked, current_task, file| {
            struct DirentSinkAdapter<'a> {
                sink: Option<directory::dirents_sink::AppendResult>,
                offset: &'a mut off_t,
            }
            impl<'a> DirentSinkAdapter<'a> {
                fn append(
                    &mut self,
                    entry: &directory::entry::EntryInfo,
                    name: &str,
                ) -> Result<(), Errno> {
                    let sink = self.sink.take();
                    self.sink = match sink {
                        s @ Some(directory::dirents_sink::AppendResult::Sealed(_)) => {
                            self.sink = s;
                            return error!(ENOSPC);
                        }
                        Some(directory::dirents_sink::AppendResult::Ok(sink)) => {
                            Some(sink.append(entry, name))
                        }
                        None => return error!(ENOTSUP),
                    };
                    Ok(())
                }
            }
            impl<'a> DirentSink for DirentSinkAdapter<'a> {
                fn add(
                    &mut self,
                    inode_num: ino_t,
                    offset: off_t,
                    entry_type: DirectoryEntryType,
                    name: &FsStr,
                ) -> Result<(), Errno> {
                    // Ignore ..
                    if name != ".." {
                        // Ignore entries with unknown types.
                        if let Some(dirent_type) =
                            fio::DirentType::from_primitive(entry_type.bits())
                        {
                            let entry_info =
                                directory::entry::EntryInfo::new(inode_num, dirent_type);
                            self.append(&entry_info, &String::from_utf8_lossy(name))?
                        }
                    }
                    *self.offset = offset;
                    Ok(())
                }
                fn offset(&self) -> off_t {
                    *self.offset
                }
            }
            let offset = match pos {
                directory::traversal_position::TraversalPosition::Start => 0,
                directory::traversal_position::TraversalPosition::Index(v) => v as i64,
                directory::traversal_position::TraversalPosition::End => {
                    return Ok((
                        directory::traversal_position::TraversalPosition::End,
                        sink.seal(),
                    ));
                }
                _ => return error!(EINVAL),
            };
            if *file.offset.lock() != offset {
                file.seek(locked, current_task, SeekTarget::Set(offset))?;
            }
            let mut file_offset = file.offset.lock();
            let mut dirent_sink = DirentSinkAdapter {
                sink: Some(directory::dirents_sink::AppendResult::Ok(sink)),
                offset: &mut file_offset,
            };
            file.readdir(locked, current_task, &mut dirent_sink)?;
            match dirent_sink.sink {
                Some(directory::dirents_sink::AppendResult::Sealed(seal)) => {
                    Ok((directory::traversal_position::TraversalPosition::End, seal))
                }
                Some(directory::dirents_sink::AppendResult::Ok(sink)) => Ok((
                    directory::traversal_position::TraversalPosition::Index(*file_offset as u64),
                    sink.seal(),
                )),
                None => error!(ENOTSUP),
            }
        })
    }

    /// Implementation of `vfs::directory::entry::DirectoryEntry::open`.
    fn directory_entry_open(
        self: Arc<Self>,
        scope: execution_scope::ExecutionScope,
        flags: impl ProtocolsExt,
        path: path::Path,
        object_request: ObjectRequestRef<'_>,
    ) -> Result<(), zx::Status> {
        if self.is_dir() {
            if path.is_dot() {
                // Reopen the current directory.
                let dir = self.reopen(&flags)?;
                object_request
                    .take()
                    .create_connection_sync::<MutableConnection<_>, _>(scope, dir, flags);
                return Ok(());
            }

            // Open a path under the current directory.
            let starnix_file = self.spawn_task({
                let credentials = self.credentials.clone();
                let create_directory = flags.creation_mode() != vfs::common::CreationMode::Never
                    && flags.create_directory();
                let open_flags = to_open_flags(&flags);
                async move |locked, current_task, file| {
                    let (node, name) = lookup_parent(locked, current_task, file, path)?;
                    let file = match current_task.open_namespace_node_at(
                        locked,
                        node.clone(),
                        name.as_ref(),
                        open_flags,
                        FileMode::ALLOW_ALL,
                        ResolveFlags::empty(),
                        AccessCheck::default(),
                    ) {
                        Err(e) if e == errno!(EISDIR) && create_directory => {
                            let mode = current_task
                                .fs()
                                .apply_umask(FileMode::from_bits(0o777) | FileMode::IFDIR);
                            let name = node.create_node(
                                locked,
                                &current_task,
                                name.as_ref(),
                                mode,
                                DeviceType::NONE,
                            )?;
                            name.open(
                                locked,
                                &current_task,
                                open_flags & !(OpenFlags::CREAT | OpenFlags::EXCL),
                                AccessCheck::skip(),
                            )?
                        }
                        f => f?,
                    };
                    Ok(StarnixNodeConnection::new(&current_task.kernel(), file, credentials))
                }
            })?;

            return starnix_file.directory_entry_open(
                scope,
                flags,
                path::Path::dot(),
                object_request,
            );
        }

        // Reopen the current file.
        if !path.is_dot() {
            return Err(zx::Status::NOT_DIR);
        }
        let file = self.reopen(&flags)?;
        object_request
            .take()
            .create_connection_sync::<file::RawIoConnection<_>, _>(scope, file, flags);
        Ok(())
    }

    fn get_attributes(
        &self,
        requested_attributes: fio::NodeAttributesQuery,
    ) -> fio::NodeAttributes2 {
        self.spawn_task(async move |_, _, file| {
            let info = file.node().info();

            // This cast is necessary depending on the architecture.
            #[allow(clippy::unnecessary_cast)]
            let link_count = info.link_count as u64;

            let (protocols, abilities) = if info.mode.contains(FileMode::IFDIR) {
                (
                    fio::NodeProtocolKinds::DIRECTORY,
                    fio::Operations::GET_ATTRIBUTES
                        | fio::Operations::UPDATE_ATTRIBUTES
                        | fio::Operations::ENUMERATE
                        | fio::Operations::TRAVERSE
                        | fio::Operations::MODIFY_DIRECTORY,
                )
            } else {
                (
                    fio::NodeProtocolKinds::FILE,
                    fio::Operations::GET_ATTRIBUTES
                        | fio::Operations::UPDATE_ATTRIBUTES
                        | fio::Operations::READ_BYTES
                        | fio::Operations::WRITE_BYTES,
                )
            };

            Ok(attributes!(
                requested_attributes,
                Mutable {
                    creation_time: info.time_status_change.into_nanos() as u64,
                    modification_time: info.time_modify.into_nanos() as u64,
                    mode: info.mode.bits(),
                    uid: info.uid,
                    gid: info.gid,
                    rdev: info.rdev.bits(),
                },
                Immutable {
                    protocols: protocols,
                    abilities: abilities,
                    content_size: info.size as u64,
                    storage_size: info.storage_size() as u64,
                    link_count: link_count,
                    id: file.fs.dev_id.bits(),
                }
            ))
        })
        .expect("spawn_task")
    }

    fn update_attributes(&self, attributes: fio::MutableNodeAttributes) {
        let _ = self.spawn_task(async move |_, _, file| {
            file.node().update_info(|info| {
                if let Some(time) = attributes.creation_time {
                    info.time_status_change = UtcInstant::from_nanos(time as i64);
                }
                if let Some(time) = attributes.modification_time {
                    info.time_modify = UtcInstant::from_nanos(time as i64);
                }
                if let Some(mode) = attributes.mode {
                    info.mode = FileMode::from_bits(mode);
                }
                if let Some(uid) = attributes.uid {
                    info.uid = uid;
                }
                if let Some(gid) = attributes.gid {
                    info.gid = gid;
                }
                if let Some(rdev) = attributes.rdev {
                    info.rdev = DeviceType::from_bits(rdev);
                }
            });
            Ok(())
        });
    }
}

impl vfs::node::Node for StarnixNodeConnection {
    async fn get_attributes(
        &self,
        requested_attributes: fio::NodeAttributesQuery,
    ) -> Result<fio::NodeAttributes2, zx::Status> {
        Ok(StarnixNodeConnection::get_attributes(self, requested_attributes))
    }
}

impl directory::entry::GetEntryInfo for StarnixNodeConnection {
    fn entry_info(&self) -> directory::entry::EntryInfo {
        let dirent_type =
            if self.is_dir() { fio::DirentType::Directory } else { fio::DirentType::File };
        directory::entry::EntryInfo::new(0, dirent_type)
    }
}

impl directory::entry_container::Directory for StarnixNodeConnection {
    fn open(
        self: Arc<Self>,
        scope: execution_scope::ExecutionScope,
        path: path::Path,
        flags: fio::Flags,
        object_request: ObjectRequestRef<'_>,
    ) -> Result<(), zx::Status> {
        self.directory_entry_open(scope, flags, path, object_request)
    }

    async fn read_dirents(
        &self,
        pos: &directory::traversal_position::TraversalPosition,
        sink: Box<dyn directory::dirents_sink::Sink>,
    ) -> Result<
        (
            directory::traversal_position::TraversalPosition,
            Box<dyn directory::dirents_sink::Sealed>,
        ),
        zx::Status,
    > {
        StarnixNodeConnection::directory_read_dirents(self, pos, sink).map_err(Errno::into)
    }
    fn register_watcher(
        self: Arc<Self>,
        _scope: execution_scope::ExecutionScope,
        _mask: fio::WatchMask,
        _watcher: directory::entry_container::DirectoryWatcher,
    ) -> Result<(), zx::Status> {
        track_stub!(TODO("https://fxbug.dev/322875605"), "register directory watcher");
        Ok(())
    }
    fn unregister_watcher(self: Arc<Self>, _key: usize) {}
}

impl directory::entry_container::MutableDirectory for StarnixNodeConnection {
    async fn update_attributes(
        &self,
        attributes: fio::MutableNodeAttributes,
    ) -> Result<(), zx::Status> {
        StarnixNodeConnection::update_attributes(self, attributes);
        Ok(())
    }
    async fn unlink(
        self: Arc<Self>,
        name: &str,
        must_be_directory: bool,
    ) -> Result<(), zx::Status> {
        let name = FsString::from(name.to_owned());
        self.spawn_task_async(async move |locked, current_task, file| {
            let kind =
                if must_be_directory { UnlinkKind::Directory } else { UnlinkKind::NonDirectory };
            file.name.entry.unlink(
                locked,
                current_task,
                &file.name.mount,
                name.as_ref(),
                kind,
                false,
            )
        })
        .await?;
        Ok(())
    }
    async fn sync(&self) -> Result<(), zx::Status> {
        Ok(())
    }
    fn rename(
        self: Arc<Self>,
        src_dir: Arc<dyn directory::entry_container::MutableDirectory>,
        src_name: path::Path,
        dst_name: path::Path,
    ) -> BoxFuture<'static, Result<(), zx::Status>> {
        let this = self.clone();
        Box::pin(async move {
            Ok(self
                .spawn_task_async(async move |locked, current_task, file| {
                    let src_dir = src_dir
                        .into_any()
                        .downcast::<StarnixNodeConnection>()
                        .map_err(|_| errno!(EXDEV))?;
                    let (dst_node, dst_name) =
                        lookup_parent(locked, current_task, &file, dst_name)?;
                    let (src_node, src_name) = if Arc::ptr_eq(&src_dir, &this) {
                        lookup_parent(locked, current_task, &file, src_name)?
                    } else {
                        src_dir.lookup_parent(src_name)?
                    };
                    NamespaceNode::rename(
                        locked,
                        current_task,
                        &src_node,
                        src_name.as_ref(),
                        &dst_node,
                        dst_name.as_ref(),
                        RenameFlags::empty(),
                    )
                })
                .await?)
        })
    }
}

impl file::File for StarnixNodeConnection {
    fn writable(&self) -> bool {
        true
    }
    async fn open_file(&self, _optionss: &file::FileOptions) -> Result<(), zx::Status> {
        Ok(())
    }
    async fn truncate(&self, length: u64) -> Result<(), zx::Status> {
        Ok(self
            .spawn_task_async(async move |locked, current_task, file| {
                file.name.truncate(locked, current_task, length)
            })
            .await?)
    }
    async fn get_backing_memory(&self, flags: fio::VmoFlags) -> Result<zx::Vmo, zx::Status> {
        Ok(self
            .spawn_task_async(async move |locked, current_task, file| {
                (|| {
                    let mut prot_flags = ProtectionFlags::empty();
                    if flags.contains(fio::VmoFlags::READ) {
                        prot_flags |= ProtectionFlags::READ;
                    }
                    if flags.contains(fio::VmoFlags::WRITE) {
                        prot_flags |= ProtectionFlags::WRITE;
                    }
                    if flags.contains(fio::VmoFlags::EXECUTE) {
                        prot_flags |= ProtectionFlags::EXEC;
                    }
                    let memory = file.get_memory(locked, current_task, None, prot_flags)?;
                    let vmo = memory.as_vmo().ok_or(zx::Status::NOT_SUPPORTED)?;
                    if flags.contains(fio::VmoFlags::PRIVATE_CLONE) {
                        let size = vmo.get_size()?;
                        vmo.create_child(zx::VmoChildOptions::SNAPSHOT_AT_LEAST_ON_WRITE, 0, size)
                    } else {
                        vmo.duplicate_handle(zx::Rights::SAME_RIGHTS)
                    }
                })()
                .map_err(|e| from_status_like_fdio!(e))
            })
            .await?)
    }

    async fn get_size(&self) -> Result<u64, zx::Status> {
        Ok(self
            .spawn_task_async(async move |_, _, file| Ok(file.node().info().size as u64))
            .await?)
    }
    async fn update_attributes(
        &self,
        attributes: fio::MutableNodeAttributes,
    ) -> Result<(), zx::Status> {
        StarnixNodeConnection::update_attributes(self, attributes);
        Ok(())
    }
    async fn sync(&self, _mode: file::SyncMode) -> Result<(), zx::Status> {
        Ok(())
    }
}

impl file::RawFileIoConnection for StarnixNodeConnection {
    async fn read(&self, count: u64) -> Result<Vec<u8>, zx::Status> {
        Ok(self
            .spawn_task_async(async move |locked, current_task, file| {
                let mut data = VecOutputBuffer::new(count as usize);
                file.read(locked, current_task, &mut data)?;
                Ok(data.into())
            })
            .await?)
    }

    async fn read_at(&self, offset: u64, count: u64) -> Result<Vec<u8>, zx::Status> {
        Ok(self
            .spawn_task_async(async move |locked, current_task, file| -> Result<Vec<u8>, Errno> {
                let mut data = VecOutputBuffer::new(count as usize);
                file.read_at(locked, current_task, offset as usize, &mut data)?;
                Ok(data.into())
            })
            .await?)
    }

    async fn write(&self, content: &[u8]) -> Result<u64, zx::Status> {
        let mut data = VecInputBuffer::new(content);
        Ok(self
            .spawn_task_async(async move |locked, current_task, file| {
                let written = file.write(locked, current_task, &mut data)?;
                Ok(written as u64)
            })
            .await?)
    }

    async fn write_at(&self, offset: u64, content: &[u8]) -> Result<u64, zx::Status> {
        let mut data = VecInputBuffer::new(content);
        Ok(self
            .spawn_task_async(async move |locked, current_task, file| {
                let written = file.write_at(locked, current_task, offset as usize, &mut data)?;
                Ok(written as u64)
            })
            .await?)
    }

    async fn seek(&self, offset: i64, origin: fio::SeekOrigin) -> Result<u64, zx::Status> {
        let target = match origin {
            fio::SeekOrigin::Start => SeekTarget::Set(offset),
            fio::SeekOrigin::Current => SeekTarget::Cur(offset),
            fio::SeekOrigin::End => SeekTarget::End(offset),
        };
        Ok(self.spawn_task(async move |locked, current_task, file| {
            let seek_result = file.seek(locked, current_task, target)?;
            Ok(seek_result as u64)
        })?)
    }

    fn set_flags(&self, flags: fio::Flags) -> Result<(), zx::Status> {
        // Called on the connection via `fcntl(FSETFL, ...)`. fuchsia.io only supports `O_APPEND`
        // right now, and does not have equivalents for the following flags:
        //  - `O_ASYNC`
        //  - `O_DIRECT`
        //  - `O_NOATIME` (only allowed if caller's EUID is same as the file's UID)
        //  - `O_NONBLOCK`
        const SETTABLE_FLAGS_MASK: OpenFlags = OpenFlags::APPEND;
        let flags = if flags.contains(fio::Flags::FILE_APPEND) {
            OpenFlags::APPEND
        } else {
            OpenFlags::empty()
        };
        Ok(self.spawn_task(async move |_, _, file| {
            file.update_file_flags(flags, SETTABLE_FLAGS_MASK);
            Ok(())
        })?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::tmpfs::TmpFs;
    use crate::testing::*;
    use crate::vfs::{FsString, Namespace};
    use starnix_uapi::auth::Capabilities;
    use std::collections::HashSet;
    use syncio::{Zxio, ZxioOpenOptions, zxio_node_attr_has_t};

    fn assert_directory_content(zxio: &Zxio, content: &[&[u8]]) {
        let expected = content.iter().map(|&x| FsString::from(x)).collect::<HashSet<_>>();
        let mut iterator = zxio.create_dirent_iterator().expect("iterator");
        iterator.rewind().expect("iterator");
        let found =
            iterator.map(|x| x.as_ref().expect("dirent").name.clone()).collect::<HashSet<_>>();
        assert_eq!(found, expected);
    }

    #[::fuchsia::test]
    async fn access_file_system() {
        spawn_kernel_and_run(async |locked, current_task| {
            let kernel = current_task.kernel();
            let fs = TmpFs::new_fs(locked, &kernel);

            let file =
                &fs.root().open_anonymous(locked, current_task, OpenFlags::RDWR).expect("open");
            let (root_handle, scope) =
                serve_file(current_task, file, FullCredentials::for_kernel()).expect("serve");

            // Capture information from the filesystem in the main thread. The filesystem must not be
            // transferred to the other thread.
            let fs_dev_id = fs.dev_id;
            std::thread::spawn(move || {
                let root_zxio = Zxio::create(root_handle.into_handle()).expect("create");

                assert_directory_content(&root_zxio, &[b"."]);
                // Check that one can reiterate from the start.
                assert_directory_content(&root_zxio, &[b"."]);

                let attrs = root_zxio
                    .attr_get(zxio_node_attr_has_t { id: true, ..Default::default() })
                    .expect("attr_get");
                assert_eq!(attrs.id, fs_dev_id.bits());

                let mut attrs = syncio::zxio_node_attributes_t::default();
                attrs.has.creation_time = true;
                attrs.has.modification_time = true;
                attrs.creation_time = 0;
                attrs.modification_time = 42;
                root_zxio.attr_set(&attrs).expect("attr_set");
                let attrs = root_zxio
                    .attr_get(zxio_node_attr_has_t {
                        creation_time: true,
                        modification_time: true,
                        ..Default::default()
                    })
                    .expect("attr_get");
                assert_eq!(attrs.creation_time, 0);
                assert_eq!(attrs.modification_time, 42);

                assert_eq!(
                    root_zxio
                        .open("foo", fio::PERM_READABLE | fio::PERM_WRITABLE, Default::default())
                        .expect_err("open"),
                    zx::Status::NOT_FOUND
                );
                let foo_zxio = root_zxio
                    .open(
                        "foo",
                        fio::PERM_READABLE
                            | fio::PERM_WRITABLE
                            | fio::Flags::FLAG_MAYBE_CREATE
                            | fio::Flags::PROTOCOL_FILE,
                        Default::default(),
                    )
                    .expect("zxio_open");
                assert_directory_content(&root_zxio, &[b".", b"foo"]);

                assert_eq!(foo_zxio.write(b"hello").expect("write"), 5);
                assert_eq!(foo_zxio.write_at(2, b"ch").expect("write_at"), 2);
                let mut buffer = [0; 7];
                assert_eq!(foo_zxio.read_at(2, &mut buffer).expect("read_at"), 3);
                assert_eq!(&buffer[..3], b"cho");
                assert_eq!(foo_zxio.seek(syncio::SeekOrigin::Start, 0).expect("seek"), 0);
                assert_eq!(foo_zxio.read(&mut buffer).expect("read"), 5);
                assert_eq!(&buffer[..5], b"hecho");

                let attrs = foo_zxio
                    .attr_get(zxio_node_attr_has_t { id: true, ..Default::default() })
                    .expect("attr_get");
                assert_eq!(attrs.id, fs_dev_id.bits());

                let mut attrs = syncio::zxio_node_attributes_t::default();
                attrs.has.creation_time = true;
                attrs.has.modification_time = true;
                attrs.creation_time = 0;
                attrs.modification_time = 42;
                foo_zxio.attr_set(&attrs).expect("attr_set");
                let attrs = foo_zxio
                    .attr_get(zxio_node_attr_has_t {
                        creation_time: true,
                        modification_time: true,
                        ..Default::default()
                    })
                    .expect("attr_get");
                assert_eq!(attrs.creation_time, 0);
                assert_eq!(attrs.modification_time, 42);

                assert_eq!(
                    root_zxio
                        .open(
                            "bar/baz",
                            fio::Flags::PROTOCOL_DIRECTORY
                                | fio::Flags::FLAG_MAYBE_CREATE
                                | fio::PERM_READABLE
                                | fio::PERM_WRITABLE,
                            Default::default(),
                        )
                        .expect_err("open"),
                    zx::Status::NOT_FOUND
                );

                let bar_zxio = root_zxio
                    .open(
                        "bar",
                        fio::Flags::PROTOCOL_DIRECTORY
                            | fio::Flags::FLAG_MAYBE_CREATE
                            | fio::PERM_READABLE
                            | fio::PERM_WRITABLE,
                        Default::default(),
                    )
                    .expect("open");
                let baz_zxio = root_zxio
                    .open(
                        "bar/baz",
                        fio::Flags::PROTOCOL_DIRECTORY
                            | fio::Flags::FLAG_MAYBE_CREATE
                            | fio::PERM_READABLE
                            | fio::PERM_WRITABLE,
                        Default::default(),
                    )
                    .expect("open");
                assert_directory_content(&root_zxio, &[b".", b"foo", b"bar"]);
                assert_directory_content(&bar_zxio, &[b".", b"baz"]);

                bar_zxio.rename("baz", &root_zxio, "quz").expect("rename");
                assert_directory_content(&bar_zxio, &[b"."]);
                assert_directory_content(&root_zxio, &[b".", b"foo", b"bar", b"quz"]);
                assert_directory_content(&baz_zxio, &[b"."]);
            })
            .join()
            .expect("join");
            scope.shutdown();
            scope.wait().await;
            // This ensures fs cannot be captures in the thread.
            std::mem::drop(fs);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn open() {
        spawn_kernel_and_run(async |locked, current_task| {
            let kernel = current_task.kernel();
            let fs = TmpFs::new_fs(locked, &kernel);

            let file = &fs
                .root()
                .open_anonymous(locked, current_task, OpenFlags::RDWR)
                .expect("open_anonymous failed");
            let (root_handle, scope) =
                serve_file(current_task, file, FullCredentials::for_kernel())
                    .expect("serve_file failed");

            std::thread::spawn(move || {
                let root_zxio =
                    Zxio::create(root_handle.into_handle()).expect("zxio create failed");

                assert_directory_content(&root_zxio, &[b"."]);
                assert_eq!(
                    root_zxio
                        .open(
                            "foo",
                            fio::PERM_READABLE | fio::PERM_WRITABLE,
                            ZxioOpenOptions::default()
                        )
                        .expect_err("open3 passed unexpectedly"),
                    zx::Status::NOT_FOUND
                );
                root_zxio
                    .open(
                        "foo",
                        fio::Flags::PROTOCOL_FILE
                            | fio::PERM_READABLE
                            | fio::PERM_WRITABLE
                            | fio::Flags::FLAG_MUST_CREATE,
                        ZxioOpenOptions::default(),
                    )
                    .expect("open3 failed");
                assert_directory_content(&root_zxio, &[b".", b"foo"]);

                assert_eq!(
                    root_zxio
                        .open(
                            "bar/baz",
                            fio::Flags::PROTOCOL_DIRECTORY
                                | fio::PERM_READABLE
                                | fio::PERM_WRITABLE
                                | fio::Flags::FLAG_MUST_CREATE,
                            ZxioOpenOptions::default()
                        )
                        .expect_err("open3 passed unexpectedly"),
                    zx::Status::NOT_FOUND
                );
                let bar_zxio = root_zxio
                    .open(
                        "bar",
                        fio::Flags::PROTOCOL_DIRECTORY
                            | fio::PERM_READABLE
                            | fio::PERM_WRITABLE
                            | fio::Flags::FLAG_MUST_CREATE,
                        ZxioOpenOptions::default(),
                    )
                    .expect("open3 failed");
                root_zxio
                    .open(
                        "bar/baz",
                        fio::Flags::PROTOCOL_DIRECTORY
                            | fio::PERM_READABLE
                            | fio::PERM_WRITABLE
                            | fio::Flags::FLAG_MUST_CREATE,
                        ZxioOpenOptions::default(),
                    )
                    .expect("open3 failed");
                assert_directory_content(&root_zxio, &[b".", b"foo", b"bar"]);
                assert_directory_content(&bar_zxio, &[b".", b"baz"]);
            })
            .join()
            .expect("join");
            scope.shutdown();
            scope.wait().await;

            // This ensures fs cannot be captured in the thread.
            std::mem::drop(fs);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn use_credentials() {
        spawn_kernel_and_run(async |locked, current_task| {
            let kernel = current_task.kernel();
            let fs = TmpFs::new_fs(locked, &kernel);

            let file = &fs
                .root()
                .open_anonymous(locked, current_task, OpenFlags::RDWR)
                .expect("open_anonymous failed");
            // Create a file as root.
            let ns = Namespace::new(fs);
            ns.root()
                .open_create_node(
                    locked,
                    current_task,
                    "test".into(),
                    FileMode::from_bits(0o600) | FileMode::IFREG,
                    DeviceType::NONE,
                    OpenFlags::empty(),
                )
                .expect("open_create_node failed");

            let mut user_credentials = FullCredentials::for_kernel();
            user_credentials.creds.fsuid = 1;
            user_credentials.creds.cap_effective = Capabilities::empty();

            let (root_handle, scope) =
                serve_file(current_task, file, user_credentials).expect("serve_file failed");

            std::thread::spawn(move || {
                let root_zxio =
                    Zxio::create(root_handle.into_handle()).expect("zxio create failed");

                assert_directory_content(&root_zxio, &[b".", b"test"]);
                assert_eq!(
                    root_zxio
                        .open(
                            "test",
                            fio::PERM_READABLE | fio::PERM_WRITABLE,
                            ZxioOpenOptions::default()
                        )
                        .expect_err("open3 passed unexpectedly"),
                    zx::Status::ACCESS_DENIED
                );
            })
            .join()
            .expect("join");
            scope.shutdown();
            scope.wait().await;

            // This ensures fs cannot be captured in the thread.
            std::mem::drop(ns);
        })
        .await;
    }
}
