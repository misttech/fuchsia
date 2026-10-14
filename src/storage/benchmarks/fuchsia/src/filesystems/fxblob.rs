// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::filesystems::{BlobFilesystem, FsManagementFilesystemInstance};
use async_trait::async_trait;
use blob_pager_and_verifier::BlobPagerAndVerifier;
use fidl_fuchsia_fxfs::{BlobCreatorMarker, BlobCreatorProxy, BlobReaderMarker, BlobReaderProxy};
use fidl_fuchsia_io as fio;
use fidl_fuchsia_storage_block as fblock;
use fidl_fuchsia_storage_mapping as fmapping;
use fuchsia_component::client::{connect_to_protocol_at_dir_root, connect_to_protocol_at_dir_svc};
use fuchsia_merkle::Hash;
use std::path::Path;
use storage_benchmarks::{
    BlockDeviceConfig, BlockDeviceFactory, CacheClearableFilesystem, Filesystem, FilesystemConfig,
};

/// Config object for starting Fxblob instances.
#[derive(Clone, Default)]
pub struct Fxblob {
    use_driver_blob_paging: bool,
}

impl Fxblob {
    pub fn new() -> Self {
        Self { use_driver_blob_paging: false }
    }

    pub fn with_driver_paging() -> Self {
        Self { use_driver_blob_paging: true }
    }
}

#[async_trait]
impl FilesystemConfig for Fxblob {
    type Filesystem = FxblobInstance;

    async fn start_filesystem(
        &self,
        block_device_factory: &dyn BlockDeviceFactory,
    ) -> FxblobInstance {
        let block_device = block_device_factory
            .create_block_device(&BlockDeviceConfig {
                requires_fvm: false,
                use_zxcrypt: false,
                volume_size: Some(104 * 1024 * 1024),
            })
            .await;
        let fxblob = FsManagementFilesystemInstance::new(
            || fs_management::Fxfs { allow_type3_blobs: true, ..Default::default() },
            block_device,
            None,
            /*as_blob=*/ true,
        )
        .await;
        let blob_creator =
            connect_to_protocol_at_dir_svc::<BlobCreatorMarker>(fxblob.exposed_dir())
                .expect("failed to connect to the BlobCreator protocol");
        let blob_reader = connect_to_protocol_at_dir_svc::<BlobReaderMarker>(fxblob.exposed_dir())
            .expect("failed to connect to the BlobReader protocol");
        let pager_and_verifier = if self.use_driver_blob_paging {
            Some(FxblobInstance::create_pager_and_verifier(&fxblob).await)
        } else {
            None
        };
        FxblobInstance { blob_creator, blob_reader, pager_and_verifier, fxblob }
    }

    fn name(&self) -> String {
        if self.use_driver_blob_paging { "fxblob_driver".to_owned() } else { "fxblob".to_owned() }
    }
}

pub struct FxblobInstance {
    blob_creator: BlobCreatorProxy,
    blob_reader: BlobReaderProxy,
    pager_and_verifier: Option<BlobPagerAndVerifier>,
    fxblob: FsManagementFilesystemInstance,
}

pub struct ErofsMount {
    root_dir: fio::DirectoryProxy,
    _parent_vmo: zx::Vmo,
    shutdown_tx: Option<futures::channel::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ErofsMount {
    pub fn root_dir(&self) -> &fio::DirectoryProxy {
        &self.root_dir
    }
}

impl Drop for ErofsMount {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl FxblobInstance {
    async fn create_pager_and_verifier(
        fxblob: &FsManagementFilesystemInstance,
    ) -> BlobPagerAndVerifier {
        let mapping_provider =
            connect_to_protocol_at_dir_svc::<fmapping::MappingProviderMarker>(fxblob.exposed_dir())
                .expect("failed to connect to the MappingProvider protocol");
        let block_proxy = fxblob
            .block_device
            .connector()
            .connect_block()
            .expect("failed to connect to block device")
            .into_proxy();
        let (mapper, mapper_server) = fidl::endpoints::create_proxy::<fblock::MapperMarker>();
        block_proxy
            .connect_mapper(mapper_server)
            .await
            .expect("FIDL error on ConnectMapper")
            .expect("ConnectMapper failed");
        BlobPagerAndVerifier::new(&mapping_provider, &mapper)
            .await
            .expect("failed to initialize BlobPagerAndVerifier")
    }

    pub fn connect_to_debug(&self) -> fidl_fuchsia_fxfs::DebugProxy {
        connect_to_protocol_at_dir_root::<fidl_fuchsia_fxfs::DebugMarker>(
            self.fxblob.exposed_services_dir(),
        )
        .expect("failed to connect to Debug protocol")
    }

    pub async fn mount_erofs(&self, blob_hash: &Hash, file_offset: u64) -> ErofsMount {
        let parent_vmo = self.get_vmo(blob_hash).await;
        let backing_vmo = if file_offset > 0 {
            let vmo_size = parent_vmo.get_size().expect("failed to get parent VMO size");
            parent_vmo
                .create_child(zx::VmoChildOptions::SLICE, file_offset, vmo_size - file_offset)
                .expect("failed to create child SLICE of parent VMO")
        } else {
            parent_vmo
                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                .expect("failed to duplicate parent VMO")
        };

        let (root_dir, root_server) = fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
        let (shutdown_tx, shutdown_rx) = futures::channel::oneshot::channel::<()>();
        let thread = std::thread::spawn(move || {
            let mut executor = fuchsia_async::LocalExecutor::default();
            executor.run_singlethreaded(async move {
                let erofs_pager = std::sync::Arc::new(
                    erofs_component::ErofsPager::new().expect("failed to create ErofsPager"),
                );
                erofs_component::ErofsVolume::serve(
                    backing_vmo,
                    erofs_pager.clone(),
                    fio::PERM_READABLE,
                    root_server,
                )
                .expect("failed to serve ErofsVolume");
                let _ = shutdown_rx.await;
                erofs_pager.unregister_all();
            });
        });

        ErofsMount {
            root_dir,
            _parent_vmo: parent_vmo,
            shutdown_tx: Some(shutdown_tx),
            thread: Some(thread),
        }
    }
}

#[async_trait]
impl Filesystem for FxblobInstance {
    async fn shutdown(self) {
        drop(self.pager_and_verifier);
        self.fxblob.shutdown().await
    }

    fn benchmark_dir(&self) -> &Path {
        self.fxblob.benchmark_dir()
    }
}

#[async_trait]
impl CacheClearableFilesystem for FxblobInstance {
    async fn clear_cache(&mut self) {
        let use_driver_blob_paging = self.pager_and_verifier.take().is_some();
        let () = self.fxblob.clear_cache().await;
        self.blob_creator =
            connect_to_protocol_at_dir_svc::<BlobCreatorMarker>(self.fxblob.exposed_dir())
                .expect("failed to connect to the BlobCreator protocol");
        self.blob_reader =
            connect_to_protocol_at_dir_svc::<BlobReaderMarker>(self.fxblob.exposed_dir())
                .expect("failed to connect to the BlobReader protocol");
        if use_driver_blob_paging {
            self.pager_and_verifier = Some(Self::create_pager_and_verifier(&self.fxblob).await);
        }
    }
}

#[async_trait]
impl BlobFilesystem for FxblobInstance {
    async fn get_vmo(&self, name: &Hash) -> zx::Vmo {
        if let Some(pager) = &self.pager_and_verifier {
            pager.create_vmo(&**name).await.expect("failed to get vmo from BlobPagerAndVerifier")
        } else {
            self.blob_reader()
                .get_vmo(&*name)
                .await
                .expect("transport error on BlobReader.GetVmo")
                .expect("failed to get vmo")
        }
    }

    fn blob_creator(&self) -> &BlobCreatorProxy {
        &self.blob_creator
    }

    fn blob_reader(&self) -> &BlobReaderProxy {
        &self.blob_reader
    }

    fn exposed_dir(&self) -> &fio::DirectoryProxy {
        self.fxblob.exposed_dir()
    }
}

#[cfg(test)]
mod tests {
    use super::Fxblob;
    use crate::filesystems::testing::check_blob_filesystem;

    #[fuchsia::test]
    async fn start_fxblob_new() {
        check_blob_filesystem(Fxblob::new()).await;
    }
}
