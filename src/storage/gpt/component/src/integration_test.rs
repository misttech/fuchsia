// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::sync::Arc;

use block_client::{BlockClient as _, MutableBufferSlice, RemoteBlockClient};
use fidl::endpoints::ServiceMarker as _;
use fidl_fuchsia_io as fio;
use fidl_fuchsia_storage_block as fblock;
use fidl_fuchsia_storage_partitions as fpartitions;
use fuchsia_component::client::connect::connect_to_protocol_at_dir_root;
use test_case::test_case;
use vmo_backed_block_server::{VmoBackedServer, VmoBackedServerConnector};

fn make_partition_entry(
    name: &str,
    type_guid: fblock::Guid,
    instance_guid: fblock::Guid,
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

fn entry_to_info(entry: &fpartitions::PartitionEntry) -> fpartitions::PartitionInfo {
    fpartitions::PartitionInfo {
        name: Some(entry.name.clone()),
        type_guid: Some(entry.type_guid),
        instance_guid: Some(entry.instance_guid),
        start_block_offset: Some(entry.start_block),
        num_blocks: Some(entry.num_blocks),
        flags: Some(entry.flags),
        ..Default::default()
    }
}

fn make_partition_info(
    name: &str,
    type_guid: fblock::Guid,
    instance_guid: fblock::Guid,
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

#[test_case(true; "overlay_enabled")]
#[test_case(false; "overlay_disabled")]
#[fuchsia::test]
async fn test_overlay(overlay_enabled: bool) {
    const NUM_BLOCKS: u64 = 1024;
    const BLOCK_SIZE: u32 = 512;
    let vmo = zx::Vmo::create(NUM_BLOCKS * BLOCK_SIZE as u64).unwrap();
    // Write a known pattern into "super" and "userdata" for detection later.  (The offsets match
    // the ones set below in initial_partitions)
    vmo.write(b"super", 50 * BLOCK_SIZE as u64).unwrap();
    vmo.write(b"userdata", 150 * BLOCK_SIZE as u64).unwrap();
    let block_server = Arc::new(
        VmoBackedServer::from_vmo(BLOCK_SIZE, vmo).expect("Failed to create VmoBackedServer"),
    );

    let gpt = {
        let filesystem = fs_management::filesystem::Filesystem::new(
            VmoBackedServerConnector::new(block_server),
            fs_management::Gpt {
                merge_super_and_userdata: overlay_enabled,
                ..fs_management::Gpt::dynamic_child()
            },
        );
        filesystem.serve_multi_volume().await.expect("Failed to start GPT")
    };

    let initial_partitions = vec![
        make_partition_entry(
            "other1",
            fblock::Guid { value: [1u8; 16] },
            fblock::Guid { value: [1u8; 16] },
            40,
            10,
            0,
        ),
        make_partition_entry(
            "super",
            fblock::Guid { value: [2u8; 16] },
            fblock::Guid { value: [2u8; 16] },
            50,
            100,
            0,
        ),
        make_partition_entry(
            "userdata",
            fblock::Guid { value: [3u8; 16] },
            fblock::Guid { value: [3u8; 16] },
            150,
            50,
            0,
        ),
        make_partition_entry(
            "other2",
            fblock::Guid { value: [4u8; 16] },
            fblock::Guid { value: [4u8; 16] },
            200,
            25,
            0,
        ),
        make_partition_entry(
            "other3",
            fblock::Guid { value: [5u8; 16] },
            fblock::Guid { value: [5u8; 16] },
            400,
            100,
            0,
        ),
        make_partition_entry(
            "super",
            fblock::Guid { value: [6u8; 16] },
            fblock::Guid { value: [6u8; 16] },
            500,
            1,
            0,
        ),
        make_partition_entry(
            "userdata",
            fblock::Guid { value: [7u8; 16] },
            fblock::Guid { value: [7u8; 16] },
            501,
            1,
            0,
        ),
    ];

    let partitions_admin =
        connect_to_protocol_at_dir_root::<fpartitions::PartitionsAdminProxy>(gpt.exposed_dir())
            .unwrap();
    partitions_admin
        .reset_partition_table(&initial_partitions[..])
        .await
        .expect("FIDL error")
        .expect("Failed to reset");

    let partition_service_dir = fuchsia_fs::directory::open_directory(
        gpt.exposed_dir(),
        fpartitions::PartitionServiceMarker::SERVICE_NAME,
        fio::PERM_READABLE,
    )
    .await
    .unwrap();

    let partitions = fuchsia_fs::directory::readdir(&partition_service_dir)
        .await
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect::<Vec<_>>();

    let mut found_partitions = vec![];
    for partition in partitions {
        let volume = fuchsia_component::client::connect_to_named_protocol_at_dir_root::<
            fblock::BlockMarker,
        >(&partition_service_dir, &format!("{}/volume", partition))
        .unwrap();

        let partition_info =
            volume.get_metadata().await.expect("FIDL error").expect("Failed to GetMetadata");

        let dir = fuchsia_fs::directory::open_directory(
            &partition_service_dir,
            &partition,
            fio::PERM_READABLE,
        )
        .await
        .unwrap();
        let mut entries = fuchsia_fs::directory::readdir(&dir)
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.name)
            .collect::<Vec<_>>();
        entries.sort();
        if overlay_enabled && partition_info.name.as_deref() == Some("super_and_userdata") {
            // Ensure that the original partition information can be queried
            assert_eq!(entries, vec!["overlay", "volume"]);
            let overlay = fuchsia_component::client::connect_to_named_protocol_at_dir_root::<
                fpartitions::OverlayPartitionMarker,
            >(&dir, "overlay")
            .unwrap();
            let overlay_partitions =
                overlay.get_partitions().await.expect("FIDL error").expect("get_partitions");
            assert_eq!(
                overlay_partitions,
                vec![
                    make_partition_info(
                        "super",
                        fblock::Guid { value: [2u8; 16] },
                        fblock::Guid { value: [2u8; 16] },
                        50,
                        100,
                        0
                    ),
                    make_partition_info(
                        "userdata",
                        fblock::Guid { value: [3u8; 16] },
                        fblock::Guid { value: [3u8; 16] },
                        150,
                        50,
                        0
                    ),
                ]
            );

            // Ensure the merged partitions contains the correct contents
            let block_client = RemoteBlockClient::new(volume).await.unwrap();
            assert_eq!(block_client.block_count(), 150);
            let mut buf = vec![0u8; 512];
            block_client
                .read_at(MutableBufferSlice::Memory(&mut buf[..]), 0)
                .await
                .expect("read failed");
            assert_eq!(b"super", &buf[..5]);
            buf.fill(0);
            block_client
                .read_at(MutableBufferSlice::Memory(&mut buf[..]), BLOCK_SIZE as u64 * 100)
                .await
                .expect("read failed");
            assert_eq!(b"userdata", &buf[..8]);
        } else {
            assert_eq!(entries, vec!["partition", "volume"]);
        }
        found_partitions.push(partition_info);
    }
    found_partitions.sort_by_key(|a| a.start_block_offset);

    let expected_partitions = if overlay_enabled {
        vec![
            make_partition_info(
                "other1",
                fblock::Guid { value: [1u8; 16] },
                fblock::Guid { value: [1u8; 16] },
                40,
                10,
                0,
            ),
            make_partition_info(
                "super_and_userdata",
                fblock::Guid { value: [2u8; 16] },
                fblock::Guid { value: [2u8; 16] },
                50,
                150,
                0,
            ),
            make_partition_info(
                "other2",
                fblock::Guid { value: [4u8; 16] },
                fblock::Guid { value: [4u8; 16] },
                200,
                25,
                0,
            ),
            make_partition_info(
                "other3",
                fblock::Guid { value: [5u8; 16] },
                fblock::Guid { value: [5u8; 16] },
                400,
                100,
                0,
            ),
            make_partition_info(
                "super",
                fblock::Guid { value: [6u8; 16] },
                fblock::Guid { value: [6u8; 16] },
                500,
                1,
                0,
            ),
            make_partition_info(
                "userdata",
                fblock::Guid { value: [7u8; 16] },
                fblock::Guid { value: [7u8; 16] },
                501,
                1,
                0,
            ),
        ]
    } else {
        initial_partitions.iter().map(entry_to_info).collect()
    };
    assert_eq!(found_partitions, expected_partitions);
}

#[test_case(true; "overlay_enabled")]
#[test_case(false; "overlay_disabled")]
#[fuchsia::test]
async fn test_commit_transaction_with_overlay(overlay_enabled: bool) {
    const NUM_BLOCKS: u64 = 1024;
    const BLOCK_SIZE: u32 = 512;
    let vmo = zx::Vmo::create(NUM_BLOCKS * BLOCK_SIZE as u64).unwrap();
    // Write a known pattern into "super" and "userdata" for detection later.  (The offsets match
    // the ones set below in initial_partitions)
    vmo.write(b"super", 50 * BLOCK_SIZE as u64).unwrap();
    vmo.write(b"userdata", 150 * BLOCK_SIZE as u64).unwrap();
    let block_server = Arc::new(VmoBackedServer::from_vmo(BLOCK_SIZE, vmo).unwrap());

    let gpt = {
        let filesystem = fs_management::filesystem::Filesystem::new(
            VmoBackedServerConnector::new(block_server),
            fs_management::Gpt {
                merge_super_and_userdata: overlay_enabled,
                ..fs_management::Gpt::dynamic_child()
            },
        );
        filesystem.serve_multi_volume().await.expect("Failed to start GPT")
    };

    let initial_partitions = vec![
        make_partition_entry(
            "other1",
            fblock::Guid { value: [1u8; 16] },
            fblock::Guid { value: [1u8; 16] },
            40,
            10,
            0,
        ),
        make_partition_entry(
            "super",
            fblock::Guid { value: [2u8; 16] },
            fblock::Guid { value: [2u8; 16] },
            50,
            100,
            0,
        ),
        make_partition_entry(
            "userdata",
            fblock::Guid { value: [3u8; 16] },
            fblock::Guid { value: [3u8; 16] },
            150,
            50,
            0,
        ),
    ];

    let partitions_admin =
        connect_to_protocol_at_dir_root::<fpartitions::PartitionsAdminProxy>(gpt.exposed_dir())
            .unwrap();
    partitions_admin
        .reset_partition_table(&initial_partitions[..])
        .await
        .expect("FIDL error")
        .expect("Failed to reset");

    let partition_service_dir = fuchsia_fs::directory::open_directory(
        gpt.exposed_dir(),
        fpartitions::PartitionServiceMarker::SERVICE_NAME,
        fio::PERM_READABLE,
    )
    .await
    .unwrap();

    let partitions = fuchsia_fs::directory::readdir(&partition_service_dir)
        .await
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect::<Vec<_>>();

    let mut found_partitions = vec![];
    // Find the "other1" partition and make a change to it.
    for partition in partitions {
        let volume = fuchsia_component::client::connect_to_named_protocol_at_dir_root::<
            fblock::BlockMarker,
        >(&partition_service_dir, &format!("{}/volume", partition))
        .unwrap();

        let mut partition_info =
            volume.get_metadata().await.expect("FIDL error").expect("Failed to GetMetadata");
        eprintln!("{partition}: {partition_info:?}");

        if partition_info.name.as_deref() == Some("other1") {
            let partitions_manager = connect_to_protocol_at_dir_root::<
                fpartitions::PartitionsManagerProxy,
            >(gpt.exposed_dir())
            .unwrap();
            let transaction = partitions_manager
                .create_transaction()
                .await
                .expect("FIDL error")
                .expect("Failed to create transaction");
            let part = fuchsia_component::client::connect_to_named_protocol_at_dir_root::<
                fpartitions::PartitionMarker,
            >(&partition_service_dir, &format!("{}/partition", partition))
            .unwrap();
            part.update_metadata(fpartitions::PartitionUpdateMetadataRequest {
                transaction: Some(transaction.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap()),
                flags: Some(1234),
                ..Default::default()
            })
            .await
            .expect("FIDL error")
            .expect("Failed to update_metadata");

            partitions_manager
                .commit_transaction(transaction)
                .await
                .expect("FIDL error")
                .expect("Failed to commit");
            partition_info =
                volume.get_metadata().await.expect("FIDL error").expect("Failed to GetMetadata");
        }

        found_partitions.push(partition_info);
    }
    found_partitions.sort_by_key(|a| a.start_block_offset);

    let expected_partitions = if overlay_enabled {
        vec![
            make_partition_info(
                "other1",
                fblock::Guid { value: [1u8; 16] },
                fblock::Guid { value: [1u8; 16] },
                40,
                10,
                1234,
            ),
            make_partition_info(
                "super_and_userdata",
                fblock::Guid { value: [2u8; 16] },
                fblock::Guid { value: [2u8; 16] },
                50,
                150,
                0,
            ),
        ]
    } else {
        vec![
            make_partition_info(
                "other1",
                fblock::Guid { value: [1u8; 16] },
                fblock::Guid { value: [1u8; 16] },
                40,
                10,
                1234,
            ),
            make_partition_info(
                "super",
                fblock::Guid { value: [2u8; 16] },
                fblock::Guid { value: [2u8; 16] },
                50,
                100,
                0,
            ),
            make_partition_info(
                "userdata",
                fblock::Guid { value: [3u8; 16] },
                fblock::Guid { value: [3u8; 16] },
                150,
                50,
                0,
            ),
        ]
    };
    assert_eq!(found_partitions, expected_partitions);
}
