// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! This library provides a centralized cache and lifecycle manager for pager-backed blob VMOs.
//!
//! # Architecture
//! When clients request a blob via `create_vmo`, the library checks a global cache. If the blob is
//! missing, the first client sets the cache entry to `Pending` and contacts the mapping server to
//! initialize a new pager-backed VMO. If concurrent requests ask for the same blob while it is
//! still `Pending`, they use `event_listener` to safely block and wait on the initial request. When
//! the mapping server replies, the first task wakes all waiting listeners simultaneously, ensuring
//! the mapping server is contacted only once.
//!
//! # Lifecycle and Eviction
//! 1. Handles returned to clients are child VMOs.
//! 2. The primary `CachedBlob` is held alive by the `strong_blob_ref` (`Option<Arc>`) inside a
//!    `fuchsia_async::PacketReceiver` (`ZeroChildrenReceiver`), which is registered with the
//!    thread's background async executor.
//! 3. When the last client drops their child VMO, the kernel fires a `ZX_VMO_ZERO_CHILDREN` signal.
//!    The executor triggers our receiver, which drops its strong reference to the blob.
//! 4. This causes the `CachedBlob` to fall out of memory and fire its `Drop` implementation to
//!    clear itself from the cache and call close on the blob's mapping provider session.

use anyhow::{Context, Error, anyhow};
use event_listener as _;
use fidl_fuchsia_storage_block as fblock;
use fidl_fuchsia_storage_mapping as fmapping;
use fuchsia_async as fasync;
use fuchsia_sync::Mutex;
use futures::TryStreamExt;
use std::collections::{HashMap, hash_map};
use std::sync::{Arc, Weak};
use zx;

// A `fuchsia_async::PacketReceiver` that watches for a VMO to reach zero children and drops the
// reference count of CachedBlob.
struct ZeroChildrenReceiver {
    strong_blob_ref: Mutex<Option<Arc<CachedBlob>>>,
}

impl fasync::PacketReceiver for ZeroChildrenReceiver {
    fn receive_packet(&self, packet: zx::Packet) {
        if let zx::PacketContents::SignalOne(signals) = packet.contents() {
            if signals.observed().contains(zx::Signals::VMO_ZERO_CHILDREN) {
                // Extract the expiring blob to drop it outside of the `strong_blob_ref` lock to
                // avoid a deadlock.
                // If we dropped it inside the lock, the CachedBlob's Drop implementation would
                // attempt to acquire the global VMO cache map lock.
                // The other path of acquiring the cache map lock is through `get_or_reserve` where
                // it acquires the map lock first and then attempts to acquire `strong_blob_ref`.
                // Holding `strong_blob_ref` and then trying to acquire the map lock creates an
                // AB-BA deadlock.
                let _cached_blob_to_drop;
                {
                    let mut strong_ref = self.strong_blob_ref.lock();
                    if let Some(cached_blob) = &*strong_ref {
                        if let Ok(info) = cached_blob.vmo.info() {
                            if info.num_children == 0 {
                                // Overwrite `strong_blob_ref` to None and extract the Arc.
                                // We bring it out to the outer scope to drop it safely.
                                _cached_blob_to_drop = strong_ref.take();
                            } else {
                                // If info.num_children != 0, a concurrent client requested the blob
                                // and created a new child VMO before we could process this packet.
                                // Resume watching for the next VMO_ZERO_CHILDREN signal.
                                cached_blob.wait_for_zero_children();
                            }
                        }
                    }
                }
            }
        }
    }
}

struct CachedBlob {
    // The parent pager-backed VMO. We should only ever vend children of this VMO to clients
    // so we can correctly track when all children are dropped to evict the blob from cache.
    vmo: zx::Vmo,
    identifier: [u8; 32],
    // Hold a weak reference to avoid a circular reference as the cache holds `CachedBlob`.
    cache: Weak<PagerVmoCache>,
    vmo_key: u32,
    registration: fasync::ReceiverRegistration<ZeroChildrenReceiver>,
}

impl CachedBlob {
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
            {
                let mut map = cache.map.lock();
                if let hash_map::Entry::Occupied(entry) = map.entry(self.identifier) {
                    if let BlobState::Ready(weak) = entry.get() {
                        // Ensure we only remove the cache entry if it still points to this expiring
                        // instance.
                        if weak.strong_count() == 0 {
                            entry.remove();
                        }
                    }
                }
            }

            // At this point the strong count is definitively zero. Tear down the extent mapping
            // session for this blob. Even if a racing thread successfully repopulated the cache map
            // above, they generated a completely new vmo_key for the extent mapping, so we must
            // clean up this expiring mapping.
            let session = cache.mapping_session.clone();
            let vmo_key = self.vmo_key;
            fasync::Task::spawn(async move {
                let _ = session.close(vmo_key).await;
            })
            .detach();
        }
    }
}

enum BlobState {
    // Blob open mapping is taking place, wait on the event. Concurrent requests wait for the
    // primary creator task to finish. Once the VMO is ready (or fails), the primary task notifies
    // this event to wake all pending tasks.
    Pending(Arc<event_listener::Event>),
    // Blob is mapped to a pager-backed VMO. The receiver watches for `ZX_VMO_ZERO_CHILDREN` to
    // clear the cache entry and close the mapping.
    Ready(Weak<CachedBlob>),
}

// A Drop guard to clean up the cache entry if create_vmo fails.
struct PendingCacheEntryGuard {
    cache: Arc<PagerVmoCache>,
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
            let mut map = self.cache.map.lock();
            // If the creation failed, clear out the pending state.
            if let Some(BlobState::Pending(completion_event)) = map.remove(&id) {
                // Wake up all suspended tasks concurrently waiting on this blob.
                completion_event.notify(usize::MAX);
            }
        }
    }
}

// Reflects the state of the blob in the PagerVmoCache.
enum CacheLookup {
    // The VMO has been created and cached. Returns a reference to it.
    Ready(Arc<CachedBlob>),
    // The VMO is currently being created by another request. Contains a listener to await
    // completion.
    Pending(event_listener::EventListener),
    // The blob is absent. Provides a drop-safe guard to be used with creation. If the task crashes
    // or returns an early error, the pending state will be cleared from the cache.
    Missing(PendingCacheEntryGuard),
}

// PagerVmoCache safely manages the concurrent creation and caching of Pager-backed VMOs to prevent
// duplicated requests and redundant Zircon root VMO mappings.
struct PagerVmoCache {
    map: Mutex<HashMap<[u8; 32], BlobState>>,
    mapping_session: fmapping::MappingSessionProxy,
}

impl PagerVmoCache {
    fn new(mapping_session: fmapping::MappingSessionProxy) -> Self {
        Self { map: Mutex::new(HashMap::new()), mapping_session }
    }

    // Queries the cache for an existing VMO. It handles three distinct cases:
    // 1. Ready:   The VMO is already cached. Caller can use it immediately.
    // 2. Pending: Another async task is actively creating this VMO. Caller receives a listener to
    //             wait without duplicated effort or data races.
    // 3. Missing: The blob isn't in the cache. Caller receives a guard to act as the "producer" and
    //             fetch the VMO, locking out other callers (putting them in Pending state) until it
    //             finishes.
    fn get_or_reserve(self: &Arc<Self>, identifier: &[u8; 32]) -> Result<CacheLookup, Error> {
        let mut map = self.map.lock();
        match map.get(identifier) {
            Some(BlobState::Ready(weak_blob)) => {
                if let Some(arc_blob) = weak_blob.upgrade() {
                    {
                        let receiver = arc_blob.registration.receiver();
                        let mut strong_ref = receiver.strong_blob_ref.lock();
                        // If strong_ref is None, it means that ZeroChildrenReceiver received a
                        // `ZX_VMO_ZERO_CHILDREN` signal and explicitly dropped it. We should
                        // transition it back to Some.
                        if strong_ref.is_none() {
                            *strong_ref = Some(arc_blob.clone());
                            // Resume watching for the zero children kernel signal
                            arc_blob.wait_for_zero_children();
                        }
                    }
                    Ok(CacheLookup::Ready(arc_blob))
                } else {
                    let completion_event = Arc::new(event_listener::Event::new());
                    map.insert(*identifier, BlobState::Pending(completion_event.clone()));
                    Ok(CacheLookup::Missing(PendingCacheEntryGuard {
                        cache: self.clone(),
                        identifier: Some(*identifier),
                    }))
                }
            }
            Some(BlobState::Pending(completion_event)) => {
                Ok(CacheLookup::Pending(completion_event.listen()))
            }
            None => {
                let completion_event = Arc::new(event_listener::Event::new());
                map.insert(*identifier, BlobState::Pending(completion_event.clone()));
                Ok(CacheLookup::Missing(PendingCacheEntryGuard {
                    cache: self.clone(),
                    identifier: Some(*identifier),
                }))
            }
        }
    }

    fn finalize_pending_request(&self, identifier: &[u8; 32], result: Option<&Arc<CachedBlob>>) {
        let mut map = self.map.lock();
        let mut entry = match map.entry(*identifier) {
            hash_map::Entry::Occupied(e) => e,
            hash_map::Entry::Vacant(_) => unreachable!("Cache entry was unexpectedly missing"),
        };

        let event = match entry.get() {
            BlobState::Pending(event) => event.clone(),
            _ => unreachable!("Cache entry was unexpectedly not Pending"),
        };

        if let Some(cached) = result {
            entry.insert(BlobState::Ready(Arc::downgrade(cached)));
        } else {
            entry.remove();
        }

        event.notify(usize::MAX);
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
    _mapper_session: fblock::MapperSessionProxy,
    port: zx::Port,
    pager: zx::Pager,
    vmo_cache: Arc<PagerVmoCache>,
}

impl BlobPagerAndVerifier {
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
        let (mapping_session, session_server) =
            fidl::endpoints::create_proxy::<fmapping::MappingSessionMarker>();
        let mapping_vmo = mapping_provider
            .open_session(session_server)
            .await
            .context("FIDL error calling MappingProvider.OpenSession")?
            .map_err(|e| anyhow!("MappingProvider open_session failed: {e:?}"))?;

        let port = zx::Port::create();
        let delivery_queue = zx::Vmo::create(0).context("Failed to create delivery queue VMO")?;
        let pager =
            zx::Pager::create(zx::PagerOptions::empty()).context("Failed to create pager")?;
        let (mapper_session, mapper_session_server) =
            fidl::endpoints::create_proxy::<fblock::MapperSessionMarker>();
        let port_dup = port.duplicate_handle(zx::Rights::SAME_RIGHTS)?;

        mapper
            .open_session(mapper_session_server, mapping_vmo, Some(port_dup), Some(delivery_queue))
            .await
            .context("FIDL error calling Mapper.OpenSession")?
            .map_err(|e| anyhow!("Mapper.OpenSession failed: {e:?}"))?;

        Ok(Self {
            _mapper_session: mapper_session,
            port,
            pager,
            vmo_cache: Arc::new(PagerVmoCache::new(mapping_session)),
        })
    }

    /// Create pager owned VMO for the blob identified by its Merkle Root Hash.
    pub async fn create_vmo(&self, identifier: &[u8; 32]) -> Result<zx::Vmo, Error> {
        let parent_vmo = loop {
            let mut guard = match self.vmo_cache.get_or_reserve(identifier)? {
                CacheLookup::Ready(vmo) => break vmo,
                CacheLookup::Pending(listener) => {
                    listener.await;
                    continue; // Re-evaluate cache now that the blocking event triggered
                }
                CacheLookup::Missing(guard) => guard,
            };

            let result: Result<(zx::Vmo, u32), Error> = async {
                // Ask Fxfs to register the blob and write its extent mappings into the shared
                // mapping VMO, returning a key that is used to generate the pager-backed VMO.
                let (size, key) = self
                    .vmo_cache
                    .mapping_session
                    .open(identifier)
                    .await
                    .context("FIDL error calling MappingSession.Open")?
                    .map_err(|e| anyhow!("MappingSession.Open failed for blob: {e:?}"))?;

                let paged_vmo =
                    self.pager.create_vmo(zx::VmoOptions::empty(), &self.port, key as u64, size)?;

                Ok((paged_vmo, key))
            }
            .await;

            // Successfully received response; dismiss the drop guard.
            guard.dismiss();

            match result {
                Ok((vmo, key)) => {
                    // Create the initial child. We vend children of this VMO to clients so we can
                    // track when all children are dropped to evict the blob from cache.
                    let first_child = vmo
                        .create_child(
                            zx::VmoChildOptions::REFERENCE | zx::VmoChildOptions::NO_WRITE,
                            0,
                            0,
                        )
                        .map_err(|s| anyhow!("Failed to create child VMO: {}", s))?;

                    let zero_children_receiver =
                        ZeroChildrenReceiver { strong_blob_ref: Mutex::new(None) };
                    let zero_children_registration =
                        fasync::EHandle::local().register_receiver(zero_children_receiver);

                    let cached = Arc::new(CachedBlob {
                        vmo,
                        identifier: *identifier,
                        cache: Arc::downgrade(&self.vmo_cache),
                        vmo_key: key,
                        registration: zero_children_registration,
                    });

                    {
                        let receiver = cached.registration.receiver();
                        let mut strong_ref = receiver.strong_blob_ref.lock();
                        *strong_ref = Some(cached.clone());
                    }

                    cached.wait_for_zero_children();

                    self.vmo_cache.finalize_pending_request(identifier, Some(&cached));

                    return Ok(first_child);
                }
                Err(e) => {
                    self.vmo_cache.finalize_pending_request(identifier, None);
                    return Err(e);
                }
            }
        };

        // Create child if `CacheLookup::Ready(vmo)` broke the loop.
        let child = parent_vmo
            .vmo
            .create_child(zx::VmoChildOptions::REFERENCE | zx::VmoChildOptions::NO_WRITE, 0, 0)
            .map_err(|s| anyhow!("Failed to create child VMO: {}", s))?;

        Ok(child)
    }

    // TODO(https://fxbug.dev/535489428): Watch for data delivery events on the delivery queue VMO,
    // cryptographically verify the payloads placed into it by the block driver, and finalize the
    // page fault via `zx_pager_supply_pages`.
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_BLOB_HASH: &[u8; 32] = b"test_blob_hash_padded_to_32_byte";
    const TEST_VMO_SIZE: u64 = 8192;
    const TEST_VMO_KEY: u32 = 42;

    struct TestEnv {
        pager_and_verifier: Arc<BlobPagerAndVerifier>,
        mapping_task: fasync::Task<()>,
        mapper_task: fasync::Task<()>,
        close_signal: Option<futures::channel::oneshot::Receiver<()>>,
    }

    impl TestEnv {
        async fn new() -> Self {
            let (mapping_proxy, mut mapping_stream) =
                fidl::endpoints::create_proxy_and_stream::<fmapping::MappingProviderMarker>();

            let (close_tx, close_rx) = futures::channel::oneshot::channel();
            let mapping_task = fasync::Task::spawn(async move {
                let mut close_tx = Some(close_tx);
                if let Some(fmapping::MappingProviderRequest::OpenSession { session, responder }) =
                    mapping_stream.try_next().await.unwrap()
                {
                    let mapping_vmo = zx::Vmo::create(zx::system_get_page_size().into()).unwrap();
                    responder.send(Ok(mapping_vmo)).unwrap();

                    let mut session_stream = session.into_stream();
                    let mut open_calls = 0;
                    while let Some(request) = session_stream.try_next().await.unwrap() {
                        match request {
                            fmapping::MappingSessionRequest::Open { identifier, responder } => {
                                assert_eq!(&identifier, TEST_BLOB_HASH);
                                open_calls += 1;
                                assert_eq!(
                                    open_calls, 1,
                                    "Open should only be called once per identifier"
                                );
                                responder.send(Ok((TEST_VMO_SIZE, TEST_VMO_KEY))).unwrap();
                            }
                            fmapping::MappingSessionRequest::Close { key, responder } => {
                                assert_eq!(key, TEST_VMO_KEY);
                                responder.send(Ok(())).unwrap();
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

            let mapper_task = fasync::Task::spawn(async move {
                if let Some(fblock::MapperRequest::OpenSession { responder, .. }) =
                    mapper_stream.try_next().await.unwrap()
                {
                    responder.send(Ok(())).unwrap();
                }
            });

            Self {
                pager_and_verifier: Arc::new(
                    BlobPagerAndVerifier::new(&mapping_proxy, &mapper_proxy).await.unwrap(),
                ),
                mapping_task,
                mapper_task,
                close_signal: Some(close_rx),
            }
        }

        async fn teardown(self) {
            drop(self.pager_and_verifier);
            self.mapping_task.await;
            self.mapper_task.await;
        }
    }

    #[fuchsia::test]
    async fn test_create_vmo() {
        let env = TestEnv::new().await;
        let vmo =
            env.pager_and_verifier.create_vmo(TEST_BLOB_HASH).await.expect("Failed to create VMO");
        let size = vmo.get_size().unwrap();

        // Pager VMO sizes are rounded up to the nearest page boundary.
        let page_size = zx::system_get_page_size() as u64;
        let expected_pages = (TEST_VMO_SIZE + page_size - 1) / page_size;
        assert_eq!(size, expected_pages * page_size);

        // Explicitly drop the VMO so `ZX_VMO_ZERO_CHILDREN` fires and the mock mapping
        // server receives the `.close()` IPC. Otherwise `teardown` will deadlock waiting
        // for the server loop to exit!
        drop(vmo);

        env.teardown().await;
    }

    #[fuchsia::test]
    async fn test_create_vmo_concurrent_access() {
        let env = TestEnv::new().await;
        let mut futures = vec![];
        for _ in 0..10 {
            let verifier = env.pager_and_verifier.clone();
            futures.push(fasync::Task::spawn(async move {
                verifier.create_vmo(TEST_BLOB_HASH).await.expect("Failed to create VMO")
            }));
        }
        let vmos = futures::future::join_all(futures).await;

        drop(vmos);
        env.teardown().await;
    }

    #[fuchsia::test]
    async fn test_zero_children_eviction() {
        let mut env = TestEnv::new().await;

        let child_vmo = env.pager_and_verifier.create_vmo(TEST_BLOB_HASH).await.unwrap();

        {
            let cache = env.pager_and_verifier.vmo_cache.map.lock();
            assert!(matches!(cache.get(TEST_BLOB_HASH), Some(BlobState::Ready { .. })));
        }

        drop(child_vmo);

        // wait for the packet receiver to observe the ZERO_CHILDREN signal and dispatch
        // `mapping_session.close()` to the mapping_server.
        env.close_signal.take().expect("Missing signal").await.expect("Failed to close");

        {
            let cache = env.pager_and_verifier.vmo_cache.map.lock();
            assert!(cache.get(TEST_BLOB_HASH).is_none());
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
                mapping_stream.try_next().await.unwrap()
            {
                let mapping_vmo = zx::Vmo::create(zx::system_get_page_size().into()).unwrap();
                responder.send(Ok(mapping_vmo)).unwrap();
                let mut session_stream = session.into_stream();

                if let Some(fmapping::MappingSessionRequest::Open {
                    responder: _responder, ..
                }) = session_stream.try_next().await.unwrap()
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
                mapper_stream.try_next().await.unwrap()
            {
                responder.send(Ok(())).unwrap();
            }
        });

        let pager_and_verifer = Arc::new(
            BlobPagerAndVerifier::new(&mapping_proxy, &mapper_proxy)
                .await
                .expect("BlobPagerAndVerifier::new failed"),
        );

        let identifier = TEST_BLOB_HASH;
        let verifier_clone = pager_and_verifer.clone();

        let (abortable_future, abort_handle) = futures::future::abortable(async move {
            let _ = verifier_clone.create_vmo(identifier).await;
        });
        let primary_task = fasync::Task::spawn(abortable_future);

        // Wait until the mock mapping session catches the `Open` request and check that the cache
        // state is updated.
        mock_opened_rx.await.expect("Open exited abruptly");
        {
            let cache = pager_and_verifer.vmo_cache.map.lock();
            assert!(matches!(cache.get(identifier), Some(BlobState::Pending(_))));
        }

        // Simulate a failed create_vmo (future abort drops execution context)
        abort_handle.abort();
        let _ = primary_task.await;

        // The cache should be cleared after due to PendingCacheEntryGuard being dropped
        {
            let cache = pager_and_verifer.vmo_cache.map.lock();
            assert!(cache.get(identifier).is_none());
        }

        drop(pager_and_verifer);
        drop(mapping_task);
        drop(mapper_task);
    }

    #[fuchsia::test]
    async fn test_cache_revival_race() {
        let mut env = TestEnv::new().await;

        let child_vmo = env.pager_and_verifier.create_vmo(TEST_BLOB_HASH).await.unwrap();

        // Simulate a concurrent thread having upgraded the cache entry, which bumps the reference
        // count of this blob from 1 to 2 - the Drop implementation for CachedBlob won't run when
        // the receiver receives the `ZX_VMO_ZERO_CHILDREN` signal.
        let second_blob_ref = {
            let cache = env.pager_and_verifier.vmo_cache.map.lock();
            match cache.get(TEST_BLOB_HASH).unwrap() {
                BlobState::Ready(weak) => weak.upgrade().unwrap(),
                _ => panic!("Expected Ready state"),
            }
        };

        // Drop the only open child VMO handle, which instantly fires `ZX_VMO_ZERO_CHILDREN`.
        drop(child_vmo);

        // Yield execution so that the `ZeroChildrenReceiver` async packet processes. The receiver
        // wakes up, and sets `*strong_blob_ref = None;`, but because our test holds
        // `second_blob_ref`, `CachedBlob::drop()` won't run.
        while second_blob_ref.registration.receiver().strong_blob_ref.lock().is_some() {
            fasync::yield_now().await;
        }

        // Now, initiate a brand new client request. It will discover that the CacheLookup is
        // `Ready(Weak)` and successfully upgrade it. Inside `get_or_reserve()`, it will introspect
        // the receiver, find that strong_blob_ref is None, and should update the strong_blob_ref.
        let new_child = env.pager_and_verifier.create_vmo(TEST_BLOB_HASH).await.unwrap();

        // Release our artificial suspension lock.
        drop(second_blob_ref);

        // Verification 1: Cache entry successfully rebuilt strong_blob_ref inside the receiver
        {
            let cache = env.pager_and_verifier.vmo_cache.map.lock();
            match cache.get(TEST_BLOB_HASH).unwrap() {
                BlobState::Ready(weak) => {
                    let strong_blob = weak.upgrade().expect("Failed to rebuild strong reference");
                    let receiver = strong_blob.registration.receiver();
                    let strong_ref = receiver.strong_blob_ref.lock();
                    assert!(strong_ref.is_some(), "Failed to restore the strong blob ref");
                }
                _ => panic!("Expected Ready state"),
            }
        }

        // Verification 2: The background `MappingSession::Close` was never requested because the
        // `CachedBlob::drop()` cycle was circumvented.
        assert_eq!(env.close_signal.as_mut().unwrap().try_recv(), Ok(None));

        // Drop the newly acquired child VMO which triggers the final eviction.
        drop(new_child);
        env.close_signal
            .take()
            .expect("Missing MappingSession::close")
            .await
            .expect("Failed to close");
        env.teardown().await;
    }
}
