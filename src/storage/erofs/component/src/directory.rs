// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::file::ErofsFile;
use crate::volume::ErofsVolume;
use erofs::{DirectoryNode, FileType, Node};
use fidl_fuchsia_io as fio;
use fuchsia_sync::Mutex;
use std::sync::Arc;
use vfs::directory::dirents_sink::{self, AppendResult};
use vfs::directory::entry::{DirectoryEntry, EntryInfo, GetEntryInfo, OpenRequest};
use vfs::directory::entry_container::{Directory, DirectoryWatcher};
use vfs::directory::immutable::connection::ImmutableConnection;
use vfs::directory::traversal_position::TraversalPosition;
use vfs::directory::watchers::Watchers;
use vfs::directory::watchers::event_producers::{SingleNameEventProducer, StaticVecEventProducer};
use vfs::execution_scope::ExecutionScope;
use vfs::path::Path;
use vfs::{CreationMode, ObjectRequestRef, ProtocolsExt};

/// A directory in the EROFS filesystem.
pub struct ErofsDirectory {
    volume: Arc<ErofsVolume>,
    node: DirectoryNode,
    watchers: Mutex<Watchers>,
}

fn check_open_flags(flags: fio::Flags, exists: bool) -> Result<(), zx::Status> {
    match flags.creation_mode() {
        CreationMode::Never => Ok(()),
        CreationMode::Always => {
            if exists {
                Err(zx::Status::ALREADY_EXISTS)
            } else {
                Err(zx::Status::NOT_SUPPORTED)
            }
        }
        CreationMode::AllowExisting => {
            if exists {
                Ok(())
            } else {
                Err(zx::Status::NOT_SUPPORTED)
            }
        }
        CreationMode::UnnamedTemporary | CreationMode::UnlinkableUnnamedTemporary => {
            Err(zx::Status::NOT_SUPPORTED)
        }
    }
}

impl ErofsDirectory {
    pub fn new(volume: Arc<ErofsVolume>, node: DirectoryNode) -> Self {
        Self { volume, node, watchers: Mutex::new(Watchers::new()) }
    }
}

impl DirectoryEntry for ErofsDirectory {
    fn open_entry(self: Arc<Self>, request: OpenRequest<'_>) -> Result<(), zx::Status> {
        request.open_dir(self)
    }
}

impl GetEntryInfo for ErofsDirectory {
    fn entry_info(&self) -> EntryInfo {
        EntryInfo::new(self.node.ino() as u64, fio::DirentType::Directory)
    }
}

impl vfs::node::Node for ErofsDirectory {
    async fn get_attributes(
        &self,
        requested_attributes: fio::NodeAttributesQuery,
    ) -> Result<fio::NodeAttributes2, zx::Status> {
        Ok(vfs::immutable_attributes!(
            requested_attributes,
            Immutable {
                protocols: fio::NodeProtocolKinds::DIRECTORY,
                abilities: fio::Operations::GET_ATTRIBUTES
                    | fio::Operations::ENUMERATE
                    | fio::Operations::TRAVERSE,
                id: self.node.ino() as u64,
            }
        ))
    }
}

impl Directory for ErofsDirectory {
    fn open(
        self: Arc<Self>,
        scope: ExecutionScope,
        mut path: Path,
        flags: fio::Flags,
        object_request: ObjectRequestRef<'_>,
    ) -> Result<(), zx::Status> {
        let (name, _) = match path.next_with_ref() {
            (path_ref, Some(name)) => (name, path_ref),
            (_, None) => {
                // We are opening this directory itself.
                check_open_flags(flags, true)?;
                object_request
                    .take()
                    .create_connection_sync::<ImmutableConnection<_>, _>(scope, self, flags);
                return Ok(());
            }
        };

        // Lookup the child in the EROFS image.
        let child_node_opt = self.volume.parser().lookup(&self.node, name).map_err(|e| {
            log::error!("Lookup failed for '{}': {:?}", name, e);
            e.to_status()
        })?;

        let child_node = match child_node_opt {
            Some(node) => node,
            None => {
                if path.is_empty() {
                    check_open_flags(flags, false)?;
                }
                return Err(zx::Status::NOT_FOUND);
            }
        };

        if path.is_empty() {
            check_open_flags(flags, true)?;
        }

        // Delegate the remaining path traversal to the child.
        match child_node {
            Node::Directory(dir_node) => {
                let child_dir = Arc::new(ErofsDirectory::new(self.volume.clone(), dir_node));
                child_dir.open(scope, path, flags, object_request)
            }
            Node::File(file_node) => {
                if !path.is_empty() {
                    return Err(zx::Status::NOT_DIR);
                }
                let child_file = ErofsFile::new(self.volume.clone(), file_node)?;
                vfs::file::serve(child_file, scope, &flags, object_request)
            }
        }
    }

    async fn read_dirents(
        &self,
        pos: &TraversalPosition,
        sink: Box<dyn dirents_sink::Sink>,
    ) -> Result<(TraversalPosition, Box<dyn dirents_sink::Sealed>), zx::Status> {
        let mut entry_offset = match pos {
            TraversalPosition::Start => 0,
            TraversalPosition::Index(index) => *index,
            TraversalPosition::End => return Ok((TraversalPosition::End, sink.seal())),
            _ => return Err(zx::Status::NOT_SUPPORTED),
        };

        let mut sink = sink;
        let mut buffer = vec![erofs::DirectoryEntry::default(); 16];

        loop {
            let filled = self
                .volume
                .parser()
                .read_directory(&self.node, entry_offset as usize, &mut buffer)
                .map_err(|e| {
                    log::error!("Read directory failed at offset {}: {:?}", entry_offset, e);
                    e.to_status()
                })?;

            for i in 0..filled {
                let entry = &buffer[i];
                if entry.name == ".." {
                    entry_offset += 1;
                    continue;
                }
                let dirent_type = match entry.file_type {
                    FileType::RegFile => fio::DirentType::File,
                    FileType::Dir => fio::DirentType::Directory,
                    FileType::Symlink => fio::DirentType::Symlink,
                    _ => fio::DirentType::Unknown,
                };

                // We have to go parse the child inode entry to find the ino.
                let child = self.volume.parser().node(entry.nid).map_err(|e| {
                    log::error!("Failed to lookup child node {} for ino: {:?}", entry.nid, e);
                    e.to_status()
                })?;
                let ino = child.ino();

                let entry_info = EntryInfo::new(ino as u64, dirent_type);
                match sink.append(&entry_info, &entry.name) {
                    AppendResult::Ok(new_sink) => {
                        sink = new_sink;
                        entry_offset += 1;
                    }
                    AppendResult::Sealed(sealed) => {
                        return Ok((TraversalPosition::Index(entry_offset), sealed));
                    }
                }
            }

            if filled < buffer.len() {
                break;
            }
        }

        Ok((TraversalPosition::End, sink.seal()))
    }

    fn register_watcher(
        self: Arc<Self>,
        scope: ExecutionScope,
        mask: fio::WatchMask,
        watcher: DirectoryWatcher,
    ) -> Result<(), zx::Status> {
        let mut entry_names = vec![];
        let mut entry_offset = 0;
        let mut buffer = vec![erofs::DirectoryEntry::default(); 16];
        loop {
            let filled = self
                .volume
                .parser()
                .read_directory(&self.node, entry_offset, &mut buffer)
                .map_err(|e| {
                    log::error!(
                        "Watcher read directory failed at offset {}: {:?}",
                        entry_offset,
                        e
                    );
                    e.to_status()
                })?;
            if filled == 0 {
                break;
            }
            for i in 0..filled {
                let name = &buffer[i].name;
                if name != ".." {
                    entry_names.push(name.clone());
                }
            }
            entry_offset += filled;
        }

        let mut names = StaticVecEventProducer::existing(entry_names);

        let mut watchers = self.watchers.lock();
        let controller = watchers.add(scope, self.clone(), mask, watcher);
        controller.send_event(&mut names);
        controller.send_event(&mut SingleNameEventProducer::idle());

        Ok(())
    }

    fn unregister_watcher(self: Arc<Self>, key: usize) {
        self.watchers.lock().remove(key);
    }
}
