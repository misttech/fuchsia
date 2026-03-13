// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::BTreeMap;
use std::sync::{Arc, Weak};

use crate::command_queue::{CommandQueue, CommandQueueHost, TaskStatusReceiver};
use crate::partition::EmmcPartition;
use block_server::callback_interface::SessionManager;
use block_server::{BlockServer, RequestId};
use fdf_component::{Driver, DriverContext, Node, ServiceInstance, driver_register};
use fidl::endpoints::ServerEnd;
use fidl_fuchsia_driver_framework::{NodeAddArgs, NodeControllerMarker, NodeError};
use fidl_fuchsia_hardware_block_volume as fvolume;
use fidl_fuchsia_storage_block as fblock;
use fidl_fuchsia_storage_block::BlockInfo;
use fidl_next_fuchsia_hardware_cqhci::{self as cqhci, EmmcPartitionId};
use fidl_next_fuchsia_hardware_rpmb as rpmb;
use fuchsia_async as fasync;
use fuchsia_async::Scope;
use fuchsia_component::server::ServiceFs;
use fuchsia_sync::Mutex;
use futures::StreamExt as _;
use log::{debug, error, info, warn};
use zx::{HandleBased as _, Status};

mod command_queue;
mod dma_buffer;
mod partition;
mod transfer_manager;

#[cfg(test)]
mod tests;

pub fn partition_name(id: EmmcPartitionId) -> &'static str {
    match id {
        EmmcPartitionId::UserDataPartition => "user",
        EmmcPartitionId::BootPartition1 => "boot1",
        EmmcPartitionId::BootPartition2 => "boot2",
    }
}

pub fn to_status(error: anyhow::Error) -> zx::Status {
    if let Some(root_cause) = error.root_cause().downcast_ref::<zx::Status>() {
        *root_cause
    } else {
        zx::Status::INTERNAL
    }
}

async fn add_child(
    node: &Node,
    args: NodeAddArgs,
    controller: ServerEnd<NodeControllerMarker>,
) -> Result<(), NodeError> {
    node.proxy().add_child(args, controller, None).await.map_err(|err| {
        warn!(err:?; "FIDL error from add_child");
        NodeError::Internal
    })?
}

async fn handle_node_requests(
    node: Arc<Node>,
    mut requests: fvolume::NodeRequestStream,
) -> Result<(), anyhow::Error> {
    while let Some(request) = requests.next().await {
        match request? {
            fvolume::NodeRequest::AddChild { args, controller, responder } => {
                let res = add_child(node.as_ref(), args, controller).await.inspect_err(|err| {
                    error!(err:?; "Failed to add child node");
                });
                responder.send(res)?;
            }
        }
    }
    Ok(())
}

struct PartitionServer {
    server: BlockServer<SessionManager<EmmcPartition>>,
}

impl PartitionServer {
    fn new(
        block_info: BlockInfo,
        partition: EmmcPartitionId,
        command_queue: Arc<CommandQueue>,
    ) -> Self {
        Self {
            server: BlockServer::new(
                block_info.block_size,
                Arc::new(SessionManager::new(Arc::new(EmmcPartition::new(
                    partition,
                    Arc::downgrade(&command_queue),
                    block_info,
                )))),
            ),
        }
    }

    fn shutdown(&self) {
        self.server.session_manager().terminate();
    }
}

impl TaskStatusReceiver for PartitionServer {
    fn complete(&self, request_id: RequestId, status: zx::Status) {
        self.server.session_manager().complete_request(request_id, status);
    }
}

struct CqhciDriver {
    _node: Arc<Node>,
    // This scope handles incoming FIDL requests.  Tasks in this scope may retain strong references
    // to [`Self::command_queue`].
    scope: Mutex<Option<Scope>>,
    partitions: Arc<Mutex<BTreeMap<String, Arc<PartitionServer>>>>,
    command_queue: Mutex<Option<Arc<CommandQueue>>>,
}

driver_register!(CqhciDriver);

struct RpmbService {
    scope: Scope,
    command_queue: Weak<CommandQueue>,
}

impl rpmb::ServiceHandler for RpmbService {
    fn device(&self, server_end: fidl_next::ServerEnd<rpmb::Rpmb>) {
        if let Some(command_queue) = self.command_queue.upgrade() {
            server_end.spawn_on(RpmbConnection { command_queue }, &self.scope);
        }
    }
}

struct RpmbConnection {
    command_queue: Arc<CommandQueue>,
}

impl rpmb::RpmbServerHandler for RpmbConnection {
    async fn get_device_info(
        &mut self,
        responder: fidl_next::Responder<rpmb::rpmb::GetDeviceInfo>,
    ) {
        match self.command_queue.get_rpmb_info().await {
            Ok(info) => {
                if let Err(err) = responder.respond(info).await {
                    log::warn!(err:?; "Failed to send rpmb GetDeviceInfo response");
                }
            }
            Err(status) => {
                log::warn!("Failed to get rpmb info: {}", status);
            }
        }
    }

    async fn request(
        &mut self,
        request: fidl_next::Request<rpmb::rpmb::Request>,
        responder: fidl_next::Responder<rpmb::rpmb::Request>,
    ) {
        let request = request.payload();
        let res = match self.command_queue.rpmb_request(request.request).await {
            Ok(()) => responder.respond(()).await,
            Err(status) => responder.respond_err(status.into_raw()).await,
        };
        if let Err(err) = res {
            log::warn!(err:?; "Failed to send rpmb response");
        }
    }
}

#[cfg(not(test))]
fn get_cqhci_client(
    service: &ServiceInstance<cqhci::Service>,
) -> Result<Box<dyn CommandQueueHost>, zx::Status> {
    let (cqhci_client_end, cqhci_server_end) = fdf_fidl::create_channel();
    service.cqhci(cqhci_server_end).map_err(|err| {
        error!(err:?; "Failed to connect to Cqhci protocol");
        zx::Status::INVALID_ARGS
    })?;
    Ok(Box::new(cqhci_client_end.spawn()))
}

/// Inject a fake instance of CommandQueueHost for testing hooks.
/// We do this in order to substitute fake MMIOs for testing purposes.
#[cfg(test)]
fn get_cqhci_client(
    _service: &ServiceInstance<cqhci::Service>,
) -> Result<Box<dyn CommandQueueHost>, zx::Status> {
    Ok(tests::TestCommandQueueHost::global())
}

impl Driver for CqhciDriver {
    const NAME: &str = "cqhci";

    async fn start(mut context: DriverContext) -> Result<Self, Status> {
        info!("cqhci driver starting");
        let (cqhci, rpmb) = {
            let service: ServiceInstance<cqhci::Service> =
                context.incoming.service().connect_next().inspect_err(|status| {
                    error!(status:?; "Failed to connect to Cqhci service");
                })?;
            let (rpmb_client_end, rpmb_server_end) = fdf_fidl::create_channel();
            service.rpmb(rpmb_server_end).map_err(|err| {
                error!(err:?; "Failed to connect to Rpmb protocol");
                zx::Status::INVALID_ARGS
            })?;
            let rpmb = rpmb_client_end.spawn();

            (get_cqhci_client(&service)?, rpmb)
        };

        let vmar =
            context.vmar().duplicate_handle(zx::Rights::SAME_RIGHTS).inspect_err(|status| {
                error!(status:?; "Failed to duplicate VMAR");
            })?;

        let mut host_info = cqhci.info().await.inspect_err(|status| {
            error!(status:?; "Failed to get host info");
        })?;

        let command_queue =
            CommandQueue::initialize(vmar, cqhci, rpmb, &mut host_info).await.map_err(|err| {
                error!(err:?; "Failed to initialize command queueing");
                to_status(err)
            })?;

        let mut fs = ServiceFs::new();
        let scope = Scope::new();

        let mut partitions = BTreeMap::new();

        for partition in host_info.partitions {
            let block_info = fblock::BlockInfo {
                block_count: partition.block_count,
                block_size: partition.block_size,
                max_transfer_size: host_info.sdmmc_host_info.max_transfer_size,
                flags: command_queue.device_flags(),
            };
            let partition_server =
                Arc::new(PartitionServer::new(block_info, partition.id, command_queue.clone()));
            command_queue.register_partition(
                partition.id,
                Arc::downgrade(&partition_server) as Weak<dyn TaskStatusReceiver>,
            );
            partitions.insert(partition_name(partition.id).to_string(), partition_server);
            fs.dir("svc").add_fidl_service_instance(partition_name(partition.id), move |request| {
                (request, partition_name(partition.id).to_string())
            });
        }

        fs.dir("svc").add_fidl_next_service_instance::<rpmb::Service, _>(
            "default",
            RpmbService { scope: scope.new_child(), command_queue: Arc::downgrade(&command_queue) },
        );

        context.serve_outgoing(&mut fs)?;

        let partitions = Arc::new(Mutex::new(partitions));
        let partitions_clone = partitions.clone();
        let node = Arc::new(context.take_node()?);
        let node_clone = node.clone();
        let driver = CqhciDriver {
            _node: node,
            scope: Mutex::new(Some(scope)),
            partitions,
            command_queue: Mutex::new(Some(command_queue)),
        };
        if let Some(scope) = driver.scope.lock().as_ref() {
            scope.spawn(async move {
                fs.for_each_concurrent(None, move |(request, partition_name)| {
                    let partitions_clone = partitions_clone.clone();
                    let node_clone = node_clone.clone();
                    async move {
                        match request {
                            fvolume::ServiceRequest::Volume(requests) => {
                                let partitions_clone = partitions_clone.clone();
                                let partition =
                                    partitions_clone.lock().get(&partition_name).cloned();
                                if let Some(partition) = partition {
                                    if let Err(err) =
                                        partition.server.handle_requests(requests).await
                                    {
                                        error!(
                                            err:?;
                                            "Failed to handle requests for part {partition_name}"
                                        );
                                    }
                                } else {
                                    error!("Invalid partition {partition_name}");
                                }
                            }
                            fvolume::ServiceRequest::Node(requests) => {
                                let node_clone = node_clone.clone();
                                if let Err(err) = handle_node_requests(node_clone, requests).await {
                                    error!(
                                        err:?;
                                        "Failed to handle node requests for part {partition_name}"
                                    );
                                }
                            }
                            fvolume::ServiceRequest::InlineEncryption(_) => {
                                // TODO(https://fxbug.dev/42176727): Support inlineencryption
                                error!("InlineEncryption not yet supported");
                            }
                        }
                    }
                })
                .await;
            });
        }

        info!("cqhci driver started");
        Ok(driver)
    }

    async fn stop(&self) {
        info!("cqhci driver stopping");
        let Some(command_queue) = self.command_queue.lock().take() else { unreachable!() };
        command_queue.shutdown().await;
        debug!("cqhci shut down");

        {
            let partitions = std::mem::take(&mut *self.partitions.lock());
            for (_, partition) in partitions {
                fasync::unblock(move || partition.shutdown()).await;
            }
        }
        debug!("sessions closed");
        let scope = self.scope.lock().take();
        if let Some(scope) = scope {
            scope.cancel().await;
        }
        debug!("scope cancelled");
        debug_assert!(Arc::strong_count(&command_queue) == 1);
        command_queue.unpin_buffers();
        info!("cqhci driver stopped");
    }
}
