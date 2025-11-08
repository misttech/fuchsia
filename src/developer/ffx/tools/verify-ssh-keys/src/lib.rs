// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use argh::{ArgsInfo, FromArgs};
use async_trait::async_trait;
use ffx_config::EnvironmentContext;
use ffx_ssh::keys::SshKey;
use ffx_writer::{MachineWriter, ToolIO as _};
use fho::{FfxContext, FfxMain, FfxTool, Result};
use std::io::Write;
use target_holders::SshAddrHolder;

#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "verify-ssh-keys", description = "TODO")]
pub struct VerifyCommand {}

#[derive(FfxTool)]
pub struct VerifyTool {
    #[command]
    _cmd: VerifyCommand,
    env: EnvironmentContext,
    device_addr: SshAddrHolder,
}

#[async_trait(?Send)]
impl FfxMain for VerifyTool {
    type Writer = MachineWriter<SshKey>;

    async fn main(self, mut writer: Self::Writer) -> Result<()> {
        let found_keys =
            ffx_ssh::keys::find_matching_ssh_keys(&self.env, *self.device_addr).await?;
        if !writer.is_machine() {
            writeln!(&mut writer, "Found the following public keys matching your device:\n\n")
                .bug()?;
        }
        for key in found_keys.into_iter() {
            writer.item(&key)?;
        }
        if !writer.is_machine() {
            writeln!(
                &mut writer,
                "\n\nIf you are still not able to connect to your device, you may need to move one of your public/private key pairs to a different directory.
Please consult https://fuchsia.dev/fuchsia-src/development/tools/ffx/workflows/create-ssh-keys-for-devices for more details")
                .bug()?;
        }
        Ok(())
    }
}
