// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fdf_component::{Driver, DriverContext, Node, NodeBuilder, ServiceInstance, driver_register};
use fidl_next_fuchsia_hardware_i2cimpl as i2cimpl;
use log::{error, info};
use zx::Status;

/// The implementation of our driver will live in this object, which implements [`Driver`].
#[allow(unused)]
struct ZirconChildDriver {
    /// The [`Node`] is our handle to the node we bound to. We need to keep this handle
    /// open to keep the node around.
    node: Node,
}

// This creates the exported driver registration structures that allow the driver host to
// find and run the start and stop methods on our `ZirconChildDriver`.
driver_register!(ZirconChildDriver);

impl Driver for ZirconChildDriver {
    const NAME: &str = "zircon_child_rust_next_driver";

    async fn start(mut context: DriverContext) -> Result<Self, Status> {
        info!(
            "Binding node client. Every driver needs to do this for the driver to be considered loaded."
        );
        let node = context.take_node()?;

        let device = get_i2cimpl_device(&context).unwrap();
        let device_sender = device.sender().clone();
        fuchsia_async::Task::spawn(async { device.run_sender().await.unwrap() }).detach();
        let transfer_size =
            device_sender.get_max_transfer_size().unwrap().await.unwrap().unwrap().size;
        info!("i2cimpl max transfer size: {transfer_size}");

        info!("Adding child node with i2cimpl max transfer size as a property value");
        let child_node = NodeBuilder::new("transport-child")
            .add_property(bind_fuchsia_test::TEST_CHILD, u64::from(transfer_size) as u32)
            .build();
        node.add_child(child_node).await?;

        device_sender.set_bitrate(0x5u32).unwrap().await.unwrap().unwrap();

        Ok(Self { node })
    }

    async fn stop(&self) {
        info!("ZirconChildDriver::stop() was invoked. Use this function to do any cleanup needed.");
    }
}

fn get_i2cimpl_device(
    context: &DriverContext,
) -> Result<fidl_next::Client<i2cimpl::Device, fdf_fidl::DriverChannel>, Status> {
    let service_proxy: ServiceInstance<i2cimpl::Service> =
        context.incoming.service().connect_next()?;

    let (client_end, server_end) = fdf_fidl::create_channel();

    service_proxy.device(server_end).map_err(|err| {
        error!("Error connecting to i2cimpl device proxy at driver startup: {err}");
        Status::INTERNAL
    })?;

    Ok(fidl_next::Client::new(client_end))
}
