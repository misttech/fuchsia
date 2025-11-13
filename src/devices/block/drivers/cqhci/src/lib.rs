// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fdf_component::{Driver, DriverContext, Node, driver_register};
use log::info;
use zx::Status;

struct CqhciDriver {
    _node: Node,
    scope: fuchsia_async::Scope,
}

driver_register!(CqhciDriver);

impl Driver for CqhciDriver {
    const NAME: &str = "cqhci";

    async fn start(mut context: DriverContext) -> Result<Self, Status> {
        let node = context.take_node()?;
        info!("cqhci driver started");
        Ok(CqhciDriver { _node: node, scope: fuchsia_async::Scope::new() })
    }

    async fn stop(&self) {
        info!("Shutting down cqhci");
        self.scope.to_handle().cancel().await;
        info!("Shutdown complete");
    }
}
