// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use async_trait::async_trait;
use component_debug::cli::capability_cmd;
use component_debug_fdomain as component_debug;
use errors::ffx_error;
use ffx_component::rcs::connect_to_realm_query_f as connect_to_realm_query;
use ffx_component_capability_args::ComponentCapabilityCommand;
use ffx_writer::SimpleWriter;
use fho::{FfxMain, FfxTool};
use target_holders::fdomain::RemoteControlProxyHolder;

#[derive(FfxTool)]
pub struct CapabilityTool {
    #[command]
    cmd: ComponentCapabilityCommand,
    rcs: RemoteControlProxyHolder,
}

fho::embedded_plugin!(CapabilityTool);

#[async_trait(?Send)]
impl FfxMain for CapabilityTool {
    // TODO(b/471819893) Support actual JSON output, not just "raw"
    type Writer = SimpleWriter;
    type Error = ::fho::Error;

    async fn main(self, writer: Self::Writer) -> fho::Result<()> {
        let realm_query = connect_to_realm_query(&self.rcs).await?;
        // All errors from component_debug library are user-visible.
        capability_cmd(self.cmd.capability, realm_query, writer)
            .await
            .map_err(|e| ffx_error!(e))?;
        Ok(())
    }
}
