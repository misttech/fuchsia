// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
mod args;
use args::{MonitorCommand, SubCommand};

use async_trait::async_trait;
use fho::{FfxMain, FfxTool, Result, bug};
use std::io::Write;

#[derive(FfxTool)]
pub struct MonitorTool {
    #[command]
    cmd: MonitorCommand,
}

#[async_trait(?Send)]
impl FfxMain for MonitorTool {
    type Writer = ffx_writer::SimpleWriter;
    async fn main(self, mut writer: Self::Writer) -> Result<()> {
        match self.cmd.subcommand {
            SubCommand::Start(start_cmd) => {
                writeln!(writer, "Starting monitor server on port {}", start_cmd.port)
                    .map_err(|e| bug!(e))?;
                Ok(())
            }
            SubCommand::Stop(_stop_cmd) => {
                writeln!(writer, "Stopping monitor server.").map_err(|e| bug!(e))?;
                Ok(())
            }
        }
    }
}
