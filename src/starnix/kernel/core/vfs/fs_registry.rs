// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::security;
use crate::task::CurrentTask;
use crate::vfs::{FileSystemHandle, FileSystemOptions, FsStr, FsString};
use starnix_sync::{FsRegistryLock, LockDepMutex};
use starnix_uapi::errors::Errno;
use std::collections::BTreeMap;
use std::sync::Arc;

type CreateFs = Arc<
    dyn Fn(&CurrentTask, FileSystemOptions) -> Result<FileSystemHandle, Errno>
        + Send
        + Sync
        + 'static,
>;

#[derive(Default)]
pub struct FsRegistry {
    registry: LockDepMutex<BTreeMap<FsString, CreateFs>, FsRegistryLock>,
}

impl FsRegistry {
    pub fn register<F>(&self, fs_type: &FsStr, create_fs: F)
    where
        F: Fn(&CurrentTask, FileSystemOptions) -> Result<FileSystemHandle, Errno>
            + Send
            + Sync
            + 'static,
    {
        let existing = self.registry.lock().insert(fs_type.into(), Arc::new(create_fs));
        assert!(existing.is_none());
    }

    pub fn create(
        &self,
        current_task: &CurrentTask,
        fs_type: &FsStr,
        options: FileSystemOptions,
    ) -> Option<Result<FileSystemHandle, Errno>> {
        let create_fs = self.registry.lock().get(fs_type).map(Arc::clone)?;
        Some(create_fs(current_task, options).and_then(|fs| {
            assert_eq!(fs_type, fs.name(), "FileSystem::name() must match the registered name.");
            security::file_system_resolve_security(&current_task, &fs)?;
            Ok(fs)
        }))
    }

    pub fn list_all(&self) -> Vec<FsString> {
        self.registry.lock().keys().cloned().collect()
    }
}
