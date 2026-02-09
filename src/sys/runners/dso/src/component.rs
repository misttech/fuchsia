// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::error::StartError;
use crate::loader::{Hooks, Library, Loader};
use crate::util;
use async_trait::async_trait;
use derivative::Derivative;
use elf_runner::ElfComponentLaunchInfo;
use elf_runner::config::ElfProgramConfig;
use fdf::OnDispatcher;
use fidl::endpoints::ServerEnd;
use futures::channel::oneshot;
use futures::future::BoxFuture;
use futures::prelude::*;
use futures::select;
use log::{info, warn};
use namespace::Namespace;
use runner::component::StopInfo;
use runner::{StartInfo, component as runner_component};
use std::ffi::CString;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::{ptr, thread};
use vfs::ExecutionScope;
use zx::HandleBased;
use {
    fidl_fuchsia_component as fcomponent, fidl_fuchsia_component_runner as frunner,
    fidl_fuchsia_process as fprocess, fidl_fuchsia_process_lifecycle as flifecycle,
    fuchsia_async as fasync,
};

pub(super) type TerminateCallback = Box<dyn FnOnce(&str) + Send>;

pub(super) async fn start(
    start_info: frunner::ComponentStartInfo,
    controller_server: ServerEnd<frunner::ComponentControllerMarker>,
    env: &fdf_env::Environment,
    scope: &fasync::ScopeHandle,
    terminate_cb: TerminateCallback,
) -> Result<(), StartError> {
    let LaunchInfo { controller, exit_fut } =
        match Component::launch(start_info, &env, terminate_cb).await {
            Ok(c) => c,
            Err(e) => {
                _ = controller_server.close_with_epitaph((&e).into());
                return Err(e);
            }
        };

    let (requests, control) = controller_server.into_stream_and_control_handle();
    let url = controller.url.clone();
    let controller = runner_component::Controller::new(controller, requests, control);
    scope.spawn_local(async move {
        info!(url:%; "Started component");
        _ = controller.serve(pin!(exit_fut)).await;
        info!(url:%; "Component stopped");
    });
    Ok(())
}

#[derive(Derivative)]
#[derivative(Debug)]
struct Component {
    url: String,
    argv: Vec<String>,
    environ: Vec<String>,
    ns: Namespace,
    handle_infos: Vec<fprocess::HandleInfo>,
    lifecycle_client: flifecycle::LifecycleProxy,
    library: Library,
    local_scope: ExecutionScope,
    control: InnerControl,
}

struct LaunchInfo {
    controller: Controller,
    exit_fut: Pin<Box<dyn Future<Output = StopInfo> + 'static>>,
}

impl Component {
    async fn launch(
        start_info: frunner::ComponentStartInfo,
        env: &fdf_env::Environment,
        terminate_routine: TerminateCallback,
    ) -> Result<LaunchInfo, StartError> {
        let dso_path = runner::get_program_string(&start_info, "binary")
            .ok_or(StartError::InvalidArgs)?
            .to_string();

        let mut start_info = StartInfo::try_from(start_info)?;
        let mut config = ElfProgramConfig::parse_and_check(&start_info.program, None)?;
        // DSO components should always use their lifecycle handle so always give them one.
        config.notify_lifecycle_stop = true;
        let ElfComponentLaunchInfo {
            ns,
            handle_infos,
            utc_clock: _,
            lifecycle_client,
            outgoing_directory: _,
            local_scope,
        } = ElfComponentLaunchInfo::new(&mut start_info, &config, None)?;
        let lifecycle_client = lifecycle_client.expect("lifecycle missing");
        let argv = runner::get_program_args_from_dict(&start_info.program)?;
        let environ = runner::get_environ(&start_info.program)?.unwrap_or_default();
        let is_async = runner::get_bool(&start_info.program, "async")?;
        let sched_role = runner::get_string(&start_info.program, "scheduler_role");

        let dso_vmo = util::get_pkg_file_vmo(&ns, &dso_path).await?;
        let dso_name = util::basename(&dso_path);
        dso_vmo.set_name(&zx::Name::new_lossy(dso_name)).map_err(|_| StartError::Internal)?;
        let lib_dir = util::open_pkg_path(&ns, "lib").await?;

        let library = Loader::install(dso_name, dso_vmo, lib_dir)
            .map_err(|err| StartError::LoadDso { name: dso_name.into(), err })?;

        let url = start_info.resolved_url.clone();

        let driver = Arc::new(());
        let name = dso_name.to_string();
        let control;
        let dispatcher;
        let shutdown_done_rx;
        if is_async {
            let mut fdf_dispatcher =
                fdf::DispatcherBuilder::new().name(dso_name).shutdown_observer(move |_| {
                    info!(name:%; "dispatcher shutdown");
                });
            if let Some(sched_role) = sched_role {
                fdf_dispatcher = fdf_dispatcher.scheduler_role(sched_role);
                info!(dso_name:%, sched_role:%; "Applied scheduler role");
            }
            let driver = Arc::into_raw(driver);
            let runtime_handle = env.new_driver(driver);
            // SAFETY: This is safe because `driver` was obtained from [`Arc::into_raw`] above.
            let driver = unsafe { Arc::from_raw(driver) };
            let fdf_dispatcher = runtime_handle
                .new_dispatcher(fdf_dispatcher)
                .map_err(|err| StartError::CreateDispatcher { name: dso_name.into(), err })?;
            let (tx, rx) = oneshot::channel();
            control = InnerControl::Async(Some(AsyncControl {
                runtime_handle: Some(runtime_handle),
                _driver: driver,
                shutdown_done_tx: Some(tx),
            }));
            dispatcher = Some(fdf_dispatcher);
            shutdown_done_rx = Some(rx);
        } else {
            control = InnerControl::Sync(SyncControl {
                terminate_cb: Some(terminate_routine),
                thread_handle: None,
            });
            dispatcher = None;
            shutdown_done_rx = None;
        };

        let component = Self {
            url,
            argv,
            environ,
            ns,
            lifecycle_client,
            handle_infos,
            library,
            local_scope,
            control,
        };
        component
            .run(dso_name, dispatcher, shutdown_done_rx)
            .map_err(|err| StartError::Execute { err })
    }

    fn run(
        self,
        dso_name: &str,
        dispatcher: Option<fdf::Dispatcher>,
        shutdown_done_rx: Option<oneshot::Receiver<()>>,
    ) -> Result<LaunchInfo, zx::Status> {
        let Self {
            url,
            library,
            argv,
            environ,
            ns,
            lifecycle_client,
            handle_infos,
            local_scope,
            mut control,
        } = self;
        let is_async = matches!(control, InnerControl::Async(_));
        let hooks = Hooks::new_from_library(&library, is_async).map_err(|expected_symbol| {
            warn!(url:%; "Failed to start component: symbol `{}` missing from \
                              shared object", expected_symbol.to_string_lossy());
            zx::Status::NOT_FOUND
        })?;
        let c_dso_name = CString::new(dso_name).unwrap();

        let (mut handle, mut handle_info): (Vec<_>, Vec<_>) = handle_infos
            .into_iter()
            .map(|h| {
                let fprocess::HandleInfo { handle, id } = h;
                (handle.into_raw(), id)
            })
            .unzip();

        let ns = ns.flatten();
        let mut names_alloc = Vec::with_capacity(ns.len());
        for (i, entry) in ns.into_iter().enumerate() {
            let namespace::Entry { path, directory } = entry;
            names_alloc.push(CString::new(format!("{path}")).unwrap());
            handle.push(directory.into_handle().into_raw());
            let info = fuchsia_runtime::HandleInfo::new(
                fuchsia_runtime::HandleType::NamespaceDirectory,
                i as u16,
            )
            .as_raw();
            handle_info.push(info);
        }

        let exit_wait;
        match &mut control {
            InnerControl::Async(None) => unreachable!("missing async control"),
            InnerControl::Async(Some(_)) => {
                let dso_name = dso_name.to_string();
                let dispatcher =
                    dispatcher.expect("missing dispatcher for async component").release();
                let dispatcher2 = dispatcher.clone();
                let (exit_code_tx, exit_code_rx) = oneshot::channel();
                let shutdown_done_rx = shutdown_done_rx.expect("missing shutdown_done_rx");

                dispatcher.spawn(async move {
                    let mut names: Vec<_> = names_alloc.iter().map(|n| n.as_ptr()).collect();
                    let argv_alloc: Vec<_> =
                        argv.into_iter().map(|s| CString::new(s).unwrap()).collect();
                    let mut argv: Vec<_> = argv_alloc.iter().map(|s| s.as_ptr()).collect();
                    argv.insert(0, c_dso_name.as_ptr());

                    let envp_alloc: Vec<_> =
                        environ.into_iter().map(|s| CString::new(s).unwrap()).collect();
                    let mut envp: Vec<_> = envp_alloc.iter().map(|s| s.as_ptr()).collect();
                    envp.push(ptr::null_mut());

                    // TODO(https://fxbug.dev/403545512): Deallocate handles once the program
                    // terminates
                    let handle = handle.leak();
                    let handle_info = handle_info.leak();
                    // SAFETY: Inputs are not freed until the dispatcher is shutdown.
                    let code = unsafe {
                        hooks.dso_start_async(
                            handle,
                            handle_info,
                            &mut names,
                            &mut argv,
                            &mut envp,
                            dispatcher2,
                        )
                    };
                    if code != 0 {
                        warn!(dso_name:%, code:%; "async component dso_start returned non-zero");
                        _ = exit_code_tx.send(code);
                    } else {
                        drop(exit_code_tx);
                        // Don't deallocate argv and envp so the program can continue using them
                        // TODO(https://fxbug.dev/403545512): Deallocate them once the program
                        // terminates
                        for name in names_alloc {
                            _ = CString::into_raw(name);
                        }
                        for arg in argv_alloc {
                            _ = CString::into_raw(arg);
                        }
                        for env in envp_alloc {
                            _ = CString::into_raw(env);
                        }
                    }
                }).unwrap();

                exit_wait = async move {
                    let mut shutdown_wait = async move {
                        _ = shutdown_done_rx.await;
                    }
                    .boxed()
                    .fuse();
                    let mut exit_code_wait = exit_code_rx.boxed().fuse();
                    loop {
                        select! {
                            _ = shutdown_wait => {
                                return Ok(None);
                            }
                            res = exit_code_wait => {
                                match res {
                                    Ok(c) => return Ok(Some(c)),
                                    Err(_) => {
                                        // We end up here if dso_main_async returned 0. In that case
                                        // no exit code will be sent so there is no more exit code
                                        // to wait for.
                                        exit_code_wait = async move {
                                            std::future::pending::<()>().await;
                                            Ok(0)
                                        }.boxed().fuse();
                                    }
                                }
                            }
                        }
                    }
                }
                .boxed()
                .fuse();
            }
            InnerControl::Sync(control) => {
                let (exit_tx, exit_rx) = oneshot::channel();
                let h = thread::Builder::new()
                    .name(dso_name.into())
                    .spawn(move || {
                        let mut names: Vec<_> = names_alloc.iter().map(|n| n.as_ptr()).collect();
                        let argv_alloc: Vec<_> =
                            argv.into_iter().map(|s| CString::new(s).unwrap()).collect();
                        let mut argv: Vec<_> = argv_alloc.iter().map(|s| s.as_ptr()).collect();
                        argv.insert(0, c_dso_name.as_ptr());

                        let envp_alloc: Vec<_> =
                            environ.into_iter().map(|s| CString::new(s).unwrap()).collect();
                        let mut envp: Vec<_> = envp_alloc.iter().map(|s| s.as_ptr()).collect();
                        envp.push(ptr::null_mut());

                        // SAFETY: Inputs are not freed until `dso_start` returns.
                        let code = unsafe {
                            hooks.dso_start(
                                &mut handle,
                                &mut handle_info,
                                &mut names,
                                &mut argv,
                                &mut envp,
                            )
                        };
                        _ = exit_tx.send(Some(code));
                    })
                    .expect("spawn component thread");
                control.thread_handle = Some(h);
                exit_wait = exit_rx.boxed().fuse();
            }
        };

        let component_url = url.clone();
        let is_async = match &mut control {
            InnerControl::Async(_) => true,
            InnerControl::Sync(_) => false,
        };
        let lifecycle_client2 = lifecycle_client.clone();
        let exit_fut = async move {
            let mut exit_wait = exit_wait;
            let mut lifecycle_close_wait = if is_async {
                async move {
                    let mut event_stream = lifecycle_client2.take_event_stream();
                    while let Some(_) = event_stream.next().await {
                        // TODO(https://fxbug.dev/403545512): Handle lifecycle events like OnEscrow
                    }
                    // Lifecycle channel closed means component has exited.
                }
                .boxed()
            } else {
                std::future::pending::<()>().boxed()
            }
            .fuse();
            select! {
                _ = lifecycle_close_wait => {
                    info!(component_url:%, exit_code:% = 0, is_async:%; "Component terminated");
                    return StopInfo { termination_status: zx::Status::OK, exit_code: Some(0) };
                }
                res = exit_wait => {
                    match res {
                        Ok(Some(exit_code)) => {
                            let exit_code: i64 = exit_code.into();
                            info!(component_url:%, exit_code:%, is_async:%; "Component terminated");
                            return StopInfo {
                                termination_status: zx::Status::OK,
                                exit_code: Some(exit_code),
                            };
                        }
                        Ok(None) | Err(_) => {
                            // This case handles async dispatcher shutdown
                            assert!(is_async);
                            warn!(component_url:%, is_async:%; "Component terminated (killed)");
                            return StopInfo::from_error(
                                fcomponent::Error::InstanceDied,
                                None,
                            );
                        }
                    }
                }
            }
        };
        let controller = Controller {
            url,
            _library: library,
            _local_scope: local_scope,
            lifecycle_client,
            control,
        };
        Ok(LaunchInfo { controller, exit_fut: Box::pin(exit_fut) })
    }
}

#[derive(Debug)]
enum InnerControl {
    Sync(SyncControl),
    // `None` if shutdown has completed and there is nothing left to do
    Async(Option<AsyncControl>),
}

#[derive(Derivative)]
#[derivative(Debug)]
struct SyncControl {
    #[derivative(Debug = "ignore")]
    terminate_cb: Option<TerminateCallback>,
    thread_handle: Option<thread::JoinHandle<()>>,
}

#[derive(Debug)]
struct AsyncControl {
    runtime_handle: Option<fdf_env::Driver<()>>,
    _driver: Arc<()>,
    shutdown_done_tx: Option<oneshot::Sender<()>>,
}

impl Drop for AsyncControl {
    fn drop(&mut self) {
        // We may end up here without maybe_shutdown_dispatcher if an error preempted the component
        // from starting.
        let Self { runtime_handle, _driver, shutdown_done_tx: _ } = self;
        // Must be done before dropping `_driver`, otherwise it will panic.
        runtime_handle.take().map(|r| r.shutdown(|_| {}));
    }
}

async fn maybe_shutdown_dispatcher(ctl: Option<AsyncControl>) {
    let Some(mut ctl) = ctl else {
        return;
    };
    let (tx, rx) = oneshot::channel();
    ctl.runtime_handle.take().expect("shutdown called twice").shutdown(|_| {
        _ = tx.send(());
    });
    _ = rx.await;
    _ = ctl.shutdown_done_tx.take().expect("shutdown called twice").send(());
}

#[derive(Derivative)]
#[derivative(Debug)]
struct Controller {
    // For debugging
    url: String,
    // Must be kept alive while executable is running
    _library: Library,
    _local_scope: ExecutionScope,
    control: InnerControl,
    lifecycle_client: flifecycle::LifecycleProxy,
}

#[async_trait]
impl runner_component::Controllable for Controller {
    fn teardown<'a>(&mut self) -> BoxFuture<'a, ()> {
        match &mut self.control {
            InnerControl::Async(control) => maybe_shutdown_dispatcher(control.take()).boxed(),
            InnerControl::Sync(control) => {
                // If there was a thread it should have exited by now.
                if let Some(thread_handle) = control.thread_handle.take() {
                    thread_handle.join().unwrap();
                }
                return async move {}.boxed();
            }
        }
    }

    async fn kill(&mut self) {
        match &mut self.control {
            InnerControl::Async(control) => {
                maybe_shutdown_dispatcher(control.take()).await;
            }
            InnerControl::Sync(control) => {
                if let Some(cb) = control.terminate_cb.take() {
                    (cb)(&self.url);
                }
            }
        }
    }

    fn stop<'a>(&mut self) -> BoxFuture<'a, ()> {
        let lifecycle_client = self.lifecycle_client.clone();
        async move {
            _ = lifecycle_client.stop();
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use fidl::endpoints::{self, ClientEnd};
    use futures::poll;
    use futures::task::Poll;
    use {fidl_fuchsia_data as fdata, fidl_fuchsia_io as fio};

    const TEST_ROLE: &'static str = "fuchsia.dso.test";
    const MAX_DISPATCHER_THREADS: u32 = 1;

    #[derive(Debug)]
    struct ComponentInfo {
        controller: frunner::ComponentControllerProxy,
        termination_rx: Option<oneshot::Receiver<()>>,
        _scope: fasync::Scope,
        _outgoing: ClientEnd<fio::DirectoryMarker>,
        _runtime: ClientEnd<fio::DirectoryMarker>,
    }

    #[derive(Debug, Clone, Copy)]
    enum Syncness {
        Sync,
        Async,
    }

    fn terminate_cb() -> (TerminateCallback, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();
        let f = Box::new(move |_url: &str| {
            _ = tx.send(());
        });
        (f, rx)
    }

    async fn start_component(
        env: &fdf_env::Environment,
        binary: &str,
        syncness: Syncness,
    ) -> Result<ComponentInfo, StartError> {
        let scope = fasync::Scope::new();
        let (controller, controller_server) = endpoints::create_endpoints();
        let (outgoing, outgoing_server) = endpoints::create_endpoints();
        let (runtime, runtime_server) = endpoints::create_endpoints();
        let (pkg, pkg_server) = zx::Channel::create();
        let is_async = match syncness {
            Syncness::Sync => "false",
            Syncness::Async => "true",
        };
        fdio::open("/pkg", fio::PERM_READABLE | fio::PERM_EXECUTABLE, pkg_server).unwrap();
        let start_info = frunner::ComponentStartInfo {
            resolved_url: Some("fuchsia-pkg://fuchsia.com/something#meta/something.cm".into()),
            program: Some(fdata::Dictionary {
                entries: Some(vec![
                    fdata::DictionaryEntry {
                        key: "runner".into(),
                        value: Some(Box::new(fdata::DictionaryValue::Str("dso".into()))),
                    },
                    fdata::DictionaryEntry {
                        key: "binary".into(),
                        value: Some(Box::new(fdata::DictionaryValue::Str(binary.into()))),
                    },
                    fdata::DictionaryEntry {
                        key: "async".into(),
                        value: Some(Box::new(fdata::DictionaryValue::Str(is_async.into()))),
                    },
                ]),
                ..Default::default()
            }),
            component_instance: Some(zx::Event::create()),
            ns: Some(vec![frunner::ComponentNamespaceEntry {
                path: Some("/pkg".into()),
                directory: Some(ClientEnd::from(pkg)),
                ..Default::default()
            }]),
            outgoing_dir: Some(outgoing_server),
            runtime_dir: Some(runtime_server),
            ..Default::default()
        };
        let (termination_routine, termination_rx) = terminate_cb();
        start(start_info, controller_server, env, &scope, termination_routine).await?;
        Ok(ComponentInfo {
            controller: controller.into_proxy(),
            termination_rx: Some(termination_rx),
            _scope: scope,
            _outgoing: outgoing,
            _runtime: runtime,
        })
    }

    fn make_env() -> fdf_env::Environment {
        let env = fdf_env::Environment::start(0).unwrap();
        env.set_thread_limit(TEST_ROLE, MAX_DISPATCHER_THREADS).unwrap();
        env
    }

    fn lib_so(name: &str) -> String {
        format!("lib/{name}")
    }

    unsafe extern "C" {
        safe fn simple_sync_read_run_counter() -> u32;
        safe fn simple_async_read_run_counter() -> u32;
        safe fn hanging_sync_read_run_counter() -> u32;
        safe fn hanging_async_read_run_counter() -> u32;
    }

    #[fuchsia::test]
    async fn start_binary_not_found() {
        let env = make_env();
        let binary = lib_so("libdoes_not_exist.so");
        for syncness in [Syncness::Sync, Syncness::Async] {
            let res = start_component(&env, &binary, syncness).await;
            assert_matches!(
                &res,
                Err(StartError::OpenDsoFidl { path, err: _ }) if *path == binary
            );
            assert_eq!(
                zx::Status::from(&res.unwrap_err()).into_raw(),
                fcomponent::Error::ResourceNotFound.into_primitive() as i32
            );
        }
    }

    #[fuchsia::test]
    async fn start_and_exit_sync() {
        fn read_run_counter() -> usize {
            simple_sync_read_run_counter() as usize
        }

        const NUM_REPS: usize = 3;
        let env = make_env();
        let binary = lib_so("libsimple_sync.so");
        for _ in 0..NUM_REPS {
            let mut component = start_component(&env, &binary, Syncness::Sync).await.unwrap();
            let mut event_stream = component.controller.take_event_stream();
            assert_matches!(
                event_stream.next().await,
                Some(Ok(frunner::ComponentControllerEvent::OnStop {
                    payload: frunner::ComponentStopInfo {
                        termination_status: Some(0),
                        exit_code: Some(0),
                        ..
                    }
                }))
            );
            assert_matches!(event_stream.next().await, None);
            // Termination callback should not run.
            assert_matches!(component.termination_rx.take().unwrap().await, Err(oneshot::Canceled));
        }
        assert_eq!(read_run_counter(), NUM_REPS);
    }

    // TODO(https://fxbug.dev/403545512): Enable this test once async exit support is working.
    #[fuchsia::test]
    #[ignore]
    async fn start_and_exit_async() {
        fn read_run_counter() -> usize {
            simple_async_read_run_counter() as usize
        }

        const NUM_REPS: usize = 3;
        let env = make_env();
        let binary = lib_so("libsimple_async.so");
        for _ in 0..NUM_REPS {
            let component = start_component(&env, &binary, Syncness::Async).await.unwrap();
            let mut event_stream = component.controller.take_event_stream();
            assert_matches!(
                event_stream.next().await,
                Some(Ok(frunner::ComponentControllerEvent::OnStop {
                    payload: frunner::ComponentStopInfo {
                        termination_status: Some(0),
                        exit_code: Some(0),
                        ..
                    }
                }))
            );
            assert_matches!(event_stream.next().await, None);
        }
        assert_eq!(read_run_counter(), NUM_REPS);
    }

    #[fuchsia::test]
    async fn start_and_exit_sync_with_error() {
        const NUM_REPS: usize = 3;
        let env = make_env();
        let binary = lib_so("liberror_sync.so");
        for _ in 0..NUM_REPS {
            let mut component = start_component(&env, &binary, Syncness::Sync).await.unwrap();
            let mut event_stream = component.controller.take_event_stream();
            assert_matches!(
                event_stream.next().await,
                Some(Ok(frunner::ComponentControllerEvent::OnStop {
                    payload: frunner::ComponentStopInfo {
                        termination_status: Some(0),
                        // Returned by dso_main[_async]
                        exit_code: Some(128),
                        ..
                    }
                }))
            );
            assert_matches!(event_stream.next().await, None);
            // Termination callback should not run.
            assert_matches!(component.termination_rx.take().unwrap().await, Err(oneshot::Canceled));
        }
    }

    #[fuchsia::test]
    async fn start_and_exit_async_with_error() {
        const NUM_REPS: usize = 3;
        let env = make_env();
        let binary = lib_so("liberror_async.so");
        for _ in 0..NUM_REPS {
            let component = start_component(&env, &binary, Syncness::Async).await.unwrap();
            let mut event_stream = component.controller.take_event_stream();
            assert_matches!(
                event_stream.next().await,
                Some(Ok(frunner::ComponentControllerEvent::OnStop {
                    payload: frunner::ComponentStopInfo {
                        termination_status: Some(0),
                        // Returned by dso_main[_async]
                        exit_code: Some(128),
                        ..
                    }
                }))
            );
            assert_matches!(event_stream.next().await, None);
        }
    }

    // TODO(https://fxbug.dev/403545512): Write start_on_stop test once we support passing the
    // lifecycle channel to sync and async components.

    #[fuchsia::test]
    async fn start_and_kill_sync() {
        fn read_run_counter() -> usize {
            hanging_sync_read_run_counter() as usize
        }

        const NUM_REPS: usize = 3;
        let env = make_env();
        let binary = lib_so("libhanging_sync.so");

        let mut components = vec![];
        for _ in 0..NUM_REPS {
            components.push(start_component(&env, &binary, Syncness::Sync).await.unwrap());
        }
        while read_run_counter() < NUM_REPS {
            fasync::Timer::new(fasync::MonotonicDuration::from_millis(100)).await
        }
        assert_eq!(read_run_counter(), NUM_REPS);
        for i in 0..NUM_REPS {
            components[i].controller.kill().unwrap();
        }
        for i in 0..NUM_REPS {
            // Termination callback should run. But the runner won't close the controller channel.
            // (In a real setting the channel will close because the termination routine exits the
            // process.)
            let mut event_stream = components[i].controller.take_event_stream();
            assert_matches!(poll!(event_stream.next()), Poll::Pending);
            assert_matches!(components[i].termination_rx.take().unwrap().await, Ok(()));
        }
    }

    #[fuchsia::test]
    async fn start_and_kill_async() {
        fn read_run_counter() -> usize {
            hanging_async_read_run_counter() as usize
        }

        const NUM_REPS: usize = 3;
        let env = make_env();
        let binary = lib_so("libhanging_async.so");

        let mut components = vec![];
        for _ in 0..NUM_REPS {
            components.push(start_component(&env, &binary, Syncness::Async).await.unwrap());
        }
        while read_run_counter() < NUM_REPS {
            fasync::Timer::new(fasync::MonotonicDuration::from_millis(100)).await
        }
        assert_eq!(read_run_counter(), NUM_REPS);
        for i in 0..NUM_REPS {
            components[i].controller.kill().unwrap();
        }
        for i in 0..NUM_REPS {
            let mut event_stream = components[i].controller.take_event_stream();
            assert_matches!(
                event_stream.next().await,
                Some(Ok(frunner::ComponentControllerEvent::OnStop {
                    payload: frunner::ComponentStopInfo {
                        termination_status: Some(s),
                        exit_code: None,
                        ..
                    }
                }))
                if s == i32::try_from(fcomponent::Error::InstanceDied.into_primitive()).unwrap()
            );
            assert_matches!(event_stream.next().await, None);
        }
    }
}
