// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fdf_component::{
    Driver, DriverContext, Node, NodeBuilder, ZirconServiceOffer, driver_register,
};
use fidl_next::{Request, Responder, ServerEnd, ServerSender};
use fidl_next_fuchsia_hardware_i2c as i2c;
use fuchsia_async::{Scope, ScopeHandle};
use fuchsia_component::server::ServiceFs;
use futures::StreamExt as _;
use log::info;
use zx::Status;

/// The implementation of our driver will live in this object, which implements [`Driver`].
#[allow(unused)]
struct ZirconParentDriver {
    /// The [`NodeProxy`] is our handle to the node we bound to. We need to keep this handle
    /// open to keep the node around.
    node: Node,
    /// The scope for the driver.
    scope: Scope,
}

// This creates the exported driver registration structures that allow the driver host to
// find and run the start and stop methods on our `ZirconParentDriver`.
driver_register!(ZirconParentDriver);

struct DeviceServer;

impl i2c::DeviceServerHandler for DeviceServer {
    async fn transfer(
        &mut self,
        sender: &ServerSender<i2c::Device>,
        _: Request<i2c::device::Transfer>,
        responder: Responder<i2c::device::Transfer>,
    ) {
        responder
            .respond(
                sender,
                Ok::<_, i32>(i2c::DeviceTransferResponse {
                    read_data: vec![vec![0x1u8, 0x2, 0x3]],
                }),
            )
            .await
            .unwrap();
    }

    async fn get_name(
        &mut self,
        sender: &ServerSender<i2c::Device>,
        responder: Responder<i2c::device::GetName>,
    ) {
        responder
            .respond(
                sender,
                Ok::<_, i32>(i2c::DeviceGetNameResponse { name: "rust i2c server".to_string() }),
            )
            .await
            .unwrap();
    }
}

struct Service {
    scope: ScopeHandle,
}

impl i2c::ServiceHandler<zx::Channel> for Service {
    fn device(&self, server_end: ServerEnd<i2c::Device>) {
        server_end.spawn_on(DeviceServer, &self.scope).detach_on_drop();
    }
}

impl Driver for ZirconParentDriver {
    const NAME: &str = "zircon_parent_rust_next_driver";

    async fn start(mut context: DriverContext) -> Result<Self, Status> {
        info!(
            "Binding node client. Every driver needs to do this for the driver to be considered loaded."
        );
        let node = context.take_node()?;

        let scope = Scope::new();

        info!("Offering an i2c service in the outgoing directory");
        let mut outgoing = ServiceFs::new();
        let offer = ZirconServiceOffer::<i2c::Service>::new_next()
            .add_default_named_next(&mut outgoing, "default", Service { scope: scope.to_handle() })
            .build();

        info!("Creating child node with a service offer");
        let child_node =
            NodeBuilder::new("zircon_transport_rust_next_child").add_offer(offer).build();
        node.add_child(child_node).await?;

        context.serve_outgoing(&mut outgoing)?;

        scope.spawn(outgoing.collect());

        Ok(Self { node, scope })
    }

    async fn stop(&self) {
        info!(
            "ZirconParentDriver::stop() was invoked. Use this function to do any cleanup needed."
        );
    }
}
