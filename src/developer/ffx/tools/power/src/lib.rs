// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use anyhow::{Context, Result};
use ffx_writer::SimpleWriter;
use fho::{FfxMain, FfxTool};
use fidl::endpoints::DiscoverableProtocolMarker;
use target_holders::RemoteControlProxyHolder;
use {
    fidl_fuchsia_power as fpower, fidl_fuchsia_power_manager_debug as fdebug,
    fidl_fuchsia_power_topology_test as fpt, fidl_fuchsia_sys2 as fsys,
};

mod args;

struct Connector {
    remote_control: fho::Result<RemoteControlProxyHolder>,
}

impl Connector {
    fn new(remote_control: fho::Result<RemoteControlProxyHolder>) -> Self {
        Self { remote_control }
    }

    async fn get_capability<S: DiscoverableProtocolMarker>(
        &self,
        moniker: &str,
    ) -> Result<S::Proxy> {
        let Ok(ref remote_control) = self.remote_control else {
            anyhow::bail!("{}", self.remote_control.as_ref().unwrap_err());
        };
        // Try to connect via fuchsia.developer.remotecontrol/RemoteControl.ConnectCapability.
        let (proxy, server) = fidl::endpoints::create_proxy::<S>();
        remote_control
            .connect_capability(
                moniker,
                fsys::OpenDirType::ExposedDir,
                S::PROTOCOL_NAME,
                server.into_channel(),
            )
            .await?
            .map_err(|e| {
                anyhow::anyhow!(
                    "failed to connect to {} at {} as {}: {:?}",
                    S::DEBUG_NAME,
                    moniker,
                    S::PROTOCOL_NAME,
                    e
                )
            })?;
        Ok(proxy)
    }
}

impl powercli::connector::Connector for Connector {
    async fn get_system_activity_control(&self) -> Result<fpt::SystemActivityControlProxy> {
        self.get_capability::<fpt::SystemActivityControlMarker>(
            "/core/system-activity-governor-controller",
        )
        .await
        .context("Failed to connect to system activity control service")
    }
    async fn get_debug(&self) -> Result<fdebug::DebugProxy> {
        self.get_capability::<fdebug::DebugMarker>("/bootstrap/power_manager")
            .await
            .context("Failed to connect to power manager debug service")
    }
    async fn get_reboot_initiator(&self) -> Result<fpower::CollaborativeRebootInitiatorProxy> {
        self.get_capability::<fpower::CollaborativeRebootInitiatorMarker>(
            "/bootstrap/shutdown_shim",
        )
        .await
        .context("Failed to connect to collaborative reboot initiator service")
    }
}

#[derive(FfxTool)]
pub struct PowerTool {
    remote_control: fho::Result<RemoteControlProxyHolder>,
    #[command]
    cmd: args::PowerCommand,
}

#[async_trait::async_trait(?Send)]
impl FfxMain for PowerTool {
    type Writer = SimpleWriter;

    async fn main(self, mut writer: Self::Writer) -> fho::Result<()> {
        powercli::power(self.cmd.into(), Connector::new(self.remote_control), &mut writer)
            .await
            .map_err(Into::into)
    }
}
