// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod args;

mod application_activity;

use anyhow::{Context, Result};
use args::{SystemActivityCommand, SystemActivitySubcommand};
use fidl_fuchsia_power_topology_test as fpt;
use std::io::Write;

pub async fn system_activity(
    cmd: SystemActivityCommand,
    writer: &mut dyn Write,
    system_activity_control: fpt::SystemActivityControlProxy,
) -> Result<()> {
    match cmd.subcommand {
        SystemActivitySubcommand::ApplicationActivity(subcmd) => {
            application_activity::application_activity(subcmd, writer, system_activity_control)
                .await
                .context("application-activity subcommand failed")?;
        }
    };
    Ok(())
}
