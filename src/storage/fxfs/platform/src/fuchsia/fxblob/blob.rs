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
use anyhow::{Context, Error, anyhow, bail, ensure};
use delivery_blob::compression::{CompressionAlgorithm, CompressionInfo};
use fidl_fuchsia_feedback::{Annotation, Attachment, CrashReport};
use fidl_fuchsia_mem::Buffer as MemBuffer;
use fuchsia_component_client::connect_to_protocol;
use fuchsia_merkle::{Hash, MerkleVerifier, ReadSizedMerkleVerifier};
use futures::{StreamExt, TryStreamExt, try_join};
use fxfs::blob_metadata::{BlobFormat, BlobMetadata, FxfsBlobMetadataExt};
use fxfs::errors::FxfsError;
use fxfs::lock_keys;
use fxfs::log::*;
use fxfs::object_handle::ObjectHandle;
use fxfs::object_store::transaction::LockKey;
use fxfs::object_store::{AttributeId, DataObjectHandle, ObjectDescriptor, StoreObjectHandle};
use fxfs::round::round_down;
use fxfs_macros::ToWeakNode;
use mapping::Extent as MappingExtent;
use refaults_vmo::AtomicBitVec;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use storage_device::buffer::{Buffer, BufferFuture, MutableBufferRef};
use zx::Status;

/// The extent mappings for a blob's data and merkle attributes.
///
/// Each `MappingExtent` maps a logical byte range to a physical device offset.
/// Extents are sorted by logical offset and non-overlapping.
pub struct BlobExtents {
    pub data: Vec<MappingExtent>,
    pub merkle: Vec<MappingExtent>,
}

// When the top bit of the open count is set, it means the file has been deleted and when the count
// drops to zero, it will be tombstoned.  Once it has dropped to zero, it cannot be opened again
// (assertions will fire).
const PURGED: usize = 1 << (usize::BITS - 1);

const EMPTY_BLOB_HASH: Hash = Hash::from_array([
    0x15, 0xec, 0x7b, 0xf0, 0xb5, 0x07, 0x32, 0xb4, 0x9f, 0x82, 0x28, 0xe0, 0x7d, 0x24, 0x36, 0x53,
    0x38, 0xf9, 0xe3, 0xab, 0x99, 0x4b, 0x00, 0xaf, 0x08, 0xe5, 0xa3, 0xbf, 0xfe, 0x55, 0xfd, 0x8b,
]);

/// State for serving page requests via Fxfs's internal pager.
///
/// Only populated when `use_driver_blob_paging` is false; when driver blob paging is enabled,
/// `pkg-cache` creates and pages the blob VMOs directly via the block driver mapper.
struct FxBlobPagerState {
    vmo: zx::Vmo,
    merkle_verifier: ReadSizedMerkleVerifier,
    compression_info: Option<CompressionInfo>,
    pager_packet_receiver_registration: Arc<PagerPacketReceiverRegistration<FxBlob>>,
    chunks_supplied: AtomicBitVec,
}

/// Represents an immutable blob stored on Fxfs with associated an merkle tree.
#[derive(ToWeakNode)]
pub struct FxBlob {
    handle: StoreObjectHandle<FxVolume>,
    open_count: AtomicUsize,
    merkle_root: Hash,
    uncompressed_size: u64, // always set.
    pager_state: Option<FxBlobPagerState>,
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
        let uncompressed_size = match &metadata.format {
            BlobFormat::Uncompressed => stored_size,
            BlobFormat::ChunkedZstd { uncompressed_size, .. }
            | BlobFormat::ChunkedLz4 { uncompressed_size, .. } => *uncompressed_size,
        };

        // Check the uncompressed size of the blob against the number of merkle leaves. This only
        // ensures that the uncompressed size lands within the same merkle block as the true
        // uncompressed size. To fully validate the uncompressed size requires reading in the last
        // merkle block and verifying it.
        let expect_merkle_hashes = if uncompressed_size <= fuchsia_merkle::BLOCK_SIZE as u64 {
            0
        } else {
            uncompressed_size.div_ceil(fuchsia_merkle::BLOCK_SIZE as u64)
        };
        ensure!(
            metadata.merkle_leaves.len() as u64 == expect_merkle_hashes,
            FxfsError::IntegrityError
        );
        // Fully validate the empty blob and its size.
        ensure!(
            (uncompressed_size == 0) == (merkle_root == EMPTY_BLOB_HASH),
            FxfsError::IntegrityError
        );

        if handle.owner().use_driver_blob_paging() {
            return Ok(Arc::new(Self {
                handle,
                open_count: AtomicUsize::new(0),
                merkle_root,
                uncompressed_size,
                pager_state: None,
            }));
        }

        let compression_info = match &metadata.format {
            BlobFormat::Uncompressed => None,
            BlobFormat::ChunkedZstd { chunk_size, compressed_offsets, .. } => {
                Some(CompressionInfo::new(
                    *chunk_size,
                    stored_size,
                    compressed_offsets,
                    CompressionAlgorithm::Zstd,
                )?)
            }
            BlobFormat::ChunkedLz4 { chunk_size, compressed_offsets, .. } => {
                Some(CompressionInfo::new(
                    *chunk_size,
                    stored_size,
                    compressed_offsets,
                    CompressionAlgorithm::Lz4,
                )?)
            }
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
                open_count: AtomicUsize::new(0),
                merkle_root,
                uncompressed_size,
                pager_state: Some(FxBlobPagerState {
                    vmo,
                    merkle_verifier,
                    compression_info,
                    pager_packet_receiver_registration: Arc::new(
                        pager_packet_receiver_registration,
                    ),
                    chunks_supplied,
                }),
            }
        }))
    }

    /// Returns the new blob.
    pub fn overwrite_me(
        self: &Arc<Self>,
        handle: DataObjectHandle<FxVolume>,
        // The Merkle verifier and optional compression metadata needed to initialise the new
        // blob's `FxBlobPagerState` when Fxfs acts as the pager, or `None` when driver blob paging
        // is enabled.
        pager_info: Option<(MerkleVerifier, Option<CompressionInfo>)>,
    ) -> Arc<Self> {
        let pager_state =
            if let (Some(old_pager_state), Some((merkle_verifier, compression_info))) =
                (&self.pager_state, pager_info)
            {
                let min_chunk_size = min_chunk_size(&compression_info);
                let merkle_verifier =
                    ReadSizedMerkleVerifier::new(merkle_verifier, min_chunk_size as usize).expect(
                        "The chunk size should have been validated by the delivery blob parser",
                    );
                // The chunk size may have changed between the old blob and the new blob.
                // Preserving the chunks supplied bits isn't important.
                let chunks_supplied =
                    AtomicBitVec::new(self.uncompressed_size.div_ceil(min_chunk_size));
                let vmo = old_pager_state.vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();

                Some(FxBlobPagerState {
                    vmo,
                    merkle_verifier,
                    compression_info,
                    pager_packet_receiver_registration: old_pager_state
                        .pager_packet_receiver_registration
                        .clone(),
                    chunks_supplied,
                })
            } else {
                None
            };

        let new_blob = Arc::new(Self {
            handle: handle.into_store_object_handle(),
            open_count: AtomicUsize::new(0),
            merkle_root: self.merkle_root,
            uncompressed_size: self.uncompressed_size,
            pager_state,
        });

        // We have tests that rely on the cache being purged and there are races where the
        // `FxBlob::drop` isn't called early enough, which can make the test flaky.
        self.handle.owner().cache().remove(self.as_ref());

        if let Some(old_pager_state) = &self.pager_state {
            // Lock must be held until the open counts is incremented to prevent concurrent handling
            // of zero children signals.
            let receiver_lock = old_pager_state
                .pager_packet_receiver_registration
                .receiver()
                .set_receiver(&new_blob);
            if receiver_lock.is_strong() {
                // If there was a strong moved between them, then the counts exchange as well. It is
                // only important that the increment happen under the lock as it may handle the next
                // zero children signal, no new requests can now go to the old blob, and because
                // existing requests hold an open count reference using `try_keep_open` for the
                // duration of the request, we can immediately decrement the open count of the old
                // blob.
                new_blob.open_count_add_one();
                self.clone().open_count_sub_one();
            }
        }
        new_blob
    }

    pub fn root(&self) -> Hash {
        self.merkle_root
    }

    pub fn byte_size(&self) -> u64 {
        self.uncompressed_size
    }

    pub fn has_fxfs_pager(&self) -> bool {
        self.pager_state.is_some()
    }

    fn pager_state(&self) -> &FxBlobPagerState {
        self.pager_state
            .as_ref()
            .expect("FxBlob pager state not initialized when use_driver_blob_paging is enabled")
    }

    /// Returns the extents mapping logical offsets to device offsets for both merkle and data
    /// attributes. Each `MappingExtent` maps a logical byte range to a physical device offset.
    /// Extents are sorted by logical offset and non-overlapping.
    pub async fn get_mapping_extents(&self) -> Result<BlobExtents, Error> {
        let tree = &self.handle.store().tree();
        let layer_set = tree.layer_set();

        // Query data extents
        let mut merger = layer_set.merger();
        let stream = self.handle.extent_stream(&mut merger, AttributeId::DATA).await?;
        // TODO(https://fxbug.dev/535489428): Remove use of vec here.
        let data = stream
            .map(|res| {
                let extent = res?;
                MappingExtent::try_new(extent.logical_range(), Some(extent.device_range().start))
                    .map_err(|_| anyhow!(FxfsError::Inconsistent))
            })
            .try_collect()
            .await?;

        // Query Merkle extents
        let mut merger = layer_set.merger();
        let stream = self.handle.extent_stream(&mut merger, AttributeId::BLOB_METADATA).await?;
        let merkle = stream
            .map(|res| {
                let extent = res?;
                MappingExtent::try_new(extent.logical_range(), Some(extent.device_range().start))
                    .map_err(|_| anyhow!(FxfsError::Inconsistent))
            })
            .try_collect()
            .await?;

        Ok(BlobExtents { data, merkle })
    }

    /// Returns the stored (on-disk) byte size of the blob data attribute.
    pub async fn stored_size(&self) -> Result<u64, Error> {
        self.handle.store().get_attribute_size(self.handle.object_id(), AttributeId::DATA).await
    }

    fn record_page_fault_metric(&self, range: &Range<u64>) {
        let pager_state = self.pager_state();
        let page_size = zx::system_get_page_size() as u64;
        let chunk_size: u64 = min_chunk_size(&pager_state.compression_info);
        let pages_per_chunk = chunk_size / page_size;

        let first_chunk = range.start / chunk_size;
        let full_end_chunk = range.end / chunk_size;

        let mut supplied_pages =
            pager_state.chunks_supplied.test_and_set_range(first_chunk, full_end_chunk)
                * pages_per_chunk;

        // The end of the range may not be chunk aligned if it's the last chunk.
        let trailing_pages = (range.end % chunk_size) / page_size;
        if trailing_pages > 0 {
            supplied_pages +=
                pager_state.chunks_supplied.test_and_set_range(full_end_chunk, full_end_chunk + 1)
                    * trailing_pages;
        }

        if supplied_pages > 0 {
            self.handle
                .owner()
                .blob_resupplied_count()
                .increment(supplied_pages, Ordering::Relaxed);
        }
    }

    /// Allocates a device buffer for use with hardware/driver reads.
    fn allocate_buffer(&self, size: u64) -> BufferFuture<'_> {
        self.handle.store().device().allocate_buffer(size as usize)
    }

    /// Allocates a trusted buffer that no other processes or drivers can access,
    /// suitable for zero-copy decompression and Merkle tree verification.
    fn allocate_trusted_buffer(&self, size: u64) -> BufferFuture<'_> {
        self.handle.owner().blob_allocator().allocate_buffer(size as usize)
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
        self.handle.read_aligned_unchecked(AttributeId::DATA, offset, buf, &guard).await
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
    /// Returns [`Status::NOT_SUPPORTED`] when driver blob paging is enabled on the volume.
    ///
    /// *WARNING*: We need to ensure the open count is non-zero before invoking this function, so
    /// it is only implemented for [`OpenedNode<FxBlob>`]. This prevents the blob from being purged
    /// before we get a chance to register it with the pager for [`zx::Signals::VMO_ZERO_CHILDREN`].
    pub fn create_child_vmo(&self) -> Result<zx::Vmo, Status> {
        let blob = self.0.as_ref();
        let pager_state = blob.pager_state.as_ref().ok_or(Status::NOT_SUPPORTED)?;
        let child_vmo = pager_state.vmo.create_child(
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
                .ensure_local()
                .graveyard()
                .queue_tombstone_object(store.store_object_id(), self.object_id());
        }
    }

    fn object_descriptor(&self) -> ObjectDescriptor {
        ObjectDescriptor::File
    }

    fn terminate(&self) {
        if let Some(pager_state) = &self.pager_state {
            pager_state.pager_packet_receiver_registration.stop_watching_for_zero_children();
        }
    }

    fn mark_to_be_purged(self: Arc<Self>) {
        let old = self.open_count.fetch_or(PURGED, Ordering::Relaxed);
        assert!(old & PURGED == 0);
        if old == 0 {
            let store = self.handle.store();
            store
                .filesystem()
                .ensure_local()
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
        &self.pager_state().pager_packet_receiver_registration
    }

    fn vmo(&self) -> &zx::Vmo {
        &self.pager_state().vmo
    }

    fn page_in(self: Arc<Self>, range: PageInRange<Self>) {
        let read_ahead_size = if let Some(compression_info) = &self.pager_state().compression_info {
            read_ahead_size_for_chunk_size(compression_info.chunk_size(), READ_AHEAD_SIZE)
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

        let pager_state = self.pager_state();
        let mut buffer = match pager_state.compression_info {
            Some(_) => self.allocate_trusted_buffer(range.end - range.start).await,
            None => self.allocate_buffer(range.end - range.start).await,
        };
        let unaligned_bytes =
            (std::cmp::min(range.end, self.uncompressed_size) - range.start) as usize;
        match &pager_state.compression_info {
            None => self.read_blocks(range.start, buffer.as_mut()).await?,
            Some(compression_info) => {
                let compressed_offsets =
                    compression_info.compressed_range_for_uncompressed_range(&range)?;
                let bs = self.handle.block_size();
                let aligned = bs.align_range_outwards(&compressed_offsets).unwrap();
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
                            aligned,
                            compression_info.compressed_size()
                        )
                    })?;
                    let compressed_buf_range = (compressed_offsets.start - aligned.start) as usize
                        ..(compressed_offsets.end - aligned.start) as usize;

                    let decompression_result = {
                        fxfs_trace::duration!("blob-decompress", "len" => unaligned_bytes);
                        compression_info.decompress(
                            compressed_buf.as_ptr_slice().subslice(compressed_buf_range),
                            buffer.subslice_mut(0..unaligned_bytes).try_as_mut_slice().unwrap(),
                            range.start,
                        )
                    };
                    match decompression_result {
                        Ok(()) => break,
                        Err(error) => {
                            record_decompression_error_crash_report(
                                &compressed_buf.to_vec(),
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
        // Zero the tail before verification.
        buffer.subslice_mut(unaligned_bytes..buffer.len()).fill(0);
        {
            // TODO(https://fxbug.dev/42073035): This should be offloaded to the kernel at which
            // point we can delete this.
            fxfs_trace::duration!("blob-verify", "len" => unaligned_bytes);
            pager_state.merkle_verifier.verify_aligned(
                range.start as usize,
                buffer.as_ptr_slice().subslice(0..buffer.len()),
                unaligned_bytes,
            )?;
        }
        Ok(buffer)
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
        read_ahead_size_for_chunk_size(compression_info.chunk_size(), READ_AHEAD_SIZE)
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
    use crate::fuchsia::testing::{TestFixture, TestFixtureOptions};
    use crate::fxblob::testing::open_blob_fixture;
    use assert_matches::assert_matches;
    use blob_writer::BlobWriter;
    use delivery_blob::compression::{ChunkedArchiveOptions, CompressionAlgorithm};
    use delivery_blob::{CompressionMode, Type1Blob};
    use fuchsia_async::epoch::Epoch;
    use fuchsia_component_client::connect_to_protocol_at_dir_svc;
    use fxfs::object_store::{HandleOptions, ObjectStore};
    use fxfs_make_blob_image::FxBlobBuilder;
    use storage_device::DeviceHolder;
    use storage_device::fake_device::FakeDevice;
    use storage_units::page_size;

    const BLOCK_SIZE: u64 = fuchsia_merkle::BLOCK_SIZE as u64;
    const CHUNK_SIZE: usize = 32 * 1024;

    #[fuchsia::test(threads = 10)]
    async fn test_empty_blob() {
        let fixture = new_blob_fixture().await;

        let data = vec![];
        let hash = fixture.write_blob(&data, CompressionMode::Never).await;
        assert_eq!(fixture.read_blob(hash).await, data);

        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_large_blob() {
        let fixture = new_blob_fixture().await;

        let data = vec![3; 3_000_000];
        let hash = fixture.write_blob(&data, CompressionMode::Never).await;

        assert_eq!(fixture.read_blob(hash).await, data);

        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_get_multiple_mapping_extents() {
        let fixture = new_blob_fixture().await;

        {
            let data = vec![0xff; 3_000_000];
            let hash = fixture.write_blob(&data, CompressionMode::Never).await;
            let name = format!("{}", hash);

            let handle = fixture.get_blob_handle(&name).await;
            let mut transaction =
                handle.new_transaction().await.expect("failed to create transaction");
            let mut buf = handle.allocate_buffer(8192).await;
            buf.as_mut_ptr_slice().fill(0xaa);
            handle
                .txn_write(&mut transaction, 0, buf.as_ref())
                .await
                .expect("txn_write at offset 0 failed");
            handle
                .txn_write(&mut transaction, 16384, buf.as_ref())
                .await
                .expect("txn_write at offset 16384 failed");
            transaction.commit().await.expect("failed to commit transaction");

            let blob = fixture.get_blob(hash).await.expect("getting blob failed");
            let extents = blob.get_mapping_extents().await.expect("get_mapping_extents failed");

            assert!(!extents.merkle.is_empty(), "Expected at least one merkle extent");
            assert!(
                extents.data.len() > 1,
                "Expected multiple data extents, got {}",
                extents.data.len()
            );

            // The overwritten extents should each be exactly 8192 bytes.
            let overwrite_extent = extents
                .data
                .iter()
                .find(|e| e.logical_range().start == 0)
                .expect("extent at offset 0");
            assert_eq!(
                overwrite_extent.logical_range().end - overwrite_extent.logical_range().start,
                8192
            );
            let overwrite_extent = extents
                .data
                .iter()
                .find(|e| e.logical_range().start == 16384)
                .expect("extent at offset 16384");
            assert_eq!(
                overwrite_extent.logical_range().end - overwrite_extent.logical_range().start,
                8192
            );
        }

        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_large_compressed_blob() {
        let fixture = new_blob_fixture().await;

        let data = vec![3; 3_000_000];
        let hash = fixture.write_blob(&data, CompressionMode::Always).await;

        assert_eq!(fixture.read_blob(hash).await, data);

        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_non_page_aligned_blob() {
        let fixture = new_blob_fixture().await;

        let page_size = page_size().get() as usize;
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

    #[fuchsia::test(threads = 10)]
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
            buf.fill(0);
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

    #[fuchsia::test(threads = 10)]
    async fn test_blob_invalid_uncompressed_size() {
        use fxfs::object_handle::WriteObjectHandle;

        let fixture = new_blob_fixture().await;

        // 1. A single-block blob (uncompressed_size <= BLOCK_SIZE) should have 0 merkle leaves in
        // metadata. If metadata contains 1 leaf (even if it equals the root hash), opening fails.
        {
            let data = vec![0xaa; BLOCK_SIZE as usize];
            let hash = fixture.write_blob(&data, CompressionMode::Never).await;
            let handle = fixture.get_blob_handle(&hash.to_string()).await;
            BlobMetadata { merkle_leaves: vec![hash.into()], format: BlobFormat::Uncompressed }
                .write_to(&handle)
                .await
                .expect("write_to failed");
            let err = fixture.get_blob(hash).await.err().expect("get_blob should fail");
            assert_matches!(err.downcast_ref::<FxfsError>(), Some(FxfsError::IntegrityError));
        }

        // 2. A multi-block compressed blob (uncompressed_size > BLOCK_SIZE) with 0 merkle leaves in
        // metadata must fail to open with IntegrityError.
        {
            let data = vec![0xbb; (BLOCK_SIZE * 2) as usize];
            let hash = fixture.write_blob(&data, CompressionMode::Always).await;
            let handle = fixture.get_blob_handle(&hash.to_string()).await;
            let mut metadata = BlobMetadata::read_from(&handle).await.expect("read_from failed");
            metadata.merkle_leaves.clear();
            metadata.write_to(&handle).await.expect("write_to failed");
            let err = fixture.get_blob(hash).await.err().expect("get_blob should fail");
            assert_matches!(err.downcast_ref::<FxfsError>(), Some(FxfsError::IntegrityError));
        }

        // 3. A multi-block blob whose uncompressed size disagrees with the number of merkle leaves
        // (even when merkle_leaves validly hashes to the merkle root) must fail with
        // IntegrityError.
        {
            let data = vec![0xcc; (BLOCK_SIZE * 2) as usize];
            let hash = fixture.write_blob(&data, CompressionMode::Never).await;
            let handle = fixture.get_blob_handle(&hash.to_string()).await;
            handle.truncate(BLOCK_SIZE * 3).await.expect("truncate failed");
            let err = fixture.get_blob(hash).await.err().expect("get_blob should fail");
            assert_matches!(err.downcast_ref::<FxfsError>(), Some(FxfsError::IntegrityError));
        }

        // 4. A non-empty single-block blob truncated to 0 bytes must fail to open with
        // IntegrityError.
        {
            let data = vec![0xdd; 100];
            let hash = fixture.write_blob(&data, CompressionMode::Never).await;
            let handle = fixture.get_blob_handle(&hash.to_string()).await;
            handle.truncate(0).await.expect("truncate failed");
            let err = fixture.get_blob(hash).await.err().expect("get_blob should fail");
            assert_matches!(err.downcast_ref::<FxfsError>(), Some(FxfsError::IntegrityError));
        }

        // 5. An empty blob whose uncompressed size is non-zero must fail to open with
        // IntegrityError.
        {
            let hash = fixture.write_blob(&[], CompressionMode::Never).await;
            let handle = fixture.get_blob_handle(&hash.to_string()).await;
            handle.truncate(100).await.expect("truncate failed");
            let err = fixture.get_blob(hash).await.err().expect("get_blob should fail");
            assert_matches!(err.downcast_ref::<FxfsError>(), Some(FxfsError::IntegrityError));
        }

        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
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

    #[fuchsia::test(threads = 10)]
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
        assert!(CompressionInfo::new(COMPRESSED_BLOB_CHUNK_SIZE, 100, &[], ZSTD).is_err());
        assert!(CompressionInfo::new(COMPRESSED_BLOB_CHUNK_SIZE, 100, &[1], ZSTD).is_err());
        assert!(CompressionInfo::new(COMPRESSED_BLOB_CHUNK_SIZE, 100, &[0], ZSTD).is_ok());
    }

    #[fuchsia::test]
    fn test_compression_info_offsets_must_be_sorted() {
        assert!(CompressionInfo::new(COMPRESSED_BLOB_CHUNK_SIZE, 100, &[0, 1, 2], ZSTD).is_ok());
        assert!(CompressionInfo::new(COMPRESSED_BLOB_CHUNK_SIZE, 100, &[0, 2, 1], ZSTD).is_err());
        assert!(CompressionInfo::new(COMPRESSED_BLOB_CHUNK_SIZE, 100, &[0, 1, 1], ZSTD).is_err());
    }

    #[fuchsia::test]
    fn test_compression_info_compressed_range_for_uncompressed_range() {
        fn check_compression_ranges(
            offsets: &[u64],
            compressed_size: u64,
            expected_ranges: &[Range<u64>],
            chunk_size: u64,
            read_ahead_size: u64,
        ) {
            let compression_info =
                CompressionInfo::new(chunk_size, compressed_size, offsets, ZSTD).unwrap();
            for (i, expected_range) in expected_ranges.iter().enumerate() {
                let i = i as u64;
                let result = compression_info
                    .compressed_range_for_uncompressed_range(
                        &(i * read_ahead_size..(i + 1) * read_ahead_size),
                    )
                    .unwrap();
                assert_eq!(&result, expected_range);
            }
        }
        check_compression_ranges(
            &[0, 10, 20, 30],
            30,
            &[0..10, 10..20, 20..30, 30..30],
            COMPRESSED_BLOB_CHUNK_SIZE,
            COMPRESSED_BLOB_CHUNK_SIZE,
        );
        check_compression_ranges(
            &[0, 10, 20, 30],
            30,
            &[0..20, 20..30],
            COMPRESSED_BLOB_CHUNK_SIZE,
            COMPRESSED_BLOB_CHUNK_SIZE * 2,
        );
        check_compression_ranges(
            &[0, 10, 20, 30],
            30,
            &[0..30],
            COMPRESSED_BLOB_CHUNK_SIZE,
            COMPRESSED_BLOB_CHUNK_SIZE * 4,
        );
        check_compression_ranges(
            &[0, 10, 20, 30, MAX_SMALL_OFFSET + 10],
            MAX_SMALL_OFFSET + 10,
            &[0..MAX_SMALL_OFFSET + 10, MAX_SMALL_OFFSET + 10..MAX_SMALL_OFFSET + 10],
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
            MAX_SMALL_OFFSET + 50,
            &[
                0..20,
                20..MAX_SMALL_OFFSET + 10,
                MAX_SMALL_OFFSET + 10..MAX_SMALL_OFFSET + 30,
                MAX_SMALL_OFFSET + 30..MAX_SMALL_OFFSET + 50,
            ],
            COMPRESSED_BLOB_CHUNK_SIZE,
            COMPRESSED_BLOB_CHUNK_SIZE * 2,
        );
    }

    #[fuchsia::test]
    fn test_compression_info_compressed_range_for_uncompressed_range_errors() {
        let compression_info = CompressionInfo::new(
            COMPRESSED_BLOB_CHUNK_SIZE,
            MAX_SMALL_OFFSET + 50,
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
        let compressed_size = compressed_data.len() as u64;
        (
            CompressionInfo::new(
                CHUNK_SIZE as u64,
                compressed_size,
                &compressed_offsets,
                CompressionAlgorithm::Lz4,
            )
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
            .decompress(&compressed_data[..], &mut decompressed_data[0..CHUNK_SIZE], 0)
            .expect("failed to decompress");
        assert_eq!(uncompressed_data, decompressed_data[0..CHUNK_SIZE]);

        // Too small of destination buffer.
        compression_info
            .decompress(&compressed_data[..], &mut decompressed_data[0..CHUNK_SIZE - 1], 0)
            .expect_err("decompression should fail");

        // Too large of destination buffer.
        compression_info
            .decompress(&compressed_data[..], &mut decompressed_data[0..CHUNK_SIZE - 1], 0)
            .expect_err("decompression should fail");
    }

    #[fuchsia::test]
    fn test_compression_info_decompress_multiple_chunks() {
        fn slice_for_chunks<'a>(
            compressed_data: &'a [u8],
            compression_info: &CompressionInfo,
            chunks: Range<u64>,
        ) -> &'a [u8] {
            let range = compression_info
                .compressed_range_for_uncompressed_range(
                    &(chunks.start * CHUNK_SIZE as u64..chunks.end * CHUNK_SIZE as u64),
                )
                .unwrap();
            &compressed_data[range.start as usize..range.end as usize]
        }

        const BLOB_SIZE: usize = CHUNK_SIZE * 4 + 4096;
        let (compression_info, compressed_data, uncompressed_data) =
            build_compression_info(BLOB_SIZE);
        let mut decompressed_data = vec![0u8; BLOB_SIZE];

        // Decompress the entire blob.
        compression_info
            .decompress(&compressed_data[..], &mut decompressed_data[..], 0)
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

    #[fuchsia::test(threads = 10)]
    async fn test_refault_metric() {
        let fixture = new_blob_fixture().await;
        {
            let volume = fixture.volume().volume().clone();
            const FILE_SIZE: u64 = READ_AHEAD_SIZE * 4 - 4096;
            let data = vec![0xffu8; FILE_SIZE as usize];
            let hash = fixture.write_blob(&data, CompressionMode::Never).await;

            let blob = fixture.get_opened_blob(hash).await.unwrap();
            // Nothing has been read yet.
            assert_eq!(
                &blob.pager_state().chunks_supplied.to_vec(4),
                &[false, false, false, false]
            );

            blob.pager_state().vmo.read_to_vec::<u8>(4096, 4096).unwrap();

            assert_eq!(&blob.pager_state().chunks_supplied.to_vec(4), &[true, false, false, false]);

            blob.pager_state()
                .vmo
                .read_to_vec::<u8>(READ_AHEAD_SIZE * 2 + 4096, READ_AHEAD_SIZE)
                .unwrap();
            assert_eq!(&blob.pager_state().chunks_supplied.to_vec(4), &[true, false, true, true]);

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

            // Two chunks were resupplied (one full chunk and the last chunk which is one page
            // short), and the counter is expressed in pages.
            let pages_per_chunk = READ_AHEAD_SIZE / zx::system_get_page_size() as u64;
            assert_eq!(
                volume.blob_resupplied_count().read(Ordering::SeqCst),
                2 * pages_per_chunk - 1
            );
        }

        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_driver_blob_paging_does_not_create_vmo() {
        let fixture = TestFixture::open(
            DeviceHolder::new(FakeDevice::new(16384, 512)),
            TestFixtureOptions {
                encrypted: false,
                as_blob: true,
                use_driver_blob_paging: true,
                ..Default::default()
            },
        )
        .await;

        {
            let data = vec![0xabu8; 65_536];
            let hash = fixture.write_blob(&data, CompressionMode::Always).await;

            let opened_blob = fixture.get_opened_blob(hash).await.expect("get_opened_blob failed");
            let old_object_id = opened_blob.object_id();
            assert!(!opened_blob.has_fxfs_pager());
            assert_eq!(opened_blob.byte_size(), data.len() as u64);
            assert!(opened_blob.stored_size().await.expect("stored_size failed") > 0);
            let extents =
                opened_blob.get_mapping_extents().await.expect("get_mapping_extents failed");
            assert!(!extents.data.is_empty());
            assert!(!extents.merkle.is_empty());

            // Creating a child VMO directly on an FxBlob opened without a pager fails with
            // NOT_SUPPORTED.
            assert_eq!(
                opened_blob.create_child_vmo().expect_err("create_child_vmo succeeded"),
                Status::NOT_SUPPORTED
            );

            // Overwriting the open blob must also succeed without creating a VMO, while keeping
            // the old object alive until `opened_blob` is dropped.
            let delivery_data = Type1Blob::generate(&data, CompressionMode::Never);
            let writer = fixture.create_blob(&hash.into(), true).await.expect("create_blob failed");
            let mut blob_writer = BlobWriter::create(writer, delivery_data.len() as u64)
                .await
                .expect("BlobWriter::create failed");
            blob_writer.write(&delivery_data).await.expect("blob_writer.write failed");

            fixture.fs().graveyard().flush().await;
            assert!(
                ObjectStore::open_object(
                    fixture.volume().volume(),
                    old_object_id,
                    HandleOptions::default(),
                    None,
                )
                .await
                .is_ok(),
                "Overwritten blob object should remain alive while OpenedNode is held"
            );

            drop(opened_blob);
            fixture.fs().graveyard().flush().await;
            assert_matches!(
                ObjectStore::open_object(
                    fixture.volume().volume(),
                    old_object_id,
                    HandleOptions::default(),
                    None,
                )
                .await
                .err()
                .expect("Overwritten blob object should be purged after OpenedNode is dropped")
                .downcast_ref::<FxfsError>(),
                Some(FxfsError::NotFound)
            );

            let overwritten_blob =
                fixture.get_opened_blob(hash).await.expect("get_opened_blob after overwrite");
            assert!(!overwritten_blob.has_fxfs_pager());
            assert_eq!(overwritten_blob.byte_size(), data.len() as u64);
            assert_eq!(
                overwritten_blob.stored_size().await.expect("stored_size failed"),
                data.len() as u64
            );
            drop(overwritten_blob);

            // Calling BlobReader.GetVmo falls back to Fxfs's internal blob pager.
            let blob_reader =
                connect_to_protocol_at_dir_svc::<fidl_fuchsia_fxfs::BlobReaderMarker>(
                    fixture.volume_out_dir(),
                )
                .expect("connect_to_protocol_at_dir_svc failed");
            let vmo = blob_reader
                .get_vmo(&hash.into())
                .await
                .expect("get_vmo FIDL failed")
                .map_err(Status::err_from_raw)
                .expect("get_vmo failed");
            let mut read_buf = vec![0u8; data.len()];
            vmo.read(&mut read_buf, 0).expect("vmo read failed");
            assert_eq!(read_buf, data);
        }

        fixture.close().await;
    }
}
