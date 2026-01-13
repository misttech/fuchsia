// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::sync::Arc;

use anyhow::{Context, Error, Result};
use block_client::RemoteBlockClient;
use fidl::endpoints::ServiceMarker as _;
use fshost_assembly_config::{BlockDeviceConfig, BlockDeviceIdentifiers, BlockDeviceParent};
use fuchsia_component_test::{Capability, RealmBuilder, Ref, Route};
use fuchsia_driver_test::{DriverTestRealmBuilder, DriverTestRealmInstance};
use futures::FutureExt as _;
use ramdevice_client::RamdiskClientBuilder;
use vmo_backed_block_server::{InitialContents, VmoBackedServerOptions};
use zx::HandleBased as _;
use {
    fidl_fuchsia_component_test as ftest, fidl_fuchsia_driver_test as fdt,
    fidl_fuchsia_hardware_block_volume as fvolume, fidl_fuchsia_hardware_ramdisk as framdisk,
    fidl_fuchsia_io as fio, fidl_fuchsia_storage_block as fblock,
    fidl_fuchsia_testing_simple as fsimple, fuchsia_async as fasync,
};

const BLOCK_SIZE: u64 = 512;
const NUM_BLOCKS: u64 = 128;
const RAMDISK_SIZE: u64 = BLOCK_SIZE * NUM_BLOCKS;

async fn format_gpt_vmo(partitions: Vec<gpt::PartitionInfo>) -> Result<zx::Vmo, Error> {
    let vmo = zx::Vmo::create(RAMDISK_SIZE)?;
    let vmo_clone = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS)?;
    let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fblock::BlockMarker>();
    let server = fasync::Task::spawn(async move {
        let block_server = VmoBackedServerOptions {
            initial_contents: InitialContents::FromVmo(vmo_clone),
            block_size: BLOCK_SIZE as u32,
            ..Default::default()
        }
        .build()
        .unwrap();
        block_server.serve(stream).await.unwrap();
    });

    {
        let client = Arc::new(RemoteBlockClient::new(proxy).await?);
        gpt::Gpt::format(client, partitions).await.context("Failed to format GPT")?;
    }

    server.await;
    Ok(vmo)
}

#[fuchsia::test]
async fn integration() {
    const PART_1_TYPE_GUID: [u8; 16] = [0xaa; 16];
    const PART_1_INSTANCE_GUID: [u8; 16] = [0xbb; 16];
    const PART_1_NAME: &str = "my-part";

    const PART_2_TYPE_GUID: [u8; 16] = [0xcc; 16];
    const PART_2_INSTANCE_GUID: [u8; 16] = [0xdd; 16];
    const PART_2_NAME: &str = "other-part";
    // This test creates a driver test realm with a constellation of components necessary to
    // exercise block-relay.
    //
    // The setup is as follows:
    // - A ramdisk driver hosts a GPT-formatted block device.
    // - Fshost runs, and launches block-relay as a child component.  It is configured to forward
    //   the partition labeled "my-part" to block-relay.
    // - A driver toy_driver.cm binds on fuchsia.block.gpt.PARTITION_NAME == "my-part", and does
    //   basic verification of interacting with the block device.
    //
    // block-relay is responsible for adding DF nodes for entries it finds in /block.  If it is
    // working properly, toy_driver.cm will eventually be bound.
    let builder = RealmBuilder::new().await.expect("failed to create RealmBuilder");
    builder.driver_test_realm_setup().await.expect("failed to setup driver test realm");

    let dtr_exposes = vec![
        ftest::Capability::Service(ftest::Service {
            name: Some(framdisk::ServiceMarker::SERVICE_NAME.to_string()),
            ..Default::default()
        }),
        ftest::Capability::Service(ftest::Service {
            name: Some(fvolume::ServiceMarker::SERVICE_NAME.to_string()),
            ..Default::default()
        }),
        ftest::Capability::Service(ftest::Service {
            name: Some(fsimple::ServiceMarker::SERVICE_NAME.to_string()),
            ..Default::default()
        }),
    ];
    builder
        .driver_test_realm_add_dtr_exposes(&dtr_exposes)
        .await
        .expect("failed to add dtr exposes");

    let mut fshost = fshost_testing::FshostBuilder::new("test-fshost");
    fshost.set_device_config(vec![
        BlockDeviceConfig {
            device: String::from("name-is-irrelevant"),
            from: BlockDeviceIdentifiers {
                label: PART_1_NAME.to_string(),
                parent: BlockDeviceParent::Gpt,
            },
        },
        BlockDeviceConfig {
            device: String::from("other-part"),
            from: BlockDeviceIdentifiers {
                label: PART_2_NAME.to_string(),
                parent: BlockDeviceParent::Gpt,
            },
        },
    ]);
    let fshost = fshost.build(&builder).await;

    builder
        .add_route(
            Route::new()
                .capability(Capability::service::<fvolume::ServiceMarker>())
                .capability(Capability::directory("dev-topological").rights(fio::R_STAR_DIR))
                .from(Ref::child(fuchsia_driver_test::COMPONENT_NAME))
                .to(&fshost),
        )
        .await
        .expect("failed to add route");
    builder
        .add_route(
            Route::new()
                .capability(Capability::dictionary("diagnostics"))
                .from(Ref::parent())
                .to(&fshost),
        )
        .await
        .expect("failed to add route");

    let realm = builder.build().await.expect("failed to build realm");
    realm
        .driver_test_realm_start(fdt::RealmArgs {
            root_driver: Some("fuchsia-boot:///platform-bus#meta/platform-bus.cm".to_owned()),
            dtr_exposes: Some(dtr_exposes),
            software_devices: Some(vec![fdt::SoftwareDevice {
                device_name: "ram-disk".to_string(),
                device_id: bind_fuchsia_platform::BIND_PLATFORM_DEV_DID_RAM_DISK,
            }]),
            ..Default::default()
        })
        .await
        .expect("failed to start driver test realm");

    // Create a ramdisk with a formatted GPT.  Fshost will automatically bind to it.
    // Note that we format locally in a VmoBackedServer, then later bind to a ramdisk.  This is
    // necessary to avoid fshost racily binding to an uninitialized device.

    let ramdisk_vmo = format_gpt_vmo(vec![
        gpt::PartitionInfo {
            label: PART_1_NAME.to_string(),
            type_guid: gpt::Guid::from_bytes(PART_1_TYPE_GUID),
            instance_guid: gpt::Guid::from_bytes(PART_1_INSTANCE_GUID),
            start_block: 4,
            num_blocks: 6,
            flags: 0,
        },
        gpt::PartitionInfo {
            label: PART_2_NAME.to_string(),
            type_guid: gpt::Guid::from_bytes(PART_2_TYPE_GUID),
            instance_guid: gpt::Guid::from_bytes(PART_2_INSTANCE_GUID),
            start_block: 10,
            num_blocks: 10,
            flags: 0,
        },
    ])
    .await
    .expect("Failed to format GPT");

    let ramdisk = RamdiskClientBuilder::new_with_vmo(ramdisk_vmo, Some(BLOCK_SIZE))
        .use_v2()
        .publish()
        .ramdisk_service(
            fuchsia_fs::directory::open_directory(
                realm.root.get_exposed_dir(),
                framdisk::ServiceMarker::SERVICE_NAME,
                fio::PERM_READABLE,
            )
            .await
            .expect("failed to open directory"),
        )
        .build()
        .await
        .expect("failed to create ramdisk");

    // Wait for the toy driver to bind.
    let exposed_dir = realm.root.get_exposed_dir();
    let simple_service =
        fuchsia_component::client::Service::open_from_dir(exposed_dir, fsimple::ServiceMarker)
            .expect("failed to open simple service");
    const DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);
    let instance: fsimple::ServiceProxy = futures::select! {
        instance = simple_service.watch_for_any().fuse() => instance.expect("failed to connect"),
        _ = fasync::Timer::new(DEADLINE).fuse() => panic!("Driver never bound"),
    };
    let proxy = instance.connect_to_simple().expect("Failed to connect");
    proxy.on_start().await.expect("FIDL error").expect("OnStart failed");

    ramdisk.destroy_and_wait_for_removal().await.expect("failed to destroy ramdisk");
}
