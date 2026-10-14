// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Integration tests for partition management, GPT detection, and block device topology.
//!
//! This suite exercises fshost's block device matching pipeline and partition management. For
//! example, this includes partition table discovery (GPT initialization and reset), block device
//! topology publishing (such as `/block` and `/debug_block`), device configuration routing, and
//! partition stitching (`merge_super_and_userdata`).

use crypt_policy as _;
use fidl::endpoints::{DiscoverableProtocolMarker as _, ServiceMarker as _};
use fidl_fuchsia_fshost::RecoveryProxy;
use fidl_fuchsia_io as fio;
use fidl_fuchsia_storage_block as fpartition;
use fidl_fuchsia_storage_block::BlockMarker;
use fidl_fuchsia_storage_partitions as fpartitions;
use fshost_test_fixture::disk_builder::{Disk, DiskBuilder, TEST_DISK_BLOCK_SIZE};
use fshost_test_fixture::{
    BlockDeviceConfig, BlockDeviceIdentifiers, BlockDeviceParent, TestFixture,
};
use fuchsia_async as fasync;
use fuchsia_component::client::connect_to_named_protocol_at_dir_root;

pub mod config;

use config::{blob_fs_type, data_fs_spec, data_fs_type, new_builder, volumes_spec};

fn make_partition_entry(
    name: &str,
    type_guid: fpartition::Guid,
    instance_guid: fpartition::Guid,
    start_block: u64,
    num_blocks: u64,
    flags: u64,
) -> fpartitions::PartitionEntry {
    fpartitions::PartitionEntry {
        name: name.to_string(),
        type_guid,
        instance_guid,
        start_block,
        num_blocks,
        flags,
    }
}

#[allow(unused)]
fn make_partition_info(
    name: &str,
    type_guid: fpartition::Guid,
    instance_guid: fpartition::Guid,
    start_block_offset: u64,
    num_blocks: u64,
    flags: u64,
) -> fpartitions::PartitionInfo {
    fpartitions::PartitionInfo {
        name: Some(name.to_string()),
        type_guid: Some(type_guid),
        instance_guid: Some(instance_guid),
        start_block_offset: Some(start_block_offset),
        num_blocks: Some(num_blocks),
        flags: Some(flags),
        ..Default::default()
    }
}

async fn gpt_num_partitions(fixture: &TestFixture) -> usize {
    let partitions = fixture.dir(
        fidl_fuchsia_storage_partitions::PartitionServiceMarker::SERVICE_NAME,
        fuchsia_fs::PERM_READABLE,
    );
    fuchsia_fs::directory::readdir(&partitions).await.expect("Failed to read partitions").len()
}

#[fuchsia::test]
async fn initialized_gpt() {
    let mut builder = new_builder();
    builder.with_disk().format_volumes(volumes_spec()).with_gpt().format_data(data_fs_spec());
    builder.with_extra_disk().set_uninitialized();
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;
    fixture.check_test_data_file().await;
    fixture.check_test_blob().await;

    assert_eq!(gpt_num_partitions(&fixture).await, 1);

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn uninitialized_gpt() {
    let mut builder = new_builder().with_uninitialized_disk();
    builder.fshost().set_config_value("ramdisk_image", true);
    builder.with_extra_disk().set_uninitialized();
    let fixture = builder.build().await;

    assert_eq!(gpt_num_partitions(&fixture).await, 0);

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn reset_uninitialized_gpt() {
    let mut builder = new_builder().with_uninitialized_disk();
    builder.fshost().set_config_value("ramdisk_image", true);
    builder.with_extra_disk().set_uninitialized();
    let fixture = builder.build().await;

    assert_eq!(gpt_num_partitions(&fixture).await, 0);

    let recovery: RecoveryProxy = fixture.realm.root.connect_to_protocol_at_exposed_dir().unwrap();
    recovery
        .init_system_partition_table(&[make_partition_entry(
            "part",
            fpartition::Guid { value: [0xabu8; 16] },
            fpartition::Guid { value: [0xcdu8; 16] },
            4,
            1,
            0,
        )])
        .await
        .expect("FIDL error")
        .expect("init_system_partition_table failed");

    assert_eq!(gpt_num_partitions(&fixture).await, 1);

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn reset_initialized_gpt() {
    let mut builder = new_builder();
    builder.with_disk().format_volumes(volumes_spec()).with_gpt().format_data(data_fs_spec());
    builder.fshost().set_config_value("ramdisk_image", true);
    builder.with_extra_disk().set_uninitialized();
    let fixture = builder.build().await;

    assert_eq!(gpt_num_partitions(&fixture).await, 1);

    let recovery: RecoveryProxy = fixture.realm.root.connect_to_protocol_at_exposed_dir().unwrap();
    recovery
        .init_system_partition_table(&[
            make_partition_entry(
                "part",
                fpartition::Guid { value: [0xabu8; 16] },
                fpartition::Guid { value: [0xcdu8; 16] },
                4,
                1,
                0,
            ),
            make_partition_entry(
                "part2",
                fpartition::Guid { value: [0x11u8; 16] },
                fpartition::Guid { value: [0x22u8; 16] },
                5,
                1,
                0,
            ),
        ])
        .await
        .expect("FIDL error")
        .expect("init_system_partition_table failed");

    assert_eq!(gpt_num_partitions(&fixture).await, 2);

    fixture.tear_down().await;
}

// Tests that discovered block devices are published to fshost's `/debug_block` directory,
// verifying that device sources, the `fuchsia.storage.block.Block` protocol, and topological
// `bus_path` metadata are correctly exposed.
#[fuchsia::test]
async fn debug_block_directory() {
    let mut builder = new_builder();
    builder.with_disk().format_volumes(volumes_spec()).format_data(data_fs_spec());
    let fixture = builder.build().await;

    // Make sure the filesystems are enumerated before trying to access the block devices. The
    // debug directory is populated as the devices are emitted by the watcher.
    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    let block = fuchsia_fs::directory::open_directory(
        fixture.exposed_dir(),
        "debug_block",
        fio::PERM_READABLE,
    )
    .await
    .unwrap();

    // Check that the block directory contains some of the required things for the shell tools
    let source =
        fuchsia_fs::directory::open_file(&block, "000/source", fio::PERM_READABLE).await.unwrap();
    // This is a smoke check - we can't check for a concrete source because it's different (and
    // potentially unstable) depending on the configuration, and it's not that useful to be a
    // change detector.
    assert!(fuchsia_fs::file::read_to_string(&source).await.unwrap().len() > 0);

    let volume = connect_to_named_protocol_at_dir_root::<BlockMarker>(
        &block,
        "000/fuchsia.storage.block.Block",
    )
    .unwrap();
    assert_eq!(
        volume.get_info().await.unwrap().map_err(zx::Status::err_from_raw).unwrap().block_size,
        512,
    );

    let bus_topology =
        fuchsia_fs::directory::open_file(&block, "000/bus_path", fio::PERM_READABLE).await.unwrap();
    let content = fuchsia_fs::file::read_to_string(&bus_topology).await.unwrap();
    // Don't be too strict; just make sure it isn't <unknown> or <none> which come from fshost.
    assert!(!content.is_empty());
    assert!(!content.starts_with("<"));

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn expose_unmanaged_block_devices() {
    let mut builder = new_builder();
    builder.with_disk().format_volumes(volumes_spec()).with_gpt().format_data(data_fs_spec());
    builder.with_extra_disk().set_uninitialized().size(8192);
    let fixture = builder.build().await;

    // Make sure the filesystems are enumerated before trying to access the block devices. The block
    // directory is populated as the devices are emitted by the watcher.
    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    let block_dir =
        fuchsia_fs::directory::open_directory(fixture.exposed_dir(), "block", fio::PERM_READABLE)
            .await
            .unwrap();
    let mut dirents = fuchsia_fs::directory::readdir(&block_dir).await.expect("readdir failed");
    let device_path = dirents.pop().unwrap().name;
    assert!(dirents.is_empty(), "Multiple devices published");

    let path = format!("{}/{}", &device_path, BlockMarker::PROTOCOL_NAME);
    let volume = fuchsia_component::client::connect_to_named_protocol_at_dir_root::<BlockMarker>(
        &block_dir, &path,
    )
    .unwrap();
    let metadata =
        volume.get_metadata().await.expect("FIDL error").expect("Failed to get metadata");
    assert_eq!(metadata.num_blocks, Some(8192 / TEST_DISK_BLOCK_SIZE as u64));

    fixture.tear_down().await;
}

// Regression test for https://fxbug.dev/408423972.
#[fuchsia::test]
async fn fuse_gpt_once_container_found() {
    let mut builder = new_builder();
    builder.with_disk().format_volumes(volumes_spec());
    let fixture = builder.build().await;

    let partitions = fixture.dir(
        fidl_fuchsia_storage_partitions::PartitionServiceMarker::SERVICE_NAME,
        fuchsia_fs::PERM_READABLE,
    );
    let task = fasync::Task::spawn(async move {
        // This call will block until fshost finds the system container, and at that point it will
        // fuse shut PartitionService, failing all callers.  See logic in mount_fxblob in
        // FshostEnvironment.
        fuchsia_fs::directory::readdir(&partitions)
            .await
            .expect_err("readdir should (eventually) fail")
    });

    // Once the filesystems are enumerated, the above task should be unblocked.
    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    task.await;

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn device_config() {
    let builder = new_builder().with_device_config(vec![
        BlockDeviceConfig {
            device: String::from("fts"),
            from: BlockDeviceIdentifiers {
                label: String::from("fts"),
                parent: BlockDeviceParent::Gpt,
            },
        },
        BlockDeviceConfig {
            device: String::from("test-device"),
            from: BlockDeviceIdentifiers {
                label: String::from("boot_a"),
                parent: BlockDeviceParent::Gpt,
            },
        },
        BlockDeviceConfig {
            device: String::from("boot_b"),
            from: BlockDeviceIdentifiers {
                label: String::from("boot_b"),
                parent: BlockDeviceParent::Gpt,
            },
        },
    ]);
    let mut fixture = builder.build().await;

    // By attempting to open and use the block directory before we add the disk, we confirm that
    // queuing requests for configured devices works as expected. If the queuing doesn't work, this
    // will fail with PEER_CLOSED instead.
    let fts_dir = fixture.dir("block/fts", fio::PERM_READABLE);
    let volume =
        fuchsia_component::client::connect_to_protocol_at_dir_root::<BlockMarker>(&fts_dir)
            .unwrap();
    let task =
        fasync::Task::spawn(
            async move { volume.get_metadata().await.unwrap().unwrap().num_blocks },
        );

    let mut disk = DiskBuilder::new();
    disk.with_gpt()
        .format_volumes(volumes_spec())
        .with_extra_gpt_partition("fts", 1)
        .with_extra_gpt_partition("boot_a", 1)
        .with_extra_gpt_partition("boot_b", 1);
    fixture.add_main_disk(Disk::Builder(disk)).await;
    // Add a second disk, to make sure that fshost only enumerates the right one.  Fshost
    // disambiguates by the presence of the system partition.
    let mut secondary_disk = DiskBuilder::new();
    secondary_disk.with_gpt().with_extra_gpt_partition("fts", 5);
    fixture.add_disk(Disk::Builder(secondary_disk)).await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    assert_eq!(task.await, Some(1));

    let fts_dir = fixture.dir("block/fts", fio::PERM_READABLE);
    let volume =
        fuchsia_component::client::connect_to_protocol_at_dir_root::<BlockMarker>(&fts_dir)
            .unwrap();
    let metadata = volume.get_metadata().await.unwrap().unwrap();
    assert_eq!(metadata.num_blocks, Some(1));

    let boot_a_dir = fixture.dir("block/test-device", fio::PERM_READABLE);
    let volume =
        fuchsia_component::client::connect_to_protocol_at_dir_root::<BlockMarker>(&boot_a_dir)
            .unwrap();
    let metadata = volume.get_metadata().await.unwrap().unwrap();
    assert_eq!(metadata.num_blocks, Some(1));

    let boot_b_dir = fixture.dir("block/boot_b", fio::PERM_READABLE);
    let volume =
        fuchsia_component::client::connect_to_protocol_at_dir_root::<BlockMarker>(&boot_b_dir)
            .unwrap();
    let metadata = volume.get_metadata().await.unwrap().unwrap();
    assert_eq!(metadata.num_blocks, Some(1));

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn gpt_all_binds_multiple_disks() {
    let mut builder = new_builder().with_device_config(vec![BlockDeviceConfig {
        device: String::from("test-part"),
        from: BlockDeviceIdentifiers {
            label: String::from("test_part"),
            parent: BlockDeviceParent::Gpt,
        },
    }]);
    builder.fshost().set_config_value("gpt_all", true);
    builder.with_disk().format_volumes(volumes_spec()).with_gpt().format_data(data_fs_spec());
    builder
        .with_extra_disk()
        .with_gpt()
        .with_unformatted_volume_manager()
        .with_extra_gpt_partition("test_part", 1);
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    // Check that the extra partition is available.
    let test_part_dir = fixture.dir("block/test-part", fio::PERM_READABLE);
    let volume =
        fuchsia_component::client::connect_to_protocol_at_dir_root::<BlockMarker>(&test_part_dir)
            .unwrap();
    let metadata = volume.get_metadata().await.unwrap().unwrap();
    assert_eq!(metadata.num_blocks, Some(1));

    // One of the disks has one gpt partition and the other has two. Because of a quirk of the
    // integration tests, the one that actually doesn't have any formatted information (the one
    // with two partitions) gets registered as the system gpt and exported via the partition
    // service. We double check that happened how we expect.
    assert_eq!(gpt_num_partitions(&fixture).await, 2);

    fixture.tear_down().await;
}

#[cfg(feature = "fxblob")]
mod fxblob {
    use super::*;
    use fshost_test_fixture::VFS_TYPE_FXFS;
    use fshost_test_fixture::disk_builder::DEFAULT_DISK_SIZE;

    // This test exercises merging super and userdata into a single logical "fxfs" partition. The
    // GPT will be formatted "super" (which contains Fxfs) and "userdata" (which is unformatted),
    // and it is expected that fshost sees a merged "fxfs" partition. The test only works on fxfs,
    // because fxfs supports mounting on a larger partition than it was formatted with.
    #[fuchsia::test]
    async fn merge_super_and_userdata() {
        use fidl_fuchsia_storage_partitions::OverlayPartitionMarker;
        use fs_management::FVM_TYPE_GUID;
        use fshost_test_fixture::disk_builder::{DEFAULT_TEST_TYPE_GUID, FVM_PART_INSTANCE_GUID};

        // fxfs will ignore blocks that are not aligned to a page, so make sure that we give at
        // least this many blocks to super, so we can exercise Fxfs claiming the additional space.
        const USERDATA_NUM_BLOCKS: u64 = 4096 / TEST_DISK_BLOCK_SIZE as u64;

        let mut builder = new_builder();
        builder.fshost().set_config_value("merge_super_and_userdata", true);
        let mut fixture = builder.build().await;

        let mut disk = DiskBuilder::new();
        disk.with_gpt()
            .format_volumes(volumes_spec())
            .with_system_partition_label("super")
            // NOTE: The "userdata" partition will be physically contiguous with the "super"
            // partition.
            .with_extra_gpt_partition("userdata", USERDATA_NUM_BLOCKS)
            .with_extra_gpt_partition("other", 1);
        fixture.add_main_disk(Disk::Builder(disk)).await;

        fixture.check_fs_type("blob", blob_fs_type()).await;
        fixture.check_fs_type("data", data_fs_type()).await;
        fixture.check_test_blob().await;

        let partitions = fixture.dir(
            fidl_fuchsia_storage_partitions::PartitionServiceMarker::SERVICE_NAME,
            fuchsia_fs::PERM_READABLE,
        );
        let instances = fuchsia_fs::directory::readdir(&partitions)
            .await
            .expect("readdir failed")
            .into_iter()
            .map(|entry| entry.name)
            .collect::<Vec<_>>();
        assert_eq!(instances.len(), 2);
        let mut found_merged_partition = false;
        for instance in instances {
            let dir =
                fuchsia_fs::directory::open_directory(&partitions, &instance, fio::PERM_READABLE)
                    .await
                    .expect("open dir failed");
            let volume =
                connect_to_named_protocol_at_dir_root::<BlockMarker>(&dir, "volume").unwrap();
            let metadata =
                volume.get_metadata().await.expect("FIDL error").expect("Failed to get metadata");
            assert_ne!(metadata.name.as_ref().unwrap(), "super");
            assert_ne!(metadata.name.as_ref().unwrap(), "userdata");
            if metadata.name.as_ref().unwrap() == "super_and_userdata" {
                found_merged_partition = true;
                const SUPER_NUM_BLOCKS: u64 = DEFAULT_DISK_SIZE / TEST_DISK_BLOCK_SIZE as u64 - 138;
                assert_eq!(metadata.start_block_offset, None);
                assert_eq!(metadata.flags, None);
                assert_eq!(metadata.num_blocks, Some(SUPER_NUM_BLOCKS + USERDATA_NUM_BLOCKS));
                assert_eq!(metadata.type_guid, Some(fpartition::Guid { value: FVM_TYPE_GUID }));
                assert_eq!(
                    metadata.instance_guid,
                    Some(fpartition::Guid { value: FVM_PART_INSTANCE_GUID })
                );
                let overlay = connect_to_named_protocol_at_dir_root::<OverlayPartitionMarker>(
                    &dir, "overlay",
                )
                .unwrap();
                let infos = overlay
                    .get_partitions()
                    .await
                    .expect("FIDL error")
                    .expect("Failed to get parts");
                assert_eq!(
                    infos,
                    vec![
                        make_partition_info(
                            "super",
                            fpartition::Guid { value: FVM_TYPE_GUID },
                            fpartition::Guid { value: FVM_PART_INSTANCE_GUID },
                            64,
                            SUPER_NUM_BLOCKS,
                            0,
                        ),
                        make_partition_info(
                            "userdata",
                            fpartition::Guid { value: DEFAULT_TEST_TYPE_GUID },
                            fpartition::Guid { value: FVM_PART_INSTANCE_GUID },
                            64 + SUPER_NUM_BLOCKS,
                            USERDATA_NUM_BLOCKS,
                            0,
                        ),
                    ]
                )
            }
        }
        assert!(found_merged_partition, "No super+userdata found");

        fixture.tear_down().await;
    }

    #[fuchsia::test]
    async fn test_provision_fxfs() {
        let mut builder = new_builder();
        builder.fshost().set_config_value("provision_fxfs", true);
        builder.fshost().set_config_value("merge_super_and_userdata", true);
        let mut fixture = builder.build().await;

        let mut disk = DiskBuilder::new();
        // Use unformatted volume manager to build an unformatted disk
        disk.with_gpt()
            .with_unformatted_volume_manager()
            .with_system_partition_label("super")
            .with_extra_gpt_partition("userdata", 1)
            .with_extra_gpt_partition("other", 1);
        fixture.add_main_disk(Disk::Builder(disk)).await;

        fixture.check_system_partitions(vec!["other", "super_and_userdata"]).await;
        fixture.check_fs_type("data", VFS_TYPE_FXFS).await;
        fixture.check_test_data_file().await;

        fixture.tear_down().await;
    }
}
