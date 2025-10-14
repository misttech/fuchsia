// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::ranged_device::RangedDevice;
use crate::*;
use anyhow::Context;
use f2fs_reader::F2fsReader;
use fxfs::filesystem::{FxFilesystemBuilder, OpenFxFilesystem};
use fxfs::object_handle::ReadObjectHandle;
use fxfs::object_store::journal::super_block::SuperBlockInstance;
use fxfs::object_store::volume::root_volume as fxfs_root_volume;
use fxfs::object_store::{Directory, HandleOptions, ObjectStore};
use fxfs_insecure_crypto::InsecureCrypt;

use std::io::Read;
use std::sync::Arc;
use storage_device::fake_device::FakeDevice;
use storage_device::{Device, DeviceHolder};

fn create_device_with_image_at_offset(path: &str, offset: u64) -> FakeDevice {
    let path = std::path::PathBuf::from(path);
    let mut file = std::fs::File::open(&path).expect("open image");
    let mut data = Vec::new();
    zstd::Decoder::new(&mut file)
        .expect("decompress image")
        .read_to_end(&mut data)
        .expect("read image");

    // Make sure device is large enough for Fxfs superblocks and the F2FS image.
    let device_size = std::cmp::max(
        offset + data.len() as u64,
        SuperBlockInstance::B.first_extent().end + FXFS_BLOCK_SIZE as u64,
    );

    let device =
        FakeDevice::new(device_size.div_ceil(F2FS_BLOCK_SIZE as u64), F2FS_BLOCK_SIZE as u32);

    // Write in chunks to avoid exceeding transfer heap size.
    const CHUNK_SIZE: usize = 1024 * 1024;
    let mut written = 0;
    while written < data.len() {
        let len = std::cmp::min(data.len() - written, CHUNK_SIZE);
        let mut buffer = futures::executor::block_on(device.allocate_buffer(len));
        buffer.as_mut_slice().copy_from_slice(&data[written..written + len]);
        futures::executor::block_on(device.write(offset + written as u64, buffer.as_ref()))
            .expect("write image");
        written += len;
    }
    drop(data);
    device
}

async fn test_fxfs_migration_at_offset(offset: u64) {
    let device = Arc::new(create_device_with_image_at_offset("/pkg/testdata/f2fs.img.zst", offset));

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

    let insecure_crypt = InsecureCrypt::new();
    let crypt: Arc<dyn Crypt> = Arc::new(insecure_crypt);
    let device = Arc::try_unwrap(device).map_err(|_| ()).expect("only one ref to device");

    let device = Box::pin(migrate_device(offset, DeviceHolder::new(device), crypt.clone()))
        .await
        .expect("migrate_device");

    // Reopen RW so we can mount Fxfs normally.
    device.reopen(false);
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
    let device = Arc::new(create_device_with_image_at_offset("/pkg/testdata/f2fs.img.zst", offset));

    // Read data from F2FS before migration.
    let (uuid, superblock, expected_data, ino) = {
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
        (f2fs.superblock().uuid, f2fs.superblock().clone(), f2fs_data, ino)
    };

    let device = Arc::try_unwrap(device).map_err(|_| ()).expect("only one ref to device");

    let mut insecure_crypt = InsecureCrypt::new();
    insecure_crypt.set_filesystem_uuid(&uuid);
    insecure_crypt.add_wrapping_key(fscrypt::main_key_to_identifier(&[0; 64]), [0; 64].into());
    insecure_crypt.add_wrapping_key([0; 16], [0; 64].into());
    let crypt: Arc<dyn Crypt> = Arc::new(insecure_crypt);

    let device = Box::pin(migrate_device(offset, DeviceHolder::new(device), crypt.clone()))
        .await
        .expect("migrate_device");

    // Reopen RW so we can mount Fxfs normally.
    device.reopen(false);
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
    let device = Arc::new(create_device_with_image_at_offset("/pkg/testdata/f2fs.img.zst", offset));

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

    let mut insecure_crypt = InsecureCrypt::new();
    insecure_crypt.set_filesystem_uuid(&uuid);
    insecure_crypt.add_wrapping_key(fscrypt::main_key_to_identifier(&[0; 64]), [0; 64].into());

    let crypt: Arc<dyn Crypt> = Arc::new(insecure_crypt);

    let device = Box::pin(migrate_device(offset, DeviceHolder::new(device), crypt.clone()))
        .await
        .expect("migrate_device");

    // Reopen RW so we can mount Fxfs normally.
    device.reopen(false);
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
