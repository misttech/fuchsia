// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::*;
use anyhow::Context;
use f2fs_reader::F2fsReader;
use fidl_fuchsia_hardware_inlineencryption::{DeviceMarker, DeviceRequest, DeviceRequestStream};
use fxfs::filesystem::{FxFilesystemBuilder, OpenFxFilesystem};
use fxfs::object_handle::ReadObjectHandle;
use fxfs::object_store::journal::super_block::SuperBlockInstance;
use fxfs::object_store::volume::root_volume as fxfs_root_volume;
use fxfs::object_store::{Directory, HandleOptions, ObjectStore};
use fxfs_platform::fuchsia::RemoteCrypt;
use starnix_crypt::{CryptService, Lblk32KeyInfo, UserKey};
use storage_device::block_device::BlockDevice;
use storage_device::ranged_device::RangedDevice;

use std::io::Read;
use std::sync::Arc;
use storage_device::{Device, DeviceHolder};
use vmo_backed_block_server::{VmoBackedServer, VmoBackedServerTestingExt};

fn create_device_with_image_at_offset(path: &str, offset: u64) -> Arc<VmoBackedServer> {
    let path = std::path::PathBuf::from(path);
    let mut reader = zstd::Decoder::new(std::fs::File::open(&path).expect("open image"))
        .expect("decompress image");
    let mut data = Vec::new();
    reader.read_to_end(&mut data).expect("failed to read image");

    // Make sure device is large enough for Fxfs superblocks and the F2FS image.
    let device_size = std::cmp::max(
        offset + data.len() as u64,
        SuperBlockInstance::B.first_extent().end + FXFS_BLOCK_SIZE as u64,
    );

    let vmo = zx::Vmo::create(device_size).expect("failed to create vmo");
    vmo.write(&data, offset).expect("failed to write image to vmo");
    Arc::new(VmoBackedServer::from_vmo(F2FS_BLOCK_SIZE as u32, vmo))
}

async fn handle_inline_crypto_requests(
    mut stream: DeviceRequestStream,
    server: Arc<VmoBackedServer>,
) {
    while let Some(Ok(request)) = stream.next().await {
        match request {
            DeviceRequest::ProgramKey { wrapped_key, data_unit_size: _, responder } => {
                let mut main_key = [0; 64];
                assert!(wrapped_key.len() <= main_key.len());
                main_key[..wrapped_key.len()].copy_from_slice(&wrapped_key);
                let slot = server.program_key(main_key);
                responder.send(Ok(slot)).unwrap_or_else(|e| {
                    log::error!("failed to send ProgramKey response. error: {:?}", e);
                });
            }
            DeviceRequest::DeriveRawSecret { wrapped_key: _, responder } => {
                log::warn!("DeriveRawSecret not implemented");
                responder.send(Err(zx::Status::NOT_SUPPORTED.into_raw())).unwrap_or_else(|e| {
                    log::error!("failed to send DeriveRawSecret response. error: {:?}", e);
                });
            }
        }
    }
}

async fn test_fxfs_migration_at_offset(offset: u64) {
    let block_server = create_device_with_image_at_offset("/pkg/testdata/f2fs.img.zst", offset);
    let device = Arc::new(
        BlockDevice::new(
            RemoteBlockClient::new(block_server.connect::<BlockProxy>())
                .await
                .expect("Unable to create block client"),
            false,
        )
        .await
        .unwrap(),
    );

    // Open f2fs to get UUID.
    let block_size = device.block_size() as u64;
    let start_block = offset / block_size;
    let num_blocks = device.block_count() - start_block;

    let original_superblock = {
        let ranged_device = Arc::new(
            RangedDevice::new(device.clone(), start_block, num_blocks)
                .expect("create ranged device"),
        );
        let f2fs = F2fsReader::open_device(ranged_device).await.expect("f2fs open ok");
        (*f2fs.superblock()).clone()
    };

    let block_server_clone = block_server.clone();
    let (client, server) = fidl::endpoints::create_sync_proxy::<DeviceMarker>();
    std::thread::spawn(move || {
        LocalExecutor::default().run_singlethreaded(async {
            handle_inline_crypto_requests(server.into_stream(), block_server_clone).await
        })
    });

    let crypt_service = Arc::new(CryptService::new());
    setup_starnix_crypt_volume_keys(&crypt_service);

    let (crypt_client_end, crypt_proxy) = fidl::endpoints::create_endpoints::<CryptMarker>();

    let crypt_service_clone = Arc::clone(&crypt_service);
    fuchsia_async::Task::spawn(async move {
        if let Err(err) =
            crypt_service_clone.handle_connection(crypt_proxy.into_stream(), Some(client)).await
        {
            log::error!(err:?; "Crypt service failure");
        }
    })
    .detach();

    let crypt = Arc::new(RemoteCrypt::new(crypt_client_end)) as Arc<dyn Crypt>;
    let device = Arc::try_unwrap(device).map_err(|_| ()).expect("only one ref to device");

    Box::pin(migrate_device(offset, DeviceHolder::new(device), crypt.clone()))
        .await
        .expect("migrate_device");

    let device = DeviceHolder::new(
        BlockDevice::new(
            RemoteBlockClient::new(block_server.connect::<BlockProxy>())
                .await
                .expect("Unable to create block client"),
            false,
        )
        .await
        .unwrap(),
    );

    let fxfs = FxFilesystemBuilder::new().read_only(true).open(device).await.expect("open failed");

    let root_vol = fxfs_root_volume(fxfs.clone()).await.expect("Opening root volume");
    let vol = root_vol
        .volume("userdata", StoreOptions { crypt: Some(crypt.clone()), ..StoreOptions::default() })
        .await
        .expect("Opening volume");
    fxfs::fsck::fsck_volume(&fxfs, vol.store_object_id(), Some(crypt)).await.expect("fsck volume");
    let root_directory =
        Directory::open(&vol, vol.root_directory_object_id()).await.expect("open failed");
    let ino = original_superblock.root_ino;

    // Note that we can't check file contents in this test as we haven't given fxfs encryption keys.
    let check_file_contents = false;
    Box::pin(verify(offset, fxfs.device(), &fxfs, ino, root_directory, check_file_contents))
        .await
        .expect("verify");

    fxfs.close().await.expect("close ok");
}

fn setup_starnix_crypt_volume_keys(crypt_service: &Arc<CryptService>) {
    let mut raw_data_key = vec![0u8; 32];
    zx::cprng_draw(&mut raw_data_key);
    let (data_wrapping_key_id, data_cipher) =
        crypt_service.derive_fxfs_wrapping_key_id_and_cipher(&raw_data_key);

    let mut raw_metadata_key = vec![0u8; 32];
    zx::cprng_draw(&mut raw_metadata_key);
    let (metadata_wrapping_key_id, metadata_cipher) =
        crypt_service.derive_fxfs_wrapping_key_id_and_cipher(&raw_metadata_key);

    crypt_service
        .add_wrapping_key(
            metadata_wrapping_key_id.into(),
            UserKey::FxfsKey { cipher: metadata_cipher },
            0,
        )
        .expect("failed to add metadata volume key");

    crypt_service
        .add_wrapping_key(data_wrapping_key_id.into(), UserKey::FxfsKey { cipher: data_cipher }, 0)
        .expect("failed to add data volume key");

    crypt_service
        .set_active_key(metadata_wrapping_key_id.into(), KeyPurpose::Metadata)
        .expect("failed to set active metadata key");
    crypt_service
        .set_active_key(data_wrapping_key_id.into(), KeyPurpose::Data)
        .expect("failed to set active data key");
}

// Migrates an f2fs device to fxfs and verifies directory tree matches.
// Note this test can't verify file contents as we haven't given encryption keys.
#[fuchsia::test]
async fn test_fxfs_migration_no_keys() {
    Box::pin(test_fxfs_migration_at_offset(0)).await;
}

#[fuchsia::test]
async fn test_fxfs_migration_with_offset() {
    // Place f2fs image between Fxfs superblocks.
    let offset = SuperBlockInstance::A.first_extent().end + FXFS_BLOCK_SIZE as u64;
    Box::pin(test_fxfs_migration_at_offset(offset)).await;
}

async fn recurse_resolve_f2fs(f2fs: &F2fsReader, ino: u32, path: &str) -> u32 {
    if let Some((head, rest)) = path.split_once("/") {
        for entry in f2fs.readdir(ino).await.expect("readdir") {
            if entry.filename == head {
                return Box::pin(recurse_resolve_f2fs(f2fs, entry.ino, rest)).await;
            }
        }
    } else {
        for entry in f2fs.readdir(ino).await.expect("readdir") {
            if entry.filename == path {
                return entry.ino;
            }
        }
    }
    panic!("Path not found: {path:?}");
}

// Read a single file encrypted with fscrypt's INO_LBLK32 mode.
#[fuchsia::test]
async fn test_fxfs_read_lblk32_ino_file() {
    let offset = 0;
    let block_server = create_device_with_image_at_offset("/pkg/testdata/f2fs.img.zst", offset);
    let device = Arc::new(
        BlockDevice::new(
            RemoteBlockClient::new(block_server.connect::<BlockProxy>())
                .await
                .expect("Unable to create block client"),
            false,
        )
        .await
        .unwrap(),
    );

    // Read data from F2FS before migration.
    let (superblock, expected_data, ino) = {
        let block_size = device.block_size() as u64;
        let start_block = offset / block_size;
        let num_blocks = device.block_count() - start_block;
        let ranged_device = Arc::new(
            RangedDevice::new(device.clone(), start_block, num_blocks)
                .expect("create ranged device"),
        );
        let mut f2fs = F2fsReader::open_device(ranged_device).await.expect("f2fs open ok");
        f2fs.add_key(&[0; 64]);

        let ino = recurse_resolve_f2fs(&f2fs, f2fs.root_ino(), "fscrypt/a/b/inlined").await;
        let inode = f2fs.read_inode(ino).await.expect("read file");
        let f2fs_data = f2fs.read_data(&inode, 0).await.expect("read data");
        let f2fs_data = f2fs_data.map(|b| b.as_slice().to_vec());
        (f2fs.superblock().clone(), f2fs_data, ino)
    };

    let device = Arc::try_unwrap(device).map_err(|_| ()).expect("only one ref to device");

    let block_server_clone = block_server.clone();
    let (client, server) = fidl::endpoints::create_sync_proxy::<DeviceMarker>();
    std::thread::spawn(move || {
        LocalExecutor::default().run_singlethreaded(async {
            handle_inline_crypto_requests(server.into_stream(), block_server_clone).await
        })
    });

    let crypt_service = Arc::new(CryptService::new());
    setup_starnix_crypt_volume_keys(&crypt_service);

    let main_key: Vec<u8> = [0; 64].to_vec();
    let (wrapping_key_id, derived_keys) =
        crypt_service.derive_wrapping_key_id_and_lblk32_derived_keys(&main_key, false, &client);
    crypt_service
        .add_wrapping_key(
            wrapping_key_id,
            UserKey::InlineCryptoLblk32Key {
                key_info: Lblk32KeyInfo {
                    hardware_wrapped: false,
                    slot: None,
                    derived_keys,
                    main_key,
                },
            },
            0,
        )
        .expect("add wrapping key failed");

    let (crypt_client_end, crypt_proxy) = fidl::endpoints::create_endpoints::<CryptMarker>();

    let crypt_service_clone = Arc::clone(&crypt_service);
    fuchsia_async::Task::spawn(async move {
        if let Err(err) =
            crypt_service_clone.handle_connection(crypt_proxy.into_stream(), Some(client)).await
        {
            log::error!(err:?; "Crypt service failure");
        }
    })
    .detach();

    let crypt = Arc::new(RemoteCrypt::new(crypt_client_end)) as Arc<dyn Crypt>;

    Box::pin(migrate_device(offset, DeviceHolder::new(device), crypt.clone()))
        .await
        .expect("migrate_device");

    let device = DeviceHolder::new(
        BlockDevice::new(
            RemoteBlockClient::new(block_server.connect::<BlockProxy>())
                .await
                .expect("Unable to create block client"),
            false,
        )
        .await
        .unwrap(),
    );

    let fxfs = FxFilesystemBuilder::new().read_only(true).open(device).await.expect("open failed");

    let root_vol = fxfs_root_volume(fxfs.clone()).await.expect("Opening root volume");
    let vol = root_vol
        .volume("userdata", StoreOptions { crypt: Some(crypt.clone()), ..StoreOptions::default() })
        .await
        .expect("Opening volume");
    fxfs::fsck::fsck_volume(&fxfs, vol.store_object_id(), Some(crypt.clone()))
        .await
        .expect("fsck volume");
    let root_directory =
        Directory::open(&vol, vol.root_directory_object_id()).await.expect("open failed");

    // This is the data originally written into the file via our generation script.
    const EXPECTED_CONTENTS: &[u8] = b"test45678abcdef_12345678";
    // Confirm f2fs returns this data.
    assert_eq!(
        &expected_data.as_ref().unwrap().as_slice()[..EXPECTED_CONTENTS.len()],
        EXPECTED_CONTENTS
    );

    // Confirm fxfs also returns this data.
    let fxfs_object = ObjectStore::open_object(&vol, ino as u64, HandleOptions::default(), None)
        .await
        .expect("open object");
    if let Some(expected_data) = expected_data {
        let mut buf = fxfs_object.allocate_buffer(4096).await;
        assert_eq!(fxfs_object.read(0, buf.as_mut()).await.expect("read"), EXPECTED_CONTENTS.len());
        assert_eq!(
            &buf.as_slice()[..EXPECTED_CONTENTS.len()],
            &expected_data[..EXPECTED_CONTENTS.len()]
        );
    }
    Box::pin(verify(
        offset,
        fxfs.device(),
        &fxfs,
        superblock.root_ino,
        root_directory,
        /*check_file_contents=*/ true,
    ))
    .await
    .expect("verify");

    fxfs.close().await.expect("close ok");
}

#[fuchsia::test]
async fn test_fxfs_verify_encrypted_data() {
    let offset = 0;
    let block_server = create_device_with_image_at_offset("/pkg/testdata/f2fs.img.zst", offset);
    let device = Arc::new(
        BlockDevice::new(
            RemoteBlockClient::new(block_server.connect::<BlockProxy>())
                .await
                .expect("Unable to create block client"),
            false,
        )
        .await
        .unwrap(),
    );

    // Get UUID from F2FS.
    let (uuid, superblock) = {
        let block_size = device.block_size() as u64;
        let start_block = offset / block_size;
        let num_blocks = device.block_count() - start_block;
        let ranged_device = Arc::new(
            RangedDevice::new(device.clone(), start_block, num_blocks)
                .expect("create ranged device"),
        );
        let f2fs = F2fsReader::open_device(ranged_device).await.expect("f2fs open ok");
        (f2fs.superblock().uuid, (*f2fs.superblock()).clone())
    };

    let device = Arc::try_unwrap(device).map_err(|_| ()).expect("only one ref to device");

    let block_server_clone = block_server.clone();
    let (client, server) = fidl::endpoints::create_sync_proxy::<DeviceMarker>();
    std::thread::spawn(move || {
        LocalExecutor::default().run_singlethreaded(async {
            handle_inline_crypto_requests(server.into_stream(), block_server_clone).await
        })
    });

    let crypt_service = Arc::new(CryptService::new());
    setup_starnix_crypt_volume_keys(&crypt_service);

    let main_key: Vec<u8> = [0; 64].to_vec();
    let (wrapping_key_id, derived_keys) =
        crypt_service.derive_wrapping_key_id_and_lblk32_derived_keys(&main_key, false, &client);
    crypt_service
        .add_wrapping_key(
            wrapping_key_id,
            UserKey::InlineCryptoLblk32Key {
                key_info: Lblk32KeyInfo {
                    hardware_wrapped: false,
                    slot: None,
                    derived_keys,
                    main_key,
                },
            },
            0,
        )
        .expect("add wrapping key failed");

    let (crypt_client_end, crypt_proxy) = fidl::endpoints::create_endpoints::<CryptMarker>();

    let crypt_service_clone = Arc::clone(&crypt_service);
    fuchsia_async::Task::spawn(async move {
        if let Err(err) =
            crypt_service_clone.handle_connection(crypt_proxy.into_stream(), Some(client)).await
        {
            log::error!(err:?; "Crypt service failure");
        }
    })
    .detach();

    let crypt = Arc::new(RemoteCrypt::new(crypt_client_end)) as Arc<dyn Crypt>;

    Box::pin(migrate_device(offset, DeviceHolder::new(device), crypt.clone()))
        .await
        .expect("migrate_device");

    let device = DeviceHolder::new(
        BlockDevice::new(
            RemoteBlockClient::new(block_server.connect::<BlockProxy>())
                .await
                .expect("Unable to create block client"),
            false,
        )
        .await
        .unwrap(),
    );

    let fxfs = FxFilesystemBuilder::new().read_only(true).open(device).await.expect("open failed");

    assert_eq!(&uuid, fxfs.super_block_header().guid.0.as_bytes());

    let root_vol = fxfs_root_volume(fxfs.clone()).await.expect("Opening root volume");
    let vol = root_vol
        .volume("userdata", StoreOptions { crypt: Some(crypt.clone()), ..StoreOptions::default() })
        .await
        .expect("Opening volume");
    fxfs::fsck::fsck_volume(&fxfs, vol.store_object_id(), Some(crypt.clone()))
        .await
        .expect("fsck volume");
    let root_directory =
        Directory::open(&vol, vol.root_directory_object_id()).await.expect("open failed");

    Box::pin(verify(
        offset,
        fxfs.device(),
        &fxfs,
        superblock.root_ino,
        root_directory,
        /*check_file_contents=*/ true,
    ))
    .await
    .expect("verify");
    fxfs.close().await.expect("close ok");
}
// Verifies that the directory tree in fxfs matches f2fs.
// If check_file_contents is true, opens f2fs and verifies file contents.
async fn verify(
    offset: u64,
    device: Arc<dyn Device>,
    fxfs: &OpenFxFilesystem,
    ino: u32,
    root_directory: Directory<ObjectStore>,
    check_file_contents: bool,
) -> Result<(), Error> {
    let block_size = device.block_size() as u64;
    let start_block = offset / block_size;
    let num_blocks = device.block_count() - start_block;
    let ranged_device = Arc::new(
        RangedDevice::new(device.clone(), start_block, num_blocks).context("RangedDevice::new")?,
    );
    let mut f2fs =
        F2fsReader::open_device(ranged_device).await.context("Failed to open f2fs image")?;
    f2fs.add_key(&[0; 64]);

    crate::verify(&f2fs, fxfs, ino, root_directory, check_file_contents).await
}
