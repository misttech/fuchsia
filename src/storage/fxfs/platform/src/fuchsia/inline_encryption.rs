// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Tests fxfs compatibility with inline encryption using a software-based crypto implementation.

#[cfg(test)]
mod tests {
    use crate::component::new_block_client;
    use crate::file::FxFile;
    use crate::fuchsia::RemoteCrypt;
    use crate::fuchsia::volume::FxVolume;
    use fidl_fuchsia_fxfs::{CryptMarker, KeyPurpose};
    use fidl_fuchsia_hardware_inlineencryption::{DeviceMarker, DeviceSynchronousProxy};
    use fidl_fuchsia_io as fio;
    use fuchsia_async::Task;
    use fxfs::filesystem::{FxFilesystem, FxFilesystemBuilder};
    use fxfs::lock_keys;
    use fxfs::object_handle::{ObjectHandle, ReadObjectHandle, WriteObjectHandle};
    use fxfs::object_store::directory::Directory;
    use fxfs::object_store::transaction::{LockKey, Options};
    use fxfs::object_store::volume::root_volume;
    use fxfs::object_store::{HandleOptions, NewChildStoreOptions, ObjectStore, StoreOptions};
    use fxfs_crypto::Crypt;
    use refaults_vmo::PageRefaultCounter;
    use std::sync::{Arc, Weak};
    use storage_device::DeviceHolder;
    use storage_device::block_device::BlockDevice;
    use test_case::test_case;
    use vfs::node::Node;
    use vmo_backed_block_server::{
        InitialContents, VmoBackedServerOptions, VmoBackedServerTestingExt,
    };

    const TEST_UUID: [u8; 16] =
        [73, 142, 230, 48, 132, 165, 68, 97, 141, 247, 22, 242, 153, 171, 153, 38];

    const TEST_DEVICE_BLOCK_SIZE: u32 = 512;

    enum KeyIdentifierDerivationAlgorithm {
        Lblk32,
        FuchsiaSpecific,
    }

    async fn create_vol_with_starnix_crypt(
        filesystem: Arc<FxFilesystem>,
        insecure_inline_crypto_proxy: DeviceSynchronousProxy,
        key_identifier_derivation_algorithm: KeyIdentifierDerivationAlgorithm,
        name: &str,
    ) -> (Arc<ObjectStore>, Arc<starnix_crypt::CryptService>) {
        let raw_data_key = vec![0u8; 32];
        let raw_metadata_key = vec![0u8; 32];
        let crypt_service = Arc::new(starnix_crypt::CryptService::new(
            &raw_metadata_key,
            &raw_data_key,
            matches!(key_identifier_derivation_algorithm, KeyIdentifierDerivationAlgorithm::Lblk32),
            Some(insecure_inline_crypto_proxy),
        ));
        crypt_service.set_uuid(TEST_UUID);
        let crypt_service_clone = crypt_service.clone();
        let (crypt_client, crypt_server) = fidl::endpoints::create_endpoints::<CryptMarker>();
        Task::spawn(async move {
            crypt_service_clone
                .handle_connection(crypt_server.into_stream())
                .await
                .expect("CryptService failed");
        })
        .detach();

        let crypt = Arc::new(RemoteCrypt::new(crypt_client));
        let root_volume = root_volume(filesystem.clone()).await.expect("root_volume failed");
        let vol = root_volume
            .new_volume(
                name,
                NewChildStoreOptions {
                    options: StoreOptions {
                        crypt: Some(crypt.clone() as Arc<dyn Crypt>),
                        ..StoreOptions::default()
                    },
                    ..NewChildStoreOptions::default()
                },
            )
            .await
            .expect("new_volume failed");

        (vol, crypt_service)
    }

    async fn create_vol_with_fxfs_crypt(
        filesystem: Arc<FxFilesystem>,
        name: &str,
    ) -> Arc<ObjectStore> {
        let crypt_service = Arc::new(fxfs_crypt::CryptService::new());
        crypt_service
            .add_wrapping_key(0, fxfs_insecure_crypto::DATA_KEY.to_vec())
            .expect("add_wrapping_key failed");
        crypt_service
            .add_wrapping_key(1, fxfs_insecure_crypto::METADATA_KEY.to_vec())
            .expect("add_wrapping_key failed");
        crypt_service.set_active_key(KeyPurpose::Data, 0).expect("set_active_key failed");
        crypt_service.set_active_key(KeyPurpose::Metadata, 1).expect("set_active_key failed");

        let (crypt_client, crypt_server) = fidl::endpoints::create_endpoints::<CryptMarker>();
        Task::spawn(async move {
            crypt_service
                .handle_request(fxfs_crypt::Services::Crypt(crypt_server.into_stream()))
                .await
                .expect("CryptService failed");
        })
        .detach();

        let crypt = Arc::new(RemoteCrypt::new(crypt_client));
        let root_volume = root_volume(filesystem.clone()).await.expect("root_volume failed");
        root_volume
            .new_volume(
                name,
                NewChildStoreOptions {
                    options: StoreOptions {
                        crypt: Some(crypt.clone() as Arc<dyn Crypt>),
                        ..StoreOptions::default()
                    },
                    ..NewChildStoreOptions::default()
                },
            )
            .await
            .expect("new_volume failed")
    }

    #[test_case(KeyIdentifierDerivationAlgorithm::Lblk32;
        "derive key identifiers with lblk32 fscrypt mode")]
    #[test_case(KeyIdentifierDerivationAlgorithm::FuchsiaSpecific;
        "derive key identifiers with Fuchsia-specific algorithm")]
    #[fuchsia::test]
    async fn test_write_and_read_inline_encrypted_files(
        key_identifier_derivation_algorithm: KeyIdentifierDerivationAlgorithm,
    ) {
        let block_server = Arc::new(
            VmoBackedServerOptions {
                block_size: TEST_DEVICE_BLOCK_SIZE,
                initial_contents: InitialContents::FromCapacity(393216),
                ..Default::default()
            }
            .build()
            .expect("build from VmoBackedServerOptions failed"),
        );

        let filesystem = FxFilesystemBuilder::new()
            .format(true)
            .inline_crypto_enabled(true)
            .barriers_enabled(true)
            .open(DeviceHolder::new(
                BlockDevice::new(
                    new_block_client(block_server.connect())
                        .await
                        .expect("failed to create new block client"),
                    false,
                )
                .await
                .expect("failed to create block device"),
            ))
            .await
            .expect("failed to open filesystem");

        let block_server_clone = block_server.clone();
        let (insecure_inline_crypto_proxy, server) =
            fidl::endpoints::create_sync_proxy::<DeviceMarker>();
        Task::spawn(async move {
            block_server_clone
                .connect_insecure_inline_encryption_server(server.into_channel().into(), TEST_UUID)
                .await;
        })
        .detach();

        // Use Starnix CryptService which supports creating and storing keys in the format expected
        // for inline encryption.
        let (starnix_vol, crypt_service) = create_vol_with_starnix_crypt(
            filesystem.clone(),
            insecure_inline_crypto_proxy,
            key_identifier_derivation_algorithm,
            "starnix",
        )
        .await;

        let starnix_vol_root_dir =
            Directory::open(&starnix_vol, starnix_vol.root_directory_object_id())
                .await
                .expect("open failed");

        // Create a directory that has a wrapping key identifier.
        let mut transaction = filesystem
            .clone()
            .new_transaction(
                lock_keys![LockKey::object(
                    starnix_vol.store_object_id(),
                    starnix_vol.root_directory_object_id()
                )],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");
        let dir = starnix_vol_root_dir
            .create_child_dir(&mut transaction, "dir")
            .await
            .expect("create_child_dir failed");
        transaction.commit().await.expect("transaction commit failed");
        // `crypt_service` uses the (sync) crypt proxy to add wrapping keys. Wrap this with
        // `fuchsia_async::unblock` so that the test thread will not block on this operation.
        let key_identifier = fuchsia_async::unblock(move || {
            crypt_service.add_wrapping_key(&[0xab; 32], 0).expect("add_wrapping_key failed")
        })
        .await;
        let transaction = filesystem
            .clone()
            .new_transaction(
                lock_keys![LockKey::object(starnix_vol.store_object_id(), dir.object_id())],
                Options::default(),
            )
            .await
            .expect("new transaction failed");
        dir.update_attributes(
            transaction,
            Some(&fio::MutableNodeAttributes {
                wrapping_key_id: Some(key_identifier),
                ..Default::default()
            }),
            0,
            None,
        )
        .await
        .expect("update attributes failed");

        // Files created within this directory will have wrapped key of type `FscryptInoLblk32File`.
        let mut transaction = filesystem
            .clone()
            .new_transaction(
                lock_keys![LockKey::object(starnix_vol.store_object_id(), dir.object_id())],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");
        let handle = dir
            .create_child_file(&mut transaction, "file")
            .await
            .expect("create_child_file failed");
        transaction.commit().await.expect("commit failed");

        // Write and read using inline encryption
        let mut buf = handle.allocate_buffer(2 * TEST_DEVICE_BLOCK_SIZE as usize).await;
        buf.as_mut_slice().fill(0xaa);
        handle.write_or_append(Some(0), buf.as_ref()).await.expect("write failed");

        let mut buf = handle.allocate_buffer(2 * TEST_DEVICE_BLOCK_SIZE as usize).await;
        handle.read(0, buf.as_mut()).await.expect("read failed");
        assert_eq!(buf.as_slice(), vec![0xaa; 2 * TEST_DEVICE_BLOCK_SIZE as usize]);

        filesystem.close().await.expect("close failed");
    }

    #[fuchsia::test]
    async fn test_multiple_volume_different_crypt_services() {
        let block_server = Arc::new(
            VmoBackedServerOptions {
                block_size: TEST_DEVICE_BLOCK_SIZE,
                initial_contents: InitialContents::FromCapacity(393216),
                ..Default::default()
            }
            .build()
            .expect("build from VmoBackedServerOptions failed"),
        );

        // TODO(https://fxbug.dev/474479747): `inline_crypto_enabled` assumes *all* encrypted stores
        // uses inline encryption and skips computing checksums. We only want to skip if the
        // encrypted store uses inline encryption. We need to find a better way of determining this.
        let filesystem = FxFilesystemBuilder::new()
            .format(true)
            .inline_crypto_enabled(true)
            .barriers_enabled(true)
            .open(DeviceHolder::new(
                BlockDevice::new(
                    new_block_client(block_server.connect())
                        .await
                        .expect("failed to create new block client"),
                    false,
                )
                .await
                .expect("failed to create block device"),
            ))
            .await
            .expect("failed to open filesystem");

        let block_server_clone = block_server.clone();
        let (insecure_inline_crypto_proxy, server) =
            fidl::endpoints::create_sync_proxy::<DeviceMarker>();
        Task::spawn(async move {
            block_server_clone
                .connect_insecure_inline_encryption_server(server.into_channel().into(), TEST_UUID)
                .await;
        })
        .detach();

        let (starnix_vol, starnix_crypt_service) = create_vol_with_starnix_crypt(
            filesystem.clone(),
            insecure_inline_crypto_proxy,
            KeyIdentifierDerivationAlgorithm::Lblk32,
            "starnix",
        )
        .await;
        // Note that fxfs crypt service does not support creating key of type `FscryptInoLblk32File`
        let vol_with_fxfs_crypt = create_vol_with_fxfs_crypt(filesystem.clone(), "vol").await;

        // (1) Test write and read from file in volume with Starnix crypt service
        let starnix_vol_root_dir =
            Directory::open(&starnix_vol, starnix_vol.root_directory_object_id())
                .await
                .expect("open failed");
        let mut transaction = filesystem
            .clone()
            .new_transaction(
                lock_keys![LockKey::object(
                    starnix_vol.store_object_id(),
                    starnix_vol.root_directory_object_id()
                )],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");
        let dir = starnix_vol_root_dir
            .create_child_dir(&mut transaction, "dir")
            .await
            .expect("create_child_dir failed");
        transaction.commit().await.expect("transaction commit failed");
        let key_identifier = fuchsia_async::unblock(move || {
            starnix_crypt_service.add_wrapping_key(&[0xab; 32], 0).expect("add_wrapping_key failed")
        })
        .await;
        let transaction = filesystem
            .clone()
            .new_transaction(
                lock_keys![LockKey::object(starnix_vol.store_object_id(), dir.object_id())],
                Options::default(),
            )
            .await
            .expect("new transaction failed");
        dir.update_attributes(
            transaction,
            Some(&fio::MutableNodeAttributes {
                wrapping_key_id: Some(key_identifier),
                ..Default::default()
            }),
            0,
            None,
        )
        .await
        .expect("update attributes failed");
        let mut transaction = filesystem
            .clone()
            .new_transaction(
                lock_keys![LockKey::object(starnix_vol.store_object_id(), dir.object_id())],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");
        let handle = dir
            .create_child_file(&mut transaction, "file")
            .await
            .expect("create_child_file failed");
        transaction.commit().await.expect("commit failed");

        // Write and read using inline encryption
        let mut buf = handle.allocate_buffer(2 * TEST_DEVICE_BLOCK_SIZE as usize).await;
        buf.as_mut_slice().fill(0xaa);
        handle.write_or_append(Some(0), buf.as_ref()).await.expect("write failed");

        let mut buf = handle.allocate_buffer(2 * TEST_DEVICE_BLOCK_SIZE as usize).await;
        handle.read(0, buf.as_mut()).await.expect("read failed");
        assert_eq!(buf.as_slice(), vec![0xaa; 2 * TEST_DEVICE_BLOCK_SIZE as usize]);

        // (2) Test write and read from file in volume with fxfs crypt service
        let vol_with_fxfs_crypt_root_dir =
            Directory::open(&vol_with_fxfs_crypt, vol_with_fxfs_crypt.root_directory_object_id())
                .await
                .expect("open failed");
        let mut transaction = filesystem
            .clone()
            .new_transaction(
                lock_keys![LockKey::object(
                    vol_with_fxfs_crypt.store_object_id(),
                    vol_with_fxfs_crypt.root_directory_object_id()
                )],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");
        let dir = vol_with_fxfs_crypt_root_dir
            .create_child_dir(&mut transaction, "dir1")
            .await
            .expect("create_child_dir failed");
        transaction.commit().await.expect("transaction commit failed");
        let mut transaction = filesystem
            .clone()
            .new_transaction(
                lock_keys![LockKey::object(vol_with_fxfs_crypt.store_object_id(), dir.object_id())],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");
        let handle = dir
            .create_child_file(&mut transaction, "file1")
            .await
            .expect("create_child_file failed");
        transaction.commit().await.expect("commit failed");

        // Write and read non-inline encrypted files
        let mut buf = handle.allocate_buffer(2 * TEST_DEVICE_BLOCK_SIZE as usize).await;
        buf.as_mut_slice().fill(0xbb);
        handle.write_or_append(Some(0), buf.as_ref()).await.expect("write failed");

        let mut buf = handle.allocate_buffer(2 * TEST_DEVICE_BLOCK_SIZE as usize).await;
        handle.read(0, buf.as_mut()).await.expect("read failed");
        assert_eq!(buf.as_slice(), vec![0xbb; 2 * TEST_DEVICE_BLOCK_SIZE as usize]);

        filesystem.close().await.expect("close failed");
    }

    #[test_case(KeyIdentifierDerivationAlgorithm::Lblk32;
        "derive key identifiers with lblk32 fscrypt mode")]
    #[test_case(KeyIdentifierDerivationAlgorithm::FuchsiaSpecific;
        "derive key identifiers with Fuchsia-specific algorithm")]
    #[fuchsia::test]
    async fn test_get_attribute_of_inline_encrypted_files(
        key_identifier_derivation_algorithm: KeyIdentifierDerivationAlgorithm,
    ) {
        let block_server = Arc::new(
            VmoBackedServerOptions {
                block_size: TEST_DEVICE_BLOCK_SIZE,
                initial_contents: InitialContents::FromCapacity(393216),
                ..Default::default()
            }
            .build()
            .expect("build from VmoBackedServerOptions failed"),
        );

        let filesystem = FxFilesystemBuilder::new()
            .format(true)
            .inline_crypto_enabled(true)
            .barriers_enabled(true)
            .open(DeviceHolder::new(
                BlockDevice::new(
                    new_block_client(block_server.connect())
                        .await
                        .expect("failed to create new block client"),
                    false,
                )
                .await
                .expect("failed to create block device"),
            ))
            .await
            .expect("failed to open filesystem");

        let block_server_clone = block_server.clone();
        let (insecure_inline_crypto_proxy, server) =
            fidl::endpoints::create_sync_proxy::<DeviceMarker>();
        Task::spawn(async move {
            block_server_clone
                .connect_insecure_inline_encryption_server(server.into_channel().into(), TEST_UUID)
                .await;
        })
        .detach();

        // Use Starnix CryptService which supports creating and storing keys in the format expected
        // for inline encryption.
        let (starnix_vol, crypt_service) = create_vol_with_starnix_crypt(
            filesystem.clone(),
            insecure_inline_crypto_proxy,
            key_identifier_derivation_algorithm,
            "starnix",
        )
        .await;

        let starnix_vol_root_dir =
            Directory::open(&starnix_vol, starnix_vol.root_directory_object_id())
                .await
                .expect("open failed");

        // Create a directory that has a wrapping key identifier.
        let mut transaction = filesystem
            .clone()
            .new_transaction(
                lock_keys![LockKey::object(
                    starnix_vol.store_object_id(),
                    starnix_vol.root_directory_object_id()
                )],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");
        let dir = starnix_vol_root_dir
            .create_child_dir(&mut transaction, "dir")
            .await
            .expect("create_child_dir failed");
        transaction.commit().await.expect("transaction commit failed");
        // `crypt_service` uses the (sync) crypt proxy to add wrapping keys. Wrap this with
        // `fuchsia_async::unblock` so that the test thread will not block on this operation.
        let key_identifier = fuchsia_async::unblock(move || {
            crypt_service.add_wrapping_key(&[0xab; 32], 0).expect("add_wrapping_key failed")
        })
        .await;
        let transaction = filesystem
            .clone()
            .new_transaction(
                lock_keys![LockKey::object(starnix_vol.store_object_id(), dir.object_id())],
                Options::default(),
            )
            .await
            .expect("new transaction failed");
        dir.update_attributes(
            transaction,
            Some(&fio::MutableNodeAttributes {
                wrapping_key_id: Some(key_identifier),
                ..Default::default()
            }),
            0,
            None,
        )
        .await
        .expect("update attributes failed");

        let mut transaction = filesystem
            .clone()
            .new_transaction(
                lock_keys![LockKey::object(starnix_vol.store_object_id(), dir.object_id())],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");
        let file_object = dir
            .create_child_file(&mut transaction, "file")
            .await
            .expect("create_child_file failed");
        transaction.commit().await.expect("commit failed");

        let vol = Arc::new(
            FxVolume::new(
                Weak::new(),
                starnix_vol.clone(),
                starnix_vol.store_object_id(),
                Arc::new(PageRefaultCounter::new().expect("PageRefaultCounter::new failed")),
            )
            .expect("FxVolume::new failed"),
        );
        let file = FxFile::new(
            ObjectStore::open_object(&vol, file_object.object_id(), HandleOptions::default(), None)
                .await
                .expect("open_object failed"),
        );
        let attributes = file
            .get_attributes(fio::NodeAttributesQuery::WRAPPING_KEY_ID)
            .await
            .expect("get_attributes failed");
        assert_eq!(attributes.mutable_attributes.wrapping_key_id, Some(key_identifier));

        filesystem.close().await.expect("close failed");
    }
}
