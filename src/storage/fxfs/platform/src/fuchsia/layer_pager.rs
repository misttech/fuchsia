// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Error};
use async_trait::async_trait;
use fidl_fuchsia_storage_block as fblock;
use fuchsia_async as fasync;
use futures::lock::Mutex as AsyncMutex;
use fxfs::filesystem::LayerPager;
use fxfs::object_handle::{LayerObject, ObjectHandle, ReadObjectHandle};
use fxfs::object_store::{DataObjectHandle, FileExtent, ObjectStore};
use fxfs_crypto::UnwrappedKey;
use mapping::{
    Extent, Extents, MAPPING_VMO_SIZE, MappingCommand, PENDING_COMMANDS_CAPACITY, RawMappingCommand,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use storage_device::buffer::{BufferFuture, MutableBufferRef};
use storage_units::BlockSize;
use vmo_fifo::AsyncSender;
use zx;

struct MappedVmo {
    vmo: zx::Vmo,
    vaddr: usize,
    len: usize,
}

impl MappedVmo {
    fn new(vmo: zx::Vmo, len: usize) -> Result<Self, zx::Status> {
        let page_size = zx::system_get_page_size() as usize;
        let aligned_len = len.next_multiple_of(page_size);
        let vaddr = if aligned_len > 0 {
            fuchsia_runtime::vmar_root_self().map(
                0,
                &vmo,
                0,
                aligned_len,
                zx::VmarFlags::PERM_READ,
            )?
        } else {
            0
        };
        Ok(Self { vmo, vaddr, len })
    }

    fn as_slice(&self) -> &[u8] {
        if self.vaddr == 0 || self.len == 0 {
            &[]
        } else {
            // SAFETY: self.vaddr is mapped for at least self.len bytes with PERM_READ.  Note:
            // This creates a `&[u8]` slice over memory backed by a pager-supplied VMO.  In
            // theory, the driver could write to it which is undefined behavior in Rust, but the
            // driver only supplies pages once (immutable) and this trade-off is acceptable for
            // now (we could fix this with kernel changes).
            unsafe { std::slice::from_raw_parts(self.vaddr as *const u8, self.len) }
        }
    }

    fn has_io_error(&self) -> bool {
        self.vmo
            .wait_one(zx::Signals::USER_0, zx::MonotonicInstant::INFINITE_PAST)
            .to_result()
            .is_ok()
    }

    fn purge_cached_data(&self) {
        if self.len > 0 {
            // `DONT_NEED` doesn't actually purge the data immediately; it only serves as a hint
            // to the kernel, which is the best we can do here.
            let _ = self.vmo.op_range(zx::VmoOp::DONT_NEED, 0, self.len as u64);
        }
    }

    fn prefetch(&self, range: std::ops::Range<u64>) {
        if range.end > range.start {
            let _ = self.vmo.op_range(zx::VmoOp::PREFETCH, range.start, range.end - range.start);
        }
    }
}

impl Drop for MappedVmo {
    fn drop(&mut self) {
        if self.vaddr != 0 {
            let page_size = zx::system_get_page_size() as usize;
            let aligned_len = self.len.next_multiple_of(page_size);
            // SAFETY: self.vaddr was mapped by vmar_root_self().map with aligned_len.
            unsafe {
                let _ = fuchsia_runtime::vmar_root_self().unmap(self.vaddr, aligned_len);
            }
        }
    }
}

struct PagedLayerObject {
    handle: DataObjectHandle<ObjectStore>,
    mapped: MappedVmo,
    sender: Arc<AsyncMutex<AsyncSender<RawMappingCommand>>>,
    scope: fasync::ScopeHandle,
    key: u64,
    closed: AtomicBool,
}

impl ObjectHandle for PagedLayerObject {
    fn object_id(&self) -> u64 {
        self.handle.object_id()
    }

    fn block_size(&self) -> BlockSize {
        self.handle.block_size()
    }

    fn allocate_buffer(&self, size: usize) -> BufferFuture<'_> {
        self.handle.allocate_buffer(size)
    }

    fn set_trace(&self, v: bool) {
        self.handle.set_trace(v)
    }
}

#[async_trait]
impl ReadObjectHandle for PagedLayerObject {
    async fn read_aligned(&self, offset: u64, buf: MutableBufferRef<'_>) -> Result<usize, Error> {
        self.handle.read_aligned(offset, buf).await
    }

    fn get_size(&self) -> u64 {
        self.handle.get_size()
    }
}

#[async_trait]
impl LayerObject for PagedLayerObject {
    fn as_slice(&self) -> Option<&[u8]> {
        Some(self.mapped.as_slice())
    }

    fn has_io_error(&self) -> bool {
        self.mapped.has_io_error()
    }

    fn purge_cached_data(&self) {
        self.mapped.purge_cached_data();
    }

    fn prefetch(&self, range: std::ops::Range<u64>) {
        self.mapped.prefetch(range);
    }

    async fn close(&self) {
        if !self.closed.swap(true, Ordering::Relaxed) {
            let mut sender = self.sender.lock().await;
            let _ = sender.push(MappingCommand::CloseBlob { key: self.key }.into()).await;
        }
    }

    fn get_vmo(&self) -> Option<&zx::Vmo> {
        Some(&self.mapped.vmo)
    }
}

impl Drop for PagedLayerObject {
    fn drop(&mut self) {
        // Layers are only explicitly `close()`d during compaction; when a volume is unmounted or
        // locked (or if `LayerData::open` fails), the layer is dropped directly.
        if !*self.closed.get_mut() {
            let sender = self.sender.clone();
            let key = self.key;
            self.scope.spawn(async move {
                let mut sender = sender.lock().await;
                let _ = sender.push(MappingCommand::CloseBlob { key }.into()).await;
            });
        }
    }
}

/// A read-only `LayerObject` backed by a `zx::Vmo` mapped into the current process's address
/// space. Used by remote volume clients to read layer files synchronously via
/// `SyncPersistentLayer`.
pub struct VmoLayerObject {
    mapped: MappedVmo,
    object_id: u64,
    block_size: BlockSize,
}

impl VmoLayerObject {
    pub fn new(vmo: zx::Vmo, object_id: u64, block_size: BlockSize) -> Result<Self, Error> {
        let len = vmo.get_content_size().context("Failed to get VMO content size")? as usize;
        let mapped = MappedVmo::new(vmo, len).context("Failed to map layer VMO")?;
        Ok(Self { mapped, object_id, block_size })
    }
}

impl ObjectHandle for VmoLayerObject {
    fn object_id(&self) -> u64 {
        self.object_id
    }

    fn block_size(&self) -> BlockSize {
        self.block_size
    }

    fn allocate_buffer(&self, _size: usize) -> BufferFuture<'_> {
        unreachable!("VmoLayerObject is slice-backed and does not allocate buffers")
    }
}

#[async_trait]
impl ReadObjectHandle for VmoLayerObject {
    async fn read_aligned(&self, _offset: u64, _buf: MutableBufferRef<'_>) -> Result<usize, Error> {
        unreachable!("VmoLayerObject is slice-backed and does not support read_aligned")
    }

    fn get_size(&self) -> u64 {
        self.mapped.len as u64
    }
}

#[async_trait]
impl LayerObject for VmoLayerObject {
    fn as_slice(&self) -> Option<&[u8]> {
        Some(self.mapped.as_slice())
    }

    fn has_io_error(&self) -> bool {
        self.mapped.has_io_error()
    }

    fn purge_cached_data(&self) {
        self.mapped.purge_cached_data();
    }

    fn prefetch(&self, range: std::ops::Range<u64>) {
        self.mapped.prefetch(range);
    }

    async fn close(&self) {}

    fn get_vmo(&self) -> Option<&zx::Vmo> {
        Some(&self.mapped.vmo)
    }
}

/// Services registration of layer files with a pager backed by the block mapping driver.
pub struct LayerPagerImpl {
    mapper_session: fblock::MapperSessionProxy,
    sender: Arc<AsyncMutex<AsyncSender<RawMappingCommand>>>,
    scope: fasync::Scope,
}

impl LayerPagerImpl {
    pub async fn new(mapper_proxy: &fblock::MapperProxy) -> Result<Self, Error> {
        let mapping_vmo = zx::Vmo::create(MAPPING_VMO_SIZE)?;
        let sender = AsyncSender::<RawMappingCommand>::new(
            mapping_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS)?,
            8,
            PENDING_COMMANDS_CAPACITY,
        )?;

        let (mapper_session, mapper_session_server) = fidl::endpoints::create_proxy();
        mapper_proxy
            .open_session(mapper_session_server, mapping_vmo, None, None)
            .await
            .context("FIDL error on Mapper.OpenSession")?
            .map_err(zx::Status::err_from_raw)
            .context("Failed to open mapper session")?;

        Ok(Self {
            mapper_session,
            sender: Arc::new(AsyncMutex::new(sender)),
            scope: fasync::Scope::new_with_name("layer_pager"),
        })
    }

    async fn register_layer(
        &self,
        size: u64,
        extents: &[FileExtent],
        raw_key: Option<&[u8]>,
    ) -> Result<(u64, zx::Vmo), Error> {
        let mut mapping_extents = Vec::with_capacity(extents.len());
        let mut current_offset = 0u64;
        for ext in extents {
            if ext.logical_offset() > current_offset {
                mapping_extents.push(Extent::try_new(current_offset..ext.logical_offset(), None)?);
            }
            mapping_extents
                .push(Extent::try_new(ext.logical_range(), Some(ext.device_range().start))?);
            current_offset = ext.logical_range().end;
        }
        let aligned_size = size.next_multiple_of(mapping::BLOCK_SIZE);
        if current_offset < aligned_size {
            mapping_extents.push(Extent::try_new(current_offset..aligned_size, None)?);
        }
        let data_extents = Extents::try_new(&mapping_extents, 0)?;
        let extent_count = data_extents.len() as u32;
        let encrypted = raw_key.is_some();
        let key_bytes_len = if encrypted { 32 } else { 0 };
        let allocation_size = (extent_count as usize * 8) + key_bytes_len;

        let (key, vmo) = self
            .mapper_session
            .create_vmo(size, fblock::CreateVmoOptions::SUPPLY_ZEROES_ON_ERROR)
            .await
            .context("FIDL error calling MapperSession.CreateVmo")?
            .map_err(zx::Status::err_from_raw)
            .context("MapperSession.CreateVmo returned error")?;

        if allocation_size > 0 {
            let mut sender = self.sender.lock().await;
            let mut payload = sender.reserve_payload(allocation_size).await?;
            let offset_in_vmo = payload.offset();

            let extent_bytes = extent_count as usize * 8;
            for (mut chunk, val) in payload
                .data()
                .subslice_mut(0..extent_bytes)
                .chunks_mut(8)
                .zip(Extents::encode_extents(&data_extents))
            {
                chunk.copy_from_slice(&val.to_le_bytes());
            }

            if let Some(k) = raw_key {
                payload.data().subslice_mut(extent_bytes..extent_bytes + 32).copy_from_slice(k);
            }

            let command = MappingCommand::Mappings {
                key,
                offset: offset_in_vmo as u32,
                stored_size: size,
                device_offset: 0,
                metadata_count: 0,
                extent_count,
                encrypted,
            };
            payload.commit(command.into()).await?;
        }

        Ok((key, vmo))
    }
}

#[async_trait]
impl LayerPager for LayerPagerImpl {
    async fn open_layer(
        &self,
        handle: DataObjectHandle<ObjectStore>,
        unwrapped_key: Option<UnwrappedKey>,
    ) -> Result<Arc<dyn LayerObject>, Error> {
        let size = handle.get_size();
        if size == 0 || handle.block_size() > BlockSize::SIZE_4KIB {
            return Ok(Arc::new(handle) as Arc<dyn LayerObject>);
        }
        let extents = handle.device_extents().await?;
        let (key, vmo) = self
            .register_layer(size, &extents, unwrapped_key.as_deref().map(|k| k.as_slice()))
            .await?;
        drop(unwrapped_key);
        let mapped = MappedVmo::new(vmo, size as usize).context("Failed to map layer VMO")?;
        Ok(Arc::new(PagedLayerObject {
            handle,
            mapped,
            sender: self.sender.clone(),
            scope: self.scope.to_handle(),
            key,
            closed: AtomicBool::new(false),
        }) as Arc<dyn LayerObject>)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use vmo_fifo::Receiver;

    #[fuchsia::test]
    fn test_mapped_vmo_io_error_signal() {
        let vmo = zx::Vmo::create(4096).unwrap();
        vmo.write(&[0x42u8; 4096], 0).unwrap();
        let vmo_dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();

        let mapped = MappedVmo::new(vmo, 4096).unwrap();
        assert_eq!(mapped.as_slice(), &[0x42u8; 4096]);
        assert!(!mapped.has_io_error());

        vmo_dup.signal(zx::Signals::empty(), zx::Signals::USER_0).unwrap();
        assert!(mapped.has_io_error());
    }

    #[fuchsia::test]
    async fn test_register_layer_large_extent() {
        let (mapper_proxy, mut mapper_stream) =
            fidl::endpoints::create_proxy_and_stream::<fblock::MapperMarker>();

        let large_len = (0x3fff_ffffu64 + 5) * mapping::BLOCK_SIZE;
        let start_dev = 10 * mapping::BLOCK_SIZE;

        let client_task = async move {
            let layer_pager = LayerPagerImpl::new(&mapper_proxy).await.expect("new failed");
            let file_extents = [FileExtent::new(0, start_dev..(start_dev + large_len)).unwrap()];
            let (key, _vmo) = layer_pager
                .register_layer(large_len, &file_extents, None)
                .await
                .expect("register_layer failed");
            assert_eq!(key, 1);
        };

        let server_task = async move {
            let Some(Ok(fblock::MapperRequest::OpenSession {
                session,
                mapping_vmo,
                responder,
                ..
            })) = mapper_stream.next().await
            else {
                panic!("Expected OpenSession");
            };
            responder.send(Ok(())).unwrap();

            let mut session_stream = session.into_stream();
            let Some(Ok(fblock::MapperSessionRequest::CreateVmo { responder, .. })) =
                session_stream.next().await
            else {
                panic!("Expected CreateVmo");
            };
            let vmo = zx::Vmo::create(4096).unwrap();
            responder.send(Ok((1, vmo))).unwrap();

            fasync::unblock(move || {
                let mut receiver =
                    Receiver::<RawMappingCommand>::new(mapping_vmo, PENDING_COMMANDS_CAPACITY)
                        .unwrap();
                let cmd_raw = receiver.peek().expect("peek failed");
                let cmd = MappingCommand::try_from(*cmd_raw).expect("try_from failed");

                let (offset, extent_count) = match cmd {
                    MappingCommand::Mappings {
                        key,
                        offset,
                        stored_size,
                        device_offset,
                        metadata_count,
                        extent_count,
                        encrypted,
                    } => {
                        assert_eq!(key, 1);
                        assert_eq!(stored_size, large_len);
                        assert_eq!(device_offset, 0);
                        assert_eq!(metadata_count, 0);
                        assert_eq!(extent_count, 2);
                        assert!(!encrypted);
                        (offset, extent_count)
                    }
                    _ => panic!("Expected Mappings command"),
                };

                let buffer = cmd_raw.payload_slice(offset, extent_count * 8).to_vec();
                let expected_extents =
                    Extents::try_new([Extent::new(0..large_len, Some(start_dev))], 0).unwrap();
                let mut expected_payload = Vec::new();
                for val in Extents::encode_extents(&expected_extents) {
                    expected_payload.extend_from_slice(&val.to_le_bytes());
                }
                assert_eq!(buffer, expected_payload);
                cmd_raw.pop().expect("pop failed");
            })
            .await;
        };

        futures::join!(client_task, server_task);
    }
}
