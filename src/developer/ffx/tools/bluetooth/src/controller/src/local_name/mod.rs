// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::ControllerTool;
use ffx_bluetooth_controller_args::local_name::{LocalNameCommand, LocalNameSubCommand};
use ffx_writer::{SimpleWriter, ToolIO as _};
use fho::Result;

pub async fn handle_local_name(
    tool: &ControllerTool,
    cmd: &LocalNameCommand,
    writer: &mut SimpleWriter,
) -> Result<()> {
    match &cmd.subcommand {
        LocalNameSubCommand::Set(cmd) => {
            tool.set_local_name(cmd.name.clone()).await?;
            writer.line(format!("Setting local name to: {}", cmd.name))?;
        }
    }
    Ok(())
}

impl ControllerTool {
    async fn set_local_name(&self, name: String) -> Result<()> {
        Ok(self
            .host_controller
            .set_local_name(&name)
            .await
            .map_err(|err| fho::Error::Unexpected(anyhow::anyhow!("FIDL error: {err}")))?
            .map_err(|err| {
                fho::Error::Unexpected(anyhow::anyhow!(
                    "fuchsia.bluetooth.affordances.HostController error: {err:?}"
                ))
            })?)
    }
}
