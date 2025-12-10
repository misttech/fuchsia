// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use addr::{self, TargetIpAddr};
use async_trait::async_trait;
use std::ops::Deref;
use target_behavior::target_interface;

use ffx_command_error::Result;
use fho::{FhoEnvironment, TryFromEnv, bug};
use std::net::SocketAddr;

/// Holder struct for the target's SshAddr.
#[derive(Debug, Clone)]
pub struct SshAddrHolder(SocketAddr);

#[async_trait(?Send)]
impl TryFromEnv for SshAddrHolder {
    async fn try_from_env(env: &FhoEnvironment) -> Result<Self> {
        let target_env = target_interface(env);
        let behavior = target_env.init_connection_behavior(env.environment_context()).await?;
        Ok(SshAddrHolder(match &*behavior {
            target_behavior::ConnectionBehavior::DaemonConnector(injector) => {
                let target = injector.target_factory().await?;
                let tiai = target.get_ssh_address().await.map_err(|e| bug!(e))?;
                let tia: TargetIpAddr = tiai.into();
                tia.into()
            }
            target_behavior::ConnectionBehavior::DirectConnector(connector) => {
                connector.resolution().await?.addr()?
            }
        }))
    }
}

impl Deref for SshAddrHolder {
    type Target = SocketAddr;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
