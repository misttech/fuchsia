// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use addr::TargetIpAddr;
use anyhow::{anyhow, bail, Context, Result};
use discovery::query::TargetInfoQuery;
use discovery::{
    Discovery, DiscoveryBuilder, DiscoverySources, FastbootConnectionState, TargetEvent,
    TargetHandle, TargetState,
};
use ffx_command_error::{user_error, NonFatalError};
use ffx_config::{EnvironmentContext, TryFromEnvContext};
use fidl_fuchsia_developer_ffx::{self as ffx};
use fidl_fuchsia_developer_remotecontrol::{IdentifyHostResponse, RemoteControlProxy};
use fuchsia_async::TimeoutExt;
use futures::future::{join_all, LocalBoxFuture};
use futures::{pin_mut, FutureExt, Stream, StreamExt};
use itertools::Itertools;
use netext::{IsLocalAddr, ScopedSocketAddr};
use std::cmp::Ordering;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use target_errors::FfxTargetError;

use crate::connection::Connection;
use crate::ssh_connector::SshConnector;
use crate::{get_target_specifier, UNSPECIFIED_TARGET_NAME};

const CONFIG_TARGET_SSH_TIMEOUT: &str = "target.host_pipe_ssh_timeout";
const TARGET_DEFAULT_PORT: u16 = 22;
const DEFAULT_SSH_TIMEOUT_MS: u64 = 10000;
const DISCOVERY_CACHE_DIR_CONFIG: &str = "target.discovery_cache_dir";

#[cfg(test)]
use {mockall::mock, mockall::predicate::*};

/// Check if daemon discovery is disabled, resolving locally if so.
pub async fn maybe_locally_resolve_target_spec(
    target_spec: &TargetInfoQuery,
    env_context: &EnvironmentContext,
) -> Result<TargetInfoQuery> {
    // This should be read as "is discovery enabled on the daemon".
    if crate::is_discovery_enabled(env_context).await {
        Ok(target_spec.clone())
    } else {
        log::warn!(
            "crate::is_discovery_enabled is false - using local target resolution. is_usb_discovery_disabled is {}, is_mdns_discovery_disabled is {}",
            ffx_config::is_usb_discovery_disabled(env_context),
            ffx_config::is_mdns_discovery_disabled(env_context)
        );

        locally_resolve_target_spec(target_spec, &DefaultTargetResolver::default(), env_context)
            .await
    }
}

fn replace_default_port(sa: SocketAddr) -> SocketAddr {
    let mut sa = sa;
    if sa.port() == 0 {
        sa.set_port(TARGET_DEFAULT_PORT);
    }
    sa
}

/// Attempts to resolve the query into an explicit string query that can be
/// passed to the daemon. If already an address or serial number, just return
/// it. Otherwise, perform discovery to find the address or serial #. Returns
/// Some(_) if a target has been found, None otherwise.
async fn locally_resolve_target_spec<T: TargetResolver>(
    target_spec: &TargetInfoQuery,
    resolver: &T,
    env_context: &EnvironmentContext,
) -> Result<TargetInfoQuery> {
    let query = TargetInfoQuery::from(target_spec.clone());
    let explicit_spec = match query {
        // If an address is passed in, make sure that the default port is filled in if it hadn't
        // been explicit, then just pass it on to the daemon as is
        TargetInfoQuery::Addr(addr) => format!("{}", replace_default_port(addr)),
        TargetInfoQuery::Serial(sn) => format!("serial:{sn}"),
        _ => {
            let resolution = resolver.resolve_single_target(&target_spec, env_context).await?;
            log::debug!("Locally resolved target '{target_spec:?}' to {:?}", resolution.discovered);
            resolution.target.to_spec()
        }
    };

    Ok(Some(explicit_spec).into())
}

/// A trait for resolving target queries into concrete target information.
///
/// This trait provides an abstraction over the process of finding a target
/// based on a user-provided specifier, which could be a nodename, a serial
/// number, an IP address, or other forms of target identification.
///
/// Implementors of this trait are responsible for searching for targets,
/// handling ambiguity, and returning a `Resolution` object that can be used
/// to connect to the target.
#[allow(async_fn_in_trait)]
pub trait TargetResolver: Default {
    /// Gets a set of handles from the resolver's sources that match the query
    async fn discovered_targets(
        &self,
        query: TargetInfoQuery,
        ctx: EnvironmentContext,
    ) -> Result<Vec<TargetHandle>>;

    /// Resolves a `TargetInfoQuery` into a list of matching `TargetHandle`s.
    ///
    /// This method is expected to perform discovery and return all targets
    /// that match the given query.
    async fn resolve_target_query(
        &self,
        query: TargetInfoQuery,
        ctx: &EnvironmentContext,
    ) -> Result<Vec<TargetHandle>>;

    /// Attempts to resolve a target by name from a manually configured list.
    ///
    /// This method is used to check for targets that have been explicitly
    /// defined by the user, outside of the standard discovery mechanisms.
    async fn try_resolve_manual_target(
        &self,
        name: &str,
        ctx: &EnvironmentContext,
    ) -> Result<Option<Resolution>>;

    /// Resolves a target specifier into a `Resolution` containing a connectable address.
    ///
    /// This is a high-level method that should handle various query types and
    /// ensure that a single, unambiguous target is found. It is an error if the query
    /// does not resolve to exactly one target.
    async fn resolve_target_address(
        &self,
        target_spec: &TargetInfoQuery,
        ctx: &EnvironmentContext,
    ) -> Result<Resolution, FfxTargetError> {
        let query = TargetInfoQuery::from(target_spec.clone());
        if let TargetInfoQuery::Addr(a) = query {
            return Ok(Resolution::from_addr(a));
        }
        let res = self.resolve_single_target(&target_spec, ctx).await?;
        let target_spec_info: String = target_spec.into();
        log::debug!("resolved target spec {target_spec_info} to address {:?}", res.addr());
        Ok(res)
    }

    /// Resolves a target specifier to a single `Resolution`.
    ///
    /// This method orchestrates the discovery process, including checking
    /// manual targets, and handles cases where no targets or multiple
    /// ambiguous targets are found.
    async fn resolve_single_target(
        &self,
        target_spec: &TargetInfoQuery,
        env_context: &EnvironmentContext,
    ) -> Result<Resolution, FfxTargetError> {
        if let TargetInfoQuery::Addr(a) = target_spec {
            return Ok(Resolution::from_addr(*a));
        }

        let handles_fut = self.discovered_targets(target_spec.clone(), env_context.clone()).fuse();
        pin_mut!(handles_fut);
        let mut discovered: Option<Vec<TargetHandle>> = None;

        // If the query is not specified, we won't even bother with trying to resolve a manual target.
        if let TargetInfoQuery::NodenameOrSerial(ref s) = target_spec {
            // We want to query both the manual targets and the discoverable handles concurrently.
            let manual_target_fut = self.try_resolve_manual_target(s, env_context).fuse();
            pin_mut!(manual_target_fut);
            futures::select! {
                mtr = &mut manual_target_fut => match mtr {
                    Err(e) => {
                        log::debug!("Failed to resolve target {s} as manual target: {e:?}");
                        // Keep going, waiting for the discovery to complete
                    }
                    Ok(Some(res)) => return Ok(res), // We found a manual target, so we're done
                    _ => (), // No manual target with this name
                },
                handles_res = handles_fut => match handles_res {
                    Ok(r) => discovered = Some(r),
                    Err(e) => {
                        log::warn!("Target discovery failed: {e:?}");
                        return Err(FfxTargetError::OpenTargetError {
                            err: ffx::OpenTargetError::FailedDiscovery,
                            target: Some(target_spec.into()),
                        })
                    },
                },
            }
        }

        // If we haven't gotten a result yet (because the query wasn't
        // NodenameOrSerial, or because the manual-target query completed
        // without finding a target), get it now.
        let discovered = match discovered {
            Some(targets) => targets,
            None => handles_fut.await.map_err(|e| {
                log::warn!("Target discovery failed: {e:?}");
                FfxTargetError::OpenTargetError {
                    err: ffx::OpenTargetError::FailedDiscovery,
                    target: Some(target_spec.into()),
                }
            })?,
        };

        match discovered.len() {
            0 => Err(FfxTargetError::OpenTargetError {
                err: ffx::OpenTargetError::TargetNotFound,
                target: Some(target_spec.into()),
            }),
            1 => Ok(Resolution::from_target_handle(discovered[0].clone()).map_err(|_| {
                // The conversion will fail if it is a fastboot device with no addresses, or
                // the target is in the wrong state. Roughly, we can consider that to be that
                // we couldn't find a valid target.
                FfxTargetError::OpenTargetError {
                    err: ffx::OpenTargetError::TargetNotFound,
                    target: Some(target_spec.into()),
                }
            })?),
            _ => Err(FfxTargetError::OpenTargetError {
                err: ffx::OpenTargetError::QueryAmbiguous,
                target: Some(target_spec.into()),
            }),
        }
    }
}

#[cfg(test)]
mock! {
    TargetResolver{}
    impl TargetResolver for TargetResolver {
        async fn discovered_targets(
            &self,
            query: TargetInfoQuery,
            ctx: EnvironmentContext,
        ) -> Result<Vec<TargetHandle>>;

        async fn resolve_target_query(
            &self,
            query: TargetInfoQuery,
            ctx: &EnvironmentContext,
        ) -> Result<Vec<TargetHandle>>;
        async fn try_resolve_manual_target(
            &self,
            name: &str,
            ctx: &EnvironmentContext,
        ) -> Result<Option<Resolution>>;
    }
}

/// Attempts to resolve a TargetInfoQuery into a list of discovered targets.
/// Useful when multiple results are reasonable (e.g. from `ffx target list`)
pub async fn resolve_target_query(
    query: TargetInfoQuery,
    ctx: &EnvironmentContext,
) -> Result<Vec<TargetHandle>> {
    log::debug!("Resolving query: {query:#?}");
    DefaultTargetResolver::default().resolve_target_query(query, ctx).await
}

fn build_discovery(
    sources: DiscoverySources,
    no_cache: bool,
    ctx: EnvironmentContext,
) -> Discovery {
    // Note that if there is an error getting these two config options, they
    // will simply be ignored. The alternative is to throw an error, which,
    // e.g. will cause ffx-strict to fail under certain circumstances if either
    // default config option is not overridden.
    let emu_instance_root = ctx.get(emulator_instance::EMU_INSTANCE_ROOT_DIR).ok();
    let fastboot_file_path = ctx.get(fastboot_file_discovery::FASTBOOT_FILE_PATH).ok();
    let mut builder = DiscoveryBuilder::default()
        .set_source(sources)
        .with_fastboot_devices_file_path(fastboot_file_path)
        .with_emulator_instance_root(emu_instance_root)
        .with_timeout_msecs(ctx.get(ffx_config::keys::LOCAL_DISCOVERY_TIMEOUT).ok());
    if !no_cache {
        builder = builder.with_cache_dir(get_discovery_cache_dir(&ctx));
    }
    builder.build()
}
/// Directory containing the discovery cache file
pub fn get_discovery_cache_dir(context: &EnvironmentContext) -> Option<PathBuf> {
    // We can unwrap() because conversion to Option<String> never fails
    let path_s: Option<String> = context.get(DISCOVERY_CACHE_DIR_CONFIG).unwrap();
    path_s.map(|s| s.into())
}

// Return a stream of TargetHandles that come from the specified sources, and
// match the query. The discovery will return early if a result "perfectly"
// matches the query (e.g. an mDNS response with the exact name requested).
fn get_discovery_stream_with_sources(
    query: TargetInfoQuery,
    sources: DiscoverySources,
    no_cache: bool,
    ctx: EnvironmentContext,
) -> Result<impl Stream<Item = TargetHandle>> {
    let discovery = build_discovery(sources, no_cache, ctx);
    // Just get the new handles
    let stream = discovery.discovery_stream(query).map_err(anyhow::Error::from)?;
    Ok(stream.filter_map(|ev| async move {
        if let TargetEvent::Added(th) = ev {
            Some(th)
        } else {
            None
        }
    }))
}

// Return a set of TargetHandles that come from the specified sources, and match
// the query. The discovery will return early if a result "perfectly" matches the
// query (e.g. an mDNS response with the exact name requested).
async fn get_discovered_targets_with_sources(
    query: TargetInfoQuery,
    sources: DiscoverySources,
    ctx: EnvironmentContext,
) -> Result<Vec<TargetHandle>> {
    // Currently no callers of this function need to be able to ignore the cache, so we always pass in no_cache=false
    let discovery = build_discovery(sources, false, ctx);
    discovery.discover_devices(query.clone()).await.map_err(|e| anyhow::anyhow!(e))
}

struct RetrievedTargetInfo {
    rcs_state: ffx::RemoteControlState,
    product_config: Option<String>,
    board_config: Option<String>,
    ssh_address: Option<ScopedSocketAddr>,
}

impl Default for RetrievedTargetInfo {
    fn default() -> Self {
        Self {
            rcs_state: ffx::RemoteControlState::Unknown,
            product_config: None,
            board_config: None,
            ssh_address: None,
        }
    }
}

async fn try_get_target_info(
    addr: &ScopedSocketAddr,
    context: &EnvironmentContext,
) -> Result<(Option<String>, Option<String>), crate::KnockError> {
    let connector = SshConnector::new(addr.clone(), context).context("making ssh connector")?;
    let conn = Connection::new(connector).await.context("making direct connection")?;
    let rcs = conn.rcs_proxy().await.context("getting RCS proxy")?;
    let (pc, bc) = match rcs.identify_host().await {
        Ok(Ok(id_result)) => (id_result.product_config, id_result.board_config),
        _ => (None, None),
    };
    Ok((pc, bc))
}

impl RetrievedTargetInfo {
    async fn get(context: &EnvironmentContext, addrs: &[addr::TargetAddr]) -> Result<Self> {
        let ssh_timeout: u64 =
            context.get(CONFIG_TARGET_SSH_TIMEOUT).unwrap_or(DEFAULT_SSH_TIMEOUT_MS);
        let ssh_timeout = Duration::from_millis(ssh_timeout);
        for addr in addrs {
            let Ok(addr) = TargetIpAddr::try_from(addr).map(Into::into) else {
                continue;
            };
            // Ensure there's a port
            let addr = ScopedSocketAddr::from_socket_addr(replace_default_port(addr))?;
            log::debug!("Trying to make a connection to {addr:?}");

            match try_get_target_info(&addr, context)
                .on_timeout(ssh_timeout, || {
                    Err(crate::KnockError::NonCriticalError(anyhow::anyhow!(
                        "knock_rcs() timed out"
                    )))
                })
                .await
            {
                Ok((product_config, board_config)) => {
                    return Ok(Self {
                        rcs_state: ffx::RemoteControlState::Up,
                        product_config,
                        board_config,
                        ssh_address: Some(addr.into()),
                    });
                }
                Err(crate::KnockError::NonCriticalError(e)) => {
                    log::debug!("Could not connect to {addr:?}: {e:?}");
                    continue;
                }
                e => {
                    log::debug!("Got error {e:?} when trying to connect to {addr:?}");
                    return Ok(Self {
                        rcs_state: ffx::RemoteControlState::Unknown,
                        product_config: None,
                        board_config: None,
                        ssh_address: None,
                    });
                }
            }
        }
        Ok(Self {
            rcs_state: ffx::RemoteControlState::Down,
            product_config: None,
            board_config: None,
            ssh_address: None,
        })
    }
}

async fn get_handle_info(
    handle: TargetHandle,
    context: &EnvironmentContext,
) -> Result<ffx::TargetInfo> {
    let mut serial_number = None;
    let (target_state, fastboot_interface, addresses) = match handle.state {
        TargetState::Unknown => (ffx::TargetState::Unknown, None, None),
        TargetState::Product { addrs: target_addrs, serial } => {
            serial_number = serial;
            (ffx::TargetState::Product, None, Some(target_addrs))
        }
        TargetState::Fastboot(state) => {
            serial_number.replace(state.serial_number);
            let (fastboot_connection, addresses) = match state.connection_state {
                FastbootConnectionState::Usb => (ffx::FastbootInterface::Usb, None),
                FastbootConnectionState::Tcp(addrs) => {
                    (ffx::FastbootInterface::Tcp, Some(addrs.into_iter().map(Into::into).collect()))
                }
                FastbootConnectionState::Udp(addrs) => {
                    (ffx::FastbootInterface::Udp, Some(addrs.into_iter().map(Into::into).collect()))
                }
            };
            (ffx::TargetState::Fastboot, Some(fastboot_connection), addresses)
        }
        TargetState::Zedboot => (ffx::TargetState::Zedboot, None, None),
    };
    let RetrievedTargetInfo { rcs_state, product_config, board_config, ssh_address } =
        if let Some(ref target_addrs) = addresses {
            RetrievedTargetInfo::get(context, target_addrs).await?
        } else {
            RetrievedTargetInfo::default()
        };
    let addresses =
        addresses.map(|ta| ta.into_iter().map(|x| x.into()).collect::<Vec<ffx::TargetAddrInfo>>());
    Ok(ffx::TargetInfo {
        nodename: handle.node_name,
        addresses,
        rcs_state: Some(rcs_state),
        target_state: Some(target_state),
        board_config,
        product_config,
        serial_number,
        fastboot_interface,
        ssh_address: ssh_address.map(|a| TargetIpAddr::from(*a).into()),
        ..Default::default()
    })
}

pub async fn resolve_target_query_to_info(
    query: impl Into<TargetInfoQuery>,
    ctx: &EnvironmentContext,
) -> Result<Vec<ffx::TargetInfo>> {
    let handles = resolve_target_query(query.into(), ctx).await?;
    let targets =
        join_all(handles.into_iter().map(|t| async { get_handle_info(t, ctx).await })).await;
    targets.into_iter().collect::<Result<Vec<ffx::TargetInfo>>>()
}

/// Attempts to resolve the query into a target's ssh-able address. It is an error
/// if it the query doesn't match exactly one target.
// Perhaps refactor as connect_to_target() -> Result<Connection>, since that seems
// to be the only way this function is used?
pub async fn resolve_target_address(
    target_spec: &TargetInfoQuery,
    ctx: &EnvironmentContext,
) -> Result<Resolution, FfxTargetError> {
    DefaultTargetResolver::default().resolve_target_address(target_spec, ctx).await
}

#[derive(Clone, Copy, Default)]
pub struct DefaultTargetResolver;

/// Return a stream of handles matching the query. If a target matches the query
/// exactly (i.e. the query is the full name of the target, not a substring),
/// return the handle immediately. Otherwise, run until the timeout (default 2
/// seconds, configurable via "discovery.timeout"). The contents of the stream
/// can be filtered by USB and mDNS.
pub fn get_discovery_stream(
    query: TargetInfoQuery,
    usb: bool,
    mdns: bool,
    no_cache: bool,
    ctx: &EnvironmentContext,
) -> Result<impl Stream<Item = TargetHandle>> {
    let mut sources =
        DiscoverySources::MANUAL | DiscoverySources::EMULATOR | DiscoverySources::FASTBOOT_FILE;
    if usb {
        sources = sources | DiscoverySources::USB_FASTBOOT;
    }
    if mdns {
        sources = sources | DiscoverySources::MDNS;
    }
    get_discovery_stream_with_sources(query, sources, no_cache, ctx.clone())
}

/// Return a list of handles matching the query. If a target matches the query
/// exactly (i.e. the query is the full name of the target, not a substring),
/// return the handle immediately. Otherwise, run until the timeout (default 2
/// seconds, configurable via "discovery.timeout"). The contents of the set can
/// be filtered by USB and mDNS.
pub async fn get_discovered_targets(
    query: TargetInfoQuery,
    usb: bool,
    mdns: bool,
    ctx: &EnvironmentContext,
) -> Result<Vec<TargetHandle>> {
    let mut sources =
        DiscoverySources::MANUAL | DiscoverySources::EMULATOR | DiscoverySources::FASTBOOT_FILE;
    if usb {
        sources = sources | DiscoverySources::USB_FASTBOOT;
    }
    if mdns {
        sources = sources | DiscoverySources::MDNS;
    }
    // Get nodename, in case we're trying to find an exact match
    get_discovered_targets_with_sources(query, sources, ctx.clone()).await
}

impl TargetResolver for DefaultTargetResolver {
    async fn discovered_targets(
        &self,
        query: TargetInfoQuery,
        ctx: EnvironmentContext,
    ) -> Result<Vec<TargetHandle>> {
        get_discovered_targets_with_sources(query, DiscoverySources::all(), ctx).await
    }

    async fn resolve_target_query(
        &self,
        query: TargetInfoQuery,
        ctx: &EnvironmentContext,
    ) -> Result<Vec<TargetHandle>> {
        let results: Vec<_> = self.discovered_targets(query, ctx.clone()).await?;
        log::debug!("discovery results: {results:?}");
        Ok(results)
    }

    async fn try_resolve_manual_target(
        &self,
        name: &str,
        ctx: &EnvironmentContext,
    ) -> Result<Option<Resolution>> {
        // This is something that is often mocked for testing. An improvement here would be to use the
        // environment context for locating manual targets.
        let finder = manual_targets::Config::default();
        let ssh_timeout: u64 = ctx.get(CONFIG_TARGET_SSH_TIMEOUT).unwrap_or(DEFAULT_SSH_TIMEOUT_MS);
        let ssh_timeout = Duration::from_millis(ssh_timeout);
        let mut res = None;
        for t in manual_targets::watcher::parse_manual_targets(&finder).await.into_iter() {
            let addr = t.addr();
            let mut resolution = Resolution::from_addr(addr);
            let identify = resolution
                .identify(ctx)
                .on_timeout(ssh_timeout, || {
                    Err(anyhow::anyhow!(
                        "timeout after {ssh_timeout:?} identifying manual target {t:?}"
                    ))
                })
                .await?;

            if identify.nodename == Some(String::from(name)) {
                res = Some(resolution);
            }
        }
        Ok(res)
    }
}

// Group the information collected when resolving the address. (This is
// particularly important for the rcs_proxy, which we may need when resolving
// a manual target -- we don't want make an RCS connection just to resolve the
// name, drop it, then re-establish it later.)
#[derive(Debug)]
enum ResolutionTarget {
    Addr(SocketAddr),
    Serial(String),
}

fn sort_socket_addrs(a1: &SocketAddr, a2: &SocketAddr) -> Ordering {
    match (a1.ip().is_link_local_addr(), a2.ip().is_link_local_addr()) {
        (true, true) | (false, false) => Ordering::Equal,
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
    }
}

fn choose_socketaddr_from_addresses(
    target: &TargetHandle,
    addresses: &Vec<TargetIpAddr>,
) -> Result<SocketAddr> {
    if addresses.is_empty() {
        bail!("Target discovered but does not contain addresses: {target:?}");
    }
    let mut addrs_sorted =
        addresses.into_iter().map(Into::into).sorted_by(sort_socket_addrs).collect::<Vec<_>>();
    let sock: SocketAddr = addrs_sorted.pop().ok_or_else(|| {
        anyhow!("Choosing a socketaddr from a list of addresses, must contain at least one address")
    })?;
    Ok(sock)
}

impl ResolutionTarget {
    // If the target is a product, pull out the "best" network address from the
    // target, and return it.
    fn from_target_handle(target: &TargetHandle) -> Result<ResolutionTarget> {
        match &target.state {
            TargetState::Product { addrs: addresses, .. } => {
                let sock = choose_socketaddr_from_addresses(
                    &target,
                    &addresses.into_iter().filter_map(|x| x.try_into().ok()).collect(),
                )?;
                let addr: SocketAddr = replace_default_port(sock);
                Ok(ResolutionTarget::Addr(addr))
            }
            TargetState::Fastboot(fts) => match &fts.connection_state {
                FastbootConnectionState::Usb => {
                    Ok(ResolutionTarget::Serial(fts.serial_number.clone()))
                }
                FastbootConnectionState::Tcp(addresses) => {
                    let sock: SocketAddr = choose_socketaddr_from_addresses(&target, addresses)?;
                    Ok(ResolutionTarget::Addr(sock))
                }
                FastbootConnectionState::Udp(addresses) => {
                    if addresses.is_empty() {
                        bail!("Target discovered but does not contain addresses: {target:?}");
                    }
                    let sock: SocketAddr = choose_socketaddr_from_addresses(&target, addresses)?;
                    Ok(ResolutionTarget::Addr(sock))
                }
            },
            state => {
                Err(anyhow::anyhow!("Target discovered but not in the correct state: {state:?}"))
            }
        }
    }

    fn to_spec(&self) -> String {
        match &self {
            ResolutionTarget::Addr(ssh_addr) => {
                format!("{ssh_addr}")
            }
            ResolutionTarget::Serial(serial) => {
                format!("serial:{serial}")
            }
        }
    }
}

#[derive(Debug)]
pub struct Resolution {
    target: ResolutionTarget,
    discovered: Option<TargetHandle>,
    pub connection: Option<Connection>,
    rcs_proxy: Option<RemoteControlProxy>,
    identify_host_response: Option<IdentifyHostResponse>,
}

impl Resolution {
    fn from_target(target: ResolutionTarget) -> Self {
        Self {
            target,
            discovered: None,
            connection: None,
            rcs_proxy: None,
            identify_host_response: None,
        }
    }
    fn from_addr(sa: SocketAddr) -> Self {
        let scope_id = if let SocketAddr::V6(addr) = sa { addr.scope_id() } else { 0 };
        let port = match sa.port() {
            0 => TARGET_DEFAULT_PORT,
            p => p,
        };
        let addr = TargetIpAddr::new(sa.ip(), scope_id, port).into();
        Self::from_target(ResolutionTarget::Addr(addr))
    }

    pub fn from_target_handle(th: TargetHandle) -> Result<Self> {
        let target = ResolutionTarget::from_target_handle(&th)?;
        Ok(Self { discovered: Some(th), ..Self::from_target(target) })
    }

    pub fn addr(&self) -> Result<SocketAddr> {
        match self.target {
            ResolutionTarget::Addr(addr) => Ok(addr),
            _ => bail!("target resolved to serial, not socket_addr"),
        }
    }

    pub async fn get_connection(&mut self, context: &EnvironmentContext) -> Result<&Connection> {
        if self.connection.is_none() {
            let connector = SshConnector::new(
                netext::ScopedSocketAddr::from_socket_addr(self.addr()?)?,
                context,
            )?;
            let conn = Connection::new(connector)
                .await
                .map_err(|e| crate::KnockError::CriticalError(e.into()))?;
            self.connection = Some(conn);
        }
        Ok(self.connection.as_ref().unwrap())
    }

    pub async fn get_rcs_proxy(
        &mut self,
        context: &EnvironmentContext,
    ) -> Result<&RemoteControlProxy> {
        if self.rcs_proxy.is_none() {
            let conn = self.get_connection(context).await?;
            self.rcs_proxy = Some(conn.rcs_proxy().await?);
        }
        Ok(self.rcs_proxy.as_ref().unwrap())
    }

    pub async fn identify(
        &mut self,
        context: &EnvironmentContext,
    ) -> Result<&IdentifyHostResponse> {
        if self.identify_host_response.is_none() {
            let rcs_proxy = self.get_rcs_proxy(context).await?;
            self.identify_host_response = Some(
                rcs_proxy
                    .identify_host()
                    .await?
                    .map_err(|e| anyhow::anyhow!("Error identifying host: {e:?}"))?,
            );
        }
        Ok(self.identify_host_response.as_ref().unwrap())
    }
}

impl TryFromEnvContext for Resolution {
    fn try_from_env_context<'a>(
        env: &'a EnvironmentContext,
    ) -> LocalBoxFuture<'a, ffx_command_error::Result<Self>> {
        Box::pin(async {
            let unspecified_target = UNSPECIFIED_TARGET_NAME.to_owned();
            let target_spec = get_target_specifier(env).await?;
            let target_spec_unwrapped = if env.is_strict() {
                target_spec.as_ref().ok_or(user_error!(
                    "You must specify a target via `-t <target_name>` before any command arguments"
                ))?
            } else {
                target_spec.as_ref().unwrap_or(&unspecified_target)
            };
            log::trace!("resolving target spec address from {}", target_spec_unwrapped);
            let spec: TargetInfoQuery = target_spec.into();
            let resolution = resolve_target_address(&spec, env)
                .await
                .map_err(|e| ffx_command_error::Error::User(NonFatalError(e.into()).into()))?;
            Ok(resolution)
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};

    #[fuchsia::test]
    async fn test_sort_socket_addrs() {
        struct TestCase {
            a1: SocketAddr,
            a2: SocketAddr,
            want_res: Ordering,
            name: String,
        }

        let cases: Vec<TestCase> = vec![
            TestCase {
                a1: "127.0.0.1:8082".parse().expect("Valid SocketAddr"),
                a2: "[::1]:8082".parse().expect("Valid SocketAddr"),
                want_res: Ordering::Equal,
                name: "Loopback v4 and v6 equal".to_string(),
            },
            TestCase {
                a1: "192.168.1.1:8082".parse().expect("Valid SocketAddr"),
                a2: "[2002:2::%2]:8082".parse().expect("Valid SocketAddr"),
                want_res: Ordering::Equal,
                name: "Non local v4 and non local v6 local equal".to_string(),
            },
            TestCase {
                // Local
                a1: SocketAddr::V6(SocketAddrV6::new(
                    Ipv6Addr::new(0xfe80, 0, 0, 0, 1, 6, 7, 8),
                    8080,
                    0,
                    1,
                )),
                // Non Local
                a2: SocketAddr::V6(SocketAddrV6::new(
                    Ipv6Addr::new(0x2607, 0xf8b0, 0x4005, 0x805, 0, 0, 0, 0x200e),
                    8080,
                    0,
                    1,
                )),
                want_res: Ordering::Less,
                name: "local v6 and non v6 local less".to_string(),
            },
            TestCase {
                // Local
                a1: SocketAddr::V6(SocketAddrV6::new(
                    Ipv6Addr::new(0xfe80, 0, 0, 0, 1, 6, 7, 8),
                    8080,
                    0,
                    1,
                )),
                // Local
                a2: SocketAddr::V6(SocketAddrV6::new(
                    Ipv6Addr::new(0xfe80, 0, 0, 0, 2, 6, 7, 8),
                    8080,
                    0,
                    1,
                )),
                want_res: Ordering::Equal,
                name: "local v6 and local v6 local equal".to_string(),
            },
        ];

        for case in cases {
            let got = sort_socket_addrs(&case.a1, &case.a2);
            assert_eq!(
                got, case.want_res,
                "TestCase: {0}. Results differ: {1:?}, {2:?}",
                case.name, got, case.want_res
            );
        }
    }

    fn get_addr_and_spec() -> (SocketAddr, TargetInfoQuery) {
        let addr = "127.0.0.1:123".to_string();
        let addr_spec =
            TargetInfoQuery::Addr(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 123));
        let sa = addr.parse::<SocketAddr>().unwrap();
        (sa, addr_spec)
    }

    #[fuchsia::test]
    async fn test_can_resolve_target_locally_addr() {
        let test_env = ffx_config::test_init().await.unwrap();
        let resolver = MockTargetResolver::new();
        // A network address will resolve to itself
        let (_, addr_spec) = get_addr_and_spec();
        // Note that this will fail if we try to call resolve_target_spec()
        // since we haven't mocked a return value. So it's also checking that no
        // resolution is done.
        let target_spec =
            locally_resolve_target_spec(&addr_spec.clone(), &resolver, &test_env.context)
                .await
                .unwrap();
        assert_eq!(target_spec, addr_spec.clone().into());
    }

    #[fuchsia::test]
    async fn test_can_resolve_target_locally_serial() {
        let test_env = ffx_config::test_init().await.unwrap();
        let resolver = MockTargetResolver::new();
        // A serial spec will resolve to itself
        let sn = "abcdef".to_string();
        let sn_spec = TargetInfoQuery::Serial(sn.clone());
        // Note that this will fail if we try to call resolve_target_spec()
        // since we still haven't mocked a return value. So it's also checking that no
        // resolution is done.
        let target_spec =
            locally_resolve_target_spec(&sn_spec.clone(), &resolver, &test_env.context)
                .await
                .unwrap();
        assert_eq!(target_spec, sn_spec.clone());
    }

    fn make_target_handle_for_product(name: &str, sa: SocketAddr) -> TargetHandle {
        let state = TargetState::Product { addrs: vec![sa.into()], serial: None };
        let th = TargetHandle { node_name: Some(String::from(name)), state, manual: false };
        th
    }

    #[fuchsia::test]
    async fn test_can_resolve_target_locally_dns() {
        let test_env = ffx_config::test_init().await.unwrap();
        let mut resolver = MockTargetResolver::new();
        // A DNS name will satisfy the resolution request
        let name = "foobar".to_string();
        let name_spec = TargetInfoQuery::NodenameOrSerial(name.clone());
        let (sa, addr_spec) = get_addr_and_spec();
        let th = make_target_handle_for_product(&name, sa);
        resolver.expect_try_resolve_manual_target().return_once(move |_, _| Ok(None));
        resolver.expect_discovered_targets().return_once(move |_, _| Ok(vec![th]));
        let target_spec =
            locally_resolve_target_spec(&name_spec.clone(), &resolver, &test_env.context)
                .await
                .unwrap();
        assert_eq!(target_spec, addr_spec);
    }

    #[fuchsia::test]
    async fn test_can_resolve_target_locally_serial_name() {
        let test_env = ffx_config::test_init().await.unwrap();
        let mut resolver = MockTargetResolver::new();
        // Test with "<serial>", _not_ "serial:<serial>"
        let sn = "abcdef".to_string();
        let sn_spec = TargetInfoQuery::Serial(sn.clone());
        let th = TargetHandle {
            node_name: None,
            state: TargetState::Fastboot(discovery::FastbootTargetState {
                serial_number: sn.clone(),
                connection_state: discovery::FastbootConnectionState::Usb,
            }),
            manual: false,
        };
        resolver.expect_try_resolve_manual_target().return_once(move |_, _| Ok(None));
        resolver.expect_discovered_targets().return_once(move |_, _| Ok(vec![th]));
        let target_spec =
            locally_resolve_target_spec(&(sn.clone().into()), &resolver, &test_env.context)
                .await
                .unwrap();
        assert_eq!(target_spec, sn_spec);
    }

    #[fuchsia::test]
    async fn test_can_resolve_target_locally_name_ambiguous() {
        let test_env = ffx_config::test_init().await.unwrap();
        let mut resolver = MockTargetResolver::new();
        // An ambiguous name will result in an error
        let name = "foobar".to_string();
        let (sa, _) = get_addr_and_spec();
        let th1 = make_target_handle_for_product(&name, sa);
        let th2 = make_target_handle_for_product(&name, sa);
        resolver.expect_try_resolve_manual_target().return_once(move |_, _| Ok(None));
        resolver.expect_discovered_targets().return_once(move |_, _| Ok(vec![th1, th2]));
        let target_spec_res =
            locally_resolve_target_spec(&("foo".to_string().into()), &resolver, &test_env.context)
                .await;
        assert!(target_spec_res.is_err());
        // XXX We should produce OpenTargetError::AmbiguousQuery, not DaemonError::TargetAmbiguous
        assert!(dbg!(target_spec_res.unwrap_err().to_string()).contains("multiple targets"));
    }

    #[fuchsia::test]
    async fn test_can_resolve_target_locally_first() {
        let test_env = ffx_config::test_init().await.unwrap();
        let mut resolver = MockTargetResolver::new();
        // A "first" query will satisfy the resolution request
        let first_spec = TargetInfoQuery::First;
        let (sa, addr_spec) = get_addr_and_spec();
        let th = make_target_handle_for_product("foo", sa);
        resolver.expect_try_resolve_manual_target().return_once(move |_, _| Ok(None));
        resolver.expect_discovered_targets().return_once(move |_, _| Ok(vec![th]));
        let target_spec =
            locally_resolve_target_spec(&first_spec, &resolver, &test_env.context).await.unwrap();
        assert_eq!(target_spec, addr_spec);
    }

    #[fuchsia::test]
    async fn test_can_resolve_target_locally_first_ambiguous() {
        let test_env = ffx_config::test_init().await.unwrap();
        let mut resolver = MockTargetResolver::new();
        // A "first" query will fail if there are multiple matches
        let name = "foobar".to_string();
        let (sa, _) = get_addr_and_spec();
        let th1 = make_target_handle_for_product(&name, sa);
        let th2 = make_target_handle_for_product(&name, sa);
        resolver.expect_try_resolve_manual_target().return_once(move |_, _| Ok(None));
        resolver.expect_discovered_targets().return_once(move |_, _| Ok(vec![th1, th2]));
        let first_spec = TargetInfoQuery::First;
        let target_spec_res =
            locally_resolve_target_spec(&first_spec, &resolver, &test_env.context).await;
        assert!(target_spec_res.is_err());
        assert!(dbg!(target_spec_res.unwrap_err().to_string()).contains("More than one device"));
    }

    // XXX Creating a reasonable test for the rest of the behavior:
    // * partial matching of names
    // * timing out when no matching targets return
    // * returning early when there is an exact name match
    #[fuchsia::test]
    async fn test_get_discovery_cache_dir() {
        let test_env = ffx_config::test_init().await.unwrap();
        let cache_dir = "/tmp/cache";
        test_env
            .context
            .query(DISCOVERY_CACHE_DIR_CONFIG)
            .level(Some(ffx_config::ConfigLevel::User))
            .set(serde_json::Value::String(cache_dir.to_string()))
            .unwrap();

        let result = get_discovery_cache_dir(&test_env.context);
        assert_eq!(result, Some(PathBuf::from(cache_dir)));
    }
}
