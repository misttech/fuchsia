// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::gpt::GptManager;
use block_server::{BlockServer, SessionManager};
use fuchsia_sync::Mutex;
use std::collections::BTreeMap;
use std::sync::{Arc, Weak};
use vfs::directory::helper::DirectlyMutable as _;

/// A directory of instances of the fuchsia.storage.partitions.PartitionService service.
pub struct PartitionsDirectory {
    node: Arc<vfs::directory::immutable::Simple>,
    entries: Mutex<BTreeMap<String, PartitionsDirectoryEntry>>,
}

impl std::fmt::Debug for PartitionsDirectory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        f.debug_struct("PartitionDirectory").field("entries", &self.entries).finish()
    }
}

impl PartitionsDirectory {
    pub fn new(node: Arc<vfs::directory::immutable::Simple>) -> Self {
        Self { node, entries: Default::default() }
    }

    pub fn clear(&self) {
        self.node.remove_all_entries();
        self.entries.lock().clear();
    }

    /// Adds an entry for a GPT partition.  Serves the "volume" and "partition" protocols.
    pub fn add_partition<SM: SessionManager + Send + Sync + 'static>(
        &self,
        name: &str,
        block_server: Weak<BlockServer<SM>>,
        gpt_manager: Weak<GptManager>,
        gpt_index: usize,
    ) {
        let entry = PartitionsDirectoryEntry::new_partition(block_server, gpt_manager, gpt_index);
        self.node.add_entry(name, entry.node.clone()).expect("Added an entry twice");
        self.entries.lock().insert(name.to_string(), entry);
    }

    /// Adds an entry for an overlay partition.  Serves the "volume" and "overlay" protocols.
    pub fn add_overlay<SM: SessionManager + Send + Sync + 'static>(
        &self,
        name: &str,
        block_server: Weak<BlockServer<SM>>,
        gpt_manager: Weak<GptManager>,
        gpt_indexes: Vec<usize>,
    ) {
        let entry = PartitionsDirectoryEntry::new_overlay(block_server, gpt_manager, gpt_indexes);
        self.node.add_entry(name, entry.node.clone()).expect("Added an entry twice");
        self.entries.lock().insert(name.to_string(), entry);
    }
}

/// A node which hosts an instance of fuchsia.storage.partitions.PartitionService.
pub struct PartitionsDirectoryEntry {
    node: Arc<vfs::directory::immutable::Simple>,
}

impl std::fmt::Debug for PartitionsDirectoryEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        f.debug_struct("PartitionDirectoryEntry").finish()
    }
}

impl PartitionsDirectoryEntry {
    fn new_partition<SM: SessionManager + Send + Sync + 'static>(
        block_server: Weak<BlockServer<SM>>,
        gpt_manager: Weak<GptManager>,
        gpt_index: usize,
    ) -> Self {
        let node = vfs::directory::immutable::simple();
        node.add_entry(
            "volume",
            vfs::service::host(move |requests| {
                let server = block_server.clone();
                async move {
                    if let Some(server) = server.upgrade() {
                        if let Err(err) = server.handle_requests(requests).await {
                            log::error!(err:?; "Error handling requests");
                        }
                    }
                }
            }),
        )
        .unwrap();
        node.add_entry(
            "partition",
            vfs::service::host(move |requests| {
                let manager = gpt_manager.clone();
                async move {
                    if let Some(manager) = manager.upgrade() {
                        if let Err(err) =
                            manager.handle_partitions_requests(gpt_index, requests).await
                        {
                            log::error!(err:?; "Error handling requests");
                        }
                    }
                }
            }),
        )
        .unwrap();

        Self { node }
    }

    fn new_overlay<SM: SessionManager + Send + Sync + 'static>(
        block_server: Weak<BlockServer<SM>>,
        gpt_manager: Weak<GptManager>,
        gpt_indexes: Vec<usize>,
    ) -> Self {
        let node = vfs::directory::immutable::simple();
        node.add_entry(
            "volume",
            vfs::service::host(move |requests| {
                let server = block_server.clone();
                async move {
                    if let Some(server) = server.upgrade() {
                        if let Err(err) = server.handle_requests(requests).await {
                            log::error!(err:?; "Error handling requests");
                        }
                    }
                }
            }),
        )
        .unwrap();
        node.add_entry(
            "overlay",
            vfs::service::host(move |requests| {
                let manager = gpt_manager.clone();
                let gpt_indexes = gpt_indexes.clone();
                async move {
                    if let Some(manager) = manager.upgrade() {
                        if let Err(err) =
                            manager.handle_overlay_partitions_requests(gpt_indexes, requests).await
                        {
                            log::error!(err:?; "Error handling requests");
                        }
                    }
                }
            }),
        )
        .unwrap();

        Self { node }
    }
}
