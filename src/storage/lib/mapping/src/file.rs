// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::reader::{BlockService, read_aligned_range};
use crate::{Extents, MappingCommand, NullPageRequest, PageRequest, RawMappingCommand};
use anyhow::{Error, anyhow, bail};
use blob_metadata::{BlobFormat, BlobMetadata};
use byteorder::{LittleEndian, ReadBytesExt};
use delivery_blob::compression::{CompressionAlgorithm, CompressionInfo, StreamingDecompressor};
use fuchsia_sync::Mutex;
use futures::channel::oneshot;
use std::cmp::min;
use std::collections::hash_map::{Entry, HashMap};
use std::ops::{ControlFlow, Range};
use std::sync::Arc;
use vmo_fifo::Message;

/// Default readahead size used for streaming reads and decompression (128 KiB).
pub const READ_AHEAD_SIZE: u64 = 128 * 1024;

/// Calculates the readahead size for a given chunk size, rounding down `suggested_read_ahead_size`
/// to a multiple of `chunk_size`, or returning `chunk_size` if it is larger than
/// `suggested_read_ahead_size`.
pub fn read_ahead_size_for_chunk_size(chunk_size: u64, suggested_read_ahead_size: u64) -> u64 {
    if chunk_size >= suggested_read_ahead_size {
        chunk_size
    } else {
        (suggested_read_ahead_size / chunk_size) * chunk_size
    }
}

/// A mapped file containing extents and optional decompression metadata.
pub struct File {
    extents: Extents,
    uncompressed_size: u64,
    compression_info: Option<CompressionInfo>,
}

impl File {
    pub fn new(
        extents: Extents,
        uncompressed_size: u64,
        compression_info: Option<CompressionInfo>,
    ) -> Self {
        Self { extents, uncompressed_size, compression_info }
    }

    /// Returns the extents mapping logical offsets to device offsets.
    pub fn extents(&self) -> &Extents {
        &self.extents
    }

    /// Returns the uncompressed size of the file in bytes.
    pub fn uncompressed_size(&self) -> u64 {
        self.uncompressed_size
    }

    /// Returns decompression metadata if the file is compressed.
    pub fn compression_info(&self) -> Option<&CompressionInfo> {
        self.compression_info.as_ref()
    }

    /// Streams and decodes the uncompressed range requested by `page_request`, applying readahead.
    pub fn read_range(
        &self,
        service: &(impl BlockService + ?Sized),
        mut page_request: impl PageRequest,
    ) {
        let page_size = zx::system_get_page_size() as u64;
        let original_range = page_request.range();
        if original_range.is_empty() {
            return;
        }

        let page_aligned_size = self.uncompressed_size.next_multiple_of(page_size);
        if original_range.start >= page_aligned_size {
            return;
        }

        let read_ahead_size = match &self.compression_info {
            Some(info) => read_ahead_size_for_chunk_size(info.chunk_size(), READ_AHEAD_SIZE),
            None => READ_AHEAD_SIZE,
        };

        let read_range = (original_range.start / read_ahead_size) * read_ahead_size
            ..std::cmp::min(
                original_range.end.next_multiple_of(read_ahead_size),
                page_aligned_size,
            );
        if page_request.prepare(read_range.clone()).is_err() {
            return;
        }

        match &self.compression_info {
            None => {
                let mut current_offset = read_range.start;
                let uncompressed_size = self.uncompressed_size;

                read_aligned_range(&self.extents, read_range, service, move |res| {
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
                let Ok((mut decompressor, aligned_range)) =
                    StreamingDecompressor::new(info.clone(), self.uncompressed_size, page_request)
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
    requests: Vec<R>,
    waiters: Vec<oneshot::Sender<Arc<File>>>,
}

impl<R> Default for LoadingSlot<R> {
    fn default() -> Self {
        Self { requests: Vec::new(), waiters: Vec::new() }
    }
}

enum FileEntry<R> {
    Loading(LoadingSlot<R>),
    Loaded(Arc<File>),
}

impl<R> Default for FileEntry<R> {
    fn default() -> Self {
        Self::Loading(LoadingSlot::default())
    }
}

/// A thread-safe registry of active [`File`] instances indexed by their Zircon pager port key.
pub struct Files<S: ?Sized, F, R> {
    service: Arc<S>,
    request_factory: F,
    map: Mutex<HashMap<u64, FileEntry<R>>>,
}

impl<S: BlockService + ?Sized, R: PageRequest, F: Fn(u64, Range<u64>) -> R + Send + Sync + 'static>
    Files<S, F, R>
{
    /// Creates a new file registry with the provided block service and request factory.
    pub fn new(service: Arc<S>, request_factory: F) -> Self {
        Self { service, request_factory, map: Mutex::new(HashMap::new()) }
    }

    /// Returns a reference to the block service.
    pub fn service(&self) -> &Arc<S> {
        &self.service
    }

    /// Handles a page request from `PagerThread`.
    ///
    /// If the file is loaded, reads the range into a newly allocated buffer immediately.
    /// If the file is currently loading or unmapped, queues the request to be fulfilled
    /// when loaded.
    pub fn handle_page_request(&self, key: u64, range: Range<u64>) {
        let req = (self.request_factory)(key, range);
        let mut map = self.map.lock();
        match map.entry(key) {
            Entry::Occupied(mut entry) => match entry.get_mut() {
                FileEntry::Loaded(file) => {
                    let file = Arc::clone(file);
                    drop(map);
                    file.read_range(self.service.as_ref(), req);
                }
                FileEntry::Loading(slot) => {
                    slot.requests.push(req);
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
                entry.insert(FileEntry::Loading(LoadingSlot {
                    requests: vec![req],
                    waiters: Vec::new(),
                }));
            }
        }
    }

    /// Marks `key` as currently loading metadata, preserving any page requests that arrived
    /// prior to the mapping command.
    pub fn begin_loading(&self, key: u64) {
        self.map.lock().entry(key).or_default();
    }

    /// Inserts a file into the registry under `key`, immediately draining and servicing any
    /// page requests that arrived while metadata was loading.
    fn insert(&self, key: u64, file: Arc<File>) {
        let (reqs, waiters) = {
            let mut map = self.map.lock();
            let prev = map.insert(key, FileEntry::Loaded(file.clone()));
            match prev {
                Some(FileEntry::Loading(slot)) => (slot.requests, slot.waiters),
                _ => (Vec::new(), Vec::new()),
            }
        };

        for waiter in waiters {
            let _ = waiter.send(file.clone());
        }

        for req in reqs {
            file.read_range(self.service.as_ref(), req);
        }
    }

    /// Returns the loaded [`File`] registered under `key`, if present.
    pub fn get_file(&self, key: u64) -> Option<Arc<File>> {
        let map = self.map.lock();
        match map.get(&key) {
            Some(FileEntry::Loaded(file)) => Some(file.clone()),
            _ => None,
        }
    }

    /// Returns a future that completes with the loaded [`File`] registered under `key`,
    /// waiting asynchronously if the file is currently loading or hasn't arrived yet.
    pub async fn wait_for_file(&self, key: u64) -> Result<Arc<File>, Error> {
        let receiver = {
            let mut map = self.map.lock();
            match map.entry(key) {
                Entry::Occupied(mut entry) => match entry.get_mut() {
                    FileEntry::Loaded(file) => return Ok(file.clone()),
                    FileEntry::Loading(slot) => {
                        let (sender, receiver) = oneshot::channel();
                        slot.waiters.push(sender);
                        receiver
                    }
                },
                Entry::Vacant(entry) => {
                    let (sender, receiver) = oneshot::channel();
                    entry.insert(FileEntry::Loading(LoadingSlot {
                        requests: Vec::new(),
                        waiters: vec![sender],
                    }));
                    receiver
                }
            }
        };

        receiver.await.map_err(|_| anyhow!("File loading cancelled"))
    }

    /// Removes the file registered under `key`.
    pub fn remove(&self, key: u64) {
        self.map.lock().remove(&key);
    }

    /// Returns `true` if `key` is currently in the loading state.
    #[cfg(test)]
    pub fn is_loading(&self, key: u64) -> bool {
        matches!(self.map.lock().get(&key), Some(FileEntry::Loading(_)))
    }

    /// Returns `true` if `key` is currently in the loaded state.
    #[cfg(test)]
    pub fn is_loaded(&self, key: u64) -> bool {
        matches!(self.map.lock().get(&key), Some(FileEntry::Loaded(_)))
    }
}

impl<S: BlockService + ?Sized> Files<S, fn(u64, Range<u64>) -> NullPageRequest, NullPageRequest> {
    /// Creates a file registry without a pager for intermediate (e.g. partition) sessions.
    pub fn new_without_pager(service: Arc<S>) -> Self {
        Self::new(service, |_, _| NullPageRequest)
    }
}

/// The version where `BlobMetadata` was introduced in Fxfs.
/// Eventually, we'll need to integrate Fxfs's code for upgrading data structures.
const BLOB_METADATA_VERSION: u32 = 53;

/// Deserializes versioned `BlobMetadata` from the raw on-disk bytes.
fn deserialize_blob_metadata(mut bytes: &[u8]) -> Result<BlobMetadata, anyhow::Error> {
    use bincode::Options;
    let options = bincode::DefaultOptions::new().allow_trailing_bytes();

    let version = bytes.read_u32::<LittleEndian>()?;
    if version < BLOB_METADATA_VERSION {
        bail!(
            "Unsupported blob metadata version: {} (expected >= {})",
            version,
            BLOB_METADATA_VERSION
        );
    }

    options
        .deserialize::<BlobMetadata>(bytes)
        .map_err(|e| anyhow!("Failed to deserialize BlobMetadata: {e:?}"))
}

/// Reads blob metadata asynchronously from `metadata_extents` using `service`.
/// Once the metadata is retrieved, deserialized, and parsed, `callback` is invoked with
/// `(uncompressed_size, Option<CompressionInfo>)`.
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
        let metadata = match deserialize_blob_metadata(&metadata_bytes) {
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

/// RAII guard that manages the lifecycle of a file transitioning from loading metadata to loaded.
///
/// When metadata is being fetched asynchronously from storage, the file entry in [`Files`]
/// remains in the [`FileEntry::Loading`] state, accumulating incoming page requests in its queue.
///
/// - On success: [`LoadingFileGuard::commit`] consumes the guard, stores the fully initialized
///   [`File`], and immediately drains and fulfills all queued page requests.
/// - On failure or cancellation: If dropped before `commit` is called (e.g. due to storage I/O
///   error, corrupted metadata, or session teardown), the `Drop` implementation cleans up the
///   entry by removing `key` from [`Files`]. Dropping the loading slot drops all queued
///   [`PageRequest`] objects, which fails the pending page requests in the kernel pager.
struct LoadingFileGuard<
    S: BlockService + ?Sized + 'static,
    R: PageRequest,
    F: Fn(u64, Range<u64>) -> R + Send + Sync + 'static,
> {
    files: Option<Arc<Files<S, F, R>>>,
    key: u64,
}

impl<
    S: BlockService + ?Sized + 'static,
    R: PageRequest,
    F: Fn(u64, Range<u64>) -> R + Send + Sync + 'static,
> LoadingFileGuard<S, R, F>
{
    /// Commits the loaded file to the registry, transferring ownership and draining all queued
    /// page requests.
    fn commit(mut self, file: Arc<File>) {
        self.files.take().unwrap().insert(self.key, file);
    }
}

impl<
    S: BlockService + ?Sized + 'static,
    R: PageRequest,
    F: Fn(u64, Range<u64>) -> R + Send + Sync + 'static,
> Drop for LoadingFileGuard<S, R, F>
{
    fn drop(&mut self) {
        if let Some(files) = self.files.take() {
            files.remove(self.key);
        }
    }
}

/// Processes a raw mapping command (`RawMappingCommand`), decoding extent descriptors,
/// reading blob metadata from storage, and inserting/removing the file from `files`.
pub fn process_mapping_command<
    S: BlockService + ?Sized + 'static,
    R: PageRequest,
    F: Fn(u64, Range<u64>) -> R + Send + Sync + 'static,
>(
    msg: &Message<'_, RawMappingCommand>,
    files: &Arc<Files<S, F, R>>,
) -> Result<(), Error> {
    let cmd = **msg;
    match MappingCommand::try_from(cmd)? {
        MappingCommand::Mappings {
            key,
            offset,
            stored_size,
            device_offset,
            metadata_count,
            blob_count,
        } => {
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
            let data_extents = Extents::from_encoded(data_bytes.iter_as::<u64>(), device_offset)
                .ok_or_else(|| anyhow!("Failed to decode data extents"))?;
            let metadata_extents =
                Extents::from_encoded(metadata_bytes.iter_as::<u64>(), device_offset)
                    .ok_or_else(|| anyhow!("Failed to decode metadata extents"))?;

            files.begin_loading(key);
            let service = files.service().clone();
            let guard = LoadingFileGuard { files: Some(files.clone()), key };
            read_blob_metadata(
                service.as_ref(),
                &metadata_extents,
                stored_size,
                move |(uncompressed_size, compression_info)| {
                    let file =
                        Arc::new(File::new(data_extents, uncompressed_size, compression_info));
                    guard.commit(file);
                },
            );
            Ok(())
        }
        MappingCommand::CloseBlob { key } => {
            files.remove(key);
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
    use bincode::Options;
    use byteorder::WriteBytesExt;
    use delivery_blob::compression::{ChunkedArchiveOptions, CompressionAlgorithm};
    use fuchsia_async as fasync;
    use std::sync::Arc;

    fn serialize_metadata(metadata: &BlobMetadata) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.write_u32::<LittleEndian>(BLOB_METADATA_VERSION).unwrap();
        bincode::DefaultOptions::new()
            .allow_trailing_bytes()
            .serialize_into(&mut bytes, metadata)
            .unwrap();
        bytes
    }

    #[test]
    fn test_read_range_uncompressed() {
        let block_count = 8;
        let mut expected_data = vec![0u8; (block_count as u64 * BLOCK_SIZE) as usize];
        for (i, byte) in expected_data.iter_mut().enumerate() {
            *byte = (i % 255) as u8;
        }
        let service = FakeBlockService::new(expected_data.clone());

        let extents = Extents::try_new([Extent::new(0..(8 * BLOCK_SIZE), Some(0))], 0).unwrap();
        let file = Arc::new(File::new(extents, 8 * BLOCK_SIZE, None));

        let (page_request, rx) = TestVecBuffer::new_with_range(0..(8 * BLOCK_SIZE));
        file.read_range(&service, page_request);

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
            Extents::try_new([Extent::new(0..(stored_blocks * BLOCK_SIZE), Some(0))], 0).unwrap();
        let compression_info = CompressionInfo::new(
            chunk_size as u64,
            stored_size,
            &compressed_offsets,
            CompressionAlgorithm::Zstd,
        )
        .unwrap();
        let file = Arc::new(File::new(extents, uncompressed_size as u64, Some(compression_info)));

        let dest_alloc_size = uncompressed_size.next_multiple_of(chunk_size);
        let (mut page_request, rx) = TestVecBuffer::new_with_range(0..(uncompressed_size as u64));
        page_request.data.resize(dest_alloc_size, 0);
        file.read_range(&service, page_request);

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
            Extents::try_new([Extent::new(0..(stored_blocks * BLOCK_SIZE), Some(0))], 0).unwrap();
        let compression_info = CompressionInfo::new(
            chunk_size as u64,
            stored_size,
            &compressed_offsets,
            CompressionAlgorithm::Lz4,
        )
        .unwrap();
        let file = Arc::new(File::new(extents, uncompressed_size as u64, Some(compression_info)));

        let (page_request, rx) = TestVecBuffer::new_with_range(0..(uncompressed_size as u64));
        file.read_range(&service, page_request);

        assert_eq!(rx.commits(), vec![(0, chunk_size), (chunk_size as u64, chunk_size)]);
        assert_eq!(rx.output(), uncompressed_data);
    }

    #[test]
    fn test_read_range_invalid_range_noop() {
        let service = FakeBlockService::new(vec![0u8; 8192]);
        let extents = Extents::try_new([Extent::new(0..8192, Some(0))], 0).unwrap();
        let file = Arc::new(File::new(extents, 8192, None));

        let (page_request, rx) = TestVecBuffer::new_with_range(4096..4096);
        // start >= end should be a no-op returning Ok(())
        file.read_range(&service, page_request);
        assert_eq!(rx.commits().len(), 0);
    }

    #[test]
    fn test_file_getters() {
        let extents = Extents::try_new([Extent::new(0..8192, Some(0))], 0).unwrap();
        let uncompressed_size = 8192u64;

        let file_uncompressed = File::new(extents, uncompressed_size, None);
        assert_eq!(file_uncompressed.uncompressed_size(), 8192);
        assert!(file_uncompressed.compression_info().is_none());

        let compression_info =
            CompressionInfo::new(32768, 4096, &[0], CompressionAlgorithm::Zstd).unwrap();
        let file_compressed = File::new(
            Extents::try_new([Extent::new(0..8192, Some(0))], 0).unwrap(),
            uncompressed_size,
            Some(compression_info),
        );
        assert!(file_compressed.compression_info().is_some());
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

        let extents = Extents::try_new([Extent::new(0..8192, Some(0))], 0).unwrap();
        let file = File::new(extents, 8192, None);

        let (page_request, rx) = TestVecBuffer::new_with_range(0..8192);

        file.read_range(&FailingBlockService, page_request);
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
            Extents::try_new([Extent::new(0..(block_count * BLOCK_SIZE), Some(0))], 0).unwrap();
        let file = File::new(extents, block_count * BLOCK_SIZE, None);

        let (page_request, rx) = TestVecBuffer::new_with_range(0..(block_count * BLOCK_SIZE));
        file.read_range(&service, page_request);

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

        let extents = Extents::try_new([Extent::new(0..8192, Some(0))], 0).unwrap();
        let file = File::new(extents, uncompressed_size, None);

        let (page_request, rx) = TestVecBuffer::new_with_range(0..8192);
        file.read_range(&service, page_request);

        assert_eq!(rx.commits(), vec![(0, 8192)]);
        assert_eq!(&rx.output()[..5000], &expected_data[..5000]);
    }

    #[test]
    fn test_read_range_uncompressed_readahead() {
        let total_blocks = 64; // 256 KiB
        let mut expected_data = vec![0u8; (total_blocks * BLOCK_SIZE) as usize];
        for (i, byte) in expected_data.iter_mut().enumerate() {
            *byte = ((i * 17) % 251) as u8;
        }
        let service = FakeBlockService::new(expected_data.clone());

        let extents =
            Extents::try_new([Extent::new(0..(total_blocks * BLOCK_SIZE), Some(0))], 0).unwrap();
        let file = File::new(extents, total_blocks * BLOCK_SIZE, None);

        // Request 1 block at offset 4096. Readahead should expand to 0..128 KiB.
        let (page_request, rx) = TestVecBuffer::new_with_range(4096..8192);
        file.read_range(&service, page_request);

        assert_eq!(rx.commits(), vec![(0, READ_AHEAD_SIZE as usize)]);
        assert_eq!(
            &rx.output()[..READ_AHEAD_SIZE as usize],
            &expected_data[..READ_AHEAD_SIZE as usize]
        );
    }

    #[test]
    fn test_read_range_uncompressed_readahead_second_window() {
        let total_blocks = 64; // 256 KiB
        let mut expected_data = vec![0u8; (total_blocks * BLOCK_SIZE) as usize];
        for (i, byte) in expected_data.iter_mut().enumerate() {
            *byte = ((i * 19) % 251) as u8;
        }
        let service = FakeBlockService::new(expected_data.clone());

        let extents =
            Extents::try_new([Extent::new(0..(total_blocks * BLOCK_SIZE), Some(0))], 0).unwrap();
        let file = File::new(extents, total_blocks * BLOCK_SIZE, None);

        // Request 1 block at offset 132 KiB (135168..139264).
        // Readahead should expand to 128 KiB..256 KiB (131072..262144).
        let (page_request, rx) = TestVecBuffer::new_with_range(135168..139264);
        file.read_range(&service, page_request);

        assert_eq!(rx.commits(), vec![(READ_AHEAD_SIZE, READ_AHEAD_SIZE as usize)]);
        assert_eq!(
            &rx.output()[..READ_AHEAD_SIZE as usize],
            &expected_data[READ_AHEAD_SIZE as usize..2 * READ_AHEAD_SIZE as usize]
        );
    }

    #[test]
    fn test_read_range_uncompressed_readahead_tail_capped() {
        let uncompressed_size = 140_000u64;
        let total_blocks = (uncompressed_size.next_multiple_of(BLOCK_SIZE) / BLOCK_SIZE) as u64;
        let mut expected_data = vec![0u8; (total_blocks * BLOCK_SIZE) as usize];
        for i in 0..uncompressed_size as usize {
            expected_data[i] = ((i * 23) % 251) as u8;
        }
        let service = FakeBlockService::new(expected_data.clone());

        let extents =
            Extents::try_new([Extent::new(0..(total_blocks * BLOCK_SIZE), Some(0))], 0).unwrap();
        let file = File::new(extents, uncompressed_size, None);

        // Request 1 block at offset 132 KiB (135168..139264).
        // Readahead window starts at 128 KiB (131072) and would normally extend to 256 KiB
        // (262144), but should be capped at page_aligned_size (143360).
        let (page_request, rx) = TestVecBuffer::new_with_range(135168..139264);
        file.read_range(&service, page_request);

        let expected_start = READ_AHEAD_SIZE;
        let page_aligned_size = uncompressed_size.next_multiple_of(BLOCK_SIZE);
        let expected_len = (page_aligned_size - expected_start) as usize;
        assert_eq!(rx.commits(), vec![(expected_start, expected_len)]);

        let valid_len = (uncompressed_size - expected_start) as usize;
        assert_eq!(
            &rx.output()[..valid_len],
            &expected_data[expected_start as usize..uncompressed_size as usize]
        );
        // The remaining tail bytes within the last page must be zero-filled.
        assert!(rx.output()[valid_len..expected_len].iter().all(|&b| b == 0));
    }

    #[test]
    fn test_read_range_compressed_second_readahead_window() {
        let chunk_count = 8;
        let chunk_size = 32768usize;
        let uncompressed_size = chunk_count * chunk_size;
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

        let stored_size = compressed_data.len() as u64;
        let stored_blocks = stored_size.div_ceil(BLOCK_SIZE);
        let mut device_data = vec![0u8; (stored_blocks * BLOCK_SIZE) as usize];
        device_data[..compressed_data.len()].copy_from_slice(&compressed_data);
        let service = FakeBlockService::new(device_data);

        let extents =
            Extents::try_new([Extent::new(0..(stored_blocks * BLOCK_SIZE), Some(0))], 0).unwrap();
        let compression_info = CompressionInfo::new(
            chunk_size as u64,
            stored_size,
            &compressed_offsets,
            CompressionAlgorithm::Zstd,
        )
        .unwrap();
        let file = File::new(extents, uncompressed_size as u64, Some(compression_info));

        // Request 1 block in the second 128 KiB readahead window (e.g. 135168..139264).
        // Readahead should expand to 131072..262144 (chunks 4, 5, 6, 7).
        let (page_request, rx) = TestVecBuffer::new_with_range(135168..139264);
        file.read_range(&service, page_request);

        assert_eq!(
            rx.commits(),
            vec![
                (131072, chunk_size),
                (163840, chunk_size),
                (196608, chunk_size),
                (229376, chunk_size),
            ]
        );
        assert_eq!(&rx.output()[..131072], &uncompressed_data[131072..262144]);
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
            Extents::try_new([Extent::new(0..(stored_blocks * BLOCK_SIZE), Some(0))], 0).unwrap();
        let compression_info = CompressionInfo::new(
            chunk_size as u64,
            stored_size,
            &compressed_offsets,
            CompressionAlgorithm::Zstd,
        )
        .unwrap();
        let file = File::new(extents, uncompressed_size as u64, Some(compression_info));

        // Pre-fill destination buffer with 0xFF bytes to verify tail zeroing
        let (mut page_request, rx) = TestVecBuffer::new_with_range(0..(uncompressed_size as u64));
        page_request.data.resize(65536, 0);
        page_request.data.fill(0xFF);
        file.read_range(&service, page_request);

        assert_eq!(rx.commits(), vec![(0, chunk_size), (chunk_size as u64, chunk_size)]);
        assert_eq!(&rx.output()[..uncompressed_size], &uncompressed_data[..]);
        assert_eq!(&rx.output()[uncompressed_size..65536], &[0u8; 31744]);
    }

    #[test]
    fn test_files_registry() {
        let extents = Extents::try_new([Extent::new(0..4096, Some(0))], 0).unwrap();
        let file = Arc::new(File::new(extents, 4096, None));
        let service = Arc::new(FakeBlockService::new(vec![0u8; 4096]));
        let files = Files::new(service, |_key, _range| TestVecBuffer::new(4096).0);

        assert!(!files.is_loading(100));
        files.begin_loading(100);
        assert!(files.is_loading(100));

        files.insert(100, file.clone());
        assert!(!files.is_loading(100));
        files.remove(100);
        assert!(!files.is_loading(100));
    }

    #[test]
    fn test_files_new_without_pager() {
        let extents = Extents::try_new([Extent::new(0..4096, Some(0))], 0).unwrap();
        let file = Arc::new(File::new(extents, 4096, None));
        let service = Arc::new(FakeBlockService::new(vec![0u8; 4096]));
        let files = Files::new_without_pager(service);

        files.insert(100, file.clone());
        assert_eq!(files.get_file(100).unwrap().uncompressed_size(), 4096);
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
        let encoded_metadata = serialize_metadata(&metadata);
        let mut device_data = vec![0u8; BLOCK_SIZE as usize];
        device_data[..encoded_metadata.len()].copy_from_slice(&encoded_metadata);

        let service = DelayedBlockService::new(device_data);
        let metadata_extents = Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(0))], 0).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        read_blob_metadata(service.as_ref(), &metadata_extents, 2400, move |res| {
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
        let encoded_metadata = serialize_metadata(&metadata);
        let mut device_data = vec![0u8; BLOCK_SIZE as usize];
        device_data[..encoded_metadata.len()].copy_from_slice(&encoded_metadata);

        let service = DelayedBlockService::new(device_data);
        let metadata_extents = Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(0))], 0).unwrap();

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
        let metadata_extents = Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(0))], 0).unwrap();

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
        let encoded_metadata = serialize_metadata(&metadata);
        let mut device_data = vec![0u8; (2 * BLOCK_SIZE) as usize];
        device_data[BLOCK_SIZE as usize..BLOCK_SIZE as usize + encoded_metadata.len()]
            .copy_from_slice(&encoded_metadata);

        let service = DelayedBlockService::new(device_data);
        let files = Arc::new(Files::new(service.clone(), |_k, _r| TestVecBuffer::new(4096).0));

        let data_extents = Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(0))], 0).unwrap();
        let meta_extents =
            Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(BLOCK_SIZE))], 0).unwrap();
        let mut payload_bytes = Vec::new();
        for w in
            Extents::encode_extents(&data_extents).chain(Extents::encode_extents(&meta_extents))
        {
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
            stored_size: 4096,
            device_offset: 0,
            metadata_count: 1,
            blob_count: 1,
        };
        payload_buf.commit(cmd).unwrap();
        let mut receiver = vmo_fifo::Receiver::<crate::RawMappingCommand>::new(vmo, 16).unwrap();

        let msg = receiver.peek().unwrap();
        process_mapping_command(&msg, &files).unwrap();

        // File is initially in Loading state.
        assert!(files.is_loading(99));

        // Complete metadata read (which will fail due to corrupt CompressionInfo).
        service.wait_and_trigger_sync();

        // Files map should have cleaned up the failed entry on drop.
        assert!(!files.is_loading(99));
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
        let encoded_metadata = serialize_metadata(&metadata);
        let mut device_data = vec![0u8; (2 * BLOCK_SIZE) as usize];
        device_data[BLOCK_SIZE as usize..BLOCK_SIZE as usize + encoded_metadata.len()]
            .copy_from_slice(&encoded_metadata);

        let service = DelayedBlockService::new(device_data);
        let dropped = Arc::new(AtomicBool::new(false));
        let dropped_clone = dropped.clone();
        let files = Arc::new(Files::new(service.clone(), move |_k, r| DroppingBuffer {
            data: vec![0u8; 4096],
            range: r,
            dropped: dropped_clone.clone(),
        }));

        let data_extents = Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(0))], 0).unwrap();
        let meta_extents =
            Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(BLOCK_SIZE))], 0).unwrap();
        let mut payload_bytes = Vec::new();
        for w in
            Extents::encode_extents(&data_extents).chain(Extents::encode_extents(&meta_extents))
        {
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
            stored_size: 4096,
            device_offset: 0,
            metadata_count: 1,
            blob_count: 1,
        };
        payload_buf.commit(cmd).unwrap();
        let mut receiver = vmo_fifo::Receiver::<crate::RawMappingCommand>::new(vmo, 16).unwrap();

        let msg = receiver.peek().unwrap();
        process_mapping_command(&msg, &files).unwrap();

        assert!(files.is_loading(99));

        // Pager request arrives while loading. Buffer should be created and held.
        files.handle_page_request(99, 0..4096);
        assert!(!dropped.load(Ordering::Relaxed));

        // Complete metadata read (which will fail due to corrupt CompressionInfo).
        service.wait_and_trigger_sync();

        // Dropping guard cleans up LoadingSlot, which drops the queued buffer.
        assert!(dropped.load(Ordering::Relaxed));
        assert!(!files.is_loading(99));
    }

    #[fuchsia::test]
    async fn test_pager_concurrent_request_during_metadata_read() {
        use vmo_fifo::SyncSender;
        use zx::{Pager, PagerOptions, Port, Rights, Vmo, VmoOptions};

        let metadata = BlobMetadata { merkle_leaves: vec![], format: BlobFormat::Uncompressed };
        let encoded_metadata = serialize_metadata(&metadata);
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

        let files = Arc::new(Files::new(service.clone(), move |_key, _range| {
            page_request_clone.lock().take().unwrap()
        }));

        let _pager_thread = crate::PagerThread::spawn(
            port.duplicate_handle(Rights::SAME_RIGHTS).unwrap(),
            files.clone(),
        );

        let pager = Pager::create(PagerOptions::empty()).unwrap();
        let vmo_blob = pager.create_vmo(VmoOptions::empty(), &port, 42, BLOCK_SIZE).unwrap();
        let vmo_blob_clone = vmo_blob.duplicate_handle(Rights::SAME_RIGHTS).unwrap();

        // Encode mapping command with 1 data extent and 1 metadata extent
        let data_extents = Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(0))], 0).unwrap();
        let meta_extents =
            Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(BLOCK_SIZE))], 0).unwrap();
        let mut payload_bytes = Vec::new();
        for w in
            Extents::encode_extents(&data_extents).chain(Extents::encode_extents(&meta_extents))
        {
            payload_bytes.extend_from_slice(&w.to_le_bytes());
        }

        let cmd = crate::RawMappingCommand {
            opcode: crate::MAPPINGS_COMMAND,
            offset: 0,
            key: 42,
            stored_size: BLOCK_SIZE,
            device_offset: 0,
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
            let files_for_process = files.clone();
            s.spawn(move || {
                process_mapping_command(&msg, &files_for_process).unwrap();
            });

            // Wait until metadata block read has been submitted to service
            // (file is in Loading state).
            while service.pending.lock().is_empty() {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }

            // Trigger page fault from a detached background thread while file is loading metadata.
            std::thread::spawn(move || {
                let mut b = [0u8; 1];
                let _ = vmo_blob_clone.read(&mut b, 0);
            });

            // Complete metadata block read; this will insert the file and immediately
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
        let encoded_metadata = serialize_metadata(&metadata);
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

        let files = Arc::new(Files::new(service.clone(), move |_key, _range| {
            page_request_clone.lock().take().unwrap()
        }));

        let _pager_thread = crate::PagerThread::spawn(
            port.duplicate_handle(Rights::SAME_RIGHTS).unwrap(),
            files.clone(),
        );

        let pager = Pager::create(PagerOptions::empty()).unwrap();
        let vmo_blob = pager.create_vmo(VmoOptions::empty(), &port, 42, BLOCK_SIZE).unwrap();
        let vmo_blob_clone = vmo_blob.duplicate_handle(Rights::SAME_RIGHTS).unwrap();

        // Encode mapping command with 1 data extent and 1 metadata extent
        let data_extents = Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(0))], 0).unwrap();
        let meta_extents =
            Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(BLOCK_SIZE))], 0).unwrap();
        let mut payload_bytes = Vec::new();
        for w in
            Extents::encode_extents(&data_extents).chain(Extents::encode_extents(&meta_extents))
        {
            payload_bytes.extend_from_slice(&w.to_le_bytes());
        }

        let cmd = crate::RawMappingCommand {
            opcode: crate::MAPPINGS_COMMAND,
            offset: 0,
            key: 42,
            stored_size: BLOCK_SIZE,
            device_offset: 0,
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

        // Wait briefly to ensure page request arrives and is queued as pending mapping in `files`.
        std::thread::sleep(std::time::Duration::from_millis(50));

        std::thread::scope(|s| {
            let msg = receiver.peek().unwrap();
            let files_for_process = files.clone();
            s.spawn(move || {
                process_mapping_command(&msg, &files_for_process).unwrap();
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
        let files = Arc::new(Files::new(service, |_k, _r| TestVecBuffer::new(4096).0));

        let vmo = zx::Vmo::create(65536).unwrap();
        let mut sender = vmo_fifo::SyncSender::<crate::RawMappingCommand>::new(
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            1024,
            16,
        )
        .unwrap();

        // 1. Send Mappings command with 0 metadata extents (uncompressed file, loads immediately).
        let data_extents = Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(0))], 0).unwrap();
        let mut payload_bytes = Vec::new();
        for w in Extents::encode_extents(&data_extents) {
            payload_bytes.extend_from_slice(&w.to_le_bytes());
        }
        let mut payload_buf = sender.reserve_payload(payload_bytes.len()).unwrap();
        payload_buf.data().copy_from_slice(&payload_bytes);
        let cmd = crate::RawMappingCommand {
            opcode: crate::MAPPINGS_COMMAND,
            offset: payload_buf.offset(),
            key: 123,
            stored_size: 4096,
            device_offset: 0,
            metadata_count: 0,
            blob_count: 1,
        };
        payload_buf.commit(cmd).unwrap();

        let mut receiver = vmo_fifo::Receiver::<crate::RawMappingCommand>::new(vmo, 16).unwrap();
        let msg = receiver.peek().unwrap();
        process_mapping_command(&msg, &files).unwrap();
        msg.pop().unwrap();

        assert!(files.is_loaded(123));

        // 2. Send CloseBlob command.
        sender
            .push(crate::RawMappingCommand {
                opcode: crate::CLOSE_BLOB_COMMAND,
                offset: 0,
                key: 123,
                stored_size: 0,
                device_offset: 0,
                metadata_count: 0,
                blob_count: 0,
            })
            .unwrap();

        let msg = receiver.peek().unwrap();
        process_mapping_command(&msg, &files).unwrap();
        msg.pop().unwrap();

        assert!(!files.is_loaded(123));
    }

    #[test]
    fn test_read_ahead_size_for_chunk_size() {
        assert_eq!(read_ahead_size_for_chunk_size(32 * 1024, 32 * 1024), 32 * 1024);
        assert_eq!(read_ahead_size_for_chunk_size(48 * 1024, 32 * 1024), 48 * 1024);
        assert_eq!(read_ahead_size_for_chunk_size(64 * 1024, 32 * 1024), 64 * 1024);

        assert_eq!(read_ahead_size_for_chunk_size(32 * 1024, 64 * 1024), 64 * 1024);
        assert_eq!(read_ahead_size_for_chunk_size(48 * 1024, 64 * 1024), 48 * 1024);
        assert_eq!(read_ahead_size_for_chunk_size(64 * 1024, 64 * 1024), 64 * 1024);
        assert_eq!(read_ahead_size_for_chunk_size(96 * 1024, 64 * 1024), 96 * 1024);

        assert_eq!(read_ahead_size_for_chunk_size(32 * 1024, 128 * 1024), 128 * 1024);
        assert_eq!(read_ahead_size_for_chunk_size(48 * 1024, 128 * 1024), 96 * 1024);
        assert_eq!(read_ahead_size_for_chunk_size(64 * 1024, 128 * 1024), 128 * 1024);
        assert_eq!(read_ahead_size_for_chunk_size(96 * 1024, 128 * 1024), 96 * 1024);
    }

    #[test]
    fn test_deserialize_blob_metadata() {
        let metadata = BlobMetadata { merkle_leaves: vec![], format: BlobFormat::Uncompressed };

        // Valid metadata with version 53.
        let bytes = serialize_metadata(&metadata);
        let deserialized = deserialize_blob_metadata(&bytes).unwrap();
        assert_eq!(deserialized, metadata);

        // Buffer too short (< 4 bytes).
        assert!(deserialize_blob_metadata(&[1, 2, 3]).is_err());

        // Unsupported version (< 53).
        let mut old_version_bytes = bytes.clone();
        (&mut old_version_bytes[..4]).write_u32::<LittleEndian>(52).unwrap();
        assert!(deserialize_blob_metadata(&old_version_bytes).is_err());
    }

    #[fuchsia::test]
    async fn test_wait_for_file_before_and_after_insert() {
        let service = Arc::new(FakeBlockService::new(vec![]));
        let files = Arc::new(Files::new_without_pager(service));

        let extents = Extents::try_new([Extent::new(0..BLOCK_SIZE, Some(0))], 0).unwrap();
        let file = Arc::new(File::new(extents, BLOCK_SIZE, None));

        // Wait before insert.
        let files_clone = files.clone();
        let file_clone = file.clone();
        let wait_task =
            fasync::Task::spawn(async move { files_clone.wait_for_file(42).await.unwrap() });

        // Insert the file.
        files.insert(42, file_clone);
        let loaded_file = wait_task.await;
        assert_eq!(loaded_file.uncompressed_size(), BLOCK_SIZE);

        // Wait after insert.
        let loaded_file2 = files.wait_for_file(42).await.unwrap();
        assert_eq!(loaded_file2.uncompressed_size(), BLOCK_SIZE);
    }
}
