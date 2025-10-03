// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Result};
use fuchsia_component::client;
use powercli::args::PowerCommand;
use {fidl_fuchsia_power_topology_test as fpt, fuchsia_async as fasync};

struct Connector {}

impl Connector {
    fn new() -> Self {
        Self {}
    }
}

impl powercli::connector::Connector for Connector {
    async fn get_system_activity_control(&self) -> Result<fpt::SystemActivityControlProxy> {
        client::connect_to_protocol::<fpt::SystemActivityControlMarker>()
            .context("Failed to connect to system activity control service")
    }
}

#[fasync::run_singlethreaded]
async fn main() -> Result<()> {
    let cmd: PowerCommand = argh::from_env();
    powercli::power(cmd, Connector::new(), &mut std::io::stdout()).await
}
