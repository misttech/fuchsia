// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![recursion_limit = "256"]
#![allow(clippy::too_many_arguments)]
// TODO(https://fxbug.dev/42073005): Remove this allow once the lint is fixed.
#![allow(unknown_lints, clippy::extra_unused_type_parameters)]

// Avoid unused crate warnings on non-test/non-debug builds because this needs to be an
// unconditional dependency for rustdoc generation.
use extended_pstate as _;
use tracing_mutex as _;

use anyhow::{Context as _, Error};
use async_lock::OnceCell;
use fidl_fuchsia_component_runner as frunner;
use fidl_fuchsia_memory_attribution as fattribution;
use fidl_fuchsia_process_lifecycle as flifecycle;
use fidl_fuchsia_starnix_container as fstarcontainer;
use fuchsia_async as fasync;
use fuchsia_component::server::ServiceFs;
use fuchsia_inspect::health::Reporter;
use fuchsia_runtime as fruntime;
use futures::{StreamExt, TryStreamExt};
use starnix_core::mm::{init_usercopy, zxio_maybe_faultable_copy_impl};
use starnix_kernel_runner::{
    Container, ContainerServiceConfig, create_component_from_stream, serve_component_runner,
    serve_container_controller, serve_memory_attribution_provider_elfkernel,
};
use starnix_kernel_structured_config::Config as KernelStructuredConfig;
use starnix_logging::{
    CATEGORY_STARNIX, NAME_START_KERNEL, log_debug, log_error, log_info, log_warn,
};
use std::rc::Rc;

/// Overrides the `zxio_maybe_faultable_copy` weak symbol found in zxio.
#[unsafe(no_mangle)]
extern "C" fn zxio_maybe_faultable_copy(
    dest: *mut u8,
    src: *const u8,
    count: usize,
    ret_dest: bool,
) -> bool {
    // SAFETY: we know that we are either copying from or to a buffer that
    // zxio (and thus Starnix) owns per `zxio_maybe_faultable_copy`'s
    // documentation.
    unsafe { zxio_maybe_faultable_copy_impl(dest, src, count, ret_dest) }
}

/// Overrides the `zxio_fault_catching_disabled` weak symbol found in zxio.
#[unsafe(no_mangle)]
extern "C" fn zxio_fault_catching_disabled() -> bool {
    false
}

fn maybe_serve_lifecycle(container: Rc<OnceCell<Container>>) -> Option<fasync::Task<()>> {
    if let Some(lifecycle) =
        fruntime::take_startup_handle(fruntime::HandleInfo::new(fruntime::HandleType::Lifecycle, 0))
    {
        Some(fasync::Task::local(async move {
            let mut stream =
                fidl::endpoints::ServerEnd::<flifecycle::LifecycleMarker>::new(lifecycle.into())
                    .into_stream();
            if let Ok(Some(request)) = stream.try_next().await {
                match request {
                    flifecycle::LifecycleRequest::Stop { .. } => {
                        if let Some(container) = container.get() {
                            container.kernel.shut_down();
                        } else {
                            log_warn!("Stopping kernel process without a running container.");
                            std::process::exit(0);
                        }
                    }
                }
            }
        }))
    } else {
        log_warn!("No lifecycle channel received from ELF runner.");
        None
    }
}

enum KernelServices {
    /// This service lets clients start a single container using this kernel.
    ///
    /// The `starnix_kernel` is capable of running a single container, which can be started using
    /// this protocol. Attempts to use this protocol a second time will fail.
    ///
    /// This service uses the `ComponentRunner` protocol but the service is exposed using the name
    /// `fuchsia.starnix.container.Runner` to reduce confusion with the instance of the
    /// `ComponentRunner` protocol that runs components inside the container.
    ContainerRunner(frunner::ComponentRunnerRequestStream),

    /// This service lets clients run components inside the container being run by this kernel.
    ///
    /// This service will wait to process any requests until the kernel starts a container.
    ///
    /// This service is also exposed via the container itself.
    ComponentRunner(frunner::ComponentRunnerRequestStream),

    /// This service lets clients control the container being run by this kernel.
    ///
    /// This service will wait to process any requests until the kernel starts a container.
    ///
    /// This service is also exposed via the container itself.
    ContainerController(fstarcontainer::ControllerRequestStream),

    /// This service lets clients read which memory resources are used to run the
    /// various starnix programs in the container.
    ///
    /// It provides a finer grained attribution than possible with Zircon process level
    /// tooling, because all starnix processes in a container share the same handle table.
    /// This protocol lets the kernel report exactly which VMOs it used to run a starnix
    /// program, out of VMOs in the shared handle table.
    ///
    /// The starnix runner connects to this protocol to report memory attribution
    /// information for each container it runs.
    MemoryAttributionProvider(fattribution::ProviderRequestStream),
}

#[fuchsia::main(
    // Don't add any statically declared tags to reduce right-ward drift in log output. In practice
    // all logs get tagged with task info that makes it clear from context the log comes from
    // Starnix.
    logging_tags = [],
    // LINT.IfChange(starnix_panic_tefmo)
    logging_panic_prefix="\n\n\n\nSTARNIX KERNEL PANIC\n\n\n\n",
    // LINT.ThenChange(//tools/testing/tefmocheck/string_in_log_check.go:starnix_panic_tefmo)
)]
async fn main() -> Result<(), Error> {
    // Make sure that if this process panics in normal mode that the whole kernel's job is killed.
    fruntime::job_default()
        .set_critical(zx::JobCriticalOptions::RETCODE_NONZERO, &fruntime::process_self())
        .context("ensuring main process panics kill whole kernel")?;

    let kernel_structured_config = KernelStructuredConfig::take_from_startup_handle();
    let KernelStructuredConfig { extra_features: kernel_extra_features, prefetch, .. } =
        kernel_structured_config;

    if prefetch {
        let file_proxy = fuchsia_fs::file::open_in_namespace(
            "/pkg/bin/starnix_kernel",
            fidl_fuchsia_io::PERM_READABLE | fidl_fuchsia_io::PERM_EXECUTABLE,
        )
        .context("open /pkg/bin/starnix_kernel")?;

        let vmo = file_proxy
            .get_backing_memory(
                fidl_fuchsia_io::VmoFlags::READ | fidl_fuchsia_io::VmoFlags::EXECUTE,
            )
            .await
            .context("call get_backing_memory")?
            .map_err(zx::Status::from_raw)
            .context("get_backing_memory")?;
        let size = vmo.get_size().context("get vmo size")?;
        vmo.op_range(zx::VmoOp::PREFETCH, 0, size).context("prefetch vmo")?;

        log_info!("Prefetched starnix_kernel binary");
    }

    let _inspect_server_task = inspect_runtime::publish(
        fuchsia_inspect::component::init_inspector_with_size(1_000_000),
        inspect_runtime::PublishOptions::default(),
    );
    fuchsia_inspect::component::serve_inspect_stats();
    let mut health = fuchsia_inspect::component::health();
    health.set_starting_up();

    fuchsia_trace_provider::trace_provider_create_with_fdio();
    fuchsia_trace_provider::trace_provider_wait_for_init();
    fuchsia_trace::instant!(CATEGORY_STARNIX, NAME_START_KERNEL, fuchsia_trace::Scope::Thread);

    // We use `inspector_print_debug_info` directly instead of `backtrace_request_thread` because
    // `backtrace_request_thread` relies on the exception mechanism (crashsvc). If we use
    // exceptions, the exception is attributed to the main Starnix kernel process (which detects the
    // lockup), not the process containing the locked-up thread. This would prevent crashsvc from
    // accessing the correct thread state and stack. By holding the thread handle directly, we can
    // inspect it regardless of its process.
    // SAFETY: This declares external C symbols from the Zircon inspector library.
    unsafe extern "C" {
        fn inspector_print_debug_info(
            out: *mut std::ffi::c_void,
            process: zx::sys::zx_handle_t,
            thread: zx::sys::zx_handle_t,
        );
        static stderr: *mut std::ffi::c_void;
    }

    async fn dump_thread_backtrace(thread: &zx::Thread) {
        use zx::Task;

        let _suspend_token = match thread.suspend() {
            Ok(token) => token,
            Err(e) => {
                log_error!("Failed to suspend thread: {:?}", e);
                return;
            }
        };

        // Wait for suspended signal asynchronously.
        match fuchsia_async::OnSignals::new(thread, zx::Signals::THREAD_SUSPENDED).await {
            Ok(_signals) => (),
            Err(e) => {
                log_error!("Failed to wait for thread suspension: {:?}", e);
                return;
            }
        }

        // SAFETY: Calling FFI is safe when passing valid handles.
        unsafe {
            let process_self = fuchsia_runtime::process_self().raw_handle();
            inspector_print_debug_info(stderr, process_self, thread.raw_handle());
        }
    }

    const LOCKUP_DETECTOR_INTERVAL_MINUTES: i64 = 2;
    let _lockup_detector_task = fasync::Task::spawn(async {
        loop {
            fasync::Timer::new(zx::MonotonicInstant::after(zx::MonotonicDuration::from_minutes(
                LOCKUP_DETECTOR_INTERVAL_MINUTES,
            )))
            .await;
            let _waiting_guard = starnix_core::task::ThreadLockupDetector::pause_tracking();
            let long_running = starnix_core::task::ThreadLockupDetector::get_long_running_threads(
                zx::MonotonicDuration::from_minutes(LOCKUP_DETECTOR_INTERVAL_MINUTES),
            );
            if !long_running.is_empty() {
                let koids: Vec<zx::Koid> = long_running.iter().map(|r| r.koid).collect();
                log_error!(
                    "Detected threads locked up for more than {} minutes: {:?}",
                    LOCKUP_DETECTOR_INTERVAL_MINUTES,
                    koids
                );
                #[cfg(all(target_os = "fuchsia", not(doc)))]
                {
                    for registered in &long_running {
                        dump_thread_backtrace(&registered.thread).await;
                    }

                    let reporter = fuchsia_component::client::connect_to_protocol::<
                        fidl_fuchsia_feedback::CrashReporterMarker,
                    >();
                    match reporter {
                        Ok(reporter) => {
                            let report = fidl_fuchsia_feedback::CrashReport {
                                program_name: Some("starnix_kernel".to_string()),
                                crash_signature: Some(
                                    "fuchsia-starnix_kernel-thread-lockup".to_string(),
                                ),
                                is_fatal: Some(false),
                                annotations: Some(vec![fidl_fuchsia_feedback::Annotation {
                                    key: "starnix.lockup_koids".to_string(),
                                    value: format!(
                                        "{:?}",
                                        koids.iter().map(|k| k.raw_koid()).collect::<Vec<_>>()
                                    ),
                                }]),
                                ..Default::default()
                            };
                            match reporter.file_report(report).await {
                                Ok(Ok(_)) => log_debug!("Filed crash report for thread lockup."),
                                Ok(Err(e)) => {
                                    log_warn!(e:?; "Failed to file crash report for thread lockup.")
                                }
                                Err(e) => {
                                    log_warn!(e:?; "Failed to call file_report for thread lockup.")
                                }
                            }
                        }
                        Err(e) => {
                            log_warn!(e:?; "Failed to connect to CrashReporter");
                        }
                    }
                }
            }
        }
    });

    starnix_kernel_runner::initialize();
    let container = Rc::new(OnceCell::<Container>::new());
    let _lifecycle_task = maybe_serve_lifecycle(container.clone());

    let mut fs = ServiceFs::new_local();
    fs.dir("svc")
        .add_fidl_service_at("fuchsia.starnix.container.Runner", KernelServices::ContainerRunner)
        .add_fidl_service(KernelServices::ComponentRunner)
        .add_fidl_service(KernelServices::ContainerController)
        .add_fidl_service(KernelServices::MemoryAttributionProvider);

    let inspector = fuchsia_inspect::component::inspector();
    #[cfg(target_arch = "x86_64")]
    {
        inspector.root().record_string(
            "x86_64_extended_pstate_strategy",
            format!("{:?}", *extended_pstate::x86_64::PREFERRED_STRATEGY),
        );
    }
    inspector.root().record_lazy_child("not_found", starnix_logging::not_found_lazy_node_callback);
    inspector.root().record_lazy_child("stubs", starnix_logging::track_stub_lazy_node_callback);
    starnix_logging::register_stub_context_callback();

    log_debug!("Serving kernel services on outgoing directory handle.");
    fs.take_and_serve_directory_handle()?;
    health.set_ok();

    // We call this early during Starnix boot to make sure the usercopy utilities
    // are ready for use before any restricted-mode/Linux processes are created.
    init_usercopy();

    while let Some(request) = fs.next().await {
        match request {
            KernelServices::ContainerRunner(stream) => {
                let container = container.clone();
                let kernel_extra_features = kernel_extra_features.clone();
                fuchsia_async::Task::local(async move {
                    let mut config: Option<ContainerServiceConfig> = None;
                    let container = container
                        .get_or_try_init(|| async {
                            create_component_from_stream(stream, kernel_extra_features).await.map(
                                |(container, new_config)| {
                                    config = Some(new_config);
                                    container
                                },
                            )
                        })
                        .await
                        .expect("failed to start container");
                    if let Some(config) = config {
                        container
                            .serve(config)
                            .await
                            .expect("failed to serve the expected services from the container");
                    } else {
                        log_error!("No config provided for container, not running it.");
                    }
                })
                .detach();
            }
            KernelServices::ComponentRunner(stream) => {
                let container = container.clone();
                fuchsia_async::Task::local(async move {
                    serve_component_runner(stream, container.wait().await.system_task())
                        .await
                        .expect("failed to start component runner");
                })
                .detach();
            }
            KernelServices::ContainerController(stream) => {
                let container = container.clone();
                fuchsia_async::Task::local(async move {
                    serve_container_controller(stream, container.wait().await.system_task())
                        .await
                        .expect("failed to start container controller");
                })
                .detach();
            }
            KernelServices::MemoryAttributionProvider(stream) => {
                let container = container.clone();
                fuchsia_async::Task::local(async move {
                    serve_memory_attribution_provider_elfkernel(stream, container.wait().await)
                        .await
                        .expect("failed to start memory attribution provider");
                })
                .detach();
            }
        }
    }

    Ok(())
}
