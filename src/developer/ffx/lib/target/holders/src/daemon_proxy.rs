// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::init_daemon_connection_behavior;
use anyhow::Context;
use async_trait::async_trait;
use discovery::query::TargetInfoQuery;
use errors::{ffx_bail, ffx_error};
use fdomain_client::fidl::DiscoverableProtocolMarker;
use fdomain_fuchsia_developer_remotecontrol::{
    RemoteControlMarker as FRemoteControlMarker, RemoteControlProxy as FRemoteControlProxy,
};
use ffx_command_error::{user_error, Error, FfxContext, Result};
use ffx_config::EnvironmentContext;
use ffx_core::{downcast_injector_error, FfxInjectorError, Injector};
use ffx_daemon::{get_daemon_proxy_single_link, is_daemon_running_at_path, DaemonConfig};
use ffx_target::fho::target_interface;
use ffx_target::{get_remote_proxy, open_target_with_fut};
use fho::{FhoEnvironment, TryFromEnv, TryFromEnvWith};
use fidl::endpoints::{Proxy, ServerEnd};
use fidl_fuchsia_developer_ffx::{DaemonError, DaemonProxy, TargetInfo, TargetProxy, VersionInfo};
use fidl_fuchsia_developer_remotecontrol::RemoteControlProxy;
use futures::FutureExt;
use std::future::Future;
use std::marker::PhantomData;
use std::ops::Deref;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use target_errors::FfxTargetError;
use timeout::timeout;
use {fdomain_fuchsia_io as fio_fdomain, fidl_fuchsia_developer_ffx as ffx_fidl};

#[derive(Clone, Debug)]
pub struct DaemonProxyHolder(ffx_fidl::DaemonProxy);

impl Deref for DaemonProxyHolder {
    type Target = ffx_fidl::DaemonProxy;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<ffx_fidl::DaemonProxy> for DaemonProxyHolder {
    fn from(value: ffx_fidl::DaemonProxy) -> Self {
        DaemonProxyHolder(value)
    }
}

#[async_trait(?Send)]
impl TryFromEnv for DaemonProxyHolder {
    async fn try_from_env(env: &FhoEnvironment) -> Result<Self> {
        let target_env = target_interface(env);
        if target_env.behavior().is_none() {
            let b = init_daemon_connection_behavior(env.environment_context()).await?;
            target_env.set_behavior(b)?;
        }
        // Might need to revisit whether it's necessary to cast every daemon_factory() invocation
        // into a user error. This line originally casted every error into "Failed to create daemon
        // proxy", which obfuscates the original error.
        target_env
            .injector::<Self>(env)
            .await?
            .daemon_factory()
            .await
            .map(Into::into)
            .map_err(|e| user_error!("{}", e))
    }
}

#[derive(Debug, Clone, Default)]
pub struct WithDaemonProtocol<P>(PhantomData<fn() -> P>);

#[async_trait(?Send)]
impl<P> TryFromEnvWith for WithDaemonProtocol<P>
where
    P: Proxy + Clone + 'static,
    P::Protocol: fidl::endpoints::DiscoverableProtocolMarker,
{
    type Output = P;
    async fn try_from_env_with(self, env: &FhoEnvironment) -> Result<P> {
        load_daemon_protocol(env).await
    }
}

/// A decorator for daemon proxies.
///
/// Example:
///
/// ```rust
/// #[derive(FfxTool)]
/// struct Tool {
///     #[with(fho::daemon_protocol())]
///     foo_proxy: FooProxy,
/// }
/// ```
pub fn daemon_protocol<P>() -> WithDaemonProtocol<P> {
    WithDaemonProtocol(Default::default())
}

async fn load_daemon_protocol<P>(env: &FhoEnvironment) -> Result<P>
where
    P: Proxy + Clone + 'static,
    P::Protocol: fidl::endpoints::DiscoverableProtocolMarker,
{
    let svc_name = <P::Protocol as fidl::endpoints::DiscoverableProtocolMarker>::PROTOCOL_NAME;
    let daemon = DaemonProxyHolder::try_from_env(env).await?;
    let (proxy, server_end) = create_proxy().bug_context("creating proxy")?;

    daemon
        .connect_to_protocol(svc_name, server_end.into_channel())
        .await
        .bug_context("Connecting to protocol")?
        .map_err(|err| Error::User(target_errors::map_daemon_error(svc_name, err)))?;

    Ok(proxy)
}

fn create_proxy<P>() -> Result<(P, ServerEnd<P::Protocol>)>
where
    P: Proxy + 'static,
    P::Protocol: fidl::endpoints::DiscoverableProtocolMarker,
{
    Ok(fidl::endpoints::create_proxy::<P::Protocol>())
}

/// Lock-protected contents of [ProxyState]
enum ProxyStateInner<T: Proxy + Clone> {
    Uninitialized,
    Initialized(T),
    Failed,
}

impl<T: Proxy + Clone> ProxyStateInner<T> {
    /// See [ProxyState::get_or_try_init]
    async fn get_or_try_init<F: Future<Output = anyhow::Result<T>>>(
        &mut self,
        mut f: impl FnMut(bool) -> F,
    ) -> anyhow::Result<T> {
        if matches!(self, ProxyStateInner::Uninitialized) {
            *self = ProxyStateInner::Initialized(f(true).await?)
        }
        match self {
            ProxyStateInner::Uninitialized => unreachable!(),
            ProxyStateInner::Initialized(x) if !x.is_closed() => Ok(x.clone()),
            _ => {
                *self = ProxyStateInner::Failed;
                let proxy = f(false).await?;
                *self = ProxyStateInner::Initialized(proxy.clone());
                Ok(proxy)
            }
        }
    }
}

/// Container for a FIDL proxy which can be initialized lazily, and which will
/// re-initialize when the proxy is closed if possible.
struct ProxyState<T: Proxy + Clone>(futures::lock::Mutex<ProxyStateInner<T>>);

impl<T: Proxy + Clone> Default for ProxyState<T> {
    fn default() -> Self {
        ProxyState(futures::lock::Mutex::new(ProxyStateInner::Uninitialized))
    }
}

impl<T: Proxy + Clone> ProxyState<T> {
    /// Gets the proxy contained in this [`ProxyState`]. If the proxy hasn't
    /// been set, *or* it is in a closed state, the closure will be called to
    /// get a future which will construct a new proxy with which to initialize.
    async fn get_or_try_init<F: Future<Output = anyhow::Result<T>>>(
        &self,
        f: impl FnMut(bool) -> F,
    ) -> anyhow::Result<T> {
        self.0.lock().await.get_or_try_init(f).await
    }
}

pub struct Injection {
    env_context: EnvironmentContext,
    version_info: ffx_build_version::VersionInfo,
    target_spec: Option<String>,
    node: Arc<overnet_core::Router>,
    daemon_once: ProxyState<DaemonProxy>,
    remote_once: ProxyState<RemoteControlProxy>,
    fdomain:
        Mutex<Option<(Arc<fdomain_client::Client>, Arc<Mutex<fidl_fuchsia_io::DirectoryProxy>>)>>,
}

pub const CONFIG_DAEMON_AUTOSTART: &str = "daemon.autostart";

impl std::fmt::Debug for Injection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Injection").finish()
    }
}

impl Injection {
    pub fn new(
        env_context: EnvironmentContext,
        version_info: ffx_build_version::VersionInfo,
        node: Arc<overnet_core::Router>,
        target_spec: Option<String>,
    ) -> Self {
        Self {
            env_context,
            version_info,
            node,
            target_spec,
            daemon_once: Default::default(),
            remote_once: Default::default(),
            fdomain: Mutex::new(None),
        }
    }

    pub async fn initialize_overnet(
        env_context: EnvironmentContext,
        router_interval: Option<Duration>,
        version_info: ffx_build_version::VersionInfo,
    ) -> ffx_command_error::Result<Injection> {
        log::debug!("Initializing Overnet");
        let node = overnet_core::Router::new(router_interval)
            .bug_context("Failed to initialize overnet")?;
        log::debug!("Getting target");
        let target_spec = ffx_target::get_target_specifier(&env_context).await?;
        log::debug!("Building Injection");
        Ok(Injection::new(env_context, version_info, node, target_spec))
    }

    async fn init_remote_proxy(
        &self,
        target_info: &mut Option<TargetInfo>,
    ) -> anyhow::Result<RemoteControlProxy> {
        let daemon_proxy = self.daemon_factory().await?;
        let target_spec: TargetInfoQuery = self.target_spec.clone().into();
        let proxy_timeout = self.env_context.get_proxy_timeout().await?;
        get_remote_proxy(
            &target_spec,
            daemon_proxy,
            proxy_timeout,
            Some(target_info),
            &self.env_context,
        )
        .await
    }

    async fn target_factory_inner(&self) -> anyhow::Result<TargetProxy> {
        // See if we need to do local resolution. (Do it here not in
        // open_target_with_fut because o_t_w_f is not async)
        let target_spec = ffx_target::maybe_locally_resolve_target_spec(
            &self.target_spec.clone().into(),
            &self.env_context,
        )
        .await?;
        let daemon_proxy = self.daemon_factory().await?;
        let (target_proxy, target_proxy_fut) = open_target_with_fut(
            &target_spec,
            daemon_proxy.clone(),
            self.env_context.get_proxy_timeout().await?,
        )?;
        target_proxy_fut.await?;
        Ok(target_proxy)
    }

    fn daemon_timeout_error(&self) -> FfxTargetError {
        FfxTargetError::DaemonError { err: DaemonError::Timeout, target: self.target_spec.clone() }
    }

    async fn daemon_factory_impl(
        &self,
        should_autostart: bool,
    ) -> anyhow::Result<DaemonProxy, FfxInjectorError> {
        downcast_injector_error(
            self.daemon_once
                .get_or_try_init(|first_connection| {
                    let start_mode = if should_autostart {
                        DaemonStart::AutoStart
                    } else {
                        DaemonStart::DoNotAutoStart
                    };
                    init_daemon_proxy(
                        start_mode,
                        Arc::clone(&self.node),
                        self.env_context.clone(),
                        self.version_info.clone(),
                        first_connection,
                    )
                })
                .await,
        )
    }

    async fn remote_factory_fdomain_inner(
        &self,
        toolbox: fidl_fuchsia_io::DirectoryProxy,
    ) -> anyhow::Result<FRemoteControlProxy> {
        let fdomain = {
            let mut fdomain = self.fdomain.lock().unwrap();
            if let Some((fdomain, proxy)) = &*fdomain {
                *proxy.lock().unwrap() = toolbox;
                fdomain.clone()
            } else {
                let toolbox = Arc::new(Mutex::new(toolbox));
                let client_toolbox = Arc::clone(&toolbox);
                let client = fdomain_local::local_client(move || {
                    let toolbox = Clone::clone(&*client_toolbox.lock().unwrap());

                    let (client, server) = fidl::endpoints::create_endpoints();
                    if let Err(error) = toolbox.open(
                        ".",
                        fidl_fuchsia_io::Flags::PROTOCOL_DIRECTORY,
                        &fidl_fuchsia_io::Options::default(),
                        server.into(),
                    ) {
                        log::debug!(error:?; "Could not open svc folder in toolbox namespace");
                    };
                    Ok(client)
                });

                fdomain.get_or_insert((client, toolbox)).0.clone()
            }
        };

        let namespace = fdomain.namespace().await?;
        let namespace =
            fdomain_client::fidl::ClientEnd::<fio_fdomain::DirectoryMarker>::new(namespace)
                .into_proxy();
        let (proxy, server_end) = fdomain.create_proxy::<FRemoteControlMarker>();
        namespace.open(
            FRemoteControlMarker::PROTOCOL_NAME,
            fio_fdomain::Flags::PROTOCOL_SERVICE,
            &fio_fdomain::Options::default(),
            server_end.into_channel(),
        )?;
        Ok(proxy)
    }
}

#[async_trait(?Send)]
impl Injector for Injection {
    async fn daemon_factory_force_autostart(
        &self,
    ) -> anyhow::Result<DaemonProxy, FfxInjectorError> {
        self.daemon_factory_impl(true).await
    }

    // This could get called multiple times by the plugin system via multiple threads - so make sure
    // the spawning only happens one thread at a time.
    async fn daemon_factory(&self) -> anyhow::Result<DaemonProxy, FfxInjectorError> {
        let should_autostart =
            self.env_context.query(CONFIG_DAEMON_AUTOSTART).get().unwrap_or(true);
        self.daemon_factory_impl(should_autostart).await
    }

    async fn try_daemon(&self) -> anyhow::Result<Option<DaemonProxy>> {
        let result = self
            .daemon_once
            .get_or_try_init(|first_connection| {
                init_daemon_proxy(
                    DaemonStart::DoNotAutoStart,
                    Arc::clone(&self.node),
                    self.env_context.clone(),
                    self.version_info.clone(),
                    first_connection,
                )
            })
            .await
            .ok();
        Ok(result)
    }

    async fn target_factory(&self) -> anyhow::Result<TargetProxy> {
        let timeout_error = self.daemon_timeout_error();
        let proxy_timeout = self.env_context.get_proxy_timeout().await?;
        // We pin this in order to avoid the compiler reporting "error: large
        // future with a size of 16600 bytes".
        Box::pin(timeout(proxy_timeout, self.target_factory_inner())).await.map_err(|_| {
            log::warn!("Timed out getting Target proxy for: {:?}", self.target_spec);
            timeout_error
        })?
    }

    async fn remote_factory(&self) -> anyhow::Result<RemoteControlProxy> {
        let timeout_error = self.daemon_timeout_error();
        // XXX Note: if we are doing local discovery, that will eat into this time.
        //     and if local discovery is _longer_ than the proxy timeout, we'll get
        //     a confusing error.
        let proxy_timeout = self.env_context.get_proxy_timeout().await?;
        // Use a RefCell to provide interior mutability across an await point
        let target_info = std::cell::RefCell::new(None);
        let proxy = Box::pin(timeout(proxy_timeout, async {
            self.remote_once
                .get_or_try_init(|_| async {
                    self.init_remote_proxy(&mut *target_info.borrow_mut()).await
                })
                .await
        }))
        .await
        .map_err(|_| {
            log::warn!("Timed out getting remote control proxy for: {:?}", self.target_spec);
            match target_info.borrow_mut().take() {
                Some(TargetInfo { nodename: Some(name), .. }) => {
                    FfxTargetError::DaemonError { err: DaemonError::Timeout, target: Some(name) }
                }
                _ => timeout_error,
            }
        })?;

        proxy
    }

    async fn remote_factory_fdomain(&self) -> anyhow::Result<FRemoteControlProxy> {
        let rcs = self.remote_factory().await?;
        let toolbox = rcs::toolbox::open_toolbox(&rcs).await?;

        self.remote_factory_fdomain_inner(toolbox).await
    }

    async fn is_experiment(&self, key: &str) -> bool {
        self.env_context.get(key).unwrap_or(false)
    }

    async fn build_info(&self) -> anyhow::Result<VersionInfo> {
        let version_info = ffx_build_version::build_info();
        let ffx_version_info = VersionInfo {
            commit_hash: version_info.commit_hash,
            commit_timestamp: version_info.commit_timestamp,
            build_version: version_info.build_version,
            abi_revision: version_info.abi_revision,
            api_level: version_info.api_level,
            exec_path: version_info.exec_path,
            build_id: version_info.build_id,
            ..Default::default()
        };

        Ok(ffx_version_info)
    }
}

#[derive(PartialEq, Debug, Eq)]
enum DaemonStart {
    AutoStart,
    DoNotAutoStart,
}

async fn init_daemon_proxy(
    autostart: DaemonStart,
    node: Arc<overnet_core::Router>,
    context: EnvironmentContext,
    version_info: ffx_build_version::VersionInfo,
    first_connection: bool,
) -> anyhow::Result<DaemonProxy> {
    let ascendd_path = context.get_ascendd_path().await?;

    if cfg!(not(test)) && !is_daemon_running_at_path(&ascendd_path) {
        if autostart == DaemonStart::DoNotAutoStart {
            return Err(FfxInjectorError::DaemonAutostartDisabled.into());
        }
        ffx_daemon::spawn_daemon(&context).await?;
    }

    log::debug!("Daemon available, establishing Overnet link");
    let (nodeid, proxy, link) =
        get_daemon_proxy_single_link(&node, ascendd_path.clone(), None).await?;

    // Spawn off the link task, so that FIDL functions can be called (link IO makes progress).
    let link_task = fuchsia_async::Task::local(link.map(|_| ()));

    let daemon_version_info = timeout(context.get_proxy_timeout().await?, proxy.get_version_info())
        .await
        .context("timeout")
        .map_err(|_| {
            ffx_error!(
                "ffx was unable to query the version of the running ffx daemon. \
                                 Run `ffx doctor --restart-daemon` and try again."
            )
        })?
        .context("Getting hash from daemon")?;

    // Check the version against the given comparison scheme.
    log::debug!("Checking daemon version: {version_info:?}");
    log::debug!("Daemon version info: {daemon_version_info:?}");
    let matched_proxy = !first_connection
        || (version_info.build_version == daemon_version_info.build_version
            && version_info.commit_hash == daemon_version_info.commit_hash
            && version_info.commit_timestamp == daemon_version_info.commit_timestamp);

    if matched_proxy {
        log::debug!("Found matching daemon version, using it.");
        link_task.detach();
        return Ok(proxy);
    }

    log::info!("Daemon is a different version, attempting to restart");

    // Tell the daemon to quit, and wait for the link task to finish.
    // TODO(raggi): add a timeout on this, if the daemon quit fails for some
    // reason, the link task would hang indefinitely.
    let (quit_result, _) = futures::future::join(proxy.quit(), link_task).await;

    if !quit_result.is_ok() {
        ffx_bail!(
            "ffx daemon upgrade failed unexpectedly. \n\
            Try running `ffx doctor --restart-daemon` and then retry your \
            command.\n\nError was: {:?}",
            quit_result
        )
    }

    if cfg!(not(test)) {
        ffx_daemon::spawn_daemon(&context).await?;
    }

    let (_nodeid, proxy, link) =
        get_daemon_proxy_single_link(&node, ascendd_path, Some(vec![nodeid])).await?;

    fuchsia_async::Task::local(link.map(|_| ())).detach();

    Ok(proxy)
}

#[cfg(test)]
mod test {
    use super::*;
    use async_lock::Mutex;
    use fidl::endpoints::{DiscoverableProtocolMarker, RequestStream, ServerEnd};
    use fidl_fuchsia_developer_ffx::{
        DaemonMarker, DaemonRequest, DaemonRequestStream, TargetCollectionMarker,
        TargetCollectionRequest, TargetCollectionRequestStream, TargetConnectionError,
        TargetMarker, TargetRequest,
    };
    use fidl_fuchsia_developer_remotecontrol::RemoteControlRequestStream;
    use fuchsia_async::Task;
    use futures::{AsyncReadExt, StreamExt, TryStreamExt};
    use netext::{TokioAsyncReadExt, UnixListenerStream};
    use std::path::PathBuf;
    use tokio::net::UnixListener;
    use vfs::directory::helper::DirectlyMutable;

    /// Retry a future until it succeeds or retries run out.
    async fn retry_with_backoff<E, F>(
        backoff0: Duration,
        max_backoff: Duration,
        mut f: impl FnMut() -> F,
    ) where
        F: futures::Future<Output = anyhow::Result<(), E>>,
        E: std::fmt::Debug,
    {
        let mut backoff = backoff0;
        loop {
            match f().await {
                Ok(()) => {
                    backoff = backoff0;
                }
                Err(e) => {
                    log::warn!("Operation failed: {:?} -- retrying in {:?}", e, backoff);
                    fuchsia_async::Timer::new(backoff).await;
                    backoff = std::cmp::min(backoff * 2, max_backoff);
                }
            }
        }
    }

    fn start_socket_link(node: Arc<overnet_core::Router>, sockpath: PathBuf) -> Task<()> {
        Task::spawn(async move {
            let ascendd_path = sockpath.clone();
            let node = Arc::clone(&node);
            retry_with_backoff(Duration::from_millis(100), Duration::from_secs(3), || async {
                ffx_daemon::run_single_ascendd_link(Arc::clone(&node), ascendd_path.clone()).await
            })
            .await
        })
    }

    #[fuchsia::test]
    async fn test_init_daemon_proxy_link_lost() {
        let test_env = ffx_config::test_init().await.expect("Failed to initialize test env");
        let sockpath = test_env.context.get_ascendd_path().await.expect("No ascendd path");

        // Start a listener that accepts and immediately closes the socket..
        let listener = UnixListener::bind(sockpath.to_owned()).unwrap();
        let _listen_task = Task::local(async move {
            loop {
                drop(listener.accept().await.unwrap());
            }
        });

        let res = init_daemon_proxy(
            DaemonStart::AutoStart,
            overnet_core::Router::new(None).unwrap(),
            test_env.context.clone(),
            test_version_info(),
            true,
        )
        .await;
        let str = format!("{}", res.err().unwrap());
        assert!(str.contains("link lost"));
        assert!(str.contains("ffx doctor"));
    }

    #[fuchsia::test]
    async fn test_init_daemon_proxy_timeout_no_connection() {
        let test_env = ffx_config::test_init().await.expect("Failed to initialize test env");
        let sockpath = test_env.context.get_ascendd_path().await.expect("No ascendd path");

        // Start a listener that never accepts the socket.
        let _listener = UnixListener::bind(sockpath.to_owned()).unwrap();

        let res = init_daemon_proxy(
            DaemonStart::AutoStart,
            overnet_core::Router::new(None).unwrap(),
            test_env.context.clone(),
            test_version_info(),
            true,
        )
        .await;
        let str = format!("{}", res.err().unwrap());
        assert!(str.contains("Timed out"));
        assert!(str.contains("ffx doctor"));
    }

    async fn test_daemon_custom<F, R>(
        local_node: Arc<overnet_core::Router>,
        sockpath: PathBuf,
        commit_hash: &str,
        sleep_secs: u64,
        handler: F,
    ) -> Task<()>
    where
        F: Fn(DaemonRequest) -> R + 'static,
        F::Output: Future<Output = anyhow::Result<(), fidl::Error>>,
    {
        let version_info =
            VersionInfo { commit_hash: Some(commit_hash.to_owned()), ..Default::default() };
        let daemon = overnet_core::Router::new(None).unwrap();
        let listener = UnixListener::bind(&sockpath).unwrap();
        let local_link_task = start_socket_link(Arc::clone(&local_node), sockpath.clone());

        let (sender, mut receiver) = futures::channel::mpsc::unbounded();
        daemon
            .register_service(DaemonMarker::PROTOCOL_NAME.into(), move |chan| {
                let _ = sender.unbounded_send(chan);
                Ok(())
            })
            .await
            .unwrap();

        let link_tasks = Arc::new(Mutex::new(Vec::<Task<()>>::new()));
        let link_tasks1 = link_tasks.clone();

        let listen_task = Task::local(async move {
            // let (sock, _addr) = listener.accept().await.unwrap();
            let mut stream = UnixListenerStream(listener);
            while let Some(sock) = stream.try_next().await.unwrap_or(None) {
                fuchsia_async::Timer::new(Duration::from_secs(sleep_secs)).await;
                let node_clone = Arc::clone(&daemon);
                link_tasks1.lock().await.push(Task::local(async move {
                    let (mut rx, mut tx) = sock.into_multithreaded_futures_stream().split();
                    ascendd::run_stream(node_clone, &mut rx, &mut tx)
                        .map(|r| eprintln!("link error: {:?}", r))
                        .await;
                }));
            }
        });

        // Now that we've completed setting up everything, return a task for the main loop
        // of the fake daemon.
        Task::local(async move {
            while let Some(chan) = receiver.next().await {
                let link_tasks = link_tasks.clone();
                let mut stream =
                    DaemonRequestStream::from_channel(fidl::AsyncChannel::from_channel(chan));
                while let Some(request) = stream.try_next().await.unwrap_or(None) {
                    match request {
                        DaemonRequest::GetVersionInfo { responder, .. } => {
                            responder.send(&version_info).unwrap()
                        }
                        DaemonRequest::Quit { responder, .. } => {
                            std::fs::remove_file(sockpath).unwrap();
                            listen_task.abort().await;
                            responder.send(true).unwrap();
                            // This is how long the daemon sleeps for, which
                            // is a workaround for the fact that we have no
                            // way to "flush" the response over overnet due
                            // to the constraints of mesh routing.
                            fuchsia_async::Timer::new(Duration::from_millis(20)).await;
                            link_tasks.lock().await.clear();
                            return;
                        }
                        _ => {
                            handler(request).await.unwrap();
                        }
                    }
                }
            }
            // Explicitly drop this in the task so it gets moved into it and isn't dropped
            // early.
            drop(local_link_task);
        })
    }

    async fn test_daemon(
        local_node: Arc<overnet_core::Router>,
        sockpath: PathBuf,
        commit_hash: &str,
        sleep_secs: u64,
    ) -> Task<()> {
        test_daemon_custom(local_node, sockpath, commit_hash, sleep_secs, |request| async move {
            panic!("unimplemented stub for request: {:?}", request);
        })
        .await
    }

    #[fuchsia::test]
    async fn test_init_daemon_proxy_hash_matches() {
        let test_env = ffx_config::test_init().await.expect("Failed to initialize test env");
        let sockpath = test_env.context.get_ascendd_path().await.expect("No ascendd path");
        let local_node = overnet_core::Router::new(None).unwrap();

        let sockpath1 = sockpath.to_owned();
        let local_node1 = Arc::clone(&local_node);
        let daemons_task =
            test_daemon(local_node1, sockpath1.to_owned(), "testcurrenthash", 0).await;

        let proxy = init_daemon_proxy(
            DaemonStart::AutoStart,
            local_node,
            test_env.context.clone(),
            test_version_info(),
            true,
        )
        .await
        .unwrap();
        proxy.quit().await.unwrap();
        daemons_task.await;
    }

    fn test_version_info() -> ffx_build_version::VersionInfo {
        ffx_build_version::VersionInfo {
            commit_hash: Some("testcurrenthash".to_owned()),
            ..Default::default()
        }
    }

    #[fuchsia::test]
    async fn test_init_daemon_proxy_upgrade() {
        let test_env = ffx_config::test_init().await.expect("Failed to initialize test env");
        let sockpath = test_env.context.get_ascendd_path().await.expect("No ascendd path");
        let local_node = overnet_core::Router::new(None).unwrap();

        let sockpath1 = sockpath.to_owned();
        let local_node1 = Arc::clone(&local_node);

        // Spawn two daemons, the first out of date, the second is up to date.
        // spawn the first daemon directly so we know it's all started up before we proceed
        let first_daemon =
            test_daemon(Arc::clone(&local_node1), sockpath1.to_owned(), "oldhash", 0).await;
        let daemons_task = Task::local(async move {
            // wait for the first daemon to exit before starting the second
            first_daemon.await;
            // Note: testcurrenthash is explicitly expected by #cfg in get_daemon_proxy
            // Note: The double awaits are because test_daemon is an async function that returns a task
            test_daemon(local_node1, sockpath1.to_owned(), "testcurrenthash", 0).await.await;
        });

        let proxy = init_daemon_proxy(
            DaemonStart::AutoStart,
            local_node,
            test_env.context.clone(),
            test_version_info(),
            true,
        )
        .await
        .unwrap();
        proxy.quit().await.unwrap();
        daemons_task.await;
    }

    #[fuchsia::test]
    async fn test_init_daemon_blocked_for_4s_succeeds() {
        let test_env = ffx_config::test_init().await.expect("Failed to initialize test env");
        let sockpath = test_env.context.get_ascendd_path().await.expect("No ascendd path");
        let local_node = overnet_core::Router::new(None).unwrap();

        // Spawn two daemons, the first out of date, the second is up to date.
        let sockpath1 = sockpath.to_owned();
        let local_node1 = Arc::clone(&local_node);
        let daemon_task =
            test_daemon(local_node1, sockpath1.to_owned(), "testcurrenthash", 4).await;

        let proxy = init_daemon_proxy(
            DaemonStart::AutoStart,
            local_node,
            test_env.context.clone(),
            test_version_info(),
            true,
        )
        .await
        .unwrap();
        proxy.quit().await.unwrap();
        daemon_task.await;
    }

    #[fuchsia::test]
    async fn test_init_daemon_blocked_for_long_timesout() {
        let test_env = ffx_config::test_init().await.expect("Failed to initialize test env");
        let sockpath = test_env.context.get_ascendd_path().await.expect("No ascendd path");
        let local_node = overnet_core::Router::new(None).unwrap();

        // Spawn two daemons, the first out of date, the second is up to date.
        let sockpath1 = sockpath.to_owned();
        let local_node1 = Arc::clone(&local_node);
        let _daemon_task =
            test_daemon(local_node1, sockpath1.to_owned(), "testcurrenthash", 16).await;

        let err = init_daemon_proxy(
            DaemonStart::AutoStart,
            local_node,
            test_env.context.clone(),
            test_version_info(),
            true,
        )
        .await;
        assert!(err.is_err());
        let str = format!("{:?}", err);
        assert!(str.contains("Timed out"));
        assert!(str.contains("ffx doctor"));
    }

    #[fuchsia::test]
    async fn test_remote_proxy_timeout() {
        let test_env = ffx_config::test_init().await.expect("Failed to initialize test env");
        let sockpath = test_env.context.get_ascendd_path().await.expect("No ascendd path");
        let local_node = overnet_core::Router::new(None).unwrap();

        fn start_target_task(target_handle: ServerEnd<TargetMarker>) -> Task<()> {
            let mut stream = target_handle.into_stream();

            Task::local(async move {
                while let Some(request) = stream.try_next().await.unwrap() {
                    match request {
                        TargetRequest::Identity { responder } => {
                            responder
                                .send(&TargetInfo {
                                    nodename: Some("target_name".into()),
                                    ..TargetInfo::default()
                                })
                                .unwrap();
                        }
                        // Hang forever to trigger a timeout
                        request @ TargetRequest::OpenRemoteControl { .. } => {
                            Task::local(async move {
                                let _request = request;
                                futures::future::pending::<()>().await;
                            })
                            .detach();
                        }
                        _ => panic!("unhandled: {request:?}"),
                    }
                }
            })
        }

        fn start_target_collection_task(channel: fidl::AsyncChannel) -> Task<()> {
            let mut stream = TargetCollectionRequestStream::from_channel(channel);

            Task::local(async move {
                while let Some(request) = stream.try_next().await.unwrap() {
                    eprintln!("{request:?}");
                    match request {
                        TargetCollectionRequest::OpenTarget {
                            query: _,
                            target_handle,
                            responder,
                        } => {
                            start_target_task(target_handle).detach();

                            responder.send(Ok(())).unwrap();
                        }
                        _ => panic!("unhandled: {request:?}"),
                    }
                }
            })
        }

        let daemon_request_handler = move |request| async move {
            match request {
                DaemonRequest::ConnectToProtocol { name, server_end, responder }
                    if name == TargetCollectionMarker::PROTOCOL_NAME =>
                {
                    start_target_collection_task(fidl::AsyncChannel::from_channel(server_end))
                        .detach();

                    responder.send(Ok(()))?;
                }
                _ => panic!("unhandled request: {request:?}"),
            }
            Ok(())
        };

        let sockpath1 = sockpath.to_owned();
        let local_node1 = Arc::clone(&local_node);
        test_daemon_custom(
            local_node1,
            sockpath1.to_owned(),
            "testcurrenthash",
            0,
            daemon_request_handler,
        )
        .await
        .detach();

        let injection = Injection::new(
            test_env.context.clone(),
            test_version_info(),
            local_node,
            Some("".into()),
        );

        let error = injection.remote_factory().await.unwrap_err();

        match error.downcast::<FfxTargetError>().unwrap() {
            FfxTargetError::DaemonError { err: DaemonError::Timeout, target } => {
                assert_eq!(target.as_deref(), Some(""));
            }
            err => panic!("Unexpected: {err}"),
        }
    }

    // These errors should ONLY be used with `test_rcs_connection_eventually_successful`.
    static ERRORS: std::sync::LazyLock<Arc<Mutex<Vec<TargetConnectionError>>>> =
        std::sync::LazyLock::new(|| Arc::new(Mutex::new(Vec::new())));

    #[fuchsia::test]
    async fn test_rcs_connection_eventually_successful() {
        let test_env = ffx_config::test_init().await.expect("Failed to initialize test env");
        let sockpath = test_env.context.get_ascendd_path().await.expect("No ascendd path");
        let local_node = overnet_core::Router::new(None).unwrap();
        fn start_target_task(
            target_handle: ServerEnd<TargetMarker>,
            errors: Arc<Mutex<Vec<TargetConnectionError>>>,
        ) -> Task<()> {
            let mut stream = target_handle.into_stream();

            let errors = errors.clone();
            Task::local(async move {
                while let Some(request) = stream.try_next().await.unwrap() {
                    match request {
                        TargetRequest::Identity { responder } => {
                            responder
                                .send(&TargetInfo {
                                    nodename: Some("target_name".into()),
                                    ..TargetInfo::default()
                                })
                                .unwrap();
                        }
                        TargetRequest::OpenRemoteControl { remote_control, responder } => {
                            if let Some(err) = errors.lock().await.pop() {
                                responder.send(Err(err)).unwrap();
                                continue;
                            }
                            Task::local(async move {
                                let mut stream = remote_control.into_stream();
                                while let Ok(Some(request)) = stream.try_next().await {
                                    eprintln!("Got a request for RCS proxy: {request:?}");
                                }
                            })
                            .detach();
                            responder.send(Ok(())).unwrap();
                        }
                        TargetRequest::GetSshLogs { responder } => responder.send("").unwrap(),
                        _ => {
                            eprintln!("unhandled request: {request:?}");
                            panic!("unhandled: {request:?}")
                        }
                    }
                }
            })
        }

        fn start_target_collection_task(channel: fidl::AsyncChannel) -> Task<()> {
            let errors = ERRORS.clone();
            let mut stream = TargetCollectionRequestStream::from_channel(channel);
            Task::local(async move {
                while let Some(request) = stream.try_next().await.unwrap() {
                    eprintln!("{request:?}");
                    match request {
                        TargetCollectionRequest::OpenTarget {
                            query: _,
                            target_handle,
                            responder,
                        } => {
                            start_target_task(target_handle, errors.clone()).detach();

                            responder.send(Ok(())).unwrap();
                        }
                        _ => panic!("unhandled: {request:?}"),
                    }
                }
            })
        }

        let daemon_request_handler = move |request| async move {
            match request {
                DaemonRequest::ConnectToProtocol { name, server_end, responder }
                    if name == TargetCollectionMarker::PROTOCOL_NAME =>
                {
                    start_target_collection_task(fidl::AsyncChannel::from_channel(server_end))
                        .detach();
                    responder.send(Ok(()))?;
                }
                _ => panic!("unhandled request: {request:?}"),
            }
            Ok(())
        };

        let sockpath1 = sockpath.to_owned();
        let local_node1 = Arc::clone(&local_node);
        test_daemon_custom(
            local_node1,
            sockpath1.to_owned(),
            "testcurrenthash",
            0,
            daemon_request_handler,
        )
        .await
        .detach();

        let injection = Injection::new(
            test_env.context.clone(),
            test_version_info(),
            local_node,
            Some("".into()),
        );
        let error_list = [
            TargetConnectionError::Timeout,
            TargetConnectionError::Timeout,
            TargetConnectionError::ConnectionRefused,
        ];
        ERRORS.lock().await.extend_from_slice(&error_list[..]);
        let mut target_info = None;
        assert!(injection.init_remote_proxy(&mut target_info).await.is_ok());
        // We should also get the target info here.
        assert_eq!(target_info.unwrap().nodename.unwrap(), "target_name".to_owned());

        let error_list = [
            TargetConnectionError::Timeout,
            TargetConnectionError::KeyVerificationFailure,
            TargetConnectionError::Timeout,
            TargetConnectionError::ConnectionRefused,
        ];
        ERRORS.lock().await.extend_from_slice(&error_list[..]);
        let mut target_info = None;
        let res = injection.init_remote_proxy(&mut target_info).await;
        assert!(res.is_err());
        let err = res.unwrap_err().downcast::<FfxTargetError>().unwrap();
        let FfxTargetError::TargetConnectionError { err, .. } = err else {
            panic!("Unexpected error: {err:?}");
        };
        assert_eq!(err, TargetConnectionError::KeyVerificationFailure);
        // We should still get the target info even during failure.
        assert_eq!(target_info.unwrap().nodename.unwrap(), "target_name".to_owned());
    }

    #[fuchsia::test]
    async fn test_rcs_connection_fdomain() {
        let test_env = ffx_config::test_init().await.expect("Failed to initialize test env");
        let sockpath = test_env.context.get_ascendd_path().await.expect("No ascendd path");
        let local_node = overnet_core::Router::new(None).unwrap();
        fn start_target_task(target_handle: ServerEnd<TargetMarker>) -> Task<()> {
            let mut stream = target_handle.into_stream();

            Task::local(async move {
                while let Some(request) = stream.try_next().await.unwrap() {
                    match request {
                        TargetRequest::Identity { responder } => {
                            responder
                                .send(&TargetInfo {
                                    nodename: Some("target_name".into()),
                                    ..TargetInfo::default()
                                })
                                .unwrap();
                        }
                        TargetRequest::OpenRemoteControl { remote_control, responder } => {
                            Task::local(async move {
                                let mut stream = remote_control.into_stream();
                                while let Ok(Some(request)) = stream.try_next().await {
                                    eprintln!("Got a request for RCS proxy: {request:?}");
                                }
                            })
                            .detach();
                            responder.send(Ok(())).unwrap();
                        }
                        TargetRequest::GetSshLogs { responder } => responder.send("").unwrap(),
                        _ => {
                            eprintln!("unhandled request: {request:?}");
                            panic!("unhandled: {request:?}")
                        }
                    }
                }
            })
        }

        fn start_target_collection_task(channel: fidl::AsyncChannel) -> Task<()> {
            let mut stream = TargetCollectionRequestStream::from_channel(channel);
            Task::local(async move {
                while let Some(request) = stream.try_next().await.unwrap() {
                    eprintln!("{request:?}");
                    match request {
                        TargetCollectionRequest::OpenTarget {
                            query: _,
                            target_handle,
                            responder,
                        } => {
                            start_target_task(target_handle).detach();

                            responder.send(Ok(())).unwrap();
                        }
                        _ => panic!("unhandled: {request:?}"),
                    }
                }
            })
        }

        let daemon_request_handler = move |request| async move {
            match request {
                DaemonRequest::ConnectToProtocol { name, server_end, responder }
                    if name == TargetCollectionMarker::PROTOCOL_NAME =>
                {
                    start_target_collection_task(fidl::AsyncChannel::from_channel(server_end))
                        .detach();
                    responder.send(Ok(()))?;
                }
                _ => panic!("unhandled request: {request:?}"),
            }
            Ok(())
        };

        let sockpath1 = sockpath.to_owned();
        let local_node1 = Arc::clone(&local_node);
        test_daemon_custom(
            local_node1,
            sockpath1.to_owned(),
            "testcurrenthash",
            0,
            daemon_request_handler,
        )
        .await
        .detach();

        let injection = Injection::new(
            test_env.context.clone(),
            test_version_info(),
            local_node,
            Some("".into()),
        );

        let dir = vfs::directory::immutable::simple();
        dir.add_entry(
            "fuchsia.developer.remotecontrol.RemoteControl",
            vfs::service::host(|mut request_stream: RemoteControlRequestStream| async move {
                while let Ok(Some(request)) = request_stream.try_next().await {
                    eprintln!("Got a request for RCS proxy: {request:?}");
                }
            }),
        )
        .unwrap();
        let dir_proxy = vfs::directory::serve_read_only(Arc::clone(&dir));
        assert!(injection.remote_factory_fdomain_inner(dir_proxy).await.is_ok());
    }
}
