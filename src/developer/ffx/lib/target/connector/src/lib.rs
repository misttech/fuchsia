// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use async_trait::async_trait;
use ffx_command_error::{Error, Result};
use fho::{FhoEnvironment, TryFromEnv};
use target_behavior::{
    ConnectionBehavior, DirectConnector, FhoTargetEnvironment, target_interface,
};

/// A connector lets a tool make multiple attempts to connect to an object. It
/// retains the environment in the tool body to allow this.
#[derive(Clone)]
pub struct Connector<T: TryFromEnv> {
    env: FhoEnvironment,
    target_env: FhoTargetEnvironment,
    _connects_to: std::marker::PhantomData<T>,
}

impl<T> Connector<T>
where
    T: TryFromEnv<Error = ffx_command_error::Error>,
{
    /// Try to get a `T` from the environment. Will wait for the target to
    /// appear if it is non-responsive. If that occurs, `log_target_wait` will
    /// be called prior to waiting.
    pub async fn try_connect(
        &self,
        mut log_target_wait: impl FnMut(&Option<String>, &Option<Error>) -> Result<()>,
    ) -> Result<T> {
        let behavior =
            self.target_env.init_connection_behavior(self.env.environment_context()).await?;
        // TODO(b/540442481): Clean up naming (e.g. "Direct", "DirectConnector") in a follow-up CL now that direct connection is the only mechanism.
        let ConnectionBehavior::Direct(dc) = &*behavior;
        direct_connector_try_connect::<T>(&self.env, dc, &mut log_target_wait).await
    }
}

#[async_trait(?Send)]
impl<T> TryFromEnv for Connector<T>
where
    T: TryFromEnv<Error = ffx_command_error::Error>,
{
    type Error = ffx_command_error::Error;
    async fn try_from_env(env: &FhoEnvironment) -> Result<Self, Self::Error> {
        let target_env = target_interface(env);
        Ok(Connector { env: env.clone(), target_env, _connects_to: Default::default() })
    }
}

async fn direct_connector_try_connect<T>(
    env: &FhoEnvironment,
    dc: &DirectConnector,
    log_target_wait: &mut impl FnMut(&Option<String>, &Option<Error>) -> Result<()>,
) -> Result<T>
where
    T: TryFromEnv<Error = ffx_command_error::Error>,
{
    loop {
        let target_spec = {
            let resolution = dc.resolution().await.map_err(|e| e.into_command_error())?;
            let _ = resolution
                .get_connection(env.environment_context())
                .await
                .map_err(|e| e.into_command_error())?;
            resolution.target_spec()
        };
        return match T::try_from_env(env).await {
            Err(conn_error) => {
                let e = conn_error.downcast_non_fatal()?;
                log::debug!("error when trying to connect using TryFromEnv: {e}");
                log_target_wait(&Some(target_spec), &Some(Error::User(e)))?;
                continue;
            }
            Ok(res) => Ok(res),
        };
    }
}
