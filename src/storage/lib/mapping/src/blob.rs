// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::reader::{BlockService, read_aligned_range};
use crate::{Extents, MappingCommand, PageRequest, RawMappingCommand};
use anyhow::{Error, anyhow};
use bincode::deserialize;
use blob_metadata::{BlobFormat, BlobMetadata};
use delivery_blob::compression::{CompressionAlgorithm, CompressionInfo, StreamingDecompressor};
use fuchsia_sync::Mutex;
use std::cmp::min;
use std::collections::hash_map::{Entry, HashMap};
use std::ops::{ControlFlow, Range};
use std::sync::Arc;
use vmo_fifo::Message;

/// A mapped blob containing extents and decompression metadata.
pub struct Blob {
    extents: Extents,
    uncompressed_size: u64,
    compression_info: Option<Arc<CompressionInfo>>,
}

impl Blob {
    pub fn new(
        extents: Extents,
        uncompressed_size: u64,
        compression_info: Option<CompressionInfo>,
    ) -> Self {
        Self { extents, uncompressed_size, compression_info: compression_info.map(Arc::new) }
    }

    /// Returns the extents mapping logical offsets to device offsets.
    pub fn extents(&self) -> &Extents {
        &self.extents
    }

    /// Returns the uncompressed size of the blob in bytes.
    pub fn uncompressed_size(&self) -> u64 {
        self.uncompressed_size
    }

    /// Returns decompression metadata if the blob is compressed.
    pub fn compression_info(&self) -> Option<&CompressionInfo> {
        self.compression_info.as_deref()
    }

    /// Streams and decodes the uncompressed range requested by `page_request`.
    ///
    /// For uncompressed blobs, both `range.start` and `range.end` must be multiples of
    /// `BLOCK_SIZE`. For compressed blobs, `range.start` must be a multiple of the compression
    /// chunk size, and `range.end` must either be a multiple of the chunk size or equal to
    /// `uncompressed_size`.
    pub fn read_range(
        &self,
        service: &(impl BlockService + ?Sized),
        mut page_request: impl PageRequest,
    ) {
        let range = page_request.range();
        if range.is_empty() {
            return;
        }

        if page_request.prepare(range.clone()).is_err() {
            return;
        }

        match &self.compression_info {
            None => {
                let mut current_offset = range.start;
                let uncompressed_size = self.uncompressed_size;

                read_aligned_range(&self.extents, range, service, move |res| {
                    let Ok(buffer) = res else {
                        return ControlFlow::Break(());
                    };
                    let valid_len =
                        min(buffer.len() as u64, uncompressed_size.saturating_sub(current_offset))
                            as usize;
                    let dest = page_request.mut_ptr_slice().subslice_mut(0..buffer.len());
                    let (mut head, mut tail) = dest.split_at_mut(valid_len);
                    head.copy_from_ptr_slice(buffer.as_ptr_slice().subslice(0..valid_len));
                    tail.fill(0);
                    if page_request.commit(buffer.len()).is_err() {
                        return ControlFlow::Break(());
                    }
                    current_offset += buffer.len() as u64;
                    ControlFlow::Continue(())
                });
            }
            Some(info) => {
                let info = Arc::clone(info);
                let Ok((mut decompressor, aligned_range)) =
                    StreamingDecompressor::new(info, self.uncompressed_size, page_request)
                else {
                    // The range must be out of range. This should be handled when `page_request`
                    // is dropped.
                    return;
                };

                read_aligned_range(&self.extents, aligned_range, service, move |res| {
                    let Ok(buffer) = res else {
                        return ControlFlow::Break(());
                    };
                    if decompressor.push(buffer.as_ptr_slice()).is_err() {
                        return ControlFlow::Break(());
                    }
                    ControlFlow::Continue(())
                });
            }
        }
    }
}

struct LoadingSlot<R> {
    requests: Mutex<Vec<R>>,
}

enum BlobEntry<R> {
    Loading(Arc<LoadingSlot<R>>),
    Loaded(Arc<Blob>),
}

/// A thread-safe registry of active [`Blob`] instances indexed by their Zircon pager port key.
pub struct Blobs<S: ?Sized, F, R> {
    service: Arc<S>,
    request_factory: F,
    map: Mutex<HashMap<u64, BlobEntry<R>>>,
}

impl<S: BlockService + ?Sized, R: PageRequest, F: Fn(u64, Range<u64>) -> R + Send + Sync + 'static>
    Blobs<S, F, R>
{
    /// Creates a new blob registry with the provided block service and request factory.
    pub fn new(service: Arc<S>, request_factory: F) -> Self {
        Self { service, request_factory, map: Mutex::new(HashMap::new()) }
    }

    /// Returns a reference to the block service.
    pub fn service(&self) -> &Arc<S> {
        &self.service
    }

    /// Handles a page request from `PagerThread`.
    ///
    /// If the blob is loaded, reads the range into a newly allocated buffer immediately.
    /// If the blob is currently loading or unmapped, queues the request to be fulfilled
    /// when loaded.
    pub fn handle_page_request(&self, key: u64, range: Range<u64>) {
        let req = (self.request_factory)(key, range);
        let mut map = self.map.lock();
        match map.entry(key) {
            Entry::Occupied(entry) => match entry.get() {
                BlobEntry::Loaded(blob) => {
                    let blob = Arc::clone(blob);
                    drop(map);
                    blob.read_range(self.service.as_ref(), req);
                }
                BlobEntry::Loading(slot) => {
                    slot.requests.lock().push(req);
                }
            },
            Entry::Vacant(entry) => {
                // Page requests can arrive before the corresponding `Mappings` command
                // is processed. We create a loading entry and queue the request so it can be
                // serviced once the mapping arrives and metadata finishes loading.
                //
                // Potential weakness: Unknown keys are unbounded. If an invalid or bogus page
                // request arrives for a key that is never mapped, this entry will remain in
                // memory until the session is dropped.
                let slot = Arc::new(LoadingSlot { requests: Mutex::new(vec![req]) });
                entry.insert(BlobEntry::Loading(slot));
            }
        }
    }

    /// Marks `key` as currently loading metadata, preserving any page requests that arrived
    /// prior to the mapping command.
    pub fn begin_loading(&self, key: u64) {
        let mut map = self.map.lock();
        map.entry(key).or_insert_with(|| {
            BlobEntry::Loading(Arc::new(LoadingSlot { requests: Mutex::new(Vec::new()) }))
        });
    }

    /// Inserts a blob into the registry under `key`, immediately draining and servicing any
    /// page requests that arrived while metadata was loading.
    fn insert(&self, key: u64, blob: Arc<Blob>) {
        let reqs = {
            let mut map = self.map.lock();
            let prev = map.insert(key, BlobEntry::Loaded(blob.clone()));
            match prev {
                Some(BlobEntry::Loading(slot)) => std::mem::take(&mut *slot.requests.lock()),
                _ => Vec::new(),
            }
        };

        for req in reqs {
            blob.read_range(self.service.as_ref(), req);
        }
    }

    /// Removes the blob registered under `key`.
    pub fn remove(&self, key: u64) {
        self.map.lock().remove(&key);
    }

    /// Returns `true` if `key` is currently in the loading state.
    #[cfg(test)]
    pub fn is_loading(&self, key: u64) -> bool {
        matches!(self.map.lock().get(&key), Some(BlobEntry::Loading(_)))
    }

    /// Returns `true` if `key` is currently in the loaded state.
    #[cfg(test)]
    pub fn is_loaded(&self, key: u64) -> bool {
        matches!(self.map.lock().get(&key), Some(BlobEntry::Loaded(_)))
    }
}

/// Reads the blob metadata from `metadata_extents` using `service` and constructs the blob's
/// uncompressed size and optional [`CompressionInfo`], invoking `callback` upon success.
///
/// On failure or if the operation is aborted, `callback` is dropped without being invoked.
pub fn read_blob_metadata(
    service: &(impl BlockService + ?Sized),
    metadata_extents: &Extents,
    stored_data_size: u64,
    callback: impl FnOnce((u64, Option<CompressionInfo>)) + Send + 'static,
) {
    let mut total_metadata_len = 0u64;
    for extent in metadata_extents.iter_extents(0) {
        total_metadata_len += extent.len();
    }
    if total_metadata_len == 0 {
        callback((stored_data_size, None));
        return;
    }

    let mut callback = Some(callback);
    let mut metadata_bytes = Some(Vec::with_capacity(total_metadata_len as usize));
    read_aligned_range(metadata_extents, 0..total_metadata_len, service, move |res| {
        let Ok(buffer) = res else {
            log::error!(error:? = res.unwrap_err(); "Failed to read metadata blocks");
            return ControlFlow::Break(());
        };
        let bytes = metadata_bytes.as_mut().unwrap();
        let copy_len = min(buffer.len(), total_metadata_len as usize - bytes.len());
        buffer.as_ptr_slice().subslice(0..copy_len).append_to(bytes);
        if bytes.len() < total_metadata_len as usize {
            return ControlFlow::Continue(());
        }

        let metadata_bytes = metadata_bytes.take().unwrap();
        let cb = callback.take().unwrap();
        let metadata = match deserialize::<BlobMetadata>(&metadata_bytes) {
            Ok(metadata) => metadata,
            Err(error) => {
                log::error!(error:?; "Failed to deserialize BlobMetadata");
                return ControlFlow::Break(());
            }
        };

        let res = match metadata.format {
            BlobFormat::Uncompressed => (stored_data_size, None),
            BlobFormat::ChunkedZstd { uncompressed_size, chunk_size, compressed_offsets } => {
                match CompressionInfo::new(
                    chunk_size,
                    stored_data_size,
                    &compressed_offsets,
                    CompressionAlgorithm::Zstd,
                ) {
                    Ok(info) => (uncompressed_size, Some(info)),
                    Err(error) => {
                        log::error!(error:?; "Failed to parse Zstd CompressionInfo");
                        return ControlFlow::Break(());
                    }
                }
            }
            BlobFormat::ChunkedLz4 { uncompressed_size, chunk_size, compressed_offsets } => {
                match CompressionInfo::new(
                    chunk_size,
                    stored_data_size,
                    &compressed_offsets,
                    CompressionAlgorithm::Lz4,
                ) {
                    Ok(info) => (uncompressed_size, Some(info)),
                    Err(error) => {
                        log::error!(error:?; "Failed to parse Lz4 CompressionInfo");
                        return ControlFlow::Break(());
                    }
                }
            }
        };
        cb(res);
        ControlFlow::Break(())
    });
}

/// RAII guard that manages the lifecycle of a blob transitioning from loading metadata to loaded.
///
/// When metadata is being fetched asynchronously from storage, the blob entry in [`Blobs`]
/// remains in the [`BlobEntry::Loading`] state, accumulating incoming page requests in its queue.
///
/// - On success: [`LoadingBlobGuard::commit`] consumes the guard, stores the fully initialized
///   [`Blob`], and immediately drains and fulfills all queued page requests.
/// - On failure or cancellation: If dropped before `commit` is called (e.g. due to storage I/O
///   error, corrupted metadata, or session teardown), the `Drop` implementation cleans up the
///   entry by removing `key` from [`Blobs`]. Dropping the loading slot drops all queued
///   [`PageRequest`] objects, which fails the pending page requests in the kernel pager.
struct LoadingBlobGuard<
    S: BlockService + ?Sized + 'static,
    R: PageRequest,
    F: Fn(u64, Range<u64>) -> R + Send + Sync + 'static,
> {
    blobs: Option<Arc<Blobs<S, F, R>>>,
    key: u64,
}

impl<
    S: BlockService + ?Sized + 'static,
    R: PageRequest,
    F: Fn(u64, Range<u64>) -> R + Send + Sync + 'static,
> LoadingBlobGuard<S, R, F>
{
    /// Commits the loaded blob to the registry, transferring ownership and draining all queued
    /// page requests.
    fn commit(mut self, blob: Arc<Blob>) {
        self.blobs.take().unwrap().insert(self.key, blob);
    }
}

impl<
    S: BlockService + ?Sized + 'static,
    R: PageRequest,
    F: Fn(u64, Range<u64>) -> R + Send + Sync + 'static,
> Drop for LoadingBlobGuard<S, R, F>
{
    fn drop(&mut self) {
        if let Some(blobs) = self.blobs.take() {
            blobs.remove(self.key);
        }
    }
}

/// Processes a raw mapping command (`RawMappingCommand`), decoding extent descriptors,
/// reading blob metadata from storage, and inserting/removing the blob from `blobs`.
pub fn process_mapping_command<
    S: BlockService + ?Sized + 'static,
    R: PageRequest,
    F: Fn(u64, Range<u64>) -> R + Send + Sync + 'static,
>(
    msg: &Message<'_, RawMappingCommand>,
    blobs: &Arc<Blobs<S, F, R>>,
) -> Result<(), Error> {
    let cmd = **msg;
    match MappingCommand::try_from(cmd)? {
        MappingCommand::Mappings { key, offset, metadata_count, blob_count } => {
            let blob_bytes_len = (blob_count as usize)
                .checked_mul(8)
                .ok_or_else(|| anyhow!("Overflow calculating blob extent byte length"))?;
            let metadata_bytes_len = (metadata_count as usize)
                .checked_mul(8)
                .ok_or_else(|| anyhow!("Overflow calculating metadata extent byte length"))?;
            let total_bytes_len = blob_bytes_len
                .checked_add(metadata_bytes_len)
                .ok_or_else(|| anyhow!("Overflow calculating total extent byte length"))?;
            let payload_len: u32 = total_bytes_len
                .try_into()
                .map_err(|_| anyhow!("Extent byte length exceeds u32"))?;

            let payload_bytes = msg.payload_slice(offset, payload_len);
            let data_bytes = payload_bytes.subslice(0..blob_bytes_len);
            let metadata_bytes = payload_bytes.subslice(blob_bytes_len..total_bytes_len);
            let data_extents = Extents::from_encoded(data_bytes.iter_as::<u64>())
                .ok_or_else(|| anyhow!("Failed to decode data extents"))?;
            let metadata_extents = Extents::from_encoded(metadata_bytes.iter_as::<u64>())
                .ok_or_else(|| anyhow!("Failed to decode metadata extents"))?;

            let stored_data_size = data_extents.iter_extents(0).map(|e| e.len()).sum::<u64>();
            blobs.begin_loading(key);
            let service = blobs.service().clone();
            let guard = LoadingBlobGuard { blobs: Some(blobs.clone()), key };
            read_blob_metadata(
                service.as_ref(),
                &metadata_extents,
                stored_data_size,
                move |(uncompressed_size, compression_info)| {
                    let blob =
                        Arc::new(Blob::new(data_extents, uncompressed_size, compression_info));
                    guard.commit(blob);
                },
            );
            Ok(())
        }
        MappingCommand::CloseBlob { key } => {
            blobs.remove(key);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::OwnedBuffer;
    use crate::reader::tests::FakeBlockService;
    use crate::testing::TestVecBuffer;
    use crate::{BLOCK_SIZE, Extent};
    use anyhow::Error;
    use delivery_blob::compression::{ChunkedArchiveOptions, CompressionAlgorithm};
    use std::sync::Arc;

    #[test]
    fn test_read_range_uncompressed() {
        let block_count = 8;
        let mut expected_data = vec![0u8; (block_count as u64 * BLOCK_SIZE) as usize];
        for (i, byte) in expected_data.iter_mut().enumerate() {
            *byte = (i % 255) as u8;
        }
        let service = FakeBlockService::new(expected_data.clone());

        let extents = Extents::encode_extents(&[Extent::new(0..(8 * BLOCK_SIZE), Some(0))]);
        let extents = Extents::from_encoded(extents).unwrap();
        let blob = Arc::new(Blob::new(extents, 8 * BLOCK_SIZE, None));

        let (page_request, rx) = TestVecBuffer::new_with_range(0..(8 * BLOCK_SIZE));
        blob.read_range(&service, page_request);

        assert_eq!(rx.commits(), vec![(0, (8 * BLOCK_SIZE) as usize)]);
        assert_eq!(rx.output(), expected_data);
    }

    #[test]
    fn test_read_range_compressed_zstd() {
        let uncompressed_size = 32768 * 2 + 1024;
        let mut uncompressed_data = vec![0u8; uncompressed_size];
        for (i, byte) in uncompressed_data.iter_mut().enumerate() {
            *byte = ((i * 7) % 255) as u8;
        }

        let options =
            ChunkedArchiveOptions::V3 { compression_algorithm: CompressionAlgorithm::Zstd };
        let archive =
            delivery_blob::compression::ChunkedArchive::new(&uncompressed_data, options).unwrap();

        let mut compressed_offsets = vec![0];
        let mut compressed_data = vec![];
        for chunk in archive.chunks() {
            compressed_data.extend_from_slice(&chunk.compressed_data);
            compressed_offsets.push(compressed_data.len() as u64);
        }
        compressed_offsets.pop();

        let chunk_size = archive.chunk_size();
        let stored_size = compressed_data.len() as u64;
        let stored_blocks = stored_size.div_ceil(BLOCK_SIZE);
        let mut device_data = vec![0u8; (stored_blocks * BLOCK_SIZE) as usize];
        device_data[..compressed_data.len()].copy_from_slice(&compressed_data);
        let service = FakeBlockService::new(device_data);

        let extents =
            Extents::encode_extents(&[Extent::new(0..(stored_blocks * BLOCK_SIZE), Some(0))]);
        let extents = Extents::from_encoded(extents).unwrap();
        let compression_info = CompressionInfo::new(
            chunk_size as u64,
            stored_size,
            &compressed_offsets,
            CompressionAlgorithm::Zstd,
        )
        .unwrap();
        let blob = Arc::new(Blob::new(extents, uncompressed_size as u64, Some(compression_info)));

        let dest_alloc_size = uncompressed_size.next_multiple_of(chunk_size);
        let (mut page_request, rx) = TestVecBuffer::new_with_range(0..(uncompressed_size as u64));
        page_request.data.resize(dest_alloc_size, 0);
        blob.read_range(&service, page_request);

        assert_eq!(
            rx.commits(),
            vec![
                (0, chunk_size),
                (chunk_size as u64, chunk_size),
                (chunk_size as u64 * 2, chunk_size)
            ]
        );
        assert_eq!(&rx.output()[..uncompressed_size], &uncompressed_data[..]);
    }

    #[test]
    fn test_read_range_compressed_lz4_split_across_buffers() {
        let uncompressed_size = 32768 * 2;
        let mut uncompressed_data = vec![0u8; uncompressed_size];
        for (i, byte) in uncompressed_data.iter_mut().enumerate() {
            *byte = ((i * 13) % 255) as u8;
        }

        let options =
            ChunkedArchiveOptions::V3 { compression_algorithm: CompressionAlgorithm::Lz4 };
        let archive =
            delivery_blob::compression::ChunkedArchive::new(&uncompressed_data, options).unwrap();

        let mut compressed_offsets = vec![0];
        let mut compressed_data = vec![];
        for chunk in archive.chunks() {
            compressed_data.extend_from_slice(&chunk.compressed_data);
            compressed_offsets.push(compressed_data.len() as u64);
        }
        compressed_offsets.pop();

        let chunk_size = archive.chunk_size();
        let stored_size = compressed_data.len() as u64;
        let stored_blocks = stored_size.div_ceil(BLOCK_SIZE);
        let mut device_data = vec![0u8; (stored_blocks * BLOCK_SIZE) as usize];
        device_data[..compressed_data.len()].copy_from_slice(&compressed_data);

        // Force a small block allocation limit (e.g. 4096 bytes) so that read_aligned_range
        // splits the compressed chunks across multiple consecutive OwnedBuffers!
        let service = FakeBlockService::new_with_cap(device_data, Some(4096));

        let extents =
            Extents::encode_extents(&[Extent::new(0..(stored_blocks * BLOCK_SIZE), Some(0))]);
        let extents = Extents::from_encoded(extents).unwrap();
        let compression_info = CompressionInfo::new(
            chunk_size as u64,
            stored_size,
            &compressed_offsets,
            CompressionAlgorithm::Lz4,
        )
        .unwrap();
        let blob = Arc::new(Blob::new(extents, uncompressed_size as u64, Some(compression_info)));

        let (page_request, rx) = TestVecBuffer::new_with_range(0..(uncompressed_size as u64));
        blob.read_range(&service, page_request);

        assert_eq!(rx.commits(), vec![(0, chunk_size), (chunk_size as u64, chunk_size)]);
        assert_eq!(rx.output(), uncompressed_data);
    }

    #[test]
    fn test_read_range_invalid_range_noop() {
        let service = FakeBlockService::new(vec![0u8; 8192]);
        let extents = Extents::encode_extents(&[Extent::new(0..8192, Some(0))]);
        let extents = Extents::from_encoded(extents).unwrap();
        let blob = Arc::new(Blob::new(extents, 8192, None));

        let (page_request, rx) = TestVecBuffer::new_with_range(4096..4096);
        // start >= end should be a no-op returning Ok(())
        blob.read_range(&service, page_request);
        assert_eq!(rx.commits().len(), 0);
    }

    #[test]
    fn test_blob_getters() {
        let extents_raw = Extents::encode_extents(&[Extent::new(0..8192, Some(0))]);
        let extents = Extents::from_encoded(extents_raw.clone()).unwrap();
        let uncompressed_size = 8192u64;

        let blob_uncompressed = Blob::new(extents, uncompressed_size, None);
        assert_eq!(blob_uncompressed.uncompressed_size(), 8192);
        assert!(blob_uncompressed.compression_info().is_none());

        let compression_info =
            CompressionInfo::new(32768, 4096, &[0], CompressionAlgorithm::Zstd).unwrap();
        let blob_compressed = Blob::new(
            Extents::from_encoded(extents_raw).unwrap(),
            uncompressed_size,
            Some(compression_info),
        );
        assert!(blob_compressed.compression_info().is_some());
    }

    #[test]
    fn test_read_range_block_service_error_returns_err() {
        struct FailingBlockService;
        impl BlockService for FailingBlockService {
            fn allocate_buffer(&self, max_len: usize) -> storage_device::buffer::OwnedBuffer {
                FakeBlockService::new(vec![0u8; max_len]).allocate_buffer(max_len)
            }
            fn read_blocks(
                &self,
                _device_offset: u64,
                _dest_buffer: storage_device::buffer::OwnedBuffer,
                _on_complete: Box<
                    dyn FnOnce(Result<storage_device::buffer::OwnedBuffer, Error>) + Send,
                >,
            ) -> Result<(), Error> {
                Err(anyhow::anyhow!("block read failure"))
            }
        }

        let extents = Extents::encode_extents(&[Extent::new(0..8192, Some(0))]);
        let extents = Extents::from_encoded(extents).unwrap();
        let blob = Blob::new(extents, 8192, None);

        let (page_request, rx) = TestVecBuffer::new_with_range(0..8192);

        blob.read_range(&FailingBlockService, page_request);
        assert_eq!(rx.commits().len(), 0);
    }

    #[test]
    fn test_read_range_uncompressed_multi_chunk() {
        let block_count = 4;
        let mut expected_data = vec![0u8; (block_count as u64 * BLOCK_SIZE) as usize];
        for (i, byte) in expected_data.iter_mut().enumerate() {
            *byte = ((i * 11) % 255) as u8;
        }
        // Force capping to 4096 bytes per buffer allocation so read_range processes
        // 4 separate chunks.
        let service = FakeBlockService::new_with_cap(expected_data.clone(), Some(4096));

        let extents =
            Extents::encode_extents(&[Extent::new(0..(block_count * BLOCK_SIZE), Some(0))]);
        let extents = Extents::from_encoded(extents).unwrap();
        let blob = Blob::new(extents, block_count * BLOCK_SIZE, None);

        let (page_request, rx) = TestVecBuffer::new_with_range(0..(block_count * BLOCK_SIZE));
        blob.read_range(&service, page_request);

        assert_eq!(rx.commits().len(), 4);
        assert_eq!(rx.output(), expected_data);
    }

    #[test]
    fn test_read_range_uncompressed_unaligned_uncompressed_size() {
        let uncompressed_size = 5000u64;
        let mut expected_data = vec![0u8; 8192];
        for (i, byte) in expected_data.iter_mut().enumerate() {
            *byte = (i % 251) as u8;
        }
        let service = FakeBlockService::new(expected_data.clone());

        let extents = Extents::encode_extents(&[Extent::new(0..8192, Some(0))]);
        let extents = Extents::from_encoded(extents).unwrap();
        let blob = Blob::new(extents, uncompressed_size, None);

        let (page_request, rx) = TestVecBuffer::new_with_range(0..8192);
        blob.read_range(&service, page_request);

        assert_eq!(rx.commits(), vec![(0, 8192)]);
        assert_eq!(&rx.output()[..5000], &expected_data[..5000]);
    }

    #[test]
    fn test_read_range_compressed_tail_chunk_only() {
        let uncompressed_size = 32768 * 2 + 1024;
        let mut uncompressed_data = vec![0u8; uncompressed_size];
        for (i, byte) in uncompressed_data.iter_mut().enumerate() {
            *byte = ((i * 7) % 251) as u8;
        }

        let options =
            ChunkedArchiveOptions::V3 { compression_algorithm: CompressionAlgorithm::Zstd };
        let archive =
            delivery_blob::compression::ChunkedArchive::new(&uncompressed_data, options).unwrap();

        let mut compressed_offsets = vec![0];
        let mut compressed_data = vec![];
        for chunk in archive.chunks() {
            compressed_data.extend_from_slice(&chunk.compressed_data);
            compressed_offsets.push(compressed_data.len() as u64);
        }
        compressed_offsets.pop();

        let chunk_size = archive.chunk_size();
        let stored_size = compressed_data.len() as u64;
        let stored_blocks = stored_size.div_ceil(BLOCK_SIZE);
        let mut device_data = vec![0u8; (stored_blocks * BLOCK_SIZE) as usize];
        device_data[..compressed_data.len()].copy_from_slice(&compressed_data);
        let service = FakeBlockService::new(device_data);

        let extents =
            Extents::encode_extents(&[Extent::new(0..(stored_blocks * BLOCK_SIZE), Some(0))]);
        let extents = Extents::from_encoded(extents).unwrap();
        let compression_info = CompressionInfo::new(
            chunk_size as u64,
            stored_size,
            &compressed_offsets,
            CompressionAlgorithm::Zstd,
        )
        .unwrap();
        let blob = Blob::new(extents, uncompressed_size as u64, Some(compression_info));

        let tail_start = chunk_size as u64 * 2;
        let (mut page_request, rx) =
            TestVecBuffer::new_with_range(tail_start..(uncompressed_size as u64));
        page_request.data.resize(32768, 0);
        blob.read_range(&service, page_request);

        assert_eq!(rx.commits(), vec![(tail_start, chunk_size)]);
        assert_eq!(&rx.output()[..1024], &uncompressed_data[65536..]);
    }

    #[test]
    fn test_read_range_compressed_partial_final_chunk_zero_tail() {
        let uncompressed_size = 32768 + 1024;
        let mut uncompressed_data = vec![0u8; uncompressed_size];
        for (i, byte) in uncompressed_data.iter_mut().enumerate() {
            *byte = ((i * 13) % 251) as u8;
        }

        let options =
            ChunkedArchiveOptions::V3 { compression_algorithm: CompressionAlgorithm::Zstd };
        let archive =
            delivery_blob::compression::ChunkedArchive::new(&uncompressed_data, options).unwrap();

        let mut compressed_offsets = vec![0];
        let mut compressed_data = vec![];
        for chunk in archive.chunks() {
            compressed_data.extend_from_slice(&chunk.compressed_data);
            compressed_offsets.push(compressed_data.len() as u64);
        }
        compressed_offsets.pop();

        let chunk_size = archive.chunk_size();
        let stored_size = compressed_data.len() as u64;
        let stored_blocks = stored_size.div_ceil(BLOCK_SIZE);
        let mut device_data = vec![0u8; (stored_blocks * BLOCK_SIZE) as usize];
        device_data[..compressed_data.len()].copy_from_slice(&compressed_data);
        let service = FakeBlockService::new(device_data);

        let extents =
            Extents::encode_extents(&[Extent::new(0..(stored_blocks * BLOCK_SIZE), Some(0))]);
        let extents = Extents::from_encoded(extents).unwrap();
        let compression_info = CompressionInfo::new(
            chunk_size as u64,
            stored_size,
            &compressed_offsets,
            CompressionAlgorithm::Zstd,
        )
        .unwrap();
        let blob = Blob::new(extents, uncompressed_size as u64, Some(compression_info));

        // Pre-fill destination buffer with 0xFF bytes to verify tail zeroing
        let (mut page_request, rx) = TestVecBuffer::new_with_range(0..(uncompressed_size as u64));
        page_request.data.resize(65536, 0);
        page_request.data.fill(0xFF);
        blob.read_range(&service, page_request);

        assert_eq!(rx.commits(), vec![(0, chunk_size), (chunk_size as u64, chunk_size)]);
        assert_eq!(&rx.output()[..uncompressed_size], &uncompressed_data[..]);
        assert_eq!(&rx.output()[uncompressed_size..65536], &[0u8; 31744]);
    }

    #[test]
    fn test_blobs_registry() {
        let extents = Extents::encode_extents(&[Extent::new(0..4096, Some(0))]);
        let extents = Extents::from_encoded(extents).unwrap();
        let blob = Arc::new(Blob::new(extents, 4096, None));
        let service = Arc::new(FakeBlockService::new(vec![0u8; 4096]));
        let blobs = Blobs::new(service, |_key, _range| TestVecBuffer::new(4096).0);

        assert!(!blobs.is_loading(100));
        blobs.begin_loading(100);
        assert!(blobs.is_loading(100));

        blobs.insert(100, blob.clone());
        assert!(!blobs.is_loading(100));
        blobs.remove(100);
        assert!(!blobs.is_loading(100));
    }

    struct DelayedBlockService {
        device_data: Vec<u8>,
        pending: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
    }

    impl DelayedBlockService {
        fn new(device_data: Vec<u8>) -> Arc<Self> {
            Arc::new(Self { device_data, pending: Mutex::new(Vec::new()) })
        }

        fn wait_and_trigger_sync(&self) {
            loop {
                let callbacks = std::mem::take(&mut *self.pending.lock());
                if !callbacks.is_empty() {
                    for cb in callbacks {
                        cb();
                    }
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }

    impl BlockService for DelayedBlockService {
        fn allocate_buffer(&self, max_len: usize) -> OwnedBuffer {
            FakeBlockService::new(vec![0u8; max_len]).allocate_buffer(max_len)
        }

        fn read_blocks(
            &self,
            device_offset: u64,
            mut dest_buffer: OwnedBuffer,
            on_complete: Box<dyn FnOnce(Result<OwnedBuffer, Error>) + Send>,
        ) -> Result<(), Error> {
            let start = device_offset as usize;
            let end = (start + dest_buffer.len()).min(self.device_data.len());
            dest_buffer
                .as_mut_ptr_slice()
                .subslice_mut(0..end - start)
                .copy_from_slice(&self.device_data[start..end]);
            self.pending.lock().push(Box::new(move || {
                on_complete(Ok(dest_buffer));
            }));
            Ok(())
        }
    }

    #[fuchsia::test]
    fn test_read_blob_metadata_callback() {
        let uncompressed_size = 65536u64;
        let chunk_size = 32768u64;
        let compressed_offsets = vec![0u64, 1200u64];
        let metadata = BlobMetadata {
            merkle_leaves: vec![],
            format: BlobFormat::ChunkedZstd {
                uncompressed_size,
                chunk_size,
                compressed_offsets: compressed_offsets.clone(),
            },
        };
        let encoded_metadata = bincode::serialize(&metadata).unwrap();
        let mut device_data = vec![0u8; BLOCK_SIZE as usize];
        device_data[..encoded_metadata.len()].copy_from_slice(&encoded_metadata);

        let service = DelayedBlockService::new(device_data);
        let metadata_extents =
            Extents::from_encoded(Extents::encode_extents(&[Extent::new(0..BLOCK_SIZE, Some(0))]))
                .unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        read_blob_metadata(service.as_ref(), &metadata_extents, 1200, move |res| {
            let _ = tx.send(res);
        });

        // Trigger delayed storage read completion.
        service.wait_and_trigger_sync();

        let (size, info) = rx.recv().unwrap();
        assert_eq!(size, uncompressed_size);
        assert!(info.is_some());
    }

    #[fuchsia::test]
    fn test_read_blob_metadata_error_drops_callback() {
        // Corrupt CompressionInfo where compressed_offsets are invalid (descending)
        let metadata = BlobMetadata {
            merkle_leaves: vec![],
            format: BlobFormat::ChunkedZstd {
                uncompressed_size: 65536,
                chunk_size: 32768,
                compressed_offsets: vec![1000, 500],
            },
        };
        let encoded_metadata = bincode::serialize(&metadata).unwrap();
        let mut device_data = vec![0u8; BLOCK_SIZE as usize];
        device_data[..encoded_metadata.len()].copy_from_slice(&encoded_metadata);

        let service = DelayedBlockService::new(device_data);
        let metadata_extents =
            Extents::from_encoded(Extents::encode_extents(&[Extent::new(0..BLOCK_SIZE, Some(0))]))
                .unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        read_blob_metadata(service.as_ref(), &metadata_extents, 1200, move |res| {
            let _ = tx.send(res);
        });

        service.wait_and_trigger_sync();

        // Callback should have been dropped on error without sending a message.
        assert!(rx.recv().is_err());
    }

    #[fuchsia::test]
    fn test_read_blob_metadata_corrupt_bincode_drops_callback() {
        // Corrupt random bytes that cannot be deserialized as BlobMetadata
        let device_data = vec![0xFFu8; BLOCK_SIZE as usize];
        let service = DelayedBlockService::new(device_data);
        let metadata_extents =
            Extents::from_encoded(Extents::encode_extents(&[Extent::new(0..BLOCK_SIZE, Some(0))]))
                .unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        read_blob_metadata(service.as_ref(), &metadata_extents, 1200, move |res| {
            let _ = tx.send(res);
        });

        service.wait_and_trigger_sync();

        // Callback should have been dropped on error without sending a message.
        assert!(rx.recv().is_err());
    }

    #[fuchsia::test]
    fn test_process_mapping_command_error_cleans_up_blob() {
        let metadata = BlobMetadata {
            merkle_leaves: vec![],
            format: BlobFormat::ChunkedZstd {
                uncompressed_size: 65536,
                chunk_size: 32768,
                compressed_offsets: vec![1000, 500],
            },
        };
        let encoded_metadata = bincode::serialize(&metadata).unwrap();
        let mut device_data = vec![0u8; (2 * BLOCK_SIZE) as usize];
        device_data[BLOCK_SIZE as usize..BLOCK_SIZE as usize + encoded_metadata.len()]
            .copy_from_slice(&encoded_metadata);

        let service = DelayedBlockService::new(device_data);
        let blobs = Arc::new(Blobs::new(service.clone(), |_k, _r| TestVecBuffer::new(4096).0));

        let data_extent_words = Extents::encode_extents(&[Extent::new(0..BLOCK_SIZE, Some(0))]);
        let meta_extent_words =
            Extents::encode_extents(&[Extent::new(0..BLOCK_SIZE, Some(BLOCK_SIZE))]);
        let mut payload_bytes = Vec::new();
        for w in data_extent_words.iter().chain(meta_extent_words.iter()) {
            payload_bytes.extend_from_slice(&w.to_le_bytes());
        }

        let vmo = zx::Vmo::create(65536).unwrap();
        let mut sender = vmo_fifo::SyncSender::<crate::RawMappingCommand>::new(
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            1024,
            16,
        )
        .unwrap();
        let mut payload_buf = sender.reserve_payload(payload_bytes.len()).unwrap();
        payload_buf.data().copy_from_slice(&payload_bytes);
        let cmd = crate::RawMappingCommand {
            opcode: crate::MAPPINGS_COMMAND,
            offset: payload_buf.offset(),
            key: 99,
            metadata_count: 1,
            blob_count: 1,
        };
        payload_buf.commit(cmd).unwrap();
        let mut receiver = vmo_fifo::Receiver::<crate::RawMappingCommand>::new(vmo, 16).unwrap();

        let msg = receiver.peek().unwrap();
        process_mapping_command(&msg, &blobs).unwrap();

        // Blob is initially in Loading state.
        assert!(blobs.is_loading(99));

        // Complete metadata read (which will fail due to corrupt CompressionInfo).
        service.wait_and_trigger_sync();

        // Blobs map should have cleaned up the failed entry on drop.
        assert!(!blobs.is_loading(99));
    }

    #[fuchsia::test]
    fn test_loading_page_request_buffer_dropped_on_failure() {
        use delivery_blob::compression::{ChunkedArchiveError, DataBuffer};
        use std::sync::atomic::{AtomicBool, Ordering};
        use storage_ptr_slice::MutPtrByteSlice;

        struct DroppingBuffer {
            data: Vec<u8>,
            range: Range<u64>,
            dropped: Arc<AtomicBool>,
        }
        impl Drop for DroppingBuffer {
            fn drop(&mut self) {
                self.dropped.store(true, Ordering::Relaxed);
            }
        }
        impl DataBuffer for DroppingBuffer {
            fn range(&self) -> Range<u64> {
                self.range.clone()
            }
            fn mut_ptr_slice(&mut self) -> MutPtrByteSlice<'_> {
                MutPtrByteSlice::from(&mut self.data[..])
            }
            fn commit(&mut self, _size: usize) -> Result<(), ChunkedArchiveError> {
                Ok(())
            }
        }
        impl PageRequest for DroppingBuffer {
            fn prepare(&mut self, _range: Range<u64>) -> Result<(), ChunkedArchiveError> {
                Ok(())
            }
        }

        let metadata = BlobMetadata {
            merkle_leaves: vec![],
            format: BlobFormat::ChunkedZstd {
                uncompressed_size: 65536,
                chunk_size: 32768,
                compressed_offsets: vec![1000, 500],
            },
        };
        let encoded_metadata = bincode::serialize(&metadata).unwrap();
        let mut device_data = vec![0u8; (2 * BLOCK_SIZE) as usize];
        device_data[BLOCK_SIZE as usize..BLOCK_SIZE as usize + encoded_metadata.len()]
            .copy_from_slice(&encoded_metadata);

        let service = DelayedBlockService::new(device_data);
        let dropped = Arc::new(AtomicBool::new(false));
        let dropped_clone = dropped.clone();
        let blobs = Arc::new(Blobs::new(service.clone(), move |_k, r| DroppingBuffer {
            data: vec![0u8; 4096],
            range: r,
            dropped: dropped_clone.clone(),
        }));

        let data_extent_words = Extents::encode_extents(&[Extent::new(0..BLOCK_SIZE, Some(0))]);
        let meta_extent_words =
            Extents::encode_extents(&[Extent::new(0..BLOCK_SIZE, Some(BLOCK_SIZE))]);
        let mut payload_bytes = Vec::new();
        for w in data_extent_words.iter().chain(meta_extent_words.iter()) {
            payload_bytes.extend_from_slice(&w.to_le_bytes());
        }

        let vmo = zx::Vmo::create(65536).unwrap();
        let mut sender = vmo_fifo::SyncSender::<crate::RawMappingCommand>::new(
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            1024,
            16,
        )
        .unwrap();
        let mut payload_buf = sender.reserve_payload(payload_bytes.len()).unwrap();
        payload_buf.data().copy_from_slice(&payload_bytes);
        let cmd = crate::RawMappingCommand {
            opcode: crate::MAPPINGS_COMMAND,
            offset: payload_buf.offset(),
            key: 99,
            metadata_count: 1,
            blob_count: 1,
        };
        payload_buf.commit(cmd).unwrap();
        let mut receiver = vmo_fifo::Receiver::<crate::RawMappingCommand>::new(vmo, 16).unwrap();

        let msg = receiver.peek().unwrap();
        process_mapping_command(&msg, &blobs).unwrap();

        assert!(blobs.is_loading(99));

        // Pager request arrives while loading. Buffer should be created and held.
        blobs.handle_page_request(99, 0..4096);
        assert!(!dropped.load(Ordering::Relaxed));

        // Complete metadata read (which will fail due to corrupt CompressionInfo).
        service.wait_and_trigger_sync();

        // Dropping guard cleans up LoadingSlot, which drops the queued buffer.
        assert!(dropped.load(Ordering::Relaxed));
        assert!(!blobs.is_loading(99));
    }

    #[fuchsia::test]
    async fn test_pager_concurrent_request_during_metadata_read() {
        use vmo_fifo::SyncSender;
        use zx::{Pager, PagerOptions, Port, Rights, Vmo, VmoOptions};

        let metadata = BlobMetadata { merkle_leaves: vec![], format: BlobFormat::Uncompressed };
        let encoded_metadata = bincode::serialize(&metadata).unwrap();
        let mut device_data = vec![0u8; (2 * BLOCK_SIZE) as usize];
        // Metadata stored at physical block 1
        device_data[BLOCK_SIZE as usize..BLOCK_SIZE as usize + encoded_metadata.len()]
            .copy_from_slice(&encoded_metadata);
        // Data stored at physical block 0
        device_data[..4].copy_from_slice(&[10, 20, 30, 40]);

        let service = DelayedBlockService::new(device_data);
        let port = Port::create();

        let (page_request, rx) = TestVecBuffer::new(BLOCK_SIZE as usize);
        let page_request_holder = Arc::new(Mutex::new(Some(page_request)));
        let page_request_clone = page_request_holder.clone();

        let blobs = Arc::new(Blobs::new(service.clone(), move |_key, _range| {
            page_request_clone.lock().take().unwrap()
        }));

        let _pager_thread = crate::PagerThread::spawn(
            port.duplicate_handle(Rights::SAME_RIGHTS).unwrap(),
            blobs.clone(),
        );

        let pager = Pager::create(PagerOptions::empty()).unwrap();
        let vmo_blob = pager.create_vmo(VmoOptions::empty(), &port, 42, BLOCK_SIZE).unwrap();
        let vmo_blob_clone = vmo_blob.duplicate_handle(Rights::SAME_RIGHTS).unwrap();

        // Encode mapping command with 1 data extent and 1 metadata extent
        let data_extent_words = Extents::encode_extents(&[Extent::new(0..BLOCK_SIZE, Some(0))]);
        let meta_extent_words =
            Extents::encode_extents(&[Extent::new(0..BLOCK_SIZE, Some(BLOCK_SIZE))]);
        let mut payload_bytes = Vec::new();
        for w in data_extent_words.iter().chain(meta_extent_words.iter()) {
            payload_bytes.extend_from_slice(&w.to_le_bytes());
        }

        let cmd = crate::RawMappingCommand {
            opcode: crate::MAPPINGS_COMMAND,
            offset: 0,
            key: 42,
            metadata_count: 1,
            blob_count: 1,
        };

        let vmo = Vmo::create(65536).unwrap();
        let mut sender = SyncSender::<crate::RawMappingCommand>::new(
            vmo.duplicate_handle(Rights::SAME_RIGHTS).unwrap(),
            1024,
            16,
        )
        .unwrap();
        let mut payload_buf = sender.reserve_payload(payload_bytes.len()).unwrap();
        payload_buf.data().copy_from_slice(&payload_bytes);
        payload_buf.commit(cmd).unwrap();
        let mut receiver = vmo_fifo::Receiver::<crate::RawMappingCommand>::new(vmo, 16).unwrap();

        std::thread::scope(|s| {
            let msg = receiver.peek().unwrap();
            let blobs_for_process = blobs.clone();
            s.spawn(move || {
                process_mapping_command(&msg, &blobs_for_process).unwrap();
            });

            // Wait until metadata block read has been submitted to service
            // (blob is in Loading state).
            while service.pending.lock().is_empty() {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }

            // Trigger page fault from a detached background thread while blob is loading metadata.
            std::thread::spawn(move || {
                let mut b = [0u8; 1];
                let _ = vmo_blob_clone.read(&mut b, 0);
            });

            // Complete metadata block read; this will insert the blob and immediately
            // drain queued page requests.
            service.wait_and_trigger_sync();
        });

        // Complete data block read requested by pager thread when draining queued request.
        service.wait_and_trigger_sync();

        // Check if page request made during metadata read was serviced!
        assert!(!rx.commits().is_empty(), "Page request made during metadata read was dropped!");
    }

    #[fuchsia::test]
    async fn test_pager_request_arrives_before_mapping_command() {
        use vmo_fifo::SyncSender;
        use zx::{Pager, PagerOptions, Port, Rights, Vmo, VmoOptions};

        let metadata = BlobMetadata { merkle_leaves: vec![], format: BlobFormat::Uncompressed };
        let encoded_metadata = bincode::serialize(&metadata).unwrap();
        let mut device_data = vec![0u8; (2 * BLOCK_SIZE) as usize];
        // Metadata stored at physical block 1
        device_data[BLOCK_SIZE as usize..BLOCK_SIZE as usize + encoded_metadata.len()]
            .copy_from_slice(&encoded_metadata);
        // Data stored at physical block 0
        device_data[..4].copy_from_slice(&[10, 20, 30, 40]);

        let service = DelayedBlockService::new(device_data);
        let port = Port::create();

        let (page_request, rx) = TestVecBuffer::new(BLOCK_SIZE as usize);
        let page_request_holder = Arc::new(Mutex::new(Some(page_request)));
        let page_request_clone = page_request_holder.clone();

        let blobs = Arc::new(Blobs::new(service.clone(), move |_key, _range| {
            page_request_clone.lock().take().unwrap()
        }));

        let _pager_thread = crate::PagerThread::spawn(
            port.duplicate_handle(Rights::SAME_RIGHTS).unwrap(),
            blobs.clone(),
        );

        let pager = Pager::create(PagerOptions::empty()).unwrap();
        let vmo_blob = pager.create_vmo(VmoOptions::empty(), &port, 42, BLOCK_SIZE).unwrap();
        let vmo_blob_clone = vmo_blob.duplicate_handle(Rights::SAME_RIGHTS).unwrap();

        // Encode mapping command with 1 data extent and 1 metadata extent
        let data_extent_words = Extents::encode_extents(&[Extent::new(0..BLOCK_SIZE, Some(0))]);
        let meta_extent_words =
            Extents::encode_extents(&[Extent::new(0..BLOCK_SIZE, Some(BLOCK_SIZE))]);
        let mut payload_bytes = Vec::new();
        for w in data_extent_words.iter().chain(meta_extent_words.iter()) {
            payload_bytes.extend_from_slice(&w.to_le_bytes());
        }

        let cmd = crate::RawMappingCommand {
            opcode: crate::MAPPINGS_COMMAND,
            offset: 0,
            key: 42,
            metadata_count: 1,
            blob_count: 1,
        };

        let vmo = Vmo::create(65536).unwrap();
        let mut sender = SyncSender::<crate::RawMappingCommand>::new(
            vmo.duplicate_handle(Rights::SAME_RIGHTS).unwrap(),
            1024,
            16,
        )
        .unwrap();
        let mut payload_buf = sender.reserve_payload(payload_bytes.len()).unwrap();
        payload_buf.data().copy_from_slice(&payload_bytes);
        payload_buf.commit(cmd).unwrap();
        let mut receiver = vmo_fifo::Receiver::<crate::RawMappingCommand>::new(vmo, 16).unwrap();

        // Trigger page fault from a background thread BEFORE the mapping command is processed.
        std::thread::spawn(move || {
            let mut b = [0u8; 1];
            let _ = vmo_blob_clone.read(&mut b, 0);
        });

        // Wait briefly to ensure page request arrives and is queued as pending mapping in `blobs`.
        std::thread::sleep(std::time::Duration::from_millis(50));

        std::thread::scope(|s| {
            let msg = receiver.peek().unwrap();
            let blobs_for_process = blobs.clone();
            s.spawn(move || {
                process_mapping_command(&msg, &blobs_for_process).unwrap();
            });

            // Trigger metadata read completion.
            service.wait_and_trigger_sync();
        });

        // Complete data block read requested by pager thread when draining queued request.
        service.wait_and_trigger_sync();

        // Check if page request made before mapping command was serviced!
        assert!(!rx.commits().is_empty(), "Page request made before mapping command was dropped!");
    }

    #[fuchsia::test]
    fn test_process_mapping_command_close_blob() {
        let service = Arc::new(FakeBlockService::new(vec![0u8; 8192]));
        let blobs = Arc::new(Blobs::new(service, |_k, _r| TestVecBuffer::new(4096).0));

        let vmo = zx::Vmo::create(65536).unwrap();
        let mut sender = vmo_fifo::SyncSender::<crate::RawMappingCommand>::new(
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            1024,
            16,
        )
        .unwrap();

        // 1. Send Mappings command with 0 metadata extents (uncompressed blob, loads immediately).
        let data_extent_words = Extents::encode_extents(&[Extent::new(0..BLOCK_SIZE, Some(0))]);
        let mut payload_bytes = Vec::new();
        for w in &data_extent_words {
            payload_bytes.extend_from_slice(&w.to_le_bytes());
        }
        let mut payload_buf = sender.reserve_payload(payload_bytes.len()).unwrap();
        payload_buf.data().copy_from_slice(&payload_bytes);
        let cmd = crate::RawMappingCommand {
            opcode: crate::MAPPINGS_COMMAND,
            offset: payload_buf.offset(),
            key: 123,
            metadata_count: 0,
            blob_count: 1,
        };
        payload_buf.commit(cmd).unwrap();

        let mut receiver = vmo_fifo::Receiver::<crate::RawMappingCommand>::new(vmo, 16).unwrap();
        let msg = receiver.peek().unwrap();
        process_mapping_command(&msg, &blobs).unwrap();
        msg.pop().unwrap();

        assert!(blobs.is_loaded(123));

        // 2. Send CloseBlob command.
        sender
            .push(crate::RawMappingCommand {
                opcode: crate::CLOSE_BLOB_COMMAND,
                offset: 0,
                key: 123,
                metadata_count: 0,
                blob_count: 0,
            })
            .unwrap();

        let msg = receiver.peek().unwrap();
        process_mapping_command(&msg, &blobs).unwrap();
        msg.pop().unwrap();

        assert!(!blobs.is_loaded(123));
    }
}
