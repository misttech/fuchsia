// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::resources::PlatformResources;

use fdf_component::{Driver, DriverContext, DriverError, Node, driver_register};
use log::info;

/// Interfaces with the Fuchsia Driver Framework.
struct VirtioGpuDisplayDriver {
    /// The driver must maintain an open connection to the Node.
    #[expect(unused)]
    node: Node,
}

driver_register!(VirtioGpuDisplayDriver);

impl Driver for VirtioGpuDisplayDriver {
    const NAME: &str = "virtio-gpu-display";

    async fn start(mut context: DriverContext) -> Result<Self, DriverError> {
        info!("VirtioGpuDisplayDriver::start()");

        #[allow(unused)]
        let platform_resources = PlatformResources::build(&mut context)?;

        let node = context.take_node()?;

        Ok(Self { node })
    }

    async fn stop(&self) {
        info!("VirtioGpuDisplayDriver::stop()");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fdf_component::testing::harness::TestHarness;

    #[fuchsia::test]
    async fn test_driver_start() {
        let mut harness = TestHarness::<VirtioGpuDisplayDriver>::new();

        let started_driver = harness.start_driver().await.expect("Driver start failed");

        started_driver.stop_driver().await;
    }
}
