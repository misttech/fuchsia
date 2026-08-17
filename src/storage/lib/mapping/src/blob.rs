// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::reader::{BlockService, read_aligned_range};
use crate::{Extents, PageRequest};
use delivery_blob::compression::{CompressionInfo, StreamingDecompressor};
use fuchsia_sync::Mutex;
use std::cmp::min;
use std::collections::HashMap;
use std::ops::{ControlFlow, Range};
use std::sync::Arc;

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

    /// Streams and decodes the specified uncompressed `range` into the provided `page_request`.
    ///
    /// For uncompressed blobs, both `range.start` and `range.end` must be multiples of
    /// `BLOCK_SIZE`. For compressed blobs, `range.start` must be a multiple of the compression
    /// chunk size, and `range.end` must either be a multiple of the chunk size or equal to
    /// `uncompressed_size`.
    pub fn read_range(
        &self,
        range: Range<u64>,
        service: &(impl BlockService + ?Sized),
        mut page_request: impl PageRequest,
    ) {
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
                    let buffer = match res {
                        Ok(buf) => buf,
                        Err(_) => {
                            return ControlFlow::Break(());
                        }
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
                    let buffer = match res {
                        Ok(buf) => buf,
                        Err(_) => {
                            return ControlFlow::Break(());
                        }
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

/// A thread-safe registry of active [`Blob`] instances indexed by their Zircon pager port key.
#[derive(Default)]
pub struct Blobs {
    map: Mutex<HashMap<u64, Arc<Blob>>>,
}

impl Blobs {
    /// Creates a new empty blob registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts a blob into the registry under `key`, returning the previous blob if one existed.
    pub fn insert(&self, key: u64, blob: Arc<Blob>) -> Option<Arc<Blob>> {
        self.map.lock().insert(key, blob)
    }

    /// Retrieves a cloned handle to the blob registered under `key`, or `None` if not present.
    pub fn get(&self, key: u64) -> Option<Arc<Blob>> {
        self.map.lock().get(&key).cloned()
    }

    /// Removes and returns the blob registered under `key`, or `None` if not present.
    pub fn remove(&self, key: u64) -> Option<Arc<Blob>> {
        self.map.lock().remove(&key)
    }

    /// Returns the number of blobs in the registry.
    pub fn len(&self) -> usize {
        self.map.lock().len()
    }

    /// Returns `true` if the registry contains no blobs.
    pub fn is_empty(&self) -> bool {
        self.map.lock().is_empty()
    }

    /// Removes all blobs from the registry.
    pub fn clear(&self) {
        self.map.lock().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let extents = Extents::from_encoded(&extents).unwrap();
        let blob = Arc::new(Blob::new(extents, 8 * BLOCK_SIZE, None));

        let (page_request, rx) = TestVecBuffer::new(expected_data.len());
        blob.read_range(0..(8 * BLOCK_SIZE), &service, page_request);

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
        let extents = Extents::from_encoded(&extents).unwrap();
        let compression_info = CompressionInfo::new(
            chunk_size as u64,
            stored_size,
            &compressed_offsets,
            CompressionAlgorithm::Zstd,
        )
        .unwrap();
        let blob = Arc::new(Blob::new(extents, uncompressed_size as u64, Some(compression_info)));

        let dest_alloc_size = uncompressed_size.next_multiple_of(chunk_size);
        let (page_request, rx) = TestVecBuffer::new(dest_alloc_size);
        blob.read_range(0..(uncompressed_size as u64), &service, page_request);

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
        let extents = Extents::from_encoded(&extents).unwrap();
        let compression_info = CompressionInfo::new(
            chunk_size as u64,
            stored_size,
            &compressed_offsets,
            CompressionAlgorithm::Lz4,
        )
        .unwrap();
        let blob = Arc::new(Blob::new(extents, uncompressed_size as u64, Some(compression_info)));

        let (page_request, rx) = TestVecBuffer::new(uncompressed_size);
        blob.read_range(0..(uncompressed_size as u64), &service, page_request);

        assert_eq!(rx.commits(), vec![(0, chunk_size), (chunk_size as u64, chunk_size)]);
        assert_eq!(rx.output(), uncompressed_data);
    }

    #[test]
    fn test_read_range_invalid_range_noop() {
        let service = FakeBlockService::new(vec![0u8; 8192]);
        let extents = Extents::encode_extents(&[Extent::new(0..8192, Some(0))]);
        let extents = Extents::from_encoded(&extents).unwrap();
        let blob = Arc::new(Blob::new(extents, 8192, None));

        let (page_request, rx) = TestVecBuffer::new_with_offset(0, 4096);
        // start >= end should be a no-op returning Ok(())
        blob.read_range(4096..4096, &service, page_request);
        assert_eq!(rx.commits().len(), 0);
    }

    #[test]
    fn test_blob_getters() {
        let extents_raw = Extents::encode_extents(&[Extent::new(0..8192, Some(0))]);
        let extents = Extents::from_encoded(&extents_raw).unwrap();
        let uncompressed_size = 8192u64;

        let blob_uncompressed = Blob::new(extents, uncompressed_size, None);
        assert_eq!(blob_uncompressed.uncompressed_size(), 8192);
        assert!(blob_uncompressed.compression_info().is_none());

        let compression_info =
            CompressionInfo::new(32768, 4096, &[0], CompressionAlgorithm::Zstd).unwrap();
        let blob_compressed = Blob::new(
            Extents::from_encoded(&extents_raw).unwrap(),
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
        let extents = Extents::from_encoded(&extents).unwrap();
        let blob = Blob::new(extents, 8192, None);

        let (page_request, rx) = TestVecBuffer::new(8192);

        blob.read_range(0..8192, &FailingBlockService, page_request);
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
        let extents = Extents::from_encoded(&extents).unwrap();
        let blob = Blob::new(extents, block_count * BLOCK_SIZE, None);

        let (page_request, rx) = TestVecBuffer::new(expected_data.len());
        blob.read_range(0..(block_count * BLOCK_SIZE), &service, page_request);

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
        let extents = Extents::from_encoded(&extents).unwrap();
        let blob = Blob::new(extents, uncompressed_size, None);

        let (page_request, rx) = TestVecBuffer::new(8192);
        blob.read_range(0..8192, &service, page_request);

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
        let extents = Extents::from_encoded(&extents).unwrap();
        let compression_info = CompressionInfo::new(
            chunk_size as u64,
            stored_size,
            &compressed_offsets,
            CompressionAlgorithm::Zstd,
        )
        .unwrap();
        let blob = Blob::new(extents, uncompressed_size as u64, Some(compression_info));

        let tail_start = chunk_size as u64 * 2;
        let (page_request, rx) = TestVecBuffer::new_with_offset(32768, tail_start);
        blob.read_range(tail_start..(uncompressed_size as u64), &service, page_request);

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
        let extents = Extents::from_encoded(&extents).unwrap();
        let compression_info = CompressionInfo::new(
            chunk_size as u64,
            stored_size,
            &compressed_offsets,
            CompressionAlgorithm::Zstd,
        )
        .unwrap();
        let blob = Blob::new(extents, uncompressed_size as u64, Some(compression_info));

        // Pre-fill destination buffer with 0xFF bytes to verify tail zeroing
        let (mut page_request, rx) = TestVecBuffer::new(65536);
        page_request.data.fill(0xFF);
        blob.read_range(0..(uncompressed_size as u64), &service, page_request);

        assert_eq!(rx.commits(), vec![(0, chunk_size), (chunk_size as u64, chunk_size)]);
        assert_eq!(&rx.output()[..uncompressed_size], &uncompressed_data[..]);
        assert_eq!(&rx.output()[uncompressed_size..65536], &[0u8; 31744]);
    }

    #[test]
    fn test_blobs_registry() {
        let extents = Extents::encode_extents(&[Extent::new(0..4096, Some(0))]);
        let extents = Extents::from_encoded(&extents).unwrap();
        let blob = Arc::new(Blob::new(extents, 4096, None));
        let blobs = Blobs::new();

        assert!(blobs.is_empty());
        assert_eq!(blobs.len(), 0);
        assert!(blobs.get(100).is_none());

        blobs.insert(100, blob.clone());
        assert_eq!(blobs.len(), 1);
        assert!(!blobs.is_empty());
        assert!(blobs.get(100).is_some());

        blobs.remove(100);
        assert!(blobs.is_empty());
    }
}
