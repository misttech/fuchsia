// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::ops::Deref;

use crate::{init_connection_behavior, TargetProxyHolder};
use anyhow::anyhow;
use async_trait::async_trait;
use ffx_command_error::Result;
use ffx_ssh::parse::HostAddr;
use ffx_target::fho::{target_interface, FhoConnectionBehavior};
use fho::{FhoEnvironment, TryFromEnv};
use fidl_fuchsia_developer_ffx as ffx_fidl;

#[derive(Clone, Debug)]
pub struct HostAddrHolder(Option<HostAddr>);

impl Deref for HostAddrHolder {
    type Target = Option<HostAddr>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<HostAddr> for HostAddrHolder {
    fn from(value: HostAddr) -> Self {
        HostAddrHolder::from(Some(value))
    }
}

impl From<Option<HostAddr>> for HostAddrHolder {
    fn from(value: Option<HostAddr>) -> Self {
        HostAddrHolder(value)
    }
}

impl From<HostAddrHolder> for Option<HostAddr> {
    fn from(value: HostAddrHolder) -> Self {
        value.0
    }
}

impl From<Option<ffx_fidl::SshHostAddrInfo>> for HostAddrHolder {
    fn from(value: Option<ffx_fidl::SshHostAddrInfo>) -> Self {
        HostAddrHolder::from(value.map(|x| HostAddr::from(x.address)))
    }
}

impl From<String> for HostAddrHolder {
    fn from(value: String) -> Self {
        HostAddrHolder::from(Some(HostAddr::from(value)))
    }
}

#[async_trait(?Send)]
impl TryFromEnv for HostAddrHolder {
    async fn try_from_env(env: &FhoEnvironment) -> Result<Self> {
        let target_env = target_interface(env);
        let behavior = if let Some(behavior) = target_env.behavior() {
            behavior
        } else {
            let b = init_connection_behavior(env.environment_context()).await?;
            target_env.set_behavior(b.clone())?;
            b
        };
        match behavior {
            FhoConnectionBehavior::DaemonConnector(_) => {
                // Get a target proxy
                let tp = TargetProxyHolder::try_from_env(env).await?;
                let id = tp
                    .identity()
                    .await
                    .map_err(|e| anyhow!("Got Error getting target identity: {}", e))?;
                Ok(HostAddrHolder::from(id.ssh_host_address))
            }
            FhoConnectionBehavior::DirectConnector(direct) => {
                let conn = direct.connection().await?;
                let host_addr_info = conn.host_ssh_address();
                Ok(HostAddrHolder::from(host_addr_info))
            }
        }
    }
}
