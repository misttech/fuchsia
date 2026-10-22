// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fuchsia::errors::map_to_status;
use crate::fuchsia::file::FxFile;
use crate::fuchsia::volume::FxVolume;
use anyhow::Error;
use fidl_fuchsia_fxfs as ffxfs;
use fidl_fuchsia_io as fio;
use fuchsia_sync::Mutex;
use futures::StreamExt;
use fxfs::log::error;
use fxfs::object_store::ObjectStore;
use std::sync::Arc;

pub type UnmountFn =
    Box<dyn FnOnce() -> futures::future::BoxFuture<'static, Result<(), Error>> + Send>;

/// `FxRemoteVolume` manages a remotely served Fxfs volume for in-process co-location (e.g.
/// Starnix). It handles the `fuchsia.fxfs.RemoteVolume` FIDL interface.
pub struct FxRemoteVolume {
    store: Arc<ObjectStore>,
    volume: Option<Arc<FxVolume>>,
    unmount_fn: Mutex<Option<UnmountFn>>,
    scope: fuchsia_async::Scope,
}

#[fxfs_trace::trace]
impl FxRemoteVolume {
    pub fn new(
        store: Arc<ObjectStore>,
        volume: Option<Arc<FxVolume>>,
        unmount_fn: Option<UnmountFn>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            volume,
            unmount_fn: Mutex::new(unmount_fn),
            scope: fuchsia_async::Scope::new_with_name("fxfs-remote-vol"),
        })
    }

    pub fn store(&self) -> &Arc<ObjectStore> {
        &self.store
    }

    pub fn serve(self: &Arc<Self>, stream: ffxfs::RemoteVolumeRequestStream) {
        let this = self.clone();
        self.scope.spawn(async move {
            this.handle_requests(stream).await;
        });
    }

    pub fn shutdown(&self) {
        let _ = self.scope.to_handle().cancel();
    }

    pub async fn wait(&self) {
        self.scope.on_no_tasks().await;
    }

    /// Returns the persistent layers from the ObjectStore's LSM tree as read-only VMOs.
    pub async fn get_layer_set(&self) -> Result<ffxfs::LayerSet, Error> {
        let layer_set = self.store.tree().immutable_layer_set();
        let mut layer_files = Vec::new();
        let rights =
            zx::Rights::BASIC | zx::Rights::READ | zx::Rights::MAP | zx::Rights::GET_PROPERTY;
        for layer in layer_set.layers.iter() {
            if let Some(handle) = layer.handle() {
                let object_id = handle.object_id();
                let vmo = if let Some(vmo) = handle.get_vmo() {
                    vmo.duplicate_handle(rights)?
                } else {
                    let size = handle.get_size();
                    let vmo = zx::Vmo::create(size)?;
                    vmo.set_content_size(&size)?;
                    if size > 0 {
                        let aligned_size = handle.block_size().align_up(size).unwrap() as usize;
                        let mut buf = handle.allocate_buffer(aligned_size).await;
                        let read_len = handle.read_aligned(0, buf.as_mut()).await?;
                        vmo.write(&buf.subslice(..read_len).to_vec(), 0)?;
                    }
                    vmo.replace_handle(rights)?
                };
                layer_files.push(ffxfs::LayerFile { object_id, vmo });
            }
        }
        Ok(ffxfs::LayerSet { layers: layer_files })
    }

    async fn unmount(&self) -> Result<(), Error> {
        let unmount_fn = self.unmount_fn.lock().take();
        if let Some(unmount_fn) = unmount_fn {
            unmount_fn().await?;
        } else if let Some(volume) = &self.volume {
            volume.terminate().await;
        }
        Ok(())
    }

    /// Dispatches FIDL requests for `fuchsia.fxfs.RemoteVolume`.
    pub async fn handle_requests(self: Arc<Self>, mut stream: ffxfs::RemoteVolumeRequestStream) {
        let mut unmounted = false;
        while let Some(request) = stream.next().await {
            match request {
                Ok(ffxfs::RemoteVolumeRequest::OpenFile { object_id, file, responder }) => {
                    let res: Result<fio::FileInfo, zx::Status> = async {
                        let volume = self.volume.as_ref().ok_or(zx::Status::NOT_SUPPORTED)?;
                        // TODO: Support unnamed temporary files (which are in the graveyard).
                        let opened_node =
                            volume.open_file_by_id(object_id).await.map_err(map_to_status)?;
                        let stream = zx::Stream::create(
                            zx::StreamOptions::MODE_READ | zx::StreamOptions::MODE_WRITE,
                            opened_node.vmo(),
                            0,
                        )?;
                        let client_stream = stream.duplicate_handle(zx::Rights::SAME_RIGHTS)?;
                        let file_info =
                            fio::FileInfo { stream: Some(client_stream), ..Default::default() };
                        let scope = volume.scope().clone();
                        let flags =
                            fio::Flags::PROTOCOL_FILE | fio::PERM_READABLE | fio::PERM_WRITABLE;
                        let mut request = vfs::ObjectRequest::new(
                            flags,
                            &fio::Options::default(),
                            file.into_channel(),
                        );
                        FxFile::create_connection_with_stream_async(
                            opened_node,
                            scope,
                            flags,
                            (&mut request).into(),
                            stream,
                        )
                        .await?;
                        Ok(file_info)
                    }
                    .await;
                    let _ = responder.send(res.map_err(|s| s.into_raw()));
                }

                Ok(ffxfs::RemoteVolumeRequest::Close { responder }) => {
                    let res = self.unmount().await;
                    let _ = responder.send(res.map_err(|e| map_to_status(e).into_raw()));
                    // NOTE: After calling this, this task might be dropped at the next await
                    // point.
                    self.shutdown();
                    unmounted = true;
                    break;
                }

                Err(e) => {
                    error!(e:?; "RemoteVolume request stream error");
                    break;
                }
            }
        }
        if !unmounted {
            let _ = self.unmount().await;
            self.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl::endpoints::create_proxy;
    use fxfs::filesystem::FxFilesystemBuilder;
    use fxfs::object_handle::{ObjectHandle, WriteObjectHandle};
    use fxfs::object_store::transaction::Options;
    use fxfs::object_store::volume::root_volume;
    use fxfs_insecure_crypto::new_insecure_crypt;
    use storage_device::DeviceHolder;
    use storage_device::fake_device::FakeDevice;

    fn new_test_fs_builder() -> FxFilesystemBuilder {
        FxFilesystemBuilder::new()
    }

    #[fuchsia::test]
    async fn test_remote_volume_layer_set() {
        let device = DeviceHolder::new(FakeDevice::new(16384, 512));
        let fs = new_test_fs_builder().format(true).open(device).await.unwrap();
        let root_vol = root_volume(fs.clone()).await.unwrap();
        let store = root_vol.new_volume("test_remote_vol", Default::default()).await.unwrap();

        let mut transaction = store
            .new_transaction(
                fxfs::object_store::transaction::LockKeys::default(),
                Options::default(),
            )
            .await
            .unwrap();
        let _file = ObjectStore::create_object(
            &store,
            &mut transaction,
            fxfs::object_store::HandleOptions::default(),
            None,
        )
        .await
        .unwrap();
        transaction.commit().await.unwrap();
        store.flush().await.expect("flush failed");

        let remote_vol = FxRemoteVolume::new(store.clone(), None, None);
        let layer_set = remote_vol.get_layer_set().await.expect("get_layer_set failed");
        assert!(!layer_set.layers.is_empty());

        fs.close().await.expect("close failed");
    }

    #[fuchsia::test]
    async fn test_remote_volume_close_fidl() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let device = DeviceHolder::new(FakeDevice::new(16384, 512));
        let fs = new_test_fs_builder().format(true).open(device).await.unwrap();
        let root_vol = root_volume(fs.clone()).await.unwrap();
        let store = root_vol.new_volume("test_remote_vol", Default::default()).await.unwrap();

        let unmounted = Arc::new(AtomicBool::new(false));
        let unmounted_clone = unmounted.clone();
        let unmount_fn: UnmountFn = Box::new(move || {
            Box::pin(async move {
                unmounted_clone.store(true, Ordering::Relaxed);
                Ok(())
            })
        });

        let remote_vol = FxRemoteVolume::new(store.clone(), None, Some(unmount_fn));

        let (proxy, server_end) = create_proxy::<ffxfs::RemoteVolumeMarker>();
        remote_vol.serve(server_end.into_stream());

        assert!(!unmounted.load(Ordering::Relaxed));

        // Call Close FIDL method.
        let close_res = proxy.close().await.expect("close FIDL call failed");
        assert!(close_res.is_ok());

        // Verify unmount callback was invoked.
        assert!(unmounted.load(Ordering::Relaxed));

        fs.close().await.expect("close failed");
    }

    #[fuchsia::test(threads = 10)]
    async fn test_remote_volume_open_file() {
        const WRAPPING_KEY_ID: [u8; 16] = [1; 16];

        let device = DeviceHolder::new(FakeDevice::new(16384, 512));
        let fs = new_test_fs_builder().format(true).open(device).await.unwrap();
        let crypt = Arc::new(new_insecure_crypt());
        crypt.add_wrapping_key(WRAPPING_KEY_ID, [1; 32].into()).unwrap();

        let root_vol = root_volume(fs.clone()).await.unwrap();
        let store = root_vol
            .new_volume(
                "test_remote_vol",
                fxfs::object_store::NewChildStoreOptions {
                    options: fxfs::object_store::StoreOptions {
                        crypt: Some(crypt.clone()),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let (client_end, server_end) =
            fidl::endpoints::create_endpoints::<ffxfs::RemoteVolumeMarker>();
        let proxy = Arc::new(ffxfs::RemoteVolumeSynchronousProxy::new(client_end.into_channel()));
        let fx_volume = Arc::new(
            FxVolume::new(
                std::sync::Weak::new(),
                store.clone(),
                zx::Event::create().koid().unwrap().raw_koid(),
                "test_vol_server".to_string(),
                Arc::new(refaults_vmo::PageFaultCounter::new().unwrap()),
                Default::default(),
            )
            .unwrap(),
        );

        let remote_vol = FxRemoteVolume::new(store.clone(), Some(fx_volume.clone()), None);
        remote_vol.serve(server_end.into_stream());

        let mut transaction = store
            .new_transaction(
                fxfs::object_store::transaction::LockKeys::default(),
                Options::default(),
            )
            .await
            .unwrap();
        let fscrypt_file = ObjectStore::create_object(
            &store,
            &mut transaction,
            fxfs::object_store::HandleOptions::default(),
            Some(WRAPPING_KEY_ID.into()),
        )
        .await
        .unwrap();
        let fscrypt_file_id = fscrypt_file.object_id();
        transaction.commit().await.unwrap();

        let payload = b"encrypted file read test";
        let mut buf = fscrypt_file.allocate_buffer(payload.len()).await;
        buf.as_mut_ptr_slice().copy_from_slice(payload);
        fscrypt_file.write_or_append(Some(0), buf.as_ref()).await.unwrap();

        let (file_proxy, file_server_end) = create_proxy::<fio::FileMarker>();
        let file_info = proxy
            .open_file(fscrypt_file_id, file_server_end, zx::MonotonicInstant::INFINITE)
            .unwrap()
            .expect("open_file failed");
        let stream = file_info.stream.expect("Stream expected in FileInfo");
        let stream_clone = stream.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let read_bytes = fuchsia_async::unblock(move || {
            stream_clone
                .read_at_to_vec(zx::StreamReadOptions::empty(), 0, payload.len())
                .expect("stream read failed")
        })
        .await;
        assert_eq!(&read_bytes, payload);

        drop(stream);
        drop(file_proxy);
        drop(proxy);

        remote_vol.wait().await;
        fs.close().await.expect("close failed");
    }
}
