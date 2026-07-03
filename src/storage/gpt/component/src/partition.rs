// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use crate::gpt::GptPartition;
use anyhow::Error;
use block_client::{ReadOptions, VmoId, WriteOptions};
use block_server::async_interface::{PassthroughSession, SessionManager};
use block_server::{DeviceInfo, OffsetMap};
use fidl_fuchsia_storage_block as fblock;
use fuchsia_async as fasync;

use fuchsia_sync::Mutex;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::num::NonZero;
use std::sync::{Arc, Weak};

/// A wrapper around a VmoId which keeps it active until all requests which use the Vmoid are
/// complete.  Strong references are held by ongoing requests.
pub struct VmoIdWrapper {
    partition: Weak<GptPartition>,
    vmo_id: VmoId,
}

impl std::ops::Deref for VmoIdWrapper {
    type Target = VmoId;
    fn deref(&self) -> &Self::Target {
        &self.vmo_id
    }
}

impl Drop for VmoIdWrapper {
    fn drop(&mut self) {
        // Turn it into an ID so that if the spawned task is dropped, the assertion in VmoId::drop
        // doesn't fire.  It will mean the ID is leaked, but it's most likely that the server is
        // being shut down anyway so it shouldn't matter.
        let vmo_id = self.vmo_id.take().into_id();
        if let Some(partition) = self.partition.upgrade() {
            fasync::Task::spawn(async move {
                if let Err(e) = partition.detach_vmo(VmoId::new(vmo_id)).await {
                    log::error!("detach_vmo failed: {:?}", e);
                }
            })
            .detach();
        }
    }
}

/// PartitionBackend is an implementation of block_server's Interface which is backed by a windowed
/// view of the underlying GPT device.
pub struct PartitionBackend {
    partition: Arc<GptPartition>,
    vmo_keys_to_vmoids_map: Mutex<BTreeMap<usize, Arc<VmoIdWrapper>>>,
    passthrough: bool,
}

impl block_server::async_interface::Interface for PartitionBackend {
    async fn open_session(
        &self,
        session_manager: Arc<SessionManager<Self>>,
        stream: fblock::SessionRequestStream,
        offset_map: OffsetMap,
        block_size: u32,
    ) -> Result<(), Error> {
        if !self.passthrough || !offset_map.is_empty() {
            // For now, we don't support double-passthrough.  We could as needed for nested GPT.
            // If we support this, we can remove I/O and vmoid management from this struct.
            return session_manager.serve_session(stream, offset_map, block_size).await;
        }
        let (proxy, server_end) = fidl::endpoints::create_proxy::<fblock::SessionMarker>();
        self.partition.open_passthrough_session(server_end);
        let passthrough = PassthroughSession::new(proxy);
        passthrough.serve(stream).await
    }

    async fn on_attach_vmo(&self, vmo: &zx::Vmo) -> Result<(), zx::Status> {
        let key = std::ptr::from_ref(vmo) as usize;
        // SAFETY: GPT does not map VMOs in its own process, so it cannot violate Rust's aliasing
        // guarantees.  Safety is delegated to the client process that mapped the VMO.
        let vmo_id = unsafe { self.partition.attach_vmo(vmo) }.await?;
        self.vmo_keys_to_vmoids_map.lock().insert(
            key,
            Arc::new(VmoIdWrapper { partition: Arc::downgrade(&self.partition), vmo_id }),
        );
        Ok(())
    }

    fn on_detach_vmo(&self, vmo: &zx::Vmo) {
        // Note that we will not immediately detach the VMO.  This happens when the last reference
        // to it is dropped (in [`VmoIdWrapper::drop`]).
        let key = std::ptr::from_ref(vmo) as usize;
        self.vmo_keys_to_vmoids_map.lock().remove(&key);
    }

    fn get_info(&self) -> Cow<'_, DeviceInfo> {
        Cow::Owned(self.partition.get_info())
    }

    async fn read(
        &self,
        device_block_offset: u64,
        block_count: u32,
        vmo: &Arc<zx::Vmo>,
        vmo_offset: u64, // *bytes* not blocks
        opts: ReadOptions,
        trace_flow_id: Option<NonZero<u64>>,
    ) -> Result<(), zx::Status> {
        let vmo_id = self.get_vmoid(vmo)?;
        self.partition
            .read(device_block_offset, block_count, &vmo_id, vmo_offset, opts, trace_flow_id)
            .await
    }

    async fn write(
        &self,
        device_block_offset: u64,
        length: u32,
        vmo: &Arc<zx::Vmo>,
        vmo_offset: u64, // *bytes* not blocks
        opts: WriteOptions,
        trace_flow_id: Option<NonZero<u64>>,
    ) -> Result<(), zx::Status> {
        let vmo_id = self.get_vmoid(vmo)?;
        self.partition
            .write(device_block_offset, length, &vmo_id, vmo_offset, opts, trace_flow_id)
            .await
    }

    async fn flush(&self, trace_flow_id: Option<NonZero<u64>>) -> Result<(), zx::Status> {
        self.partition.flush(trace_flow_id).await
    }

    async fn trim(
        &self,
        device_block_offset: u64,
        block_count: u32,
        trace_flow_id: Option<NonZero<u64>>,
    ) -> Result<(), zx::Status> {
        self.partition.trim(device_block_offset, block_count, trace_flow_id).await
    }
}

impl PartitionBackend {
    #[cfg(test)]
    pub fn vmo_count(&self) -> usize {
        self.vmo_keys_to_vmoids_map.lock().len()
    }

    pub fn new(partition: Arc<GptPartition>, passthrough: bool) -> Arc<Self> {
        Arc::new(Self {
            partition,
            vmo_keys_to_vmoids_map: Mutex::new(BTreeMap::new()),
            passthrough,
        })
    }

    /// Returns the old info.
    pub fn update_info(&self, info: gpt::PartitionInfo) -> gpt::PartitionInfo {
        self.partition.update_info(info)
    }

    fn get_vmoid(&self, vmo: &zx::Vmo) -> Result<Arc<VmoIdWrapper>, zx::Status> {
        let key = std::ptr::from_ref(vmo) as usize;
        self.vmo_keys_to_vmoids_map.lock().get(&key).map(Arc::clone).ok_or(zx::Status::NOT_FOUND)
    }
}
