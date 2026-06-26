// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use addr::TargetIpAddr;
use anyhow::{Context as _, Result};
use compat_info::CompatibilityInfo;
use discovery::{DiscoverySources, TargetHandle};
use ffx_config::keys::TARGET_DEFAULT_KEY;

use ffx_config::{ConfigLevel, EnvironmentContext};
use fidl::endpoints::create_proxy;
use fidl::prelude::*;
use fidl_fuchsia_developer_ffx::{
    self as ffx, DaemonError, DaemonProxy, TargetCollectionMarker, TargetCollectionProxy,
    TargetMarker, TargetQuery,
};
use fidl_fuchsia_developer_remotecontrol::{RemoteControlMarker, RemoteControlProxy};
use fidl_fuchsia_net as net;
use fuchsia_async::Timer;
use futures::future::{Either, pending};
use futures::{Future, FutureExt, TryStreamExt, select};
use log::{debug, info};
use std::net::IpAddr;
use std::time::Duration;
use target_errors::FfxTargetError;
use thiserror::Error;
use timeout::timeout;

#[cfg(test)]
use mockall::predicate::*;

pub mod analytics;
pub mod connection;
pub mod info;
pub mod list;
pub mod ssh_connector;
pub mod usb_connector;
pub mod vsock_connector;

mod cache;
mod error;
mod fdomain_transport;
mod fidl_pipe;
mod resolve;
mod target_connector;

pub use cache::{
    create_target_cache, get_discovery_cache_dir, get_discovery_cache_file,
    get_discovery_cache_recheck_time, remove_target_cache,
};
pub use connection::{Connection, ConnectionError};
pub use discovery::desc::{Description, FastbootInterface};
pub use discovery::query::TargetInfoQuery;
pub use error::FfxTargetCrateError;
pub use fidl_pipe::{FidlPipe, create_overnet_socket};
pub use info::TargetInfo;
pub use list::list_targets;
pub use resolve::{
    DefaultTargetResolver, Resolution, TargetResolver, build_discovery,
    build_discovery_from_config, discover_single_default_target, get_discovered_targets,
    get_discovery_stream, maybe_locally_resolve_target_spec, resolve_target_address,
};
pub use target_connector::{
    FDomainConnection, OvernetConnection, TargetConnection, TargetConnectionError, TargetConnector,
};

/// Re-export of [`fidl_fuchsia_developer_ffx::TargetProxy`] for ease of use
pub use fidl_fuchsia_developer_ffx::TargetProxy;

pub use target_errors::{UNKNOWN_TARGET_NAME, UNSPECIFIED_TARGET_NAME};

/// Emit an analytics event indicating an RCS proxy was created via the daemon.
pub async fn emit_daemon_rcs_proxy_event(ty: &str) {
    connection::emit_rcs_proxy_event(ty, Some(true), true).await
}

/// Attempt to connect to RemoteControl on a target device using a connection to a daemon.
///
/// The optional |target| is a string matcher as defined in fuchsia.developer.ffx.TargetQuery
/// fidl table.
pub async fn get_remote_proxy(
    target_spec: &TargetInfoQuery,
    daemon_proxy: DaemonProxy,
    proxy_timeout: Duration,
    mut target_info: Option<&mut Option<ffx::TargetInfo>>,
    context: &EnvironmentContext,
) -> std::result::Result<RemoteControlProxy, FfxTargetError> {
    let mut target_info_out = None;
    // Target connection retries utilize exponential backoff (starting at 100ms up to 5s) to
    // prevent high-frequency, CPU-intensive retry spinning during target reboots or prolonged
    // offline states. This backoff is only active/necessary for daemon-based connections;
    // direct mode connections utilize a separate flow that immediately bubbles up failures
    // without retry looping.
    let mut retry_delay = Duration::from_millis(100);
    const MAX_RETRY_DELAY: Duration = Duration::from_secs(5);
    // Track the last encountered non-fatal connection error to prevent spamming logs with
    // duplicate retry messages on every attempt.
    let mut last_error: Option<ffx::TargetConnectionError> = None;
    let res = loop {
        match get_remote_proxy_impl(
            &target_spec,
            &daemon_proxy,
            &proxy_timeout,
            &mut target_info_out,
            &context,
        )
        .await
        {
            Ok(p) => break Ok(p),
            Err(e) => match &e {
                FfxTargetError::TargetConnectionError { err, .. } => match err {
                    ffx::TargetConnectionError::KeyVerificationFailure
                    | ffx::TargetConnectionError::InvalidArgument
                    | ffx::TargetConnectionError::PermissionDenied => {
                        break Err(e.clone());
                    }
                    _ => {
                        let current_error = *err;
                        if last_error != Some(current_error) {
                            log::info!(
                                "Retrying connection after non-fatal error encountered: {e}"
                            );
                            last_error = Some(current_error);
                        }
                        fuchsia_async::Timer::new(retry_delay).await;
                        retry_delay = std::cmp::min(retry_delay * 2, MAX_RETRY_DELAY);
                        continue;
                    }
                },
                _ => {
                    break Err(e.clone());
                }
            },
        }
    };
    if let Some(ref mut info_out) = target_info {
        **info_out = target_info_out.into();
    }
    res
}

async fn get_remote_proxy_impl(
    target_spec: &TargetInfoQuery,
    daemon_proxy: &DaemonProxy,
    proxy_timeout: &Duration,
    target_info: &mut Option<ffx::TargetInfo>,
    context: &EnvironmentContext,
) -> std::result::Result<RemoteControlProxy, FfxTargetError> {
    // See if we need to do local resolution. (Do it here not in
    // open_target_with_fut because o_t_w_f is not async)
    let target_spec = resolve::maybe_locally_resolve_target_spec(target_spec, context)
        .await
        .map_err(|_| FfxTargetError::OpenTargetError {
            err: ffx::OpenTargetError::FailedDiscovery,
            target: target_spec.clone().into(),
            targets: vec![],
        })?;
    let tsc = target_spec.clone();
    let (target_proxy, target_proxy_fut) =
        open_target_with_fut(&tsc, daemon_proxy.clone(), *proxy_timeout)?;
    let mut target_proxy_fut = target_proxy_fut.boxed_local().fuse();
    let (remote_proxy, remote_server_end) = create_proxy::<RemoteControlMarker>();
    let mut open_remote_control_fut =
        target_proxy.open_remote_control(remote_server_end).boxed_local().fuse();
    let res = loop {
        select! {
            res = open_remote_control_fut => {
                match res {
                    Err(_) => {
                        // Getting here is most likely the result of a PEER_CLOSED error, which
                        // may be because the target_proxy closure has propagated faster than
                        // the error (which can happen occasionally). To counter this, wait for
                        // the target proxy to complete, as it will likely only need to be
                        // polled once more (open_remote_control_fut partially depends on it).
                        let _ = target_proxy_fut.await;
                        return Err(FfxTargetError::DaemonError {
                            err: DaemonError::ProtocolOpenError,
                            target: target_spec.clone().into(),
                        });
                    }
                    Ok(r) => break r,
                }
            }
            res = target_proxy_fut => res?,
        }
    };
    let info =
        target_proxy.identity().await.map_err(|e| FfxTargetError::DaemonCommunicationError {
            target: target_spec.clone().into(),
            error: std::sync::Arc::new(e),
        })?;
    *target_info = Some(info.into());
    match res {
        Ok(_) => Ok(remote_proxy),
        Err(err) => Err(FfxTargetError::TargetConnectionError {
            err,
            target: target_spec.into(),
            logs: Some(
                target_proxy
                    .get_ssh_logs()
                    .await
                    .unwrap_or_else(|_| "failed to get logs".to_string()),
            ),
        }),
    }
}

/// Attempt to connect to a target given a connection to a daemon.
///
/// The returned future must be polled to completion. It is returned separately
/// from the TargetProxy to enable immediately pushing requests onto the TargetProxy
/// before connecting to the target completes.
///
/// The optional |target| is a string matcher as defined in fuchsia.developer.ffx.TargetQuery
/// fidl table.
pub fn open_target_with_fut<'a, 'b: 'a>(
    target: &'a TargetInfoQuery,
    daemon_proxy: DaemonProxy,
    target_timeout: Duration,
) -> std::result::Result<
    (TargetProxy, impl Future<Output = std::result::Result<(), FfxTargetError>> + 'a),
    FfxTargetError,
> {
    let (tc_proxy, tc_server_end) = create_proxy::<TargetCollectionMarker>();
    let (target_proxy, target_server_end) = create_proxy::<TargetMarker>();
    let target_collection_fut = async move {
        daemon_proxy
            .connect_to_protocol(
                TargetCollectionMarker::PROTOCOL_NAME,
                tc_server_end.into_channel(),
            )
            .await
            .map_err(|_| FfxTargetError::DaemonError {
                err: DaemonError::ProtocolOpenError,
                target: target.clone().into(),
            })?
            .map_err(|err| FfxTargetError::DaemonError {
                err: err.into(),
                target: target.clone().into(),
            })?;
        Ok(())
    };
    let target_handle_fut = async move {
        timeout(
            target_timeout,
            tc_proxy.open_target(
                &TargetQuery { string_matcher: target.clone().into(), ..Default::default() },
                target_server_end,
            ),
        )
        .await
        .map_err(|_| FfxTargetError::DaemonError {
            err: DaemonError::Timeout,
            target: target.clone().into(),
        })?
        .map_err(|_| FfxTargetError::DaemonError {
            err: DaemonError::ProtocolOpenError,
            target: target.clone().into(),
        })?
        .map_err(|err| FfxTargetError::OpenTargetError {
            err,
            target: target.clone().into(),
            targets: vec![],
        })?;
        Ok(())
    };
    let fut = async move {
        let ((), ()) = futures::try_join!(target_collection_fut, target_handle_fut)?;
        Ok(())
    };

    Ok((target_proxy, fut))
}

pub fn is_discovery_enabled(ctx: &EnvironmentContext) -> bool {
    // TODO (b/355292969): put back the discovery check after we've addressed the flakes associated
    // with client-side discovery. (Currently re-enabled, but I want to validate the flake before resolving
    // this bug -slgrady 8/7/24)
    // true
    !ffx_config::is_usb_discovery_disabled(ctx) || !ffx_config::is_mdns_discovery_disabled(ctx)
}

#[derive(Debug, Error)]
pub enum KnockError {
    #[error("critical error: {0}")]
    Critical(KnockCriticalError),
    #[error("non-critical error: {0}")]
    NonCritical(KnockNonCriticalError),
}

#[derive(Debug, Error)]
pub enum KnockCriticalError {
    #[error("Timeout opening target {target}")]
    TimeoutOpeningTarget { target: String },
    #[error("Lost connection to the Daemon: {detail}")]
    LostDaemonConnection { detail: String },
    #[error("FIDL error: {0}")]
    Fidl(String),
    #[error("Target error: {0}")]
    TargetError(String),
    #[error("Other critical error: {0}")]
    Custom(String),
}

#[derive(Debug, Error)]
pub enum KnockNonCriticalError {
    #[error("Target not found: {target}")]
    TargetNotFound { target: String },
    #[error("RCS knock failed: {detail}")]
    RcsKnockFailed { detail: String },
    #[error("Timeout: {detail}")]
    Timeout { detail: String },
    #[error("Other non-critical error: {0}")]
    Custom(String),
}

// Derive from rcs knock timeout as this is the minimum amount of time to knock.
// Uses nanos to ensure that if RCS_KNOCK_TIMEOUT changes it is using the smallest unit possible.
//
// This is written as such due to some inconsistencies with Duration::from_nanos where `as_nanos()`
// returns a u128 but `from_nanos()` takes a u64.
pub const DEFAULT_RCS_KNOCK_TIMEOUT: Duration =
    Duration::new(rcs::RCS_KNOCK_TIMEOUT.as_secs() * 3, rcs::RCS_KNOCK_TIMEOUT.subsec_nanos() * 3);

impl From<ConnectionError> for KnockError {
    fn from(e: ConnectionError) -> Self {
        match e {
            ConnectionError::KnockError(ke) => ke,
            other => KnockError::Critical(KnockCriticalError::Custom(format!("{:?}", other))),
        }
    }
}

/// Attempts to "knock" a target to determine if it is up and connectable via RCS.
///
/// This is intended to be run in a loop, with a non-critical error implying the caller
/// should call again, and a critical error implying the caller should raise the error
/// and no longer loop.
pub async fn knock_target(target: &TargetProxy) -> Result<(), KnockError> {
    knock_target_with_timeout(target, DEFAULT_RCS_KNOCK_TIMEOUT).await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitFor {
    DeviceOnline,
    DeviceOffline,
}

const DOWN_REPOLL_DELAY_MS: u64 = 500;

pub async fn wait_for_device(
    wait_timeout: Option<Duration>,
    env: &EnvironmentContext,
    target_spec: &Option<String>,
    behavior: WaitFor,
) -> Result<(), ffx_command_error::Error> {
    let ever_found = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let use_cache = behavior == WaitFor::DeviceOffline;
    let knocker = LocalRcsKnockerImpl { ever_found: ever_found.clone(), use_cache };
    wait_for_device_inner(knocker, wait_timeout, env, target_spec, behavior, ever_found).await
}

async fn wait_for_device_inner(
    knocker: impl RcsKnocker,
    wait_timeout: Option<Duration>,
    env: &EnvironmentContext,
    target_spec: &Option<String>,
    behavior: WaitFor,
    ever_found: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<(), ffx_command_error::Error> {
    let ever_knocked = std::sync::atomic::AtomicBool::new(false);
    let target_spec_clone = target_spec.clone();
    let knock_fut = async {
        loop {
            futures_lite::future::yield_now().await;
            // Note that we transform the target_spec into a query every time
            // through the loop, because while we are waiting for the device, we
            // may have to wait for a valid scope-id to appear.
            let query = match TargetInfoQuery::try_from(target_spec_clone.clone()) {
                Ok(q) => q,
                Err(e) => {
                    log::debug!("Waiting for valid target specifier: {e}");
                    Timer::new(Duration::from_millis(DOWN_REPOLL_DELAY_MS)).await;
                    continue;
                }
            };

            break match knocker.knock_rcs(&query, &env).await {
                Err(e) => {
                    log::debug!("unable to knock target: {e:?}");
                    if let WaitFor::DeviceOffline = behavior {
                        Ok(())
                    } else {
                        match e {
                            KnockError::Critical(e) => {
                                Err(ffx_command_error::Error::Unexpected(e.into()))
                            }
                            KnockError::NonCritical(e) => {
                                log::debug!("received non-critical error. retrying. error: {e}");
                                Timer::new(Duration::from_millis(DOWN_REPOLL_DELAY_MS)).await;
                                continue;
                            }
                        }
                    }
                }
                Ok(()) => {
                    ever_knocked.store(true, std::sync::atomic::Ordering::Relaxed);
                    if let WaitFor::DeviceOffline = behavior {
                        Timer::new(Duration::from_millis(DOWN_REPOLL_DELAY_MS)).await;
                        continue;
                    } else {
                        Ok(())
                    }
                }
            };
        }
    };
    let timer = if wait_timeout.is_some() {
        Either::Left(fuchsia_async::Timer::new(wait_timeout.unwrap()))
    } else {
        Either::Right(pending())
    };
    futures_lite::FutureExt::or(knock_fut, async {
        timer.await;
        let was_knocked = ever_knocked.load(std::sync::atomic::Ordering::Relaxed);
        Err(ffx_command_error::Error::User(match behavior {
            WaitFor::DeviceOnline => FfxTargetError::DaemonError {
                err: DaemonError::Timeout,
                target: target_spec.clone().into(),
            }
            .into(),
            WaitFor::DeviceOffline => {
                if was_knocked {
                    FfxTargetError::DaemonError {
                        err: DaemonError::ShutdownTimeout,
                        target: target_spec.clone().into(),
                    }
                    .into()
                } else if ever_found.load(std::sync::atomic::Ordering::Relaxed) {
                    let msg = match target_spec {
                        Some(spec) if !spec.is_empty() => format!("Timeout waiting for device to shut down. Device \"{spec}\" was found but never responsive."),
                        _ => "Timeout waiting for device to shut down. The device was found but never responsive.".to_string(),
                    };
                    anyhow::anyhow!(msg).into()
                } else {
                    let discovery_timeout_ms = env.get::<u64, _>(ffx_config::keys::LOCAL_DISCOVERY_TIMEOUT).unwrap_or(2000);
                    let wait_timeout_ms = wait_timeout.map(|d| d.as_millis() as u64).unwrap_or(u64::MAX);

                    if wait_timeout_ms < discovery_timeout_ms {
                        anyhow::anyhow!("Timeout waiting for device to shut down. The specified timeout ({}ms) was too short to allow discovery to complete (discovery timeout is {}ms).", wait_timeout_ms, discovery_timeout_ms).into()
                    } else {
                        let msg = match target_spec {
                            Some(spec) if !spec.is_empty() => format!("Timeout waiting for device to shut down. Device \"{spec}\" was never found."),
                            _ => "Timeout waiting for device to shut down. The device was never found.".to_string(),
                        };
                        anyhow::anyhow!(msg).into()
                    }
                }
            }
        }))
    })
    .await
}

/// Represents the ability to knock RCS on a specified Target.
#[cfg_attr(test, mockall::automock)]
pub trait RcsKnocker {
    fn knock_rcs(
        &self,
        target_spec: &TargetInfoQuery,
        env: &EnvironmentContext,
    ) -> impl Future<Output = Result<(), KnockError>>;
}

///  Knocks RCS without calling the ffx daemon.
pub struct LocalRcsKnockerImpl {
    pub ever_found: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub use_cache: bool,
}

impl<T: RcsKnocker + ?Sized> RcsKnocker for Box<T> {
    fn knock_rcs(
        &self,
        target_spec: &TargetInfoQuery,
        env: &EnvironmentContext,
    ) -> impl Future<Output = Result<(), KnockError>> {
        (**self).knock_rcs(target_spec, env)
    }
}

impl RcsKnocker for LocalRcsKnockerImpl {
    async fn knock_rcs(
        &self,
        target_spec: &TargetInfoQuery,
        env: &EnvironmentContext,
    ) -> Result<(), KnockError> {
        knock_target_daemonless_impl(
            target_spec,
            env,
            None,
            self.use_cache,
            Some(self.ever_found.clone()),
        )
        .await
        .map(|compat| {
            let msg = match compat {
                Some(c) => format!("Received compat info: {c:?}"),
                None => format!("No compat info received"),
            };
            log::debug!("Knocked target. {msg}");
        })
    }
}

/// Attempts to "knock" a target to determine if it is up and connectable via RCS, within
/// a specified timeout.
///
/// This is intended to be run in a loop, with a non-critical error implying the caller
/// should call again, and a critical error implying the caller should raise the error
/// and no longer loop.
///
/// The timeout must be longer than `rcs::RCS_KNOCK_TIMEOUT`
async fn knock_target_with_timeout(
    target: &TargetProxy,
    rcs_timeout: Duration,
) -> Result<(), KnockError> {
    if rcs_timeout <= rcs::RCS_KNOCK_TIMEOUT {
        return Err(KnockError::Critical(KnockCriticalError::Custom(format!(
            "rcs verification timeout must be greater than {:?}",
            rcs::RCS_KNOCK_TIMEOUT
        ))));
    }
    let (rcs_proxy, remote_server_end) = create_proxy::<RemoteControlMarker>();

    let open_result = timeout(rcs_timeout, target.open_remote_control(remote_server_end)).await;
    match open_result {
        Err(_) => {
            return Err(KnockError::NonCritical(KnockNonCriticalError::Timeout {
                detail: "timing out opening remote control".to_string(),
            }));
        }
        Ok(Err(e)) => {
            return Err(KnockError::NonCritical(KnockNonCriticalError::Custom(format!(
                "FIDL error opening remote control: {:?}",
                e
            ))));
        }
        Ok(Ok(Err(e))) => {
            return Err(KnockError::NonCritical(KnockNonCriticalError::Custom(format!(
                "open remote control err: {:?}",
                e
            ))));
        }
        Ok(Ok(Ok(()))) => {}
    }

    match rcs::knock_rcs(&rcs_proxy).await {
        Ok(()) => Ok(()),
        Err(e) => Err(KnockError::NonCritical(KnockNonCriticalError::RcsKnockFailed {
            detail: format!("{e:?}"),
        })),
    }
}

/// Same as `knock_target_with_timeout` but takes a `TargetCollection` and an
/// optional target name and finds the target to knock. Uses the configured
/// default target if `target_name` is `None`.
pub async fn knock_target_by_name(
    target_name: &Option<String>,
    target_collection_proxy: &TargetCollectionProxy,
    open_timeout: Duration,
    rcs_timeout: Duration,
) -> Result<(), KnockError> {
    let (target_proxy, target_remote) = create_proxy::<TargetMarker>();

    let open_result = timeout::timeout(
        open_timeout,
        target_collection_proxy.open_target(
            &TargetQuery { string_matcher: target_name.clone(), ..Default::default() },
            target_remote,
        ),
    )
    .await;

    match open_result {
        Err(_) => {
            return Err(KnockError::NonCritical(KnockNonCriticalError::Timeout {
                detail: "Timeout opening target.".to_string(),
            }));
        }
        Ok(Err(e)) => {
            return Err(KnockError::Critical(KnockCriticalError::LostDaemonConnection {
                detail: format!("Full context:\n{}", e),
            }));
        }
        Ok(Ok(Err(e))) => {
            return Err(KnockError::Critical(KnockCriticalError::TargetError(format!("{:?}", e))));
        }
        Ok(Ok(Ok(()))) => {}
    }

    knock_target_with_timeout(&target_proxy, rcs_timeout).await
}

/// Identical to the above "knock_target" but does not use the daemon.
///
/// Keep in mind because there is no daemon being used, the connection process must be bootstrapped
/// for each attempt, so this function may need more time to run than the functions that perform
/// this action through the daemon (which is presumed to be already active). As a result, if
/// `knock_timeout` is set to `None`, the default timeout will be set to 2 times
/// `DEFAULT_RCS_KNOCK_TIMEOUT`.
pub async fn knock_target_daemonless(
    target_spec: &TargetInfoQuery,
    context: &EnvironmentContext,
    knock_timeout: Option<Duration>,
) -> Result<Option<CompatibilityInfo>, KnockError> {
    knock_target_daemonless_impl(target_spec, context, knock_timeout, false, None).await
}

pub(crate) async fn knock_target_daemonless_impl(
    target_spec: &TargetInfoQuery,
    context: &EnvironmentContext,
    knock_timeout: Option<Duration>,
    use_cache: bool,
    ever_found: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<Option<CompatibilityInfo>, KnockError> {
    let knock_timeout = knock_timeout.unwrap_or(DEFAULT_RCS_KNOCK_TIMEOUT * 2);
    let res_future = async {
        log::debug!("resolving target spec address from {target_spec:?}");
        let discovery = build_discovery_from_config(context);
        let resolver = resolve::DefaultTargetResolver::new(discovery);
        let res = resolver.resolve_target_address(target_spec, use_cache, context).await.map_err(
            |e| match e {
                // When knocking, it's not critical if we have not yet found the target. The caller should just retry
                FfxTargetError::OpenTargetError {
                    err: ffx::OpenTargetError::TargetNotFound,
                    ..
                } => KnockError::NonCritical(KnockNonCriticalError::TargetNotFound {
                    target: format!("{:?}", target_spec),
                }),
                _ => KnockError::Critical(KnockCriticalError::TargetError(format!("{:?}", e))),
            },
        )?;

        if let Some(ever_found) = ever_found {
            ever_found.store(true, std::sync::atomic::Ordering::Relaxed);
        }

        log::debug!("daemonless knock connecting to resolved target {:?}", res);
        let conn = match res.get_connection_if_already_established() {
            Some(c) => c,
            None => {
                let conn = res.get_connection(context).await.map_err(|e| {
                    KnockError::Critical(KnockCriticalError::TargetError(format!("{:?}", e)))
                })?;
                log::debug!("daemonless knock connection established");
                let _ = conn.rcs_proxy_fdomain().await.map_err(|e| {
                    KnockError::NonCritical(KnockNonCriticalError::Custom(format!("{:?}", e)))
                })?;
                conn
            }
        };
        Ok(conn.compatibility_info())
    };
    futures_lite::pin!(res_future);
    timeout::timeout(knock_timeout, res_future).await.map_err(|_| {
        KnockError::NonCritical(KnockNonCriticalError::Timeout {
            detail: "daemonless knock timeout".to_string(),
        })
    })?
}

/// Get the target specifier.
///
/// ## What this function does
///
/// The target specifier is a string taken from the `EnvironmentContext` one of
/// two ways:
///
/// 1. From the top-level `ffx` `-t/--target` command line argument.
/// 2. From an environment variable. The environment will be searched, in order,
///    for `$FUCHSIA_DEVICE_ADDR`, and then `$FUCHSIA_NODENAME`. If neither are
///    set, then this function will return `Ok(None)`.
///
/// `ffx` validates that `-t/--target` must be set whenever using `--strict`,
/// erroring out if the target is not explicitly specified via command line.
///
///
/// ## Underlying config specifics
///
/// For more detail: The target specifier always comes from the config key
/// `target.default`. In step (1) the top level command line argument `--target`
/// or `-t` sets the `target.default` value for the _runtime config_, which
/// supersedes all other config levels. For more info see the `ffx_config`
/// crate.
///
/// In step (2) the `target.default` value (at the _default config_) is set to
/// an array of environment variables. The first environment variable found is
/// returned. If none are found, this function returns `Ok(None)`.
/// See the `target.default` field in `//src/developer/ffx/data/config.json`.
///
/// Note: Stateful config sources for `target.default` are always bypassed and
/// ignored here (i.e. ConfigLevel::{User, Build, Global}).
/// Only stateless config sources (i.e. `ConfigLevel::{Runtime, Default}`) for
/// `target.default` are used to determine the target specifier.
/// See https://fxbug.dev/394619603 for the rationale around this decision.
///
///
/// ## How the return value is intended to be used
///
/// The result is a string which can be turned into a `TargetInfoQuery` to match
/// against the available targets (by name, address, etc). We don't return the
/// query itself because some callers assume the specifier is the name of the
/// target. This is used for the purposes of error messages or other forms of
/// presentation. The repo server, for example, only works if an explicit
/// device name (exact match) is provided.  In other contexts, it is valid for
/// the specifier to be a substring of the nodename, a network address, serial
/// number, or vsock identifier.
pub fn get_target_specifier(context: &EnvironmentContext) -> Result<Option<String>> {
    if let Some(ts) = context.get_overridden_target_specifier() {
        return Ok(ts);
    }
    let target_spec = match context
        .query(TARGET_DEFAULT_KEY)
        .level(Some(ConfigLevel::Runtime))
        .build()
        .get_optional::<Option<String>>(context)
    {
        Ok(None) => context
            .query(TARGET_DEFAULT_KEY)
            .level(Some(ConfigLevel::Default))
            .build()
            .get_optional::<Option<String>>(context),
        runtime_result => runtime_result,
    }?;

    match target_spec {
        Some(ref target) => info!("Target specifier: ['{target:?}']"),
        None => debug!("No target specified"),
    }
    Ok(target_spec)
}

pub async fn add_manual_target(
    target_collection_proxy: &TargetCollectionProxy,
    addr: IpAddr,
    scope_id: u32,
    port: u16,
    wait: bool,
) -> Result<()> {
    let ip = match addr {
        IpAddr::V6(i) => net::IpAddress::Ipv6(net::Ipv6Address { addr: i.octets().into() }),
        IpAddr::V4(i) => net::IpAddress::Ipv4(net::Ipv4Address { addr: i.octets().into() }),
    };
    let addr = if port > 0 {
        ffx::TargetIpAddrInfo::IpPort(ffx::TargetIpPort { ip, port, scope_id })
    } else {
        ffx::TargetIpAddrInfo::Ip(ffx::TargetIp { ip, scope_id })
    };

    let taddr = TargetIpAddr::from(&addr);
    const DEFAULT_SSH_PORT: u16 = 22;
    let taddr_str = match taddr.ip() {
        IpAddr::V4(_) => format!("{}", taddr),
        IpAddr::V6(_) => format!("[{}]", taddr),
    };
    let port = taddr.port();
    let target = Some(format!("{}:{}", taddr_str, if port == 0 { DEFAULT_SSH_PORT } else { port }));

    let (client, mut stream) =
        fidl::endpoints::create_request_stream::<ffx::AddTargetResponder_Marker>();
    target_collection_proxy
        .add_target(
            &taddr.into(),
            &ffx::AddTargetConfig { verify_connection: Some(wait), ..Default::default() },
            client,
        )
        .context("calling AddTarget")?;
    let res = match stream.try_next().await {
        Ok(Some(ffx::AddTargetResponder_Request::Success { .. })) => Ok(()),
        Ok(Some(ffx::AddTargetResponder_Request::Error { err, .. })) => Err(err),
        Err(e) => {
            return Err(FfxTargetError::DaemonCommunicationError {
                target: target.clone(),
                error: std::sync::Arc::new(e),
            }
            .into());
        }
        Ok(None) => {
            return Err(FfxTargetError::DaemonError {
                err: DaemonError::Timeout,
                target: target.clone(),
            }
            .into());
        }
    };

    // Pass formatted ip and port to target connection error, so it is more user friendly
    res.map_err(|e| {
        let err = e.connection_error.unwrap();
        let logs = e.connection_error_logs.map(|v| v.join("\n"));
        FfxTargetError::TargetConnectionError { err, target, logs }.into()
    })
}

/// Discover fastboot targets only. Useful for fastboot-related plugins (flash/bootloader/fastboot).
pub async fn discover_fastboot_target(
    ctx: &EnvironmentContext,
    query: TargetInfoQuery,
    timeout: Option<u64>,
) -> Result<TargetHandle> {
    let mut builder = crate::resolve::build_discovery_builder(DiscoverySources::all(), ctx);
    if let Some(ms) = timeout {
        builder = builder.with_timeout_msecs(Some(ms));
    };
    let disco = builder.build(&ctx);

    let discovered_devices = disco.discover_devices(query.clone()).await?;
    let filtered: Vec<_> = discovered_devices
        .into_iter()
        .filter(|h| matches!(h.state, discovery::TargetState::Fastboot(_)))
        .collect();

    resolve::expect_single_target(&query, filtered).map_err(|e| e.into())
}

#[cfg(test)]
mod test {
    use super::*;
    use ffx_command_error::bug;
    use ffx_config::{test_env, test_init};
    use futures_lite::future::{pending, ready};
    use tempfile::tempdir;

    #[fuchsia::test]
    async fn test_get_target_specifier_unset() {
        // Explicitly initialize the test with no env vars.
        // That way, $FUCHSIA_NODENAME and $FUCHSIA_DEVICE_ADDR are both unset.
        let env = test_env().build().unwrap();

        let target_spec = get_target_specifier(&env.context).unwrap();
        assert_eq!(target_spec, None);
    }

    #[fuchsia::test]
    async fn test_get_target_specifier_from_nodename_env() {
        let env = test_env().env_var("FUCHSIA_NODENAME", "nodename-default").build().unwrap();

        let target_spec = get_target_specifier(&env.context).unwrap();
        assert_eq!(target_spec, Some("nodename-default".into()));
    }

    #[fuchsia::test]
    async fn test_get_target_specifier_from_device_addr_env() {
        let env = test_env().env_var("FUCHSIA_DEVICE_ADDR", "device-addr-default").build().unwrap();

        let target_spec = get_target_specifier(&env.context).unwrap();
        assert_eq!(target_spec, Some("device-addr-default".into()));
    }

    #[fuchsia::test]
    async fn test_get_target_specifier_from_both_envs() {
        let env = test_env()
            .env_var("FUCHSIA_NODENAME", "nodename-default")
            .env_var("FUCHSIA_DEVICE_ADDR", "device-addr-default")
            .build()
            .unwrap();

        let target_spec = get_target_specifier(&env.context).unwrap();
        assert_eq!(target_spec, Some("device-addr-default".into()));
    }

    #[fuchsia::test]
    async fn test_get_target_specifier_bypasses_state() {
        let build_dir = tempdir().expect("temp dir");
        let env = test_env()
            .in_tree(build_dir.path())
            .user_config(TARGET_DEFAULT_KEY, "stateful-user-default")
            .build_config(TARGET_DEFAULT_KEY, "stateful-build-default")
            .global_config(TARGET_DEFAULT_KEY, "stateful-global-default")
            .build()
            .unwrap();

        let target_spec = get_target_specifier(&env.context).unwrap();
        assert_eq!(target_spec, None);
    }

    #[fuchsia::test]
    async fn test_get_target_specifier_from_nodename_env_bypasses_state() {
        let build_dir = tempdir().expect("temp dir");
        let env = test_env()
            .env_var("FUCHSIA_NODENAME", "nodename-default")
            .in_tree(build_dir.path())
            .user_config(TARGET_DEFAULT_KEY, "stateful-user-default")
            .build_config(TARGET_DEFAULT_KEY, "stateful-build-default")
            .global_config(TARGET_DEFAULT_KEY, "stateful-global-default")
            .build()
            .unwrap();

        let target_spec = get_target_specifier(&env.context).unwrap();
        assert_eq!(target_spec, Some("nodename-default".into()));
    }

    #[fuchsia::test]
    async fn test_get_target_specifier_from_all_sources() {
        let build_dir = tempdir().expect("temp dir");
        let env = test_env()
            .env_var("FUCHSIA_NODENAME", "nodename-default")
            .env_var("FUCHSIA_DEVICE_ADDR", "device-addr-default")
            .runtime_config(TARGET_DEFAULT_KEY, "runtime-default")
            .in_tree(build_dir.path())
            .user_config(TARGET_DEFAULT_KEY, "stateful-user-default")
            .build_config(TARGET_DEFAULT_KEY, "stateful-build-default")
            .global_config(TARGET_DEFAULT_KEY, "stateful-global-default")
            .build()
            .unwrap();

        let target_spec = get_target_specifier(&env.context).unwrap();
        assert_eq!(target_spec, Some("runtime-default".into()));
    }

    #[fuchsia::test]
    async fn test_get_override_target_spec() {
        let env = test_init().unwrap();
        let mut context = env.context.clone();
        context.override_target_specifier(&Some("foo".to_string()));
        let target = get_target_specifier(&context).expect("get_target_specifier");
        assert_eq!(target, Some("foo".to_string()));
    }
    #[fuchsia::test]
    async fn test_target_wait_too_short_timeout() {
        let (proxy, _server) = fidl::endpoints::create_proxy::<ffx::TargetMarker>();
        let res = knock_target_with_timeout(&proxy, rcs::RCS_KNOCK_TIMEOUT).await;
        assert!(res.is_err());
        let res = knock_target_with_timeout(
            &proxy,
            rcs::RCS_KNOCK_TIMEOUT.checked_sub(Duration::new(0, 1)).unwrap(),
        )
        .await;
        assert!(res.is_err());
    }

    #[fuchsia::test]
    async fn test_bad_timeout() {
        let env = test_init().unwrap();
        assert!(
            knock_target_daemonless(
                &TargetInfoQuery::NodenameOrSerial("foo".to_string()),
                &env.context,
                Some(rcs::RCS_KNOCK_TIMEOUT)
            )
            .await
            .is_err()
        );
    }

    #[fuchsia::test]
    async fn wait_for_device_knock_works() {
        let mut mock = MockRcsKnocker::new();
        mock.expect_knock_rcs().returning(|_, _| Box::pin(async { Ok(()) }));
        let env = ffx_config::test_init().unwrap();
        let res = wait_for_device_inner(
            mock,
            Some(Duration::from_secs(3000)),
            &env.context,
            &Some("foo".to_string()),
            WaitFor::DeviceOnline,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await;
        assert!(res.is_ok(), "{:?}", res);
    }

    #[fuchsia::test]
    async fn wait_for_device_timeout_on_shutdown() {
        let mut mock = MockRcsKnocker::new();
        let mut seq = mockall::Sequence::new();
        mock.expect_knock_rcs()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|_, _| Box::pin(ready(Ok(()))));
        mock.expect_knock_rcs().returning(|_, _| Box::pin(pending()));
        let env = ffx_config::test_init().unwrap();
        let res = wait_for_device_inner(
            mock,
            Some(Duration::from_secs(3)),
            &env.context,
            &Some("foo".to_string()),
            WaitFor::DeviceOffline,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await;
        // This step is essential for converting the error properly. Otherwise converting it to top
        // level anyhow error will lost context and turn the error into a string, making
        // downcasting infeasible.
        let anyhow_err: anyhow::Error =
            res.unwrap_err().source().expect("should have an anyhow error source");
        let FfxTargetError::DaemonError { err, .. } =
            anyhow_err.downcast_ref::<FfxTargetError>().expect("expected target error")
        else {
            panic!("Received unexpected error: {anyhow_err:?}");
        };
        assert!(matches!(err, DaemonError::ShutdownTimeout));
    }

    #[fuchsia::test]
    async fn wait_for_device_timeout_on_shutdown_never_knocked() {
        let mut mock = MockRcsKnocker::new();
        mock.expect_knock_rcs().returning(|_, _| Box::pin(pending()));
        let env = ffx_config::test_init().unwrap();
        let res = wait_for_device_inner(
            mock,
            Some(Duration::from_secs(3)),
            &env.context,
            &Some("foo".to_string()),
            WaitFor::DeviceOffline,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await;
        let err = res.unwrap_err();
        let err_msg = err.to_string();
        assert!(
            err_msg.contains(
                "Timeout waiting for device to shut down. Device \"foo\" was never found."
            ),
            "expected new error message, got: {err_msg}"
        );
    }

    #[fuchsia::test]
    async fn wait_for_device_timeout_on_shutdown_found_but_never_responsive() {
        let mut mock = MockRcsKnocker::new();
        mock.expect_knock_rcs().returning(|_, _| Box::pin(pending()));
        let env = ffx_config::test_init().unwrap();
        let ever_found = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let res = wait_for_device_inner(
            mock,
            Some(Duration::from_secs(1)),
            &env.context,
            &Some("foo".to_string()),
            WaitFor::DeviceOffline,
            ever_found.clone(),
        )
        .await;
        let err = res.unwrap_err();
        let err_msg = err.to_string();
        assert!(
            err_msg.contains(
                "Timeout waiting for device to shut down. Device \"foo\" was found but never responsive."
            ),
            "expected new error message, got: {err_msg}"
        );
    }

    #[fuchsia::test]
    async fn wait_for_device_timeout_on_shutdown_short_timeout() {
        let mut mock = MockRcsKnocker::new();
        mock.expect_knock_rcs().returning(|_, _| Box::pin(pending()));

        let env = ffx_config::test_env()
            .user_config(ffx_config::keys::LOCAL_DISCOVERY_TIMEOUT, 2000)
            .build()
            .unwrap();

        let res = wait_for_device_inner(
            mock,
            Some(Duration::from_secs(1)),
            &env.context,
            &Some("foo".to_string()),
            WaitFor::DeviceOffline,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await;
        let err = res.unwrap_err();
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("was too short to allow discovery to complete"),
            "expected short timeout message, got: {err_msg}"
        );
    }

    #[fuchsia::test]
    async fn wait_for_device_hangs_indefinitely() {
        let mut mock = MockRcsKnocker::new();
        mock.expect_knock_rcs().returning(|_, _| Box::pin(pending()));
        let env = ffx_config::test_init().unwrap();
        let res = wait_for_device_inner(
            mock,
            Some(Duration::from_secs(3)),
            &env.context,
            &Some("foo".to_string()),
            WaitFor::DeviceOnline,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await;
        assert!(res.is_err(), "{:?}", res);
    }

    #[fuchsia::test]
    async fn wait_for_device_critical_error_causes_failure() {
        let mut mock = MockRcsKnocker::new();
        mock.expect_knock_rcs().times(1).returning(|_, _| {
            Box::pin(async {
                Err(KnockError::Critical(KnockCriticalError::Custom(format!("{}", bug!("Oh no!")))))
            })
        });
        let env = ffx_config::test_init().unwrap();
        let res = wait_for_device_inner(
            mock,
            Some(Duration::from_secs(5)),
            &env.context,
            &Some("foo".to_string()),
            WaitFor::DeviceOnline,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await;
        assert!(res.is_err(), "{:?}", res);
    }

    #[fuchsia::test]
    async fn wait_for_device_critical_error_does_not_cause_failure_waiting_for_down() {
        let mut mock = MockRcsKnocker::new();
        mock.expect_knock_rcs().times(1).returning(|_, _| {
            Box::pin(async {
                Err(KnockError::Critical(KnockCriticalError::Custom(format!("{}", bug!("Oh no!")))))
            })
        });
        let env = ffx_config::test_init().unwrap();
        let res = wait_for_device_inner(
            mock,
            Some(Duration::from_secs(5)),
            &env.context,
            &Some("foo".to_string()),
            WaitFor::DeviceOffline,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await;
        assert!(res.is_ok(), "{:?}", res);
    }

    #[fuchsia::test]
    async fn non_critical_error_causes_eventual_timeout() {
        let mut mock = MockRcsKnocker::new();
        mock.expect_knock_rcs().returning(|_, _| {
            Box::pin(async {
                Err(KnockError::NonCritical(KnockNonCriticalError::Custom(format!(
                    "{}",
                    bug!("Oh no!")
                ))))
            })
        });
        let env = ffx_config::test_init().unwrap();
        let res = wait_for_device_inner(
            mock,
            Some(Duration::from_secs(3)),
            &env.context,
            &Some("foo".to_string()),
            WaitFor::DeviceOnline,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await;
        assert!(res.is_err(), "{:?}", res);
    }

    #[fuchsia::test]
    async fn non_critical_error_returns_ok_for_down_target() {
        let mut mock = MockRcsKnocker::new();
        mock.expect_knock_rcs().returning(|_, _| {
            Box::pin(async {
                Err(KnockError::NonCritical(KnockNonCriticalError::Custom(format!(
                    "{}",
                    bug!("Oh no!")
                ))))
            })
        });
        let env = ffx_config::test_init().unwrap();
        let res = wait_for_device_inner(
            mock,
            Some(Duration::from_secs(5)),
            &env.context,
            &Some("foo".to_string()),
            WaitFor::DeviceOffline,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await;
        assert!(res.is_ok(), "{:?}", res);
    }

    #[fuchsia::test]
    async fn knock_error_reattempt_successful() {
        let mut mock = MockRcsKnocker::new();
        let mut seq = mockall::Sequence::new();
        mock.expect_knock_rcs().times(1).in_sequence(&mut seq).returning(|_, _| {
            Box::pin(ready(Err(KnockError::NonCritical(KnockNonCriticalError::Timeout {
                detail: "timeout".to_string(),
            }))))
        });
        mock.expect_knock_rcs()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|_, _| Box::pin(ready(Ok(()))));
        let env = ffx_config::test_init().unwrap();
        let res = wait_for_device_inner(
            mock,
            Some(Duration::from_secs(10)),
            &env.context,
            &Some("foo".to_string()),
            WaitFor::DeviceOnline,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await;
        assert!(res.is_ok(), "{:?}", res);
    }

    #[fuchsia::test]
    async fn wait_for_offline_after_online() {
        let mut mock = MockRcsKnocker::new();
        let mut seq = mockall::Sequence::new();
        mock.expect_knock_rcs()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|_, _| Box::pin(ready(Ok(()))));
        mock.expect_knock_rcs()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|_, _| Box::pin(ready(Ok(()))));
        mock.expect_knock_rcs().times(1).in_sequence(&mut seq).returning(|_, _| {
            Box::pin(ready(Err(KnockError::NonCritical(KnockNonCriticalError::Custom(format!(
                "{}",
                bug!("Oh no it's not connected")
            ))))))
        });
        let env = ffx_config::test_init().unwrap();
        let res = wait_for_device_inner(
            mock,
            Some(Duration::from_secs(3)),
            &env.context,
            &Some("foo".to_string()),
            WaitFor::DeviceOffline,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await;
        assert!(res.is_ok(), "{:?}", res);
    }

    #[fuchsia::test]
    async fn wait_for_down_when_able_to_connect_to_device() {
        let mut mock = MockRcsKnocker::new();
        mock.expect_knock_rcs().returning(|_, _| Box::pin(ready(Ok(()))));
        let env = ffx_config::test_init().unwrap();
        let res = wait_for_device_inner(
            mock,
            Some(Duration::from_secs(3)),
            &env.context,
            &Some("foo".to_string()),
            WaitFor::DeviceOffline,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await;
        assert!(res.is_err(), "{:?}", res);
    }

    // We implement the fake daemon and target mock handlers locally rather than using
    // `FakeDaemon` from the protocols crate to prevent circular dependencies (as the
    // protocols crate depends on `ffx_target`).
    async fn run_fake_daemon(
        mut stream: ffx::DaemonRequestStream,
        connection_counter: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        while let Ok(Some(req)) = stream.try_next().await {
            match req {
                ffx::DaemonRequest::ConnectToProtocol { name, server_end, responder } => {
                    if name == ffx::TargetCollectionMarker::PROTOCOL_NAME {
                        let stream =
                            fidl::endpoints::ServerEnd::<ffx::TargetCollectionMarker>::new(
                                server_end,
                            )
                            .into_stream();
                        let connection_counter = connection_counter.clone();
                        fuchsia_async::Task::local(async move {
                            run_fake_target_collection(stream, connection_counter).await;
                        })
                        .detach();
                        responder.send(Ok(())).unwrap();
                    } else {
                        responder.send(Err(ffx::DaemonError::ProtocolOpenError)).unwrap();
                    }
                }
                _ => {}
            }
        }
    }

    async fn run_fake_target_collection(
        mut stream: ffx::TargetCollectionRequestStream,
        connection_counter: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        while let Ok(Some(req)) = stream.try_next().await {
            match req {
                ffx::TargetCollectionRequest::OpenTarget { query: _, target_handle, responder } => {
                    let stream = target_handle.into_stream();
                    let connection_counter = connection_counter.clone();
                    fuchsia_async::Task::local(async move {
                        run_fake_target(stream, connection_counter).await;
                    })
                    .detach();
                    responder.send(Ok(())).unwrap();
                }
                _ => {}
            }
        }
    }

    async fn run_fake_target(
        mut stream: ffx::TargetRequestStream,
        connection_counter: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        while let Ok(Some(req)) = stream.try_next().await {
            match req {
                ffx::TargetRequest::OpenRemoteControl { remote_control: _, responder } => {
                    let attempt =
                        connection_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if attempt < 2 {
                        responder.send(Err(ffx::TargetConnectionError::ConnectionRefused)).unwrap();
                    } else {
                        responder.send(Ok(())).unwrap();
                    }
                }
                ffx::TargetRequest::Identity { responder } => {
                    responder.send(&ffx::TargetInfo::default()).unwrap();
                }
                ffx::TargetRequest::GetSshLogs { responder } => {
                    responder.send("mock ssh logs").unwrap();
                }
                _ => {}
            }
        }
    }

    // Verify that daemon-based target connection retries utilize the exponential backoff delay,
    // avoiding high-frequency retry loops when encountering non-fatal connection errors.
    #[fuchsia::test]
    #[allow(clippy::large_futures)]
    async fn test_daemon_remote_proxy_retry_rate() {
        let env = test_init().unwrap();
        let (daemon_proxy, daemon_stream) =
            fidl::endpoints::create_proxy_and_stream::<ffx::DaemonMarker>();
        let connection_counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter_clone = connection_counter.clone();

        fuchsia_async::Task::local(async move {
            run_fake_daemon(daemon_stream, counter_clone).await;
        })
        .detach();

        let target_spec = TargetInfoQuery::NodenameOrSerial("fake-device".to_string());

        let proxy_timeout = Duration::from_millis(50);
        let start = std::time::Instant::now();

        let res =
            get_remote_proxy(&target_spec, daemon_proxy, proxy_timeout, None, &env.context).await;
        let elapsed = start.elapsed();

        assert!(res.is_ok(), "Expected connection to succeed, got {:?}", res);

        let retries = connection_counter.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            retries, 3,
            "Expected exactly 3 attempts (2 retries and 1 success), got {}",
            retries
        );
        assert!(
            elapsed >= Duration::from_millis(250),
            "Expected elapsed time to be at least 250ms due to backoff, got {:?}",
            elapsed
        );
    }
}
