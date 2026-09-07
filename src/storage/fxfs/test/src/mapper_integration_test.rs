// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Error};
use blob_pager_and_verifier::{DeliveryQueueProcessor, TestVmoProvider};
use blob_writer::BlobWriter;
use delivery_blob::{CompressionMode, Type1Blob, Type3Blob};
use fidl_fuchsia_fs_startup::{CreateOptions, MountOptions};
use fidl_fuchsia_fxfs::BlobCreatorMarker;
use fidl_fuchsia_storage_block as fblock;
use fidl_fuchsia_storage_mapping::MappingProviderMarker;
use fidl_fuchsia_storage_partitions as fpartitions;
use fs_management::filesystem::Filesystem as FsManagementFilesystem;
use fuchsia_component::client::{Service, connect_to_protocol, connect_to_protocol_at_dir_svc};
use fuchsia_merkle::Hash;
use std::sync::Arc;

struct TestBlob {
    data: Vec<u8>,
    delivery_data: Vec<u8>,
    hash: Hash,
}

impl TestBlob {
    fn new_uncompressed(data: Vec<u8>) -> Self {
        let hash = fuchsia_merkle::root_from_slice(&data);
        let delivery_data = Type1Blob::generate(&data, CompressionMode::Never);
        Self { data, delivery_data, hash }
    }

    fn new_type1_compressed(data: Vec<u8>) -> Self {
        let hash = fuchsia_merkle::root_from_slice(&data);
        let delivery_data = Type1Blob::generate(&data, CompressionMode::Always);
        Self { data, delivery_data, hash }
    }

    fn new_type3_compressed(data: Vec<u8>) -> Self {
        let hash = fuchsia_merkle::root_from_slice(&data);
        let delivery_data = Type3Blob::generate(&data, CompressionMode::Always);
        Self { data, delivery_data, hash }
    }
}

#[fuchsia::test(threads = 4)]
async fn test_mapper_on_gpt_partition() -> Result<(), Error> {
    log::info!("Connecting to partition service...");
    let (partition_service, partitions_manager, _gpt) = match (
        Service::open(fpartitions::PartitionServiceMarker),
        connect_to_protocol::<fpartitions::PartitionsManagerMarker>(),
    ) {
        (Ok(service), Ok(manager))
            if service.clone().enumerate().await.is_ok_and(|list| !list.is_empty()) =>
        {
            log::info!("Using system GPT partition service on block device.");
            (service, manager, None)
        }
        _ => {
            log::warn!(
                "================================================================================"
            );
            log::warn!(
                "WARNING: No system GPT partition service found! Falling back to VmoBackedServer!"
            );
            log::warn!(
                "================================================================================"
            );
            let block_size = 4096u64;
            let block_count = 256 * 1024 * 1024 / block_size;
            let vmo = zx::Vmo::create(block_count * block_size)?;
            let block_server = Arc::new(
                vmo_backed_block_server::VmoBackedServer::from_vmo(block_size as u32, vmo)
                    .context("Failed to create VmoBackedServer")?,
            );
            let gpt_fs = FsManagementFilesystem::new(
                vmo_backed_block_server::VmoBackedServerConnector::new(block_server),
                fs_management::Gpt { ..fs_management::Gpt::dynamic_child() },
            );
            let serving = gpt_fs.serve_multi_volume().await.context("Failed to start GPT")?;
            let partitions_admin = fuchsia_component::client::connect_to_protocol_at_dir_root::<
                fpartitions::PartitionsAdminMarker,
            >(serving.exposed_dir())
            .context("Failed to connect to PartitionsAdmin")?;
            let num_blocks = block_matcher::DEFAULT_BENCHMARK_FVM_SIZE_BYTES / block_size;
            let initial_partitions = vec![fpartitions::PartitionEntry {
                name: fs_management::format::constants::BENCHMARK_FVM_VOLUME_NAME.to_string(),
                type_guid: fblock::Guid {
                    value: fs_management::format::constants::BENCHMARK_FVM_TYPE_GUID,
                },
                instance_guid: fblock::Guid { value: [1u8; 16] },
                start_block: 4,
                num_blocks,
                flags: 0,
            }];
            partitions_admin
                .reset_partition_table(&initial_partitions)
                .await
                .context("FIDL error on reset_partition_table")?
                .map_err(zx::Status::err_from_raw)
                .context("Failed to reset partition table")?;
            let partitions =
                Service::open_from_dir(serving.exposed_dir(), fpartitions::PartitionServiceMarker)
                    .context("Failed to open PartitionService from GPT")?;
            let manager = fuchsia_component::client::connect_to_protocol_at_dir_root::<
                fpartitions::PartitionsManagerMarker,
            >(serving.exposed_dir())
            .context("Failed to connect to PartitionsManager")?;
            (partitions, manager, Some(serving))
        }
    };

    let partition = Arc::new(
        block_matcher::find_or_create_test_partition(
            partition_service,
            partitions_manager,
            block_matcher::DEFAULT_BENCHMARK_FVM_SIZE_BYTES,
        )
        .await
        .context("Failed to find or create test partition")?,
    );

    log::info!("Formatting Fxfs on test partition...");
    let mut fs = FsManagementFilesystem::new(
        partition.clone(),
        fs_management::Fxfs { allow_type3_blobs: true, ..Default::default() },
    );
    fs.format().await.context("Failed to format Fxfs")?;

    log::info!("Serving Fxfs multi-volume...");
    let serving = fs.serve_multi_volume().await.context("Failed to serve Fxfs")?;
    let volume = serving
        .create_volume(
            "blob",
            CreateOptions::default(),
            MountOptions { as_blob: Some(true), ..Default::default() },
        )
        .await
        .context("Failed to create blob volume")?;

    let blob_creator = connect_to_protocol_at_dir_svc::<BlobCreatorMarker>(volume.exposed_dir())
        .context("Failed to connect to BlobCreator")?;
    let mapping_provider =
        connect_to_protocol_at_dir_svc::<MappingProviderMarker>(volume.exposed_dir())
            .context("Failed to connect to MappingProvider")?;

    let mapper_proxy = partition.connect_to_mapper()?;

    log::info!("Setting up Pager, Port, and Delivery Queue...");
    let pager = Arc::new(zx::Pager::create(zx::PagerOptions::empty()).unwrap());
    let port = zx::Port::create();
    let delivery_queue = zx::Vmo::create(mapping::DELIVERY_VMO_SIZE).unwrap();

    let vmo_provider = Arc::new(TestVmoProvider::new(
        pager.clone(),
        delivery_queue.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
    ));
    let delivery_receiver = vmo_fifo::Receiver::<mapping::RawDeliveryCommand>::new(
        delivery_queue.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
        mapping::PENDING_DELIVERY_COMMANDS_CAPACITY,
    )
    .context("Failed to create delivery queue receiver")?;
    let _delivery_processor = DeliveryQueueProcessor::spawn(
        delivery_receiver,
        vmo_provider.clone(),
        delivery_queue.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
    )
    .context("Failed to spawn DeliveryQueueProcessor")?;

    log::info!("Opening mapping sessions...");
    let (mapping_session_proxy, mapping_session_server) = fidl::endpoints::create_proxy();
    let mapping_vmo = mapping_provider
        .open_session(mapping_session_server)
        .await
        .context("FIDL error on open_session")?
        .map_err(zx::Status::err_from_raw)
        .context("Failed to open mapping session")?;

    let (mapper_session_proxy, mapper_session_server) = fidl::endpoints::create_proxy();
    mapper_proxy
        .open_session(
            mapper_session_server,
            mapping_vmo,
            Some(port.duplicate_handle(zx::Rights::SAME_RIGHTS)?),
            Some(delivery_queue),
        )
        .await
        .context("FIDL error on Mapper.OpenSession")?
        .map_err(zx::Status::err_from_raw)
        .context("Failed to open mapper session on partition")?;

    let test_blobs = vec![
        TestBlob::new_uncompressed(b"Short uncompressed blob payload".to_vec()),
        TestBlob::new_uncompressed(vec![0x42u8; 3500]),
        TestBlob::new_uncompressed(vec![0x77u8; 32 * 1024]),
        TestBlob::new_type1_compressed(vec![0xAAu8; 64 * 1024]),
        TestBlob::new_type3_compressed(vec![0x55u8; 128 * 1024]),
    ];

    for (idx, blob) in test_blobs.iter().enumerate() {
        log::info!("Writing test blob {idx} (size: {} bytes)...", blob.data.len());
        let writer_client_end = blob_creator
            .create(&blob.hash.into(), false)
            .await
            .context("FIDL error on BlobCreator.Create")?
            .map_err(|e| anyhow::anyhow!("CreateBlob error: {e:?}"))
            .context("Failed to create blob")?;
        let writer_proxy = writer_client_end.into_proxy();
        let mut blob_writer = BlobWriter::create(writer_proxy, blob.delivery_data.len() as u64)
            .await
            .context("Failed to create BlobWriter")?;
        blob_writer.write(&blob.delivery_data).await.context("Failed to write blob data")?;

        log::info!("Opening blob {idx} in mapping session...");
        let (uncompressed_size, key) = mapping_session_proxy
            .open(blob.hash.as_bytes())
            .await
            .context("FIDL error on MappingSession.Open")?
            .map_err(zx::Status::err_from_raw)
            .context("Failed to open blob in mapping session")?;
        let key = key as u64;

        assert_eq!(uncompressed_size, blob.data.len() as u64);

        log::info!("Creating paged VMO for blob {idx} (key {key})...");
        let paged_vmo = pager
            .create_vmo(zx::VmoOptions::empty(), &port, key, uncompressed_size)
            .context("Failed to create paged VMO")?;

        vmo_provider.register_vmo(key, paged_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS)?);

        log::info!("Reading blob {idx} data back from paged VMO...");
        let expected_len = blob.data.len();
        let paged_vmo_reader = paged_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS)?;
        let reader_thread = std::thread::spawn(move || {
            let mut read_buf = vec![0u8; expected_len];
            paged_vmo_reader.read(&mut read_buf, 0).expect("paged vmo read failed");
            read_buf
        });

        let read_data = reader_thread.join().unwrap();
        assert_eq!(&read_data, &blob.data, "Blob {idx} data readback mismatch!");

        vmo_provider.unregister_vmo(key);
        mapping_session_proxy
            .close(key as u32)
            .await
            .context("FIDL error on MappingSession.Close")?
            .map_err(zx::Status::err_from_raw)
            .context("Failed to close blob in mapping session")?;
        log::info!("Blob {idx} verified and closed successfully.");
    }

    log::info!("Closing mapper session...");
    mapper_session_proxy
        .close()
        .await
        .context("FIDL error on MapperSession.Close")?
        .map_err(zx::Status::err_from_raw)
        .context("Failed to close mapper session")?;
    serving.shutdown().await.context("Failed to shutdown Fxfs")?;

    log::info!("All blobs verified successfully over mapper on GPT partition!");
    Ok(())
}
