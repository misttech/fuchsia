// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Error, anyhow};
use fidl::endpoints::{
    ClientEnd, DiscoverableProtocolMarker as _, RequestStream as _, ServiceMarker,
    create_endpoints, create_request_stream,
};
use fidl_fuchsia_hardware_block_volume::NodeProxy;
use fidl_fuchsia_process_lifecycle::{LifecycleRequest, LifecycleRequestStream};
use fuchsia_component::client::{connect_to_protocol, connect_to_protocol_at_path};
use fuchsia_fs::directory::{WatchEvent, WatchMessage, readdir};
use futures::{FutureExt as _, StreamExt as _, TryStreamExt as _};
use zerocopy::FromBytes as _;
use zerocopy::byteorder::{LE, U16, U32};
use {
    fidl_fuchsia_component_decl as fcd, fidl_fuchsia_component_sandbox as fsandbox,
    fidl_fuchsia_driver_framework as fdf, fidl_fuchsia_hardware_block_volume as fvolume,
    fidl_fuchsia_io as fio, fidl_fuchsia_storage_block as fblock, fuchsia_async as fasync,
};

#[repr(C)]
#[derive(
    Clone, Copy, zerocopy::FromBytes, zerocopy::Immutable, zerocopy::KnownLayout, PartialEq, Eq,
)]
struct TypeGuid {
    data1: U32<LE>,
    data2: U16<LE>,
    data3: U16<LE>,
    data4: [u8; 8],
}

const EMPTY_GUID: TypeGuid =
    TypeGuid { data1: U32::ZERO, data2: U16::ZERO, data3: U16::ZERO, data4: [0; 8] };

impl std::fmt::Display for TypeGuid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
            self.data1,
            self.data2,
            self.data3,
            self.data4[0],
            self.data4[1],
            self.data4[2],
            self.data4[3],
            self.data4[4],
            self.data4[5],
            self.data4[6],
            self.data4[7]
        ))
    }
}

fn handle_receive_request(
    request: fsandbox::DirReceiverReceiveRequest,
    service_directory: &fio::DirectoryProxy,
) -> Result<(), Error> {
    let channel = request.channel.ok_or_else(|| anyhow!("No channel"))?;
    let subdir = request.subdir.ok_or_else(|| anyhow!("No subdir"))?;
    let flags = request.flags.ok_or_else(|| anyhow!("No flags"))?;
    Ok(service_directory.open(&subdir, flags, &fio::Options::default(), channel)?)
}

async fn run_receiver(
    mut stream: fsandbox::DirReceiverRequestStream,
    block_instance_dir: fio::DirectoryProxy,
) {
    // We need to construct a directory which looks like fuchsia.hardware.block.volume.Service with
    // a single instance (backed by block_instance_dir).
    let pseudo_dir = vfs::directory::serve_read_only(vfs::pseudo_directory! {
        "default" => vfs::pseudo_directory! {
            "volume" => vfs::service::endpoint(move |_, channel| {
                if let Err(err) = block_instance_dir.open(
                    fblock::BlockMarker::PROTOCOL_NAME,
                    fio::Flags::PROTOCOL_SERVICE,
                    &fio::Options::default(),
                    channel.into(),
                ) {
                    log::warn!(err:?; "Failed to forward Receive request");
                    // Nothing else to do; channel has already been consumed
                }
            }),
        },
    });
    while let Ok(Some(request)) = stream.try_next().await {
        match request {
            fsandbox::DirReceiverRequest::Receive { payload, control_handle: _ } => {
                if let Err(err) = handle_receive_request(payload, &pseudo_dir) {
                    log::warn!(err:?; "Failed to forward Receive request");
                    // Nothing else to do; payload has already been consumed
                }
            }
            _ => {}
        }
    }
}

async fn add_child_node(
    path: &str,
    id: &mut u64,
    node: &fvolume::NodeProxy,
    capability_store: &fsandbox::CapabilityStoreProxy,
) -> Result<Option<(ClientEnd<fdf::NodeControllerMarker>, fasync::Task<()>)>, Error> {
    let volume = connect_to_protocol_at_path::<fblock::BlockMarker>(&format!(
        "{}/{}",
        path,
        fblock::BlockMarker::PROTOCOL_NAME
    ))?;

    let (status, name_str) = volume.get_name().await?;
    zx::ok(status)?;
    let label = match name_str {
        Some(label) if !label.is_empty() => label,
        _ => {
            return Ok(None);
        }
    };
    let (status, guid_res) = volume.get_type_guid().await.unwrap_or((0, None));
    zx::ok(status)?;
    let Some(type_guid_bytes) = guid_res.map(|v| v.value) else {
        return Ok(None);
    };
    let type_guid = TypeGuid::ref_from_bytes(&type_guid_bytes[..])
        .map_err(|_| anyhow!("Invalid guid length {})", type_guid_bytes.len()))?;
    // If the partition has no type GUID, it is likely not inside the actual system partition table,
    // so don't forward it.  This is intended to deal with devices like vim3 which use the sdmmc
    // partition table and the GPT is one of several sdmmc partitions, but it is reported as having
    // an empty type GUID.
    // NOTE: This is a bit of a hack.  The right way will likely involve a per-board configuration
    // which tells block-relay which block device the system partition table is expected to reside
    // in.  See a similar comment in fshost's matcher.rs.  For now, this works.
    if *type_guid == EMPTY_GUID {
        return Ok(None);
    }
    log::info!("Forwarding partition {label} ({type_guid})");

    let dict_id = *id;
    *id += 1;
    capability_store
        .dictionary_create(dict_id)
        .await?
        .map_err(|e| anyhow!("Failed to create dict: {e:?}"))?;

    let connector_id = *id;
    *id += 1;
    let (receiver_client, receiver_stream) = create_request_stream::<fsandbox::DirReceiverMarker>();
    capability_store
        .dir_connector_create(connector_id, receiver_client)
        .await?
        .map_err(|e| anyhow!("Failed to create connector: {e:?}"))?;
    let instance_dir = fuchsia_fs::directory::open_in_namespace(path, fio::PERM_READABLE)?;
    let receiver_task = fasync::Task::spawn(run_receiver(receiver_stream, instance_dir));
    capability_store
        .dictionary_insert(
            dict_id,
            &fsandbox::DictionaryItem {
                key: fvolume::ServiceMarker::SERVICE_NAME.to_string(),
                value: connector_id,
            },
        )
        .await?
        .map_err(|e| anyhow!("Instance dict insert failed: {:?}", e))?;

    let capability = capability_store
        .export(dict_id)
        .await?
        .map_err(|e| anyhow!("Instance dict insert failed: {:?}", e))?;
    let dictionary_ref = match capability {
        fsandbox::Capability::Dictionary(d) => d,
        _ => anyhow::bail!("Exported capability was not a dictionary"),
    };

    let service_name = fvolume::ServiceMarker::SERVICE_NAME;
    let args = fdf::NodeAddArgs {
        name: Some(label.clone()),
        offers_dictionary: Some(dictionary_ref),
        offers2: Some(vec![fdf::Offer::DictionaryOffer(fcd::Offer::Service(fcd::OfferService {
            source_name: Some(service_name.to_owned()),
            target_name: Some(service_name.to_owned()),
            source_instance_filter: Some(vec!["default".to_owned()]),
            renamed_instances: Some(vec![fcd::NameMapping {
                source_name: "default".to_owned(),
                target_name: "default".to_owned(),
            }]),
            ..Default::default()
        }))]),
        properties2: Some(vec![
            fdf::NodeProperty2 {
                key: bind_fuchsia_block_gpt::PARTITION_NAME.to_owned(),
                value: fdf::NodePropertyValue::StringValue(label),
            },
            fdf::NodeProperty2 {
                key: bind_fuchsia_block_gpt::PARTITION_TYPE_GUID.to_owned(),
                value: fdf::NodePropertyValue::StringValue(format!("{type_guid}")),
            },
        ]),
        ..Default::default()
    };

    let (controller, controller_server) = create_endpoints::<fdf::NodeControllerMarker>();
    let _ = node
        .add_child(args, controller_server)
        .await?
        .map_err(|e| anyhow!("AddChild failed: {:?}", e))?;
    Ok(Some((controller, receiver_task)))
}

async fn main_loop(
    mut watcher: fuchsia_fs::directory::Watcher,
    node: &NodeProxy,
) -> Result<(), Error> {
    let capability_store = connect_to_protocol::<fsandbox::CapabilityStoreMarker>()?;
    let mut id: u64 = 1;

    // We need to retain all of the NodeController handles, since the node will tear down when
    // its controller handle is dropped.
    #[allow(clippy::collection_is_never_read)]
    let mut controllers = Vec::new();
    while let Some(next) = watcher.next().await {
        let path = match next? {
            WatchMessage { event: WatchEvent::ADD_FILE | WatchEvent::EXISTING, filename }
                if filename.as_os_str() != "." =>
            {
                format!("/block/{}", filename.to_str().unwrap())
            }
            _ => continue,
        };
        match add_child_node(&path, &mut id, &node, &capability_store).await {
            Ok(Some(controller_and_task)) => {
                controllers.push(controller_and_task);
                id += 1;
            }
            Ok(None) => {}
            Err(err) => {
                log::warn!(err:?; "Failed to add child node");
            }
        }
    }
    Ok(())
}

async fn wait_for_shutdown(lifecycle_channel: zx::Channel) {
    let mut stream =
        LifecycleRequestStream::from_channel(fasync::Channel::from_channel(lifecycle_channel));
    match stream.try_next().await {
        Ok(Some(LifecycleRequest::Stop { .. })) => {
            log::info!("Received Stop request");
        }
        Ok(None) => {}
        Err(err) => {
            log::warn!(err:?; "Error listening to lifecycle channel");
        }
    }
}

#[fuchsia::main]
async fn main() -> Result<(), Error> {
    log::info!("block-relay started");

    let lifecycle =
        fuchsia_runtime::take_startup_handle(fuchsia_runtime::HandleType::Lifecycle.into())
            .ok_or_else(|| anyhow!("No lifecycle channel"))?;

    // Open /block and create a watcher.  This will hanging-get until fshost has found and
    // enumerated the system GPT.
    let block_dir = fuchsia_fs::directory::open_in_namespace(
        "/block",
        fio::PERM_READABLE | fio::Flags::PROTOCOL_DIRECTORY,
    )
    .context("failed to open /block")?;
    let watcher = fuchsia_fs::directory::Watcher::new(&block_dir)
        .await
        .with_context(|| format!("Failed to watch dir"))?;
    log::info!("Found /block");

    let service_dir = fuchsia_component::client::open_service::<fvolume::ServiceMarker>()
        .context("failed to open service")?;
    let instances = readdir(&service_dir).await.context("failed to read service instances")?;
    if instances.len() != 1 {
        return Err(anyhow!("Expected exactly one service instance, found {}", instances.len()));
    }
    let instance_name = &instances[0].name;
    let service = fuchsia_component::client::connect_to_service_instance::<fvolume::ServiceMarker>(
        instance_name,
    )
    .context("failed to connect to service instance")?;

    let node = service.connect_to_node().context("failed to connect to node protocol")?;

    futures::select! {
        () = wait_for_shutdown(lifecycle.into()).fuse() => {},
        res = main_loop(watcher, &node).fuse() => {
            if let Err(err) = res {
                log::error!(err:?; "Error in main loop");
                return Err(err);
            }
        }
    }
    log::info!("block-relay shutting down");
    Ok(())
}
