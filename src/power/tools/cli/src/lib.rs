// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod args;
pub mod connector;
mod system_activity;

use anyhow::{Context, Result};
use args::{PowerCommand, PowerSubCommand};
use connector::Connector;
use std::io;

pub async fn power(
    cmd: PowerCommand,
    connector: impl Connector,
    writer: &mut dyn io::Write,
) -> Result<()> {
    match cmd.subcommand {
        PowerSubCommand::SystemActivity(subcmd) => {
            let system_activity_control = connector
                .get_system_activity_control()
                .await
                .context("Failed to get system_activity_control")?;
            system_activity::system_activity(subcmd, writer, system_activity_control)
                .await
                .context("system-activity subcommand failed")?;
        }
    };
    Ok(())
}
