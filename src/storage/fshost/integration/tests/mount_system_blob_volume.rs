// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Tests for fuchsia.fshost/Recovery.MountSystemBlobVolume. This method is intended for use in
//! recovery environments for fxblob-based products to allow access to the system container's blob
//! volume.
//!
//! This allows a system update/OTA to be applied from a recovery image.

use device_watcher::recursive_wait;
use fidl::endpoints::{ServiceMarker as _, create_proxy};
use fshost_test_fixture::disk_builder::{Disk, test_blob_hash};
use fshost_test_fixture::{TestFixture, write_blob};
use fuchsia_component::client::{connect_to_protocol_at_dir_root, connect_to_protocol_at_dir_svc};
use {
    fidl_fuchsia_fshost as fshost, fidl_fuchsia_fxfs as ffxfs, fidl_fuchsia_io as fio,
    fuchsia_async as fasync,
};
pub mod config;
use config::{blob_fs_type, data_fs_spec, data_fs_type, new_builder, volumes_spec};

// TODO(https://fxbug.dev/42072287): Remove hardcoded paths
const GPT_PATH: &'static str = "/part-000/block";

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
    if cfg!(feature = "storage-host") {
        recursive_wait(
            &fixture.dir(
                fidl_fuchsia_storage_partitions::PartitionServiceMarker::SERVICE_NAME,
                fio::PERM_READABLE,
            ),
            "part-000",
        )
        .await
        .unwrap();
    } else {
        let ramdisk_dir =
            fixture.ramdisks.first().expect("no ramdisks?").as_dir().expect("invalid dir proxy");
        recursive_wait(ramdisk_dir, GPT_PATH).await.unwrap();
    }
    fixture
}

#[fuchsia::test]
async fn mount_system_blob_volume() {
    let system_container = build_fxblob().await;
    let fixture = start_recovery_fixture(system_container).await;
    let recovery: fshost::RecoveryProxy =
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
    let recovery: fshost::RecoveryProxy =
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
    let recovery: fshost::RecoveryProxy =
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
    let recovery: fshost::RecoveryProxy =
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
        let recovery: fshost::RecoveryProxy =
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
    let recovery: fshost::RecoveryProxy =
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
