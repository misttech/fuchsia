// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Coordinates pager-backed blob VMOs, driver-level page fault handling, and cryptographic
//! verification.
//!
//! # Overview
//! `BlobPagerAndVerifier` manages pager-backed VMOs for blobs. It decouples page fault servicing
//! from the filesystem by having the block driver read and decompress raw disk blocks directly,
//! while `BlobPagerAndVerifier` cryptographically verifies the pages before supplying them to
//! the pager VMO to unblock client reads.
//!
//! # Blob Opening and Caching
//! When a client requests a blob via `create_vmo`:
//! - **If already cached**: Clones and returns a new read-only child VMO immediately.
//! - **If currently opening**: Suspends until the in-flight request finishes, then clones and
//!   returns a child VMO once ready.
//! - **If not in cache**: Registers the blob's extent mappings with Fxfs, creates the pager-backed
//!   VMO, caches it, and returns a child VMO to the caller.
//!
//! # Page Fault Handling and Verification
//! 1. When a client reads an unpopulated page, the kernel generates a page request on the pager
//!    port, which the block driver monitors directly.
//! 2. The driver reads and decompresses the disk blocks according to the blob's extent mappings,
//!    then writes unverified pages and Merkle metadata into a shared delivery queue VMO.
//! 3. `BlobPagerAndVerifier` verifies the incoming pages against the blob's Merkle tree:
//!    - On success, it supplies the verified pages to the pager VMO (`zx_pager_supply_pages`),
//!      unblocking the client read.
//!    - On verification failure, it fails the request with `ZX_ERR_IO_DATA_INTEGRITY`.
//!
//! # Eviction and Teardown
//! 1. Clients receive read-only child VMOs cloned from the parent pager VMO.
//! 2. When all client handles to a blob are dropped, the kernel notifies the cache via
//!    `ZX_VMO_ZERO_CHILDREN`.
//! 3. The cache evicts the inactive blob and closes its mapping session with Fxfs, reclaiming
//!    driver extent tracking and kernel pager resources.

mod delivery;
mod profile;

pub use delivery::{
    DeliveryQueueProcessor, DeliveryQueueProvider, TestVmoProvider, UnverifiedPages,
};
pub use profile::ProfileRecording;

use anyhow::{Context, Error, anyhow, bail};
use event_listener as _;
use fidl_fuchsia_storage_block as fblock;
use fidl_fuchsia_storage_mapping as fmapping;
use fuchsia_async as fasync;
use fuchsia_hash::{HASH_SIZE, Hash};
use fuchsia_merkle::{MerkleVerifier, ReadSizedMerkleVerifier};
use fuchsia_sync::Mutex;
use futures::TryStreamExt as _;
use std::collections::{HashMap, HashSet, VecDeque, hash_map};
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
                    if let Some(cache) = cached_blob.cache.upgrade() {
                        cache.on_zero_children(&cached_blob);
                    }
                }
            }
        }
    }
}

#[derive(Default)]
struct InlineTailQueue {
    busy: bool,
    pending: VecDeque<delivery::ChildInlineTail>,
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
    // Weak back-reference to Cache to avoid an Arc reference cycle, since the cache already
    // holds an Arc to this blob.
    cache: Weak<Cache>,
    // Session key assigned by Fxfs for this blob mapping, used in driver delivery commands and when
    // closing the session.
    vmo_key: u64,
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
        if let Some(cache) = self.cache.upgrade() {
            let session = cache.mapping_session.clone();
            let vmo_key = self.vmo_key;
            cache.scope.spawn(async move {
                let _ = session.close(vmo_key).await;
            });
        }
    }
}

enum BlobState {
    // The blob is actively opening: extents are being registered with Fxfs and the pager VMO is
    // being created. Concurrent requests wait on `event`. Early Merkle leaves delivered by the
    // driver are placed directly into `verifier`.
    Opening {
        root_hash: [u8; 32],
        event: Arc<event_listener::Event>,
        verifier: Option<ReadSizedMerkleVerifier>,
    },
    // The blob's pager-backed VMO is created and cached. Eviction is triggered when all client
    // child VMOs are closed (`ZX_VMO_ZERO_CHILDREN`).
    Ready(Arc<CachedBlob>),
}

pub(crate) struct ActiveRecording {
    // Blobs opened by external clients during the active recording window.
    pub(crate) opened_hashes: HashSet<Hash>,
    // Maps session key to Merkle root hash for all blobs opened or accessed during the recording
    // window, retained until recording stops even if a blob is evicted before then. When all child
    // VMO handles are dropped (`ZX_VMO_ZERO_CHILDREN`), `Cache` evicts the blob and closes its
    // session key; if the same blob is later opened again during the same recording window, `Cache`
    // allocates a new monotonically increasing session key for it. Retaining every key to `Hash`
    // mapping ensures page requests recorded by the driver under any of those keys resolve back to
    // the blob's `Hash` when recording stops.
    pub(crate) hash_by_key: HashMap<u64, Hash>,
}

// Holds a pager-backed VMO created for a file in a child `MapperSession` (e.g. an EROFS file
// mapped inside a parent blob) along with its unaligned logical byte size.
struct ChildVmo {
    vmo: zx::Vmo,
    len: u64,
}

struct CacheState {
    next_key: u64,
    // Maps blob Merkle root hash to the client-allocated mapping session key.
    keys_by_hash: HashMap<[u8; 32], u64>,
    blob_states_by_key: HashMap<u64, BlobState>,
    recording: Option<ActiveRecording>,
    // Maps `(parent_blob_key, child_vmo_key)` to the child VMO registered via
    // `BlobPagerAndVerifier::open_child_session`.
    child_vmos: HashMap<(u64, u64), Arc<ChildVmo>>,
}

impl Default for CacheState {
    fn default() -> Self {
        Self {
            next_key: 1,
            keys_by_hash: HashMap::default(),
            blob_states_by_key: HashMap::default(),
            recording: None,
            child_vmos: HashMap::default(),
        }
    }
}

impl CacheState {
    fn generate_key(&mut self) -> u64 {
        let key = self.next_key;
        self.next_key += 1;
        key
    }
}

// Holds a pending reservation for a blob actively being opened.
//
// If dropped before `commit` is called (e.g. due to an early FIDL failure, pager VMO creation
// failure, or cancellation), the `Drop` implementation rolls back the reservation: it evicts the
// key from the cache, queues a close request with Fxfs, and unblocks waiting tasks.
struct PendingEntry {
    cache: Arc<Cache>,
    key: u64,
}

impl PendingEntry {
    fn key(&self) -> u64 {
        self.key
    }

    // Fulfills the pending entry by constructing the `CachedBlob`, atomically transitioning the
    // cache entry from `Opening` to `Ready`, and notifying waiting tasks.
    fn commit(self, vmo: zx::Vmo, size: u64) -> Arc<CachedBlob> {
        let zero_children_receiver = ZeroChildrenReceiver { blob: OnceLock::new() };
        let zero_children_registration =
            fasync::EHandle::local().register_receiver(zero_children_receiver);

        let (cached, event) = {
            let mut state = self.cache.state.lock();
            let (root_hash, event, verifier) = match state.blob_states_by_key.remove(&self.key) {
                Some(BlobState::Opening { root_hash, event, verifier }) => {
                    (root_hash, event, verifier)
                }
                _ => {
                    unreachable!("Blob for key {} was unexpectedly not in Opening state", self.key)
                }
            };

            let merkle_verifier = OnceLock::new();
            if let Some(verifier) = verifier {
                let _ = merkle_verifier.set(verifier);
            }

            let cached = Arc::new(CachedBlob {
                vmo,
                root_hash,
                cache: Arc::downgrade(&self.cache),
                vmo_key: self.key,
                len: size,
                registration: zero_children_registration,
                merkle_verifier,
            });

            cached
                .registration
                .receiver()
                .blob
                .set(Arc::downgrade(&cached))
                .expect("Failed to bind CachedBlob to ZeroChildrenReceiver: already initialized");

            state.blob_states_by_key.insert(self.key, BlobState::Ready(cached.clone()));
            if let Some(recording) = state.recording.as_mut() {
                recording.hash_by_key.insert(self.key, Hash::from(root_hash));
            }

            (cached, event)
        };

        cached.wait_for_zero_children();
        event.notify(usize::MAX);
        cached
    }
}

impl Drop for PendingEntry {
    fn drop(&mut self) {
        let event = {
            let mut state = self.cache.state.lock();
            match state.blob_states_by_key.get(&self.key) {
                // If the blob committed successfully, there is nothing to roll back.
                Some(BlobState::Ready(_)) => return,
                Some(BlobState::Opening { .. }) => {
                    if let Some(BlobState::Opening { root_hash, event, .. }) =
                        state.blob_states_by_key.remove(&self.key)
                    {
                        state.keys_by_hash.remove(&root_hash);
                        event
                    } else {
                        return;
                    }
                }
                None => {
                    log::error!(
                        key = self.key;
                        "PendingEntry::drop: key unexpectedly absent from cache"
                    );
                    return;
                }
            }
        };

        // Queue the close on Fxfs before waking waiters.
        // Note: If `open()` was never called or returned an error, closing this key is safe
        // because the block driver ignores `CloseBlob` for unknown keys. However, if `open()` was
        // in-flight when cancelled, or if `open()` succeeded but subsequent operations failed, this
        // close prevents leaking extent mappings.
        let session = self.cache.mapping_session.clone();
        let key = self.key;
        self.cache.scope.spawn(async move {
            let _ = session.close(key).await;
        });

        // Wake up all suspended tasks concurrently waiting on this blob.
        event.notify(usize::MAX);
    }
}

// The outcome of looking up or reserving an entry in `Cache`.
enum CacheLookup {
    // The VMO has been created and cached. Returns a child of it.
    Ready(zx::Vmo),
    // The VMO is currently being created by another request. Contains a listener to await
    // completion.
    Pending(event_listener::EventListener),
    // The blob is absent. Provides a drop-safe pending entry to be used with creation. If the task
    // crashes or returns an early error, the pending state will be cleared from the cache.
    Missing(PendingEntry),
}

// Coordinates the concurrent creation, caching, and verification of pager-backed blobs.
pub(crate) struct Cache {
    state: Mutex<CacheState>,
    pub(crate) mapping_session: fmapping::MappingSessionProxy,
    pager: Arc<zx::Pager>,
    port: zx::Port,
    delivery_vmo: zx::Vmo,
    pub(crate) scope: fasync::ScopeHandle,
    // Reusable transfer VMOs for supplying `ChildInlineTail` pages.
    transfer_vmos: Mutex<Vec<zx::Vmo>>,
    // Leader-follower queue for servicing `ChildInlineTail` commands so at most one delivery worker
    // thread ever blocks on an evicted or unsupplied parent VMO page.
    inline_tail_queue: Mutex<InlineTailQueue>,
}

const MAX_POOLED_TRANSFER_VMOS: usize = 8;

// Extracts a `zx::Status` from `err`, defaulting to `zx::Status::BAD_STATE` if none is present.
fn status_from_error(err: &Error) -> zx::Status {
    err.downcast_ref::<zx::Status>().copied().unwrap_or(zx::Status::BAD_STATE)
}

impl Cache {
    fn new(
        mapping_session: fmapping::MappingSessionProxy,
        pager: Arc<zx::Pager>,
        port: zx::Port,
        delivery_vmo: zx::Vmo,
        scope: fasync::ScopeHandle,
    ) -> Self {
        Self {
            state: Mutex::new(CacheState::default()),
            mapping_session,
            pager,
            port,
            delivery_vmo,
            scope,
            transfer_vmos: Mutex::new(Vec::new()),
            inline_tail_queue: Mutex::new(InlineTailQueue::default()),
        }
    }

    fn delivery_vmo(&self) -> &zx::Vmo {
        &self.delivery_vmo
    }

    fn get_by_key(&self, key: u64) -> Option<Arc<CachedBlob>> {
        let state = self.state.lock();
        match state.blob_states_by_key.get(&key) {
            Some(BlobState::Ready(cached)) => Some(cached.clone()),
            _ => None,
        }
    }

    // Registers a pager-backed child VMO (`child_key`) with logical byte size `len` under the
    // parent blob `parent_key`.
    fn register_child_vmo(&self, parent_key: u64, child_key: u64, vmo: zx::Vmo, len: u64) {
        self.state
            .lock()
            .child_vmos
            .insert((parent_key, child_key), Arc::new(ChildVmo { vmo, len }));
    }

    // Removes the child VMO registered under `(parent_key, child_key)`.
    fn unregister_child_vmo(&self, parent_key: u64, child_key: u64) {
        self.state.lock().child_vmos.remove(&(parent_key, child_key));
    }

    // Looks up the child VMO registered under `(parent_key, child_key)`.
    fn get_child_vmo(&self, parent_key: u64, child_key: u64) -> Option<Arc<ChildVmo>> {
        self.state.lock().child_vmos.get(&(parent_key, child_key)).cloned()
    }

    // Borrows an unbounded temporary transfer VMO from `self.transfer_vmos`, or creates a new one
    // if none is available in the pool.
    fn acquire_transfer_vmo(&self) -> Result<zx::Vmo, zx::Status> {
        if let Some(vmo) = self.transfer_vmos.lock().pop() {
            return Ok(vmo);
        }
        zx::Vmo::create_with_opts(zx::VmoOptions::UNBOUNDED, 0)
    }

    // Returns a temporary transfer VMO to `self.transfer_vmos` if the pool is not full.
    fn release_transfer_vmo(&self, vmo: zx::Vmo) {
        let mut pool = self.transfer_vmos.lock();
        if pool.len() < MAX_POOLED_TRANSFER_VMOS {
            pool.push(vmo);
        }
    }

    // Writes `valid_bytes` into a pooled unbounded transfer VMO and supplies `child_range` to
    // `child_vmo`. `zx_pager_supply_pages` supplies zeroes for any uncommitted trailing bytes and
    // decommits the supplied range in `transfer_vmo` upon success.
    fn supply_slice_via_transfer_vmo(
        &self,
        child_vmo: &zx::Vmo,
        child_range: Range<u64>,
        valid_bytes: &[u8],
    ) -> Result<(), zx::Status> {
        let bounded_len = (child_range.end - child_range.start) as usize;
        let copy_len = std::cmp::min(valid_bytes.len(), bounded_len);
        let transfer_vmo = self.acquire_transfer_vmo()?;
        if copy_len > 0 {
            transfer_vmo.write(&valid_bytes[..copy_len], 0)?;
        }
        self.pager.supply_pages(child_vmo, child_range, &transfer_vmo, 0)?;
        self.release_transfer_vmo(transfer_vmo);
        Ok(())
    }

    // Validates alignment/bounds of a parent blob chunk at `[offset .. offset + length)` and
    // verifies it against `parent_key`'s Merkle tree at `parent_offset`.
    fn verify_parent_chunk(
        &self,
        parent_key: u64,
        parent_offset: u64,
        offset: u32,
        length: u32,
        payload: Option<PtrByteSlice<'_>>,
    ) -> Result<(), Error> {
        let page_size = zx::system_get_page_size() as u64;
        let page_size_u32 = page_size as u32;
        let Some(parent_slice) = payload else {
            return Err(
                Error::from(zx::Status::INVALID_ARGS).context("ChildData payload out of bounds")
            );
        };
        if offset % page_size_u32 != 0 {
            return Err(Error::from(zx::Status::INVALID_ARGS)
                .context(format!("ChildData payload offset must be page aligned: {offset}")));
        }
        if length % page_size_u32 != 0 {
            return Err(Error::from(zx::Status::INVALID_ARGS)
                .context(format!("ChildData payload length must be page aligned: {length}")));
        }
        if parent_offset % page_size != 0 {
            return Err(Error::from(zx::Status::INVALID_ARGS).context(format!(
                "ChildData parent_offset must be page aligned: {parent_offset}"
            )));
        }

        let cached_blob = self.get_by_key(parent_key).ok_or_else(|| {
            Error::from(zx::Status::BAD_STATE).context(format!(
                "Unknown or expired parent key {parent_key} in deliver_child_pages"
            ))
        })?;
        let verifier = cached_blob.merkle_verifier.get().ok_or_else(|| {
            Error::from(zx::Status::BAD_STATE)
                .context(format!("ChildData received for uninitialized parent blob {parent_key}"))
        })?;
        let remaining_in_parent = cached_blob.len.saturating_sub(parent_offset);
        let unaligned_parent_len = std::cmp::min(length as u64, remaining_in_parent) as usize;
        if let Err(e) =
            verifier.verify_aligned(parent_offset as usize, parent_slice, unaligned_parent_len)
        {
            return Err(Error::from(zx::Status::IO_DATA_INTEGRITY)
                .context(format!("Failed to verify parent payload for blob {parent_key}: {e:?}")));
        }
        Ok(())
    }

    // Reads `data.tail_length` bytes from `data.parent_byte_offset` in the verified parent blob VMO
    // (`data.parent_key`) and supplies a zero-padded page to `data.child_key` at
    // `data.child_target_offset`.
    //
    // DANGER: This function is reentrant! Reading from `cached_blob.vmo` can trigger a kernel page
    // fault on the parent blob if the page has been evicted or not yet supplied. That fault is
    // routed to the block driver and fulfilled by a `DeliveryCommand::Data` message on the same
    // delivery queue. Before calling this function, the caller must pop the `ChildInlineTail`
    // message from the FIFO and ensure another delivery worker thread is available (and woken via
    // `receiver.will_maybe_reenter()`) to service incoming `DeliveryCommand::Data` messages.
    fn process_one_inline_tail(&self, data: delivery::ChildInlineTail) -> Result<(), Error> {
        let Some(child_vmo) = self.get_child_vmo(data.parent_key, data.child_key) else {
            bail!("Unknown child VMO ({}, {})", data.parent_key, data.child_key);
        };
        let page_size = zx::system_get_page_size() as u64;
        if data.child_target_offset % page_size != 0 {
            bail!(
                "ChildInlineTail child_target_offset must be page aligned: {}",
                data.child_target_offset
            );
        }
        if data.tail_length == 0 || (data.tail_length as u64) > page_size {
            bail!("ChildInlineTail invalid tail_length: {}", data.tail_length);
        }
        let child_page_aligned_size = child_vmo.len.next_multiple_of(page_size);
        if data.child_target_offset >= child_page_aligned_size {
            return Ok(());
        }
        let child_range = data.child_target_offset..data.child_target_offset + page_size;

        let res: Result<(), Error> = (|| {
            let cached_blob = self.get_by_key(data.parent_key).ok_or_else(|| {
                Error::from(zx::Status::BAD_STATE).context(format!(
                    "Unknown or expired parent key {} in deliver_child_inline_tail",
                    data.parent_key
                ))
            })?;
            let valid_in_child = child_vmo.len.saturating_sub(data.child_target_offset);
            let copy_len = std::cmp::min(data.tail_length as u64, valid_in_child) as usize;
            let mut page = [0u8; 4096];
            if copy_len > 0 {
                if data.parent_byte_offset.checked_add(copy_len as u64).unwrap_or(u64::MAX)
                    > cached_blob.len
                {
                    return Err(Error::from(zx::Status::IO_DATA_INTEGRITY)
                        .context("ChildInlineTail parent_byte_offset out of parent blob bounds"));
                }
                // DANGER: Reading from the pager-backed parent VMO is reentrant: if the parent page
                // is not resident, this blocks the current delivery worker thread on a page fault
                // that must be resolved by another delivery worker thread processing
                // `DeliveryCommand::Data` on the same delivery queue. Do not hold any locks needed
                // by `deliver_pages` across this read.
                cached_blob
                    .vmo
                    .read(&mut page[..copy_len], data.parent_byte_offset)
                    .context("Failed to read inline tail from parent VMO")?;
            }
            self.supply_slice_via_transfer_vmo(
                &child_vmo.vmo,
                child_range.clone(),
                &page[..copy_len],
            )
            .context("Failed to supply inline tail page")?;
            Ok(())
        })();

        if let Err(err) = res {
            self.report_pager_failure(&child_vmo.vmo, child_range, status_from_error(&err));
            return Err(err);
        }
        Ok(())
    }

    #[cfg(test)]
    fn is_merkle_initialized(&self, key: u64) -> bool {
        let state = self.state.lock();
        match state.blob_states_by_key.get(&key) {
            Some(BlobState::Opening { verifier, .. }) => verifier.is_some(),
            Some(BlobState::Ready(blob)) => blob.merkle_verifier.get().is_some(),
            None => false,
        }
    }

    pub(crate) fn start_recording(&self) -> Result<(), zx::Status> {
        let mut state = self.state.lock();
        if state.recording.is_some() {
            return Err(zx::Status::BAD_STATE);
        }
        let hash_by_key =
            state.keys_by_hash.iter().map(|(&hash, &key)| (key, Hash::from(hash))).collect();
        state.recording = Some(ActiveRecording { opened_hashes: HashSet::new(), hash_by_key });
        Ok(())
    }

    fn record_open(&self, identifier: &[u8; 32]) {
        let mut state = self.state.lock();
        if let Some(recording) = state.recording.as_mut() {
            recording.opened_hashes.insert(Hash::from(*identifier));
        }
    }

    pub(crate) fn take_recording(&self) -> Option<ActiveRecording> {
        self.state.lock().recording.take()
    }

    pub(crate) fn abort_recording(&self) {
        self.state.lock().recording = None;
    }

    /// Looks up the blob in the cache and returns its current [`CacheLookup`] state:
    ///
    /// - [`CacheLookup::Ready`]: The blob is already cached. Returns a new read-only child VMO.
    /// - [`CacheLookup::Pending`]: Another task is actively opening this blob. Returns an
    ///   `EventListener` so the caller can await completion without duplicate work.
    /// - [`CacheLookup::Missing`]: The blob is absent. Atomically reserves the key and slot
    ///   by inserting `BlobState::Opening`, returning a [`PendingEntry`] for the caller to fulfill.
    fn lookup(self: &Arc<Self>, identifier: &[u8; 32]) -> Result<CacheLookup, Error> {
        let mut state = self.state.lock();
        if let Some(&key) = state.keys_by_hash.get(identifier) {
            match state.blob_states_by_key.get(&key) {
                Some(BlobState::Ready(cached)) => {
                    let child = cached
                        .create_child()
                        .map_err(|s| anyhow!("Failed to create child VMO: {s}"))?;
                    return Ok(CacheLookup::Ready(child));
                }
                Some(BlobState::Opening { event, .. }) => {
                    return Ok(CacheLookup::Pending(event.listen()));
                }
                None => unreachable!("keys_by_hash pointed to non-existent key {key}"),
            }
        }

        let event = Arc::new(event_listener::Event::new());
        let key = state.generate_key();
        state.keys_by_hash.insert(*identifier, key);
        state
            .blob_states_by_key
            .insert(key, BlobState::Opening { root_hash: *identifier, event, verifier: None });
        Ok(CacheLookup::Missing(PendingEntry { cache: self.clone(), key }))
    }

    pub(crate) async fn open_vmo(
        self: &Arc<Self>,
        identifier: &[u8; 32],
    ) -> Result<zx::Vmo, Error> {
        loop {
            let entry = match self.lookup(identifier)? {
                CacheLookup::Ready(child) => return Ok(child),
                CacheLookup::Pending(listener) => {
                    listener.await;
                    continue; // Re-evaluate cache now that the blocking event triggered
                }
                CacheLookup::Missing(entry) => entry,
            };

            let key = entry.key();

            // Ask Fxfs to register the blob and write its extent mappings into the shared mapping
            // VMO, using the client-allocated key.
            let size = self
                .mapping_session
                .open(key, identifier)
                .await
                .context("FIDL error calling MappingSession.Open")?
                .map_err(|e| {
                    Error::from(zx::Status::err_from_raw(e))
                        .context("MappingSession.Open failed for blob")
                })?;

            let vmo = self.pager.create_vmo(zx::VmoOptions::empty(), &self.port, key, size)?;
            let vmo_name = format!("blob-{}", &Hash::from(*identifier).to_string()[..8]);
            if let Ok(name) = zx::Name::new(&vmo_name) {
                let _ = vmo.set_name(&name);
            }

            // Create the initial child. We vend children of this VMO to clients so we can track
            // when all children are dropped to evict the blob from cache.
            let first_child = vmo
                .create_child(zx::VmoChildOptions::REFERENCE | zx::VmoChildOptions::NO_WRITE, 0, 0)
                .map_err(|s| anyhow!("Failed to create child VMO: {}", s))?;

            entry.commit(vmo, size);

            return Ok(first_child);
        }
    }

    fn on_zero_children(&self, blob: &Arc<CachedBlob>) {
        // On zero children, evict the blob from the cache and close its Fxfs mapping session.
        // We must hold this lock before checking `num_children` so `lookup` cannot clone
        // a child between this check and the following eviction, which would leave that client
        // holding a VMO whose storage mappings have already been closed.
        let mut state = self.state.lock();
        if let Ok(info) = blob.vmo.info() {
            if info.num_children > 0 {
                // A concurrent client acquired a child VMO before we processed this packet.
                // Resume watching for the next ZERO_CHILDREN signal.
                blob.wait_for_zero_children();
                return;
            }
        }
        // Evict from `keys_by_hash`.
        if let hash_map::Entry::Occupied(entry) = state.keys_by_hash.entry(blob.root_hash) {
            if *entry.get() == blob.vmo_key {
                entry.remove();
            }
        }
        // Evict from `blob_states_by_key`.
        if let hash_map::Entry::Occupied(entry) = state.blob_states_by_key.entry(blob.vmo_key) {
            if let BlobState::Ready(cached) = entry.get() {
                if Arc::ptr_eq(cached, blob) {
                    entry.remove();
                }
            }
        }
    }

    fn create_merkle_verifier(
        root_hash: &[u8; 32],
        vmo_key: u64,
        hashes: Box<[Hash]>,
        read_alignment: usize,
    ) -> Result<ReadSizedMerkleVerifier, Error> {
        // For blobs <= 8192 bytes (a single block), the Merkle tree consists of only a single leaf,
        // which is identical to the root hash itself. Fxfs omits storing leaf hashes on disk for
        // single-block blobs to save space, resulting in empty leaf metadata read by the block
        // server. In that case, synthesise the single leaf from the blob's Merkle root hash.
        let hashes = if hashes.is_empty() { Box::new([Hash::from(*root_hash)]) } else { hashes };

        match MerkleVerifier::new(Hash::from(*root_hash), hashes) {
            Ok(verifier) => ReadSizedMerkleVerifier::new(verifier, read_alignment)
                .map_err(|e| anyhow!("Failed to create ReadSizedMerkleVerifier: {e:?}")),
            Err(e) => {
                bail!("Failed to verify merkle leaves for key {vmo_key}: {e:?}");
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

/// Bundles the context for a child VMO page delivery and reports a pager failure on `Drop`
/// unless [`ChildPagesDelivery::deliver`] succeeds in supplying the pages.
struct ChildPagesDelivery<'a> {
    cache: &'a Cache,
    child_vmo: Arc<ChildVmo>,
    child_range: Range<u64>,
    data: delivery::ChildData<'a>,
    status: Option<zx::Status>,
}

impl<'a> ChildPagesDelivery<'a> {
    // Validates target alignment and bounds `data` to the child VMO's page-aligned size. Returns
    // `Ok(None)` if `child_target_offset` is already at or beyond the end of the child VMO.
    fn new(cache: &'a Cache, data: delivery::ChildData<'a>) -> Result<Option<Self>, Error> {
        let child_vmo = cache.get_child_vmo(data.parent_key, data.child_key).ok_or_else(|| {
            anyhow!("Unknown child VMO ({}, {})", data.parent_key, data.child_key)
        })?;

        let page_size = zx::system_get_page_size() as u64;
        if data.child_target_offset % page_size != 0 {
            bail!(
                "ChildData child_target_offset must be page aligned: {}",
                data.child_target_offset
            );
        }
        if data.child_length == 0 || (data.child_length as u64) % page_size != 0 {
            bail!(
                "ChildData child_length must be non-zero and page aligned: {}",
                data.child_length
            );
        }

        let child_page_aligned_size = child_vmo.len.next_multiple_of(page_size);
        let bounded_child_len = std::cmp::min(
            data.child_length as u64,
            child_page_aligned_size.saturating_sub(data.child_target_offset),
        );
        if bounded_child_len == 0 {
            return Ok(None);
        }
        let child_range = data.child_target_offset..data.child_target_offset + bounded_child_len;
        Ok(Some(Self { cache, child_vmo, child_range, data, status: Some(zx::Status::BAD_STATE) }))
    }

    // Supplies `self.child_range` in `self.child_vmo` from `source_vmo` at `source_offset` and
    // clears `self.status` so `Drop` does not report a pager failure.
    fn supply_pages(&mut self, source_vmo: &zx::Vmo, source_offset: u64) -> Result<(), Error> {
        match self.cache.pager.supply_pages(
            &self.child_vmo.vmo,
            self.child_range.clone(),
            source_vmo,
            source_offset,
        ) {
            Ok(()) => {
                self.status = None;
                Ok(())
            }
            Err(status) => {
                self.status = Some(status);
                bail!("Failed to supply child pages: {status:?}");
            }
        }
    }

    // Verifies the parent chunk, zeroes any trailing bytes beyond `self.child_vmo.len`, and
    // supplies the pages to `self.child_vmo`.
    fn deliver(mut self) -> Result<(), Error> {
        let page_size = zx::system_get_page_size() as u64;
        let page_size_u32 = page_size as u32;
        let bounded_child_len = self.child_range.end - self.child_range.start;

        if self.data.length == 0 {
            // A zero-length parent payload indicates a sparse extent in the child file.
            let transfer_vmo = self.cache.acquire_transfer_vmo()?;
            self.supply_pages(&transfer_vmo, 0)?;
            self.cache.release_transfer_vmo(transfer_vmo);
            return Ok(());
        }

        if let Err(err) = self.cache.verify_parent_chunk(
            self.data.parent_key,
            self.data.parent_offset,
            self.data.offset,
            self.data.length,
            self.data.payload.as_ref().map(|p| p.as_ptr_slice()),
        ) {
            self.status = Some(status_from_error(&err));
            return Err(err);
        }

        if self.data.in_chunk_offset % page_size_u32 != 0 {
            bail!("ChildData in_chunk_offset must be page aligned: {}", self.data.in_chunk_offset);
        }
        if self.data.in_chunk_offset.checked_add(bounded_child_len as u32).unwrap_or(u32::MAX)
            > self.data.length
        {
            bail!("Child slice out of parent chunk bounds");
        }
        let source_offset = self.data.offset + self.data.in_chunk_offset;

        // Zero any bytes beyond the child file's logical size (`child_vmo.len`).
        if self.child_range.end > self.child_vmo.len {
            let valid_in_slice =
                self.child_vmo.len.saturating_sub(self.data.child_target_offset) as usize;
            let bounded_usize = bounded_child_len as usize;
            if valid_in_slice < bounded_usize {
                let in_chunk = self.data.in_chunk_offset as usize;
                self.data
                    .payload
                    .as_mut()
                    .unwrap()
                    .subslice_mut((in_chunk + valid_in_slice)..(in_chunk + bounded_usize))
                    .fill(0);
            }
        }

        let vmo_offset = self.data.payload_region_offset + source_offset as u64;
        self.supply_pages(&self.cache.delivery_vmo, vmo_offset)
    }
}

impl Drop for ChildPagesDelivery<'_> {
    fn drop(&mut self) {
        if let Some(status) = self.status.take() {
            self.cache.report_pager_failure(&self.child_vmo.vmo, self.child_range.clone(), status);
        }
    }
}

impl delivery::DeliveryQueueProvider for Cache {
    fn deliver_pages(
        &self,
        key: u64,
        target_offset: u64,
        unverified_pages: UnverifiedPages<'_>,
    ) -> Result<(), Error> {
        let cached_blob = self
            .get_by_key(key)
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

    fn register_blob(
        &self,
        key: u64,
        leaf_data: PtrByteSlice<'_>,
        read_alignment: usize,
    ) -> Result<(), Error> {
        if leaf_data.len() % HASH_SIZE != 0 {
            bail!("RegisterBlob invalid leaf length must be a multiple of HASH_SIZE");
        }

        let hashes: Vec<Hash> = (0..(leaf_data.len() / HASH_SIZE))
            .map(|i| {
                let chunk = leaf_data.subslice(i * HASH_SIZE..(i + 1) * HASH_SIZE);
                let mut hash_bytes = [0u8; HASH_SIZE];
                chunk.copy_to_slice(&mut hash_bytes);
                Hash::from(hash_bytes)
            })
            .collect();

        let mut state = self.state.lock();
        match state.blob_states_by_key.get_mut(&key) {
            Some(BlobState::Opening { root_hash, verifier, .. }) => {
                if verifier.is_some() {
                    log::warn!(key;
                        "RegisterBlob received for blob but it is already initialized"
                    );
                    return Ok(());
                }
                *verifier = Some(Self::create_merkle_verifier(
                    root_hash,
                    key,
                    hashes.into_boxed_slice(),
                    read_alignment,
                )?);
                Ok(())
            }
            Some(BlobState::Ready(cached)) => {
                if cached.merkle_verifier.get().is_some() {
                    log::warn!(key;
                        "RegisterBlob received for blob but it is already initialized"
                    );
                    return Ok(());
                }
                let verifier = Self::create_merkle_verifier(
                    &cached.root_hash,
                    key,
                    hashes.into_boxed_slice(),
                    read_alignment,
                )?;
                let _ = cached.merkle_verifier.set(verifier);
                Ok(())
            }
            None => {
                bail!("RegisterBlob received for unknown or expired key {key}");
            }
        }
    }

    // Verifies the incoming parent chunk and supplies the requested slice to the child VMO.
    fn deliver_child_pages(&self, data: delivery::ChildData<'_>) -> Result<(), Error> {
        if let Some(delivery) = ChildPagesDelivery::new(self, data)? {
            delivery.deliver()?;
        }
        Ok(())
    }

    // Supplies an inline-tail page to the child VMO from the parent blob VMO, limiting inline-tail
    // processing to at most one delivery worker thread at a time so the remaining workers stay free
    // to service incoming parent `Data` commands if reading the parent VMO faults.
    fn deliver_child_inline_tail(
        &self,
        data: delivery::ChildInlineTail,
        receiver: &vmo_fifo::Receiver<mapping::RawDeliveryCommand>,
    ) -> Result<(), Error> {
        {
            let mut queue = self.inline_tail_queue.lock();
            if queue.busy {
                queue.pending.push_back(data);
                return Ok(());
            }
            queue.busy = true;
        }

        // `process_one_inline_tail` is reentrant and may block on a parent VMO page fault that
        // requires another delivery worker thread to process `DeliveryCommand::Data` from
        // `receiver`.
        receiver.will_maybe_reenter();

        let mut current = data;
        loop {
            if let Err(error) = self.process_one_inline_tail(current) {
                log::warn!(error:?; "Failed to process ChildInlineTail");
            }
            let mut queue = self.inline_tail_queue.lock();
            if let Some(next) = queue.pending.pop_front() {
                current = next;
            } else {
                queue.busy = false;
                break;
            }
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
    mapper_session: fblock::MapperSessionProxy,
    cache: Arc<Cache>,
    scope: fasync::Scope,
}

impl BlobPagerAndVerifier {
    /// Returns a reference to the shared delivery queue VMO.
    pub fn delivery_vmo(&self) -> &zx::Vmo {
        self.cache.delivery_vmo()
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
        let scope_handle = scope.to_handle();
        let cache = Arc::new(Cache::new(
            mapping_session,
            pager,
            port,
            delivery_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS)?,
            scope_handle,
        ));
        let _delivery_processor =
            delivery::DeliveryQueueProcessor::spawn(receiver, cache.clone(), delivery_vmo)?;

        Ok(Self { _delivery_processor, mapper_session, cache, scope })
    }

    /// Starts recording a profile for `name` and replaying any existing profile of the same name.
    pub async fn start_profile(
        &self,
        name: impl Into<String>,
    ) -> Result<ProfileRecording, zx::Status> {
        ProfileRecording::start(self.cache.clone(), &self.mapper_session, name.into()).await
    }

    /// Starts recording and replaying the named profile, stopping and committing the profile in the
    /// background after `duration` elapses.
    pub async fn record_and_replay_profile(
        &self,
        name: impl Into<String>,
        duration: zx::MonotonicDuration,
    ) {
        let name = name.into();
        let recording = match self.start_profile(name.clone()).await {
            Ok(recording) => recording,
            Err(status) => {
                log::warn!(status:?, name = name.as_str(); "Failed to start profile recording");
                return;
            }
        };
        self.scope.spawn(async move {
            fasync::Timer::new(duration).await;
            if let Err(status) = recording.stop().await {
                log::warn!(status:?, name = name.as_str(); "Failed to stop profile recording");
            }
        });
    }

    /// Create pager owned VMO for the blob identified by its Merkle Root Hash.
    pub async fn create_vmo(&self, identifier: &[u8; 32]) -> Result<zx::Vmo, Error> {
        let vmo = self.cache.open_vmo(identifier).await?;
        self.cache.record_open(identifier);
        Ok(vmo)
    }

    /// Opens a child `MapperSession` backed by the blob identified by its Merkle root hash.
    ///
    /// Child VMOs created on `session` are backed by `self.pager` and a dedicated `child_port`
    /// passed to the driver. When a child VMO faults, the driver reads the corresponding parent
    /// blob chunks and sends `DeliveryCommand::ChildData` over the shared delivery queue so
    /// `BlobPagerAndVerifier` can verify the parent data before supplying pages to the child VMO.
    pub async fn open_child_session(
        &self,
        identifier: &[u8; 32],
        mapping_vmo: zx::Vmo,
        session: fidl::endpoints::ServerEnd<fblock::MapperSessionMarker>,
    ) -> Result<(), zx::Status> {
        let parent_key = {
            let state = self.cache.state.lock();
            state.keys_by_hash.get(identifier).copied()
        }
        .ok_or(zx::Status::NOT_FOUND)?;

        let child_port = zx::Port::create();
        let child_port_for_driver = child_port.duplicate_handle(zx::Rights::SAME_RIGHTS)?;
        let (driver_session_proxy, driver_session_server) =
            fidl::endpoints::create_proxy::<fblock::MapperSessionMarker>();

        self.mapper_session
            .open_child_session(
                driver_session_server,
                mapping_vmo,
                parent_key,
                Some(child_port_for_driver),
                None,
            )
            .await
            .map_err(|_| zx::Status::PEER_CLOSED)?
            .map_err(zx::Status::err_from_raw)?;

        let pager = self.cache.pager.clone();
        let cache = self.cache.clone();
        self.scope.spawn(async move {
            let mut stream = session.into_stream();
            let mut child_keys = HashSet::new();
            while let Ok(Some(request)) = stream.try_next().await {
                match request {
                    fblock::MapperSessionRequest::CreateVmo {
                        key,
                        size,
                        options: _,
                        responder,
                    } => {
                        let res = pager
                            .create_vmo(zx::VmoOptions::empty(), &child_port, key, size)
                            .and_then(|vmo| {
                                let dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS)?;
                                cache.register_child_vmo(parent_key, key, dup, size);
                                child_keys.insert(key);
                                Ok(vmo)
                            });
                        let _ = responder.send(res.map_err(zx::Status::into_raw));
                    }
                    fblock::MapperSessionRequest::CloseVmo { key, .. } => {
                        if child_keys.remove(&key) {
                            cache.unregister_child_vmo(parent_key, key);
                        }
                    }
                    fblock::MapperSessionRequest::OpenChildSession { responder, .. } => {
                        let _ = responder.send(Err(zx::Status::NOT_SUPPORTED.into_raw()));
                    }
                    fblock::MapperSessionRequest::RecordProfile { responder, .. } => {
                        let _ = responder.send(Err(zx::Status::NOT_SUPPORTED.into_raw()));
                    }
                    fblock::MapperSessionRequest::Close { responder } => {
                        let _ = driver_session_proxy.close().await;
                        let _ = responder.send(Ok(()));
                        break;
                    }
                    fblock::MapperSessionRequest::_UnknownMethod { .. } => {}
                }
            }
            for child_key in child_keys {
                cache.unregister_child_vmo(parent_key, child_key);
            }
        });

        Ok(())
    }

    /// Returns a VFS [`vfs::service::Service`] that serves `fuchsia.storage.mapping/FileMapper`
    /// by resolving [`vmo_blob::VmoBlob`] tokens in `scope` and opening child mapper sessions on
    /// `self`.
    pub fn get_file_mapper_handler(
        self: &Arc<Self>,
        scope: vfs::execution_scope::ExecutionScope,
    ) -> Arc<vfs::service::Service> {
        let verifier = self.clone();
        vfs::service::host(move |mut stream: fmapping::FileMapperRequestStream| {
            use futures::TryStreamExt as _;
            let verifier = verifier.clone();
            let scope = scope.clone();
            async move {
                while let Some(request) = stream.try_next().await.unwrap_or_else(|error| {
                    log::error!(error:?; "error reading FileMapper request");
                    None
                }) {
                    match request {
                        fmapping::FileMapperRequest::OpenSession {
                            file_token,
                            mapping_vmo,
                            session,
                            responder,
                        } => {
                            let res = async {
                                let (node, rights) = scope
                                    .token_registry()
                                    .get_owner_and_rights(file_token.into())?
                                    .ok_or(zx::Status::NOT_FOUND)?;
                                if !rights.contains(fidl_fuchsia_io::Rights::READ_BYTES) {
                                    return Err(zx::Status::ACCESS_DENIED);
                                }
                                let vmo_blob = node
                                    .into_any()
                                    .downcast::<vmo_blob::VmoBlob>()
                                    .map_err(|_| zx::Status::INVALID_ARGS)?;
                                verifier
                                    .open_child_session(vmo_blob.root_hash(), mapping_vmo, session)
                                    .await
                            }
                            .await;
                            let _ = responder.send(res.map_err(zx::Status::into_raw));
                        }
                        fmapping::FileMapperRequest::_UnknownMethod { .. } => {}
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::TryStreamExt;
    use futures::channel::oneshot;
    use mapping::{DeliveryCommand, RawDeliveryCommand};
    use test_case::test_case;
    use vmo_fifo::SyncSender;

    const TARGET_READ_AHEAD_SIZE: usize = mapping::TARGET_READ_AHEAD_SIZE as usize;
    const TEST_BLOB_SIZE: u64 = (TARGET_READ_AHEAD_SIZE * 2) as u64;
    const TEST_VMO_KEY: u64 = 1;

    // Used for testing BlobPagerAndVerifier interactions.
    // We arbitrarily allocate a 32KiB buffer so the payload spans multiple VMO pages.
    //
    // This test environment assumes a single blob file, identified by the `valid_root` hash.
    struct TestEnv {
        pager_and_verifier: Arc<BlobPagerAndVerifier>,
        mapping_task: fasync::Task<()>,
        mapper_task: fasync::Task<()>,
        close_signal: Option<oneshot::Receiver<()>>,
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

            let (close_tx, close_rx) = oneshot::channel();
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
                            fmapping::MappingSessionRequest::Open {
                                key,
                                identifier,
                                responder,
                            } => {
                                assert_eq!(identifier, expected_hash);
                                assert_eq!(key, TEST_VMO_KEY);
                                open_calls += 1;
                                assert_eq!(
                                    open_calls, 1,
                                    "Open should only be called once per identifier"
                                );
                                responder.send(Ok(blob_size)).expect("send failed");
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

            let (tx_delivery, rx_delivery) = oneshot::channel();
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
        started_rx: Option<oneshot::Receiver<()>>,
        done_rx: oneshot::Receiver<()>,
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
        let (started_tx, started_rx) = oneshot::channel();
        let (done_tx, done_rx) = oneshot::channel();

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
            let state = env.pager_and_verifier.cache.state.lock();
            assert_eq!(state.keys_by_hash.get(&env.valid_root), Some(&TEST_VMO_KEY));
            assert!(matches!(
                state.blob_states_by_key.get(&TEST_VMO_KEY),
                Some(BlobState::Ready(_))
            ));
        }

        drop(child_vmo);

        // wait for the packet receiver to observe the ZERO_CHILDREN signal and dispatch
        // `mapping_session.close()` to the mapping_server.
        env.close_signal.take().expect("Missing signal").await.expect("Failed to close");

        {
            let state = env.pager_and_verifier.cache.state.lock();
            assert!(state.keys_by_hash.get(&env.valid_root).is_none());
            assert!(state.blob_states_by_key.get(&TEST_VMO_KEY).is_none());
        }

        env.teardown().await;
    }

    #[fuchsia::test]
    async fn test_create_vmo_multiple_distinct_blobs() {
        use futures::StreamExt;

        let (mapping_proxy, mut mapping_stream) =
            fidl::endpoints::create_proxy_and_stream::<fmapping::MappingProviderMarker>();
        let (mapper_proxy, mut mapper_stream) =
            fidl::endpoints::create_proxy_and_stream::<fblock::MapperMarker>();

        let (close_tx, mut close_rx) = futures::channel::mpsc::unbounded();
        let mapping_task = fasync::Task::spawn(async move {
            if let Some(fmapping::MappingProviderRequest::OpenSession { session, responder }) =
                mapping_stream.try_next().await.expect("try_next failed")
            {
                let mapping_vmo = zx::Vmo::create(zx::system_get_page_size().into())
                    .expect("zx::Vmo::create failed");
                responder.send(Ok(mapping_vmo)).expect("send failed");

                let mut session_stream = session.into_stream();
                while let Some(request) = session_stream.try_next().await.expect("try_next failed")
                {
                    match request {
                        fmapping::MappingSessionRequest::Open { responder, .. } => {
                            responder.send(Ok(8192)).expect("send failed");
                        }
                        fmapping::MappingSessionRequest::Close { key, responder } => {
                            responder.send(Ok(())).expect("send failed");
                            let _ = close_tx.unbounded_send(key);
                        }
                        _ => {}
                    }
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

        let pager_and_verifier = Arc::new(
            BlobPagerAndVerifier::new(&mapping_proxy, &mapper_proxy)
                .await
                .expect("BlobPagerAndVerifier::new failed"),
        );

        let hash1: [u8; 32] = [0x11; 32];
        let hash2: [u8; 32] = [0x22; 32];
        let hash3: [u8; 32] = [0x33; 32];

        let vmo1 = pager_and_verifier.create_vmo(&hash1).await.expect("create_vmo 1 failed");
        assert_eq!(
            vmo1.get_name().expect("get_name failed"),
            zx::Name::new("blob-11111111").expect("zx::Name::new failed")
        );
        let vmo2 = pager_and_verifier.create_vmo(&hash2).await.expect("create_vmo 2 failed");
        let vmo3 = pager_and_verifier.create_vmo(&hash3).await.expect("create_vmo 3 failed");

        let (key1, key2, key3) = {
            let state = pager_and_verifier.cache.state.lock();
            let k1 = *state.keys_by_hash.get(&hash1).expect("hash1 missing");
            let k2 = *state.keys_by_hash.get(&hash2).expect("hash2 missing");
            let k3 = *state.keys_by_hash.get(&hash3).expect("hash3 missing");
            assert_ne!(k1, k2);
            assert_ne!(k2, k3);
            assert_ne!(k1, k3);
            assert!(matches!(state.blob_states_by_key.get(&k1), Some(BlobState::Ready(_))));
            assert!(matches!(state.blob_states_by_key.get(&k2), Some(BlobState::Ready(_))));
            assert!(matches!(state.blob_states_by_key.get(&k3), Some(BlobState::Ready(_))));
            (k1, k2, k3)
        };

        // Evict blob 2 by dropping its only child VMO
        drop(vmo2);
        let closed_key = close_rx.next().await.expect("expected close signal");
        assert_eq!(closed_key, key2);

        // Verify blob 2 was evicted while blob 1 and 3 remain cached
        {
            let state = pager_and_verifier.cache.state.lock();
            assert!(state.keys_by_hash.get(&hash2).is_none());
            assert!(state.blob_states_by_key.get(&key2).is_none());
            assert_eq!(state.keys_by_hash.get(&hash1), Some(&key1));
            assert_eq!(state.keys_by_hash.get(&hash3), Some(&key3));
            assert!(matches!(state.blob_states_by_key.get(&key1), Some(BlobState::Ready(_))));
            assert!(matches!(state.blob_states_by_key.get(&key3), Some(BlobState::Ready(_))));
        }

        // Re-open blob 2: it should generate a new key and open successfully
        let vmo2_new = pager_and_verifier.create_vmo(&hash2).await.expect("re-create vmo 2 failed");
        {
            let state = pager_and_verifier.cache.state.lock();
            let k = *state.keys_by_hash.get(&hash2).expect("hash2 missing after re-open");
            assert_ne!(k, key2);
            assert!(matches!(state.blob_states_by_key.get(&k), Some(BlobState::Ready(_))));
        }

        drop(vmo1);
        drop(vmo3);
        drop(vmo2_new);

        drop(pager_and_verifier);
        mapping_task.await;
        mapper_task.await;
    }

    #[fuchsia::test]
    async fn test_cache_is_cleared_when_create_vmo_fails() {
        let (mapping_proxy, mut mapping_stream) =
            fidl::endpoints::create_proxy_and_stream::<fmapping::MappingProviderMarker>();
        let (mapper_proxy, mut mapper_stream) =
            fidl::endpoints::create_proxy_and_stream::<fblock::MapperMarker>();

        let (mock_opened_tx, mock_opened_rx) = oneshot::channel();
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
            let state = pager_and_verifer.cache.state.lock();
            assert_eq!(state.keys_by_hash.get(identifier), Some(&1));
            assert!(matches!(state.blob_states_by_key.get(&1), Some(BlobState::Opening { .. })));
        }

        // Simulate a failed create_vmo (future abort drops execution context)
        abort_handle.abort();
        let _ = primary_task.await;

        // The cache should be cleared after due to PendingEntry being dropped
        {
            let state = pager_and_verifer.cache.state.lock();
            assert!(state.keys_by_hash.get(identifier).is_none());
            assert!(state.blob_states_by_key.get(&1).is_none());
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

        let (open_received_tx, open_received_rx) = oneshot::channel();
        let (reply_tx, reply_rx) = oneshot::channel();

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
            let state = pager_and_verifier.cache.state.lock();
            assert!(matches!(
                state.keys_by_hash.get(&hash).and_then(|k| state.blob_states_by_key.get(k)),
                Some(BlobState::Opening { .. })
            ));
        }

        // A second caller arrives while the primary creation is still pending.
        let verifier2 = pager_and_verifier.clone();
        let secondary_future =
            fasync::Task::spawn(async move { verifier2.create_vmo(&hash).await });

        // Allow Fxfs to reply with an error to the primary caller.
        reply_tx.send(()).expect("reply_tx send failed");

        // Both callers must return an error and complete without hanging on abandoned state, and
        // the primary caller must preserve the underlying zx::Status::NOT_FOUND.
        let primary_err = primary_future.await.expect_err("expected primary create_vmo to fail");
        assert_eq!(primary_err.downcast_ref::<zx::Status>(), Some(&zx::Status::NOT_FOUND));
        assert!(secondary_future.await.is_err());

        // Cache should be empty.
        {
            let state = pager_and_verifier.cache.state.lock();
            assert!(state.keys_by_hash.get(&hash).is_none());
            assert!(state.blob_states_by_key.is_empty());
        }

        drop(pager_and_verifier);
        drop(mapping_task);
        drop(mapper_task);
    }

    #[fuchsia::test]
    async fn test_register_blob_arrives_before_open_completes() {
        // Test that if `RegisterBlob` arrives while `session.open().await` is still pending
        // (the arrival race), the blob's Merkle tree is retained and page reads succeed once
        // `create_vmo` completes.
        let (mapping_proxy, mut mapping_stream) =
            fidl::endpoints::create_proxy_and_stream::<fmapping::MappingProviderMarker>();
        let (mapper_proxy, mut mapper_stream) =
            fidl::endpoints::create_proxy_and_stream::<fblock::MapperMarker>();

        let blob_data = vec![0x42u8; TEST_BLOB_SIZE as usize];
        let (root, leaf_hashes) =
            fuchsia_merkle::MerkleRootBuilder::new(Vec::new()).complete(&blob_data);
        let expected_hash: [u8; 32] = root.into();

        let mut flat_leaves = Vec::new();
        for hash in &leaf_hashes {
            flat_leaves.extend_from_slice(hash.as_bytes());
        }

        let (tx_delivery, rx_delivery) = oneshot::channel();
        let (open_key_tx, open_key_rx) = oneshot::channel();
        let (resume_open_tx, resume_open_rx) = oneshot::channel();
        let (close_tx, close_rx) = oneshot::channel();

        let mapping_task = fasync::Task::spawn(async move {
            let mut close_tx = Some(close_tx);
            let mut open_key_tx = Some(open_key_tx);
            let mut resume_open_rx = Some(resume_open_rx);
            if let Some(fmapping::MappingProviderRequest::OpenSession { session, responder }) =
                mapping_stream.try_next().await.expect("try_next failed")
            {
                let mapping_vmo = zx::Vmo::create(zx::system_get_page_size().into())
                    .expect("zx::Vmo::create failed");
                responder.send(Ok(mapping_vmo)).expect("send failed");

                let mut session_stream = session.into_stream();
                while let Some(request) = session_stream.try_next().await.expect("try_next failed")
                {
                    match request {
                        fmapping::MappingSessionRequest::Open { key, identifier, responder } => {
                            assert_eq!(identifier, expected_hash);
                            // Send key to test thread so it can deliver leaves before Open returns
                            if let Some(tx) = open_key_tx.take() {
                                tx.send(key).expect("open_key_tx failed");
                            }
                            if let Some(rx) = resume_open_rx.take() {
                                rx.await.expect("resume_open_rx failed");
                            }
                            responder.send(Ok(TEST_BLOB_SIZE)).expect("send failed");
                        }
                        fmapping::MappingSessionRequest::Close { key: _, responder } => {
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

        let mapper_task = fasync::Task::spawn(async move {
            if let Some(fblock::MapperRequest::OpenSession { delivery_queue, responder, .. }) =
                mapper_stream.try_next().await.expect("try_next failed")
            {
                tx_delivery.send(delivery_queue).expect("send failed");
                responder.send(Ok(())).expect("send failed");
            }
        });

        let pager_and_verifier = Arc::new(
            BlobPagerAndVerifier::new(&mapping_proxy, &mapper_proxy)
                .await
                .expect("BlobPagerAndVerifier::new failed"),
        );

        let delivery_vmo =
            rx_delivery.await.expect("rx_delivery wait failed").expect("delivery_vmo was None");
        let mut sender = SyncSender::<RawDeliveryCommand>::new(
            delivery_vmo
                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                .expect("duplicate_handle failed"),
            zx::system_get_page_size() as usize,
            mapping::PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .expect("SyncSender::new failed");

        let verifier_clone = pager_and_verifier.clone();
        let create_task =
            fasync::Task::spawn(async move { verifier_clone.create_vmo(&expected_hash).await });

        // Wait until `open()` is called and get the client-allocated key
        let key = open_key_rx.await.expect("open_key_rx failed");

        // While `open()` is still pending, send `RegisterBlob`
        let mut payload =
            sender.reserve_payload(flat_leaves.len()).expect("reserve_payload failed");
        payload.data().copy_from_slice(&flat_leaves);
        let raw_cmd: RawDeliveryCommand = DeliveryCommand::RegisterBlob {
            key: key as u64,
            offset: payload.offset(),
            length: flat_leaves.len() as u32,
            read_alignment: TARGET_READ_AHEAD_SIZE as u32,
        }
        .into();
        payload.commit(raw_cmd).expect("commit failed");

        // Wait until delivery thread processes `RegisterBlob` on the pending entry
        while !pager_and_verifier.cache.is_merkle_initialized(key) {
            fasync::Timer::new(std::time::Duration::from_millis(5)).await;
        }

        // Resume Open and wait for `create_vmo` to finish
        resume_open_tx.send(()).expect("resume_open_tx failed");
        let vmo = create_task.await.expect("create_vmo failed");

        // We should be able to deliver data and read it immediately
        let chunk = &blob_data[..TARGET_READ_AHEAD_SIZE];
        let mut data_payload = sender.reserve_payload(chunk.len()).expect("reserve_payload failed");
        data_payload.data().copy_from_slice(chunk);
        let data_cmd: RawDeliveryCommand = DeliveryCommand::Data {
            key: key as u64,
            offset: data_payload.offset(),
            length: chunk.len() as u32,
            target_offset: 0,
        }
        .into();
        data_payload.commit(data_cmd).expect("commit failed");

        let mut read_buf = vec![0u8; TARGET_READ_AHEAD_SIZE];
        vmo.read(&mut read_buf, 0).expect("read failed");
        assert_eq!(read_buf, chunk);

        drop(vmo);
        close_rx.await.expect("close_rx failed");

        drop(pager_and_verifier);
        mapping_task.await;
        mapper_task.await;
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
            key: TEST_VMO_KEY,
            offset: bad_payload.offset(),
            length: corrupted_leaves.len() as u32,
            read_alignment: TARGET_READ_AHEAD_SIZE as u32,
        }
        .into();
        bad_payload.commit(bad_raw_cmd).expect("commit failed");

        // Yield to allow background thread to process the invalid merkle
        fasync::Timer::new(std::time::Duration::from_millis(5)).await;

        let blob =
            env.pager_and_verifier.cache.get_by_key(TEST_VMO_KEY).expect("get_by_key failed");

        // merkle_verifier should remain as None as the leaves are corrupted
        assert!(blob.merkle_verifier.get().is_none());

        // Test with valid leaves
        let mut payload =
            sender.reserve_payload(env.valid_leaves.len()).expect("reserve_payload failed");
        payload.data().copy_from_slice(&env.valid_leaves);

        let raw_cmd: RawDeliveryCommand = DeliveryCommand::RegisterBlob {
            key: TEST_VMO_KEY,
            offset: payload.offset(),
            length: env.valid_leaves.len() as u32,
            read_alignment: TARGET_READ_AHEAD_SIZE as u32,
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

        let blob =
            env.pager_and_verifier.cache.get_by_key(TEST_VMO_KEY).expect("get_by_key failed");

        // Test with unknown/expired key
        let mut invalid_key_payload =
            sender.reserve_payload(env.valid_leaves.len()).expect("reserve_payload failed");
        invalid_key_payload.data().copy_from_slice(&env.valid_leaves);
        let invalid_key_cmd: RawDeliveryCommand = DeliveryCommand::RegisterBlob {
            key: 9999 as u64,
            offset: invalid_key_payload.offset(),
            length: env.valid_leaves.len() as u32,
            read_alignment: TARGET_READ_AHEAD_SIZE as u32,
        }
        .into();
        invalid_key_payload.commit(invalid_key_cmd).expect("commit failed");
        fasync::Timer::new(std::time::Duration::from_millis(5)).await;
        assert!(blob.merkle_verifier.get().is_none());

        // Test out-of-bounds offset/length
        let oob_payload = sender.reserve_payload(8).expect("reserve_payload failed");
        let oob_cmd: RawDeliveryCommand = DeliveryCommand::RegisterBlob {
            key: TEST_VMO_KEY,
            offset: u32::MAX - 4, // Malicious offset
            length: 8,
            read_alignment: TARGET_READ_AHEAD_SIZE as u32,
        }
        .into();
        oob_payload.commit(oob_cmd).expect("commit failed");
        fasync::Timer::new(std::time::Duration::from_millis(5)).await;
        assert!(blob.merkle_verifier.get().is_none());

        // Test invalid leaf length (not a multiple of HASH_SIZE)
        let invalid_len_payload = sender.reserve_payload(10).expect("reserve_payload failed");
        let invalid_len_cmd: RawDeliveryCommand = DeliveryCommand::RegisterBlob {
            key: TEST_VMO_KEY,
            offset: invalid_len_payload.offset(),
            length: 10,
            read_alignment: TARGET_READ_AHEAD_SIZE as u32,
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
            key: TEST_VMO_KEY,
            offset: payload.offset(),
            length: env.valid_leaves.len() as u32,
            read_alignment: TARGET_READ_AHEAD_SIZE as u32,
        }
        .into();
        payload.commit(raw_cmd).expect("commit failed");

        let blob =
            env.pager_and_verifier.cache.get_by_key(TEST_VMO_KEY).expect("get_by_key failed");
        while blob.merkle_verifier.get().is_none() {
            fasync::Timer::new(std::time::Duration::from_millis(5)).await;
        }

        let test_payload = &env.blob_data[..TARGET_READ_AHEAD_SIZE];

        let mut payload =
            sender.reserve_payload(test_payload.len()).expect("reserve_payload failed");
        payload.data().copy_from_slice(&test_payload);

        let cmd: RawDeliveryCommand = DeliveryCommand::Data {
            key: TEST_VMO_KEY,
            target_offset: 0,
            length: test_payload.len() as u32,
            offset: payload.offset(),
        }
        .into();
        payload.commit(cmd).expect("commit failed");

        let mut read_buf = vec![0u8; TARGET_READ_AHEAD_SIZE];
        paged_vmo.read(&mut read_buf, 0).expect("read paged_vmo failed");
        assert_eq!(read_buf, test_payload);

        drop(paged_vmo);
        env.teardown().await;
    }

    #[fuchsia::test]
    async fn test_delivery_data_corrupted() {
        // Verify that when a delivered chunk fails verification, client threads waiting
        // for data within that chunk receive `ZX_ERR_IO_DATA_INTEGRITY`.
        let blob_size = TARGET_READ_AHEAD_SIZE as u64;
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
            key: TEST_VMO_KEY,
            offset: payload.offset(),
            length: env.valid_leaves.len() as u32,
            read_alignment: TARGET_READ_AHEAD_SIZE as u32,
        }
        .into();
        payload.commit(raw_cmd).expect("commit failed");

        let blob =
            env.pager_and_verifier.cache.get_by_key(TEST_VMO_KEY).expect("get_by_key failed");
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
            key: TEST_VMO_KEY,
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
        let read_alignment = TARGET_READ_AHEAD_SIZE;
        let blob_size = (read_alignment * 2) as u64;
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
            key: TEST_VMO_KEY,
            offset: payload.offset(),
            length: env.valid_leaves.len() as u32,
            read_alignment: read_alignment as u32,
        }
        .into();
        payload.commit(raw_cmd).expect("commit failed");

        let blob =
            env.pager_and_verifier.cache.get_by_key(TEST_VMO_KEY).expect("get_by_key failed");
        while blob.merkle_verifier.get().is_none() {
            fasync::Timer::new(std::time::Duration::from_millis(5)).await;
        }

        let page_size = zx::system_get_page_size() as usize;

        // Chunk 1 test for successful page request.
        // Start a reader on page 0 and deliver the valid first chunk (0..read_alignment).
        let paged_vmo_clone1 =
            paged_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("duplicate_handle failed");
        let (tx1, rx1) = oneshot::channel();
        let expected_page0 = env.blob_data[..page_size].to_vec();
        let thread1 = std::thread::spawn(move || {
            let mut buf = vec![0u8; page_size];
            paged_vmo_clone1.read(&mut buf, 0).expect("read should succeed");
            assert_eq!(buf, expected_page0);
            let _ = tx1.send(());
        });
        fasync::Timer::new(std::time::Duration::from_millis(5)).await;

        let valid_chunk = &env.blob_data[..read_alignment];
        let mut payload =
            sender.reserve_payload(valid_chunk.len()).expect("reserve_payload failed");
        payload.data().copy_from_slice(valid_chunk);
        let raw_cmd: RawDeliveryCommand = DeliveryCommand::Data {
            key: TEST_VMO_KEY,
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
            read_alignment as u64,
            page_size,
            zx::Status::IO_DATA_INTEGRITY,
        );
        reader2.wait_started().await;
        fasync::Timer::new(std::time::Duration::from_millis(5)).await;

        // Deliver corrupted second chunk (read_alignment..read_alignment * 2).
        let mut corrupted_chunk = env.blob_data[read_alignment..].to_vec();
        corrupted_chunk[0] ^= 0xFF;

        let mut payload =
            sender.reserve_payload(corrupted_chunk.len()).expect("reserve_payload failed");
        payload.data().copy_from_slice(&corrupted_chunk);
        let raw_cmd: RawDeliveryCommand = DeliveryCommand::Data {
            key: TEST_VMO_KEY,
            offset: payload.offset(),
            length: corrupted_chunk.len() as u32,
            target_offset: read_alignment as u64,
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
        let read_alignment = TARGET_READ_AHEAD_SIZE;
        let blob_size = (read_alignment * 3) as u64;
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
            key: TEST_VMO_KEY,
            offset: payload.offset(),
            length: env.valid_leaves.len() as u32,
            read_alignment: read_alignment as u32,
        }
        .into();
        payload.commit(raw_cmd).expect("commit failed");

        let blob =
            env.pager_and_verifier.cache.get_by_key(TEST_VMO_KEY).expect("get_by_key failed");
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
        let valid_chunk1 = &env.blob_data[..read_alignment];
        let mut payload =
            sender.reserve_payload(valid_chunk1.len()).expect("reserve_payload failed");
        payload.data().copy_from_slice(valid_chunk1);
        let raw_cmd: RawDeliveryCommand = DeliveryCommand::Data {
            key: TEST_VMO_KEY,
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
        let mut corrupted_chunk2 = env.blob_data[read_alignment..read_alignment * 2].to_vec();
        corrupted_chunk2[0] ^= 0xFF;
        let mut payload =
            sender.reserve_payload(corrupted_chunk2.len()).expect("reserve_payload failed");
        payload.data().copy_from_slice(&corrupted_chunk2);
        let raw_cmd: RawDeliveryCommand = DeliveryCommand::Data {
            key: TEST_VMO_KEY,
            offset: payload.offset(),
            length: corrupted_chunk2.len() as u32,
            target_offset: read_alignment as u64,
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
            key: TEST_VMO_KEY,
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

    #[test_case(TARGET_READ_AHEAD_SIZE; "read_alignment_128k")]
    #[test_case(96 * 1024; "read_alignment_96k")]
    #[fuchsia::test]
    async fn test_delivery_data_multiple_chunks(read_alignment: usize) {
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
            key: TEST_VMO_KEY,
            offset: payload.offset(),
            length: env.valid_leaves.len() as u32,
            read_alignment: read_alignment as u32,
        }
        .into();
        payload.commit(raw_cmd).expect("commit failed");

        let blob =
            env.pager_and_verifier.cache.get_by_key(TEST_VMO_KEY).expect("get_by_key failed");
        while blob.merkle_verifier.get().is_none() {
            fasync::Timer::new(std::time::Duration::from_millis(5)).await;
        }

        let expected_data = env.blob_data.clone();
        let paged_vmo_clone =
            paged_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("duplicate_handle failed");

        let (tx, rx) = oneshot::channel();
        let thread = std::thread::spawn(move || {
            let mut buf = vec![0u8; expected_data.len()];
            paged_vmo_clone.read(&mut buf, 0).expect("failed to read from paged vmo");
            assert_eq!(buf, expected_data);
            let _ = tx.send(());
        });

        // Push incrementally - chunks must be a multiple of the blob's read_alignment
        for (i, chunk) in env.blob_data.chunks(read_alignment).enumerate() {
            let mut payload = sender.reserve_payload(chunk.len()).expect("reserve_payload failed");
            payload.data().copy_from_slice(chunk);
            let raw_cmd: RawDeliveryCommand = DeliveryCommand::Data {
                key: TEST_VMO_KEY,
                offset: payload.offset(),
                length: chunk.len() as u32,
                target_offset: (i * read_alignment) as u64,
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
        let data_size = (TARGET_READ_AHEAD_SIZE + 1024) as u64; // Blob with unaligned size
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
            key: TEST_VMO_KEY,
            offset: payload.offset(),
            length: env.valid_leaves.len() as u32,
            read_alignment: TARGET_READ_AHEAD_SIZE as u32,
        }
        .into();
        payload.commit(raw_cmd).expect("commit failed");

        let blob =
            env.pager_and_verifier.cache.get_by_key(TEST_VMO_KEY).expect("get_by_key failed");
        while blob.merkle_verifier.get().is_none() {
            fasync::Timer::new(std::time::Duration::from_millis(5)).await;
        }

        let test_payload = env.blob_data.clone();
        assert_eq!(test_payload.len(), data_size as usize);

        let chunk1 = &test_payload[..TARGET_READ_AHEAD_SIZE];
        let mut payload1 = sender.reserve_payload(chunk1.len()).expect("reserve_payload failed");
        payload1.data().copy_from_slice(chunk1);
        let off1 = payload1.offset();
        payload1
            .commit(
                DeliveryCommand::Data {
                    key: TEST_VMO_KEY,
                    target_offset: 0,
                    length: chunk1.len() as u32,
                    offset: off1,
                }
                .into(),
            )
            .expect("commit failed");

        let chunk2 = &test_payload[TARGET_READ_AHEAD_SIZE..];
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
                    key: TEST_VMO_KEY,
                    target_offset: TARGET_READ_AHEAD_SIZE as u64,
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

    #[fuchsia::test]
    async fn test_open_child_session() {
        let (mapping_proxy, mut mapping_stream) =
            fidl::endpoints::create_proxy_and_stream::<fmapping::MappingProviderMarker>();
        let (mapper_proxy, mut mapper_stream) =
            fidl::endpoints::create_proxy_and_stream::<fblock::MapperMarker>();

        let _mapping_task = fasync::Task::spawn(async move {
            if let Some(fmapping::MappingProviderRequest::OpenSession { session, responder }) =
                mapping_stream.try_next().await.expect("try_next failed")
            {
                let mapping_vmo = zx::Vmo::create(zx::system_get_page_size().into())
                    .expect("zx::Vmo::create failed");
                responder.send(Ok(mapping_vmo)).expect("send failed");

                let mut session_stream = session.into_stream();
                while let Some(request) = session_stream.try_next().await.expect("try_next failed")
                {
                    match request {
                        fmapping::MappingSessionRequest::Open { responder, .. } => {
                            responder.send(Ok(8192)).expect("send failed");
                        }
                        fmapping::MappingSessionRequest::Close { responder, .. } => {
                            responder.send(Ok(())).expect("send failed");
                        }
                        _ => {}
                    }
                }
            }
        });

        let (child_key_tx, child_key_rx) = oneshot::channel();
        let _mapper_task = fasync::Task::spawn(async move {
            if let Some(fblock::MapperRequest::OpenSession { session, responder, .. }) =
                mapper_stream.try_next().await.expect("try_next failed")
            {
                responder.send(Ok(())).expect("send failed");
                let mut session_stream = session.into_stream();
                if let Some(fblock::MapperSessionRequest::OpenChildSession {
                    parent_key,
                    port,
                    delivery_queue,
                    responder,
                    ..
                }) = session_stream.try_next().await.expect("try_next failed")
                {
                    assert!(port.is_some());
                    assert!(delivery_queue.is_none());
                    child_key_tx.send(parent_key).expect("send failed");
                    responder.send(Ok(())).expect("send failed");
                }
            }
        });

        let pager_and_verifier = Arc::new(
            BlobPagerAndVerifier::new(&mapping_proxy, &mapper_proxy)
                .await
                .expect("BlobPagerAndVerifier::new failed"),
        );

        let hash: [u8; 32] = [0x55; 32];
        let unknown_hash: [u8; 32] = [0x99; 32];

        // Unknown blob should return NOT_FOUND.
        let (_client, server) = fidl::endpoints::create_endpoints::<fblock::MapperSessionMarker>();
        let mapping_vmo = zx::Vmo::create(4096).unwrap();
        assert_eq!(
            pager_and_verifier.open_child_session(&unknown_hash, mapping_vmo, server).await,
            Err(zx::Status::NOT_FOUND)
        );

        // Open the blob first via create_vmo.
        let vmo = pager_and_verifier.create_vmo(&hash).await.expect("create_vmo failed");
        let (client_proxy, server) = fidl::endpoints::create_proxy::<fblock::MapperSessionMarker>();
        let mapping_vmo = zx::Vmo::create(4096).unwrap();
        pager_and_verifier
            .open_child_session(&hash, mapping_vmo, server)
            .await
            .expect("open_child_session failed");

        let parent_key = child_key_rx.await.expect("failed to receive parent_key");
        assert_eq!(parent_key, TEST_VMO_KEY);

        let _child_vmo = client_proxy
            .create_vmo(7, 4096, fblock::CreateVmoOptions::empty())
            .await
            .expect("create_vmo fidl failed")
            .expect("create_vmo returned error");
        assert!(pager_and_verifier.cache.get_child_vmo(TEST_VMO_KEY, 7).is_some());

        client_proxy.close_vmo(7).expect("close_vmo fidl failed");
        while pager_and_verifier.cache.get_child_vmo(TEST_VMO_KEY, 7).is_some() {
            fasync::Timer::new(std::time::Duration::from_millis(5)).await;
        }
        drop(vmo);
    }

    #[fuchsia::test]
    async fn test_deliver_child_pages() {
        let env = TestEnv::new(32 * 1024).await;
        let _parent_vmo =
            env.pager_and_verifier.create_vmo(&env.valid_root).await.expect("create_vmo failed");

        let mut sender = SyncSender::<RawDeliveryCommand>::new(
            env.delivery_vmo
                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                .expect("duplicate_handle failed"),
            zx::system_get_page_size() as usize,
            mapping::PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .expect("SyncSender::new failed");

        // Register parent blob with 32 KiB verification alignment.
        let mut payload =
            sender.reserve_payload(env.valid_leaves.len()).expect("reserve_payload failed");
        payload.data().copy_from_slice(&env.valid_leaves);
        let off = payload.offset();
        payload
            .commit(
                DeliveryCommand::RegisterBlob {
                    key: TEST_VMO_KEY,
                    offset: off,
                    length: env.valid_leaves.len() as u32,
                    read_alignment: 32 * 1024,
                }
                .into(),
            )
            .expect("commit failed");

        let blob =
            env.pager_and_verifier.cache.get_by_key(TEST_VMO_KEY).expect("get_by_key failed");
        while blob.merkle_verifier.get().is_none() {
            fasync::Timer::new(std::time::Duration::from_millis(5)).await;
        }

        let child_port = zx::Port::create();
        let child_key = 77u64;
        let child_vmo = env
            .pager_and_verifier
            .cache
            .pager
            .create_vmo(zx::VmoOptions::empty(), &child_port, child_key, 100)
            .unwrap();
        env.pager_and_verifier.cache.register_child_vmo(
            TEST_VMO_KEY,
            child_key,
            child_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            100,
        );

        let mut payload = sender.reserve_payload(32 * 1024).expect("reserve_payload failed");
        payload.data().copy_from_slice(&env.blob_data);
        let off = payload.offset();
        payload
            .commit(
                DeliveryCommand::ChildData {
                    parent_key: TEST_VMO_KEY,
                    parent_offset: 0,
                    offset: off,
                    length: 32 * 1024,
                    child_key,
                    child_target_offset: 0,
                    in_chunk_offset: 4096,
                    child_length: 4096,
                }
                .into(),
            )
            .expect("commit failed");

        let mut buf = vec![0u8; 4096];
        child_vmo.read(&mut buf, 0).expect("read child_vmo");
        assert_eq!(&buf[..100], &[0x42u8; 100]);
        assert_eq!(&buf[100..], &[0u8; 3996]);

        env.teardown().await;
    }

    #[fuchsia::test]
    async fn test_deliver_child_inline_tail_from_parent_vmo() {
        let env = TestEnv::new(32 * 1024).await;
        let _parent_vmo =
            env.pager_and_verifier.create_vmo(&env.valid_root).await.expect("create_vmo failed");

        let mut sender = SyncSender::<RawDeliveryCommand>::new(
            env.delivery_vmo
                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                .expect("duplicate_handle failed"),
            zx::system_get_page_size() as usize,
            mapping::PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .expect("SyncSender::new failed");

        // Register parent blob with 32 KiB verification alignment.
        let mut payload =
            sender.reserve_payload(env.valid_leaves.len()).expect("reserve_payload failed");
        payload.data().copy_from_slice(&env.valid_leaves);
        let off = payload.offset();
        payload
            .commit(
                DeliveryCommand::RegisterBlob {
                    key: TEST_VMO_KEY,
                    offset: off,
                    length: env.valid_leaves.len() as u32,
                    read_alignment: 32 * 1024,
                }
                .into(),
            )
            .expect("commit failed");

        let blob =
            env.pager_and_verifier.cache.get_by_key(TEST_VMO_KEY).expect("get_by_key failed");
        while blob.merkle_verifier.get().is_none() {
            fasync::Timer::new(std::time::Duration::from_millis(5)).await;
        }

        // Register a child VMO with size 100 bytes (inline tail at offset 4096 + 500 in parent).
        let child_key = 77u64;
        let child_port = zx::Port::create();
        let child_vmo = env
            .pager_and_verifier
            .cache
            .pager
            .create_vmo(zx::VmoOptions::empty(), &child_port, child_key, 100)
            .unwrap();
        env.pager_and_verifier.cache.register_child_vmo(
            TEST_VMO_KEY,
            child_key,
            child_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            100,
        );

        // Push ChildInlineTail BEFORE the parent VMO page is populated!
        // This verifies that `ChildInlineTail` pops the FIFO message first and does not deadlock
        // when `cached_blob.vmo.read(...)` blocks waiting for a subsequent `DeliveryCommand::Data`.
        sender
            .reserve_payload(0)
            .expect("reserve_payload failed")
            .commit(
                DeliveryCommand::ChildInlineTail {
                    parent_key: TEST_VMO_KEY,
                    parent_byte_offset: 4096 + 500,
                    child_key,
                    child_target_offset: 0,
                    tail_length: 100,
                }
                .into(),
            )
            .expect("commit failed");

        // Now deliver the parent blob's 32 KiB data chunk via `DeliveryCommand::Data`.
        let mut payload = sender.reserve_payload(32 * 1024).expect("reserve_payload failed");
        payload.data().copy_from_slice(&env.blob_data);
        let off = payload.offset();
        payload
            .commit(
                DeliveryCommand::Data {
                    key: TEST_VMO_KEY,
                    target_offset: 0,
                    offset: off,
                    length: 32 * 1024,
                }
                .into(),
            )
            .expect("commit failed");

        let mut buf = vec![0u8; 4096];
        child_vmo.read(&mut buf, 0).expect("read child_vmo");
        assert_eq!(&buf[..100], &[0x42u8; 100]);
        assert_eq!(&buf[100..], &[0u8; 3996]);

        env.teardown().await;
    }

    #[fuchsia::test]
    async fn test_get_file_mapper_handler() {
        use vfs::ToObjectRequest as _;
        use vfs::file::StreamIoConnection;

        let (mapping_proxy, mut mapping_stream) =
            fidl::endpoints::create_proxy_and_stream::<fmapping::MappingProviderMarker>();
        let (mapper_proxy, mut mapper_stream) =
            fidl::endpoints::create_proxy_and_stream::<fblock::MapperMarker>();

        let _mapping_task = fasync::Task::spawn(async move {
            if let Some(fmapping::MappingProviderRequest::OpenSession { session, responder }) =
                mapping_stream.try_next().await.expect("try_next failed")
            {
                let mapping_vmo = zx::Vmo::create(zx::system_get_page_size().into())
                    .expect("zx::Vmo::create failed");
                responder.send(Ok(mapping_vmo)).expect("send failed");

                let mut session_stream = session.into_stream();
                while let Some(request) = session_stream.try_next().await.expect("try_next failed")
                {
                    match request {
                        fmapping::MappingSessionRequest::Open { responder, .. } => {
                            responder.send(Ok(8192)).expect("send failed");
                        }
                        fmapping::MappingSessionRequest::Close { responder, .. } => {
                            responder.send(Ok(())).expect("send failed");
                        }
                        _ => {}
                    }
                }
            }
        });

        let (child_key_tx, child_key_rx) = oneshot::channel();
        let _mapper_task = fasync::Task::spawn(async move {
            if let Some(fblock::MapperRequest::OpenSession { session, responder, .. }) =
                mapper_stream.try_next().await.expect("try_next failed")
            {
                responder.send(Ok(())).expect("send failed");
                let mut session_stream = session.into_stream();
                if let Some(fblock::MapperSessionRequest::OpenChildSession {
                    parent_key,
                    responder,
                    ..
                }) = session_stream.try_next().await.expect("try_next failed")
                {
                    child_key_tx.send(parent_key).expect("send failed");
                    responder.send(Ok(())).expect("send failed");
                }
            }
        });

        let pager_and_verifier = Arc::new(
            BlobPagerAndVerifier::new(&mapping_proxy, &mapper_proxy)
                .await
                .expect("BlobPagerAndVerifier::new failed"),
        );

        let hash: [u8; 32] = [0x66; 32];
        let vmo = pager_and_verifier.create_vmo(&hash).await.expect("create_vmo failed");

        let scope = vfs::execution_scope::ExecutionScope::new();
        let (file_mapper, file_mapper_server) =
            fidl::endpoints::create_proxy::<fmapping::FileMapperMarker>();
        fidl_fuchsia_io::Flags::PROTOCOL_SERVICE
            .to_object_request(file_mapper_server.into_channel())
            .handle(|object_request| {
                vfs::service::serve(
                    pager_and_verifier.get_file_mapper_handler(scope.clone()),
                    scope.clone(),
                    &fidl_fuchsia_io::Flags::PROTOCOL_SERVICE,
                    object_request,
                )
            });

        // 1. Unregistered token returns NOT_FOUND.
        let unregistered_token = zx::Event::create();
        let (_client, server) = fidl::endpoints::create_endpoints::<fblock::MapperSessionMarker>();
        assert_eq!(
            file_mapper
                .open_session(unregistered_token, zx::Vmo::create(4096).unwrap(), server)
                .await
                .unwrap(),
            Err(zx::Status::NOT_FOUND.into_raw())
        );

        // 2. Non-readable VmoBlob connection returns ACCESS_DENIED.
        let vmo_blob =
            vmo_blob::VmoBlob::new(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(), hash);
        let (no_read_proxy, no_read_server) =
            fidl::endpoints::create_proxy::<fidl_fuchsia_io::FileMarker>();
        let no_read_flags = fidl_fuchsia_io::Flags::PROTOCOL_FILE;
        let scope_clone = scope.clone();
        let vmo_blob_clone = vmo_blob.clone();
        scope.spawn(no_read_flags.to_object_request(no_read_server).handle_async(
            async move |object_request| {
                object_request
                    .create_connection::<StreamIoConnection<_>, _>(
                        scope_clone,
                        vmo_blob_clone,
                        no_read_flags,
                    )
                    .await
            },
        ));
        let no_read_token = zx::Event::from(
            no_read_proxy.get_token().await.unwrap().map_err(zx::Status::err_from_raw).unwrap(),
        );
        let (_client, server) = fidl::endpoints::create_endpoints::<fblock::MapperSessionMarker>();
        assert_eq!(
            file_mapper
                .open_session(no_read_token, zx::Vmo::create(4096).unwrap(), server)
                .await
                .unwrap(),
            Err(zx::Status::ACCESS_DENIED.into_raw())
        );

        // 3. Non-VmoBlob file connection returns INVALID_ARGS.
        let other_file = vfs::file::vmo::read_only(b"not a vmo blob");
        let (other_proxy, other_server) =
            fidl::endpoints::create_proxy::<fidl_fuchsia_io::FileMarker>();
        fidl_fuchsia_io::PERM_READABLE.to_object_request(other_server).handle(|object_request| {
            vfs::file::serve(
                other_file,
                scope.clone(),
                &fidl_fuchsia_io::PERM_READABLE,
                object_request,
            )
        });
        let other_token = zx::Event::from(
            other_proxy.get_token().await.unwrap().map_err(zx::Status::err_from_raw).unwrap(),
        );
        let (_client, server) = fidl::endpoints::create_endpoints::<fblock::MapperSessionMarker>();
        assert_eq!(
            file_mapper
                .open_session(other_token, zx::Vmo::create(4096).unwrap(), server)
                .await
                .unwrap(),
            Err(zx::Status::INVALID_ARGS.into_raw())
        );

        // 4. Readable VmoBlob connection succeeds and opens a child mapper session.
        let (read_proxy, read_server) =
            fidl::endpoints::create_proxy::<fidl_fuchsia_io::FileMarker>();
        let read_flags = fidl_fuchsia_io::PERM_READABLE;
        let scope_clone = scope.clone();
        scope.spawn(read_flags.to_object_request(read_server).handle_async(
            async move |object_request| {
                object_request
                    .create_connection::<StreamIoConnection<_>, _>(
                        scope_clone,
                        vmo_blob,
                        read_flags,
                    )
                    .await
            },
        ));
        let valid_token = zx::Event::from(
            read_proxy.get_token().await.unwrap().map_err(zx::Status::err_from_raw).unwrap(),
        );
        let (client_proxy, server) = fidl::endpoints::create_proxy::<fblock::MapperSessionMarker>();
        file_mapper
            .open_session(valid_token, zx::Vmo::create(4096).unwrap(), server)
            .await
            .unwrap()
            .map_err(zx::Status::err_from_raw)
            .expect("open_session should succeed");

        let parent_key = child_key_rx.await.expect("failed to receive parent_key");
        assert_eq!(parent_key, TEST_VMO_KEY);

        let _child_vmo = client_proxy
            .create_vmo(7, 4096, fblock::CreateVmoOptions::empty())
            .await
            .expect("create_vmo fidl failed")
            .expect("create_vmo returned error");
        assert!(pager_and_verifier.cache.get_child_vmo(TEST_VMO_KEY, 7).is_some());

        drop(vmo);
    }
}
