// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::Resolution;
use crate::target_connector::{
    BUFFER_SIZE, FDomainConnection, OvernetConnection, TargetConnection, TargetConnectionError,
    TargetConnector,
};
use anyhow::Result;
use ffx_command_error::FfxContext as _;
use ffx_config::{EnvironmentContext, TryFromEnvContext};
use futures::future::LocalBoxFuture;
use std::fmt::Debug;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::BufReader;

const OVERNET_VSOCK_PORT: u32 = 202;
const FDOMAIN_VSOCK_PORT: u32 = 203;

const CONFIG_START_DRIVER: &str = "connectivity.usb_driver_autostart";

pub struct UsbConnector {
    driver: usb_driver_api::Driver,
    cid: u32,
}

impl Debug for UsbConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsbConnector").field("cid", &self.cid).finish()
    }
}

impl UsbConnector {
    pub async fn new(cid: u32, env_context: &EnvironmentContext) -> Result<Self> {
        let socket_path: Option<PathBuf> =
            env_context.get(usb_driver_api::CONFIG_USB_SOCKET_PATH)?;

        let socket_path = if let Some(socket_path) = socket_path {
            socket_path
        } else {
            usb_driver_api::default_usb_socket_path()?
        };

        try_daemon_autostart(&socket_path, env_context);

        let driver = usb_driver_api::Driver::init(socket_path).await?;
        Ok(Self { driver, cid })
    }
}

impl UsbConnector {
    async fn connect_overnet(&mut self) -> Result<OvernetConnection, TargetConnectionError> {
        let conn = self.driver.connect(self.cid, OVERNET_VSOCK_PORT).await.map_err(|e| {
            TargetConnectionError::Fatal(anyhow::anyhow!("Connection error: {e:?}"))
        })?;
        let (output, input) = conn.into_split();
        let output = BufReader::with_capacity(BUFFER_SIZE, output);
        let (_sender, errors) = async_channel::unbounded();
        Ok(OvernetConnection {
            output: Box::new(output),
            input: Box::new(input),
            errors,
            compat: None,
            main_task: None,
            ssh_host_address: None,
        })
    }

    async fn connect_fdomain(&mut self) -> Result<FDomainConnection, TargetConnectionError> {
        let conn = self.driver.connect(self.cid, FDOMAIN_VSOCK_PORT).await.map_err(|e| {
            TargetConnectionError::Fatal(anyhow::anyhow!("Connection error: {e:?}"))
        })?;
        let (output, input) = conn.into_split();
        let output = BufReader::with_capacity(BUFFER_SIZE, output);
        let (_sender, errors) = async_channel::unbounded();
        Ok(FDomainConnection {
            output: Box::new(output),
            input: Box::new(input),
            errors,
            main_task: None,
        })
    }
}

impl TryFromEnvContext for UsbConnector {
    fn try_from_env_context<'a>(
        env: &'a EnvironmentContext,
    ) -> LocalBoxFuture<'a, ffx_command_error::Result<Self>> {
        Box::pin(async {
            let resolution = Resolution::try_from_env_context(env).await?;
            let cid = resolution.usb_cid().ok_or_else(|| {
                ffx_command_error::user_error!(
                    "query did not resolve a USB CID. Resolved the following: {:?}",
                    resolution,
                )
            })?;
            UsbConnector::new(cid, env).await.bug().map_err(Into::into)
        })
    }
}

impl TargetConnector for UsbConnector {
    const CONNECTION_TYPE: &'static str = "USB VSOCK";

    async fn connect(&mut self) -> Result<TargetConnection, TargetConnectionError> {
        let fdomain = match self.connect_fdomain().await {
            Ok(f) => Some(f),
            Err(e) => {
                // Eventually we should just return the error here, making
                // FDomain authoritative about whether the device is
                // connectable. For now we'll fall through because it's less
                // likely to cause breakages prior to migration.
                log::warn!("Connecting with FDomain encountered error {e:?}");
                None
            }
        };
        let overnet = self.connect_overnet().await;

        if let Some(fdomain) = fdomain {
            if let Some(overnet) = overnet.ok() {
                Ok(TargetConnection::Both(fdomain, overnet))
            } else {
                Ok(TargetConnection::FDomain(fdomain))
            }
        } else {
            overnet.map(TargetConnection::Overnet)
        }
    }
}

/// Try to auto-start the daemon if it is appropriate to do so.
pub fn try_daemon_autostart(path: &PathBuf, context: &EnvironmentContext) {
    if context.is_strict() || context.is_isolated() {
        return;
    }

    if !context.get(CONFIG_START_DRIVER).unwrap_or(true) {
        return;
    }

    let cmd = context.rerun_prefix();
    let mut cmd = match cmd {
        Ok(cmd) => cmd,
        Err(error) => {
            log::warn!(error:?; "Could not get rerun prefix to spawn USB driver");
            return;
        }
    };
    let socket_path_config =
        format!("{}={}", usb_driver_api::CONFIG_USB_SOCKET_PATH, path.to_string_lossy());
    let child = cmd
        .args(["-c", socket_path_config.as_str(), "usb-driver", "--background"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let child = match child {
        Ok(child) => child,
        Err(error) => {
            log::warn!(error:?; "Could not spawn USB driver process");
            return;
        }
    };

    let output = match child.wait_with_output() {
        Ok(output) => output,
        Err(error) => {
            log::warn!(error:?; "Error waiting for USB driver to start");
            return;
        }
    };

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        log::warn!(
            exit_status:? = output.status,
            stdout = stdout.as_str(),
            stderr = stderr.as_str();
            "USB driver exited with bad status");
    }
}
