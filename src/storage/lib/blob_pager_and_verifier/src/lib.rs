// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Error};
use fidl_fuchsia_storage_block as fblock;
use fidl_fuchsia_storage_mapping as fmapping;

/// Coordinates between FxBlob, the block driver, and the kernel to manage pager-backed blobs such
/// that page requests can be handled at the driver instead of the filesystem layer.
///
/// `BlobPagerAndVerifier` uses a dedicated Pager to create pager-backed VMOs for blobs. Kernel page
/// faults on these VMOs are routed directly to the block driver, which reads and decompresses the
/// raw data. The driver then passes this data back to `BlobPagerAndVerifier` for cryptographic
/// verification and page fault resolution.
pub struct BlobPagerAndVerifier {
    mapping_session: fmapping::MappingSessionProxy,
    _mapper_session: fblock::MapperSessionProxy,
    port: zx::Port,
    pager: zx::Pager,
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
            .map_err(|e| anyhow::anyhow!("MappingProvider open_session failed: {e:?}"))?;

        let port = zx::Port::create();
        let delivery_queue = zx::Vmo::create(0).context("Failed to create delivery queue VMO")?;

        let pager =
            zx::Pager::create(zx::PagerOptions::empty()).context("Failed to create pager")?;

        let (mapper_session, mapper_session_server) =
            fidl::endpoints::create_proxy::<fblock::MapperSessionMarker>();

        let port_dup = port.duplicate_handle(zx::Rights::SAME_RIGHTS)?;

        mapper
            .open_session(
                mapper_session_server,
                mapping_vmo,
                None, // mapping_offset
                port_dup,
                delivery_queue,
            )
            .await
            .context("FIDL error calling Mapper.OpenSession")?
            .map_err(|e| anyhow::anyhow!("Mapper.OpenSession failed: {e:?}"))?;

        Ok(Self { mapping_session, _mapper_session: mapper_session, port, pager })
    }

    // TODO(https://fxbug.dev/535489428): Add support for concurrent access.
    /// Create pager owned VMO for the blob identified by its Merkle Root Hash.
    pub async fn create_vmo(&self, identifier: &[u8]) -> Result<zx::Vmo, Error> {
        // Ask Fxfs to register the blob and write its extent mappings into the shared mapping VMO,
        // returning a key that is used to generate the pager-backed VMO.
        let (size, key) = self
            .mapping_session
            .open(identifier)
            .await
            .context("FIDL error calling MappingSession.Open")?
            .map_err(|e| anyhow::anyhow!("MappingSession.Open failed for blob: {e:?}"))?;

        // Create with VmoOptions::empty to specify a fixed-size VMO.
        let vmo = self.pager.create_vmo(zx::VmoOptions::empty(), &self.port, key as u64, size)?;

        Ok(vmo)
    }

    // TODO(https://fxbug.dev/535489428): Watch for data delivery events on the delivery queue VMO,
    // cryptographically verify the payloads placed into it by the block driver, and finalize the
    // page fault via `zx_pager_supply_pages`.
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::TryStreamExt;

    async fn setup_mock_servers() -> (
        fmapping::MappingProviderProxy,
        fblock::MapperProxy,
        fuchsia_async::Task<()>,
        fuchsia_async::Task<()>,
    ) {
        let (mapping_proxy, mut mapping_stream) =
            fidl::endpoints::create_proxy_and_stream::<fmapping::MappingProviderMarker>();

        let mapping_task = fuchsia_async::Task::spawn(async move {
            if let Some(fmapping::MappingProviderRequest::OpenSession { session, responder }) =
                mapping_stream.try_next().await.unwrap()
            {
                let mapping_vmo = zx::Vmo::create(zx::system_get_page_size().into()).unwrap();
                responder.send(Ok(mapping_vmo)).unwrap();

                let mut session_stream = session.into_stream();
                while let Some(request) = session_stream.try_next().await.unwrap() {
                    match request {
                        fmapping::MappingSessionRequest::Open { identifier, responder } => {
                            assert_eq!(identifier, b"test_blob_hash");
                            responder.send(Ok((10_000, 123))).unwrap();
                        }
                        _ => {}
                    }
                }
            }
        });

        let (mapper_proxy, mut mapper_stream) =
            fidl::endpoints::create_proxy_and_stream::<fblock::MapperMarker>();

        let mapper_task = fuchsia_async::Task::spawn(async move {
            if let Some(fblock::MapperRequest::OpenSession { responder, .. }) =
                mapper_stream.try_next().await.unwrap()
            {
                responder.send(Ok(())).unwrap();
            }
        });

        (mapping_proxy, mapper_proxy, mapping_task, mapper_task)
    }

    #[fuchsia::test]
    async fn test_create_vmo() {
        let (mapping_proxy, mapper_proxy, mapping_task, mapper_task) = setup_mock_servers().await;

        let pager_and_verifer = BlobPagerAndVerifier::new(&mapping_proxy, &mapper_proxy)
            .await
            .expect("BlobPagerAndVerifier::new failed");

        let identifier = b"test_blob_hash";
        let vmo = pager_and_verifer.create_vmo(identifier).await.expect("Failed to create VMO");
        let size = vmo.get_size().unwrap();

        // Pager VMO sizes are rounded up to the nearest page boundary.
        let page_size = zx::system_get_page_size() as u64;
        let expected_pages = (10_000 + page_size - 1) / page_size;
        assert_eq!(size, expected_pages * page_size);

        // Close the connection and wait for the mock servers to tear down
        drop(pager_and_verifer);
        mapping_task.await;
        mapper_task.await;
    }
}
