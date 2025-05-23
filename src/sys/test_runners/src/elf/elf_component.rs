// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::SuiteServer;
use crate::errors::ArgumentError;
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use fidl::endpoints::{create_proxy, ClientEnd, ProtocolMarker, Proxy, ServerEnd};
use fidl_fuchsia_ldsvc::LoaderMarker;
use fidl_fuchsia_test_runner::{
    LibraryLoaderCacheBuilderMarker, LibraryLoaderCacheMarker, LibraryLoaderCacheProxy,
};
use fuchsia_async::{self as fasync, TimeoutExt};
use fuchsia_component::client::connect_to_protocol;
use fuchsia_component::server::ServiceFs;
use fuchsia_runtime::job_default;
use futures::future::{abortable, BoxFuture};
use futures::prelude::*;
use log::{error, info, warn};
use namespace::Namespace;
use runner::component::StopInfo;
use std::mem;
use std::ops::Deref;
use std::path::Path;
use std::sync::{Arc, Mutex};
use thiserror::Error;
use vfs::execution_scope::ExecutionScope;
use vfs::file::vmo::read_only;
use vfs::tree_builder::TreeBuilder;
use zx::{self as zx, AsHandleRef, HandleBased, Task};
use {
    fidl_fuchsia_component as fcomponent, fidl_fuchsia_component_runner as fcrunner,
    fidl_fuchsia_io as fio,
};

static PKG_PATH: &'static str = "/pkg";

// Maximum time that the runner will wait for break_on_start eventpair to signal.
// This is set to prevent debuggers from blocking us for too long, either intentionally
// or unintentionally.
const MAX_WAIT_BREAK_ON_START: zx::MonotonicDuration = zx::MonotonicDuration::from_millis(300);

/// Error encountered running test component
#[derive(Debug, Error)]
pub enum ComponentError {
    #[error("start info is missing resolved url")]
    MissingResolvedUrl,

    #[error("error for test {}: {:?}", _0, _1)]
    InvalidArgs(String, anyhow::Error),

    #[error("Cannot run test {}, no namespace was supplied.", _0)]
    MissingNamespace(String),

    #[error("Cannot run test {}, as no outgoing directory was supplied.", _0)]
    MissingOutDir(String),

    #[error("Cannot run test {}, as no runtime directory was supplied.", _0)]
    MissingRuntimeDir(String),

    #[error("Cannot run test {}, as no /pkg directory was supplied.", _0)]
    MissingPkg(String),

    #[error("Cannot load library for {}: {}.", _0, _1)]
    LibraryLoadError(String, anyhow::Error),

    #[error("Cannot load executable binary '{}': {}", _0, _1)]
    LoadingExecutable(String, anyhow::Error),

    #[error("Cannot create vmo child for test {}: {}", _0, _1)]
    VmoChild(String, anyhow::Error),

    #[error("Cannot run suite server: {:?}", _0)]
    ServeSuite(anyhow::Error),

    #[error("Cannot serve runtime directory: {:?}", _0)]
    ServeRuntimeDir(anyhow::Error),

    #[error("{}: {:?}", _0, _1)]
    Fidl(String, fidl::Error),

    #[error("cannot create job: {:?}", _0)]
    CreateJob(zx::Status),

    #[error("Cannot set config vmo: {:?}", _0)]
    ConfigVmo(anyhow::Error),

    #[error("cannot create channel: {:?}", _0)]
    CreateChannel(zx::Status),

    #[error("cannot duplicate job: {:?}", _0)]
    DuplicateJob(zx::Status),

    #[error("invalid url")]
    InvalidUrl,
}

impl ComponentError {
    /// Convert this error into its approximate `fuchsia.component.Error` equivalent.
    pub fn as_zx_status(&self) -> zx::Status {
        let status = match self {
            Self::MissingResolvedUrl => fcomponent::Error::InvalidArguments,
            Self::InvalidArgs(_, _) => fcomponent::Error::InvalidArguments,
            Self::MissingNamespace(_) => fcomponent::Error::InvalidArguments,
            Self::MissingOutDir(_) => fcomponent::Error::InvalidArguments,
            Self::MissingRuntimeDir(_) => fcomponent::Error::InvalidArguments,
            Self::MissingPkg(_) => fcomponent::Error::InvalidArguments,
            Self::LibraryLoadError(_, _) => fcomponent::Error::Internal,
            Self::LoadingExecutable(_, _) => fcomponent::Error::InstanceCannotStart,
            Self::VmoChild(_, _) => fcomponent::Error::Internal,
            Self::ServeSuite(_) => fcomponent::Error::Internal,
            Self::ServeRuntimeDir(_) => fcomponent::Error::Internal,
            Self::Fidl(_, _) => fcomponent::Error::Internal,
            Self::CreateJob(_) => fcomponent::Error::ResourceUnavailable,
            Self::CreateChannel(_) => fcomponent::Error::ResourceUnavailable,
            Self::DuplicateJob(_) => fcomponent::Error::Internal,
            Self::InvalidUrl => fcomponent::Error::InvalidArguments,
            Self::ConfigVmo(_) => fcomponent::Error::Internal,
        };
        zx::Status::from_raw(status.into_primitive().try_into().unwrap())
    }
}

/// All information about this test ELF component.
#[derive(Debug)]
pub struct Component {
    /// Component URL
    pub url: String,

    /// Component name
    pub name: String,

    /// Binary path for this component relative to /pkg in 'ns'
    pub binary: String,

    /// Arguments for this test.
    pub args: Vec<String>,

    /// Environment variables for this test.
    pub environ: Option<Vec<String>>,

    /// Namespace to pass to test process.
    pub ns: Namespace,

    /// Parent job in which all test processes should be executed.
    pub job: zx::Job,

    /// Options to create process with.
    pub options: zx::ProcessOptions,

    /// Handle to library loader cache.
    lib_loader_cache: LibraryLoaderCacheProxy,

    /// cached executable vmo.
    executable_vmo: zx::Vmo,

    /// The structured config vmo.
    pub config_vmo: Option<zx::Vmo>,

    /// Component instance token, used only in tracing
    pub component_instance: Option<fidl::Event>,
}

pub struct BuilderArgs {
    /// Component URL
    pub url: String,

    /// Component name
    pub name: String,

    /// Binary path for this component relative to /pkg in 'ns'
    pub binary: String,

    /// Arguments for this test.
    pub args: Vec<String>,

    /// Environment variables for this test.
    pub environ: Option<Vec<String>>,

    /// Namespace to pass to test process.
    pub ns: Namespace,

    /// Parent job in which all test processes should be executed.
    pub job: zx::Job,

    /// The options to create the process with.
    pub options: zx::ProcessOptions,

    /// The structured config vmo.
    pub config: Option<zx::Vmo>,
}

impl Component {
    /// Create new object using `ComponentStartInfo`.
    /// On success returns self and outgoing_dir from `ComponentStartInfo`.
    pub async fn new<F>(
        start_info: fcrunner::ComponentStartInfo,
        validate_args: F,
    ) -> Result<
        (Self, ServerEnd<fio::DirectoryMarker>, ServerEnd<fio::DirectoryMarker>),
        ComponentError,
    >
    where
        F: 'static + Fn(&Vec<String>) -> Result<(), ArgumentError>,
    {
        let url =
            runner::get_resolved_url(&start_info).ok_or(ComponentError::MissingResolvedUrl)?;
        let name = Path::new(&url)
            .file_name()
            .ok_or_else(|| ComponentError::InvalidUrl)?
            .to_str()
            .ok_or_else(|| ComponentError::InvalidUrl)?
            .to_string();

        let args = runner::get_program_args(&start_info)
            .map_err(|e| ComponentError::InvalidArgs(url.clone(), e.into()))?;
        validate_args(&args).map_err(|e| ComponentError::InvalidArgs(url.clone(), e.into()))?;

        let binary = runner::get_program_binary(&start_info)
            .map_err(|e| ComponentError::InvalidArgs(url.clone(), e.into()))?;

        // It's safe to unwrap `start_info.program` below because if the field
        // were empty, this func would have a returned an error by now.
        let program = start_info.program.as_ref().unwrap();
        let environ = runner::get_environ(program)
            .map_err(|e| ComponentError::InvalidArgs(url.clone(), e.into()))?;
        let is_shared_process = runner::get_bool(program, "is_shared_process").unwrap_or(false);

        let ns = start_info.ns.ok_or_else(|| ComponentError::MissingNamespace(url.clone()))?;
        let ns = Namespace::try_from(ns)
            .map_err(|e| ComponentError::InvalidArgs(url.clone(), e.into()))?;

        let outgoing_dir =
            start_info.outgoing_dir.ok_or_else(|| ComponentError::MissingOutDir(url.clone()))?;

        let runtime_dir =
            start_info.runtime_dir.ok_or_else(|| ComponentError::MissingRuntimeDir(url.clone()))?;

        let (pkg_dir, lib_proxy) = get_pkg_and_lib_proxy(&ns, &url)?;

        let executable_vmo = library_loader::load_vmo(pkg_dir, &binary)
            .await
            .map_err(|e| ComponentError::LoadingExecutable(binary.clone(), e))?;
        let lib_loader_cache_builder = connect_to_protocol::<LibraryLoaderCacheBuilderMarker>()
            .map_err(|e| ComponentError::LibraryLoadError(url.clone(), e))?;

        let (lib_loader_cache, server_end) = create_proxy::<LibraryLoaderCacheMarker>();
        lib_loader_cache_builder
            .create(lib_proxy.into_channel().unwrap().into_zx_channel().into(), server_end)
            .map_err(|e| {
                ComponentError::Fidl("cannot communicate with lib loader cache".into(), e)
            })?;

        let config_vmo = match start_info.encoded_config {
            None => None,
            Some(config) => Some(runner::get_config_vmo(config).map_err(|e| {
                ComponentError::ConfigVmo(anyhow!("Failed to get config vmo: {}", e))
            })?),
        };

        Ok((
            Self {
                url: url,
                name: name,
                binary: binary,
                args: args,
                environ,
                ns: ns,
                job: job_default().create_child_job().map_err(ComponentError::CreateJob)?,
                executable_vmo,
                lib_loader_cache,
                options: if is_shared_process {
                    zx::ProcessOptions::SHARED
                } else {
                    zx::ProcessOptions::empty()
                },
                config_vmo,
                component_instance: start_info.component_instance,
            },
            outgoing_dir,
            runtime_dir,
        ))
    }

    pub fn config_vmo(&self) -> Result<Option<zx::Vmo>, ComponentError> {
        match &self.config_vmo {
            None => Ok(None),
            Some(vmo) => Ok(Some(
                vmo.as_handle_ref()
                    .duplicate(zx::Rights::SAME_RIGHTS)
                    .map_err(|_| {
                        ComponentError::VmoChild(
                            self.url.clone(),
                            anyhow!("Failed to clone config_vmo"),
                        )
                    })?
                    .into(),
            )),
        }
    }

    pub fn executable_vmo(&self) -> Result<zx::Vmo, ComponentError> {
        vmo_create_child(&self.executable_vmo)
            .map_err(|e| ComponentError::VmoChild(self.url.clone(), e))
    }

    pub fn loader_service(&self, loader: ServerEnd<LoaderMarker>) {
        if let Err(e) = self.lib_loader_cache.serve(loader) {
            error!("Cannot serve lib loader: {:?}", e);
        }
    }

    pub async fn create_for_tests(args: BuilderArgs) -> Result<Self, ComponentError> {
        let (pkg_dir, lib_proxy) = get_pkg_and_lib_proxy(&args.ns, &args.url)?;
        let executable_vmo = library_loader::load_vmo(pkg_dir, &args.binary)
            .await
            .map_err(|e| ComponentError::LoadingExecutable(args.url.clone(), e))?;
        let lib_loader_cache_builder = connect_to_protocol::<LibraryLoaderCacheBuilderMarker>()
            .map_err(|e| ComponentError::LibraryLoadError(args.url.clone(), e))?;

        let (lib_loader_cache, server_end) = create_proxy::<LibraryLoaderCacheMarker>();
        lib_loader_cache_builder
            .create(lib_proxy.into_channel().unwrap().into_zx_channel().into(), server_end)
            .map_err(|e| {
                ComponentError::Fidl("cannot communicate with lib loader cache".into(), e)
            })?;

        Ok(Self {
            url: args.url,
            name: args.name,
            binary: args.binary,
            args: args.args,
            environ: args.environ,
            ns: args.ns,
            job: args.job,
            lib_loader_cache,
            executable_vmo,
            options: args.options,
            config_vmo: None,
            component_instance: None,
        })
    }
}

fn vmo_create_child(vmo: &zx::Vmo) -> Result<zx::Vmo, anyhow::Error> {
    let size = vmo.get_size().context("Cannot get vmo size.")?;
    vmo.create_child(
        zx::VmoChildOptions::SNAPSHOT_AT_LEAST_ON_WRITE | zx::VmoChildOptions::NO_WRITE,
        0,
        size,
    )
    .context("cannot create child vmo")
}

// returns (pkg_dir, lib_proxy)
fn get_pkg_and_lib_proxy<'a>(
    ns: &'a Namespace,
    url: &String,
) -> Result<(&'a ClientEnd<fio::DirectoryMarker>, fio::DirectoryProxy), ComponentError> {
    // Locate the '/pkg' directory proxy previously added to the new component's namespace.
    let pkg_dir = ns
        .get(&PKG_PATH.parse().unwrap())
        .ok_or_else(|| ComponentError::MissingPkg(url.clone()))?;

    let lib_proxy =
        fuchsia_component::directory::open_directory_async(pkg_dir, "lib", fio::RX_STAR_DIR)
            .map_err(Into::into)
            .map_err(|e| ComponentError::LibraryLoadError(url.clone(), e))?;
    Ok((pkg_dir, lib_proxy))
}

#[async_trait]
impl runner::component::Controllable for ComponentRuntime {
    async fn kill(&mut self) {
        if let Some(component) = &self.component {
            info!("kill request component: {}", component.url);
        }
        self.kill_self();
    }

    fn stop<'a>(&mut self) -> BoxFuture<'a, ()> {
        if let Some(component) = &self.component {
            info!("stop request component: {}", component.url);
        }
        self.kill_self();
        async move {}.boxed()
    }
}

impl Drop for ComponentRuntime {
    fn drop(&mut self) {
        if let Some(component) = &self.component {
            info!("drop component: {}", component.url);
        }
        self.kill_self();
    }
}

/// Information about all the test instances running for this component.
struct ComponentRuntime {
    /// handle to abort component's outgoing services.
    outgoing_abortable_handle: Option<futures::future::AbortHandle>,

    /// handle to abort running test suite servers.
    suite_service_abortable_handles: Option<Arc<Mutex<Vec<futures::future::AbortHandle>>>>,

    /// job containing all processes in this component.
    job: Option<zx::Job>,

    /// component object which is stored here for safe keeping. It would be dropped when test is
    /// stopped/killed.
    component: Option<Arc<Component>>,
}

impl ComponentRuntime {
    fn new(
        outgoing_abortable_handle: futures::future::AbortHandle,
        suite_service_abortable_handles: Arc<Mutex<Vec<futures::future::AbortHandle>>>,
        job: zx::Job,
        component: Arc<Component>,
    ) -> Self {
        Self {
            outgoing_abortable_handle: Some(outgoing_abortable_handle),
            suite_service_abortable_handles: Some(suite_service_abortable_handles),
            job: Some(job),
            component: Some(component),
        }
    }

    fn kill_self(&mut self) {
        // drop component.
        if let Some(component) = self.component.take() {
            info!("killing component: {}", component.url);
        }

        // kill outgoing server.
        if let Some(h) = self.outgoing_abortable_handle.take() {
            h.abort();
        }

        // kill all suite servers.
        if let Some(handles) = self.suite_service_abortable_handles.take() {
            let handles = handles.lock().unwrap();
            for h in handles.deref() {
                h.abort();
            }
        }

        // kill all test processes if running.
        if let Some(job) = self.job.take() {
            let _ = job.kill();
        }
    }
}

/// Setup and run test component in background.
///
/// * `F`: Function which returns new instance of `SuiteServer`.
pub async fn start_component<F, U, S>(
    start_info: fcrunner::ComponentStartInfo,
    mut server_end: ServerEnd<fcrunner::ComponentControllerMarker>,
    get_test_server: F,
    validate_args: U,
) -> Result<(), ComponentError>
where
    F: 'static + Fn() -> S + Send,
    U: 'static + Fn(&Vec<String>) -> Result<(), ArgumentError>,
    S: SuiteServer,
{
    let resolved_url = runner::get_resolved_url(&start_info).unwrap_or(String::new());
    if let Err(e) =
        start_component_inner(start_info, &mut server_end, get_test_server, validate_args).await
    {
        // Take ownership of `server_end`.
        let server_end = take_server_end(&mut server_end);
        runner::component::report_start_error(
            e.as_zx_status(),
            format!("{}", e),
            &resolved_url,
            server_end,
        );
        return Err(e);
    }
    Ok(())
}

async fn start_component_inner<F, U, S>(
    mut start_info: fcrunner::ComponentStartInfo,
    server_end: &mut ServerEnd<fcrunner::ComponentControllerMarker>,
    get_test_server: F,
    validate_args: U,
) -> Result<(), ComponentError>
where
    F: 'static + Fn() -> S + Send,
    U: 'static + Fn(&Vec<String>) -> Result<(), ArgumentError>,
    S: SuiteServer,
{
    let break_on_start = start_info.break_on_start.take();
    let (component, outgoing_dir, runtime_dir) = Component::new(start_info, validate_args).await?;
    let component = Arc::new(component);

    // Debugger support:
    // 1. Serve the runtime directory providing the "elf/job_id" entry.
    let mut runtime_dir_builder = TreeBuilder::empty_dir();
    let job_id = component
        .job
        .get_koid()
        .map_err(|s| ComponentError::ServeRuntimeDir(anyhow!("cannot get job koid: {}", s)))?
        .raw_koid();
    runtime_dir_builder
        .add_entry(&["elf", "job_id"], read_only(job_id.to_string()))
        .map_err(|e| ComponentError::ServeRuntimeDir(anyhow!("cannot add elf/job_id: {}", e)))?;

    vfs::directory::serve_on(
        runtime_dir_builder.build(),
        fio::PERM_READABLE,
        ExecutionScope::new(),
        runtime_dir,
    );

    // 2. Wait on `break_on_start` before spawning any processes.
    if let Some(break_on_start) = break_on_start {
        fasync::OnSignals::new(&break_on_start, zx::Signals::OBJECT_PEER_CLOSED)
            .on_timeout(MAX_WAIT_BREAK_ON_START, || Err(zx::Status::TIMED_OUT))
            .await
            .err()
            .map(|e| warn!("Failed to wait break_on_start on {}: {}", component.name, e));
    }

    let job_runtime_dup = component
        .job
        .duplicate_handle(zx::Rights::SAME_RIGHTS)
        .map_err(ComponentError::DuplicateJob)?;

    let job_watch_dup = component
        .job
        .duplicate_handle(zx::Rights::SAME_RIGHTS)
        .map_err(ComponentError::DuplicateJob)?;
    let mut fs = ServiceFs::new();

    let suite_server_abortable_handles = Arc::new(Mutex::new(vec![]));
    let weak_test_suite_abortable_handles = Arc::downgrade(&suite_server_abortable_handles);
    let weak_component = Arc::downgrade(&component);

    let url = component.url.clone();
    fs.dir("svc").add_fidl_service(move |stream| {
        let abortable_handles = weak_test_suite_abortable_handles.upgrade();
        if abortable_handles.is_none() {
            return;
        }
        let abortable_handles = abortable_handles.unwrap();
        let mut abortable_handles = abortable_handles.lock().unwrap();
        let abortable_handle = get_test_server().run(weak_component.clone(), &url, stream);
        abortable_handles.push(abortable_handle);
    });

    fs.serve_connection(outgoing_dir).map_err(ComponentError::ServeSuite)?;
    let (fut, abortable_handle) = abortable(fs.collect::<()>());

    let component_runtime = ComponentRuntime::new(
        abortable_handle,
        suite_server_abortable_handles,
        job_runtime_dup,
        component,
    );

    fasync::Task::spawn(async move {
        // as error on abortable will always return Aborted,
        // no need to check that, as it is a valid usecase.
        fut.await.ok();
    })
    .detach();

    let server_end = take_server_end(server_end);
    let (controller_stream, control) = server_end.into_stream_and_control_handle();
    let controller =
        runner::component::Controller::new(component_runtime, controller_stream, control);

    let termination_fut = Box::pin(async move {
        // Just return 'OK' here. Any actual errors will be handled through
        // the test protocol.
        let _ =
            fasync::OnSignals::new(&job_watch_dup.as_handle_ref(), zx::Signals::TASK_TERMINATED)
                .await;
        StopInfo::from_ok(None)
    });

    fasync::Task::spawn(controller.serve(termination_fut)).detach();

    Ok(())
}

fn take_server_end<P: ProtocolMarker>(end: &mut ServerEnd<P>) -> ServerEnd<P> {
    let invalid_end: ServerEnd<P> = zx::Handle::invalid().into();
    mem::replace(end, invalid_end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elf::EnumeratedTestCases;
    use crate::errors::{EnumerationError, RunTestError};
    use anyhow::Error;
    use assert_matches::assert_matches;
    use fidl::endpoints::{self};
    use fidl_fuchsia_test::{Invocation, RunListenerProxy};
    use futures::future::{AbortHandle, Aborted};
    use namespace::NamespaceError;
    use std::sync::Weak;

    fn create_ns_from_current_ns(
        dir_paths: Vec<(&str, fio::Flags)>,
    ) -> Result<Namespace, NamespaceError> {
        let mut ns = vec![];
        for (path, permission) in dir_paths {
            let chan = fuchsia_fs::directory::open_in_namespace(path, permission)
                .unwrap()
                .into_channel()
                .unwrap()
                .into_zx_channel();
            let handle = ClientEnd::new(chan);

            ns.push(fcrunner::ComponentNamespaceEntry {
                path: Some(path.to_string()),
                directory: Some(handle),
                ..Default::default()
            });
        }
        Namespace::try_from(ns)
    }

    macro_rules! child_job {
        () => {
            job_default().create_child_job().unwrap()
        };
    }

    async fn sample_test_component() -> Result<Arc<Component>, Error> {
        let ns =
            create_ns_from_current_ns(vec![("/pkg", fio::PERM_READABLE | fio::PERM_EXECUTABLE)])?;

        Ok(Arc::new(
            Component::create_for_tests(BuilderArgs {
                url: "fuchsia-pkg://fuchsia.com/sample_test#test.cm".to_owned(),
                name: "test.cm".to_owned(),
                binary: "bin/test_runners_lib_lib_test".to_owned(), //reference self binary
                args: vec![],
                environ: None,
                ns: ns,
                job: child_job!(),
                options: zx::ProcessOptions::empty(),
                config: None,
            })
            .await?,
        ))
    }

    async fn dummy_func() -> u32 {
        2
    }

    struct DummyServer {}

    #[async_trait]
    impl SuiteServer for DummyServer {
        fn run(
            self,
            _component: Weak<Component>,
            _test_url: &str,
            _stream: fidl_fuchsia_test::SuiteRequestStream,
        ) -> AbortHandle {
            let (_, handle) = abortable(async {});
            handle
        }

        async fn enumerate_tests(
            &self,
            _test_component: Arc<Component>,
        ) -> Result<EnumeratedTestCases, EnumerationError> {
            Ok(Arc::new(vec![]))
        }

        async fn run_tests(
            &self,
            _invocations: Vec<Invocation>,
            _run_options: fidl_fuchsia_test::RunOptions,
            _component: Arc<Component>,
            _run_listener: &RunListenerProxy,
        ) -> Result<(), RunTestError> {
            Ok(())
        }
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn start_component_error() {
        let start_info = fcrunner::ComponentStartInfo {
            resolved_url: None,
            program: None,
            ns: None,
            outgoing_dir: None,
            runtime_dir: None,
            ..Default::default()
        };
        let (client_controller, server_controller) = endpoints::create_proxy();
        let get_test_server = || DummyServer {};
        let err = start_component(start_info, server_controller, get_test_server, |_| Ok(())).await;
        assert_matches!(err, Err(ComponentError::MissingResolvedUrl));
        let expected_status = zx::Status::from_raw(
            fcomponent::Error::InvalidArguments.into_primitive().try_into().unwrap(),
        );
        let s = assert_matches!(
            client_controller.take_event_stream().next().await,
            Some(Err(fidl::Error::ClientChannelClosed { status: s, .. })) => s
        );
        assert_eq!(s, expected_status);
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn start_component_works() {
        let _ = sample_test_component().await.unwrap();
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn component_runtime_kill_job_works() {
        let component = sample_test_component().await.unwrap();

        let mut futs = vec![];
        let mut handles = vec![];
        for _i in 0..10 {
            let (fut, handle) = abortable(dummy_func());
            futs.push(fut);
            handles.push(handle);
        }

        let (out_fut, out_handle) = abortable(dummy_func());
        let mut runtime = ComponentRuntime::new(
            out_handle,
            Arc::new(Mutex::new(handles)),
            child_job!(),
            component.clone(),
        );

        assert_eq!(Arc::strong_count(&component), 2);
        runtime.kill_self();

        for fut in futs {
            assert_eq!(fut.await, Err(Aborted));
        }

        assert_eq!(out_fut.await, Err(Aborted));

        assert_eq!(Arc::strong_count(&component), 1);
    }
}
