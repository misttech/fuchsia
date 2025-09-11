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
use tokio::io::BufReader;

const OVERNET_VSOCK_PORT: u32 = 202;
const FDOMAIN_VSOCK_PORT: u32 = 203;

pub struct UsbConnector {
    driver: usb_driver_api::Driver,
    cid: u32,
    env_context: EnvironmentContext,
}

impl Debug for UsbConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsbConnector")
            .field("cid", &self.cid)
            .field("env_context", &self.env_context)
            .finish()
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

        let driver = usb_driver_api::Driver::init(socket_path).await?;
        Ok(Self { driver, cid, env_context: env_context.clone() })
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
                    "query did not resolve an IP address. Resolved the following: {:?}",
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
