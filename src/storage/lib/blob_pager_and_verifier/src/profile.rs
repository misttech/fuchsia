// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::Cache;
use fidl_fuchsia_storage_block as fblock;
use fuchsia_async as fasync;
use fuchsia_hash::Hash;
use futures::future::{BoxFuture, join_all};
use std::collections::{HashMap, HashSet, hash_map};
use std::sync::Arc;
use storage_profile::{BlobMessage, FileMessage, Message};

// Matches the 4 KiB block alignment used by Fxfs on-disk profile files and `mapping::BLOCK_SIZE`
// in the block driver (`BlobMessage` and `FileMessage` entries are packed into 4 KiB blocks
// without crossing block boundaries).
const PROFILE_BLOCK_SIZE: usize = 4096;

// Matches `REPLAY_THREADS` in Fxfs's `profile.rs`: two blocking threads allow overlapping
// `ZX_VMO_OP_PREFETCH` page-in latency without saturating the block driver or executor pool.
const REPLAY_THREADS: usize = 2;

struct ReplayRequest {
    vmo: Arc<zx::Vmo>,
    offset: u64,
}

struct ReplayHandle {
    replay_threads: BoxFuture<'static, ()>,
    // Holds the task that opens blobs and keeps their child VMOs cached in `local_cache` during
    // replay. Dropping `ReplayHandle` drops this `fasync::Task`, which aborts the task and drops
    // `local_cache` (allowing `Cache` to evict unused blobs) and `sender`.
    _cache_task: fasync::Task<()>,
}

impl ReplayHandle {
    fn start(cache: &Arc<Cache>, replay_vmo: zx::Vmo) -> Result<Self, zx::Status> {
        let messages =
            BlobMessage::decode_from_vmo(replay_vmo, PROFILE_BLOCK_SIZE).map_err(|error| {
                log::warn!(error:?; "Failed to decode replay profile VMO");
                if error == zx::Status::OUT_OF_RANGE {
                    zx::Status::OUT_OF_RANGE
                } else {
                    zx::Status::IO_DATA_INTEGRITY
                }
            })?;

        let (sender, receiver) = async_channel::unbounded::<ReplayRequest>();

        // Create async_channel. An async task reads the profile, opens the blobs, and populates
        // the channel, then `REPLAY_THREADS` blocking threads consume it and prefetch pages.
        let mut replay_threads = Vec::with_capacity(REPLAY_THREADS);
        let page_size = zx::system_get_page_size() as u64;
        for _ in 0..REPLAY_THREADS {
            let receiver = receiver.clone();
            replay_threads.push(fasync::unblock(move || {
                while let Ok(request) = receiver.recv_blocking() {
                    if let Err(error) =
                        request.vmo.op_range(zx::VmoOp::PREFETCH, request.offset, page_size)
                    {
                        log::warn!(
                            error:?,
                            offset = request.offset;
                            "Failed to prefetch blob page during profile replay"
                        );
                    }
                    // `sender.close()` below closes the channel while keeping `sender` alive
                    // (`sender_count() == 1`) so replay threads finish draining queued requests.
                    // If `ReplayHandle` is dropped early on abort, `_cache_task` is dropped along
                    // with `sender` (`sender_count() == 0`), causing replay threads to exit here.
                    if receiver.sender_count() == 0 {
                        return;
                    }
                }
            }));
        }
        let replay_threads = Box::pin(async move {
            join_all(replay_threads).await;
        });

        let cache_clone = cache.clone();
        let cache_task = cache
            .scope
            .spawn(async move {
                // Hold the blobs in cache until replay is stopped. Optional as None indicates
                // that the blob could not be opened, and we want to cache that failure.
                let mut local_cache: HashMap<Hash, Option<Arc<zx::Vmo>>> = HashMap::new();
                for msg in messages {
                    if msg.is_open_marker() {
                        continue;
                    }
                    let vmo = match local_cache.entry(msg.id) {
                        hash_map::Entry::Occupied(entry) => match entry.get() {
                            Some(vmo) => vmo.clone(),
                            // Found a cached error.
                            None => continue,
                        },
                        hash_map::Entry::Vacant(entry) => {
                            match cache_clone.open_vmo(&msg.id.into()).await {
                                Ok(vmo) => {
                                    let vmo = Arc::new(vmo);
                                    entry.insert(Some(vmo.clone()));
                                    vmo
                                }
                                Err(error) => {
                                    log::debug!(
                                        error:?,
                                        hash:? = msg.id;
                                        "Failed to open blob from profile replay"
                                    );
                                    // Cache the error.
                                    entry.insert(None);
                                    continue;
                                }
                            }
                        }
                    };
                    if let Err(error) = sender.send(ReplayRequest { vmo, offset: msg.offset }).await
                    {
                        log::warn!(error:?; "Failed to send request to replay thread");
                        break;
                    }
                }
                drop(cache_clone);
                sender.close();
                // Keep `local_cache` alive so replayed blobs stay cached until replay is stopped.
                let () = std::future::pending().await;
            })
            .into();

        Ok(Self { replay_threads, _cache_task: cache_task })
    }
}

/// An active profile recording and replay session driven by [`crate::BlobPagerAndVerifier`].
///
/// If dropped without calling [`ProfileRecording::stop`], any in-flight replay and driver
/// recording are automatically aborted without writing a profile to the filesystem.
pub struct ProfileRecording {
    name: String,
    cache: Arc<Cache>,
    recording_proxy: Option<fblock::RecordingProxy>,
    replay_handle: Option<ReplayHandle>,
}

impl std::fmt::Debug for ProfileRecording {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfileRecording")
            .field("name", &self.name)
            .field("has_replay", &self.replay_handle.is_some())
            .finish_non_exhaustive()
    }
}

impl ProfileRecording {
    pub(crate) async fn start(
        cache: Arc<Cache>,
        mapper_session: &fblock::MapperSessionProxy,
        name: String,
    ) -> Result<Self, zx::Status> {
        cache.start_recording()?;
        // Construct `Self` immediately so `Drop` calls `self.cache.abort_recording()` if any
        // subsequent step fails or if the `start` future is cancelled at an `.await` point.
        let mut this = Self { name, cache, recording_proxy: None, replay_handle: None };

        let (recording_proxy, recording_server) =
            fidl::endpoints::create_proxy::<fblock::RecordingMarker>();
        match mapper_session.record_profile(recording_server).await {
            Ok(Ok(())) => {}
            Ok(Err(raw_status)) => return Err(zx::Status::err_from_raw(raw_status)),
            Err(error) => {
                log::warn!(error:?; "FIDL error calling MapperSession.RecordProfile");
                return Err(zx::Status::INTERNAL);
            }
        }
        this.recording_proxy = Some(recording_proxy);

        let replay_vmo = match this.cache.mapping_session.get_profile(&this.name).await {
            Ok(Ok(vmo)) => Some(vmo),
            Ok(Err(raw_status)) => {
                let status = zx::Status::err_from_raw(raw_status);
                if status == zx::Status::NOT_FOUND {
                    None
                } else {
                    log::warn!(
                        status:?,
                        name = this.name.as_str();
                        "MappingSession.GetProfile failed"
                    );
                    return Err(status);
                }
            }
            Err(error) => {
                log::warn!(
                    error:?,
                    name = this.name.as_str();
                    "FIDL error calling MappingSession.GetProfile"
                );
                return Err(zx::Status::INTERNAL);
            }
        };

        if let Some(vmo) = replay_vmo {
            this.replay_handle = Some(ReplayHandle::start(&this.cache, vmo)?);
        }

        Ok(this)
    }

    /// Stops any active replay, finalizes the driver recording, filters by blobs opened during the
    /// recording window, and writes the encoded profile to the filesystem via `MappingSession`.
    pub async fn stop(mut self) -> Result<(), zx::Status> {
        if let Some(replay_handle) = self.replay_handle.take() {
            replay_handle.replay_threads.await;
        }

        let recording_proxy = self.recording_proxy.take().ok_or(zx::Status::BAD_STATE)?;
        let stop_res = recording_proxy.stop().await;
        let active_recording = self.cache.take_recording().ok_or(zx::Status::BAD_STATE)?;

        let driver_vmo = match stop_res {
            Ok(Ok(vmo)) => vmo,
            Ok(Err(raw_status)) => return Err(zx::Status::err_from_raw(raw_status)),
            Err(error) => {
                log::warn!(error:?; "FIDL error calling Recording.Stop");
                return Err(zx::Status::INTERNAL);
            }
        };

        let file_messages =
            FileMessage::decode_from_vmo(driver_vmo, PROFILE_BLOCK_SIZE).map_err(|error| {
                if error == zx::Status::OUT_OF_RANGE {
                    log::warn!("Driver profile VMO exceeds maximum allowed size");
                    zx::Status::OUT_OF_RANGE
                } else {
                    log::warn!(error:?; "Failed to decode driver profile VMO");
                    zx::Status::IO_DATA_INTEGRITY
                }
            })?;

        let mut seen = HashSet::new();
        let blob_messages = file_messages
            .filter_map(|msg| {
                let &hash = active_recording.hash_by_key.get(&msg.id)?;
                active_recording
                    .opened_hashes
                    .contains(&hash)
                    .then_some(BlobMessage { id: hash, offset: msg.offset })
            })
            .filter(|&blob_msg| seen.insert(blob_msg));

        let recorded_vmo = BlobMessage::encode_to_vmo(blob_messages, PROFILE_BLOCK_SIZE)?;

        match self.cache.mapping_session.write_profile(&self.name, recorded_vmo).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(raw_status)) => {
                let status = zx::Status::err_from_raw(raw_status);
                log::warn!(
                    status:?,
                    name = self.name.as_str();
                    "MappingSession.WriteProfile failed"
                );
                Err(status)
            }
            Err(error) => {
                log::warn!(
                    error:?,
                    name = self.name.as_str();
                    "FIDL error calling MappingSession.WriteProfile"
                );
                Err(zx::Status::INTERNAL)
            }
        }
    }
}

impl Drop for ProfileRecording {
    fn drop(&mut self) {
        self.cache.abort_recording();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BlobPagerAndVerifier;
    use fidl_fuchsia_storage_mapping as fmapping;
    use fuchsia_sync::Mutex;
    use futures::channel::oneshot;
    use futures::{StreamExt, TryStreamExt};
    use mapping::{DeliveryCommand, RawDeliveryCommand};
    use vmo_fifo::SyncSender;

    #[fuchsia::test]
    async fn test_profile_recorder_replay_filtering_and_eviction() {
        let (mapping_proxy, mut mapping_stream) =
            fidl::endpoints::create_proxy_and_stream::<fmapping::MappingProviderMarker>();
        let (mapper_proxy, mut mapper_stream) =
            fidl::endpoints::create_proxy_and_stream::<fblock::MapperMarker>();

        let blob_data = vec![0x42u8; 8192];
        let (root1, _) = fuchsia_merkle::MerkleRootBuilder::new(Vec::new()).complete(&blob_data);
        let hash1: [u8; 32] = root1.into();
        let blob_data2 = vec![0x24u8; 8192];
        let (root2, _) = fuchsia_merkle::MerkleRootBuilder::new(Vec::new()).complete(&blob_data2);
        let hash2: [u8; 32] = root2.into();
        let blob_data3 = vec![0x99u8; 8192];
        let (root3, _) = fuchsia_merkle::MerkleRootBuilder::new(Vec::new()).complete(&blob_data3);
        let hash3: [u8; 32] = root3.into();
        let missing_hash: [u8; 32] = [0xde; 32];

        // Replay profile contains `missing_hash` (twice, to verify negative lookup caching),
        // `hash1` (which a client will also open), and `hash2` (replay-only, never opened by a
        // client).
        let replay_entries = vec![
            BlobMessage { id: Hash::from(missing_hash), offset: 0 },
            BlobMessage { id: Hash::from(missing_hash), offset: 4096 },
            BlobMessage { id: Hash::from(hash1), offset: 0 },
            BlobMessage { id: Hash::from(hash2), offset: 0 },
        ];
        let next_replay_vmo = Arc::new(Mutex::new(Some(
            BlobMessage::encode_to_vmo(&replay_entries, PROFILE_BLOCK_SIZE)
                .expect("encode_to_vmo failed"),
        )));
        let next_replay_vmo_for_server = next_replay_vmo.clone();

        let (written_tx, written_rx) = oneshot::channel();
        let (close_tx, mut close_rx) = futures::channel::mpsc::unbounded();
        let opened_keys = Arc::new(Mutex::new(HashMap::<[u8; 32], Vec<u64>>::new()));
        let opened_keys_clone = opened_keys.clone();

        let mapping_task = fasync::Task::spawn(async move {
            let mut written_tx = Some(written_tx);
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
                        fmapping::MappingSessionRequest::GetProfile { name, responder } => {
                            assert_eq!(name, ".boot");
                            match next_replay_vmo_for_server.lock().take() {
                                Some(vmo) => responder.send(Ok(vmo)).expect("send failed"),
                                None => responder
                                    .send(Err(zx::Status::NOT_FOUND.into_raw()))
                                    .expect("send failed"),
                            }
                        }
                        fmapping::MappingSessionRequest::WriteProfile {
                            name,
                            profile_vmo,
                            responder,
                        } => {
                            assert_eq!(name, ".boot");
                            if let Some(tx) = written_tx.take() {
                                let _ = tx.send(profile_vmo);
                            }
                            responder.send(Ok(())).expect("send failed");
                        }
                        fmapping::MappingSessionRequest::Open { key, identifier, responder } => {
                            let arr: [u8; 32] =
                                identifier.as_slice().try_into().expect("invalid hash slice");
                            opened_keys_clone.lock().entry(arr).or_default().push(key);
                            if arr == missing_hash {
                                responder
                                    .send(Err(zx::Status::NOT_FOUND.into_raw()))
                                    .expect("send failed");
                            } else {
                                responder.send(Ok(8192)).expect("send failed");
                            }
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

        let (tx_delivery, rx_delivery) = oneshot::channel();
        let (recording_dropped_tx, recording_dropped_rx) = oneshot::channel();
        let opened_keys_for_mapper = opened_keys.clone();
        let mapper_task = fasync::Task::spawn(async move {
            let mut recording_dropped_tx = Some(recording_dropped_tx);
            if let Some(fblock::MapperRequest::OpenSession {
                session,
                port,
                delivery_queue,
                responder,
                ..
            }) = mapper_stream.try_next().await.expect("try_next failed")
            {
                let port = port.expect("expected port");
                let delivery_vmo = delivery_queue.expect("expected delivery_queue");
                tx_delivery
                    .send(
                        delivery_vmo
                            .duplicate_handle(zx::Rights::SAME_RIGHTS)
                            .expect("duplicate_handle failed"),
                    )
                    .expect("send failed");
                responder.send(Ok(())).expect("send failed");

                // Spawn a mock pager thread on `port` that supplies pages when `ZX_VMO_OP_PREFETCH`
                // triggers a page request during replay.
                let opened_keys_for_pager = opened_keys_for_mapper.clone();
                std::thread::spawn(move || {
                    let mut sender = SyncSender::<RawDeliveryCommand>::new(
                        delivery_vmo,
                        zx::system_get_page_size() as usize,
                        mapping::PENDING_DELIVERY_COMMANDS_CAPACITY,
                    )
                    .expect("SyncSender::new failed");
                    while let Ok(packet) = port.wait(zx::MonotonicInstant::INFINITE) {
                        if let zx::PacketContents::Pager(pager_packet) = packet.contents() {
                            if pager_packet.command()
                                == zx::sys::zx_page_request_command_t::ZX_PAGER_VMO_READ
                            {
                                let key = packet.key();
                                let range = pager_packet.range();
                                let reg_payload =
                                    sender.reserve_payload(0).expect("reserve_payload failed");
                                let reg_cmd: RawDeliveryCommand = DeliveryCommand::RegisterBlob {
                                    key,
                                    offset: reg_payload.offset(),
                                    length: 0,
                                    read_alignment: 8192,
                                }
                                .into();
                                reg_payload.commit(reg_cmd).expect("commit failed");

                                let data = if opened_keys_for_pager
                                    .lock()
                                    .get(&hash1)
                                    .is_some_and(|ks| ks.contains(&key))
                                {
                                    &blob_data
                                } else {
                                    &blob_data2
                                };
                                let mut payload = sender
                                    .reserve_payload(data.len())
                                    .expect("reserve_payload failed");
                                payload.data().copy_from_slice(data);
                                let cmd: RawDeliveryCommand = DeliveryCommand::Data {
                                    key,
                                    offset: payload.offset(),
                                    length: data.len() as u32,
                                    target_offset: range.start,
                                }
                                .into();
                                payload.commit(cmd).expect("commit failed");
                            }
                        }
                    }
                });

                let mut session_stream = session.into_stream();
                while let Some(request) = session_stream.try_next().await.expect("try_next failed")
                {
                    if let fblock::MapperSessionRequest::RecordProfile { recording, responder } =
                        request
                    {
                        responder.send(Ok(())).expect("send failed");
                        let mut rec_stream = recording.into_stream();
                        match rec_stream.try_next().await.expect("try_next failed") {
                            Some(fblock::RecordingRequest::Stop { responder }) => {
                                let keys = opened_keys_for_mapper.lock().clone();
                                let k1 = keys.get(&hash1).expect("hash1 keys missing")[0];
                                let k2 = keys.get(&hash2).expect("hash2 keys missing")[0];
                                let k3_first = keys.get(&hash3).expect("hash3 keys missing")[0];
                                let k3_second = keys.get(&hash3).expect("hash3 keys missing")[1];
                                let driver_entries = vec![
                                    FileMessage { id: k1, offset: 0 },
                                    FileMessage { id: k2, offset: 0 },
                                    FileMessage { id: k3_first, offset: 0 },
                                    // Duplicate `(hash3, 0)` under the reopened key `k3_second`
                                    FileMessage { id: k3_second, offset: 0 },
                                    FileMessage { id: k3_second, offset: 4096 },
                                ];
                                let vmo =
                                    FileMessage::encode_to_vmo(&driver_entries, PROFILE_BLOCK_SIZE)
                                        .expect("encode_to_vmo failed");
                                responder.send(Ok(vmo)).expect("send failed");
                            }
                            None => {
                                if let Some(tx) = recording_dropped_tx.take() {
                                    let _ = tx.send(());
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        });

        let pager_and_verifier = Arc::new(
            BlobPagerAndVerifier::new(&mapping_proxy, &mapper_proxy)
                .await
                .expect("BlobPagerAndVerifier::new failed"),
        );
        let _delivery_vmo = rx_delivery.await.expect("rx_delivery failed");

        let recording =
            pager_and_verifier.start_profile(".boot").await.expect("start_profile failed");

        // Concurrent `start_profile` while recording is active must fail with `BAD_STATE`.
        let err = pager_and_verifier
            .start_profile(".boot")
            .await
            .expect_err("Concurrent start_profile succeeded");
        assert_eq!(err, zx::Status::BAD_STATE);

        // Wait for the failed open of `missing_hash` to clean up its key reservation via `Close`.
        let missing_closed_key = close_rx.next().await.expect("expected close for missing_hash");
        assert_eq!(
            missing_closed_key,
            opened_keys.lock().get(&missing_hash).expect("missing_hash key missing")[0]
        );

        // Client opens `hash1` and `hash3` (leaving `hash2` unopened by any client).
        let vmo1 = pager_and_verifier.create_vmo(&hash1).await.expect("create_vmo hash1 failed");
        let vmo3 = pager_and_verifier.create_vmo(&hash3).await.expect("create_vmo hash3 failed");

        // Evict `hash3` and re-open it before `stop()` to verify `hash_by_key` retains evicted
        // keys and deduplicates `(hash3, 0)` across keys.
        drop(vmo3);
        let _ = close_rx.next().await.expect("expected close for first vmo3");
        let vmo3_reopened =
            pager_and_verifier.create_vmo(&hash3).await.expect("re-open hash3 failed");
        drop(vmo3_reopened);
        let _ = close_rx.next().await.expect("expected close for second vmo3");

        // Stop recording: this waits for replay to finish, unpins `hash2` (causing its eviction),
        // filters out `hash2`, retains `hash1` and `hash3`, deduplicates `(hash3, 0)`, and writes
        // the profile VMO via `MappingSession.WriteProfile`.
        recording.stop().await.expect("recording.stop failed");
        let recorded_vmo = written_rx.await.expect("written_rx failed");

        // Verify negative lookup caching: `missing_hash` appeared twice in `replay_entries`, so
        // `MappingSession.Open` must have been called only once for it.
        assert_eq!(
            opened_keys.lock().get(&missing_hash).expect("missing_hash key missing").len(),
            1
        );

        // `hash2` was only opened by replay and must be evicted upon `stop()`.
        let closed_key = close_rx.next().await.expect("expected close for unpinned replay blob");
        assert_eq!(closed_key, opened_keys.lock().get(&hash2).expect("hash2 key missing")[0]);

        let decoded: Vec<_> = BlobMessage::decode_from_vmo(recorded_vmo, PROFILE_BLOCK_SIZE)
            .expect("decode_from_vmo failed")
            .collect();
        assert_eq!(
            decoded,
            vec![
                BlobMessage { id: Hash::from(hash1), offset: 0 },
                BlobMessage { id: Hash::from(hash3), offset: 0 },
                BlobMessage { id: Hash::from(hash3), offset: 4096 },
            ]
        );

        // Start a second recording and drop `ProfileRecording` without calling `stop()` to verify
        // that the driver recording is aborted and replay-pinned blobs are unpinned.
        *next_replay_vmo.lock() = Some(
            BlobMessage::encode_to_vmo(
                &[BlobMessage { id: Hash::from(hash2), offset: 0 }],
                PROFILE_BLOCK_SIZE,
            )
            .expect("encode_to_vmo failed"),
        );
        let recording2 =
            pager_and_verifier.start_profile(".boot").await.expect("second start_profile failed");
        drop(recording2);
        recording_dropped_rx.await.expect("expected driver Recording channel to close on abort");

        drop(vmo1);
        let _ = close_rx.next().await.expect("expected close for vmo1 or aborted replay");
        drop(pager_and_verifier);
        mapping_task.await;
        mapper_task.await;
    }
}
