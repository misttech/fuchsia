// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fuchsia::errors::map_to_status;
use crate::fuchsia::fxblob::directory::BlobDirectory;
use crate::fuchsia::pager::PagerBacked;
use anyhow::Error;
use fidl_fuchsia_storage_mapping as fmapping;
use fuchsia_merkle::Hash;
use futures::TryStreamExt;
use fxfs::errors::FxfsError;
use log::{error, warn};
use mapping::{
    Extents, MAPPING_VMO_SIZE, MappingCommand, PENDING_COMMANDS_CAPACITY, RawMappingCommand,
};
use std::sync::Arc;
use vmo_fifo::AsyncSender;

/// BlobMappingProvider services requests to open mapping sessions over a given BlobDirectory.
pub struct BlobMappingProvider {
    blob_directory: Arc<BlobDirectory>,
}

impl BlobMappingProvider {
    pub fn new(blob_directory: Arc<BlobDirectory>) -> Result<Self, Error> {
        Ok(Self { blob_directory })
    }

    pub async fn handle_mapping_provider_requests(
        self: Arc<Self>,
        mut stream: fmapping::MappingProviderRequestStream,
    ) {
        while let Ok(Some(request)) = stream.try_next().await {
            match request {
                fmapping::MappingProviderRequest::OpenSession { session, responder } => {
                    let vmo = match zx::Vmo::create(MAPPING_VMO_SIZE) {
                        Ok(v) => v,
                        Err(e) => {
                            let _ = responder.send(Err(e.into_raw()));
                            continue;
                        }
                    };
                    let sender = match AsyncSender::<RawMappingCommand>::new(
                        vmo,
                        8,
                        PENDING_COMMANDS_CAPACITY,
                    ) {
                        Ok(s) => s,
                        Err(e) => {
                            let _ = responder.send(Err(e.into_raw()));
                            continue;
                        }
                    };
                    let vmo_clone = match sender.vmo().duplicate_handle(zx::Rights::SAME_RIGHTS) {
                        Ok(v) => v,
                        Err(e) => {
                            let _ = responder.send(Err(e.into_raw()));
                            continue;
                        }
                    };
                    if let Err(error) = responder.send(Ok(vmo_clone)) {
                        error!(error:?; "Failed to send open session response");
                    } else {
                        let mapping_session =
                            BlobMappingSession::new(self.blob_directory.clone(), sender);
                        self.blob_directory.volume().scope().spawn(async move {
                            mapping_session
                                .handle_mapping_session_requests(session.into_stream())
                                .await;
                        });
                    }
                }
                fmapping::MappingProviderRequest::_UnknownMethod { ordinal, .. } => {
                    warn!(ordinal; "Unknown MappingProvider method");
                }
            }
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct OpenedBlob {
    /// The session-unique identifier for the registered blob.
    pub key: u64,
    /// The uncompressed byte size of the blob.
    pub size: u64,
}

/// `BlobMappingSession` services requests to open or close blobs within a `BlobDirectory`.
/// Upon opening a blob, the blob's underlying extents (data and merkle blocks) are retrieved and
/// then forwarded to client asynchronously via `sender`. A a session-unique `key` is assigned for
/// each opened blob, which the client can reference during a `Close` request.
pub struct BlobMappingSession {
    blob_directory: Arc<BlobDirectory>,
    sender: AsyncSender<RawMappingCommand>,
    next_key: u64,
}

impl BlobMappingSession {
    pub fn new(blob_directory: Arc<BlobDirectory>, sender: AsyncSender<RawMappingCommand>) -> Self {
        Self { blob_directory, sender, next_key: 1 }
    }

    /// Retrieves the extent mappings for the blob and registers the blob in the mapping session.
    /// Returns an `OpenedBlob` containing the node size and the uniquely generated key used to
    /// identify this blob in this session.
    async fn open_blob(&mut self, hash: Hash) -> Result<OpenedBlob, Error> {
        let node = self.blob_directory.open_blob(&hash.into()).await?.ok_or(FxfsError::NotFound)?;

        let extents = node.get_mapping_extents().await?;
        let size = node.as_ref().byte_size();
        let blob_count = extents.data.len() as u32;
        let metadata_count = extents.merkle.len() as u32;

        let key = self.next_key;
        self.next_key += 1;
        let allocation_size = (blob_count + metadata_count) as usize * std::mem::size_of::<u64>();

        if allocation_size > 0 {
            let mut payload = self.sender.reserve_payload(allocation_size).await?;
            let offset_in_vmo = payload.offset();

            for (mut chunk, val_res) in payload.data().chunks_mut(std::mem::size_of::<u64>()).zip(
                Extents::encode_extents_iter(&extents.data)
                    .chain(Extents::encode_extents_iter(&extents.merkle)),
            ) {
                chunk.write(val_res.to_le());
            }

            let command = MappingCommand::Mappings {
                key: key as u64,
                offset: offset_in_vmo as u32,
                metadata_count,
                blob_count,
            };

            payload.commit(command.into()).await?;
        }

        Ok(OpenedBlob { key, size })
    }

    /// Unregisters the blob mapping and signals the block driver to terminate tracking.
    async fn close_blob(&mut self, key: u64) -> Result<(), Error> {
        self.sender.push(MappingCommand::CloseBlob { key }.into()).await?;
        Ok(())
    }

    pub async fn handle_mapping_session_requests(
        mut self,
        mut stream: fmapping::MappingSessionRequestStream,
    ) {
        while let Ok(Some(request)) = stream.try_next().await {
            match request {
                fmapping::MappingSessionRequest::Open { identifier, responder } => {
                    // We expect the identifier to be a Merkle Root Hash for the blob.
                    if identifier.len() != fuchsia_hash::HASH_SIZE {
                        responder.send(Err(zx::Status::INVALID_ARGS.into_raw())).unwrap_or_else(
                            |error| warn!(error:?; "Failed to send mapping session response"),
                        );
                        continue;
                    }
                    let hash = Hash::from(<[u8; 32]>::try_from(identifier.as_slice()).unwrap());
                    match self.open_blob(hash).await {
                        Ok(opened_blob) => {
                            responder
                                .send(Ok((opened_blob.size, opened_blob.key as u32)))
                                .unwrap_or_else(|error| {
                                    warn!(error:?; "Failed to send mapping session response")
                                });
                        }
                        Err(error) => {
                            error!(error:?; "Failed to open blob");
                            responder.send(Err(map_to_status(error).into_raw())).unwrap_or_else(
                                |error| warn!(error:?; "Failed to send mapping session response"),
                            );
                        }
                    }
                }
                fmapping::MappingSessionRequest::Close { key, responder } => {
                    let result = match self.close_blob(key as u64).await {
                        Ok(()) => Ok(()),
                        Err(error) => {
                            error!(error:?; "Failed to close blob");
                            Err(map_to_status(error).into_raw())
                        }
                    };
                    responder.send(result).unwrap_or_else(
                        |error| warn!(error:?; "Failed to send mapping session response"),
                    );
                }
                fmapping::MappingSessionRequest::_UnknownMethod { ordinal, .. } => {
                    warn!(ordinal; "Unknown MappingSession method");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fuchsia::fxblob::testing::{BlobFixture, new_blob_fixture};
    use delivery_blob::CompressionMode;
    use fuchsia_async as fasync;
    use vmo_fifo::Receiver;

    #[fuchsia::test]
    async fn test_blob_mapping_provider() {
        let fixture = new_blob_fixture().await;
        // Test with a large amount of non-compressible data to generate many extents
        let data = vec![42; 300_000];
        let hash = fixture.write_blob(&data, CompressionMode::Never).await;

        let blob_dir = fixture
            .volume()
            .root()
            .clone()
            .as_node()
            .into_any()
            .downcast::<BlobDirectory>()
            .expect("Failed to downcast root directory to BlobDirectory");

        let node = blob_dir
            .open_blob(&hash.into())
            .await
            .expect("Failed to open blob in Fxfs")
            .expect("open_blob returned None instead of node");
        let extents = node.get_mapping_extents().await.expect("Failed to retrieve extents");
        let data_extents = extents.data;
        let merkle_extents = extents.merkle;

        // Un-dropped nodes pin the Blob as actively opened. The unmount routine in fixture.close()
        // will wait forever for this blob to be fully closed, causing a test timeout.
        drop(node);

        let vmo = zx::Vmo::create(MAPPING_VMO_SIZE).unwrap();
        let client_mapping = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let sender =
            AsyncSender::<RawMappingCommand>::new(vmo, 8, PENDING_COMMANDS_CAPACITY).unwrap();

        let mut session = BlobMappingSession::new(blob_dir, sender);

        let receiver_task = fasync::unblock(move || {
            let mut receiver = Receiver::<RawMappingCommand>::new(client_mapping, 256)
                .expect("Failed to create the Receiver wrapper");

            // First Open Command
            let cmd1_raw = receiver.peek().expect("peek failed");
            let cmd1 = MappingCommand::try_from(*cmd1_raw).expect("try_from failed");

            let (cmd1_offset, cmd1_blob_count, cmd1_metadata_count) = match cmd1 {
                MappingCommand::Mappings { key, offset, metadata_count, blob_count } => {
                    assert_eq!(key, 1);
                    assert_eq!(blob_count, data_extents.len() as u32);
                    assert_eq!(metadata_count, merkle_extents.len() as u32);
                    (offset, blob_count, metadata_count)
                }
                _ => panic!("Expected Mappings command"),
            };

            // Verify payload
            let total_extents = cmd1_blob_count + cmd1_metadata_count;
            let buffer = cmd1_raw.payload_slice(cmd1_offset, total_extents * 8).to_vec();

            let mut expected_payload = Vec::new();
            for val in Extents::encode_extents_iter(&data_extents)
                .chain(Extents::encode_extents_iter(&merkle_extents))
            {
                expected_payload.extend_from_slice(&val.to_le_bytes());
            }
            assert_eq!(buffer, expected_payload);
            cmd1_raw.pop().expect("Failed pop_commit");

            let cmd2_raw = receiver.peek().expect("peek failed");
            let cmd2 = MappingCommand::try_from(*cmd2_raw).expect("try_from failed");
            match cmd2 {
                MappingCommand::CloseBlob { key } => assert_eq!(key, 1),
                _ => panic!("Expected CloseBlob command"),
            };
            cmd2_raw.pop().expect("Failed pop_commit");
        });

        let server_task = async move {
            let OpenedBlob { key, .. } = session.open_blob(hash).await.expect("open_blob failed");
            assert_eq!(key, 1);

            session.close_blob(key).await.expect("close_blob failed on existing key");

            std::mem::drop(session);
        };

        futures::join!(receiver_task, server_task);

        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_missing_blob() {
        let fixture = new_blob_fixture().await;
        let blob_dir = fixture
            .volume()
            .root()
            .clone()
            .as_node()
            .into_any()
            .downcast::<BlobDirectory>()
            .expect("Failed to downcast");

        let vmo = zx::Vmo::create(MAPPING_VMO_SIZE).unwrap();
        let sender =
            AsyncSender::<RawMappingCommand>::new(vmo, 8, PENDING_COMMANDS_CAPACITY).unwrap();
        let mut session = BlobMappingSession::new(blob_dir, sender);
        let hash = Hash::from([1u8; 32]);
        session
            .open_blob(hash)
            .await
            .expect_err("open_blob should fail with blob that doesn't exist");

        std::mem::drop(session);
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_invalid_key() {
        let fixture = new_blob_fixture().await;
        let blob_dir = fixture
            .volume()
            .root()
            .clone()
            .as_node()
            .into_any()
            .downcast::<BlobDirectory>()
            .expect("Failed to downcast");

        let vmo = zx::Vmo::create(MAPPING_VMO_SIZE).unwrap();
        let sender =
            AsyncSender::<RawMappingCommand>::new(vmo, 8, PENDING_COMMANDS_CAPACITY).unwrap();
        let mut session = BlobMappingSession::new(blob_dir, sender);
        session.close_blob(42).await.expect("close_blob should return Ok with invalid key");
        std::mem::drop(session);

        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_mapping_provider_and_mapping_session() {
        let fixture = new_blob_fixture().await;
        let data = vec![42; 8192];
        let hash = fixture.write_blob(&data, CompressionMode::Never).await;

        let blob_dir = fixture
            .volume()
            .root()
            .clone()
            .as_node()
            .into_any()
            .downcast::<BlobDirectory>()
            .expect("Failed to downcast root directory to BlobDirectory");

        let scope = blob_dir.volume().scope().clone();
        let server = Arc::new(
            BlobMappingProvider::new(blob_dir).expect("Failed to create BlobMappingProvider"),
        );

        // Spawn the mapping provider stream.
        let (provider_proxy, provider_server_end) =
            fidl::endpoints::create_proxy::<fmapping::MappingProviderMarker>();
        scope.spawn(async move {
            server.handle_mapping_provider_requests(provider_server_end.into_stream()).await;
        });

        // Open a mapping session from the mapping provider
        let (session_proxy, session_server_end) =
            fidl::endpoints::create_proxy::<fmapping::MappingSessionMarker>();
        let _shared_vmo = provider_proxy
            .open_session(session_server_end)
            .await
            .expect("open_session failed")
            .expect("vmo returned an error");

        let id: [u8; 32] = hash.into();
        let (size, key) = session_proxy
            .open(&id)
            .await
            .expect("open failed")
            .expect("open explicitly returned an error");

        assert_eq!(size, 8192);
        assert_eq!(key, 1);

        session_proxy
            .close(key)
            .await
            .expect("close failed")
            .expect("close explicitly returned an error");

        // Test some failures

        // Sending an invalid length hash to open should return INVALID_ARGS
        let bad_length_id = vec![1u8, 2, 3];
        let invalid_args_err =
            session_proxy.open(&bad_length_id).await.expect("open wire call failed").unwrap_err();
        assert_eq!(invalid_args_err, zx::Status::INVALID_ARGS.into_raw());

        // Sending a valid length hash that does not exist should return NOT_FOUND
        let not_found_id = [0u8; 32];
        let not_found_err =
            session_proxy.open(&not_found_id).await.expect("open wire call failed").unwrap_err();
        assert_eq!(not_found_err, zx::Status::NOT_FOUND.into_raw());

        // Trying to close an invalid blob key currently always succeeds. The BlobMappingServer
        // unconditionally passes the command down the FIFO queue to the block driver and doesn't
        // explicitly track active connections.
        session_proxy.close(123).await.expect("close wire call failed").expect("close failed");

        fixture.close().await;
    }
}
