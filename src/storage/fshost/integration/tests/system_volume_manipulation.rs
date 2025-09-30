// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Tests for fuchsia.fshost/Recovery methods that manipulate volumes in the system container.
//! These methods are intended for use in recovery environments to enable fxblob-based products
//! to apply a system update or flash a new system image.

pub mod config;
use block_client::RemoteBlockClient;
use config::{blob_fs_type, data_fs_spec, data_fs_type, new_builder, volumes_spec};
use device_watcher::recursive_wait;
use fidl::endpoints::{ServiceMarker as _, create_proxy};
use fshost_test_fixture::disk_builder::{
    Disk, expected_fxblob_volumes, list_all_fxfs_volumes, test_blob_hash,
};
use fshost_test_fixture::{TestFixture, write_blob};
use fuchsia_component::client::{connect_to_protocol_at_dir_root, connect_to_protocol_at_dir_svc};
use fxfs_make_blob_image::FxBlobBuilder;
use sparse::builder::{DataSource, SparseImageBuilder};
use std::sync::Arc;
use storage_device::DeviceHolder;
use storage_device::block_device::BlockDevice;
use vmo_backed_block_server::{VmoBackedServer, VmoBackedServerTestingExt};
use zx::HandleBased as _;
use {
    fidl_fuchsia_fshost as ffshost, fidl_fuchsia_fxfs as ffxfs,
    fidl_fuchsia_hardware_block as fblock, fidl_fuchsia_io as fio, fuchsia_async as fasync,
};

const TEST_BLOBS: [&[u8]; 3] =
    ["Goodbye, stranger!".as_bytes(), "Hello, world!".as_bytes(), &['a' as u8; 16_384]];
const BLOCK_SIZE: u32 = 4096;
const SPARSE_CHUNK_SIZE: u64 = BLOCK_SIZE as u64 * 2;

/// Initializes a disk to have an fxfs partition with a blob and data volume. The blob volume will
/// contain an initial blob.
async fn build_fxblob() -> Disk {
    let mut builder = new_builder();
    builder.with_disk().format_volumes(volumes_spec()).format_data(data_fs_spec()).with_gpt();
    let fixture = builder.build().await;
    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;
    fixture.check_test_blob(true).await;
    fixture.tear_down().await.unwrap()
}

/// Creates a new test fixture in what we consider recovery mode using `system_container` to
/// represent the underlying mutable storage.
async fn start_recovery_fixture(system_container: Disk) -> TestFixture {
    let mut builder = new_builder().with_disk_from(system_container);
    builder.fshost().set_config_value("ramdisk_image", true);
    builder.with_zbi_ramdisk().format_volumes(volumes_spec());
    let fixture = builder.build().await;
    // Wait for the zbi ramdisk filesystems
    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;
    // Also wait for any driver binding on the "on-disk" devices
    recursive_wait(
        &fixture.dir(
            fidl_fuchsia_storage_partitions::PartitionServiceMarker::SERVICE_NAME,
            fio::PERM_READABLE,
        ),
        "part-000",
    )
    .await
    .unwrap();
    fixture
}

#[fuchsia::test]
async fn mount_system_blob_volume() {
    let system_container = build_fxblob().await;
    let fixture = start_recovery_fixture(system_container).await;
    let recovery: ffshost::RecoveryProxy =
        fixture.realm.root.connect_to_protocol_at_exposed_dir().unwrap();

    // Invoke MountSystemBlobVolume
    let (blob_exposed_dir, blob_exposed_dir_server) = create_proxy::<fio::DirectoryMarker>();
    recovery
        .mount_system_blob_volume(blob_exposed_dir_server)
        .await
        .unwrap()
        .expect("MountSystemBlobVolume unexpectedly failed");
    let blob_svc =
        fuchsia_fs::directory::open_directory(&blob_exposed_dir, "svc", fio::PERM_READABLE)
            .await
            .unwrap();

    // We should still be able to read our test blob back.
    let blob_reader =
        connect_to_protocol_at_dir_root::<ffxfs::BlobReaderMarker>(&blob_svc).unwrap();
    blob_reader
        .get_vmo(test_blob_hash().as_bytes().try_into().unwrap())
        .await
        .unwrap()
        .expect("missing test blob");

    // Make sure we can create & write a blob with the BlobCreator.
    let data = "Hello, world!".as_bytes();
    let blob_creator =
        connect_to_protocol_at_dir_root::<ffxfs::BlobCreatorMarker>(&blob_svc).unwrap();
    let hash = write_blob(blob_creator, data).await;
    let hash = hash.as_bytes().try_into().unwrap();

    // Ensure we can read the blob back.
    let vmo = blob_reader.get_vmo(hash).await.unwrap().unwrap();
    let mut buff = vec![];
    buff.resize(data.len(), 0);
    vmo.read(&mut buff, 0).unwrap();
    assert_eq!(data, buff.as_slice());
    fixture.tear_down().await;
}

#[fuchsia::test]
async fn mount_system_blob_volume_with_format() {
    let system_container = build_fxblob().await;
    let fixture = start_recovery_fixture(system_container).await;
    let recovery: ffshost::RecoveryProxy =
        fixture.realm.root.connect_to_protocol_at_exposed_dir().unwrap();

    // Format the existing blob volume and then mount the newly created volume.
    recovery.format_system_blob_volume().await.unwrap().expect("FormatSystemBlobVolume failed");

    let (blob_exposed_dir, blob_exposed_dir_server) = create_proxy::<fio::DirectoryMarker>();
    recovery
        .mount_system_blob_volume(blob_exposed_dir_server)
        .await
        .unwrap()
        .expect("MountSystemBlobVolume failed");
    let blob_root = fuchsia_fs::directory::open_directory(
        &blob_exposed_dir,
        "root",
        fio::PERM_READABLE | fio::Flags::PERM_INHERIT_EXECUTE,
    )
    .await
    .unwrap();
    let blob_svc =
        fuchsia_fs::directory::open_directory(&blob_exposed_dir, "svc", fio::PERM_READABLE)
            .await
            .unwrap();

    // Verify there are no blobs since we should have formatted the blob volume.
    assert!(fuchsia_fs::directory::readdir(&blob_root).await.unwrap().is_empty());

    // Make sure we can create & write a blob with the BlobCreator.
    let data = "Hello, world!".as_bytes();
    let blob_creator =
        connect_to_protocol_at_dir_root::<ffxfs::BlobCreatorMarker>(&blob_svc).unwrap();
    let hash = write_blob(blob_creator, data).await;
    let hash = hash.as_bytes().try_into().unwrap();

    // Ensure we can read the blob back.
    let blob_reader =
        connect_to_protocol_at_dir_root::<ffxfs::BlobReaderMarker>(&blob_svc).unwrap();
    let vmo = blob_reader.get_vmo(hash).await.unwrap().unwrap();
    let mut buff = vec![];
    buff.resize(data.len(), 0);
    vmo.read(&mut buff, 0).unwrap();
    assert_eq!(data, buff.as_slice());

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn mount_system_blob_volume_handles_corrupt_partition_table() {
    // Initialize the system GPT but don't actually format or initialize any volumes.
    let system_container = {
        let mut builder = new_builder();
        builder.with_disk().with_gpt();
        let fixture = builder.build().await;
        fixture.tear_down().await.unwrap()
    };
    let fixture = start_recovery_fixture(system_container).await;
    let recovery: ffshost::RecoveryProxy =
        fixture.realm.root.connect_to_protocol_at_exposed_dir().unwrap();

    // Both FormatSystemBlobVolume and MountSystemBlobVolume require a valid system container
    // partition to be present in the GPT and the system container must have a valid fxfs partition.
    let _ = recovery
        .format_system_blob_volume()
        .await
        .unwrap()
        .expect_err("FormatSystemBlobVolume succeeded on a corrupt filesystem!");

    let (_blob_exposed_dir, blob_exposed_dir_server) = create_proxy::<fio::DirectoryMarker>();
    let _ = recovery
        .mount_system_blob_volume(blob_exposed_dir_server)
        .await
        .unwrap()
        .expect_err("MountSystemBlobVolume succeeded on a corrupt filesystem!");
    fixture.tear_down().await;
}

#[fuchsia::test]
async fn mount_system_blob_volume_lifecycle() {
    let system_container = build_fxblob().await;
    let fixture = start_recovery_fixture(system_container).await;
    let recovery: ffshost::RecoveryProxy =
        fixture.realm.root.connect_to_protocol_at_exposed_dir().unwrap();

    // Invoke MountSystemBlobVolume
    let (blob_exposed_dir, blob_exposed_dir_server) = create_proxy::<fio::DirectoryMarker>();
    recovery
        .mount_system_blob_volume(blob_exposed_dir_server)
        .await
        .unwrap()
        .expect("MountSystemBlobVolume unexpectedly failed");
    let blob_svc =
        fuchsia_fs::directory::open_directory(&blob_exposed_dir, "svc", fio::PERM_READABLE)
            .await
            .unwrap();

    // Make sure the BlobReader is working and being served.
    let hash = test_blob_hash();
    let hash = hash.as_bytes().try_into().unwrap();
    let blob_reader =
        connect_to_protocol_at_dir_root::<ffxfs::BlobReaderMarker>(&blob_svc).unwrap();
    blob_reader.get_vmo(hash).await.unwrap().expect("missing blob");

    // Ensure we shut the filesystem down when we close (or drop) the last connection to the exposed
    // directory. The BlobReader service should start failing soon afterwards.
    blob_exposed_dir.close().await.unwrap().expect("close failed");
    loop {
        if let Err(e) = blob_reader.get_vmo(hash).await {
            assert!(e.is_closed());
            break;
        }
        fasync::Timer::new(std::time::Duration::from_millis(100)).await;
    }

    fixture.tear_down().await;
}

/// Ensure multiple requests to mount the system blob volume are handled gracefully. While a caller
/// still has an open handle to the system container, new mount requests should remain pending.
#[fuchsia::test]
async fn mount_system_blob_volume_handles_concurrency() {
    let system_container = build_fxblob().await;
    let fixture = start_recovery_fixture(system_container).await;

    async fn write_blob_and_get_hash(fixture: &TestFixture, data: &str) -> fuchsia_hash::Hash {
        let recovery: ffshost::RecoveryProxy =
            fixture.realm.root.connect_to_protocol_at_exposed_dir().unwrap();
        let (blob_exposed_dir, blob_exposed_dir_server) = create_proxy::<fio::DirectoryMarker>();
        recovery
            .mount_system_blob_volume(blob_exposed_dir_server)
            .await
            .unwrap()
            .expect("MountSystemBlobVolume unexpectedly failed");
        let blob_svc =
            fuchsia_fs::directory::open_directory(&blob_exposed_dir, "svc", fio::PERM_READABLE)
                .await
                .unwrap();
        let blob_creator =
            connect_to_protocol_at_dir_root::<ffxfs::BlobCreatorMarker>(&blob_svc).unwrap();
        write_blob(blob_creator, data.as_bytes()).await
    }

    // Although the protocol only allows a single instance to access the system container, we should
    // be able to gracefully wait for each future to complete. The recovery method should only
    // return once a previous iteration has completed and the filesystem was shut down cleanly.
    let blob_data = [
        "Goodbye stranger",
        "It's been nice...",
        "And I think my spaceship knows what I must do",
        "ground control to Major Tom",
    ];
    let futures: Vec<_> =
        blob_data.into_iter().map(|data| write_blob_and_get_hash(&fixture, data)).collect();
    let hashes = futures::future::join_all(futures).await;

    // Ensure we can read back all of the blobs we just wrote.
    let recovery: ffshost::RecoveryProxy =
        fixture.realm.root.connect_to_protocol_at_exposed_dir().unwrap();
    let (blob_exposed_dir, blob_exposed_dir_server) = create_proxy::<fio::DirectoryMarker>();
    recovery
        .mount_system_blob_volume(blob_exposed_dir_server)
        .await
        .unwrap()
        .expect("MountSystemBlobVolume unexpectedly failed");
    let blob_reader =
        connect_to_protocol_at_dir_svc::<ffxfs::BlobReaderMarker>(&blob_exposed_dir).unwrap();
    for hash in hashes {
        blob_reader
            .get_vmo(hash.as_bytes().try_into().unwrap())
            .await
            .unwrap()
            .expect("blob missing");
    }

    fixture.tear_down().await;
}

/// Helper function that creates a new fxfs system image containing a single blob volume with
/// some test blobs. The output is a set of VMOs containing the system image in the Android sparse
/// format. `max_chunk_size` defines the maximum size of the data payload in a given chunk.
async fn create_sparse_fxblob_image(max_chunk_size: u64) -> Vec<zx::Vmo> {
    const NUM_BLOCKS: u64 = 512;
    const DEVICE_SIZE: u64 = BLOCK_SIZE as u64 * NUM_BLOCKS;
    let fxblob_vmo = zx::Vmo::create(DEVICE_SIZE).unwrap();
    let image_size = {
        let block_server = Arc::new(VmoBackedServer::from_vmo(
            BLOCK_SIZE,
            fxblob_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
        ));
        let device = DeviceHolder::new(
            BlockDevice::new(
                RemoteBlockClient::new(block_server.connect::<fblock::BlockProxy>())
                    .await
                    .expect("Unable to create block client"),
                false,
            )
            .await
            .unwrap(),
        );
        let fxblob = FxBlobBuilder::new(device, /*compression_enabled*/ true).await.unwrap();
        for data in TEST_BLOBS {
            let blob = fxblob.generate_blob(data.to_vec()).unwrap();
            fxblob.install_blob(&blob).await.unwrap();
        }
        fxblob.finalize().await.unwrap()
    };

    // Build a set of sparse chunks based on the `max_chunk_size`, which can be thought of as
    // equivalent to the maximum download size reported by a fastboot device.
    let mut vmos = Vec::new();
    let mut offset = 0;
    while offset < image_size {
        let mut builder = SparseImageBuilder::new().set_block_size(BLOCK_SIZE);
        if offset > 0 {
            builder = builder.add_source(DataSource::Skip(offset));
        }
        let chunk_size = std::cmp::min(max_chunk_size, image_size - offset);
        builder = builder.add_source(DataSource::Vmo {
            vmo: fxblob_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            size: chunk_size,
            offset,
        });
        offset += chunk_size;
        if offset == image_size {
            builder = builder.add_source(DataSource::Skip(DEVICE_SIZE - image_size));
        }
        vmos.push(builder.build_vmo().unwrap());
    }
    vmos
}

/// Verifies that the set of [`TEST_BLOBS`] are present in the installed system image on `disk`,
/// and that the `data` volume still exists and contains the test data file.
async fn verify_installed_fxblob_system(disk: Disk) {
    let fixture = new_builder().with_disk_from(disk).build().await;
    fixture.check_fs_type("data", data_fs_type()).await;
    fixture.check_test_data_file().await;
    fixture.check_fs_type("blob", blob_fs_type()).await;

    let reader = connect_to_protocol_at_dir_root::<ffxfs::BlobReaderMarker>(
        fixture.realm.root.get_exposed_dir(),
    )
    .expect("failed to connect to the BlobReader");

    // The original blob should be missing.
    assert_eq!(
        reader
            .get_vmo(&test_blob_hash().into())
            .await
            .unwrap()
            .map_err(zx::Status::from_raw)
            .expect_err("old blob should be gone"),
        zx::Status::NOT_FOUND
    );

    // We should be able to find and read the blobs we installed.
    for data in TEST_BLOBS {
        let hash = fuchsia_merkle::from_slice(data).root();
        let vmo = reader
            .get_vmo(&hash.as_bytes().try_into().unwrap())
            .await
            .unwrap()
            .map_err(zx::Status::from_raw)
            .expect("GetVmo failed");
        let mut buf = vec![0; data.len()];
        vmo.read(&mut buf, 0).unwrap();
        assert_eq!(buf.as_slice(), data);
    }

    fixture.tear_down().await;
}

/// Test writing and installing an entire system image as a single sparse chunk.
#[fuchsia::test]
async fn write_and_install_blob_image_oneshot() {
    let system_container = build_fxblob().await;
    let fixture = start_recovery_fixture(system_container).await;
    let image = {
        let vmos = create_sparse_fxblob_image(u64::MAX).await;
        assert!(vmos.len() == 1);
        vmos.into_iter().next().unwrap()
    };
    let recovery: ffshost::RecoveryProxy =
        fixture.realm.root.connect_to_protocol_at_exposed_dir().unwrap();
    recovery
        .write_system_blob_image(image)
        .await
        .unwrap()
        .map_err(zx::Status::from_raw)
        .expect("WriteVolume failed");

    recovery
        .install_system_blob_image()
        .await
        .unwrap()
        .map_err(zx::Status::from_raw)
        .expect("InstallVolume failed");

    // Tear down the fixture, mount the system image as normal, and verify the blobs.
    let system_container = fixture.tear_down().await.unwrap();
    verify_installed_fxblob_system(system_container).await;
}

/// Ensure we can write and install a new system blob image in multiple sparse chunks.
#[fuchsia::test]
async fn write_and_install_blob_image_chunked() {
    let system_container = build_fxblob().await;
    let fixture = start_recovery_fixture(system_container).await;
    let sparse_vmos = create_sparse_fxblob_image(SPARSE_CHUNK_SIZE).await;
    assert!(sparse_vmos.len() > 1, "this test requires multiple chunks");
    let recovery: ffshost::RecoveryProxy =
        fixture.realm.root.connect_to_protocol_at_exposed_dir().unwrap();
    for (i, vmo) in sparse_vmos.into_iter().enumerate() {
        recovery
            .write_system_blob_image(vmo)
            .await
            .unwrap()
            .map_err(zx::Status::from_raw)
            .unwrap_or_else(|e| panic!("failed to write chunk {i}: {e}"));
    }

    recovery
        .install_system_blob_image()
        .await
        .unwrap()
        .map_err(zx::Status::from_raw)
        .expect("InstallVolume failed");

    // Tear down the fixture, mount the system image as normal, and verify the blobs.
    let system_container = fixture.tear_down().await.unwrap();
    verify_installed_fxblob_system(system_container).await;
}

/// Ensure that if we fail to install a blob image, we can re-attempt the process without rebooting.
#[fuchsia::test]
async fn write_and_install_blob_image_can_reattempt() {
    let system_container = build_fxblob().await;
    let fixture = start_recovery_fixture(system_container).await;
    let sparse_vmos = create_sparse_fxblob_image(SPARSE_CHUNK_SIZE).await;
    assert!(sparse_vmos.len() > 1, "this test requires multiple chunks");
    let recovery: ffshost::RecoveryProxy =
        fixture.realm.root.connect_to_protocol_at_exposed_dir().unwrap();

    // Write only the first chunk, after which we should fail to install the volume since the image
    // is incomplete.
    {
        let first_chunk =
            sparse_vmos.first().unwrap().duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        recovery
            .write_system_blob_image(first_chunk)
            .await
            .unwrap()
            .map_err(zx::Status::from_raw)
            .expect("failed to write chunk");
        recovery
            .install_system_blob_image()
            .await
            .unwrap()
            .map_err(zx::Status::from_raw)
            .expect_err("installation should fail if the image is incomplete");
    }

    // Now write all the chunks and try again.
    for (i, vmo) in sparse_vmos.into_iter().enumerate() {
        recovery
            .write_system_blob_image(vmo)
            .await
            .unwrap()
            .map_err(zx::Status::from_raw)
            .unwrap_or_else(|e| panic!("failed to write chunk {i}: {e}"));
    }
    recovery
        .install_system_blob_image()
        .await
        .unwrap()
        .map_err(zx::Status::from_raw)
        .expect("volume installation failed");

    // Tear down the fixture, mount the system image as normal, and verify the blobs.
    let system_container = fixture.tear_down().await.unwrap();
    verify_installed_fxblob_system(system_container).await;
}

/// Verifies that if we write a new blob volume image but do not install it, the uninstalled volume
/// containing the image file is cleaned up automatically when we boot up normally.
#[fuchsia::test]
async fn uninstalled_blob_volume_is_cleaned_up_on_normal_boot() {
    let system_container = build_fxblob().await;
    let fixture = start_recovery_fixture(system_container).await;
    let image = {
        let vmos = create_sparse_fxblob_image(u64::MAX).await;
        assert!(vmos.len() == 1);
        vmos.into_iter().next().unwrap()
    };
    let recovery: ffshost::RecoveryProxy =
        fixture.realm.root.connect_to_protocol_at_exposed_dir().unwrap();
    // Write the image but leave it uninstalled.
    recovery
        .write_system_blob_image(image)
        .await
        .unwrap()
        .map_err(zx::Status::from_raw)
        .expect("failed to write image");

    // Tear down the fixture and explicitly check that we have a new volume.
    let system_container = fixture.tear_down().await.unwrap();
    let volumes = list_all_fxfs_volumes(&system_container).await;
    assert!(volumes.len() > expected_fxblob_volumes().len());

    // Now boot up normally and ensure the existing blob volume remains untouched.
    let fixture = new_builder().with_disk_from(system_container).build().await;
    fixture.check_test_blob(true).await;

    // Now that we booted up normally, tear down the fixture again and ensure the extra volume is
    // gone without any explicit action on our part.
    let system_container = fixture.tear_down().await.unwrap();
    let volumes = list_all_fxfs_volumes(&system_container).await;
    assert_eq!(volumes, expected_fxblob_volumes());
}
