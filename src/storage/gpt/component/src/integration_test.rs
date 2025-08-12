// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::sync::Arc;

use block_client::{BlockClient as _, MutableBufferSlice, RemoteBlockClient};
use fidl::endpoints::ServiceMarker as _;
use fuchsia_component::client::connect::connect_to_protocol_at_dir_root;
use test_case::test_case;
use vmo_backed_block_server::{
    InitialContents, VmoBackedServerOptions, VmoBackedServerTestingExt as _,
};
use {
    fidl_fuchsia_hardware_block_partition as fpartition,
    fidl_fuchsia_hardware_block_volume as fvolume, fidl_fuchsia_io as fio,
    fidl_fuchsia_storage_partitions as fpartitions,
};

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
        VmoBackedServerOptions {
            initial_contents: InitialContents::FromVmo(vmo),
            block_size: BLOCK_SIZE,
            ..Default::default()
        }
        .build()
        .unwrap(),
    );

    let gpt = {
        let mut filesystem = fs_management::filesystem::Filesystem::from_boxed_config(
            Box::new(move |server_end| Ok(block_server.connect_server(server_end))),
            Box::new(fs_management::Gpt {
                merge_super_and_userdata: overlay_enabled,
                ..fs_management::Gpt::dynamic_child()
            }),
        );
        filesystem.serve_multi_volume().await.expect("Failed to start GPT")
    };

    let initial_partitions = vec![
        fpartitions::PartitionInfo {
            name: "other1".to_string(),
            type_guid: fpartition::Guid { value: [1u8; 16] },
            instance_guid: fpartition::Guid { value: [1u8; 16] },
            start_block: 40,
            num_blocks: 10,
            flags: 0,
        },
        fpartitions::PartitionInfo {
            name: "super".to_string(),
            type_guid: fpartition::Guid { value: [2u8; 16] },
            instance_guid: fpartition::Guid { value: [2u8; 16] },
            start_block: 50,
            num_blocks: 100,
            flags: 0,
        },
        fpartitions::PartitionInfo {
            name: "userdata".to_string(),
            type_guid: fpartition::Guid { value: [3u8; 16] },
            instance_guid: fpartition::Guid { value: [3u8; 16] },
            start_block: 150,
            num_blocks: 50,
            flags: 0,
        },
        fpartitions::PartitionInfo {
            name: "other2".to_string(),
            type_guid: fpartition::Guid { value: [4u8; 16] },
            instance_guid: fpartition::Guid { value: [4u8; 16] },
            start_block: 200,
            num_blocks: 25,
            flags: 0,
        },
        fpartitions::PartitionInfo {
            name: "other3".to_string(),
            type_guid: fpartition::Guid { value: [5u8; 16] },
            instance_guid: fpartition::Guid { value: [5u8; 16] },
            start_block: 400,
            num_blocks: 100,
            flags: 0,
        },
        // Make sure we can handle multiple super/userdata partitions (the later ones will just be
        // treated as normal partitions).  Not a likely scenario, but it's worth being explicit
        // about the behaviour.
        fpartitions::PartitionInfo {
            name: "super".to_string(),
            type_guid: fpartition::Guid { value: [6u8; 16] },
            instance_guid: fpartition::Guid { value: [6u8; 16] },
            start_block: 500,
            num_blocks: 1,
            flags: 0,
        },
        fpartitions::PartitionInfo {
            name: "userdata".to_string(),
            type_guid: fpartition::Guid { value: [7u8; 16] },
            instance_guid: fpartition::Guid { value: [7u8; 16] },
            start_block: 501,
            num_blocks: 1,
            flags: 0,
        },
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
            fvolume::VolumeMarker,
        >(&partition_service_dir, &format!("{}/volume", partition))
        .unwrap();

        let metadata =
            volume.get_metadata().await.expect("FIDL error").expect("Failed to GetMetadata");
        let partition_info = fpartitions::PartitionInfo {
            name: metadata.name.unwrap(),
            type_guid: metadata.type_guid.unwrap(),
            instance_guid: metadata.instance_guid.unwrap(),
            start_block: metadata.start_block_offset.unwrap(),
            num_blocks: metadata.num_blocks.unwrap(),
            flags: metadata.flags.unwrap(),
        };

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
        if overlay_enabled && partition_info.name == "super_and_userdata" {
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
                    fpartitions::PartitionInfo {
                        name: "super".to_string(),
                        type_guid: fpartition::Guid { value: [2u8; 16] },
                        instance_guid: fpartition::Guid { value: [2u8; 16] },
                        start_block: 50,
                        num_blocks: 100,
                        flags: 0,
                    },
                    fpartitions::PartitionInfo {
                        name: "userdata".to_string(),
                        type_guid: fpartition::Guid { value: [3u8; 16] },
                        instance_guid: fpartition::Guid { value: [3u8; 16] },
                        start_block: 150,
                        num_blocks: 50,
                        flags: 0,
                    },
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
    found_partitions.sort_by_key(|a| a.start_block);

    let expected_partitions = if overlay_enabled {
        vec![
            fpartitions::PartitionInfo {
                name: "other1".to_string(),
                type_guid: fpartition::Guid { value: [1u8; 16] },
                instance_guid: fpartition::Guid { value: [1u8; 16] },
                start_block: 40,
                num_blocks: 10,
                flags: 0,
            },
            fpartitions::PartitionInfo {
                name: "super_and_userdata".to_string(),
                type_guid: fpartition::Guid { value: [2u8; 16] },
                instance_guid: fpartition::Guid { value: [2u8; 16] },
                start_block: 50,
                num_blocks: 150,
                flags: 0,
            },
            fpartitions::PartitionInfo {
                name: "other2".to_string(),
                type_guid: fpartition::Guid { value: [4u8; 16] },
                instance_guid: fpartition::Guid { value: [4u8; 16] },
                start_block: 200,
                num_blocks: 25,
                flags: 0,
            },
            fpartitions::PartitionInfo {
                name: "other3".to_string(),
                type_guid: fpartition::Guid { value: [5u8; 16] },
                instance_guid: fpartition::Guid { value: [5u8; 16] },
                start_block: 400,
                num_blocks: 100,
                flags: 0,
            },
            fpartitions::PartitionInfo {
                name: "super".to_string(),
                type_guid: fpartition::Guid { value: [6u8; 16] },
                instance_guid: fpartition::Guid { value: [6u8; 16] },
                start_block: 500,
                num_blocks: 1,
                flags: 0,
            },
            fpartitions::PartitionInfo {
                name: "userdata".to_string(),
                type_guid: fpartition::Guid { value: [7u8; 16] },
                instance_guid: fpartition::Guid { value: [7u8; 16] },
                start_block: 501,
                num_blocks: 1,
                flags: 0,
            },
        ]
    } else {
        initial_partitions.clone()
    };
    assert_eq!(found_partitions, expected_partitions);
}
