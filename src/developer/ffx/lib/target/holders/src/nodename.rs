// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use async_trait::async_trait;
use std::ops::Deref;
use target_behavior::target_interface;

use fho::{FhoEnvironment, TryFromEnv, bug};

/// Holder struct for the target's Nodename.
#[derive(Debug, Clone)]
pub struct NodenameHolder(Option<String>);

#[async_trait(?Send)]
impl TryFromEnv for NodenameHolder {
    type Error = ffx_command_error::Error;
    async fn try_from_env(env: &FhoEnvironment) -> std::result::Result<Self, Self::Error> {
        let target_env = target_interface(env);
        let behavior = target_env.init_connection_behavior(env.environment_context()).await?;
        let identity = match &*behavior {
            target_behavior::ConnectionBehavior::DaemonConnector(injector) => {
                let target = injector.target_factory().await?;
                target.identity().await.map_err(|e| bug!(e))?.nodename
            }
            target_behavior::ConnectionBehavior::DirectConnector(conn) => {
                conn.resolution()
                    .await
                    .map_err(|e| e.into_command_error())?
                    .identify(&env.environment_context())
                    .await
                    .map_err(|e| e.into_command_error())?
                    .nodename
            }
        };
        Ok(Self(identity))
    }
}

impl Deref for NodenameHolder {
    type Target = Option<String>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
