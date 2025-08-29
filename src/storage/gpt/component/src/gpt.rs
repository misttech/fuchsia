// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::config::Config;
use crate::partition::PartitionBackend;
use crate::partitions_directory::PartitionsDirectory;
use anyhow::{Context as _, Error, anyhow};
use block_client::{
    BlockClient as _, BufferSlice, MutableBufferSlice, RemoteBlockClient, VmoId, WriteOptions,
};
use block_server::BlockServer;
use block_server::async_interface::SessionManager;

use fidl::endpoints::ServerEnd;
use fuchsia_sync::Mutex;
use futures::stream::TryStreamExt as _;
use std::collections::BTreeMap;
use std::num::NonZero;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use zx::AsHandleRef as _;
use {
    fidl_fuchsia_hardware_block as fblock, fidl_fuchsia_storage_partitions as fpartitions,
    fuchsia_async as fasync,
};

fn partition_directory_entry_name(index: u32) -> String {
    format!("part-{:03}", index)
}

/// A single partition in a GPT device.
pub struct GptPartition {
    gpt: Weak<GptManager>,
    info: Mutex<gpt::PartitionInfo>,
    block_client: Arc<RemoteBlockClient>,
}

fn trace_id(trace_flow_id: Option<NonZero<u64>>) -> u64 {
    trace_flow_id.map(|v| v.get()).unwrap_or_default()
}

impl GptPartition {
    pub fn new(
        gpt: &Arc<GptManager>,
        block_client: Arc<RemoteBlockClient>,
        info: gpt::PartitionInfo,
    ) -> Arc<Self> {
        Arc::new(Self { gpt: Arc::downgrade(gpt), info: Mutex::new(info), block_client })
    }

    pub async fn terminate(&self) {
        if let Err(error) = self.block_client.close().await {
            log::warn!(error:?; "Failed to close block client");
        }
    }

    /// Replaces the partition info, returning its old value.
    pub fn update_info(&self, info: gpt::PartitionInfo) -> gpt::PartitionInfo {
        std::mem::replace(&mut *self.info.lock(), info)
    }

    pub fn block_size(&self) -> u32 {
        self.block_client.block_size()
    }

    pub fn block_count(&self) -> u64 {
        self.info.lock().num_blocks
    }

    pub async fn attach_vmo(&self, vmo: &zx::Vmo) -> Result<VmoId, zx::Status> {
        self.block_client.attach_vmo(vmo).await
    }

    pub async fn detach_vmo(&self, vmoid: VmoId) -> Result<(), zx::Status> {
        self.block_client.detach_vmo(vmoid).await
    }

    pub fn open_passthrough_session(&self, session: ServerEnd<fblock::SessionMarker>) {
        if let Some(gpt) = self.gpt.upgrade() {
            let mapping = {
                let info = self.info.lock();
                fblock::BlockOffsetMapping {
                    source_block_offset: 0,
                    target_block_offset: info.start_block,
                    length: info.num_blocks,
                }
            };
            if let Err(err) = gpt.block_proxy.open_session_with_offset_map(session, &mapping) {
                // Client errors normally come back on `session` but that was already consumed.  The
                // client will get a PEER_CLOSED without an epitaph.
                log::warn!(err:?; "Failed to open passthrough session");
            }
        } else {
            if let Err(err) = session.close_with_epitaph(zx::Status::BAD_STATE) {
                log::warn!(err:?; "Failed to send session epitaph");
            }
        }
    }

    pub fn get_info(&self) -> block_server::DeviceInfo {
        convert_partition_info(
            &*self.info.lock(),
            self.block_client.block_flags(),
            self.block_client.max_transfer_blocks(),
        )
    }

    pub async fn read(
        &self,
        device_block_offset: u64,
        block_count: u32,
        vmo_id: &VmoId,
        vmo_offset: u64, // *bytes* not blocks
        trace_flow_id: Option<NonZero<u64>>,
    ) -> Result<(), zx::Status> {
        let dev_offset = self
            .absolute_offset(device_block_offset, block_count)
            .map(|offset| offset * self.block_size() as u64)?;
        let buffer = MutableBufferSlice::new_with_vmo_id(
            vmo_id,
            vmo_offset,
            (block_count * self.block_size()) as u64,
        );
        self.block_client.read_at_traced(buffer, dev_offset, trace_id(trace_flow_id)).await
    }

    pub fn barrier(&self) {
        self.block_client.barrier();
    }

    pub async fn write(
        &self,
        device_block_offset: u64,
        block_count: u32,
        vmo_id: &VmoId,
        vmo_offset: u64, // *bytes* not blocks
        opts: WriteOptions,
        trace_flow_id: Option<NonZero<u64>>,
    ) -> Result<(), zx::Status> {
        let dev_offset = self
            .absolute_offset(device_block_offset, block_count)
            .map(|offset| offset * self.block_size() as u64)?;
        let buffer = BufferSlice::new_with_vmo_id(
            vmo_id,
            vmo_offset,
            (block_count * self.block_size()) as u64,
        );
        self.block_client
            .write_at_with_opts_traced(buffer, dev_offset, opts, trace_id(trace_flow_id))
            .await
    }

    pub async fn flush(&self, trace_flow_id: Option<NonZero<u64>>) -> Result<(), zx::Status> {
        self.block_client.flush_traced(trace_id(trace_flow_id)).await
    }

    pub async fn trim(
        &self,
        device_block_offset: u64,
        block_count: u32,
        trace_flow_id: Option<NonZero<u64>>,
    ) -> Result<(), zx::Status> {
        let dev_offset = self
            .absolute_offset(device_block_offset, block_count)
            .map(|offset| offset * self.block_size() as u64)?;
        let len = block_count as u64 * self.block_size() as u64;
        self.block_client.trim_traced(dev_offset..dev_offset + len, trace_id(trace_flow_id)).await
    }

    // Converts a relative range specified by [offset, offset+len) into an absolute offset in the
    // GPT device, performing bounds checking within the partition.  Returns ZX_ERR_OUT_OF_RANGE for
    // an invalid offset/len.
    fn absolute_offset(&self, mut offset: u64, len: u32) -> Result<u64, zx::Status> {
        let info = self.info.lock();
        offset = offset.checked_add(info.start_block).ok_or(zx::Status::OUT_OF_RANGE)?;
        let end = offset.checked_add(len as u64).ok_or(zx::Status::OUT_OF_RANGE)?;
        if end > info.start_block + info.num_blocks {
            Err(zx::Status::OUT_OF_RANGE)
        } else {
            Ok(offset)
        }
    }
}

fn convert_partition_info(
    info: &gpt::PartitionInfo,
    device_flags: fblock::Flag,
    max_transfer_blocks: Option<NonZero<u32>>,
) -> block_server::DeviceInfo {
    block_server::DeviceInfo::Partition(block_server::PartitionInfo {
        device_flags,
        max_transfer_blocks,
        block_range: Some(info.start_block..info.start_block + info.num_blocks),
        type_guid: info.type_guid.to_bytes(),
        instance_guid: info.instance_guid.to_bytes(),
        name: info.label.clone(),
        flags: info.flags,
    })
}

fn can_merge(a: &gpt::PartitionInfo, b: &gpt::PartitionInfo) -> bool {
    a.start_block + a.num_blocks == b.start_block
}

struct PendingTransaction {
    transaction: gpt::Transaction,
    client_koid: zx::Koid,
    // A list of indexes for partitions which were added in the transaction.  When committing, all
    // newly created partitions are published.
    added_partitions: Vec<u32>,
    // A task which waits for the client end to be closed and clears the pending transaction.
    _signal_task: fasync::Task<()>,
}

struct Inner {
    gpt: gpt::Gpt,
    partitions: BTreeMap<u32, Arc<BlockServer<SessionManager<PartitionBackend>>>>,
    // Exposes all partitions for discovery by other components.  Should be kept in sync with
    // `partitions`.
    partitions_dir: PartitionsDirectory,
    pending_transaction: Option<PendingTransaction>,
}

impl Inner {
    /// Ensures that `transaction` matches our pending transaction.
    fn ensure_transaction_matches(&self, transaction: &zx::EventPair) -> Result<(), zx::Status> {
        if let Some(pending) = self.pending_transaction.as_ref() {
            if transaction.get_koid()? == pending.client_koid {
                Ok(())
            } else {
                Err(zx::Status::BAD_HANDLE)
            }
        } else {
            Err(zx::Status::BAD_STATE)
        }
    }

    async fn bind_partition(
        &mut self,
        parent: &Arc<GptManager>,
        index: u32,
        info: gpt::PartitionInfo,
        overlay_indexes: Vec<usize>,
    ) -> Result<(), Error> {
        log::trace!(
            "GPT part {index}{}: {info:?}",
            if !overlay_indexes.is_empty() { " (overlay)" } else { "" }
        );
        info.start_block
            .checked_add(info.num_blocks)
            .ok_or_else(|| anyhow!("Overflow in partition end"))?;
        let partition =
            PartitionBackend::new(GptPartition::new(parent, self.gpt.client().clone(), info));
        let block_server = Arc::new(BlockServer::new(parent.block_size, partition));
        if !overlay_indexes.is_empty() {
            self.partitions_dir.add_overlay(
                &partition_directory_entry_name(index),
                Arc::downgrade(&block_server),
                Arc::downgrade(parent),
                overlay_indexes,
            );
        } else {
            self.partitions_dir.add_partition(
                &partition_directory_entry_name(index),
                Arc::downgrade(&block_server),
                Arc::downgrade(parent),
                index as usize,
            );
        }
        self.partitions.insert(index, block_server);
        Ok(())
    }

    async fn bind_super_and_userdata_partition(
        &mut self,
        parent: &Arc<GptManager>,
        super_partition: (u32, gpt::PartitionInfo),
        userdata_partition: (u32, gpt::PartitionInfo),
    ) -> Result<(), Error> {
        let info = gpt::PartitionInfo {
            label: "super_and_userdata".to_string(),
            type_guid: super_partition.1.type_guid.clone(),
            instance_guid: super_partition.1.instance_guid.clone(),
            start_block: super_partition.1.start_block,
            num_blocks: super_partition.1.num_blocks + userdata_partition.1.num_blocks,
            flags: super_partition.1.flags,
        };
        log::trace!(
            "GPT merged parts {:?} + {:?} -> {info:?}",
            super_partition.1,
            userdata_partition.1
        );
        self.bind_partition(
            parent,
            super_partition.0,
            info,
            vec![super_partition.0 as usize, userdata_partition.0 as usize],
        )
        .await
    }

    async fn bind_all_partitions(&mut self, parent: &Arc<GptManager>) -> Result<(), Error> {
        self.partitions.clear();
        self.partitions_dir.clear();

        let mut partitions = self.gpt.partitions().clone();
        if parent.config.merge_super_and_userdata {
            // Attempt to merge the first `super` and `userdata` we find.  The rest will be treated
            // as regular partitions.
            let super_part = match partitions
                .iter()
                .find(|(_, info)| info.label == "super")
                .map(|(index, _)| *index)
            {
                Some(index) => partitions.remove_entry(&index),
                None => None,
            };
            let userdata_part = match partitions
                .iter()
                .find(|(_, info)| info.label == "userdata")
                .map(|(index, _)| *index)
            {
                Some(index) => partitions.remove_entry(&index),
                None => None,
            };
            if super_part.is_some() && userdata_part.is_some() {
                let super_part = super_part.unwrap();
                let userdata_part = userdata_part.unwrap();
                if can_merge(&super_part.1, &userdata_part.1) {
                    self.bind_super_and_userdata_partition(parent, super_part, userdata_part)
                        .await?;
                } else {
                    log::warn!("super/userdata cannot be merged");
                    self.bind_partition(parent, super_part.0, super_part.1, vec![]).await?;
                    self.bind_partition(parent, userdata_part.0, userdata_part.1, vec![]).await?;
                }
            } else if super_part.is_some() || userdata_part.is_some() {
                log::warn!("Only one of super/userdata found; not merging");
                let (index, info) = super_part.or(userdata_part).unwrap();
                self.bind_partition(parent, index, info, vec![]).await?;
            }
        }
        for (index, info) in partitions {
            self.bind_partition(parent, index, info, vec![]).await?;
        }
        Ok(())
    }

    fn add_partition(&mut self, info: gpt::PartitionInfo) -> Result<usize, gpt::AddPartitionError> {
        let pending = self.pending_transaction.as_mut().unwrap();
        let idx = self.gpt.add_partition(&mut pending.transaction, info)?;
        pending.added_partitions.push(idx as u32);
        Ok(idx)
    }
}

/// Runs a GPT device.
pub struct GptManager {
    config: Config,
    block_proxy: fblock::BlockProxy,
    block_size: u32,
    block_count: u64,
    inner: futures::lock::Mutex<Inner>,
    shutdown: AtomicBool,
}

impl std::fmt::Debug for GptManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        f.debug_struct("GptManager")
            .field("block_size", &self.block_size)
            .field("block_count", &self.block_count)
            .finish()
    }
}

impl GptManager {
    pub async fn new(
        block_proxy: fblock::BlockProxy,
        partitions_dir: Arc<vfs::directory::immutable::Simple>,
    ) -> Result<Arc<Self>, Error> {
        Self::new_with_config(block_proxy, partitions_dir, Config::default()).await
    }

    pub async fn new_with_config(
        block_proxy: fblock::BlockProxy,
        partitions_dir: Arc<vfs::directory::immutable::Simple>,
        config: Config,
    ) -> Result<Arc<Self>, Error> {
        log::info!("Binding to GPT");
        let client = Arc::new(RemoteBlockClient::new(block_proxy.clone()).await?);
        let block_size = client.block_size();
        let block_count = client.block_count();
        let gpt = gpt::Gpt::open(client).await.context("Failed to load GPT")?;

        let this = Arc::new(Self {
            config,
            block_proxy,
            block_size,
            block_count,
            inner: futures::lock::Mutex::new(Inner {
                gpt,
                partitions: BTreeMap::new(),
                partitions_dir: PartitionsDirectory::new(partitions_dir),
                pending_transaction: None,
            }),
            shutdown: AtomicBool::new(false),
        });
        this.inner.lock().await.bind_all_partitions(&this).await?;
        log::info!("Starting all partitions OK!");
        Ok(this)
    }

    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    pub fn block_count(&self) -> u64 {
        self.block_count
    }

    pub async fn create_transaction(self: &Arc<Self>) -> Result<zx::EventPair, zx::Status> {
        let mut inner = self.inner.lock().await;
        if inner.pending_transaction.is_some() {
            return Err(zx::Status::ALREADY_EXISTS);
        }
        let transaction = inner.gpt.create_transaction().unwrap();
        let (client_end, server_end) = zx::EventPair::create();
        let client_koid = client_end.get_koid()?;
        let signal_waiter = fasync::OnSignals::new(server_end, zx::Signals::EVENTPAIR_PEER_CLOSED);
        let this = self.clone();
        let task = fasync::Task::spawn(async move {
            let _ = signal_waiter.await;
            let mut inner = this.inner.lock().await;
            if inner.pending_transaction.as_ref().map_or(false, |t| t.client_koid == client_koid) {
                inner.pending_transaction = None;
            }
        });
        inner.pending_transaction = Some(PendingTransaction {
            transaction,
            client_koid,
            added_partitions: vec![],
            _signal_task: task,
        });
        Ok(client_end)
    }

    pub async fn commit_transaction(
        self: &Arc<Self>,
        transaction: zx::EventPair,
    ) -> Result<(), zx::Status> {
        if let Err((status, old_infos)) = self.commit_transaction_inner(transaction).await {
            let inner = self.inner.lock().await;
            for (idx, info) in old_infos {
                if let Some(part) = inner.partitions.get(&idx) {
                    part.session_manager().interface().update_info(info);
                }
            }
            Err(status)
        } else {
            Ok(())
        }
    }

    // On error, returns a vector of state which was modified that needs to be restored.  Scopeguard
    // doesn't work here because `inner` requires an async lock.
    async fn commit_transaction_inner(
        self: &Arc<Self>,
        transaction: zx::EventPair,
    ) -> Result<(), (zx::Status, Vec<(u32, gpt::PartitionInfo)>)> {
        let mut inner = self.inner.lock().await;
        inner.ensure_transaction_matches(&transaction).map_err(|status| (status, vec![]))?;
        let pending = std::mem::take(&mut inner.pending_transaction).unwrap();
        let mut old_infos = vec![];
        for (info, idx) in pending
            .transaction
            .partitions
            .iter()
            .zip(0u32..)
            .filter(|(info, idx)| !info.is_nil() && !pending.added_partitions.contains(idx))
        {
            let part = inner.partitions.get(&idx).ok_or_else(|| {
                log::warn!("Failed to find part {idx}");
                (zx::Status::BAD_STATE, old_infos.clone())
            })?;
            old_infos.push((idx, part.session_manager().interface().update_info(info.clone())));
        }
        if let Err(err) = inner.gpt.commit_transaction(pending.transaction).await {
            log::warn!(err:?; "Failed to commit transaction");
            return Err((zx::Status::IO, old_infos.clone()));
        }
        for idx in pending.added_partitions {
            let info = inner
                .gpt
                .partitions()
                .get(&idx)
                .ok_or_else(|| (zx::Status::BAD_STATE, old_infos.clone()))?
                .clone();
            inner.bind_partition(self, idx, info, vec![]).await.map_err(|err| {
                log::error!(err:?; "Failed to bind partition");
                (zx::Status::BAD_STATE, old_infos.clone())
            })?;
        }
        Ok(())
    }

    pub async fn add_partition(
        &self,
        request: fpartitions::PartitionsManagerAddPartitionRequest,
    ) -> Result<(), zx::Status> {
        let mut inner = self.inner.lock().await;
        inner.ensure_transaction_matches(
            request.transaction.as_ref().ok_or(zx::Status::BAD_HANDLE)?,
        )?;
        let info = gpt::PartitionInfo {
            label: request.name.ok_or(zx::Status::INVALID_ARGS)?,
            type_guid: request
                .type_guid
                .map(|value| gpt::Guid::from_bytes(value.value))
                .ok_or(zx::Status::INVALID_ARGS)?,
            instance_guid: request
                .instance_guid
                .map(|value| gpt::Guid::from_bytes(value.value))
                .unwrap_or_else(|| gpt::Guid::generate()),
            start_block: 0,
            num_blocks: request.num_blocks.ok_or(zx::Status::INVALID_ARGS)?,
            flags: request.flags.unwrap_or_default(),
        };
        let idx = inner.add_partition(info)?;
        let partition =
            inner.pending_transaction.as_ref().unwrap().transaction.partitions.get(idx).unwrap();
        log::info!(
            "Allocated partition {:?} at {:?}",
            partition.label,
            partition.start_block..partition.start_block + partition.num_blocks
        );
        Ok(())
    }

    pub async fn handle_partitions_requests(
        &self,
        gpt_index: usize,
        mut requests: fpartitions::PartitionRequestStream,
    ) -> Result<(), zx::Status> {
        while let Some(request) = requests.try_next().await.unwrap() {
            match request {
                fpartitions::PartitionRequest::UpdateMetadata { payload, responder } => {
                    responder
                        .send(
                            self.update_partition_metadata(gpt_index, payload)
                                .await
                                .map_err(|status| status.into_raw()),
                        )
                        .unwrap_or_else(
                            |err| log::error!(err:?; "Failed to send UpdateMetadata response"),
                        );
                }
            }
        }
        Ok(())
    }

    async fn update_partition_metadata(
        &self,
        gpt_index: usize,
        request: fpartitions::PartitionUpdateMetadataRequest,
    ) -> Result<(), zx::Status> {
        let mut inner = self.inner.lock().await;
        inner.ensure_transaction_matches(
            request.transaction.as_ref().ok_or(zx::Status::BAD_HANDLE)?,
        )?;

        let transaction = &mut inner.pending_transaction.as_mut().unwrap().transaction;
        let entry = transaction.partitions.get_mut(gpt_index).ok_or(zx::Status::BAD_STATE)?;
        if let Some(type_guid) = request.type_guid.as_ref().cloned() {
            entry.type_guid = gpt::Guid::from_bytes(type_guid.value);
        }
        if let Some(flags) = request.flags.as_ref() {
            entry.flags = *flags;
        }
        Ok(())
    }

    pub async fn handle_overlay_partitions_requests(
        &self,
        gpt_indexes: Vec<usize>,
        mut requests: fpartitions::OverlayPartitionRequestStream,
    ) -> Result<(), zx::Status> {
        while let Some(request) = requests.try_next().await.unwrap() {
            match request {
                fpartitions::OverlayPartitionRequest::GetPartitions { responder } => {
                    match self.get_overlay_partition_info(&gpt_indexes[..]).await {
                        Ok(partitions) => responder.send(Ok(&partitions[..])),
                        Err(status) => responder.send(Err(status.into_raw())),
                    }
                    .unwrap_or_else(
                        |err| log::error!(err:?; "Failed to send GetPartitions response"),
                    );
                }
            }
        }
        Ok(())
    }

    async fn get_overlay_partition_info(
        &self,
        gpt_indexes: &[usize],
    ) -> Result<Vec<fpartitions::PartitionInfo>, zx::Status> {
        fn convert_partition_info(info: &gpt::PartitionInfo) -> fpartitions::PartitionInfo {
            fpartitions::PartitionInfo {
                name: info.label.to_string(),
                type_guid: fidl_fuchsia_hardware_block_partition::Guid {
                    value: info.type_guid.to_bytes(),
                },
                instance_guid: fidl_fuchsia_hardware_block_partition::Guid {
                    value: info.instance_guid.to_bytes(),
                },
                start_block: info.start_block,
                num_blocks: info.num_blocks,
                flags: info.flags,
            }
        }

        let inner = self.inner.lock().await;
        let mut partitions = vec![];
        for index in gpt_indexes {
            let index: u32 = *index as u32;
            partitions.push(
                inner
                    .gpt
                    .partitions()
                    .get(&index)
                    .map(convert_partition_info)
                    .ok_or(zx::Status::BAD_STATE)?,
            );
        }
        Ok(partitions)
    }

    pub async fn reset_partition_table(
        self: &Arc<Self>,
        partitions: Vec<gpt::PartitionInfo>,
    ) -> Result<(), zx::Status> {
        let mut inner = self.inner.lock().await;
        if inner.pending_transaction.is_some() {
            return Err(zx::Status::BAD_STATE);
        }

        log::info!("Resetting gpt.  Expect data loss!!!");
        let mut transaction = inner.gpt.create_transaction().unwrap();
        transaction.partitions = partitions;
        inner.gpt.commit_transaction(transaction).await?;

        if let Err(err) = inner.bind_all_partitions(&self).await {
            log::error!(err:?; "Failed to rebind partitions");
            return Err(zx::Status::BAD_STATE);
        }
        log::info!("Rebinding partitions OK!");
        Ok(())
    }

    pub async fn shutdown(self: Arc<Self>) {
        log::info!("Shutting down gpt");
        let mut inner = self.inner.lock().await;
        inner.partitions_dir.clear();
        inner.partitions.clear();
        self.shutdown.store(true, Ordering::Relaxed);
        log::info!("Shutting down gpt OK");
    }
}

impl Drop for GptManager {
    fn drop(&mut self) {
        assert!(self.shutdown.load(Ordering::Relaxed), "Did you forget to shutdown?");
    }
}

#[cfg(test)]
mod tests {
    use super::GptManager;
    use block_client::{BlockClient as _, BufferSlice, MutableBufferSlice, RemoteBlockClient};
    use block_server::{BlockInfo, DeviceInfo, WriteOptions};
    use fidl::HandleBased as _;
    use fuchsia_component::client::connect_to_named_protocol_at_dir_root;
    use gpt::{Gpt, Guid, PartitionInfo};
    use std::num::NonZero;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use vmo_backed_block_server::{
        InitialContents, VmoBackedServer, VmoBackedServerOptions, VmoBackedServerTestingExt as _,
    };
    use {
        fidl_fuchsia_hardware_block as fblock, fidl_fuchsia_hardware_block_volume as fvolume,
        fidl_fuchsia_io as fio, fidl_fuchsia_storage_partitions as fpartitions,
        fuchsia_async as fasync,
    };

    async fn setup(
        block_size: u32,
        block_count: u64,
        partitions: Vec<PartitionInfo>,
    ) -> (Arc<VmoBackedServer>, Arc<vfs::directory::immutable::Simple>) {
        setup_with_options(
            VmoBackedServerOptions {
                initial_contents: InitialContents::FromCapacity(block_count),
                block_size,
                ..Default::default()
            },
            partitions,
        )
        .await
    }

    async fn setup_with_options(
        opts: VmoBackedServerOptions<'_>,
        partitions: Vec<PartitionInfo>,
    ) -> (Arc<VmoBackedServer>, Arc<vfs::directory::immutable::Simple>) {
        let server = Arc::new(opts.build().unwrap());
        {
            let (block_client, block_server) =
                fidl::endpoints::create_proxy::<fblock::BlockMarker>();
            let volume_stream = fidl::endpoints::ServerEnd::<fvolume::VolumeMarker>::from(
                block_server.into_channel(),
            )
            .into_stream();
            let server_clone = server.clone();
            let _task = fasync::Task::spawn(async move { server_clone.serve(volume_stream).await });
            let client = Arc::new(RemoteBlockClient::new(block_client).await.unwrap());
            Gpt::format(client, partitions).await.unwrap();
        }
        (server, vfs::directory::immutable::simple())
    }

    #[fuchsia::test]
    async fn load_unformatted_gpt() {
        let vmo = zx::Vmo::create(4096).unwrap();
        let server = Arc::new(VmoBackedServer::from_vmo(512, vmo));

        GptManager::new(server.connect(), vfs::directory::immutable::simple())
            .await
            .expect_err("load should fail");
    }

    #[fuchsia::test]
    async fn load_formatted_empty_gpt() {
        let (block_device, partitions_dir) = setup(512, 8, vec![]).await;

        let runner = GptManager::new(block_device.connect(), partitions_dir)
            .await
            .expect("load should succeed");
        runner.shutdown().await;
    }

    #[fuchsia::test]
    async fn load_formatted_gpt_with_one_partition() {
        const PART_TYPE_GUID: [u8; 16] = [2u8; 16];
        const PART_INSTANCE_GUID: [u8; 16] = [2u8; 16];
        const PART_NAME: &str = "part";

        let (block_device, partitions_dir) = setup(
            512,
            8,
            vec![PartitionInfo {
                label: PART_NAME.to_string(),
                type_guid: Guid::from_bytes(PART_TYPE_GUID),
                instance_guid: Guid::from_bytes(PART_INSTANCE_GUID),
                start_block: 4,
                num_blocks: 1,
                flags: 0,
            }],
        )
        .await;

        let partitions_dir_clone = partitions_dir.clone();
        let runner = GptManager::new(block_device.connect(), partitions_dir_clone)
            .await
            .expect("load should succeed");
        partitions_dir.get_entry("part-000").expect("No entry found");
        partitions_dir.get_entry("part-001").map(|_| ()).expect_err("Extra entry found");
        runner.shutdown().await;
    }

    #[fuchsia::test]
    async fn load_formatted_gpt_with_two_partitions() {
        const PART_TYPE_GUID: [u8; 16] = [2u8; 16];
        const PART_INSTANCE_1_GUID: [u8; 16] = [2u8; 16];
        const PART_INSTANCE_2_GUID: [u8; 16] = [3u8; 16];
        const PART_1_NAME: &str = "part1";
        const PART_2_NAME: &str = "part2";

        let (block_device, partitions_dir) = setup(
            512,
            8,
            vec![
                PartitionInfo {
                    label: PART_1_NAME.to_string(),
                    type_guid: Guid::from_bytes(PART_TYPE_GUID),
                    instance_guid: Guid::from_bytes(PART_INSTANCE_1_GUID),
                    start_block: 4,
                    num_blocks: 1,
                    flags: 0,
                },
                PartitionInfo {
                    label: PART_2_NAME.to_string(),
                    type_guid: Guid::from_bytes(PART_TYPE_GUID),
                    instance_guid: Guid::from_bytes(PART_INSTANCE_2_GUID),
                    start_block: 5,
                    num_blocks: 1,
                    flags: 0,
                },
            ],
        )
        .await;

        let partitions_dir_clone = partitions_dir.clone();
        let runner = GptManager::new(block_device.connect(), partitions_dir_clone)
            .await
            .expect("load should succeed");
        partitions_dir.get_entry("part-000").expect("No entry found");
        partitions_dir.get_entry("part-001").expect("No entry found");
        partitions_dir.get_entry("part-002").map(|_| ()).expect_err("Extra entry found");
        runner.shutdown().await;
    }

    #[fuchsia::test]
    async fn partition_io() {
        const PART_TYPE_GUID: [u8; 16] = [2u8; 16];
        const PART_INSTANCE_GUID: [u8; 16] = [2u8; 16];
        const PART_NAME: &str = "part";

        let (block_device, partitions_dir) = setup(
            512,
            8,
            vec![PartitionInfo {
                label: PART_NAME.to_string(),
                type_guid: Guid::from_bytes(PART_TYPE_GUID),
                instance_guid: Guid::from_bytes(PART_INSTANCE_GUID),
                start_block: 4,
                num_blocks: 2,
                flags: 0,
            }],
        )
        .await;

        let partitions_dir_clone = partitions_dir.clone();
        let runner = GptManager::new(block_device.connect(), partitions_dir_clone)
            .await
            .expect("load should succeed");

        let proxy = vfs::serve_directory(
            partitions_dir.clone(),
            vfs::path::Path::validate_and_split("part-000").unwrap(),
            fio::PERM_READABLE,
        );
        let block =
            connect_to_named_protocol_at_dir_root::<fvolume::VolumeMarker>(&proxy, "volume")
                .expect("Failed to open block service");
        let client = RemoteBlockClient::new(block).await.expect("Failed to create block client");

        assert_eq!(client.block_count(), 2);
        assert_eq!(client.block_size(), 512);

        let buf = vec![0xabu8; 512];
        client.write_at(BufferSlice::Memory(&buf[..]), 0).await.expect("write_at failed");
        client
            .write_at(BufferSlice::Memory(&buf[..]), 1024)
            .await
            .expect_err("write_at should fail when writing past partition end");
        let mut buf2 = vec![0u8; 512];
        client.read_at(MutableBufferSlice::Memory(&mut buf2[..]), 0).await.expect("read_at failed");
        assert_eq!(buf, buf2);
        client
            .read_at(MutableBufferSlice::Memory(&mut buf2[..]), 1024)
            .await
            .expect_err("read_at should fail when reading past partition end");
        client.trim(512..1024).await.expect("trim failed");
        client.trim(1..512).await.expect_err("trim with invalid range should fail");
        client.trim(1024..1536).await.expect_err("trim past end of partition should fail");
        runner.shutdown().await;

        // Ensure writes persisted to the partition.
        let mut buf = vec![0u8; 512];
        let client =
            RemoteBlockClient::new(block_device.connect::<fblock::BlockProxy>()).await.unwrap();
        client.read_at(MutableBufferSlice::Memory(&mut buf[..]), 2048).await.unwrap();
        assert_eq!(&buf[..], &[0xabu8; 512]);
    }

    #[fuchsia::test]
    async fn load_formatted_gpt_with_invalid_primary_header() {
        const PART_TYPE_GUID: [u8; 16] = [2u8; 16];
        const PART_INSTANCE_1_GUID: [u8; 16] = [2u8; 16];
        const PART_INSTANCE_2_GUID: [u8; 16] = [3u8; 16];
        const PART_1_NAME: &str = "part1";
        const PART_2_NAME: &str = "part2";

        let (block_device, partitions_dir) = setup(
            512,
            8,
            vec![
                PartitionInfo {
                    label: PART_1_NAME.to_string(),
                    type_guid: Guid::from_bytes(PART_TYPE_GUID),
                    instance_guid: Guid::from_bytes(PART_INSTANCE_1_GUID),
                    start_block: 4,
                    num_blocks: 1,
                    flags: 0,
                },
                PartitionInfo {
                    label: PART_2_NAME.to_string(),
                    type_guid: Guid::from_bytes(PART_TYPE_GUID),
                    instance_guid: Guid::from_bytes(PART_INSTANCE_2_GUID),
                    start_block: 5,
                    num_blocks: 1,
                    flags: 0,
                },
            ],
        )
        .await;
        {
            let (client, stream) =
                fidl::endpoints::create_proxy_and_stream::<fvolume::VolumeMarker>();
            let server = block_device.clone();
            let _task = fasync::Task::spawn(async move { server.serve(stream).await });
            let client = RemoteBlockClient::new(client).await.unwrap();
            client.write_at(BufferSlice::Memory(&[0xffu8; 512]), 512).await.unwrap();
        }

        let runner = GptManager::new(block_device.connect(), partitions_dir.clone())
            .await
            .expect("load should succeed");
        partitions_dir.get_entry("part-000").expect("No entry found");
        partitions_dir.get_entry("part-001").expect("No entry found");
        runner.shutdown().await;
    }

    #[fuchsia::test]
    async fn load_formatted_gpt_with_invalid_primary_partition_table() {
        const PART_TYPE_GUID: [u8; 16] = [2u8; 16];
        const PART_INSTANCE_1_GUID: [u8; 16] = [2u8; 16];
        const PART_INSTANCE_2_GUID: [u8; 16] = [3u8; 16];
        const PART_1_NAME: &str = "part1";
        const PART_2_NAME: &str = "part2";

        let (block_device, partitions_dir) = setup(
            512,
            8,
            vec![
                PartitionInfo {
                    label: PART_1_NAME.to_string(),
                    type_guid: Guid::from_bytes(PART_TYPE_GUID),
                    instance_guid: Guid::from_bytes(PART_INSTANCE_1_GUID),
                    start_block: 4,
                    num_blocks: 1,
                    flags: 0,
                },
                PartitionInfo {
                    label: PART_2_NAME.to_string(),
                    type_guid: Guid::from_bytes(PART_TYPE_GUID),
                    instance_guid: Guid::from_bytes(PART_INSTANCE_2_GUID),
                    start_block: 5,
                    num_blocks: 1,
                    flags: 0,
                },
            ],
        )
        .await;
        {
            let (client, stream) =
                fidl::endpoints::create_proxy_and_stream::<fvolume::VolumeMarker>();
            let server = block_device.clone();
            let _task = fasync::Task::spawn(async move { server.serve(stream).await });
            let client = RemoteBlockClient::new(client).await.unwrap();
            client.write_at(BufferSlice::Memory(&[0xffu8; 512]), 1024).await.unwrap();
        }

        let runner = GptManager::new(block_device.connect(), partitions_dir.clone())
            .await
            .expect("load should succeed");
        partitions_dir.get_entry("part-000").expect("No entry found");
        partitions_dir.get_entry("part-001").expect("No entry found");
        runner.shutdown().await;
    }

    #[fuchsia::test]
    async fn force_access_passed_through() {
        const BLOCK_SIZE: u32 = 512;
        const BLOCK_COUNT: u64 = 1024;

        struct Observer(Arc<AtomicBool>);

        impl vmo_backed_block_server::Observer for Observer {
            fn write(
                &self,
                _device_block_offset: u64,
                _block_count: u32,
                _vmo: &Arc<zx::Vmo>,
                _vmo_offset: u64,
                opts: WriteOptions,
            ) -> vmo_backed_block_server::WriteAction {
                assert_eq!(
                    opts.contains(WriteOptions::FORCE_ACCESS),
                    self.0.load(Ordering::Relaxed)
                );
                vmo_backed_block_server::WriteAction::Write
            }
        }

        let expect_force_access = Arc::new(AtomicBool::new(false));
        let (server, partitions_dir) = setup_with_options(
            VmoBackedServerOptions {
                initial_contents: InitialContents::FromCapacity(BLOCK_COUNT),
                block_size: BLOCK_SIZE,
                observer: Some(Box::new(Observer(expect_force_access.clone()))),
                ..Default::default()
            },
            vec![PartitionInfo {
                label: "foo".to_string(),
                type_guid: Guid::from_bytes([1; 16]),
                instance_guid: Guid::from_bytes([2; 16]),
                start_block: 4,
                num_blocks: 1,
                flags: 0,
            }],
        )
        .await;

        let manager = GptManager::new(server.connect(), partitions_dir.clone()).await.unwrap();

        let proxy = vfs::serve_directory(
            partitions_dir.clone(),
            vfs::path::Path::validate_and_split("part-000").unwrap(),
            fio::PERM_READABLE,
        );
        let block =
            connect_to_named_protocol_at_dir_root::<fvolume::VolumeMarker>(&proxy, "volume")
                .expect("Failed to open block service");
        let client = RemoteBlockClient::new(block).await.expect("Failed to create block client");

        let buffer = vec![0; BLOCK_SIZE as usize];
        client.write_at(BufferSlice::Memory(&buffer), 0).await.unwrap();

        expect_force_access.store(true, Ordering::Relaxed);

        client
            .write_at_with_opts(BufferSlice::Memory(&buffer), 0, WriteOptions::FORCE_ACCESS)
            .await
            .unwrap();

        manager.shutdown().await;
    }

    #[fuchsia::test]
    async fn barrier_passed_through() {
        const BLOCK_SIZE: u32 = 512;
        const BLOCK_COUNT: u64 = 1024;

        struct Observer(Arc<AtomicBool>);

        impl vmo_backed_block_server::Observer for Observer {
            fn barrier(&self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }

        let expect_barrier = Arc::new(AtomicBool::new(false));
        let (server, partitions_dir) = setup_with_options(
            VmoBackedServerOptions {
                initial_contents: InitialContents::FromCapacity(BLOCK_COUNT),
                block_size: BLOCK_SIZE,
                observer: Some(Box::new(Observer(expect_barrier.clone()))),
                ..Default::default()
            },
            vec![PartitionInfo {
                label: "foo".to_string(),
                type_guid: Guid::from_bytes([1; 16]),
                instance_guid: Guid::from_bytes([2; 16]),
                start_block: 4,
                num_blocks: 1,
                flags: 0,
            }],
        )
        .await;

        let manager = GptManager::new(server.connect(), partitions_dir.clone()).await.unwrap();

        let proxy = vfs::serve_directory(
            partitions_dir.clone(),
            vfs::path::Path::validate_and_split("part-000").unwrap(),
            fio::PERM_READABLE,
        );
        let block =
            connect_to_named_protocol_at_dir_root::<fvolume::VolumeMarker>(&proxy, "volume")
                .expect("Failed to open block service");
        let client = RemoteBlockClient::new(block).await.expect("Failed to create block client");

        let buffer = vec![0; BLOCK_SIZE as usize];
        client.write_at(BufferSlice::Memory(&buffer), 0).await.unwrap();

        client.barrier();
        client.write_at(BufferSlice::Memory(&buffer), 0).await.unwrap();

        assert!(expect_barrier.load(Ordering::Relaxed));

        manager.shutdown().await;
    }

    #[fuchsia::test]
    async fn commit_transaction() {
        const PART_TYPE_GUID: [u8; 16] = [2u8; 16];
        const PART_1_INSTANCE_GUID: [u8; 16] = [2u8; 16];
        const PART_1_NAME: &str = "part";
        const PART_2_INSTANCE_GUID: [u8; 16] = [3u8; 16];
        const PART_2_NAME: &str = "part2";

        let (block_device, partitions_dir) = setup(
            512,
            16,
            vec![
                PartitionInfo {
                    label: PART_1_NAME.to_string(),
                    type_guid: Guid::from_bytes(PART_TYPE_GUID),
                    instance_guid: Guid::from_bytes(PART_1_INSTANCE_GUID),
                    start_block: 4,
                    num_blocks: 1,
                    flags: 0,
                },
                PartitionInfo {
                    label: PART_2_NAME.to_string(),
                    type_guid: Guid::from_bytes(PART_TYPE_GUID),
                    instance_guid: Guid::from_bytes(PART_2_INSTANCE_GUID),
                    start_block: 5,
                    num_blocks: 1,
                    flags: 0,
                },
            ],
        )
        .await;
        let runner = GptManager::new(block_device.connect(), partitions_dir.clone())
            .await
            .expect("load should succeed");

        let part_0_dir = vfs::serve_directory(
            partitions_dir.clone(),
            vfs::Path::validate_and_split("part-000").unwrap(),
            fio::PERM_READABLE,
        );
        let part_1_dir = vfs::serve_directory(
            partitions_dir.clone(),
            vfs::Path::validate_and_split("part-001").unwrap(),
            fio::PERM_READABLE,
        );
        let part_0_proxy = connect_to_named_protocol_at_dir_root::<fpartitions::PartitionMarker>(
            &part_0_dir,
            "partition",
        )
        .expect("Failed to open Partition service");
        let part_1_proxy = connect_to_named_protocol_at_dir_root::<fpartitions::PartitionMarker>(
            &part_1_dir,
            "partition",
        )
        .expect("Failed to open Partition service");

        let transaction = runner.create_transaction().await.expect("Failed to create transaction");
        part_0_proxy
            .update_metadata(fpartitions::PartitionUpdateMetadataRequest {
                transaction: Some(transaction.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap()),
                type_guid: Some(fidl_fuchsia_hardware_block_partition::Guid {
                    value: [0xffu8; 16],
                }),
                ..Default::default()
            })
            .await
            .expect("FIDL error")
            .expect("Failed to update_metadata");
        part_1_proxy
            .update_metadata(fpartitions::PartitionUpdateMetadataRequest {
                transaction: Some(transaction.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap()),
                flags: Some(1234),
                ..Default::default()
            })
            .await
            .expect("FIDL error")
            .expect("Failed to update_metadata");
        runner.commit_transaction(transaction).await.expect("Failed to commit transaction");

        // Ensure the changes have propagated to the correct partitions.
        let part_0_block =
            connect_to_named_protocol_at_dir_root::<fvolume::VolumeMarker>(&part_0_dir, "volume")
                .expect("Failed to open Volume service");
        let (status, guid) = part_0_block.get_type_guid().await.expect("FIDL error");
        assert_eq!(zx::Status::from_raw(status), zx::Status::OK);
        assert_eq!(guid.unwrap().value, [0xffu8; 16]);
        let part_1_block =
            connect_to_named_protocol_at_dir_root::<fvolume::VolumeMarker>(&part_1_dir, "volume")
                .expect("Failed to open Volume service");
        let metadata =
            part_1_block.get_metadata().await.expect("FIDL error").expect("get_metadata failed");
        assert_eq!(metadata.type_guid.unwrap().value, PART_TYPE_GUID);
        assert_eq!(metadata.flags, Some(1234));

        runner.shutdown().await;
    }

    #[fuchsia::test]
    async fn commit_transaction_with_io_error() {
        const PART_TYPE_GUID: [u8; 16] = [2u8; 16];
        const PART_1_INSTANCE_GUID: [u8; 16] = [2u8; 16];
        const PART_1_NAME: &str = "part";
        const PART_2_INSTANCE_GUID: [u8; 16] = [3u8; 16];
        const PART_2_NAME: &str = "part2";

        #[derive(Clone)]
        struct Observer(Arc<AtomicBool>);
        impl vmo_backed_block_server::Observer for Observer {
            fn write(
                &self,
                _device_block_offset: u64,
                _block_count: u32,
                _vmo: &Arc<zx::Vmo>,
                _vmo_offset: u64,
                _opts: WriteOptions,
            ) -> vmo_backed_block_server::WriteAction {
                if self.0.load(Ordering::Relaxed) {
                    vmo_backed_block_server::WriteAction::Fail
                } else {
                    vmo_backed_block_server::WriteAction::Write
                }
            }
        }
        let observer = Observer(Arc::new(AtomicBool::new(false)));
        let (block_device, partitions_dir) = setup_with_options(
            VmoBackedServerOptions {
                initial_contents: InitialContents::FromCapacity(16),
                block_size: 512,
                observer: Some(Box::new(observer.clone())),
                ..Default::default()
            },
            vec![
                PartitionInfo {
                    label: PART_1_NAME.to_string(),
                    type_guid: Guid::from_bytes(PART_TYPE_GUID),
                    instance_guid: Guid::from_bytes(PART_1_INSTANCE_GUID),
                    start_block: 4,
                    num_blocks: 1,
                    flags: 0,
                },
                PartitionInfo {
                    label: PART_2_NAME.to_string(),
                    type_guid: Guid::from_bytes(PART_TYPE_GUID),
                    instance_guid: Guid::from_bytes(PART_2_INSTANCE_GUID),
                    start_block: 5,
                    num_blocks: 1,
                    flags: 0,
                },
            ],
        )
        .await;
        let runner = GptManager::new(block_device.connect(), partitions_dir.clone())
            .await
            .expect("load should succeed");

        let part_0_dir = vfs::serve_directory(
            partitions_dir.clone(),
            vfs::Path::validate_and_split("part-000").unwrap(),
            fio::PERM_READABLE,
        );
        let part_1_dir = vfs::serve_directory(
            partitions_dir.clone(),
            vfs::Path::validate_and_split("part-001").unwrap(),
            fio::PERM_READABLE,
        );
        let part_0_proxy = connect_to_named_protocol_at_dir_root::<fpartitions::PartitionMarker>(
            &part_0_dir,
            "partition",
        )
        .expect("Failed to open Partition service");
        let part_1_proxy = connect_to_named_protocol_at_dir_root::<fpartitions::PartitionMarker>(
            &part_1_dir,
            "partition",
        )
        .expect("Failed to open Partition service");

        let transaction = runner.create_transaction().await.expect("Failed to create transaction");
        part_0_proxy
            .update_metadata(fpartitions::PartitionUpdateMetadataRequest {
                transaction: Some(transaction.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap()),
                type_guid: Some(fidl_fuchsia_hardware_block_partition::Guid {
                    value: [0xffu8; 16],
                }),
                ..Default::default()
            })
            .await
            .expect("FIDL error")
            .expect("Failed to update_metadata");
        part_1_proxy
            .update_metadata(fpartitions::PartitionUpdateMetadataRequest {
                transaction: Some(transaction.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap()),
                flags: Some(1234),
                ..Default::default()
            })
            .await
            .expect("FIDL error")
            .expect("Failed to update_metadata");

        observer.0.store(true, Ordering::Relaxed); // Fail the next write
        runner.commit_transaction(transaction).await.expect_err("Commit transaction should fail");

        // Ensure the changes did not get applied.
        let part_0_block =
            connect_to_named_protocol_at_dir_root::<fvolume::VolumeMarker>(&part_0_dir, "volume")
                .expect("Failed to open Volume service");
        let (status, guid) = part_0_block.get_type_guid().await.expect("FIDL error");
        assert_eq!(zx::Status::from_raw(status), zx::Status::OK);
        assert_eq!(guid.unwrap().value, PART_TYPE_GUID);
        let part_1_block =
            connect_to_named_protocol_at_dir_root::<fvolume::VolumeMarker>(&part_1_dir, "volume")
                .expect("Failed to open Volume service");
        let metadata =
            part_1_block.get_metadata().await.expect("FIDL error").expect("get_metadata failed");
        assert_eq!(metadata.type_guid.unwrap().value, PART_TYPE_GUID);
        assert_eq!(metadata.flags, Some(0));

        runner.shutdown().await;
    }

    #[fuchsia::test]
    async fn reset_partition_tables() {
        // The test will reset the tables from ["part", "part2"] to
        // ["part3", <empty>, "part4", <125 empty entries>].
        const PART_TYPE_GUID: [u8; 16] = [2u8; 16];
        const PART_1_INSTANCE_GUID: [u8; 16] = [2u8; 16];
        const PART_1_NAME: &str = "part";
        const PART_2_INSTANCE_GUID: [u8; 16] = [3u8; 16];
        const PART_2_NAME: &str = "part2";
        const PART_3_NAME: &str = "part3";
        const PART_4_NAME: &str = "part4";

        let (block_device, partitions_dir) = setup(
            512,
            1048576 / 512,
            vec![
                PartitionInfo {
                    label: PART_1_NAME.to_string(),
                    type_guid: Guid::from_bytes(PART_TYPE_GUID),
                    instance_guid: Guid::from_bytes(PART_1_INSTANCE_GUID),
                    start_block: 4,
                    num_blocks: 1,
                    flags: 0,
                },
                PartitionInfo {
                    label: PART_2_NAME.to_string(),
                    type_guid: Guid::from_bytes(PART_TYPE_GUID),
                    instance_guid: Guid::from_bytes(PART_2_INSTANCE_GUID),
                    start_block: 5,
                    num_blocks: 1,
                    flags: 0,
                },
            ],
        )
        .await;
        let runner = GptManager::new(block_device.connect(), partitions_dir.clone())
            .await
            .expect("load should succeed");
        let nil_entry = PartitionInfo {
            label: "".to_string(),
            type_guid: Guid::from_bytes([0u8; 16]),
            instance_guid: Guid::from_bytes([0u8; 16]),
            start_block: 0,
            num_blocks: 0,
            flags: 0,
        };
        let mut new_partitions = vec![nil_entry; 128];
        new_partitions[0] = PartitionInfo {
            label: PART_3_NAME.to_string(),
            type_guid: Guid::from_bytes(PART_TYPE_GUID),
            instance_guid: Guid::from_bytes([1u8; 16]),
            start_block: 64,
            num_blocks: 2,
            flags: 0,
        };
        new_partitions[2] = PartitionInfo {
            label: PART_4_NAME.to_string(),
            type_guid: Guid::from_bytes(PART_TYPE_GUID),
            instance_guid: Guid::from_bytes([2u8; 16]),
            start_block: 66,
            num_blocks: 4,
            flags: 0,
        };
        runner.reset_partition_table(new_partitions).await.expect("reset_partition_table failed");
        partitions_dir.get_entry("part-000").expect("No entry found");
        partitions_dir.get_entry("part-001").map(|_| ()).expect_err("Extra entry found");
        partitions_dir.get_entry("part-002").expect("No entry found");

        let proxy = vfs::serve_directory(
            partitions_dir.clone(),
            vfs::path::Path::validate_and_split("part-000").unwrap(),
            fio::PERM_READABLE,
        );
        let block =
            connect_to_named_protocol_at_dir_root::<fvolume::VolumeMarker>(&proxy, "volume")
                .expect("Failed to open block service");
        let (status, name) = block.get_name().await.expect("FIDL error");
        assert_eq!(zx::Status::from_raw(status), zx::Status::OK);
        assert_eq!(name.unwrap(), PART_3_NAME);

        runner.shutdown().await;
    }

    #[fuchsia::test]
    async fn reset_partition_tables_fails_if_too_many_partitions() {
        let (block_device, partitions_dir) = setup(512, 8, vec![]).await;
        let runner = GptManager::new(block_device.connect(), partitions_dir.clone())
            .await
            .expect("load should succeed");
        let nil_entry = PartitionInfo {
            label: "".to_string(),
            type_guid: Guid::from_bytes([0u8; 16]),
            instance_guid: Guid::from_bytes([0u8; 16]),
            start_block: 0,
            num_blocks: 0,
            flags: 0,
        };
        let new_partitions = vec![nil_entry; 128];
        runner
            .reset_partition_table(new_partitions)
            .await
            .expect_err("reset_partition_table should fail");

        runner.shutdown().await;
    }

    #[fuchsia::test]
    async fn reset_partition_tables_fails_if_too_large_partitions() {
        let (block_device, partitions_dir) = setup(512, 64, vec![]).await;
        let runner = GptManager::new(block_device.connect(), partitions_dir.clone())
            .await
            .expect("load should succeed");
        let new_partitions = vec![
            PartitionInfo {
                label: "a".to_string(),
                type_guid: Guid::from_bytes([1u8; 16]),
                instance_guid: Guid::from_bytes([1u8; 16]),
                start_block: 4,
                num_blocks: 2,
                flags: 0,
            },
            PartitionInfo {
                label: "b".to_string(),
                type_guid: Guid::from_bytes([2u8; 16]),
                instance_guid: Guid::from_bytes([2u8; 16]),
                start_block: 6,
                num_blocks: 200,
                flags: 0,
            },
        ];
        runner
            .reset_partition_table(new_partitions)
            .await
            .expect_err("reset_partition_table should fail");

        runner.shutdown().await;
    }

    #[fuchsia::test]
    async fn reset_partition_tables_fails_if_partition_overlaps_metadata() {
        let (block_device, partitions_dir) = setup(512, 64, vec![]).await;
        let runner = GptManager::new(block_device.connect(), partitions_dir.clone())
            .await
            .expect("load should succeed");
        let new_partitions = vec![PartitionInfo {
            label: "a".to_string(),
            type_guid: Guid::from_bytes([1u8; 16]),
            instance_guid: Guid::from_bytes([1u8; 16]),
            start_block: 1,
            num_blocks: 2,
            flags: 0,
        }];
        runner
            .reset_partition_table(new_partitions)
            .await
            .expect_err("reset_partition_table should fail");

        runner.shutdown().await;
    }

    #[fuchsia::test]
    async fn reset_partition_tables_fails_if_partitions_overlap() {
        let (block_device, partitions_dir) = setup(512, 64, vec![]).await;
        let runner = GptManager::new(block_device.connect(), partitions_dir.clone())
            .await
            .expect("load should succeed");
        let new_partitions = vec![
            PartitionInfo {
                label: "a".to_string(),
                type_guid: Guid::from_bytes([1u8; 16]),
                instance_guid: Guid::from_bytes([1u8; 16]),
                start_block: 32,
                num_blocks: 2,
                flags: 0,
            },
            PartitionInfo {
                label: "b".to_string(),
                type_guid: Guid::from_bytes([2u8; 16]),
                instance_guid: Guid::from_bytes([2u8; 16]),
                start_block: 33,
                num_blocks: 1,
                flags: 0,
            },
        ];
        runner
            .reset_partition_table(new_partitions)
            .await
            .expect_err("reset_partition_table should fail");

        runner.shutdown().await;
    }

    #[fuchsia::test]
    async fn add_partition() {
        let (block_device, partitions_dir) = setup(512, 64, vec![PartitionInfo::nil(); 64]).await;
        let runner = GptManager::new(block_device.connect(), partitions_dir.clone())
            .await
            .expect("load should succeed");

        let transaction = runner.create_transaction().await.expect("Create transaction failed");
        let request = fpartitions::PartitionsManagerAddPartitionRequest {
            transaction: Some(transaction.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap()),
            name: Some("a".to_string()),
            type_guid: Some(fidl_fuchsia_hardware_block_partition::Guid { value: [1u8; 16] }),
            num_blocks: Some(2),
            ..Default::default()
        };
        runner.add_partition(request).await.expect("add_partition failed");
        runner.commit_transaction(transaction).await.expect("add_partition failed");

        let proxy = vfs::serve_directory(
            partitions_dir.clone(),
            vfs::path::Path::validate_and_split("part-000").unwrap(),
            fio::PERM_READABLE,
        );
        let block =
            connect_to_named_protocol_at_dir_root::<fvolume::VolumeMarker>(&proxy, "volume")
                .expect("Failed to open block service");
        let client = RemoteBlockClient::new(block).await.expect("Failed to create block client");

        assert_eq!(client.block_count(), 2);
        assert_eq!(client.block_size(), 512);

        runner.shutdown().await;
    }

    #[fuchsia::test]
    async fn partition_info() {
        const PART_TYPE_GUID: [u8; 16] = [2u8; 16];
        const PART_INSTANCE_GUID: [u8; 16] = [2u8; 16];
        const PART_NAME: &str = "part";

        let (block_device, partitions_dir) = setup_with_options(
            VmoBackedServerOptions {
                initial_contents: InitialContents::FromCapacity(16),
                block_size: 512,
                info: DeviceInfo::Block(BlockInfo {
                    max_transfer_blocks: NonZero::new(2),
                    device_flags: fblock::Flag::READONLY | fblock::Flag::REMOVABLE,
                    ..Default::default()
                }),
                ..Default::default()
            },
            vec![PartitionInfo {
                label: PART_NAME.to_string(),
                type_guid: Guid::from_bytes(PART_TYPE_GUID),
                instance_guid: Guid::from_bytes(PART_INSTANCE_GUID),
                start_block: 4,
                num_blocks: 1,
                flags: 0xabcd,
            }],
        )
        .await;

        let partitions_dir_clone = partitions_dir.clone();
        let runner = GptManager::new(block_device.connect(), partitions_dir_clone)
            .await
            .expect("load should succeed");

        let part_dir = vfs::serve_directory(
            partitions_dir.clone(),
            vfs::path::Path::validate_and_split("part-000").unwrap(),
            fio::PERM_READABLE,
        );
        let part_block =
            connect_to_named_protocol_at_dir_root::<fvolume::VolumeMarker>(&part_dir, "volume")
                .expect("Failed to open Volume service");
        let info = part_block.get_info().await.expect("FIDL error").expect("get_info failed");
        assert_eq!(info.block_count, 1);
        assert_eq!(info.block_size, 512);
        assert_eq!(info.flags, fblock::Flag::READONLY | fblock::Flag::REMOVABLE);
        assert_eq!(info.max_transfer_size, 1024);

        let metadata =
            part_block.get_metadata().await.expect("FIDL error").expect("get_metadata failed");
        assert_eq!(metadata.name, Some(PART_NAME.to_string()));
        assert_eq!(metadata.type_guid.unwrap().value, PART_TYPE_GUID);
        assert_eq!(metadata.instance_guid.unwrap().value, PART_INSTANCE_GUID);
        assert_eq!(metadata.start_block_offset, Some(4));
        assert_eq!(metadata.num_blocks, Some(1));
        assert_eq!(metadata.flags, Some(0xabcd));

        runner.shutdown().await;
    }

    #[fuchsia::test]
    async fn nested_gpt() {
        const PART_TYPE_GUID: [u8; 16] = [2u8; 16];
        const PART_INSTANCE_GUID: [u8; 16] = [2u8; 16];
        const PART_NAME: &str = "part";

        let vmo = zx::Vmo::create(64 * 512).unwrap();
        let vmo_clone = vmo.create_child(zx::VmoChildOptions::REFERENCE, 0, 0).unwrap();
        let (outer_block_device, outer_partitions_dir) = setup_with_options(
            VmoBackedServerOptions {
                initial_contents: InitialContents::FromVmo(vmo_clone),
                block_size: 512,
                info: DeviceInfo::Block(BlockInfo {
                    device_flags: fblock::Flag::READONLY | fblock::Flag::REMOVABLE,
                    ..Default::default()
                }),
                ..Default::default()
            },
            vec![PartitionInfo {
                label: PART_NAME.to_string(),
                type_guid: Guid::from_bytes(PART_TYPE_GUID),
                instance_guid: Guid::from_bytes(PART_INSTANCE_GUID),
                start_block: 4,
                num_blocks: 16,
                flags: 0xabcd,
            }],
        )
        .await;

        let outer_partitions_dir_clone = outer_partitions_dir.clone();
        let outer_runner =
            GptManager::new(outer_block_device.connect(), outer_partitions_dir_clone)
                .await
                .expect("load should succeed");

        let outer_part_dir = vfs::serve_directory(
            outer_partitions_dir.clone(),
            vfs::path::Path::validate_and_split("part-000").unwrap(),
            fio::PERM_READABLE,
        );
        let part_block =
            connect_to_named_protocol_at_dir_root::<fblock::BlockMarker>(&outer_part_dir, "volume")
                .expect("Failed to open Block service");

        let client = Arc::new(RemoteBlockClient::new(part_block.clone()).await.unwrap());
        let _ = gpt::Gpt::format(
            client,
            vec![PartitionInfo {
                label: PART_NAME.to_string(),
                type_guid: Guid::from_bytes(PART_TYPE_GUID),
                instance_guid: Guid::from_bytes(PART_INSTANCE_GUID),
                start_block: 5,
                num_blocks: 1,
                flags: 0xabcd,
            }],
        )
        .await
        .unwrap();

        let partitions_dir = vfs::directory::immutable::simple();
        let partitions_dir_clone = partitions_dir.clone();
        let runner =
            GptManager::new(part_block, partitions_dir_clone).await.expect("load should succeed");
        let part_dir = vfs::serve_directory(
            partitions_dir.clone(),
            vfs::path::Path::validate_and_split("part-000").unwrap(),
            fio::PERM_READABLE,
        );
        let inner_part_block =
            connect_to_named_protocol_at_dir_root::<fblock::BlockMarker>(&part_dir, "volume")
                .expect("Failed to open Block service");

        let client =
            RemoteBlockClient::new(inner_part_block).await.expect("Failed to create block client");
        assert_eq!(client.block_count(), 1);
        assert_eq!(client.block_size(), 512);

        let buffer = vec![0xaa; 512];
        client.write_at(BufferSlice::Memory(&buffer), 0).await.unwrap();
        client
            .write_at(BufferSlice::Memory(&buffer), 512)
            .await
            .expect_err("Write past end should fail");
        client.flush().await.unwrap();

        runner.shutdown().await;
        outer_runner.shutdown().await;

        // Check that the write targeted the correct block (4 + 5 = 9)
        let data = vmo.read_to_vec(9 * 512, 512).unwrap();
        assert_eq!(&data[..], &buffer[..]);
    }

    #[fuchsia::test]
    async fn offset_map_does_not_allow_partition_overwrite() {
        const PART_TYPE_GUID: [u8; 16] = [2u8; 16];
        const PART_INSTANCE_GUID: [u8; 16] = [2u8; 16];
        const PART_NAME: &str = "part";

        let (block_device, partitions_dir) = setup_with_options(
            VmoBackedServerOptions {
                initial_contents: InitialContents::FromCapacity(16),
                block_size: 512,
                info: DeviceInfo::Block(BlockInfo {
                    device_flags: fblock::Flag::READONLY | fblock::Flag::REMOVABLE,
                    ..Default::default()
                }),
                ..Default::default()
            },
            vec![PartitionInfo {
                label: PART_NAME.to_string(),
                type_guid: Guid::from_bytes(PART_TYPE_GUID),
                instance_guid: Guid::from_bytes(PART_INSTANCE_GUID),
                start_block: 4,
                num_blocks: 2,
                flags: 0xabcd,
            }],
        )
        .await;

        let partitions_dir_clone = partitions_dir.clone();
        let runner = GptManager::new(block_device.connect(), partitions_dir_clone)
            .await
            .expect("load should succeed");

        let part_dir = vfs::serve_directory(
            partitions_dir.clone(),
            vfs::path::Path::validate_and_split("part-000").unwrap(),
            fio::PERM_READABLE,
        );

        let part_block =
            connect_to_named_protocol_at_dir_root::<fblock::BlockMarker>(&part_dir, "volume")
                .expect("Failed to open Block service");

        // Attempting to open a session with an offset map that extends past the end of the device
        // should fail.
        let (session, server_end) = fidl::endpoints::create_proxy::<fblock::SessionMarker>();
        part_block
            .open_session_with_offset_map(
                server_end,
                &fblock::BlockOffsetMapping {
                    source_block_offset: 0,
                    target_block_offset: 1,
                    length: 2,
                },
            )
            .expect("FIDL error");
        session.get_fifo().await.expect_err("Session should be closed");

        let (session, server_end) = fidl::endpoints::create_proxy::<fblock::SessionMarker>();
        part_block
            .open_session_with_offset_map(
                server_end,
                &fblock::BlockOffsetMapping {
                    source_block_offset: 0,
                    target_block_offset: 0,
                    length: 3,
                },
            )
            .expect("FIDL error");
        session.get_fifo().await.expect_err("Session should be closed");

        runner.shutdown().await;
    }
}
