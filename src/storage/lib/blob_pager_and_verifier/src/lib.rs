// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! This library coordinates pager-backed blob VMOs, driver-level page fault handling, and
//! Merkle verification.
//!
//! # Architecture
//! When clients request a blob via `create_vmo`, `BlobPagerAndVerifier` checks `BlobCacheManager`.
//! If the blob is missing, the caller marks the cache entry as `Pending` and asks Fxfs to register
//! the blob's disk extents. Once Fxfs returns the mapping key, the caller creates a pager-backed
//! VMO and caches the resulting `CachedBlob`.
//!
//! If concurrent requests arrive while the blob is still `Pending`, they listen on an
//! `event_listener::Event`. Once the primary creator finishes (or fails), it notifies all waiting
//! listeners so they can clone the ready VMO without contacting Fxfs again.
//!
//! # Page Fault Handling and Verification
//! 1. When a client reads from an offset in a blob VMO that has not yet been loaded into memory,
//!    the kernel generates a page request on the VMO's pager port, which the block driver waits
//!    on directly.
//! 2. The driver reads and decompresses the raw disk blocks according to the blob's extent
//!    mappings, then writes the unverified pages into a shared delivery queue VMO.
//! 3. The driver also transmits the blob's Merkle tree leaf hashes over the delivery queue, which
//!    initializes a `ReadSizedMerkleVerifier` on the cached blob.
//! 4. As decompressed chunks arrive, `BlobPagerAndVerifier` verifies the pages against the expected
//!    Merkle leaves:
//!    - On success, it supplies the pages to the kernel pager (`zx_pager_supply_pages`), unblocking
//!      the client read.
//!    - On verification failure, it fails the page range with `ZX_ERR_IO_DATA_INTEGRITY`.
//!
//! # Lifecycle and Eviction
//! 1. Handles returned to clients are child VMOs cloned from the parent pager VMO.
//! 2. `BlobCacheManager` holds strong references (`Arc<CachedBlob>`) to keep active blobs in the
//!    cache.
//! 3. When all clients close their child VMOs, the kernel fires `ZX_VMO_ZERO_CHILDREN` on the
//!    parent VMO.
//! 4. The cache manager observes this signal, confirms no new child VMOs were opened in the
//!    meantime, and evicts the blob from the cache.
//! 5. Once all references (`Arc<CachedBlob>`) are dropped, a request to close the blob's extent
//!    mapping session with Fxfs will be sent.

mod delivery;

pub use delivery::{
    DELIVERY_DATA_SIZE, DeliveryQueueProcessor, DeliveryQueueProvider, TestVmoProvider,
    UnverifiedPages,
};

use anyhow::{Context, Error, anyhow, bail};
use event_listener as _;
use fidl_fuchsia_storage_block as fblock;
use fidl_fuchsia_storage_mapping as fmapping;
use fuchsia_async as fasync;
use fuchsia_hash::{HASH_SIZE, Hash};
use fuchsia_merkle::{MerkleVerifier, ReadSizedMerkleVerifier};
use fuchsia_sync::Mutex;
use std::collections::{HashMap, hash_map};
use std::ops::Range;
use std::sync::{Arc, OnceLock, Weak};
use storage_ptr_slice::PtrByteSlice;
use zx;

// A `fuchsia_async::PacketReceiver` that watches for a VMO to reach zero children and triggers
// cache eviction.
struct ZeroChildrenReceiver {
    blob: OnceLock<Weak<CachedBlob>>,
}

impl fasync::PacketReceiver for ZeroChildrenReceiver {
    fn receive_packet(&self, packet: zx::Packet) {
        if let zx::PacketContents::SignalOne(signals) = packet.contents() {
            if signals.observed().contains(zx::Signals::VMO_ZERO_CHILDREN) {
                if let Some(cached_blob) = self.blob.get().and_then(|w| w.upgrade()) {
                    if let Some(cache_manager) = cached_blob.cache_manager.upgrade() {
                        cache_manager.on_zero_children(&cached_blob);
                    }
                }
            }
        }
    }
}

// Holds the state and resources for an open blob residing in the cache.
//
// When a blob is opened, this bundles:
// - The root pager VMO that clients clone from (so we can detect when all handles close).
// - The Fxfs session key used by the block driver to deliver data and to close the mapping.
// - The Merkle tree verifier used to check incoming pages from the driver.
// - The ZeroChildrenReceiver registration that watches for zero children to trigger eviction.
struct CachedBlob {
    // The parent pager-backed VMO. Clients only receive child clones so the kernel can notify us
    // via `ZX_VMO_ZERO_CHILDREN` when all clients have dropped their handles.
    vmo: zx::Vmo,
    // The Merkle root hash of the blob (its unique identifier).
    root_hash: [u8; 32],
    // Weak back-reference to BlobCacheManager to avoid an Arc reference cycle, since the cache
    // manager already holds an Arc to this blob.
    cache_manager: Weak<BlobCacheManager>,
    // Session key assigned by Fxfs for this blob mapping, used in driver delivery commands and when
    // closing the session.
    vmo_key: u32,
    // Blob size in bytes.
    len: u64,
    // Keeps ZeroChildrenReceiver registered on the async loop while this blob is alive.
    registration: fasync::ReceiverRegistration<ZeroChildrenReceiver>,
    // Merkle tree verifier, initialised once the driver delivers the blob's leaf hashes.
    merkle_verifier: OnceLock<ReadSizedMerkleVerifier>,
}

impl CachedBlob {
    fn create_child(&self) -> Result<zx::Vmo, zx::Status> {
        self.vmo.create_child(zx::VmoChildOptions::REFERENCE | zx::VmoChildOptions::NO_WRITE, 0, 0)
    }

    fn wait_for_zero_children(&self) {
        let _ = self.vmo.wait_async(
            fasync::EHandle::local().port(),
            self.registration.key(),
            zx::Signals::VMO_ZERO_CHILDREN,
            zx::WaitAsyncOpts::empty(),
        );
    }
}

impl Drop for CachedBlob {
    fn drop(&mut self) {
        if let Some(cache_manager) = self.cache_manager.upgrade() {
            cache_manager.pending_leaves.lock().remove(&self.vmo_key);
            let session = cache_manager.mapping_session.clone();
            let vmo_key = self.vmo_key;
            cache_manager.scope.spawn(async move {
                let _ = session.close(vmo_key).await;
            });
        }
    }
}

enum BlobState {
    // The blob is currently being opened: Fxfs is registering its disk extents, and the
    // pager-backed VMO is being created. Concurrent requests wait on this event until the primary
    // creator finishes (or fails) and wakes them.
    Pending(Arc<event_listener::Event>),
    // The blob's pager-backed VMO has been created and cached. Eviction is triggered when all
    // client child VMOs are closed (`ZX_VMO_ZERO_CHILDREN`).
    Ready(Arc<CachedBlob>),
}

// A Drop guard to clean up the cache entry if create_vmo fails.
struct PendingCacheEntryGuard {
    cache_manager: Arc<BlobCacheManager>,
    identifier: Option<[u8; 32]>,
}

impl PendingCacheEntryGuard {
    fn dismiss(&mut self) {
        self.identifier = None;
    }
}

impl Drop for PendingCacheEntryGuard {
    fn drop(&mut self) {
        if let Some(id) = self.identifier.take() {
            let mut blob_states = self.cache_manager.blob_states_by_hash.lock();
            // If the creation failed, clear out the pending state.
            if let Some(BlobState::Pending(completion_event)) = blob_states.remove(&id) {
                // Wake up all suspended tasks concurrently waiting on this blob.
                completion_event.notify(usize::MAX);
            }
        }
    }
}

// The outcome of looking up or reserving an entry in `BlobCacheManager`.
enum CacheLookup {
    // The VMO has been created and cached. Returns a child of it.
    Ready(zx::Vmo),
    // The VMO is currently being created by another request. Contains a listener to await
    // completion.
    Pending(event_listener::EventListener),
    // The blob is absent. Provides a drop-safe guard to be used with creation. If the task crashes
    // or returns an early error, the pending state will be cleared from the cache.
    Missing(PendingCacheEntryGuard),
}

// BlobCacheManager coordinates the concurrent creation, caching, and verification of pager-backed
// blobs.
struct BlobCacheManager {
    blob_states_by_hash: Mutex<HashMap<[u8; 32], BlobState>>,
    // Index of open blobs by their Fxfs session key, can be used to look up blobs during driver
    // delivery.
    blobs_by_key: Mutex<HashMap<u32, Arc<CachedBlob>>>,
    // Stages Merkle leaf hashes if a `RegisterBlob` command arrives before `session.open()` returns
    // the key over FIDL to populate `blobs_by_key`. Staged leaves are transferred once `create_vmo`
    // finishes.
    //
    // TODO(https://fxbug.dev/535489428): Consider switching to client-allocated keys in
    // MappingSession.Open(key, identifier). If the client allocates the key upfront, we eliminate
    // the early arrival of `RegisterBlob` before the session key is returned over FIDL and we won't
    // need `pending_leaves`.
    pending_leaves: Mutex<HashMap<u32, Box<[Hash]>>>,
    mapping_session: fmapping::MappingSessionProxy,
    pager: Arc<zx::Pager>,
    delivery_vmo: zx::Vmo,
    scope: fasync::ScopeHandle,
}

impl BlobCacheManager {
    fn new(
        mapping_session: fmapping::MappingSessionProxy,
        pager: Arc<zx::Pager>,
        delivery_vmo: zx::Vmo,
        scope: fasync::ScopeHandle,
    ) -> Self {
        Self {
            blob_states_by_hash: Mutex::new(HashMap::new()),
            blobs_by_key: Mutex::new(HashMap::new()),
            pending_leaves: Mutex::new(HashMap::new()),
            mapping_session,
            pager,
            delivery_vmo,
            scope,
        }
    }

    fn delivery_vmo(&self) -> &zx::Vmo {
        &self.delivery_vmo
    }

    fn get_by_key(&self, key: u32) -> Option<Arc<CachedBlob>> {
        self.blobs_by_key.lock().get(&key).cloned()
    }

    // Checks the cache for an open blob and handles three cases:
    // 1. Ready:   The blob is cached. Creates and returns a new read-only child VMO.
    // 2. Pending: Another task is actively opening the blob. Returns an event listener so the
    //             caller can await completion without duplicate work.
    // 3. Missing: The blob is absent. Atomically reserves the slot by marking it `Pending`. The
    //             returned drop guard ensures the reservation is cleaned up if the caller fails
    //             or aborts.
    fn get_or_reserve(self: &Arc<Self>, identifier: &[u8; 32]) -> Result<CacheLookup, Error> {
        let mut blob_states = self.blob_states_by_hash.lock();
        match blob_states.get(identifier) {
            Some(BlobState::Ready(arc_blob)) => {
                let child = arc_blob
                    .create_child()
                    .map_err(|s| anyhow!("Failed to create child VMO: {s}"))?;
                Ok(CacheLookup::Ready(child))
            }
            Some(BlobState::Pending(completion_event)) => {
                Ok(CacheLookup::Pending(completion_event.listen()))
            }
            None => {
                let completion_event = Arc::new(event_listener::Event::new());
                blob_states.insert(*identifier, BlobState::Pending(completion_event));
                Ok(CacheLookup::Missing(PendingCacheEntryGuard {
                    cache_manager: self.clone(),
                    identifier: Some(*identifier),
                }))
            }
        }
    }

    fn on_zero_children(&self, blob: &Arc<CachedBlob>) {
        // On zero children, evict the blob from the cache and close its Fxfs mapping session.
        // We must hold this lock before checking `num_children` so `get_or_reserve` cannot clone
        // a child between this check and the following eviction, which would leave that client
        // holding a VMO whose storage mappings have already been closed.
        let mut blob_states = self.blob_states_by_hash.lock();
        if let Ok(info) = blob.vmo.info() {
            if info.num_children > 0 {
                // A concurrent client acquired a child VMO before we processed this packet.
                // Resume watching for the next ZERO_CHILDREN signal.
                blob.wait_for_zero_children();
                return;
            }
        }
        // Evict from `blob_states_by_hash`. Use `Arc::ptr_eq` to guard against races where this
        // blob was already removed or replaced by a new instance for the same hash before this
        // delayed `ZERO_CHILDREN` packet was processed on the async loop.
        if let hash_map::Entry::Occupied(entry) = blob_states.entry(blob.root_hash) {
            if let BlobState::Ready(cached) = entry.get() {
                if Arc::ptr_eq(cached, blob) {
                    entry.remove();
                }
            }
        }
        // Evict from `blobs_by_key`.
        let mut key_map = self.blobs_by_key.lock();
        if let hash_map::Entry::Occupied(entry) = key_map.entry(blob.vmo_key) {
            if Arc::ptr_eq(entry.get(), blob) {
                entry.remove();
                // TODO(https://fxbug.dev/535489428): we can probably reduce the number of locks
                // if we switch to client-allocated keys in MappingSession.Open(key, identifier).
                self.pending_leaves.lock().remove(&blob.vmo_key);
            }
        }
    }

    fn finalize_pending_request(&self, identifier: &[u8; 32], result: Option<&Arc<CachedBlob>>) {
        let mut blob_states = self.blob_states_by_hash.lock();
        let mut entry = match blob_states.entry(*identifier) {
            hash_map::Entry::Occupied(e) => e,
            hash_map::Entry::Vacant(_) => unreachable!("Cache entry was unexpectedly missing"),
        };

        let event = match entry.get() {
            BlobState::Pending(event) => event.clone(),
            _ => unreachable!("Cache entry was unexpectedly not Pending"),
        };

        if let Some(cached) = result {
            entry.insert(BlobState::Ready(cached.clone()));
            let mut key_map = self.blobs_by_key.lock();
            key_map.insert(cached.vmo_key, cached.clone());
            if let Some(hashes) = self.pending_leaves.lock().remove(&cached.vmo_key) {
                if let Err(error) = Self::initialize_merkle_verifier(cached, hashes) {
                    log::error!(error:?; "Failed to initialize verifier from early leaves");
                }
            }
        } else {
            entry.remove();
        }

        event.notify(usize::MAX);
    }

    fn initialize_merkle_verifier(blob: &CachedBlob, hashes: Box<[Hash]>) -> Result<(), Error> {
        // For blobs <= 8192 bytes (a single block), the Merkle tree consists of only a single leaf,
        // which is identical to the root hash itself. Fxfs omits storing leaf hashes on disk for
        // single-block blobs to save space, resulting in empty leaf metadata read by the block
        // server. In that case, synthesise the single leaf from the blob's Merkle root hash.
        let hashes =
            if hashes.is_empty() { Box::new([Hash::from(blob.root_hash)]) } else { hashes };

        match MerkleVerifier::new(Hash::from(blob.root_hash), hashes) {
            Ok(verifier) => {
                let sized_verifier =
                    ReadSizedMerkleVerifier::new(verifier, delivery::DELIVERY_DATA_SIZE)
                        .map_err(|e| anyhow!("Failed to create ReadSizedMerkleVerifier: {e:?}"))?;
                let _ = blob.merkle_verifier.set(sized_verifier);
                Ok(())
            }
            Err(e) => {
                bail!("Failed to verify merkle leaves for key {}: {e:?}", blob.vmo_key);
            }
        }
    }

    fn report_pager_failure(&self, vmo: &zx::Vmo, range: Range<u64>, status: zx::Status) {
        // Map to the set of statuses accepted by ZX_PAGER_OP_FAIL which are ZX_ERR_IO,
        // ZX_ERR_IO_DATA_INTEGRITY, ZX_ERR_BAD_STATE, ZX_ERR_NO_SPACE, and ZX_ERR_BUFFER_TOO_SMALL.
        let pager_status = match status {
            zx::Status::IO_DATA_INTEGRITY => zx::Status::IO_DATA_INTEGRITY,
            zx::Status::NO_SPACE => zx::Status::NO_SPACE,
            zx::Status::BUFFER_TOO_SMALL | zx::Status::FILE_BIG => zx::Status::BUFFER_TOO_SMALL,
            zx::Status::IO
            | zx::Status::IO_DATA_LOSS
            | zx::Status::IO_INVALID
            | zx::Status::IO_MISSED_DEADLINE
            | zx::Status::IO_NOT_PRESENT
            | zx::Status::IO_OVERRUN
            | zx::Status::IO_REFUSED
            | zx::Status::PEER_CLOSED => zx::Status::IO,
            _ => zx::Status::BAD_STATE,
        };

        // `ZX_PAGER_OP_FAIL` resolves pending page requests overlapping the specified range with
        // the failure status. Pages that were already supplied remain accessible.
        if let Err(error) = self.pager.op_range(zx::PagerOp::Fail(pager_status), vmo, range) {
            log::error!(error:?; "Failed to report pager failure to kernel");
        }
    }
}

impl delivery::DeliveryQueueProvider for BlobCacheManager {
    fn deliver_pages(
        &self,
        key: u64,
        target_offset: u64,
        unverified_pages: UnverifiedPages<'_>,
    ) -> Result<(), Error> {
        let cached_blob = self
            .get_by_key(key as u32)
            .ok_or_else(|| anyhow!("Unknown or expired key {key} in deliver_pages"))?;

        let page_size = zx::system_get_page_size() as u64;
        let page_aligned_size = cached_blob.len.div_ceil(page_size) * page_size;

        let verifier = match cached_blob.merkle_verifier.get() {
            Some(v) => v,
            None => {
                // If data arrives before `RegisterBlob` (or if registration failed), the blob has
                // no cryptographic metadata, so no pages can ever be verified or supplied. Fail the
                // entire page-aligned range of the VMO with `BAD_STATE` to unblock any waiting
                // client threads on this blob.
                self.report_pager_failure(
                    &cached_blob.vmo,
                    0..page_aligned_size,
                    zx::Status::BAD_STATE,
                );
                bail!("Data received for uninitialized blob {}", key);
            }
        };

        let chunk_len = unverified_pages.len_in_bytes() as u64;

        // `zx_pager_supply_pages` and `zx_pager_op_range(Fail)` require the range to be within the
        // page-aligned capacity of the VMO. If the range extends past the end of the VMO, the
        // kernel fails the call with `ZX_ERR_OUT_OF_RANGE`. Clamp the range to the VMO's
        // page-aligned limit.
        let bounded_len = std::cmp::min(chunk_len, page_aligned_size.saturating_sub(target_offset));
        let chunk_range = target_offset..target_offset + bounded_len;

        // For the final chunk of an unaligned blob, the buffer passed to `verify_aligned` is
        // page-aligned and zero-padded, but `verify_aligned` expects the remaining unaligned length
        // of the valid data.
        let remaining_in_blob = cached_blob.len.saturating_sub(target_offset);
        let unaligned_len = std::cmp::min(chunk_len, remaining_in_blob) as usize;
        if let Err(e) = verifier.verify_aligned(
            target_offset as usize,
            unverified_pages.as_ptr_byte_slice(),
            unaligned_len,
        ) {
            self.report_pager_failure(&cached_blob.vmo, chunk_range, zx::Status::IO_DATA_INTEGRITY);
            bail!("Failed to verify payload for blob {}: {:?}", key, e);
        }

        if let Err(e) = self.pager.supply_pages(
            &cached_blob.vmo,
            chunk_range.clone(),
            &self.delivery_vmo,
            unverified_pages.delivery_offset(),
        ) {
            self.report_pager_failure(&cached_blob.vmo, chunk_range, e);
            bail!("Failed to supply pages: {:?}", e);
        }

        Ok(())
    }

    fn register_blob(&self, key: u64, leaf_data: PtrByteSlice<'_>) -> Result<(), Error> {
        if leaf_data.len() % HASH_SIZE != 0 {
            bail!("RegisterBlob invalid leaf length must be a multiple of HAHS_SIZE");
        }

        let hashes: Vec<Hash> = (0..(leaf_data.len() / HASH_SIZE))
            .map(|i| {
                let chunk = leaf_data.subslice(i * HASH_SIZE..(i + 1) * HASH_SIZE);
                let mut hash_bytes = [0u8; HASH_SIZE];
                chunk.copy_to_slice(&mut hash_bytes);
                Hash::from(hash_bytes)
            })
            .collect();

        let cached_blob = self.get_by_key(key as u32);

        if let Some(blob) = cached_blob {
            if blob.merkle_verifier.get().is_some() {
                log::warn!("RegisterBlob received for blob {key} but it is already initialized");
                return Ok(());
            }
            Self::initialize_merkle_verifier(&blob, hashes.into_boxed_slice())?;
        } else {
            // `RegisterBlob` can arrive before `create_vmo` registers the cached blob. When Fxfs
            // commits the blob's extent mappings  during `Open`, it immediately signals the block
            // driver over the shared `mapping_vmo`. The driver processes the mappings and sends
            // `RegisterBlob` over `delivery_vmo`. This can all occur before FIDL response of
            // `session.open().await`. Stash the leaf hashes so that `finalize_pending_request` can
            // initialise the verifier once `create_vmo` finishes.
            self.pending_leaves.lock().insert(key as u32, hashes.into_boxed_slice());
        }
        Ok(())
    }
}

/// Coordinates between FxBlob, the block driver, and the kernel to manage pager-backed blobs such
/// that page requests can be handled at the driver instead of the filesystem layer.
///
/// `BlobPagerAndVerifier` uses a dedicated Pager to create pager-backed VMOs for blobs. Kernel page
/// faults on these VMOs are routed directly to the block driver, which reads and decompresses the
/// raw data. The driver then passes this data back to `BlobPagerAndVerifier` for cryptographic
/// verification and page fault resolution.
pub struct BlobPagerAndVerifier {
    // Ties the lifetime of the background delivery thread to the `BlobPagerAndVerifier`.
    _delivery_processor: delivery::DeliveryQueueProcessor,
    // Ties the lifetime of the block driver's mapper session to the `BlobPagerAndVerifier`.
    _mapper_session: fblock::MapperSessionProxy,
    port: zx::Port,
    pager: Arc<zx::Pager>,
    cache_manager: Arc<BlobCacheManager>,
    _scope: fasync::Scope,
}

impl BlobPagerAndVerifier {
    /// Returns a reference to the shared delivery queue VMO.
    pub fn delivery_vmo(&self) -> &zx::Vmo {
        self.cache_manager.delivery_vmo()
    }

    /// Creates a new BlobPagerAndVerifier.
    ///
    /// Establishes a mapping_provider session with FxBlob to receive a shared VMO that will be used
    /// to communicate opened blob extent mappings as well as any closed blobs. Then, opens a
    /// mapper session with the block driver, forwarding the mapping VMO alongside a Zircon port
    /// (for the driver to receive page faults) and a delivery queue VMO (for the driver to emit
    /// unverified data for verification).
    pub async fn new(
        mapping_provider: &fmapping::MappingProviderProxy,
        mapper: &fblock::MapperProxy,
    ) -> Result<Self, Error> {
        // Establish a mapping_provider session with Fxfs.
        let (mapping_session, session_server) =
            fidl::endpoints::create_proxy::<fmapping::MappingSessionMarker>();
        let mapping_vmo = mapping_provider
            .open_session(session_server)
            .await
            .context("FIDL error calling MappingProvider.OpenSession")?
            .map_err(|e| anyhow!("MappingProvider open_session failed: {e:?}"))?;

        // Establish a mapper session with the block driver.
        let port = zx::Port::create();
        let delivery_queue = zx::Vmo::create(mapping::DELIVERY_VMO_SIZE)
            .context("Failed to create delivery queue VMO")?;
        let delivery_vmo: zx::Vmo = delivery_queue
            .duplicate_handle(zx::Rights::SAME_RIGHTS)
            .context("Failed to duplicate delivery queue VMO")?;
        let delivery_queue_dup = delivery_queue.duplicate_handle(zx::Rights::SAME_RIGHTS)?;
        let receiver = vmo_fifo::Receiver::<mapping::RawDeliveryCommand>::new(
            delivery_queue,
            mapping::PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .context("Failed to create delivery queue receiver")?;
        let pager = Arc::new(
            zx::Pager::create(zx::PagerOptions::empty()).context("Failed to create pager")?,
        );
        let port_dup = port.duplicate_handle(zx::Rights::SAME_RIGHTS)?;
        let (mapper_session, mapper_session_server) =
            fidl::endpoints::create_proxy::<fblock::MapperSessionMarker>();
        mapper
            .open_session(
                mapper_session_server,
                mapping_vmo,
                Some(port_dup),
                Some(delivery_queue_dup),
            )
            .await
            .context("FIDL error calling Mapper.OpenSession")?
            .map_err(|e| anyhow!("Mapper.OpenSession failed: {e:?}"))?;

        let scope = fasync::Scope::new();
        let cache_manager = Arc::new(BlobCacheManager::new(
            mapping_session,
            pager.clone(),
            delivery_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS)?,
            scope.to_handle(),
        ));
        let _delivery_processor =
            delivery::DeliveryQueueProcessor::spawn(receiver, cache_manager.clone(), delivery_vmo)?;

        Ok(Self {
            _delivery_processor,
            _mapper_session: mapper_session,
            port,
            pager,
            cache_manager,
            _scope: scope,
        })
    }

    /// Create pager owned VMO for the blob identified by its Merkle Root Hash.
    pub async fn create_vmo(&self, identifier: &[u8; 32]) -> Result<zx::Vmo, Error> {
        loop {
            let mut guard = match self.cache_manager.get_or_reserve(identifier)? {
                CacheLookup::Ready(child) => return Ok(child),
                CacheLookup::Pending(listener) => {
                    listener.await;
                    continue; // Re-evaluate cache now that the blocking event triggered
                }
                CacheLookup::Missing(guard) => guard,
            };

            // Ask Fxfs to register the blob and write its extent mappings into the shared mapping
            // VMO, returning a key that is used to generate the pager-backed VMO.
            let (size, key) = self
                .cache_manager
                .mapping_session
                .open(identifier)
                .await
                .context("FIDL error calling MappingSession.Open")?
                .map_err(|e| anyhow!("MappingSession.Open failed for blob: {e:?}"))?;

            let vmo =
                self.pager.create_vmo(zx::VmoOptions::empty(), &self.port, key as u64, size)?;

            // Create the initial child. We vend children of this VMO to clients so we can track
            // when all children are dropped to evict the blob from cache.
            let first_child = vmo
                .create_child(zx::VmoChildOptions::REFERENCE | zx::VmoChildOptions::NO_WRITE, 0, 0)
                .map_err(|s| anyhow!("Failed to create child VMO: {}", s))?;

            let zero_children_receiver = ZeroChildrenReceiver { blob: OnceLock::new() };
            let zero_children_registration =
                fasync::EHandle::local().register_receiver(zero_children_receiver);

            let cached = Arc::new(CachedBlob {
                vmo,
                root_hash: *identifier,
                cache_manager: Arc::downgrade(&self.cache_manager),
                vmo_key: key,
                len: size,
                registration: zero_children_registration,
                merkle_verifier: OnceLock::new(),
            });

            cached.registration.receiver().blob.set(Arc::downgrade(&cached)).map_err(|_| {
                anyhow!("Failed to bind CachedBlob to ZeroChildrenReceiver: already initialized")
            })?;

            cached.wait_for_zero_children();

            // All initialization succeeded: dismiss the drop guard and store the cached blob.
            guard.dismiss();
            self.cache_manager.finalize_pending_request(identifier, Some(&cached));

            return Ok(first_child);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delivery::DELIVERY_DATA_SIZE;
    use futures::TryStreamExt;
    use mapping::{DeliveryCommand, RawDeliveryCommand};
    use vmo_fifo::SyncSender;

    const TEST_BLOB_SIZE: u64 = (DELIVERY_DATA_SIZE * 2) as u64;
    const TEST_VMO_KEY: u32 = 42;

    // Used for testing BlobPagerAndVerifier interactions.
    // We arbitrarily allocate a 32KiB buffer so the payload spans multiple VMO pages.
    //
    // This test environment assumes a single blob file, identified by the `valid_root` hash.
    struct TestEnv {
        pager_and_verifier: Arc<BlobPagerAndVerifier>,
        mapping_task: fasync::Task<()>,
        mapper_task: fasync::Task<()>,
        close_signal: Option<futures::channel::oneshot::Receiver<()>>,
        delivery_vmo: zx::Vmo,
        pub valid_root: [u8; 32],
        pub valid_leaves: Vec<u8>,
        pub blob_data: Vec<u8>,
    }

    impl TestEnv {
        async fn new(blob_size: u64) -> Self {
            let blob_data = vec![0x42u8; blob_size as usize];
            let (root, leaf_hashes) =
                fuchsia_merkle::MerkleRootBuilder::new(Vec::new()).complete(&blob_data);
            let expected_hash: [u8; 32] = root.into();

            let mut flat_leaves = Vec::new();
            for hash in &leaf_hashes {
                flat_leaves.extend_from_slice(hash.as_bytes());
            }

            let (mapping_proxy, mut mapping_stream) =
                fidl::endpoints::create_proxy_and_stream::<fmapping::MappingProviderMarker>();

            let (close_tx, close_rx) = futures::channel::oneshot::channel();
            let mapping_task = fasync::Task::spawn(async move {
                let mut close_tx = Some(close_tx);
                if let Some(fmapping::MappingProviderRequest::OpenSession { session, responder }) =
                    mapping_stream.try_next().await.expect("try_next failed")
                {
                    let mapping_vmo = zx::Vmo::create(zx::system_get_page_size().into())
                        .expect("zx::Vmo::create failed");
                    responder.send(Ok(mapping_vmo)).expect("send failed");

                    let mut session_stream = session.into_stream();
                    let mut open_calls = 0;
                    while let Some(request) =
                        session_stream.try_next().await.expect("try_next failed")
                    {
                        match request {
                            fmapping::MappingSessionRequest::Open { identifier, responder } => {
                                assert_eq!(identifier, expected_hash);
                                open_calls += 1;
                                assert_eq!(
                                    open_calls, 1,
                                    "Open should only be called once per identifier"
                                );
                                responder.send(Ok((blob_size, TEST_VMO_KEY))).expect("send failed");
                            }
                            fmapping::MappingSessionRequest::Close { key, responder } => {
                                assert_eq!(key, TEST_VMO_KEY);
                                responder.send(Ok(())).expect("send failed");
                                if let Some(tx) = close_tx.take() {
                                    let _ = tx.send(());
                                }
                            }
                            _ => {}
                        }
                    }
                }
            });

            let (mapper_proxy, mut mapper_stream) =
                fidl::endpoints::create_proxy_and_stream::<fblock::MapperMarker>();

            let (tx_delivery, rx_delivery) = futures::channel::oneshot::channel();
            let mapper_task = fasync::Task::spawn(async move {
                if let Some(fblock::MapperRequest::OpenSession {
                    delivery_queue, responder, ..
                }) = mapper_stream.try_next().await.expect("try_next failed")
                {
                    tx_delivery.send(delivery_queue).expect("send failed");
                    responder.send(Ok(())).expect("send failed");
                }
            });

            Self {
                pager_and_verifier: Arc::new(
                    BlobPagerAndVerifier::new(&mapping_proxy, &mapper_proxy)
                        .await
                        .expect("BlobPagerAndVerifier::new failed"),
                ),
                mapping_task,
                mapper_task,
                close_signal: Some(close_rx),
                delivery_vmo: rx_delivery
                    .await
                    .expect("rx_delivery wait failed")
                    .expect("delivery_vmo was None"),
                valid_root: expected_hash,
                valid_leaves: flat_leaves,
                blob_data,
            }
        }

        async fn teardown(self) {
            drop(self.pager_and_verifier);
            self.mapping_task.await;
            self.mapper_task.await;
        }
    }

    struct ExpectedReadThread {
        started_rx: Option<futures::channel::oneshot::Receiver<()>>,
        done_rx: futures::channel::oneshot::Receiver<()>,
        handle: std::thread::JoinHandle<()>,
    }

    impl ExpectedReadThread {
        async fn wait_started(&mut self) {
            if let Some(rx) = self.started_rx.take() {
                rx.await.expect("reader thread failed to start");
            }
        }

        async fn wait_and_verify(self) {
            self.done_rx.await.expect("reader thread panicked or hung");
            self.handle.join().expect("join failed");
        }
    }

    fn spawn_reader_expect_error(
        vmo: &zx::Vmo,
        offset: u64,
        size: usize,
        expected_status: zx::Status,
    ) -> ExpectedReadThread {
        let vmo_clone =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("duplicate_handle failed");
        let (started_tx, started_rx) = futures::channel::oneshot::channel();
        let (done_tx, done_rx) = futures::channel::oneshot::channel();

        let handle = std::thread::spawn(move || {
            let mut buf = vec![0u8; size];
            let _ = started_tx.send(());
            let err = vmo_clone.read(&mut buf, offset).expect_err("read should fail");
            assert_eq!(err, expected_status);
            let _ = done_tx.send(());
        });

        ExpectedReadThread { started_rx: Some(started_rx), done_rx, handle }
    }

    #[fuchsia::test]
    async fn test_create_vmo() {
        let env = TestEnv::new(TEST_BLOB_SIZE).await;
        let vmo =
            env.pager_and_verifier.create_vmo(&env.valid_root).await.expect("Failed to create VMO");
        let size = vmo.get_size().expect("get_size failed");

        // Pager VMO sizes are rounded up to the nearest page boundary.
        let page_size = zx::system_get_page_size() as u64;
        let expected_pages = (TEST_BLOB_SIZE + page_size - 1) / page_size;
        assert_eq!(size, expected_pages * page_size);

        // Explicitly drop the VMO so `ZX_VMO_ZERO_CHILDREN` fires and the mock mapping
        // server receives the `.close()` IPC. Otherwise `teardown` will deadlock waiting
        // for the server loop to exit!
        drop(vmo);

        env.teardown().await;
    }

    #[fuchsia::test]
    async fn test_create_vmo_concurrent_access() {
        let env = TestEnv::new(TEST_BLOB_SIZE).await;
        let mut futures = vec![];
        for _ in 0..10 {
            let verifier = env.pager_and_verifier.clone();
            futures.push(fasync::Task::spawn(async move {
                verifier.create_vmo(&env.valid_root).await.expect("Failed to create VMO")
            }));
        }
        let vmos = futures::future::join_all(futures).await;

        drop(vmos);
        env.teardown().await;
    }

    #[fuchsia::test]
    async fn test_zero_children_eviction() {
        let mut env = TestEnv::new(TEST_BLOB_SIZE).await;

        let child_vmo =
            env.pager_and_verifier.create_vmo(&env.valid_root).await.expect("create_vmo failed");

        {
            let cache = env.pager_and_verifier.cache_manager.blob_states_by_hash.lock();
            assert!(matches!(cache.get(&env.valid_root), Some(BlobState::Ready { .. })));
        }

        drop(child_vmo);

        // wait for the packet receiver to observe the ZERO_CHILDREN signal and dispatch
        // `mapping_session.close()` to the mapping_server.
        env.close_signal.take().expect("Missing signal").await.expect("Failed to close");

        {
            let cache = env.pager_and_verifier.cache_manager.blob_states_by_hash.lock();
            assert!(cache.get(&env.valid_root).is_none());
        }

        env.teardown().await;
    }

    #[fuchsia::test]
    async fn test_cache_is_cleared_when_create_vmo_fails() {
        let (mapping_proxy, mut mapping_stream) =
            fidl::endpoints::create_proxy_and_stream::<fmapping::MappingProviderMarker>();
        let (mapper_proxy, mut mapper_stream) =
            fidl::endpoints::create_proxy_and_stream::<fblock::MapperMarker>();

        let (mock_opened_tx, mock_opened_rx) = futures::channel::oneshot::channel();
        let mapping_task = fasync::Task::spawn(async move {
            let mut mock_opened_tx = Some(mock_opened_tx);
            if let Some(fmapping::MappingProviderRequest::OpenSession { session, responder }) =
                mapping_stream.try_next().await.expect("try_next failed")
            {
                let mapping_vmo = zx::Vmo::create(zx::system_get_page_size().into())
                    .expect("zx::Vmo::create failed");
                responder.send(Ok(mapping_vmo)).expect("send failed");
                let mut session_stream = session.into_stream();

                if let Some(fmapping::MappingSessionRequest::Open {
                    responder: _responder, ..
                }) = session_stream.try_next().await.expect("try_next failed")
                {
                    // Signal the primary test task that the session is inside Open
                    if let Some(tx) = mock_opened_tx.take() {
                        let _ = tx.send(());
                    }

                    // Wait for test task to simulate failing create_vmo
                    let () = std::future::pending().await;
                }
            }
        });

        let mapper_task = fasync::Task::spawn(async move {
            if let Some(fblock::MapperRequest::OpenSession { responder, .. }) =
                mapper_stream.try_next().await.expect("try_next failed")
            {
                responder.send(Ok(())).expect("send failed");
            }
        });

        let pager_and_verifer = Arc::new(
            BlobPagerAndVerifier::new(&mapping_proxy, &mapper_proxy)
                .await
                .expect("BlobPagerAndVerifier::new failed"),
        );

        let blob_data = vec![0x42u8; TEST_BLOB_SIZE as usize];
        let (root, _) = fuchsia_merkle::MerkleRootBuilder::new(Vec::new()).complete(&blob_data);
        let hash_val: [u8; 32] = root.into();

        let verifier_clone = pager_and_verifer.clone();

        let identifier = hash_val;
        let (abortable_future, abort_handle) = futures::future::abortable(async move {
            let _ = verifier_clone.create_vmo(&identifier).await;
        });

        let identifier = &hash_val;
        let primary_task = fasync::Task::spawn(abortable_future);

        // Wait until the mock mapping session catches the `Open` request and check that the cache
        // state is updated.
        mock_opened_rx.await.expect("Open exited abruptly");
        {
            let cache = pager_and_verifer.cache_manager.blob_states_by_hash.lock();
            assert!(matches!(cache.get(identifier), Some(BlobState::Pending(_))));
        }

        // Simulate a failed create_vmo (future abort drops execution context)
        abort_handle.abort();
        let _ = primary_task.await;

        // The cache should be cleared after due to PendingCacheEntryGuard being dropped
        {
            let cache = pager_and_verifer.cache_manager.blob_states_by_hash.lock();
            assert!(cache.get(identifier).is_none());
        }

        drop(pager_and_verifer);
        drop(mapping_task);
        drop(mapper_task);
    }

    #[fuchsia::test]
    async fn test_create_vmo_error_clears_cache_and_unblocks_waiters() {
        let (mapping_proxy, mut mapping_stream) =
            fidl::endpoints::create_proxy_and_stream::<fmapping::MappingProviderMarker>();
        let (mapper_proxy, mut mapper_stream) =
            fidl::endpoints::create_proxy_and_stream::<fblock::MapperMarker>();

        let (open_received_tx, open_received_rx) = futures::channel::oneshot::channel();
        let (reply_tx, reply_rx) = futures::channel::oneshot::channel();

        let mapping_task = fasync::Task::spawn(async move {
            if let Some(fmapping::MappingProviderRequest::OpenSession { session, responder }) =
                mapping_stream.try_next().await.expect("try_next failed")
            {
                let mapping_vmo = zx::Vmo::create(zx::system_get_page_size().into())
                    .expect("zx::Vmo::create failed");
                responder.send(Ok(mapping_vmo)).expect("send failed");
                let mut session_stream = session.into_stream();

                if let Some(fmapping::MappingSessionRequest::Open { responder, .. }) =
                    session_stream.try_next().await.expect("try_next failed")
                {
                    // Signal the test that the primary caller has entered Open and is Pending.
                    open_received_tx.send(()).expect("send open_received failed");

                    // Await permission to reply with the error.
                    reply_rx.await.expect("reply_rx failed");
                    responder.send(Err(zx::Status::NOT_FOUND.into_raw())).expect("send failed");
                }
                // Channel closes when mapping_task finishes, causing subsequent requests to fail.
            }
        });

        let mapper_task = fasync::Task::spawn(async move {
            if let Some(fblock::MapperRequest::OpenSession { responder, .. }) =
                mapper_stream.try_next().await.expect("try_next failed")
            {
                responder.send(Ok(())).expect("send failed");
            }
        });

        let pager_and_verifier = Arc::new(
            BlobPagerAndVerifier::new(&mapping_proxy, &mapper_proxy)
                .await
                .expect("BlobPagerAndVerifier::new failed"),
        );

        let hash: [u8; 32] = [0x55; 32];

        // Primary caller starts create_vmo.
        let verifier1 = pager_and_verifier.clone();
        let primary_future = fasync::Task::spawn(async move { verifier1.create_vmo(&hash).await });

        // Wait until the primary caller is handling OpenSession (cache is Pending).
        open_received_rx.await.expect("open_received_rx failed");
        {
            let cache = pager_and_verifier.cache_manager.blob_states_by_hash.lock();
            assert!(matches!(cache.get(&hash), Some(BlobState::Pending(_))));
        }

        // A second caller arrives while the primary creation is still pending.
        let verifier2 = pager_and_verifier.clone();
        let secondary_future =
            fasync::Task::spawn(async move { verifier2.create_vmo(&hash).await });

        // Allow Fxfs to reply with an error to the primary caller.
        reply_tx.send(()).expect("reply_tx send failed");

        // Both callers must return an error and complete without hanging on abandoned state.
        assert!(primary_future.await.is_err());
        assert!(secondary_future.await.is_err());

        // Cache should be empty.
        {
            let cache = pager_and_verifier.cache_manager.blob_states_by_hash.lock();
            assert!(cache.get(&hash).is_none());
        }

        drop(pager_and_verifier);
        drop(mapping_task);
        drop(mapper_task);
    }

    #[fuchsia::test]
    async fn test_delivery_register_blob_verification() {
        let env = TestEnv::new(TEST_BLOB_SIZE).await;

        let _paged_vmo =
            env.pager_and_verifier.create_vmo(&env.valid_root).await.expect("create_vmo failed");

        let mut sender = SyncSender::<RawDeliveryCommand>::new(
            env.delivery_vmo
                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                .expect("duplicate_handle failed"),
            zx::system_get_page_size() as usize, // alignment of payload
            mapping::PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .expect("SyncSender::new failed");

        // Test corrupted leaves should fail register blob
        let mut corrupted_leaves = env.valid_leaves.clone();
        corrupted_leaves[0] ^= 0xFF;

        let mut bad_payload =
            sender.reserve_payload(corrupted_leaves.len()).expect("reserve_payload failed");
        bad_payload.data().copy_from_slice(&corrupted_leaves);

        let bad_raw_cmd: RawDeliveryCommand = DeliveryCommand::RegisterBlob {
            key: TEST_VMO_KEY as u64,
            offset: bad_payload.offset(),
            length: corrupted_leaves.len() as u32,
        }
        .into();
        bad_payload.commit(bad_raw_cmd).expect("commit failed");

        // Yield to allow background thread to process the invalid merkle
        fasync::Timer::new(std::time::Duration::from_millis(5)).await;

        let blob = env
            .pager_and_verifier
            .cache_manager
            .get_by_key(TEST_VMO_KEY)
            .expect("get_by_key failed");

        // merkle_verifier should remain as None as the leaves are corrupted
        assert!(blob.merkle_verifier.get().is_none());

        // Test with valid leaves
        let mut payload =
            sender.reserve_payload(env.valid_leaves.len()).expect("reserve_payload failed");
        payload.data().copy_from_slice(&env.valid_leaves);

        let raw_cmd: RawDeliveryCommand = DeliveryCommand::RegisterBlob {
            key: TEST_VMO_KEY as u64,
            offset: payload.offset(),
            length: env.valid_leaves.len() as u32,
        }
        .into();
        payload.commit(raw_cmd).expect("commit failed");

        while blob.merkle_verifier.get().is_none() {
            fasync::Timer::new(std::time::Duration::from_millis(5)).await;
        }

        drop(_paged_vmo);
        env.teardown().await;
    }

    #[fuchsia::test]
    async fn test_register_blob_invalid_commands() {
        let env = TestEnv::new(TEST_BLOB_SIZE).await;

        let _paged_vmo =
            env.pager_and_verifier.create_vmo(&env.valid_root).await.expect("create_vmo failed");

        let mut sender = SyncSender::<RawDeliveryCommand>::new(
            env.delivery_vmo
                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                .expect("duplicate_handle failed"),
            std::mem::align_of::<RawDeliveryCommand>(),
            mapping::PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .expect("SyncSender::new failed");

        let blob = env
            .pager_and_verifier
            .cache_manager
            .get_by_key(TEST_VMO_KEY)
            .expect("get_by_key failed");

        // Test with unknown/expired key
        let mut invalid_key_payload =
            sender.reserve_payload(env.valid_leaves.len()).expect("reserve_payload failed");
        invalid_key_payload.data().copy_from_slice(&env.valid_leaves);
        let invalid_key_cmd: RawDeliveryCommand = DeliveryCommand::RegisterBlob {
            key: 9999 as u64,
            offset: invalid_key_payload.offset(),
            length: env.valid_leaves.len() as u32,
        }
        .into();
        invalid_key_payload.commit(invalid_key_cmd).expect("commit failed");
        fasync::Timer::new(std::time::Duration::from_millis(5)).await;
        assert!(blob.merkle_verifier.get().is_none());

        // Test out-of-bounds offset/length
        let oob_payload = sender.reserve_payload(8).expect("reserve_payload failed");
        let oob_cmd: RawDeliveryCommand = DeliveryCommand::RegisterBlob {
            key: TEST_VMO_KEY as u64,
            offset: u32::MAX - 4, // Malicious offset
            length: 8,
        }
        .into();
        oob_payload.commit(oob_cmd).expect("commit failed");
        fasync::Timer::new(std::time::Duration::from_millis(5)).await;
        assert!(blob.merkle_verifier.get().is_none());

        // Test invalid leaf length (not a multiple of HASH_SIZE)
        let invalid_len_payload = sender.reserve_payload(10).expect("reserve_payload failed");
        let invalid_len_cmd: RawDeliveryCommand = DeliveryCommand::RegisterBlob {
            key: TEST_VMO_KEY as u64,
            offset: invalid_len_payload.offset(),
            length: 10,
        }
        .into();
        invalid_len_payload.commit(invalid_len_cmd).expect("commit failed");
        fasync::Timer::new(std::time::Duration::from_millis(5)).await;
        assert!(blob.merkle_verifier.get().is_none());

        drop(_paged_vmo);
        env.teardown().await;
    }

    #[fuchsia::test]
    async fn test_delivery_data_supplies_pages() {
        let env = TestEnv::new(TEST_BLOB_SIZE).await;

        let paged_vmo =
            env.pager_and_verifier.create_vmo(&env.valid_root).await.expect("create_vmo failed");

        let mut sender = SyncSender::<RawDeliveryCommand>::new(
            env.delivery_vmo
                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                .expect("duplicate_handle failed"),
            zx::system_get_page_size() as usize, // alignment of payload
            mapping::PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .expect("SyncSender::new failed");

        let mut payload =
            sender.reserve_payload(env.valid_leaves.len()).expect("reserve_payload failed");
        payload.data().copy_from_slice(&env.valid_leaves);
        let raw_cmd: RawDeliveryCommand = DeliveryCommand::RegisterBlob {
            key: TEST_VMO_KEY as u64,
            offset: payload.offset(),
            length: env.valid_leaves.len() as u32,
        }
        .into();
        payload.commit(raw_cmd).expect("commit failed");

        let blob = env
            .pager_and_verifier
            .cache_manager
            .get_by_key(TEST_VMO_KEY)
            .expect("get_by_key failed");
        while blob.merkle_verifier.get().is_none() {
            fasync::Timer::new(std::time::Duration::from_millis(5)).await;
        }

        let test_payload = &env.blob_data[..DELIVERY_DATA_SIZE];

        let mut payload =
            sender.reserve_payload(test_payload.len()).expect("reserve_payload failed");
        payload.data().copy_from_slice(&test_payload);

        let cmd: RawDeliveryCommand = DeliveryCommand::Data {
            key: TEST_VMO_KEY as u64,
            target_offset: 0,
            length: test_payload.len() as u32,
            offset: payload.offset(),
        }
        .into();
        payload.commit(cmd).expect("commit failed");

        let mut read_buf = vec![0u8; DELIVERY_DATA_SIZE];
        paged_vmo.read(&mut read_buf, 0).expect("read paged_vmo failed");
        assert_eq!(read_buf, test_payload);

        drop(paged_vmo);
        env.teardown().await;
    }

    #[fuchsia::test]
    async fn test_delivery_data_corrupted() {
        // Verify that when a delivered chunk fails verification, client threads waiting
        // for data within that chunk receive `ZX_ERR_IO_DATA_INTEGRITY`.
        let blob_size = DELIVERY_DATA_SIZE as u64;
        let env = TestEnv::new(blob_size).await;

        let paged_vmo =
            env.pager_and_verifier.create_vmo(&env.valid_root).await.expect("create_vmo failed");

        let mut sender = SyncSender::<RawDeliveryCommand>::new(
            env.delivery_vmo
                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                .expect("duplicate_handle failed"),
            zx::system_get_page_size() as usize,
            mapping::PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .expect("SyncSender::new failed");

        let mut payload =
            sender.reserve_payload(env.valid_leaves.len()).expect("reserve_payload failed");
        payload.data().copy_from_slice(&env.valid_leaves);
        let raw_cmd: RawDeliveryCommand = DeliveryCommand::RegisterBlob {
            key: TEST_VMO_KEY as u64,
            offset: payload.offset(),
            length: env.valid_leaves.len() as u32,
        }
        .into();
        payload.commit(raw_cmd).expect("commit failed");

        let blob = env
            .pager_and_verifier
            .cache_manager
            .get_by_key(TEST_VMO_KEY)
            .expect("get_by_key failed");
        while blob.merkle_verifier.get().is_none() {
            fasync::Timer::new(std::time::Duration::from_millis(5)).await;
        }

        // Prepare a corrupted 128 KiB chunk (the entire blob data).
        let mut corrupted_data = env.blob_data.clone();
        corrupted_data[0] ^= 0xFF;

        let mut payload =
            sender.reserve_payload(corrupted_data.len()).expect("reserve_payload failed");
        payload.data().copy_from_slice(&corrupted_data);
        let raw_cmd: RawDeliveryCommand = DeliveryCommand::Data {
            key: TEST_VMO_KEY as u64,
            offset: payload.offset(),
            length: corrupted_data.len() as u32,
            target_offset: 0,
        }
        .into();

        // Start client threads reading different pages within the delivery data chunk.
        // When verification fails, all waiting threads must fail with `IO_DATA_INTEGRITY`.
        let page_size = zx::system_get_page_size() as usize;
        let mut reader1 =
            spawn_reader_expect_error(&paged_vmo, 0, page_size, zx::Status::IO_DATA_INTEGRITY);
        let mut reader2 = spawn_reader_expect_error(
            &paged_vmo,
            page_size as u64,
            page_size,
            zx::Status::IO_DATA_INTEGRITY,
        );

        // Wait for both client threads to start executing before giving them time to block on their
        // page faults.
        reader1.wait_started().await;
        reader2.wait_started().await;
        fasync::Timer::new(std::time::Duration::from_millis(5)).await;

        // Commit the corrupt chunk. Verification will fail, causing the blocked `vmo.read()`
        // calls for that chunk to return `IO_DATA_INTEGRITY`.
        payload.commit(raw_cmd).expect("commit failed");

        reader1.wait_and_verify().await;
        reader2.wait_and_verify().await;

        drop(paged_vmo);
        env.teardown().await;
    }

    #[fuchsia::test]
    async fn test_delivery_data_corrupted_chunk_preserves_supplied_pages() {
        let chunk_size = DELIVERY_DATA_SIZE;
        let blob_size = (chunk_size * 2) as u64;
        let env = TestEnv::new(blob_size).await;

        let paged_vmo =
            env.pager_and_verifier.create_vmo(&env.valid_root).await.expect("create_vmo failed");

        let mut sender = SyncSender::<RawDeliveryCommand>::new(
            env.delivery_vmo
                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                .expect("duplicate_handle failed"),
            zx::system_get_page_size() as usize,
            mapping::PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .expect("SyncSender::new failed");

        let mut payload =
            sender.reserve_payload(env.valid_leaves.len()).expect("reserve_payload failed");
        payload.data().copy_from_slice(&env.valid_leaves);
        let raw_cmd: RawDeliveryCommand = DeliveryCommand::RegisterBlob {
            key: TEST_VMO_KEY as u64,
            offset: payload.offset(),
            length: env.valid_leaves.len() as u32,
        }
        .into();
        payload.commit(raw_cmd).expect("commit failed");

        let blob = env
            .pager_and_verifier
            .cache_manager
            .get_by_key(TEST_VMO_KEY)
            .expect("get_by_key failed");
        while blob.merkle_verifier.get().is_none() {
            fasync::Timer::new(std::time::Duration::from_millis(5)).await;
        }

        let page_size = zx::system_get_page_size() as usize;

        // Chunk 1 test for successful page request.
        // Start a reader on page 0 and deliver the valid first chunk (0..chunk_size).
        let paged_vmo_clone1 =
            paged_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("duplicate_handle failed");
        let (tx1, rx1) = futures::channel::oneshot::channel();
        let expected_page0 = env.blob_data[..page_size].to_vec();
        let thread1 = std::thread::spawn(move || {
            let mut buf = vec![0u8; page_size];
            paged_vmo_clone1.read(&mut buf, 0).expect("read should succeed");
            assert_eq!(buf, expected_page0);
            let _ = tx1.send(());
        });
        fasync::Timer::new(std::time::Duration::from_millis(5)).await;

        let valid_chunk = &env.blob_data[..chunk_size];
        let mut payload =
            sender.reserve_payload(valid_chunk.len()).expect("reserve_payload failed");
        payload.data().copy_from_slice(valid_chunk);
        let raw_cmd: RawDeliveryCommand = DeliveryCommand::Data {
            key: TEST_VMO_KEY as u64,
            offset: payload.offset(),
            length: valid_chunk.len() as u32,
            target_offset: 0,
        }
        .into();
        payload.commit(raw_cmd).expect("commit failed");

        rx1.await.expect("thread1 panicked or hung");
        thread1.join().expect("join failed");

        // Test for failed page requests on the subsequent chunk.
        let mut reader2 = spawn_reader_expect_error(
            &paged_vmo,
            chunk_size as u64,
            page_size,
            zx::Status::IO_DATA_INTEGRITY,
        );
        reader2.wait_started().await;
        fasync::Timer::new(std::time::Duration::from_millis(5)).await;

        // Deliver corrupted second chunk (chunk_size..chunk_size * 2).
        let mut corrupted_chunk = env.blob_data[chunk_size..].to_vec();
        corrupted_chunk[0] ^= 0xFF;

        let mut payload =
            sender.reserve_payload(corrupted_chunk.len()).expect("reserve_payload failed");
        payload.data().copy_from_slice(&corrupted_chunk);
        let raw_cmd: RawDeliveryCommand = DeliveryCommand::Data {
            key: TEST_VMO_KEY as u64,
            offset: payload.offset(),
            length: corrupted_chunk.len() as u32,
            target_offset: chunk_size as u64,
        }
        .into();
        payload.commit(raw_cmd).expect("commit failed");

        reader2.wait_and_verify().await;

        // Verify that the previously supplied page 0 remains intact and readable.
        let mut buf = vec![0u8; page_size];
        paged_vmo.read(&mut buf, 0).expect("previously supplied page 0 must still be readable");
        assert_eq!(buf, env.blob_data[..page_size]);

        drop(paged_vmo);
        env.teardown().await;
    }

    #[fuchsia::test]
    async fn test_delivery_data_corrupted_multi_chunk_read() {
        let chunk_size = DELIVERY_DATA_SIZE;
        let blob_size = (chunk_size * 3) as u64;
        let env = TestEnv::new(blob_size).await;

        let paged_vmo =
            env.pager_and_verifier.create_vmo(&env.valid_root).await.expect("create_vmo failed");

        let mut sender = SyncSender::<RawDeliveryCommand>::new(
            env.delivery_vmo
                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                .expect("duplicate_handle failed"),
            zx::system_get_page_size() as usize,
            mapping::PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .expect("SyncSender::new failed");

        let mut payload =
            sender.reserve_payload(env.valid_leaves.len()).expect("reserve_payload failed");
        payload.data().copy_from_slice(&env.valid_leaves);
        let raw_cmd: RawDeliveryCommand = DeliveryCommand::RegisterBlob {
            key: TEST_VMO_KEY as u64,
            offset: payload.offset(),
            length: env.valid_leaves.len() as u32,
        }
        .into();
        payload.commit(raw_cmd).expect("commit failed");

        let blob = env
            .pager_and_verifier
            .cache_manager
            .get_by_key(TEST_VMO_KEY)
            .expect("get_by_key failed");
        while blob.merkle_verifier.get().is_none() {
            fasync::Timer::new(std::time::Duration::from_millis(5)).await;
        }

        // A 384 KiB `vmo.read()` triggers a single page request spanning multiple pages.
        // Chunk 1 (0..128 KiB) is valid, Chunk 2 (128..256 KiB) is corrupted, and Chunk 3 is not
        // delivered because delivery stops on verification failure.
        // Verify that when Chunk 2 fails verification, the page request fails and
        // `vmo.read()` returns `IO_DATA_INTEGRITY`.
        let mut reader = spawn_reader_expect_error(
            &paged_vmo,
            0,
            blob_size as usize,
            zx::Status::IO_DATA_INTEGRITY,
        );
        reader.wait_started().await;
        fasync::Timer::new(std::time::Duration::from_millis(5)).await;

        // Deliver Chunk 1 (0..128 KiB) valid.
        let valid_chunk1 = &env.blob_data[..chunk_size];
        let mut payload =
            sender.reserve_payload(valid_chunk1.len()).expect("reserve_payload failed");
        payload.data().copy_from_slice(valid_chunk1);
        let raw_cmd: RawDeliveryCommand = DeliveryCommand::Data {
            key: TEST_VMO_KEY as u64,
            offset: payload.offset(),
            length: valid_chunk1.len() as u32,
            target_offset: 0,
        }
        .into();
        payload.commit(raw_cmd).expect("commit failed");

        // Allow the reader thread time to receive the supplied first chunk, copy it, and block on
        // the second chunk's unpopulated pages.
        fasync::Timer::new(std::time::Duration::from_millis(5)).await;

        // Deliver Chunk 2 (128..256 KiB) corrupted.
        let mut corrupted_chunk2 = env.blob_data[chunk_size..chunk_size * 2].to_vec();
        corrupted_chunk2[0] ^= 0xFF;
        let mut payload =
            sender.reserve_payload(corrupted_chunk2.len()).expect("reserve_payload failed");
        payload.data().copy_from_slice(&corrupted_chunk2);
        let raw_cmd: RawDeliveryCommand = DeliveryCommand::Data {
            key: TEST_VMO_KEY as u64,
            offset: payload.offset(),
            length: corrupted_chunk2.len() as u32,
            target_offset: chunk_size as u64,
        }
        .into();
        payload.commit(raw_cmd).expect("commit failed");

        // Don't deliver chunk 3 since chunk 2 failed.

        // The `vmo.read()` must fail with IO_DATA_INTEGRITY.
        reader.wait_and_verify().await;

        drop(paged_vmo);
        env.teardown().await;
    }

    #[fuchsia::test]
    async fn test_delivery_data_uninitialized() {
        let env = TestEnv::new(TEST_BLOB_SIZE).await;

        let paged_vmo =
            env.pager_and_verifier.create_vmo(&env.valid_root).await.expect("create_vmo failed");

        let mut sender = SyncSender::<RawDeliveryCommand>::new(
            env.delivery_vmo
                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                .expect("duplicate_handle failed"),
            zx::system_get_page_size() as usize,
            mapping::PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .expect("SyncSender::new failed");

        // Spawn reader thread to generate the initial page request
        let mut reader = spawn_reader_expect_error(&paged_vmo, 0, 8192, zx::Status::BAD_STATE);
        reader.wait_started().await;
        fasync::Timer::new(std::time::Duration::from_millis(5)).await;

        // Intentionally push a Data chunk without sending RegisterBlob first
        let chunk = &env.blob_data[..8192];
        let mut payload = sender.reserve_payload(chunk.len()).expect("reserve_payload failed");
        payload.data().copy_from_slice(chunk);
        let raw_cmd: RawDeliveryCommand = DeliveryCommand::Data {
            key: TEST_VMO_KEY as u64,
            offset: payload.offset(),
            length: chunk.len() as u32,
            target_offset: 0,
        }
        .into();
        payload.commit(raw_cmd).expect("commit failed");

        reader.wait_and_verify().await;

        drop(paged_vmo);
        env.teardown().await;
    }

    #[fuchsia::test]
    async fn test_delivery_data_multiple_chunks() {
        let env = TestEnv::new(TEST_BLOB_SIZE).await;

        let paged_vmo =
            env.pager_and_verifier.create_vmo(&env.valid_root).await.expect("create_vmo failed");

        let mut sender = SyncSender::<RawDeliveryCommand>::new(
            env.delivery_vmo
                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                .expect("duplicate_handle failed"),
            zx::system_get_page_size() as usize,
            mapping::PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .expect("SyncSender::new failed");

        let mut payload =
            sender.reserve_payload(env.valid_leaves.len()).expect("reserve_payload failed");
        payload.data().copy_from_slice(&env.valid_leaves);
        let raw_cmd: RawDeliveryCommand = DeliveryCommand::RegisterBlob {
            key: TEST_VMO_KEY as u64,
            offset: payload.offset(),
            length: env.valid_leaves.len() as u32,
        }
        .into();
        payload.commit(raw_cmd).expect("commit failed");

        let blob = env
            .pager_and_verifier
            .cache_manager
            .get_by_key(TEST_VMO_KEY)
            .expect("get_by_key failed");
        while blob.merkle_verifier.get().is_none() {
            fasync::Timer::new(std::time::Duration::from_millis(5)).await;
        }

        let expected_data = env.blob_data.clone();
        let paged_vmo_clone =
            paged_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("duplicate_handle failed");

        let (tx, rx) = futures::channel::oneshot::channel();
        let thread = std::thread::spawn(move || {
            let mut buf = vec![0u8; expected_data.len()];
            paged_vmo_clone.read(&mut buf, 0).expect("failed to read from paged vmo");
            assert_eq!(buf, expected_data);
            let _ = tx.send(());
        });

        // Push incrementally - chunks must be a multiple of DELIVERY_DATA_SIZE
        let chunk_size = DELIVERY_DATA_SIZE;
        for (i, chunk) in env.blob_data.chunks(chunk_size).enumerate() {
            let mut payload = sender.reserve_payload(chunk.len()).expect("reserve_payload failed");
            payload.data().copy_from_slice(chunk);
            let raw_cmd: RawDeliveryCommand = DeliveryCommand::Data {
                key: TEST_VMO_KEY as u64,
                offset: payload.offset(),
                length: chunk.len() as u32,
                target_offset: (i * chunk_size) as u64,
            }
            .into();
            payload.commit(raw_cmd).expect("commit failed");
        }

        rx.await.expect("reading thread panicked or hung");
        thread.join().expect("join failed");

        drop(paged_vmo);
        env.teardown().await;
    }

    #[fuchsia::test]
    async fn test_delivery_data_unaligned_blob_size() {
        let data_size = (DELIVERY_DATA_SIZE + 1024) as u64; // Blob with unaligned size
        let env = TestEnv::new(data_size).await;

        let paged_vmo =
            env.pager_and_verifier.create_vmo(&env.valid_root).await.expect("Failed to create VMO");

        let mut sender = SyncSender::<RawDeliveryCommand>::new(
            env.delivery_vmo
                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                .expect("duplicate_handle failed"),
            zx::system_get_page_size() as usize, // alignment of payload
            mapping::PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .expect("SyncSender::new failed");

        let mut payload =
            sender.reserve_payload(env.valid_leaves.len()).expect("reserve_payload failed");
        payload.data().copy_from_slice(&env.valid_leaves);
        let raw_cmd: RawDeliveryCommand = DeliveryCommand::RegisterBlob {
            key: TEST_VMO_KEY as u64,
            offset: payload.offset(),
            length: env.valid_leaves.len() as u32,
        }
        .into();
        payload.commit(raw_cmd).expect("commit failed");

        let blob = env
            .pager_and_verifier
            .cache_manager
            .get_by_key(TEST_VMO_KEY)
            .expect("get_by_key failed");
        while blob.merkle_verifier.get().is_none() {
            fasync::Timer::new(std::time::Duration::from_millis(5)).await;
        }

        let test_payload = env.blob_data.clone();
        assert_eq!(test_payload.len(), data_size as usize);

        let chunk1 = &test_payload[..DELIVERY_DATA_SIZE];
        let mut payload1 = sender.reserve_payload(chunk1.len()).expect("reserve_payload failed");
        payload1.data().copy_from_slice(chunk1);
        let off1 = payload1.offset();
        payload1
            .commit(
                DeliveryCommand::Data {
                    key: TEST_VMO_KEY as u64,
                    target_offset: 0,
                    length: chunk1.len() as u32,
                    offset: off1,
                }
                .into(),
            )
            .expect("commit failed");

        let chunk2 = &test_payload[DELIVERY_DATA_SIZE..];
        let page_size = zx::system_get_page_size() as usize;
        let chunk2_len_aligned = chunk2.len().div_ceil(page_size) * page_size;
        let mut payload2 =
            sender.reserve_payload(chunk2_len_aligned).expect("reserve_payload failed");
        let payload2_data = payload2.data();
        payload2_data.subslice_mut(0..chunk2.len()).copy_from_slice(chunk2);
        payload2_data.subslice_mut(chunk2.len()..chunk2_len_aligned).fill(0);

        let off2 = payload2.offset();
        payload2
            .commit(
                DeliveryCommand::Data {
                    key: TEST_VMO_KEY as u64,
                    target_offset: DELIVERY_DATA_SIZE as u64,
                    length: chunk2_len_aligned as u32,
                    offset: off2,
                }
                .into(),
            )
            .expect("commit failed");

        let mut read_buf = vec![0u8; data_size as usize];
        paged_vmo.read(&mut read_buf, 0).expect("read paged_vmo failed");
        assert_eq!(read_buf, test_payload);

        let page_size = zx::system_get_page_size() as u64;
        // Round the unaligned data size to the next page boundary
        let out_of_bounds_offset = data_size.div_ceil(page_size) * page_size;
        // Attempt to read from an out of bounds offset
        let mut read_buf = vec![0u8; 1];
        let res = paged_vmo.read(&mut read_buf, out_of_bounds_offset);
        assert_eq!(res, Err(zx::Status::OUT_OF_RANGE));

        drop(paged_vmo);
        env.teardown().await;
    }
}
