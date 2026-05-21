// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use erofs::FileNode;
use fidl_fuchsia_io as fio;
use std::sync::Arc;
use vfs::ObjectRequestRef;
use vfs::directory::entry::{DirectoryEntry, EntryInfo, GetEntryInfo, OpenRequest};
use vfs::execution_scope::ExecutionScope;
use vfs::file::{FidlIoConnection, File, FileIo, FileLike, FileOptions, SyncMode};
use vfs::node::Node;

/// A stub implementation of an EROFS file.
pub struct ErofsFile {
    _parser: Arc<erofs::ErofsParser>,
    node: FileNode,
}

impl ErofsFile {
    pub fn new(parser: Arc<erofs::ErofsParser>, node: FileNode) -> Self {
        Self { _parser: parser, node }
    }
}

impl DirectoryEntry for ErofsFile {
    fn open_entry(self: Arc<Self>, request: OpenRequest<'_>) -> Result<(), zx::Status> {
        request.open_file(self)
    }
}

impl GetEntryInfo for ErofsFile {
    fn entry_info(&self) -> EntryInfo {
        EntryInfo::new(self.node.ino() as u64, fio::DirentType::File)
    }
}

impl Node for ErofsFile {
    async fn get_attributes(
        &self,
        requested_attributes: fio::NodeAttributesQuery,
    ) -> Result<fio::NodeAttributes2, zx::Status> {
        let content_size = self.node.size();
        Ok(vfs::immutable_attributes!(
            requested_attributes,
            Immutable {
                protocols: fio::NodeProtocolKinds::FILE,
                abilities: fio::Operations::GET_ATTRIBUTES | fio::Operations::READ_BYTES,
                content_size: content_size,
                storage_size: content_size,
                id: self.node.ino() as u64,
            }
        ))
    }
}

impl FileIo for ErofsFile {
    async fn read_at(&self, _offset: u64, _buffer: &mut [u8]) -> Result<u64, zx::Status> {
        Err(zx::Status::NOT_SUPPORTED)
    }

    async fn write_at(&self, _offset: u64, _content: &[u8]) -> Result<u64, zx::Status> {
        Err(zx::Status::NOT_SUPPORTED)
    }

    async fn append(&self, _content: &[u8]) -> Result<(u64, u64), zx::Status> {
        Err(zx::Status::NOT_SUPPORTED)
    }
}

impl File for ErofsFile {
    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        false
    }

    fn executable(&self) -> bool {
        false
    }

    async fn open_file(&self, _options: &FileOptions) -> Result<(), zx::Status> {
        Ok(())
    }

    async fn truncate(&self, _length: u64) -> Result<(), zx::Status> {
        Err(zx::Status::NOT_SUPPORTED)
    }

    async fn get_size(&self) -> Result<u64, zx::Status> {
        Ok(self.node.size())
    }

    async fn update_attributes(
        &self,
        _attributes: fio::MutableNodeAttributes,
    ) -> Result<(), zx::Status> {
        Err(zx::Status::NOT_SUPPORTED)
    }

    async fn sync(&self, _mode: SyncMode) -> Result<(), zx::Status> {
        Ok(())
    }
}

impl FileLike for ErofsFile {
    fn open(
        self: Arc<Self>,
        scope: ExecutionScope,
        options: FileOptions,
        object_request: ObjectRequestRef<'_>,
    ) -> Result<(), zx::Status> {
        FidlIoConnection::create_sync(scope, self, options, object_request.take());
        Ok(())
    }
}
