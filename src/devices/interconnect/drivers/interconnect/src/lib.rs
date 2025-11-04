// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::graph::{NodeGraph, NodeId, Path, PathId};
use fdf_component::{Driver, DriverContext, Node, NodeBuilder, ServiceOffer, driver_register};
use fidl_fuchsia_driver_framework::NodeControllerProxy;
use fuchsia_component::server::ServiceFs;
use fuchsia_inspect::Inspector;
use futures::{StreamExt, TryStreamExt};
use log::{debug, error, info, warn};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use zx::Status;

use {fidl_fuchsia_hardware_interconnect as icc, fuchsia_async as fasync, fuchsia_trace as ftrace};

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
    device: icc::DeviceProxy,
    inspect: fuchsia_inspect::Node,
    sync_state: Rc<Cell<bool>>,
}

impl Child {
    async fn set_bandwidth(
        &self,
        average_bandwidth_bps: Option<u64>,
        peak_bandwidth_bps: Option<u64>,
        tag: Option<u32>,
    ) -> Result<(), Status> {
        let average_bandwidth_bps = average_bandwidth_bps.ok_or(Status::INVALID_ARGS)?;
        let peak_bandwidth_bps = peak_bandwidth_bps.ok_or(Status::INVALID_ARGS)?;
        let tag = tag.or(self.tag);

        ftrace::duration!(c"interconnect", c"set_bandwidth",
            "path" => self.path.name(),
            "average_bandwidth_bps" => average_bandwidth_bps,
            "peak_bandwidth_bps" => peak_bandwidth_bps);

        self.inspect.record_uint("average_bandwidth_bps", average_bandwidth_bps);
        self.inspect.record_uint("peak_bandwidth_bps", peak_bandwidth_bps);

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

        let result = self
            .device
            .set_nodes_bandwidth(&requests)
            .await
            .map_err(|err| {
                error!("Failed to set bandwidth with {err}");
                Status::INTERNAL
            })?
            .map_err(Status::from_raw)?;

        for node in result {
            ftrace::instant!(c"interconnect", c"node_bandwidth", ftrace::Scope::Process,
                "id" => node.node_id.unwrap_or(0),
                "average_bandwidth_bps" => node.average_bandwidth_bps.unwrap_or(0),
                "peak_bandwidth_bps" => node.peak_bandwidth_bps.unwrap_or(0));
        }

        // TODO(b/405206028): On failure, try to set old values?

        Ok(())
    }

    async fn run_path_server(&self, mut service: icc::PathRequestStream) {
        use icc::PathRequest::*;
        while let Some(req) = service.try_next().await.unwrap() {
            match req {
                SetBandwidth { payload, responder, .. } => responder.send(
                    self.set_bandwidth(
                        payload.average_bandwidth_bps,
                        payload.peak_bandwidth_bps,
                        payload.tag,
                    )
                    .await
                    .map_err(Status::into_raw),
                ),
                // Ignore unknown requests.
                _ => {
                    warn!("Received unknown path request");
                    Ok(())
                }
            }
            .unwrap();
        }
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

        let device = context
            .incoming
            .service_marker(icc::ServiceMarker)
            .connect()?
            .connect_to_device()
            .map_err(|err| {
                error!("Error connecting to interconnect device at driver startup: {err}");
                Status::INTERNAL
            })?;

        let (nodes, edges) = device.get_node_graph().await.map_err(|err| {
            error!("Failed to get node graph with {err}");
            Status::INTERNAL
        })?;
        let mut graph = NodeGraph::new(nodes, edges)?;

        let path_endpoints = device.get_path_endpoints().await.map_err(|err| {
            error!("Failed to get path endpoints with {err}");
            Status::INTERNAL
        })?;
        let paths: Vec<_> = Result::from_iter(path_endpoints.into_iter().map(|path| {
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

        let mut children = BTreeMap::new();
        let mut sync_state_completers = Vec::new();
        let sync_state = Rc::new(Cell::new(false));
        for (path, tag) in paths {
            let name = format!("{}-{}", path.name(), path.id());
            let name_clone = name.clone();
            let offer = ServiceOffer::new()
                .add_default_named(&mut outgoing, &name, move |req| {
                    let icc::PathServiceRequest::Path(service) = req;
                    (service, name_clone.clone())
                })
                .build_zircon_offer();

            let node_args = NodeBuilder::new(&name)
                .add_property(bind_fuchsia::BIND_INTERCONNECT_PATH_ID, path.id().0)
                .add_offer(offer)
                .build();
            let controller = node.add_child(node_args).await?.into_proxy();
            let graph = graph.clone();
            let device = device.clone();
            let inspect = paths_inspect.create_child(path.name());
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
                Child { path, graph, controller, device, inspect, sync_state, tag },
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

        let scope = fasync::Scope::new_with_name("driver");

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
                Ok(Err(err)) => {
                    error!("Failed to set bandwidth with {err:?}");
                }
                Ok(Ok(result)) => {
                    for node in result {
                        ftrace::instant!(c"interconnect", c"node_bandwidth", ftrace::Scope::Process,
                            "id" => node.node_id.unwrap_or(0),
                            "average_bandwidth_bps" => node.average_bandwidth_bps.unwrap_or(0),
                            "peak_bandwidth_bps" => node.peak_bandwidth_bps.unwrap_or(0));
                    }
                }
            };
        });

        context.serve_outgoing(&mut outgoing)?;

        let children = Rc::new(children);
        scope.spawn_local(async move {
            outgoing
                .for_each_concurrent(None, move |(request, child_name)| {
                    let children = children.clone();
                    async move {
                        if let Some(node) = children.get(&child_name) {
                            node.run_path_server(request).await;
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
