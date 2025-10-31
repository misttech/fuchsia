// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fdf_component::{Driver, DriverContext, Node, NodeBuilder, ServiceOffer, driver_register};
use fidl_next::{Request, Responder, ServerEnd};
use fidl_next_fuchsia_hardware_i2cimpl::device::{GetMaxTransferSize, SetBitrate, Transact};
use fidl_next_fuchsia_hardware_i2cimpl::generic::ReadData;
use fidl_next_fuchsia_hardware_i2cimpl::{self as i2cimpl, DeviceSetBitrateResponse};
use fuchsia_async::{Scope, ScopeHandle};
use fuchsia_component::server::ServiceFs;
use futures::StreamExt as _;
use log::{info, warn};
use zx::Status;

/// The implementation of our driver will live in this object, which implements [`Driver`].
#[allow(unused)]
struct DriverTransportParent {
    /// The [`NodeProxy`] is our handle to the node we bound to. We need to keep this handle
    /// open to keep the node around.
    node: Node,
    /// The scope for the driver.
    scope: Scope,
}

// This creates the exported driver registration structures that allow the driver host to
// find and run the start and stop methods on our `DriverTransportParent`.
driver_register!(DriverTransportParent);

struct DeviceServer;

impl i2cimpl::DeviceServerHandler for DeviceServer {
    async fn get_max_transfer_size(&mut self, responder: Responder<GetMaxTransferSize>) {
        responder
            .respond(0x1234ABCDu64)
            .await
            .unwrap_or_else(|err| warn!("Failed to send get_max_transfer_size response: {err:?}"));
    }

    async fn set_bitrate(
        &mut self,
        request: Request<SetBitrate>,
        responder: Responder<SetBitrate>,
    ) {
        if request.payload().bitrate == 5 {
            responder
                .respond(DeviceSetBitrateResponse {})
                .await
                .unwrap_or_else(|err| warn!("Failed to send set_bitrate response: {err:?}"));
        } else {
            responder
                .respond_err(Status::INVALID_ARGS.into_raw())
                .await
                .unwrap_or_else(|err| warn!("Failed to send set_bitrate response: {err:?}"));
        }
    }

    async fn transact(&mut self, _request: Request<Transact>, responder: Responder<Transact>) {
        responder
            .respond([ReadData { data: [0, 1, 2] }])
            .await
            .unwrap_or_else(|err| warn!("Failed to send transact response: {err:?}"));
    }
}

struct Service {
    scope: ScopeHandle,
}

impl i2cimpl::ServiceHandler for Service {
    fn device(&self, server_end: ServerEnd<i2cimpl::Device>) {
        server_end.spawn_on(DeviceServer, &self.scope).detach_on_drop();
    }
}

impl Driver for DriverTransportParent {
    const NAME: &str = "driver_parent_rust_next_driver";

    async fn start(mut context: DriverContext) -> Result<Self, Status> {
        info!(
            "Binding node client. Every driver needs to do this for the driver to be considered loaded."
        );
        let node = context.take_node()?;

        let scope = Scope::new();

        info!("Offering an i2c service in the outgoing directory");
        let mut outgoing = ServiceFs::new();
        let offer = ServiceOffer::<i2cimpl::Service>::new_next()
            .add_default_named_next(&mut outgoing, "default", Service { scope: scope.to_handle() })
            .build_driver_offer();

        info!("Creating child node with a service offer");
        let child_node =
            NodeBuilder::new("driver_transport_rust_next_child").add_offer(offer).build();
        node.add_child(child_node).await?;

        context.serve_outgoing(&mut outgoing)?;

        scope.spawn(outgoing.collect());

        Ok(Self { node, scope })
    }

    async fn stop(&self) {
        info!(
            "DriverTransportParent::stop() was invoked. Use this function to do any cleanup needed."
        );
    }
}
