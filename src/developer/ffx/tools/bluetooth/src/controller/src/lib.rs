// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use ::async_trait::async_trait;
use ::fho::{AvailabilityFlag, FfxMain, FfxTool, Result};
use ffx_writer::{SimpleWriter, ToolIO as _};
use fidl_fuchsia_bluetooth_affordances::HostControllerProxy;
use target_holders::toolbox;

use ffx_bluetooth_controller_args::{ControllerCommand, ControllerSubCommand};

use fuchsia_bluetooth::types::{HostInfo, addresses_to_custom_string};
use prettytable::format::FormatBuilder;
use prettytable::{Table, cell, row};

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
        let hosts = self.get_hosts().await?;
        match self.cmd.subcommand {
            // ffx bluetooth controller show
            ControllerSubCommand::Show(ref _cmd) => {
                if let Some(host) = hosts.first() {
                    writer.line(host.to_string())?;
                } else {
                    writer.line("No host found.")?;
                }
            }
            // ffx bluetooth controller list
            ControllerSubCommand::List(ref _cmd) => {
                writer.line(get_hosts_list(&hosts).unwrap())?;
            }
        }
        Ok(())
    }
}

impl ControllerTool {
    async fn get_hosts(&self) -> Result<Vec<HostInfo>> {
        Ok(self
            .host_controller
            .get_hosts()
            .await
            .map_err(|err| fho::Error::Unexpected(anyhow::anyhow!("FIDL error: {err}")))?
            .map_err(|err| {
                fho::Error::Unexpected(anyhow::anyhow!(
                    "fuchsia.bluetooth.affordances.HostController error: {err:?}"
                ))
            })?
            .iter()
            .map(|host| {
                HostInfo::try_from(host.clone()).expect("Failed to convert between Host types")
            })
            .collect())
    }
}

fn get_hosts_list(hosts: &Vec<HostInfo>) -> Result<String> {
    if hosts.is_empty() {
        return Ok(String::from("No controllers detected"));
    }
    // Create table of results
    let mut table = Table::new();
    let table_format = FormatBuilder::new().padding(/*left*/ 0, /*right*/ 1).build();
    table.set_format(table_format);
    let _ = table.set_titles(row![
        "HostId",
        "Addresses",
        "Active",
        "Technology",
        "Name",
        "Discoverable",
        "Discovering",
    ]);
    for host in hosts {
        let _ = table.add_row(row![
            host.id.to_string(),
            addresses_to_custom_string(&host.addresses, "\n"),
            host.active.to_string(),
            format!("{:?}", host.technology),
            host.local_name.clone().unwrap_or_else(|| "(unknown)".to_string()),
            host.discoverable.to_string(),
            host.discovering.to_string(),
        ]);
    }
    Ok(format!("{}", table))
}
