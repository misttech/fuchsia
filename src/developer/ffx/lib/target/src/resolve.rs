// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use addr::TargetIpAddr;
use anyhow::{Context, Result, anyhow, bail};
use discovery::query::TargetInfoQuery;
use discovery::{
    DiscoveryBuilder, DiscoverySources, FastbootConnectionState, TargetDiscovery, TargetEvent,
    TargetHandle, TargetState,
};
use ffx_command_error::{NonFatalError, user_error};
use ffx_config::{EnvironmentContext, TryFromEnvContext};
use fidl_fuchsia_developer_ffx::{self as ffx};
use fidl_fuchsia_developer_remotecontrol::{IdentifyHostResponse, RemoteControlProxy};
use fuchsia_async::TimeoutExt;
use futures::future::{LocalBoxFuture, join_all};
use futures::{FutureExt, StreamExt, pin_mut};
use itertools::Itertools;
use netext::{IsLocalAddr, ScopedSocketAddr};
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;
use target_errors::FfxTargetError;

use crate::connection::Connection;
use crate::ssh_connector::SshConnector;
use crate::{UNSPECIFIED_TARGET_NAME, get_target_specifier};

const CONFIG_TARGET_SSH_TIMEOUT: &str = "target.host_pipe_ssh_timeout";
const CONFIG_LOCAL_DISCOVERY_TIMEOUT: &str = "discovery.timeout";
const TARGET_DEFAULT_PORT: u16 = 22;
const DEFAULT_SSH_TIMEOUT_MS: u64 = 10000;

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
pub trait TargetResolver {
    /// Sets the discovery sources of the resolver.
    fn with_sources(sources: DiscoverySources) -> Self;

    /// Gets the discovery sources of the resolver.
    fn sources(&self) -> DiscoverySources;

    /// Gets a stream of handles from the resolver's sources that match the query
    fn discovery_stream(
        &self,
        query: TargetInfoQuery,
        ctx: EnvironmentContext,
    ) -> Result<impl futures::Stream<Item = TargetHandle>>;

    /// Resolves a `TargetInfoQuery` into a list of matching `TargetHandle`s.
    ///
    /// This method is expected to perform discovery and return all targets
    /// that match the given query.
    #[allow(async_fn_in_trait)]
    async fn resolve_target_query(
        &self,
        query: TargetInfoQuery,
        ctx: &EnvironmentContext,
    ) -> Result<Vec<TargetHandle>>;

    /// Attempts to resolve a target by name from a manually configured list.
    ///
    /// This method is used to check for targets that have been explicitly
    /// defined by the user, outside of the standard discovery mechanisms.
    #[allow(async_fn_in_trait)]
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
    #[allow(async_fn_in_trait)]
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
    #[allow(async_fn_in_trait)]
    async fn resolve_single_target(
        &self,
        target_spec: &TargetInfoQuery,
        env_context: &EnvironmentContext,
    ) -> Result<Resolution, FfxTargetError> {
        if let TargetInfoQuery::Addr(a) = target_spec {
            return Ok(Resolution::from_addr(*a));
        }

        let handles_stream = self
            .discovery_stream(target_spec.clone(), env_context.clone())
            .map_err(|_| FfxTargetError::OpenTargetError {
                err: ffx::OpenTargetError::FailedDiscovery,
                target: Some(target_spec.into()),
            })?;
        pin_mut!(handles_stream);
        let mut discovered: Option<TargetHandle> = None;

        // If the query is not specified, we won't even bother with trying to resolve a manual target.
        if let TargetInfoQuery::NodenameOrSerial(ref s) = target_spec {
            // We want to query both the manual targets and the discoverable handles concurrently.
            let manual_target_fut = self.try_resolve_manual_target(s, env_context).fuse();
            pin_mut!(manual_target_fut);
            let next_handle_fut = handles_stream.next().fuse();
            pin_mut!(next_handle_fut);
            futures::select! {
                mtr = &mut manual_target_fut => match mtr {
                    Err(e) => {
                        log::debug!("Failed to resolve target {s} as manual target: {e:?}");
                        // Keep going, waiting for the discovery to complete
                    }
                    Ok(Some(res)) => return Ok(res), // We found a manual target, so we're done
                    _ => (), // No manual target with this name
                },
                handle_res = next_handle_fut => {
                    // We got a response from discovery
                    if let Some(handle) = handle_res {
                        discovered = Some(handle);
                    }
                },
            }
        }
        // Keep on reading (or start reading, if the target wasn't specified)
        while let Some(handle) = handles_stream.next().await {
            // If we've already discovered one, then fail, since our query is ambiguous
            if discovered.is_some() {
                return Err(FfxTargetError::OpenTargetError {
                    err: ffx::OpenTargetError::QueryAmbiguous,
                    target: Some(target_spec.into()),
                });
            } else {
                discovered = Some(handle);
            }
        }
        match discovered {
            Some(handle) => Ok(Resolution::from_target_handle(handle).map_err(|_| {
                // The conversion will fail if it is a fastboot device with no addresses, or
                // the target is in the wrong state. Roughly, we can consider that to be that
                // we couldn't find a valid target.
                FfxTargetError::OpenTargetError {
                    err: ffx::OpenTargetError::TargetNotFound,
                    target: Some(target_spec.into()),
                }
            })?),
            None => Err(FfxTargetError::OpenTargetError {
                err: ffx::OpenTargetError::TargetNotFound,
                target: Some(target_spec.into()),
            }),
        }
    }
}

// Used for testing only. Must be pub (and not cfg(test)) so it is available
// from other crates.
pub mod mock_stream {
    use discovery::TargetHandle;
    // Used in tests
    #[allow(dead_code)]
    pub struct MockHandleStream(pub Vec<TargetHandle>);
    impl futures::Stream for MockHandleStream {
        type Item = super::TargetHandle;

        // Return each handle once, then return None to close the stream.
        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            let next = if self.0.is_empty() { None } else { Some(self.0.remove(0)) };
            std::task::Poll::Ready(next)
        }
    }
}

#[cfg(test)]
mock! {
    TargetResolver{}
    impl TargetResolver for TargetResolver {
        fn with_sources(sources: DiscoverySources) -> Self;
        fn sources(&self) -> DiscoverySources;
        #[allow(refining_impl_trait)]
        fn discovery_stream(
            &self,
            query: TargetInfoQuery,
            ctx: EnvironmentContext,
        ) -> Result<crate::resolve::mock_stream::MockHandleStream>;

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
    resolve_target_query_with_sources(
        query,
        ctx,
        DiscoverySources::MDNS
            | DiscoverySources::USB_FASTBOOT
            | DiscoverySources::MANUAL
            | DiscoverySources::EMULATOR
            | DiscoverySources::FASTBOOT_FILE,
    )
    .await
}

// Return a Stream of TargetHandles that come from the specified sources,
// and match the query. We close the stream early if a result "perfectly"
// matches the query (e.g. an mDNS response with the exact name requested).
// Otherwise, we wait for CONFIG_LOCAL_DISCOVERY_TIMEOUT before closing the
// stream.
// Note that we implement the check for a perfect match here, because all
// callers (e.g. resolve_target_query(), `ffx target list`'s get_handle_stream())
// all benefit from this behavior. If we end up with a caller that wants to provide
// a query but does _not_ want to terminate the stream early, we can refactor this
// code.
fn get_discovery_stream_with_sources(
    query: TargetInfoQuery,
    sources: DiscoverySources,
    ctx: EnvironmentContext,
) -> Result<impl futures::Stream<Item = TargetHandle>> {
    let query_clone = query.clone();
    let filter = move |handle: &TargetHandle| query_clone.match_handle(handle);
    // Note that if there is an error getting these two config options, they
    // will simply be ignored. The alternative is to throw an error, which,
    // e.g. will cause ffx-strict to fail under certain circumstances if either
    // default config option is not overridden.
    let emu_instance_root = ctx.get(emulator_instance::EMU_INSTANCE_ROOT_DIR).ok();
    let fastboot_file_path = ctx.get(fastboot_file_discovery::FASTBOOT_FILE_PATH).ok();
    let discovery_delay = ctx.get(CONFIG_LOCAL_DISCOVERY_TIMEOUT).unwrap_or(2000);
    let delay = Duration::from_millis(discovery_delay);
    let discovery = DiscoveryBuilder::default()
        .with_source(sources)
        .with_fastboot_devices_file_path(fastboot_file_path)
        .with_emulator_instance_root(emu_instance_root)
        .notify_added(true)
        .build();
    let stream = discovery.discover_devices(filter)?;

    // This is tricky. We want the stream to complete immediately if we find
    // a target whose name/serial matches the query exactly. Otherwise, run
    // until the timer fires.
    // We can't use `Stream::take_until()`, because that would require us
    // to return true for the found item, and false for the _next_ item.
    // But there may be no next item, so the stream would end up waiting
    // for the timer anyway. Instead, we create two futures: the timer, and
    // one that is ready when we find the target we're looking for. Then we
    // use `Stream::take_until()`, waiting until _either_ of those futures
    // is ready (by using `race()`). The only remaining tricky part is that
    // we need to examine each event to determine if it matches what we're
    // looking for -- so we interpose a closure via `Stream::map()` that
    // examines each item, before returning them unmodified.
    // We could stop the race early in case of failure by using the same
    // technique, I suppose.
    let timer = fuchsia_async::Timer::new(delay).fuse();
    let found_target_event = async_utils::event::Event::new();
    let found_it = found_target_event.wait().fuse();
    // We can see the same handle multiple times (e.g. if it produces multiple
    // mDNS events during our timeout period). "seen" lets us dedup those
    // handles.
    let seen = Rc::new(RefCell::new(HashSet::new()));
    Ok(stream
        .filter_map(move |ev| {
            let found_ev = found_target_event.clone();
            let q_clone = query.clone();
            let seen = seen.clone();
            async move {
                match ev {
                    TargetEvent::Added(ref h) => {
                        if seen.borrow().contains(h) {
                            None
                        } else {
                            // We don't want to match against First, since if we did, we'd
                            // never be able to report an ambiguous target error.
                            if !matches!(q_clone, TargetInfoQuery::First) {
                                if q_clone.match_handle(h) {
                                    log::debug!(
                                        "Signaling early as discovered target matches query"
                                    );
                                    found_ev.signal();
                                }
                            }
                            seen.borrow_mut().insert(h.clone());
                            Some((*h).clone())
                        }
                    }
                    // We've only asked for Added events
                    _ => unreachable!(),
                }
            }
        })
        .take_until(futures_lite::future::race(timer, found_it)))
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

pub async fn resolve_target_query_with(
    query: TargetInfoQuery,
    ctx: &EnvironmentContext,
    usb: bool,
    mdns: bool,
) -> Result<Vec<TargetHandle>> {
    let mut sources =
        DiscoverySources::MANUAL | DiscoverySources::EMULATOR | DiscoverySources::FASTBOOT_FILE;
    if usb {
        sources = sources | DiscoverySources::USB_FASTBOOT;
    }
    if mdns {
        sources = sources | DiscoverySources::MDNS;
    }
    resolve_target_query_with_sources(query, ctx, sources).await
}

pub async fn resolve_target_query_with_sources(
    query: TargetInfoQuery,
    ctx: &EnvironmentContext,
    sources: DiscoverySources,
) -> Result<Vec<TargetHandle>> {
    log::debug!("Resolving query: {:#?} with sources: {:#?}", query, sources);
    // Get nodename, in case we're trying to find an exact match
    DefaultTargetResolver::with_sources(sources).resolve_target_query(query, ctx).await
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

#[derive(Clone, Copy)]
pub struct DefaultTargetResolver {
    sources: DiscoverySources,
}

impl Default for DefaultTargetResolver {
    fn default() -> Self {
        Self::with_sources(DiscoverySources::all())
    }
}

/// Return a stream of handles matching the query. If a target
/// matches the query exactly (i.e. the query is the full name
/// of the target, not a substring), return the handle immediately
/// and close the stream. Otherwise, run until the timeout
/// (default 2 seconds, configurable via "discovery.timeout").
/// The contents of the stream can be filtered by USB and mDNS.
pub async fn get_discovery_stream(
    query: TargetInfoQuery,
    usb: bool,
    mdns: bool,
    ctx: &EnvironmentContext,
) -> Result<impl futures::Stream<Item = TargetHandle>> {
    let mut sources =
        DiscoverySources::MANUAL | DiscoverySources::EMULATOR | DiscoverySources::FASTBOOT_FILE;
    if usb {
        sources = sources | DiscoverySources::USB_FASTBOOT;
    }
    if mdns {
        sources = sources | DiscoverySources::MDNS;
    }
    // Get nodename, in case we're trying to find an exact match
    get_discovery_stream_with_sources(query, sources, ctx.clone())
}

impl TargetResolver for DefaultTargetResolver {
    fn with_sources(sources: DiscoverySources) -> Self {
        Self { sources }
    }

    fn sources(&self) -> DiscoverySources {
        self.sources
    }

    fn discovery_stream(
        &self,
        query: TargetInfoQuery,
        ctx: EnvironmentContext,
    ) -> Result<impl futures::Stream<Item = TargetHandle>> {
        get_discovery_stream_with_sources(query, self.sources, ctx)
    }

    async fn resolve_target_query(
        &self,
        query: TargetInfoQuery,
        ctx: &EnvironmentContext,
    ) -> Result<Vec<TargetHandle>> {
        let results: Vec<_> = self.discovery_stream(query, ctx.clone())?.collect().await;
        log::debug!("discovery stream results: {results:?}");
        Ok(results)
    }

    #[allow(async_fn_in_trait)]
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
        resolver.expect_sources().return_once(move || DiscoverySources::all());
        resolver.expect_try_resolve_manual_target().return_once(move |_, _| Ok(None));
        resolver
            .expect_discovery_stream()
            .return_once(move |_, _| Ok(mock_stream::MockHandleStream(vec![th])));
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
        resolver.expect_sources().return_once(move || DiscoverySources::all());
        resolver.expect_try_resolve_manual_target().return_once(move |_, _| Ok(None));
        resolver
            .expect_discovery_stream()
            .return_once(move |_, _| Ok(mock_stream::MockHandleStream(vec![th])));
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
        resolver.expect_sources().return_once(move || DiscoverySources::all());
        resolver.expect_try_resolve_manual_target().return_once(move |_, _| Ok(None));
        resolver
            .expect_discovery_stream()
            .return_once(move |_, _| Ok(mock_stream::MockHandleStream(vec![th1, th2])));
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
        resolver.expect_sources().return_once(move || DiscoverySources::all());
        resolver.expect_try_resolve_manual_target().return_once(move |_, _| Ok(None));
        resolver
            .expect_discovery_stream()
            .return_once(move |_, _| Ok(mock_stream::MockHandleStream(vec![th])));
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
        resolver.expect_sources().return_once(move || DiscoverySources::all());
        resolver.expect_try_resolve_manual_target().return_once(move |_, _| Ok(None));
        resolver
            .expect_discovery_stream()
            .return_once(move |_, _| Ok(mock_stream::MockHandleStream(vec![th1, th2])));
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
    // requires mocking a Stream<Item=TargetEvent>, which is difficult since a
    // trait for returning such a stream can't be made since the items are not ?
    // Sized (according to the rust compiler). So these additional tests will
    // require some more work.
}
