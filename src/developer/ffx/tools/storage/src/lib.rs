// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use argh::{ArgsInfo, FromArgs};
use ffx_writer::SimpleWriter;
use fho::{FfxMain, FfxTool, Result, deferred};
use target_holders::moniker;

mod fxfs;

#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(subcommand)]
pub enum StorageSubCommand {
    Fxfs(fxfs::FxfsCommand),
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "storage", description = "Manage Fuchsia Filesystems.")]
pub struct StorageCommand {
    #[argh(subcommand)]
    subcommand: StorageSubCommand,
}

#[derive(FfxTool)]
pub struct StorageTool {
    #[command]
    cmd: StorageCommand,

    #[with(deferred(moniker("/bootstrap/fshost/fxfs")))]
    debug_proxy: fho::Deferred<fdomain_fuchsia_fxfs::DebugProxy>,

    #[with(deferred(moniker("/bootstrap/fshost/fxfs")))]
    profile_proxy: fho::Deferred<fdomain_fuchsia_fxfs::ProfileManagerProxy>,
}

impl FfxMain for StorageTool {
    type Writer = SimpleWriter;

    type Error = ::fho::Error;

    async fn main(self, writer: Self::Writer) -> Result<()> {
        match self.cmd.subcommand {
            StorageSubCommand::Fxfs(cmd) => {
                fxfs::handle_cmd(cmd, writer, self.debug_proxy, self.profile_proxy).await
            }
        }
    }
}
