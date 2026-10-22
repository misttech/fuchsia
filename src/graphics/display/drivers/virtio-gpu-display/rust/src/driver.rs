// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::engine::EngineService;
use crate::resources::PlatformResources;
use crate::virtio_gpu_device::VirtioGpuDeviceBuilder;
use virtio_driver::VirtioPciDeviceBuilder;

use fdf_component::{
    Driver, DriverContext, DriverError, Node, NodeBuilder, ServiceOffer, driver_register,
};
use fidl_next_fuchsia_hardware_display_engine as fidl_display_engine;
use fuchsia_component::server::ServiceFs;
use futures::StreamExt;

/// Interfaces with the Fuchsia Driver Framework.
struct VirtioGpuDisplayDriver {
    /// The driver must maintain an open connection to the Node.
    #[expect(dead_code)]
    node: Node,

    /// Serves the driver's outgoing directory.
    ///
    /// The driver must keep the task alive to accept incoming connections.
    #[expect(dead_code)]
    outgoing_directory_task: fuchsia_async::Task<()>,
}

driver_register!(VirtioGpuDisplayDriver);

impl Driver for VirtioGpuDisplayDriver {
    const NAME: &str = "virtio-gpu-display-rust";

    async fn start(mut context: DriverContext) -> Result<Self, DriverError> {
        log::debug!("virtio-gpu-display driver started");

        let platform_resources = PlatformResources::new(&mut context)?;

        let pci_device_builder = VirtioPciDeviceBuilder::new(platform_resources.pci_client).await?;
        let gpu_device_builder = VirtioGpuDeviceBuilder::new(pci_device_builder);
        let gpu_device = gpu_device_builder.build().await?;

        let device_node = context.take_node()?;

        let mut outgoing = ServiceFs::new();
        let offer = ServiceOffer::<fidl_display_engine::Service>::new_next()
            .add_default_named_next(
                &mut outgoing,
                "default",
                EngineService::new(gpu_device, platform_resources.sysmem_client),
            )
            .build_driver_offer();

        let child_node = NodeBuilder::new("virtio-gpu-display").add_offer(offer).build();
        device_node.add_child(child_node).await?;

        context.serve_outgoing(&mut outgoing)?;
        let outgoing_directory_task = fuchsia_async::Task::local(async move {
            outgoing.collect::<()>().await;
        });

        Ok(Self { node: device_node, outgoing_directory_task })
    }

    async fn stop(&self) {
        log::debug!("virtio-gpu-display driver stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fdf_component::testing::harness::TestHarness;

    // TODO(https://fxbug.dev/504722357): Figure out driver-level testing once
    // the Rust port is complete.
    #[fuchsia::test]
    #[ignore]
    async fn test_driver_start() {
        let mut harness = TestHarness::<VirtioGpuDisplayDriver>::new();

        let started_driver = harness.start_driver().await.expect("Driver start failed");

        started_driver.stop_driver().await;
    }
}
