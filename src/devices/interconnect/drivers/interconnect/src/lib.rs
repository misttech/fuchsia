// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::graph::{NodeGraph, NodeId, Path, PathId};
use fdf_component::{Driver, DriverContext, Node, NodeBuilder, ServiceOffer, driver_register};
use fidl_fuchsia_driver_framework::NodeControllerProxy;
use fidl_next::{FlexibleResult, FrameworkError};
use fuchsia_component::server::ServiceFs;
use fuchsia_inspect::{Inspector, Property, UintProperty};
use futures::channel::mpsc;
use futures::{SinkExt, StreamExt};
use log::{debug, error, info};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use zx::Status;

use fidl_next_fuchsia_hardware_interconnect as icc;
use fuchsia_async as fasync;
use fuchsia_trace as ftrace;

mod graph;

driver_register!(InterconnectDriver);

struct Child {
    /// List of nodes following directed path from start of path to end of path.
    path: Path,
    tag: Option<u32>,
    /// Directed graph which stores all nodes and bandwidth requests for each of their incoming
    /// edges.
    graph: Rc<RefCell<NodeGraph>>,
    #[allow(unused)]
    controller: NodeControllerProxy,
    device: fidl_next::Client<icc::Device>,
    #[allow(unused)]
    inspect: fuchsia_inspect::Node,
    average_bandwidth_bps: UintProperty,
    peak_bandwidth_bps: UintProperty,
    sync_state: Rc<Cell<bool>>,
}

impl Child {
    async fn set_bandwidth_impl(
        &self,
        average_bandwidth_bps: Option<u64>,
        peak_bandwidth_bps: Option<u64>,
        tag: Option<u32>,
    ) -> Result<(), Status> {
        let average_bandwidth_bps = average_bandwidth_bps.ok_or(Status::INVALID_ARGS)?;
        let peak_bandwidth_bps = peak_bandwidth_bps.ok_or(Status::INVALID_ARGS)?;
        let tag = tag.or(self.tag);

        self.average_bandwidth_bps.set(average_bandwidth_bps);
        self.peak_bandwidth_bps.set(peak_bandwidth_bps);

        ftrace::duration!(c"interconnect", c"set_bandwidth",
            "path" => self.path.name(),
            "average_bandwidth_bps" => average_bandwidth_bps,
            "peak_bandwidth_bps" => peak_bandwidth_bps);

        // If we've not hit sync_state yet, update the graph and return right away.
        if !self.sync_state.get() {
            let mut graph = self.graph.borrow_mut();
            graph.update_path(&self.path, average_bandwidth_bps, peak_bandwidth_bps, tag);
            return Ok(());
        }

        let requests = {
            let mut graph = self.graph.borrow_mut();
            graph.update_path(&self.path, average_bandwidth_bps, peak_bandwidth_bps, tag);
            graph.make_bandwidth_requests(&self.path)
        };

        let result = self.device.set_nodes_bandwidth(&requests).await.map_err(|err| {
            error!("Failed to set bandwidth with {err}");
            Status::INTERNAL
        })?;

        let response = match result {
            FlexibleResult::Ok(response) => Ok(response),
            FlexibleResult::Err(err) => {
                let err = Status::from_raw(err);
                error!("Failed to set bandwidth with {err}");
                Err(err)
            }
            FlexibleResult::FrameworkErr(err) => {
                panic!("Device does not implement `set_nodes_bandwidth`: {err:?}")
            }
        }?;

        self.graph.borrow_mut().update_stats(response.aggregated_bandwidth);

        // TODO(b/405206028): On failure, try to set old values?

        Ok(())
    }
}
impl icc::PathLocalServerHandler for &Child {
    async fn set_bandwidth(
        &mut self,
        request: fidl_next::Request<icc::path::SetBandwidth>,
        responder: fidl_next::Responder<icc::path::SetBandwidth>,
    ) {
        let payload = request.payload();
        let res = self
            .set_bandwidth_impl(
                payload.average_bandwidth_bps,
                payload.peak_bandwidth_bps,
                payload.tag,
            )
            .await;
        match res {
            Ok(()) => responder.respond(()).await.unwrap(),
            Err(err) => responder.respond_err(err.into_raw()).await.unwrap(),
        }
    }
}

struct PathService {
    scope: fasync::ScopeHandle,
    name: String,
    sender: mpsc::Sender<(fidl_next::ServerEnd<icc::Path>, String)>,
}

impl icc::PathServiceHandler for PathService {
    fn path(&self, server_end: fidl_next::ServerEnd<icc::Path>) {
        let name = self.name.clone();
        let mut sender = self.sender.clone();
        self.scope.spawn_local(async move {
            sender.send((server_end, name)).await.ok();
        });
    }
}

#[allow(unused)]
struct InterconnectDriver {
    node: Node,
    inspector: Inspector,
    scope: fasync::Scope,
}

impl InterconnectDriver {
    async fn start_local(mut context: DriverContext) -> Result<Self, Status> {
        let node = context.take_node()?;

        let inspector = Inspector::default();
        context.publish_inspect(&inspector, fasync::Scope::current())?;

        let device_service: fdf_component::ServiceInstance<icc::Service> =
            context.incoming.service().connect_next()?;
        let (device_client, device_server) = fidl_next::fuchsia::create_channel();
        device_service.device(device_server).map_err(|err| {
            error!("Error connecting to interconnect device at driver startup: {err}");
            Status::INTERNAL
        })?;
        let device = device_client.spawn();

        let node_graph = device
            .get_node_graph()
            .await
            .map_err(|err| {
                error!("Failed to get node graph with {err}");
                Status::INTERNAL
            })?
            .unwrap(); // Flexible::FrameworkErr means the method is not implemented
        let mut graph = NodeGraph::new(node_graph.nodes, node_graph.edges)?;

        let path_endpoints = device
            .get_path_endpoints()
            .await
            .map_err(|err| {
                error!("Failed to get path endpoints with {err}");
                Status::INTERNAL
            })?
            .unwrap(); // Flexible::FrameworkErr means the method is not implemented
        let paths: Vec<_> = Result::from_iter(path_endpoints.paths.into_iter().map(|path| {
            let path_id = PathId(path.id.ok_or(Status::INVALID_ARGS)?);
            let path_name = path.name.ok_or(Status::INVALID_ARGS)?;
            let src_node_id = NodeId(path.src_node_id.ok_or(Status::INVALID_ARGS)?);
            let dst_node_id = NodeId(path.dst_node_id.ok_or(Status::INVALID_ARGS)?);
            Ok::<_, Status>((
                graph.make_path(path_id, path_name, src_node_id, dst_node_id)?,
                path.tag,
            ))
        }))?;

        let mut outgoing = ServiceFs::new();

        let paths_inspect = inspector.root().create_child("paths");

        let graph = Rc::new(RefCell::new(graph));
        let graph_clone = graph.clone();
        inspector.root().record_lazy_child_with_thread_local("nodes", move || {
            Box::pin({
                let graph = graph_clone.clone();
                async move {
                    let inspector = Inspector::default();
                    graph.borrow().record_inspect(inspector.root());
                    Ok(inspector)
                }
            })
        });

        let (conn_tx, conn_rx) = mpsc::channel(1);
        let scope = fasync::Scope::new_with_name("driver");
        let mut children = BTreeMap::new();
        let mut sync_state_completers = Vec::new();
        let sync_state = Rc::new(Cell::new(false));
        for (path, tag) in paths {
            let name = format!("{}-{}", path.name(), path.id());
            let offer = ServiceOffer::new_marker_next(icc::PathService)
                .add_default_named_next(
                    &mut outgoing,
                    &name,
                    PathService {
                        scope: scope.clone(),
                        name: name.clone(),
                        sender: conn_tx.clone(),
                    },
                )
                .build_zircon_offer_next();

            let node_args = NodeBuilder::new(&name)
                .add_property(bind_fuchsia::BIND_INTERCONNECT_PATH_ID, path.id().0)
                .add_offer(offer)
                .build();
            let controller = node.add_child(node_args).await?.into_proxy();
            let graph = graph.clone();
            let device = device.clone();
            let inspect = paths_inspect.create_child(path.name());
            let average_bandwidth_bps = inspect.create_uint("average_bandwidth_bps", 0);
            let peak_bandwidth_bps = inspect.create_uint("peak_bandwidth_bps", 0);
            path.record_inspect(&inspect);

            let controller_clone = controller.clone();
            let path_name = path.name().to_string();
            sync_state_completers.push(async move {
                match controller_clone.wait_for_driver().await {
                    Err(e) => {
                        error!("Failed to wait for driver to bind and start: {e:?}");
                    }
                    Ok(r) => {
                        debug!("Driver bound to {path_name} with result: {r:?}");
                    }
                }
            });

            let sync_state = sync_state.clone();

            children.insert(
                name.clone(),
                Child {
                    path,
                    graph,
                    controller,
                    device,
                    inspect,
                    average_bandwidth_bps,
                    peak_bandwidth_bps,
                    sync_state,
                    tag,
                },
            );
        }
        inspector.root().record(paths_inspect);

        let sync_state_clone = sync_state.clone();
        inspector.root().record_lazy_child_with_thread_local("sync_state", move || {
            Box::pin({
                let sync_state = sync_state_clone.clone();
                async move {
                    let inspector = Inspector::default();
                    inspector.root().record_bool("sync_state", sync_state.get());
                    Ok(inspector)
                }
            })
        });

        // Once we all child devices spawned have had drivers bind and run their start routines, we
        // can assume they have also cast their initial votes and inform our parent to act upon
        // these votes.
        let sync_state_clone = sync_state.clone();
        scope.spawn_local(async move {
            futures::future::join_all(sync_state_completers).await;
            info!("Sync state achieved. Sending initial votes");
            sync_state_clone.set(true);

            // Vote for all nodes with all received votes thus far.
            let requests = { graph.borrow().make_inital_bandwidth_requests() };

            match device.set_nodes_bandwidth(&requests).await {
                Err(err) => {
                    error!("Failed to set bandwidth with {err}");
                }
                Ok(FlexibleResult::Ok(result)) => {
                    graph.borrow_mut().update_stats(result.aggregated_bandwidth);
                }
                Ok(FlexibleResult::Err(err)) => {
                    error!("Failed to set bandwidth with {}", Status::from_raw(err));
                }
                Ok(FlexibleResult::FrameworkErr(FrameworkError::UnknownMethod)) => {
                    panic!("Device does not implement set_nodes_bandwidth");
                }
            };
        });

        context.serve_outgoing(&mut outgoing)?;

        let children = Rc::new(children);
        scope.spawn_local(outgoing.collect());
        scope.spawn_local(async move {
            conn_rx
                .for_each_concurrent(None, move |(request, child_name)| {
                    let children = children.clone();
                    async move {
                        if let Some(node) = children.get(&child_name) {
                            let dispatcher = fidl_next::ServerDispatcher::new(request);
                            dispatcher
                                .run_local(node)
                                .await
                                .inspect_err(|err| {
                                    error!("Error in child dispatch loop for {child_name}: {err}");
                                })
                                .ok();
                        } else {
                            error!("Failed to find child {child_name}");
                        }
                    }
                })
                .await;
        });

        Ok(Self { node, inspector, scope })
    }
}

impl Driver for InterconnectDriver {
    const NAME: &str = "interconnect";

    async fn start(context: DriverContext) -> Result<Self, Status> {
        fasync::Task::local(Self::start_local(context)).await
    }

    async fn stop(&self) {}
}
