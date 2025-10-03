// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
pub mod args;

use anyhow::Result;
use args::{ApplicationActivityCommand, ApplicationActivitySubcommand};
use fidl_fuchsia_power_topology_test as fpt;
use std::io::Write;

pub async fn application_activity(
    cmd: ApplicationActivityCommand,
    _writer: &mut dyn Write,
    system_activity_control: fpt::SystemActivityControlProxy,
) -> Result<()> {
    match cmd.subcommand {
        ApplicationActivitySubcommand::Start(_) => {
            start(system_activity_control).await?;
        }
        ApplicationActivitySubcommand::Stop(_) => stop(system_activity_control).await?,
        ApplicationActivitySubcommand::Restart(command) => {
            restart(system_activity_control, command.wait_time).await?
        }
    };
    Ok(())
}

pub async fn start(system_activity_control: fpt::SystemActivityControlProxy) -> Result<()> {
    let _ = system_activity_control.start_application_activity().await?;
    Ok(())
}

pub async fn stop(system_activity_control: fpt::SystemActivityControlProxy) -> Result<()> {
    let _ = system_activity_control.stop_application_activity().await?;
    Ok(())
}

pub async fn restart(
    system_activity_control: fpt::SystemActivityControlProxy,
    wait_time: std::time::Duration,
) -> Result<()> {
    let _ =
        system_activity_control.restart_application_activity(wait_time.as_nanos() as u64).await?;
    Ok(())
}
