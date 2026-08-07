// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use async_trait::async_trait;
use errors::FfxError;
use ffx_command_error::{Error, Result};
use fho::{FhoEnvironment, TryFromEnv};
use fidl::endpoints::DiscoverableProtocolMarker;
use fidl_fuchsia_developer_ffx as ffx_fidl;
use std::time::Duration;
use target_behavior::{
    ConnectionBehavior, DirectConnector, FhoTargetEnvironment, target_interface,
};
use target_holders::DaemonProxyHolder;

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
    const OPEN_TARGET_TIMEOUT: Duration = Duration::from_millis(500);
    const KNOCK_TARGET_TIMEOUT: Duration = ffx_target::DEFAULT_RCS_KNOCK_TIMEOUT;

    /// Try to get a `T` from the environment. Will wait for the target to
    /// appear if it is non-responsive. If that occurs, `log_target_wait` will
    /// be called prior to waiting.
    pub async fn try_connect(
        &self,
        mut log_target_wait: impl FnMut(&Option<String>, &Option<Error>) -> Result<()>,
    ) -> Result<T> {
        let behavior =
            self.target_env.init_connection_behavior(self.env.environment_context()).await?;
        match *behavior {
            ConnectionBehavior::DaemonConnector(_) => {
                daemon_try_connect(
                    &self.env,
                    &mut log_target_wait,
                    Self::OPEN_TARGET_TIMEOUT,
                    Self::KNOCK_TARGET_TIMEOUT,
                )
                .await
            }
            ConnectionBehavior::DirectConnector(ref dc) => {
                direct_connector_try_connect::<T>(&self.env, dc, &mut log_target_wait).await
            }
        }
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

async fn knock_rcs(
    target: &Option<String>,
    tc_proxy: &ffx_fidl::TargetCollectionProxy,
    open_target_timeout: Duration,
    knock_target_timeout: Duration,
) -> Result<()> {
    loop {
        match ffx_target::knock_target_by_name(
            target,
            tc_proxy,
            open_target_timeout,
            knock_target_timeout,
        )
        .await
        {
            Ok(()) => break,
            Err(ffx_target::KnockError::Critical(e)) => {
                return Err(ffx_command_error::Error::Unexpected(anyhow::Error::new(e)));
            }
            Err(ffx_target::KnockError::NonCritical(_)) => {
                // Should we log the error? It'll spam like hell.
            }
        };
    }
    Ok(())
}

async fn daemon_try_connect<T>(
    env: &FhoEnvironment,
    log_target_wait: &mut impl FnMut(&Option<String>, &Option<Error>) -> Result<()>,
    open_target_timeout: Duration,
    knock_target_timeout: Duration,
) -> Result<T>
where
    T: TryFromEnv<Error = ffx_command_error::Error>,
{
    loop {
        return match T::try_from_env(env).await {
            Err(ffx_command_error::Error::User(e)) => {
                match e.downcast::<target_errors::FfxTargetError>() {
                    Ok(target_errors::FfxTargetError::DaemonError {
                        err: ffx_fidl::DaemonError::Timeout,
                        target,
                        ..
                    }) => {
                        let Ok(daemon_proxy) = DaemonProxyHolder::try_from_env(env).await else {
                            // Let the initial try_from_env detect this error.
                            continue;
                        };
                        let (tc_proxy, server_end) =
                            fidl::endpoints::create_proxy::<ffx_fidl::TargetCollectionMarker>();
                        let Ok(Ok(())) = daemon_proxy
                            .connect_to_protocol(
                                ffx_fidl::TargetCollectionMarker::PROTOCOL_NAME,
                                server_end.into_channel(),
                            )
                            .await
                        else {
                            // Let the rcs_proxy_connector detect this error too.
                            continue;
                        };
                        log_target_wait(&target, &None)?;
                        // The daemon version of this check uses a "knock" against RCS, which is
                        // essentially: keep a channel open to RCS for about a second, and if no
                        // error events come in on the channel during that time, we consider it
                        // "safe." This isn't something strictly necessary (and is not being used
                        // in the daemonless version). This was implemented when reliability with
                        // overnet was pretty spotty (when it was primarily a mesh network), and
                        // was a means to determine if a connection was "real" or if it was
                        // something stale.
                        //
                        // For non-daemon connections this isn't necessary, and we
                        // can operate under the assumption that if we have connected to an
                        // instance of an RCS proxy, we are therefore able to use it.t
                        knock_rcs(&target, &tc_proxy, open_target_timeout, knock_target_timeout)
                            .await?;
                        continue;
                    }
                    Ok(other) => return Err(Into::<FfxError>::into(other).into()),
                    Err(e) => return Err(e.into()),
                }
            }
            other => other,
        };
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
