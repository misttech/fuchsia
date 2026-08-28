// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Goodix GT6853 Touchscreen Driver for Fuchsia.

pub mod data_types;
pub mod registers;

use fdf_component::{Driver, DriverContext, DriverError, Node, driver_register};
use log::info;

struct GoodixGt6853Driver {
    _node: Node,
}

driver_register!(GoodixGt6853Driver);

impl Driver for GoodixGt6853Driver {
    const NAME: &str = "goodix-gt6853";

    async fn start(mut context: DriverContext) -> Result<Self, DriverError> {
        info!("GoodixGt6853Driver (Rust) starting...");
        let _node = context.take_node()?;
        info!("GoodixGt6853Driver (Rust) started successfully!");
        Ok(Self { _node })
    }

    async fn stop(&self) {
        info!("GoodixGt6853Driver (Rust) stopped!");
    }
}
