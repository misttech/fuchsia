// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! This module contains the [`FxBlob`] node type used to represent an immutable blob persisted to
//! disk which can be read back.

use crate::constants::*;
use crate::fuchsia::directory::FxDirectory;
use crate::fuchsia::errors::map_to_status;
use crate::fuchsia::node::{FxNode, OpenedNode};
use crate::fuchsia::pager::{
    MarkDirtyRange, PageInRange, PagerBacked, PagerPacketReceiverRegistration, default_page_in,
};
use crate::fuchsia::volume::{FxVolume, READ_AHEAD_SIZE};
use crate::fxblob::atomic_vec::AtomicBitVec;
use anyhow::{Context, Error, anyhow, bail, ensure};
use delivery_blob::compression::{CompressionAlgorithm, ThreadLocalDecompressor};
use fidl_fuchsia_feedback::{Annotation, Attachment, CrashReport};
use fidl_fuchsia_mem::Buffer as MemBuffer;
use fuchsia_component_client::connect_to_protocol;
use fuchsia_merkle::{Hash, MerkleVerifier, ReadSizedMerkleVerifier};
use futures::try_join;
use fxfs::blob_metadata::{BlobFormat, BlobMetadata};
use fxfs::errors::FxfsError;
use fxfs::lock_keys;
use fxfs::log::*;
use fxfs::object_handle::{ObjectHandle, ReadObjectHandle};
use fxfs::object_store::transaction::LockKey;
use fxfs::object_store::{AttributeId, DataObjectHandle, ObjectDescriptor, StoreObjectHandle};
use fxfs::round::{round_down, round_up};
use fxfs_macros::ToWeakNode;
use std::num::NonZero;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use storage_device::buffer::{Buffer, BufferFuture, MutableBufferRef};
use zx::Status;

// When the top bit of the open count is set, it means the file has been deleted and when the count
// drops to zero, it will be tombstoned.  Once it has dropped to zero, it cannot be opened again
// (assertions will fire).
const PURGED: usize = 1 << (usize::BITS - 1);

/// Represents an immutable blob stored on Fxfs with associated an merkle tree.
#[derive(ToWeakNode)]
pub struct FxBlob {
    handle: StoreObjectHandle<FxVolume>,
    vmo: zx::Vmo,
    open_count: AtomicUsize,
    merkle_root: Hash,
    merkle_verifier: ReadSizedMerkleVerifier,
    compression_info: Option<CompressionInfo>,
    uncompressed_size: u64, // always set.
    stored_size: u64,
    pager_packet_receiver_registration: Arc<PagerPacketReceiverRegistration<Self>>,
    chunks_supplied: AtomicBitVec,
}

// Fuchsia can have many open blobs at once. The size of FxBlob is important.
static_assertions::const_assert!(size_of::<FxBlob>() <= 192);

impl FxBlob {
    pub async fn new(
        handle: StoreObjectHandle<FxVolume>,
        merkle_root: Hash,
    ) -> Result<Arc<Self>, Error> {
        let stored_size =
            handle.store().get_attribute_size(handle.object_id(), AttributeId::DATA).await?;
        let metadata = BlobMetadata::read_from(&handle).await?;
        let (uncompressed_size, compression_info) = match &metadata.format {
            BlobFormat::Uncompressed => (stored_size, None),
            BlobFormat::ChunkedZstd { uncompressed_size, chunk_size, compressed_offsets } => (
                *uncompressed_size,
                Some(CompressionInfo::new(
                    *chunk_size,
                    compressed_offsets,
                    CompressionAlgorithm::Zstd,
                )?),
            ),
            BlobFormat::ChunkedLz4 { uncompressed_size, chunk_size, compressed_offsets } => (
                *uncompressed_size,
                Some(CompressionInfo::new(
                    *chunk_size,
                    compressed_offsets,
                    CompressionAlgorithm::Lz4,
                )?),
            ),
        };
        let merkle_verifier = metadata.into_merkle_verifier(merkle_root)?;

        let min_chunk_size = min_chunk_size(&compression_info);
        let merkle_verifier =
            ReadSizedMerkleVerifier::new(merkle_verifier, min_chunk_size as usize)?;
        let chunks_supplied = AtomicBitVec::new(uncompressed_size.div_ceil(min_chunk_size));

        Ok(Arc::new_cyclic(|weak| {
            let (vmo, pager_packet_receiver_registration) = handle
                .owner()
                .pager()
                .create_vmo(weak.clone(), uncompressed_size, zx::VmoOptions::empty())
                .unwrap();
            set_vmo_name(&vmo, &merkle_root);
            Self {
                handle,
                vmo,
                open_count: AtomicUsize::new(0),
                merkle_root,
                merkle_verifier,
                compression_info,
                uncompressed_size,
                stored_size,
                pager_packet_receiver_registration: Arc::new(pager_packet_receiver_registration),
                chunks_supplied,
            }
        }))
    }

    /// Returns the new blob.
    pub fn overwrite_me(
        self: &Arc<Self>,
        handle: DataObjectHandle<FxVolume>,
        merkle_verifier: MerkleVerifier,
        compression_info: Option<CompressionInfo>,
    ) -> Arc<Self> {
        let min_chunk_size = min_chunk_size(&compression_info);
        let merkle_verifier =
            ReadSizedMerkleVerifier::new(merkle_verifier, min_chunk_size as usize)
                .expect("The chunk size should have been validated by the delivery blob parser");
        // The chunk size may have changed between the old blob and the new blob. Preserving the
        // chunks supplied bits isn't important.
        let chunks_supplied = AtomicBitVec::new(self.uncompressed_size.div_ceil(min_chunk_size));
        let vmo = self.vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let stored_size = handle.get_size();

        let new_blob = Arc::new(Self {
            handle: handle.into_store_object_handle(),
            vmo,
            open_count: AtomicUsize::new(0),
            merkle_root: self.merkle_root,
            merkle_verifier,
            compression_info,
            uncompressed_size: self.uncompressed_size,
            stored_size,
            pager_packet_receiver_registration: self.pager_packet_receiver_registration.clone(),
            chunks_supplied,
        });

        // We have tests that rely on the cache being purged and there are races where the
        // `FxBlob::drop` isn't called early enough, which can make the test flaky.
        self.handle.owner().cache().remove(self.as_ref());

        // Lock must be held until the open counts is incremented to prevent concurrent handling of
        // zero children signals.
        let receiver_lock =
            self.pager_packet_receiver_registration.receiver().set_receiver(&new_blob);
        if receiver_lock.is_strong() {
            // If there was a strong moved between them, then the counts exchange as well. It is
            // only important that the increment happen under the lock as it may handle the next
            // zero children signal, no new requests can now go to the old blob, and because
            // existing requests hold an open count reference using `try_keep_open` for the duration
            // of the request, we can immediately decrement the open count of the old blob.
            new_blob.open_count_add_one();
            self.clone().open_count_sub_one();
        }
        new_blob
    }

    pub fn root(&self) -> Hash {
        self.merkle_root
    }

    fn record_page_fault_metric(&self, range: &Range<u64>) {
        let chunk_size: u64 = min_chunk_size(&self.compression_info);

        let first_chunk = range.start / chunk_size;
        // The end of the range may not be chunk aligned if it's the last chunk.
        let last_chunk = range.end.div_ceil(chunk_size);

        let supplied_count = self.chunks_supplied.test_and_set_range(first_chunk, last_chunk);

        if supplied_count > 0 {
            self.handle
                .owner()
                .blob_resupplied_count()
                .increment(supplied_count, Ordering::Relaxed);
        }
    }

    fn allocate_buffer(&self, size: u64) -> BufferFuture<'_> {
        self.handle.store().device().allocate_buffer(size as usize)
    }

    async fn read_blocks(&self, offset: u64, buf: MutableBufferRef<'_>) -> Result<(), Error> {
        let fs = self.handle.store().filesystem();
        let guard = fs
            .lock_manager()
            .read_lock(lock_keys![LockKey::object_attribute(
                self.handle.store().store_object_id(),
                self.handle.object_id(),
                AttributeId::DATA,
            )])
            .await;
        self.handle.read_unchecked(AttributeId::DATA, offset, buf, &guard).await
    }
}

impl Drop for FxBlob {
    fn drop(&mut self) {
        let volume = self.handle.owner();
        volume.cache().remove(self);
    }
}

impl OpenedNode<FxBlob> {
    /// Creates a read-only child VMO for this blob backed by the pager. The blob cannot be purged
    /// until all child VMOs have been destroyed.
    ///
    /// *WARNING*: We need to ensure the open count is non-zero before invoking this function, so
    /// it is only implemented for [`OpenedNode<FxBlob>`]. This prevents the blob from being purged
    /// before we get a chance to register it with the pager for [`zx::Signals::VMO_ZERO_CHILDREN`].
    pub fn create_child_vmo(&self) -> Result<zx::Vmo, Status> {
        let blob = self.0.as_ref();
        let child_vmo = blob.vmo.create_child(
            zx::VmoChildOptions::REFERENCE | zx::VmoChildOptions::NO_WRITE,
            0,
            0,
        )?;
        if blob.handle.owner().pager().watch_for_zero_children(blob).map_err(map_to_status)? {
            // Take an open count so that we keep this object alive if it is otherwise closed. This
            // is only valid since we know the current open count is non-zero, otherwise we might
            // increment the open count after the blob has been purged.
            blob.open_count_add_one();
        }
        Ok(child_vmo)
    }
}

impl FxNode for FxBlob {
    fn object_id(&self) -> u64 {
        self.handle.object_id()
    }

    fn parent(&self) -> Option<Arc<FxDirectory>> {
        unreachable!(); // Add a parent back-reference if needed.
    }

    fn set_parent(&self, _parent: Arc<FxDirectory>) {
        // NOP
    }

    fn open_count_add_one(&self) {
        let old = self.open_count.fetch_add(1, Ordering::Relaxed);
        assert!(old != PURGED && old != PURGED - 1);
    }

    fn open_count_sub_one(self: Arc<Self>) {
        let old = self.open_count.fetch_sub(1, Ordering::Relaxed);
        assert!(old & !PURGED > 0);
        if old == PURGED + 1 {
            let store = self.handle.store();
            store
                .filesystem()
                .graveyard()
                .queue_tombstone_object(store.store_object_id(), self.object_id());
        }
    }

    fn object_descriptor(&self) -> ObjectDescriptor {
        ObjectDescriptor::File
    }

    fn terminate(&self) {
        self.pager_packet_receiver_registration.stop_watching_for_zero_children();
    }

    fn mark_to_be_purged(self: Arc<Self>) {
        let old = self.open_count.fetch_or(PURGED, Ordering::Relaxed);
        assert!(old & PURGED == 0);
        if old == 0 {
            let store = self.handle.store();
            store
                .filesystem()
                .graveyard()
                .queue_tombstone_object(store.store_object_id(), self.object_id());
        }
    }
}

impl PagerBacked for FxBlob {
    fn try_keep_open(self: Arc<Self>) -> Result<OpenedNode<Self>, Arc<Self>> {
        let mut old = self.open_count.load(Ordering::Relaxed);
        loop {
            if old & !PURGED == 0 {
                return Err(self);
            }
            assert!(old & !PURGED < PURGED - 1);
            match self.open_count.compare_exchange_weak(
                old,
                old + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(OpenedNode(self)),
                Err(new_value) => old = new_value,
            }
        }
    }

    fn pager(&self) -> &crate::pager::Pager {
        self.handle.owner().pager()
    }

    fn pager_packet_receiver_registration(&self) -> &PagerPacketReceiverRegistration<Self> {
        &self.pager_packet_receiver_registration
    }

    fn vmo(&self) -> &zx::Vmo {
        &self.vmo
    }

    fn page_in(self: Arc<Self>, range: PageInRange<Self>) {
        let read_ahead_size = if let Some(compression_info) = &self.compression_info {
            read_ahead_size_for_chunk_size(compression_info.chunk_size, READ_AHEAD_SIZE)
        } else {
            READ_AHEAD_SIZE
        };
        // Delegate to the generic page handling code.
        default_page_in(self, range, read_ahead_size)
    }

    fn mark_dirty(self: Arc<Self>, _range: MarkDirtyRange<Self>) {
        unreachable!();
    }

    fn on_zero_children(self: Arc<Self>) {
        self.open_count_sub_one();
    }

    fn byte_size(&self) -> u64 {
        self.uncompressed_size
    }

    async fn aligned_read(&self, range: Range<u64>) -> Result<Buffer<'_>, Error> {
        // The vmo shouldn't have full pages beyond the end of the blob so we shouldn't be getting
        // page faults for ranges beyond the end of the blob.
        ensure!(range.start < self.uncompressed_size, FxfsError::InvalidArgs);
        self.record_page_fault_metric(&range);

        let mut buffer = self.allocate_buffer(range.end - range.start).await;
        let unaligned_bytes =
            (std::cmp::min(range.end, self.uncompressed_size) - range.start) as usize;
        match &self.compression_info {
            None => self.read_blocks(range.start, buffer.as_mut()).await?,
            Some(compression_info) => {
                let compressed_offsets =
                    match compression_info.compressed_range_for_uncompressed_range(&range)? {
                        (start, None) => start..self.stored_size,
                        (start, Some(end)) => start..end.get(),
                    };
                let bs = self.handle.block_size();
                let aligned = round_down(compressed_offsets.start, bs)
                    ..round_up(compressed_offsets.end, bs).unwrap();
                let mut compressed_buf = self.allocate_buffer(aligned.end - aligned.start).await;

                let mut decompression_errors = 0;
                loop {
                    try_join!(self.read_blocks(aligned.start, compressed_buf.as_mut()), async {
                        buffer
                            .allocator()
                            .buffer_source()
                            .commit_range(buffer.range())
                            .map_err(|e| e.into())
                    })
                    .with_context(|| {
                        format!(
                            "Failed to read compressed range {:?}, len {}",
                            aligned, self.stored_size
                        )
                    })?;
                    let compressed_buf_range = (compressed_offsets.start - aligned.start) as usize
                        ..(compressed_offsets.end - aligned.start) as usize;

                    let buf = buffer.as_mut_slice();
                    let decompression_result = {
                        fxfs_trace::duration!("blob-decompress", "len" => unaligned_bytes);
                        compression_info.decompress(
                            &compressed_buf.as_slice()[compressed_buf_range],
                            &mut buf[..unaligned_bytes],
                            range.start,
                        )
                    };
                    match decompression_result {
                        Ok(()) => break,
                        Err(error) => {
                            record_decompression_error_crash_report(
                                compressed_buf.as_slice(),
                                &range,
                                &compressed_offsets,
                                &self.merkle_root,
                            )
                            .await;
                            decompression_errors += 1;
                            if decompression_errors == 2 {
                                bail!(
                                    anyhow!(FxfsError::IntegrityError)
                                        .context(format!("Decompression error: {error:?}"))
                                );
                            } else {
                                warn!(error:?; "Decompression error; retrying");
                            }
                        }
                    }
                } // loop
                if decompression_errors > 0 {
                    info!("Read succeeded on second attempt");
                }
            }
        };
        {
            // TODO(https://fxbug.dev/42073035): This should be offloaded to the kernel at which
            // point we can delete this.
            fxfs_trace::duration!("blob-verify", "len" => unaligned_bytes);
            self.merkle_verifier
                .verify(range.start as usize, &buffer.as_slice()[..unaligned_bytes])?;
        }
        // Zero the tail.
        buffer.as_mut_slice()[unaligned_bytes..].fill(0);
        Ok(buffer)
    }
}

pub struct CompressionInfo {
    chunk_size: u64,
    // The chunked compression format stores 0 as the first offset but it's not stored here. Not
    // storing the 0 avoids the allocation for blobs smaller than the chunk size.
    small_offsets: Box<[u32]>,
    large_offsets: Box<[u64]>,
    decompressor: ThreadLocalDecompressor,
}

impl CompressionInfo {
    pub fn new(
        chunk_size: u64,
        offsets: &[u64],
        compression_algorithm: CompressionAlgorithm,
    ) -> Result<Self, Error> {
        let decompressor = compression_algorithm.thread_local_decompressor();
        if chunk_size == 0 {
            return Err(FxfsError::IntegrityError.into());
        } else if offsets.is_empty() || *offsets.first().unwrap() != 0 {
            // There should always be at least 1 offset and the first offset must always be 0.
            Err(FxfsError::IntegrityError.into())
        } else if !offsets.array_windows().all(|[a, b]| a < b) {
            // The offsets must be in ascending order.
            Err(FxfsError::IntegrityError.into())
        } else if offsets.len() == 1 {
            // Simple case where the blob is smaller than the chunk size so only the 0 offset is
            // present. The 0 isn't stored so no allocation is necessary.
            Ok(Self {
                chunk_size,
                small_offsets: Box::default(),
                large_offsets: Box::default(),
                decompressor,
            })
        } else if *offsets.last().unwrap() <= u32::MAX as u64 {
            // Check the last index first since most compressed blobs are going to be smaller
            // than 4GiB making all offsets small.
            Ok(Self {
                chunk_size,
                small_offsets: offsets[1..].iter().map(|x| *x as u32).collect(),
                large_offsets: Box::default(),
                decompressor,
            })
        } else {
            // The partition point is the index of the first compressed offset that's > u32::MAX.
            let partition_point = offsets.partition_point(|&x| x <= u32::MAX as u64);
            Ok(Self {
                chunk_size,
                small_offsets: offsets[1..partition_point].iter().map(|x| *x as u32).collect(),
                large_offsets: offsets[partition_point..].into(),
                decompressor,
            })
        }
    }

    fn compressed_range_for_uncompressed_range(
        &self,
        range: &Range<u64>,
    ) -> Result<(u64, Option<NonZero<u64>>), Error> {
        ensure!(range.start.is_multiple_of(self.chunk_size), FxfsError::Inconsistent);
        ensure!(range.start < range.end, FxfsError::Inconsistent);

        let start_chunk_index = (range.start / self.chunk_size) as usize;
        let start_offset = self
            .compressed_offset_for_chunk_index(start_chunk_index)
            .ok_or(FxfsError::OutOfRange)?;

        // The end of the range may not be aligned to the chunk size for the last chunk.
        let end_chunk_index = range.end.div_ceil(self.chunk_size) as usize;
        let end_offset = match self.compressed_offset_for_chunk_index(end_chunk_index) {
            None => None,
            Some(offset) => {
                // This isn't the last chunk so the end must be aligned.
                ensure!(range.end.is_multiple_of(self.chunk_size), FxfsError::Inconsistent);
                // `CompressionInfo::new` validates that all of the offsets are ascending. The end
                // of the range is greater than the start so this can never be 0.
                Some(NonZero::new(offset).unwrap())
            }
        };
        Ok((start_offset, end_offset))
    }

    fn compressed_offset_for_chunk_index(&self, chunk_index: usize) -> Option<u64> {
        // The "0" compressed offset isn't stored so all of the indices are shifted left by 1.
        if chunk_index == 0 {
            Some(0)
        } else if chunk_index - 1 < self.small_offsets.len() {
            Some(self.small_offsets[chunk_index - 1] as u64)
        } else if chunk_index - 1 - self.small_offsets.len() < self.large_offsets.len() {
            Some(self.large_offsets[chunk_index - 1 - self.small_offsets.len()])
        } else {
            None
        }
    }

    /// Decompress the bytes of `src` into `dst`.
    ///   - `src` is allowed to span multiple chunks.
    ///   - `dst` must have the exact size of the uncompressed bytes.
    ///   - `dst_start_offset` is the location of the uncompressed bytes within the blob and must be
    ///     chunk aligned. This is necessary for determining the chunk boundaries in `src`.
    fn decompress(
        &self,
        mut src: &[u8],
        mut dst: &mut [u8],
        dst_start_offset: u64,
    ) -> Result<(), Error> {
        ensure!(dst_start_offset.is_multiple_of(self.chunk_size), FxfsError::Inconsistent);

        let start_chunk_index = (dst_start_offset / self.chunk_size) as usize;
        let chunk_count = dst.len().div_ceil(self.chunk_size as usize);
        let mut start_offset = self
            .compressed_offset_for_chunk_index(start_chunk_index)
            .ok_or(FxfsError::Inconsistent)?;

        // Decompress each chunk individually.
        for chunk_index in start_chunk_index..(start_chunk_index + chunk_count) {
            match self.compressed_offset_for_chunk_index(chunk_index + 1) {
                Some(end_offset) => {
                    let (to_decompress, src_remaining) = src
                        .split_at_checked((end_offset - start_offset) as usize)
                        .ok_or(FxfsError::Inconsistent)?;
                    let (to_decompress_into, dst_remaining) = dst
                        .split_at_mut_checked(self.chunk_size as usize)
                        .ok_or(FxfsError::Inconsistent)?;

                    let decompressed_bytes = self.decompressor.decompress_into(
                        to_decompress,
                        to_decompress_into,
                        chunk_index,
                    )?;
                    ensure!(
                        decompressed_bytes == to_decompress_into.len(),
                        FxfsError::Inconsistent
                    );
                    src = src_remaining;
                    dst = dst_remaining;
                    start_offset = end_offset;
                }
                None => {
                    let decompressed_bytes =
                        self.decompressor.decompress_into(src, dst, chunk_index)?;
                    ensure!(decompressed_bytes == dst.len(), FxfsError::Inconsistent);
                }
            }
        }

        Ok(())
    }
}

fn set_vmo_name(vmo: &zx::Vmo, merkle_root: &Hash) {
    let trimmed_merkle = &merkle_root.to_string()[0..BLOB_NAME_HASH_LENGTH];
    let name = format!("{BLOB_NAME_PREFIX}{trimmed_merkle}");
    let name = zx::Name::new(&name).unwrap();
    vmo.set_name(&name).unwrap();
}

fn min_chunk_size(compression_info: &Option<CompressionInfo>) -> u64 {
    if let Some(compression_info) = compression_info {
        read_ahead_size_for_chunk_size(compression_info.chunk_size, READ_AHEAD_SIZE)
    } else {
        READ_AHEAD_SIZE
    }
}

fn read_ahead_size_for_chunk_size(chunk_size: u64, suggested_read_ahead_size: u64) -> u64 {
    if chunk_size >= suggested_read_ahead_size {
        chunk_size
    } else {
        round_down(suggested_read_ahead_size, chunk_size)
    }
}

async fn record_decompression_error_crash_report(
    compressed_buf: &[u8],
    uncompressed_offsets: &Range<u64>,
    compressed_offsets: &Range<u64>,
    merkle_root: &Hash,
) {
    static DONE_ONCE: AtomicBool = AtomicBool::new(false);
    if !DONE_ONCE.swap(true, Ordering::Relaxed) {
        if let Ok(proxy) = connect_to_protocol::<fidl_fuchsia_feedback::CrashReporterMarker>() {
            let size = compressed_buf.len() as u64;
            let vmo = zx::Vmo::create(size).unwrap();
            vmo.write(compressed_buf, 0).unwrap();
            if let Err(e) = proxy
                .file_report(CrashReport {
                    program_name: Some("fxfs".to_string()),
                    crash_signature: Some("fuchsia-fxfs-decompression_error".to_string()),
                    is_fatal: Some(false),
                    annotations: Some(vec![
                        Annotation {
                            key: "fxfs.range".to_string(),
                            value: format!("{:?}", uncompressed_offsets),
                        },
                        Annotation {
                            key: "fxfs.compressed_offsets".to_string(),
                            value: format!("{:?}", compressed_offsets),
                        },
                        Annotation {
                            key: "fxfs.merkle_root".to_string(),
                            value: format!("{}", merkle_root),
                        },
                    ]),
                    attachments: Some(vec![Attachment {
                        key: "fxfs_compressed_data".to_string(),
                        value: MemBuffer { vmo, size },
                    }]),
                    ..Default::default()
                })
                .await
            {
                error!(e:?; "Failed to file crash report");
            } else {
                warn!("Filed crash report for decompression error");
            }
        } else {
            error!("Failed to connect to crash report service");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fuchsia::fxblob::testing::{BlobFixture, new_blob_fixture};
    use crate::fuchsia::pager::PageInRange;
    use crate::fxblob::testing::open_blob_fixture;
    use assert_matches::assert_matches;
    use delivery_blob::CompressionMode;
    use delivery_blob::compression::{ChunkedArchiveOptions, CompressionAlgorithm};
    use fuchsia_async as fasync;
    use fuchsia_async::epoch::Epoch;
    use fxfs_make_blob_image::FxBlobBuilder;
    use storage_device::DeviceHolder;
    use storage_device::fake_device::FakeDevice;

    const BLOCK_SIZE: u64 = fuchsia_merkle::BLOCK_SIZE as u64;
    const CHUNK_SIZE: usize = 32 * 1024;

    #[fasync::run(10, test)]
    async fn test_empty_blob() {
        let fixture = new_blob_fixture().await;

        let data = vec![];
        let hash = fixture.write_blob(&data, CompressionMode::Never).await;
        assert_eq!(fixture.read_blob(hash).await, data);

        fixture.close().await;
    }

    #[fasync::run(10, test)]
    async fn test_large_blob() {
        let fixture = new_blob_fixture().await;

        let data = vec![3; 3_000_000];
        let hash = fixture.write_blob(&data, CompressionMode::Never).await;

        assert_eq!(fixture.read_blob(hash).await, data);

        fixture.close().await;
    }

    #[fasync::run(10, test)]
    async fn test_large_compressed_blob() {
        let fixture = new_blob_fixture().await;

        let data = vec![3; 3_000_000];
        let hash = fixture.write_blob(&data, CompressionMode::Always).await;

        assert_eq!(fixture.read_blob(hash).await, data);

        fixture.close().await;
    }

    #[fasync::run(10, test)]
    async fn test_non_page_aligned_blob() {
        let fixture = new_blob_fixture().await;

        let page_size = zx::system_get_page_size() as usize;
        let data = vec![0xffu8; page_size - 1];
        let hash = fixture.write_blob(&data, CompressionMode::Never).await;
        assert_eq!(fixture.read_blob(hash).await, data);

        {
            let vmo = fixture.get_blob_vmo(hash).await;
            let mut buf = vec![0x11u8; page_size];
            vmo.read(&mut buf[..], 0).expect("vmo read failed");
            assert_eq!(data, buf[..data.len()]);
            // Ensure the tail is zeroed
            assert_eq!(buf[data.len()], 0);
        }

        fixture.close().await;
    }

    #[fasync::run(10, test)]
    async fn test_blob_invalid_contents() {
        let fixture = new_blob_fixture().await;

        let data = vec![0xffu8; (READ_AHEAD_SIZE + BLOCK_SIZE) as usize];
        let hash = fixture.write_blob(&data, CompressionMode::Never).await;
        let name = format!("{}", hash);

        {
            // Overwrite the second read-ahead window.  The first window should successfully verify.
            let handle = fixture.get_blob_handle(&name).await;
            let mut transaction =
                handle.new_transaction().await.expect("failed to create transaction");
            let mut buf = handle.allocate_buffer(BLOCK_SIZE as usize).await;
            buf.as_mut_slice().fill(0);
            handle
                .txn_write(&mut transaction, READ_AHEAD_SIZE, buf.as_ref())
                .await
                .expect("txn_write failed");
            transaction.commit().await.expect("failed to commit transaction");
        }

        {
            let blob_vmo = fixture.get_blob_vmo(hash).await;
            let mut buf = vec![0; BLOCK_SIZE as usize];
            assert_matches!(blob_vmo.read(&mut buf[..], 0), Ok(_));
            assert_matches!(
                blob_vmo.read(&mut buf[..], READ_AHEAD_SIZE),
                Err(zx::Status::IO_DATA_INTEGRITY)
            );
        }

        fixture.close().await;
    }

    #[fasync::run(10, test)]
    async fn test_lz4_blob() {
        let device = DeviceHolder::new(FakeDevice::new(16384, 512));
        let blob_data = vec![0xAA; 68 * 1024];
        let fxblob_builder = FxBlobBuilder::new(device).await.unwrap();
        let blob = fxblob_builder
            .generate_blob(blob_data.clone(), Some(CompressionAlgorithm::Lz4))
            .unwrap();
        let blob_hash = blob.hash();
        fxblob_builder.install_blob(&blob).await.unwrap();
        let device = fxblob_builder.finalize().await.unwrap().0;
        device.reopen(/*read_only=*/ false);
        let fixture = open_blob_fixture(device).await;

        assert_eq!(fixture.read_blob(blob_hash).await, blob_data);

        fixture.close().await;
    }

    #[fasync::run(10, test)]
    async fn test_blob_vmos_are_immutable() {
        let fixture = new_blob_fixture().await;

        let data = vec![0xffu8; 500];
        let hash = fixture.write_blob(&data, CompressionMode::Never).await;
        let blob_vmo = fixture.get_blob_vmo(hash).await;

        // The VMO shouldn't be resizable.
        assert_matches!(blob_vmo.set_size(20), Err(_));

        // The VMO shouldn't be writable.
        assert_matches!(blob_vmo.write(b"overwrite", 0), Err(_));

        // The VMO's content size shouldn't be modifiable.
        assert_matches!(blob_vmo.set_stream_size(20), Err(_));

        fixture.close().await;
    }

    const COMPRESSED_BLOB_CHUNK_SIZE: u64 = 32 * 1024;
    const MAX_SMALL_OFFSET: u64 = u32::MAX as u64;
    const ZSTD: CompressionAlgorithm = CompressionAlgorithm::Zstd;

    #[fuchsia::test]
    fn test_compression_info_offsets_must_start_with_zero() {
        assert!(CompressionInfo::new(COMPRESSED_BLOB_CHUNK_SIZE, &[], ZSTD).is_err());
        assert!(CompressionInfo::new(COMPRESSED_BLOB_CHUNK_SIZE, &[1], ZSTD).is_err());
        assert!(CompressionInfo::new(COMPRESSED_BLOB_CHUNK_SIZE, &[0], ZSTD).is_ok());
    }

    #[fuchsia::test]
    fn test_compression_info_offsets_must_be_sorted() {
        assert!(CompressionInfo::new(COMPRESSED_BLOB_CHUNK_SIZE, &[0, 1, 2], ZSTD).is_ok());
        assert!(CompressionInfo::new(COMPRESSED_BLOB_CHUNK_SIZE, &[0, 2, 1], ZSTD).is_err());
        assert!(CompressionInfo::new(COMPRESSED_BLOB_CHUNK_SIZE, &[0, 1, 1], ZSTD).is_err());
    }

    #[fuchsia::test]
    fn test_compression_info_splitting_offsets() {
        // Single chunk blob doesn't store any offsets.
        let compression_info =
            CompressionInfo::new(COMPRESSED_BLOB_CHUNK_SIZE, &[0], ZSTD).unwrap();
        assert!(compression_info.small_offsets.is_empty());
        assert!(compression_info.large_offsets.is_empty());

        // Single small offset.
        let compression_info =
            CompressionInfo::new(COMPRESSED_BLOB_CHUNK_SIZE, &[0, 10], ZSTD).unwrap();
        assert_eq!(&*compression_info.small_offsets, &[10]);
        assert!(compression_info.large_offsets.is_empty());

        // Multiple small offsets.
        let compression_info =
            CompressionInfo::new(COMPRESSED_BLOB_CHUNK_SIZE, &[0, 10, 20, 30], ZSTD).unwrap();
        assert_eq!(&*compression_info.small_offsets, &[10, 20, 30]);
        assert!(compression_info.large_offsets.is_empty());

        // One less than the largest small offset.
        let compression_info =
            CompressionInfo::new(COMPRESSED_BLOB_CHUNK_SIZE, &[0, MAX_SMALL_OFFSET - 1], ZSTD)
                .unwrap();
        assert_eq!(&*compression_info.small_offsets, &[u32::MAX - 1]);
        assert!(compression_info.large_offsets.is_empty());

        // The largest small offset.
        let compression_info =
            CompressionInfo::new(COMPRESSED_BLOB_CHUNK_SIZE, &[0, MAX_SMALL_OFFSET], ZSTD).unwrap();
        assert_eq!(&*compression_info.small_offsets, &[u32::MAX]);
        assert!(compression_info.large_offsets.is_empty());

        // The smallest large offset.
        let compression_info =
            CompressionInfo::new(COMPRESSED_BLOB_CHUNK_SIZE, &[0, MAX_SMALL_OFFSET + 1], ZSTD)
                .unwrap();
        assert!(compression_info.small_offsets.is_empty());
        assert_eq!(&*compression_info.large_offsets, &[MAX_SMALL_OFFSET + 1]);

        // Multiple offsets around boundary between small and large offsets.
        let compression_info = CompressionInfo::new(
            COMPRESSED_BLOB_CHUNK_SIZE,
            &[0, MAX_SMALL_OFFSET - 1, MAX_SMALL_OFFSET, MAX_SMALL_OFFSET + 1],
            ZSTD,
        )
        .unwrap();
        assert_eq!(&*compression_info.small_offsets, &[u32::MAX - 1, u32::MAX]);
        assert_eq!(&*compression_info.large_offsets, &[MAX_SMALL_OFFSET + 1]);

        // Single large offset.
        let compression_info =
            CompressionInfo::new(COMPRESSED_BLOB_CHUNK_SIZE, &[0, MAX_SMALL_OFFSET + 10], ZSTD)
                .unwrap();
        assert!(compression_info.small_offsets.is_empty());
        assert_eq!(&*compression_info.large_offsets, &[MAX_SMALL_OFFSET + 10]);

        // Multiple large offsets.
        let compression_info = CompressionInfo::new(
            COMPRESSED_BLOB_CHUNK_SIZE,
            &[0, MAX_SMALL_OFFSET + 10, MAX_SMALL_OFFSET + 20],
            ZSTD,
        )
        .unwrap();
        assert!(compression_info.small_offsets.is_empty());
        assert_eq!(
            &*compression_info.large_offsets,
            &[MAX_SMALL_OFFSET + 10, MAX_SMALL_OFFSET + 20]
        );

        // Small and large offsets.
        let compression_info = CompressionInfo::new(
            COMPRESSED_BLOB_CHUNK_SIZE,
            &[0, 10, 20, MAX_SMALL_OFFSET + 10, MAX_SMALL_OFFSET + 20],
            ZSTD,
        )
        .unwrap();
        assert_eq!(&*compression_info.small_offsets, &[10, 20]);
        assert_eq!(
            &*compression_info.large_offsets,
            &[MAX_SMALL_OFFSET + 10, MAX_SMALL_OFFSET + 20]
        );
    }

    #[fuchsia::test]
    fn test_compression_info_compressed_range_for_uncompressed_range() {
        fn check_compression_ranges(
            offsets: &[u64],
            expected_ranges: &[(u64, Option<u64>)],
            chunk_size: u64,
            read_ahead_size: u64,
        ) {
            let compression_info = CompressionInfo::new(chunk_size, offsets, ZSTD).unwrap();
            for (i, range) in expected_ranges.iter().enumerate() {
                let i = i as u64;
                let result = compression_info
                    .compressed_range_for_uncompressed_range(
                        &(i * read_ahead_size..(i + 1) * read_ahead_size),
                    )
                    .unwrap();
                assert_eq!(result, (range.0, range.1.map(|end| NonZero::new(end).unwrap())));
            }
        }
        check_compression_ranges(
            &[0, 10, 20, 30],
            &[(0, Some(10)), (10, Some(20)), (20, Some(30)), (30, None)],
            COMPRESSED_BLOB_CHUNK_SIZE,
            COMPRESSED_BLOB_CHUNK_SIZE,
        );
        check_compression_ranges(
            &[0, 10, 20, 30],
            &[(0, Some(20)), (20, None)],
            COMPRESSED_BLOB_CHUNK_SIZE,
            COMPRESSED_BLOB_CHUNK_SIZE * 2,
        );
        check_compression_ranges(
            &[0, 10, 20, 30],
            &[(0, None)],
            COMPRESSED_BLOB_CHUNK_SIZE,
            COMPRESSED_BLOB_CHUNK_SIZE * 4,
        );
        check_compression_ranges(
            &[0, 10, 20, 30, MAX_SMALL_OFFSET + 10],
            &[(0, Some(MAX_SMALL_OFFSET + 10)), (MAX_SMALL_OFFSET + 10, None)],
            COMPRESSED_BLOB_CHUNK_SIZE,
            COMPRESSED_BLOB_CHUNK_SIZE * 4,
        );
        check_compression_ranges(
            &[
                0,
                10,
                20,
                30,
                MAX_SMALL_OFFSET + 10,
                MAX_SMALL_OFFSET + 20,
                MAX_SMALL_OFFSET + 30,
                MAX_SMALL_OFFSET + 40,
                MAX_SMALL_OFFSET + 50,
            ],
            &[
                (0, Some(20)),
                (20, Some(MAX_SMALL_OFFSET + 10)),
                (MAX_SMALL_OFFSET + 10, Some(MAX_SMALL_OFFSET + 30)),
                (MAX_SMALL_OFFSET + 30, Some(MAX_SMALL_OFFSET + 50)),
            ],
            COMPRESSED_BLOB_CHUNK_SIZE,
            COMPRESSED_BLOB_CHUNK_SIZE * 2,
        );
    }

    #[fuchsia::test]
    fn test_compression_info_compressed_range_for_uncompressed_range_errors() {
        let compression_info = CompressionInfo::new(
            COMPRESSED_BLOB_CHUNK_SIZE,
            &[
                0,
                10,
                20,
                30,
                MAX_SMALL_OFFSET + 10,
                MAX_SMALL_OFFSET + 20,
                MAX_SMALL_OFFSET + 30,
                MAX_SMALL_OFFSET + 40,
                MAX_SMALL_OFFSET + 50,
            ],
            ZSTD,
        )
        .unwrap();

        // The start of reads must be chunk aligned.
        assert!(
            compression_info
                .compressed_range_for_uncompressed_range(&(1..COMPRESSED_BLOB_CHUNK_SIZE),)
                .is_err()
        );

        // Reading entirely past the last offset isn't allowed.
        assert!(
            compression_info
                .compressed_range_for_uncompressed_range(
                    &(COMPRESSED_BLOB_CHUNK_SIZE * 9..COMPRESSED_BLOB_CHUNK_SIZE * 12),
                )
                .is_err()
        );

        // Reading a different amount than the read-ahead size isn't allowed for middle offsets.
        assert!(
            compression_info
                .compressed_range_for_uncompressed_range(&(0..COMPRESSED_BLOB_CHUNK_SIZE + 1),)
                .is_err()
        );
        assert!(
            compression_info
                .compressed_range_for_uncompressed_range(&(0..COMPRESSED_BLOB_CHUNK_SIZE - 1),)
                .is_err()
        );
        assert!(
            compression_info
                .compressed_range_for_uncompressed_range(
                    &(COMPRESSED_BLOB_CHUNK_SIZE..COMPRESSED_BLOB_CHUNK_SIZE * 2 + 1),
                )
                .is_err()
        );
        assert!(
            compression_info
                .compressed_range_for_uncompressed_range(
                    &(COMPRESSED_BLOB_CHUNK_SIZE..COMPRESSED_BLOB_CHUNK_SIZE * 2 - 1),
                )
                .is_err()
        );

        // Reading less than the read-ahead size for the last offset is allowed.
        assert!(
            compression_info
                .compressed_range_for_uncompressed_range(
                    &(COMPRESSED_BLOB_CHUNK_SIZE * 8..COMPRESSED_BLOB_CHUNK_SIZE * 8 + 4096),
                )
                .is_ok()
        );
    }

    #[fuchsia::test]
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

    fn build_compression_info(size: usize) -> (CompressionInfo, Vec<u8>, Vec<u8>) {
        let options =
            ChunkedArchiveOptions::V3 { compression_algorithm: CompressionAlgorithm::Lz4 };
        let mut compressor = options.compressor();
        let mut uncompressed_data = Vec::with_capacity(size);
        {
            let mut run_length = 1;
            let mut run_value: u8 = 0;
            while uncompressed_data.len() < size {
                uncompressed_data
                    .resize(std::cmp::min(uncompressed_data.len() + run_length, size), run_value);
                run_length = (run_length + 1) % 19 + 1;
                run_value = (run_value + 1) % 17;
            }
        }
        let mut compressed_offsets = vec![0];
        let mut compressed_data = vec![];
        for chunk in uncompressed_data.chunks(CHUNK_SIZE) {
            let mut compressed_chunk = compressor.compress(chunk, 0).unwrap();
            compressed_data.append(&mut compressed_chunk);
            compressed_offsets.push(compressed_data.len() as u64);
        }
        compressed_offsets.pop();
        (
            CompressionInfo::new(CHUNK_SIZE as u64, &compressed_offsets, CompressionAlgorithm::Lz4)
                .unwrap(),
            compressed_data,
            uncompressed_data,
        )
    }

    #[fuchsia::test]
    fn test_compression_info_decompress_single_chunk() {
        let (compression_info, compressed_data, uncompressed_data) =
            build_compression_info(CHUNK_SIZE);
        let mut decompressed_data = vec![0u8; CHUNK_SIZE + 1];

        compression_info
            .decompress(&compressed_data, &mut decompressed_data[0..CHUNK_SIZE], 0)
            .expect("failed to decompress");
        assert_eq!(uncompressed_data, decompressed_data[0..CHUNK_SIZE]);

        // Too small of destination buffer.
        compression_info
            .decompress(&compressed_data, &mut decompressed_data[0..CHUNK_SIZE - 1], 0)
            .expect_err("decompression should fail");

        // Too large of destination buffer.
        compression_info
            .decompress(&compressed_data, &mut decompressed_data[0..CHUNK_SIZE - 1], 0)
            .expect_err("decompression should fail");
    }

    #[fuchsia::test]
    fn test_compression_info_decompress_multiple_chunks() {
        fn slice_for_chunks<'a>(
            compressed_data: &'a [u8],
            compression_info: &CompressionInfo,
            chunks: Range<u64>,
        ) -> &'a [u8] {
            let (start, end) = compression_info
                .compressed_range_for_uncompressed_range(
                    &(chunks.start * CHUNK_SIZE as u64..chunks.end * CHUNK_SIZE as u64),
                )
                .unwrap();
            let end = end.map_or(compressed_data.len() as u64, NonZero::<u64>::get);
            &compressed_data[start as usize..end as usize]
        }

        const BLOB_SIZE: usize = CHUNK_SIZE * 4 + 4096;
        let (compression_info, compressed_data, uncompressed_data) =
            build_compression_info(BLOB_SIZE);
        let mut decompressed_data = vec![0u8; BLOB_SIZE];

        // Decompress the entire blob.
        compression_info
            .decompress(&compressed_data, &mut decompressed_data, 0)
            .expect("failed to decompress");
        assert_eq!(uncompressed_data, decompressed_data);

        // Decompress just the whole chunks.
        compression_info
            .decompress(
                slice_for_chunks(&compressed_data, &compression_info, 0..4),
                &mut decompressed_data[0..CHUNK_SIZE * 4],
                0,
            )
            .expect("failed to decompress");
        assert_eq!(&uncompressed_data[0..CHUNK_SIZE], &decompressed_data[0..CHUNK_SIZE]);

        // Too small of destination buffer for whole chunks.
        compression_info
            .decompress(
                slice_for_chunks(&compressed_data, &compression_info, 0..4),
                &mut decompressed_data[0..CHUNK_SIZE * 4 - 1],
                0,
            )
            .expect_err("decompression should fail");

        // Too large of destination buffer for whole chunks.
        compression_info
            .decompress(
                slice_for_chunks(&compressed_data, &compression_info, 0..4),
                &mut decompressed_data[0..CHUNK_SIZE * 4 + 1],
                0,
            )
            .expect_err("decompression should fail");

        // Decompress just the tail.
        let partial_chunk = slice_for_chunks(&compressed_data, &compression_info, 4..5);
        compression_info
            .decompress(partial_chunk, &mut decompressed_data[0..4096], CHUNK_SIZE as u64 * 4)
            .expect("failed to decompress");
        assert_eq!(&uncompressed_data[CHUNK_SIZE * 4..], &decompressed_data[0..4096]);

        // Too small of destination buffer for the tail.
        compression_info
            .decompress(partial_chunk, &mut decompressed_data[0..4095], CHUNK_SIZE as u64 * 4)
            .expect_err("decompression should fail");

        // Too large of destination buffer for the tail.
        compression_info
            .decompress(partial_chunk, &mut decompressed_data[0..4097], CHUNK_SIZE as u64 * 4)
            .expect_err("decompression should fail");
    }

    #[fasync::run(10, test)]
    async fn test_refault_metric() {
        let fixture = new_blob_fixture().await;
        {
            let volume = fixture.volume().volume().clone();
            const FILE_SIZE: u64 = READ_AHEAD_SIZE * 4 - 4096;
            let data = vec![0xffu8; FILE_SIZE as usize];
            let hash = fixture.write_blob(&data, CompressionMode::Never).await;

            let blob = fixture.get_opened_blob(hash).await.unwrap();
            assert_eq!(blob.chunks_supplied.len(), 4);
            // Nothing has been read yet.
            assert_eq!(&blob.chunks_supplied.get(), &[false, false, false, false]);

            blob.vmo.read_to_vec::<u8>(4096, 4096).unwrap();

            assert_eq!(&blob.chunks_supplied.get(), &[true, false, false, false]);

            blob.vmo.read_to_vec::<u8>(READ_AHEAD_SIZE * 2 + 4096, READ_AHEAD_SIZE).unwrap();
            assert_eq!(&blob.chunks_supplied.get(), &[true, false, true, true]);

            // We have loaded pages, but only once each.
            assert_eq!(volume.blob_resupplied_count().read(Ordering::SeqCst), 0);

            // Re-read some pages.

            // We can't evict pages from the VMO to get the kernel to resupply them but we can call
            // page_in directly and wait for the counters to change.
            blob.clone().page_in(PageInRange::new(
                FILE_SIZE - READ_AHEAD_SIZE..FILE_SIZE,
                blob.dup(),
                Epoch::global().guard(),
            ));
            Epoch::global().barrier().await;

            assert_eq!(volume.blob_resupplied_count().read(Ordering::SeqCst), 2);
        }

        fixture.close().await;
    }
}
