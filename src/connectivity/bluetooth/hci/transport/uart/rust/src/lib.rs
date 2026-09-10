// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fdf_component::{Driver, DriverContext, DriverError, Node, driver_register};
use fuchsia_async as fasync;
use log::info;

/// Bluetooth HCI UART transport driver in Rust.
pub struct BtTransportUart {
    _node: Node,
    _scope: fasync::Scope,
}

driver_register!(BtTransportUart);

impl Driver for BtTransportUart {
    const NAME: &str = "bt-transport-uart-rust";

    async fn start(mut context: DriverContext) -> Result<Self, DriverError> {
        info!("BtTransportUart (Rust)::start() invoked");
        let node = context.take_node()?;
        let scope = fasync::Scope::new();

        Ok(Self { _node: node, _scope: scope })
    }

    async fn stop(&self) {
        info!("BtTransportUart (Rust)::stop() invoked");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fdf_component::testing::harness::TestHarness;

    #[fuchsia::test]
    async fn test_driver_start_stop() {
        let mut harness = TestHarness::<BtTransportUart>::new();
        let started_driver =
            harness.start_driver().await.expect("driver should start successfully");
        started_driver.stop_driver().await;
    }
}
