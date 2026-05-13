// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use ::async_trait::async_trait;
use ::ffx_bluetooth_pairable_args::{PairableCommand, PairableSubCommand};
use ::fho::{AvailabilityFlag, FfxMain, FfxTool, Result};
use ffx_writer::{SimpleWriter, ToolIO as _};
use fidl_fuchsia_bluetooth_affordances::{
    HostControllerProxy, HostControllerStartPairingDelegateRequest,
};
use fidl_fuchsia_bluetooth_sys::{InputCapability, OutputCapability};
use target_holders::toolbox;

#[derive(FfxTool)]
#[check(AvailabilityFlag("bluetooth.enabled"))]
pub struct PairableTool {
    #[command]
    cmd: PairableCommand,
    #[with(toolbox())]
    host_controller: HostControllerProxy,
}

fho::embedded_plugin!(PairableTool);
#[async_trait(?Send)]
impl FfxMain for PairableTool {
    type Writer = SimpleWriter;
    async fn main(mut self, mut writer: Self::Writer) -> Result<()> {
        match self.cmd.subcommand.clone() {
            // ffx bluetooth pairable once
            PairableSubCommand::Once(ref cmd) => {
                self.allow_pairing(&cmd.input_capability, &cmd.output_capability).await?;
                writer.line("Allowing pairing")?;
            }
            // ffx bluetooth pairable stop
            PairableSubCommand::Stop(ref _cmd) => {
                self.disable_pairing().await?;
                writer.line("Disabling pairing")?;
            }
        }
        Ok(())
    }
}

impl PairableTool {
    // Enable pairing for this device to allow incoming pairing requests.
    async fn allow_pairing(
        &self,
        input_cap: &InputCapability,
        output_cap: &OutputCapability,
    ) -> Result<()> {
        let request = HostControllerStartPairingDelegateRequest {
            input_capability: Some(*input_cap),
            output_capability: Some(*output_cap),
            ..Default::default()
        };
        Ok(self
            .host_controller
            .start_pairing_delegate(&request)
            .await
            .map_err(|err| fho::Error::Unexpected(anyhow::anyhow!("FIDL error: {err}")))?
            .map_err(|err| {
                fho::Error::Unexpected(anyhow::anyhow!(
                    "fuchsia.bluetooth.affordances.HostController error: {err:?}"
                ))
            })?)
    }

    // Disable pairing for this device to reject incoming pairing requests.
    async fn disable_pairing(&self) -> Result<()> {
        Ok(self
            .host_controller
            .stop_pairing_delegate()
            .await
            .map_err(|err| fho::Error::Unexpected(anyhow::anyhow!("FIDL error: {err}")))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffx_bluetooth_pairable_args::{parse_input_capability, parse_output_capability};

    #[test]
    fn test_parse_input_capability() {
        let cases = vec![
            ("NONE", Ok(InputCapability::None)),
            ("Confirmation", Ok(InputCapability::Confirmation)),
            ("keyboard", Ok(InputCapability::Keyboard)),
            (
                "TEST",
                Err("input capability should be 'none', 'confirmation', or 'keyboard'".to_string()),
            ),
        ];
        for (input_str, expected) in cases {
            assert_eq!(parse_input_capability(input_str), expected);
        }
    }

    #[test]
    fn test_parse_output_capability() {
        let cases = vec![
            ("NONE", Ok(OutputCapability::None)),
            ("display", Ok(OutputCapability::Display)),
            ("TEST", Err("output capability should be 'none' or 'display'".to_string())),
        ];
        for (output_str, expected) in cases {
            assert_eq!(parse_output_capability(output_str), expected);
        }
    }
}
