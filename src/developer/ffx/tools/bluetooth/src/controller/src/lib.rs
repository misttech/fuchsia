// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use ::async_trait::async_trait;
use ::fho::{AvailabilityFlag, FfxMain, FfxTool, Result};
use ffx_writer::{SimpleWriter, ToolIO as _};
use fidl_fuchsia_bluetooth_affordances::HostControllerProxy;
use target_holders::toolbox;

use ffx_bluetooth_controller_args::{ControllerCommand, ControllerSubCommand};

use fuchsia_bluetooth::types::HostInfo;

#[derive(FfxTool)]
#[check(AvailabilityFlag("bluetooth.enabled"))]
pub struct ControllerTool {
    #[command]
    cmd: ControllerCommand,
    #[with(toolbox())]
    host_controller: HostControllerProxy,
}

fho::embedded_plugin!(ControllerTool);
#[async_trait(?Send)]
impl FfxMain for ControllerTool {
    type Writer = SimpleWriter;
    async fn main(self, mut writer: Self::Writer) -> Result<()> {
        let host = self.get_active_host().await?;
        match self.cmd.subcommand {
            // ffx bluetooth controller show
            ControllerSubCommand::Show(ref _cmd) => {
                writer.line(host.to_string())?;
            }
        }
        Ok(())
    }
}

impl ControllerTool {
    async fn get_active_host(&self) -> Result<HostInfo> {
        Ok(HostInfo::try_from(
            self.host_controller
                .get_active_host()
                .await
                .map_err(|err| fho::Error::Unexpected(anyhow::anyhow!("FIDL error: {err}")))?
                .map_err(|err| {
                    fho::Error::Unexpected(anyhow::anyhow!(
                        "fuchsia.bluetooth.sys.HostController error: {err:?}"
                    ))
                })?,
        )
        .expect("Failed to convert between HostInfo types"))
    }
}
