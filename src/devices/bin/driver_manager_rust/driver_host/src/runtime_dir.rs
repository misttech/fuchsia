// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use async_lock::OnceCell;
use std::sync::Arc;
use vfs::directory::entry::{DirectoryEntry, EntryInfo, GetEntryInfo, OpenRequest};
use vfs::directory::simple::Simple;
use vfs::execution_scope::ExecutionScope;
use vfs::file::{FidlIoConnection, File, FileIo, FileLike, FileOptions, SyncMode, read_only};
use vfs::node::Node;
use vfs::{ObjectRequestRef, immutable_attributes, pseudo_directory};
use zx::Status;
use {fidl_fuchsia_driver_host as fdh, fidl_fuchsia_io as fio};

pub struct ProcessInfo {
    pub job_koid: zx::Koid,
    pub process_koid: zx::Koid,
    pub main_thread_koid: zx::Koid,
}

pub struct CachedProcessInfo {
    cell: OnceCell<ProcessInfo>,
    driver_host: fdh::DriverHostProxy,
}

impl CachedProcessInfo {
    pub fn new(driver_host: fdh::DriverHostProxy) -> Self {
        Self { cell: OnceCell::new(), driver_host }
    }

    pub async fn get(&self) -> Result<&ProcessInfo, zx::Status> {
        self.cell
            .get_or_try_init(|| async {
                match self.driver_host.get_process_info().await {
                    Ok(Ok(info)) => Ok(ProcessInfo {
                        job_koid: zx::Koid::from_raw(info.0),
                        process_koid: zx::Koid::from_raw(info.1),
                        main_thread_koid: zx::Koid::from_raw(info.2),
                    }),
                    Ok(Err(e)) => Err(zx::Status::from_raw(e)),
                    _ => Err(zx::Status::INTERNAL),
                }
            })
            .await
    }
}

/// An implementation of `vfs::File` that reads its contents from the driver host's process info.
struct ElfFile {
    process_info: Arc<CachedProcessInfo>,
    info_extractor: fn(&ProcessInfo) -> String,
}

impl DirectoryEntry for ElfFile {
    fn open_entry(self: Arc<Self>, request: OpenRequest<'_>) -> Result<(), Status> {
        request.open_file(self)
    }
}

impl GetEntryInfo for ElfFile {
    fn entry_info(&self) -> EntryInfo {
        EntryInfo::new(fio::INO_UNKNOWN, fio::DirentType::File)
    }
}

impl Node for ElfFile {
    async fn get_attributes(
        &self,
        requested_attributes: fio::NodeAttributesQuery,
    ) -> Result<fio::NodeAttributes2, Status> {
        let info = self.process_info.get().await?;
        let content = (self.info_extractor)(info);
        let content_size = content.len() as u64;
        Ok(immutable_attributes!(
            requested_attributes,
            Immutable {
                protocols: fio::NodeProtocolKinds::FILE,
                abilities: fio::Operations::GET_ATTRIBUTES | fio::Operations::READ_BYTES,
                content_size: content_size,
                storage_size: content_size,
            }
        ))
    }
}

impl FileIo for ElfFile {
    async fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<u64, Status> {
        let info = self.process_info.get().await?;
        let content = (self.info_extractor)(info);
        let bytes = content.as_bytes();
        let content_size = bytes.len() as u64;

        if offset >= content_size {
            return Ok(0u64);
        }

        let start = offset as usize;
        let read_len = std::cmp::min(bytes.len() - start, buffer.len());
        buffer[..read_len].copy_from_slice(&bytes[start..][..read_len]);
        Ok(read_len as u64)
    }

    async fn write_at(&self, _offset: u64, _content: &[u8]) -> Result<u64, Status> {
        Err(Status::NOT_SUPPORTED)
    }

    async fn append(&self, _content: &[u8]) -> Result<(u64, u64), Status> {
        Err(Status::NOT_SUPPORTED)
    }
}

impl File for ElfFile {
    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        false
    }

    fn executable(&self) -> bool {
        false
    }

    async fn open_file(&self, _options: &FileOptions) -> Result<(), Status> {
        Ok(())
    }

    async fn truncate(&self, _length: u64) -> Result<(), Status> {
        Err(Status::NOT_SUPPORTED)
    }

    async fn get_size(&self) -> Result<u64, Status> {
        let info = self.process_info.get().await?;
        let content = (self.info_extractor)(info);
        Ok(content.len() as u64)
    }

    async fn update_attributes(
        &self,
        _attributes: fio::MutableNodeAttributes,
    ) -> Result<(), Status> {
        Err(Status::NOT_SUPPORTED)
    }

    async fn sync(&self, _mode: SyncMode) -> Result<(), Status> {
        Ok(())
    }
}

impl FileLike for ElfFile {
    fn open(
        self: Arc<Self>,
        scope: ExecutionScope,
        options: FileOptions,
        object_request: ObjectRequestRef<'_>,
    ) -> Result<(), Status> {
        FidlIoConnection::create_sync(scope, self, options, object_request.take());
        Ok(())
    }
}

/// Creates the runtime directory that is served to the driver host.
/// This directory contains information about the driver host process that can be used by debugging
/// tools like zxdb.
pub fn create_runtime_dir(process_info: Arc<CachedProcessInfo>) -> Arc<Simple> {
    let now = zx::MonotonicInstant::get().into_nanos().to_string();
    let process_start_time = read_only(now.into_bytes());

    let job_id = Arc::new(ElfFile {
        process_info: process_info.clone(),
        info_extractor: |info| info.job_koid.raw_koid().to_string(),
    });

    let process_id = Arc::new(ElfFile {
        process_info,
        info_extractor: |info| info.process_koid.raw_koid().to_string(),
    });

    pseudo_directory! {
        "elf" => pseudo_directory! {
            "process_start_time" => process_start_time,
            "job_id" => job_id,
            "process_id" => process_id,
        }
    }
}
