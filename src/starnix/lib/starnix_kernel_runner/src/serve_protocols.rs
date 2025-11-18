// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::Container;
use anyhow::{Context as _, Error};
use fidl::AsHandleRef;
use fidl::endpoints::{ControlHandle, RequestStream, ServerEnd};
use fuchsia_async::{
    DurationExt, {self as fasync},
};
use futures::channel::oneshot;
use futures::{
    AsyncReadExt, AsyncWriteExt, Future, FutureExt, StreamExt, TryFutureExt, TryStreamExt, pin_mut,
    select,
};
use starnix_core::execution::{create_init_child_process, execute_task_with_prerun_result};
use starnix_core::fs::devpts::create_main_and_replica;
use starnix_core::fs::fuchsia::create_fuchsia_pipe;
use starnix_core::task::{CurrentTask, ExitStatus, FullCredentials, Kernel, ProcessEntryRef};
use starnix_core::vfs::buffers::{VecInputBuffer, VecOutputBuffer};
use starnix_core::vfs::file_server::serve_file_at;
use starnix_core::vfs::socket::VsockSocket;
use starnix_core::vfs::{FdFlags, FileHandle};
use starnix_logging::{log_error, log_warn};
use starnix_modules_framebuffer::Framebuffer;
use starnix_sync::{Locked, Unlocked};
use starnix_task_command::TaskCommand;
use starnix_types::ownership::TempRef;
use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::signals::UncheckedSignal;
use starnix_uapi::{errno, error, uapi};
use std::ffi::CString;
use std::ops::DerefMut;
use {
    fidl_fuchsia_component_runner as frunner, fidl_fuchsia_element as felement,
    fidl_fuchsia_io as fio, fidl_fuchsia_memory_attribution as fattribution,
    fidl_fuchsia_posix as fposix, fidl_fuchsia_starnix_binder as fbinder,
    fidl_fuchsia_starnix_container as fstarcontainer,
};

use super::start_component;

pub fn expose_root(
    locked: &mut Locked<Unlocked>,
    system_task: &CurrentTask,
    server_end: ServerEnd<fio::DirectoryMarker>,
) -> Result<(), Error> {
    let root_file = system_task.open_file(locked, "/".into(), OpenFlags::RDONLY)?;
    serve_file_at(
        server_end.into_channel().into(),
        system_task,
        &root_file,
        FullCredentials::for_kernel(),
    )?;
    Ok(())
}

pub async fn serve_component_runner(
    request_stream: frunner::ComponentRunnerRequestStream,
    system_task: &CurrentTask,
) -> Result<(), Error> {
    request_stream
        .try_for_each_concurrent(None, |event| async {
            match event {
                frunner::ComponentRunnerRequest::Start { start_info, controller, .. } => {
                    if let Err(e) = start_component(start_info, controller, system_task).await {
                        log_error!("failed to start component: {:?}", e);
                    }
                }
                frunner::ComponentRunnerRequest::_UnknownMethod { ordinal, .. } => {
                    log_warn!("Unknown ComponentRunner request: {ordinal}");
                }
            }
            Ok(())
        })
        .await
        .map_err(Error::from)
}

fn to_winsize(window_size: Option<fstarcontainer::ConsoleWindowSize>) -> uapi::winsize {
    window_size
        .map(|window_size| uapi::winsize {
            ws_row: window_size.rows,
            ws_col: window_size.cols,
            ws_xpixel: window_size.x_pixels,
            ws_ypixel: window_size.y_pixels,
        })
        .unwrap_or(uapi::winsize::default())
}

async fn spawn_console(
    kernel: &Kernel,
    payload: fstarcontainer::ControllerSpawnConsoleRequest,
) -> Result<Result<u8, fstarcontainer::SpawnConsoleError>, Error> {
    if let (Some(console_in), Some(console_out), Some(binary_path)) =
        (payload.console_in, payload.console_out, payload.binary_path)
    {
        let binary_path = CString::new(binary_path)?;
        let argv = payload
            .argv
            .unwrap_or(vec![])
            .into_iter()
            .map(CString::new)
            .collect::<Result<Vec<_>, _>>()?;
        let environ = payload
            .environ
            .unwrap_or(vec![])
            .into_iter()
            .map(CString::new)
            .collect::<Result<Vec<_>, _>>()?;
        let window_size = to_winsize(payload.window_size);
        let current_task = create_init_child_process(
            kernel.kthreads.unlocked_for_async().deref_mut(),
            &kernel.weak_self.upgrade().expect("Kernel must still be alive"),
            TaskCommand::new(binary_path.as_bytes()),
            None,
        )?;
        let (sender, receiver) = oneshot::channel();
        let pty = execute_task_with_prerun_result(
            kernel.kthreads.unlocked_for_async().deref_mut(),
            current_task,
            move |locked, current_task| {
                let executable = current_task.open_file(
                    locked,
                    binary_path.as_bytes().into(),
                    OpenFlags::RDONLY,
                )?;
                current_task.exec(locked, executable, binary_path, argv, environ)?;
                let (pty, pts) = create_main_and_replica(locked, &current_task, window_size)?;
                let fd_flags = FdFlags::empty();
                assert_eq!(0, current_task.add_file(locked, pts.clone(), fd_flags)?.raw());
                assert_eq!(1, current_task.add_file(locked, pts.clone(), fd_flags)?.raw());
                assert_eq!(2, current_task.add_file(locked, pts, fd_flags)?.raw());
                Ok(pty)
            },
            move |result| {
                let _ = match result {
                    Ok(ExitStatus::Exit(exit_code)) => sender.send(Ok(exit_code)),
                    _ => sender.send(Err(fstarcontainer::SpawnConsoleError::Canceled)),
                };
            },
            None,
        )?;
        let _ = forward_to_pty(kernel, console_in, console_out, pty).map_err(|e| {
            log_error!("failed to forward to terminal {:?}", e);
        });

        Ok(receiver.await?)
    } else {
        Ok(Err(fstarcontainer::SpawnConsoleError::InvalidArgs))
    }
}

pub async fn serve_container_controller(
    request_stream: fstarcontainer::ControllerRequestStream,
    system_task: &CurrentTask,
) -> Result<(), Error> {
    request_stream
        .map_err(Error::from)
        .try_for_each_concurrent(None, |event| async {
            match event {
                fstarcontainer::ControllerRequest::VsockConnect {
                    payload:
                        fstarcontainer::ControllerVsockConnectRequest { port, bridge_socket, .. },
                    ..
                } => {
                    let Some(port) = port else {
                        log_warn!("vsock connection missing port");
                        return Ok(());
                    };
                    let Some(bridge_socket) = bridge_socket else {
                        log_warn!("vsock connection missing bridge_socket");
                        return Ok(());
                    };
                    connect_to_vsock(port, bridge_socket, system_task).await.unwrap_or_else(|e| {
                        log_error!("failed to connect to vsock {:?}", e);
                    });
                }
                fstarcontainer::ControllerRequest::SpawnConsole { payload, responder } => {
                    responder.send(spawn_console(system_task.kernel(), payload).await?)?;
                }
                fstarcontainer::ControllerRequest::GetVmoReferences { payload, responder } => {
                    if let Some(koid) = payload.koid {
                        let thread_groups = system_task
                            .kernel()
                            .pids
                            .read()
                            .get_thread_groups()
                            .map(TempRef::into_static)
                            .collect::<Vec<_>>();
                        let mut results = vec![];
                        for thread_group in thread_groups {
                            if let Some(leader) =
                                system_task.get_task(thread_group.leader).upgrade()
                            {
                                let fds = leader.files.get_all_fds();
                                for fd in fds {
                                    if let Ok(file) = leader.files.get(fd) {
                                        if let Ok(memory) = file.get_memory(
                                            system_task
                                                .kernel()
                                                .kthreads
                                                .unlocked_for_async()
                                                .deref_mut(),
                                            system_task,
                                            None,
                                            starnix_core::mm::ProtectionFlags::READ,
                                        ) {
                                            let memory_koid = memory
                                                .info()
                                                .expect("Failed to get memory info")
                                                .koid;
                                            if memory_koid.raw_koid() == koid {
                                                let process_name = thread_group
                                                    .process
                                                    .get_name()
                                                    .unwrap_or_default();
                                                results.push(fstarcontainer::VmoReference {
                                                    process_name: Some(process_name.to_string()),
                                                    pid: Some(leader.get_pid() as u64),
                                                    fd: Some(fd.raw()),
                                                    koid: Some(koid),
                                                    ..Default::default()
                                                });
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        let _ =
                            responder.send(&fstarcontainer::ControllerGetVmoReferencesResponse {
                                references: Some(results),
                                ..Default::default()
                            });
                    }
                }
                fstarcontainer::ControllerRequest::GetJobHandle { responder } => {
                    let _result = responder.send(fstarcontainer::ControllerGetJobHandleResponse {
                        job: Some(
                            fuchsia_runtime::job_default()
                                .duplicate(zx::Rights::SAME_RIGHTS)
                                .expect("Failed to dup handle"),
                        ),
                        ..Default::default()
                    });
                }
                fstarcontainer::ControllerRequest::SendSignal {
                    payload:
                        fstarcontainer::ControllerSendSignalRequest {
                            pid: Some(pid),
                            signal: Some(signal),
                            ..
                        },
                    responder,
                } => {
                    let pids = system_task.kernel().pids.read();
                    if let Some(ProcessEntryRef::Process(target_thread_group)) =
                        pids.get_process(pid)
                    {
                        #[allow(
                            clippy::undocumented_unsafe_blocks,
                            reason = "Force documented unsafe blocks in Starnix"
                        )]
                        match unsafe {
                            target_thread_group.send_signal_unchecked_debug(
                                system_task,
                                UncheckedSignal::new(signal),
                            )
                        } {
                            Ok(_) => {
                                let _result = responder.send(Ok(()));
                            }
                            Err(_) => {
                                let _result =
                                    responder.send(Err(fstarcontainer::SignalError::InvalidSignal));
                            }
                        };
                    } else {
                        let _result =
                            responder.send(Err(fstarcontainer::SignalError::InvalidTarget));
                    };
                }
                // The request did not contain both a signal and a target pid.
                fstarcontainer::ControllerRequest::SendSignal { responder, .. } => {
                    log_error!("malformed SendSignal request");
                    let _result = responder.send(Err(fstarcontainer::SignalError::InvalidTarget));
                }
                fstarcontainer::ControllerRequest::_UnknownMethod { .. } => (),
            }
            Ok(())
        })
        .await
}

async fn connect_to_vsock(
    port: u32,
    bridge_socket: fidl::Socket,
    system_task: &CurrentTask,
) -> Result<(), Error> {
    let socket = loop {
        if let Ok(socket) = system_task.kernel().default_abstract_vsock_namespace.lookup(&port) {
            break socket;
        };
        fasync::Timer::new(fasync::MonotonicDuration::from_millis(100).after_now()).await;
    };

    let pipe = create_fuchsia_pipe(
        system_task.kernel().kthreads.unlocked_for_async().deref_mut(),
        system_task,
        bridge_socket,
        OpenFlags::RDWR | OpenFlags::NONBLOCK,
    )?;
    socket.downcast_socket::<VsockSocket>().unwrap().remote_connection(
        system_task.kernel().kthreads.unlocked_for_async().deref_mut(),
        &socket,
        system_task,
        pipe,
    )?;

    Ok(())
}

fn forward_to_pty(
    kernel: &Kernel,
    console_in: fidl::Socket,
    console_out: fidl::Socket,
    pty: FileHandle,
) -> Result<(), Error> {
    // Matches fuchsia.io.Transfer capacity, somewhat arbitrarily.
    const BUFFER_CAPACITY: usize = 8192;

    let mut rx = fuchsia_async::Socket::from_socket(console_in);
    let mut tx = fuchsia_async::Socket::from_socket(console_out);
    let pty_sink = pty.clone();
    kernel.kthreads.spawn_async(async move |locked_and_task| {
        let _result: Result<(), Error> = (async || {
            let mut buffer = vec![0u8; BUFFER_CAPACITY];
            loop {
                let bytes = rx.read(&mut buffer[..]).await?;
                if bytes == 0 {
                    return Ok(());
                }
                pty_sink.write(
                    &mut locked_and_task.unlocked(),
                    locked_and_task.current_task(),
                    &mut VecInputBuffer::new(&buffer[..bytes]),
                )?;
            }
        })()
        .await;
    });

    let pty_source = pty;
    kernel.kthreads.spawn({
        move |locked, current_task| {
            let _result: Result<(), Error> =
                fasync::LocalExecutor::default().run_singlethreaded(async {
                    let mut buffer = VecOutputBuffer::new(BUFFER_CAPACITY);
                    loop {
                        buffer.reset();
                        let bytes = pty_source.read(locked, current_task, &mut buffer)?;
                        if bytes == 0 {
                            return Ok(());
                        }
                        tx.write_all(buffer.data()).await?;
                    }
                });
        }
    });

    Ok(())
}

pub async fn serve_graphical_presenter(
    mut request_stream: felement::GraphicalPresenterRequestStream,
    kernel: &Kernel,
) -> Result<(), Error> {
    while let Some(request) = request_stream.next().await {
        match request.context("reading graphical presenter request")? {
            felement::GraphicalPresenterRequest::PresentView {
                view_spec,
                annotation_controller: _,
                view_controller_request: _,
                responder,
            } => match view_spec.viewport_creation_token {
                Some(token) => {
                    let fb = Framebuffer::get(kernel).context("getting framebuffer from kernel")?;
                    fb.present_view(token);
                    let _ = responder.send(Ok(()));
                }
                None => {
                    let _ = responder.send(Err(felement::PresentViewError::InvalidArgs));
                }
            },
        }
    }
    Ok(())
}

/// Serves the memory attribution provider for the Kernel ELF component.
pub fn serve_memory_attribution_provider_elfkernel(
    mut request_stream: fattribution::ProviderRequestStream,
    container: &Container,
) -> impl Future<Output = Result<(), Error>> {
    let observer = container.new_memory_attribution_observer(request_stream.control_handle());
    async move {
        while let Some(event) = request_stream.try_next().await? {
            match event {
                fattribution::ProviderRequest::Get { responder } => {
                    observer.next(responder);
                }
                fattribution::ProviderRequest::_UnknownMethod {
                    ordinal, control_handle, ..
                } => {
                    log_error!("Invalid request to AttributionProvider: {ordinal}");
                    control_handle.shutdown_with_epitaph(zx::Status::INVALID_ARGS);
                }
            }
        }
        Ok(())
    }
}

/// Serves the memory attribution provider for the Container component.
pub fn serve_memory_attribution_provider_container(
    mut request_stream: fattribution::ProviderRequestStream,
    kernel: &Kernel,
) -> impl Future<Output = ()> {
    let observer = kernel.new_memory_attribution_observer(request_stream.control_handle());
    async move {
        while let Some(event) = request_stream
            .try_next()
            .await
            .inspect_err(|err| {
                log_warn!("Error while serving container memory attribution: {:?}", err)
            })
            .ok()
            .flatten()
        {
            match event {
                fattribution::ProviderRequest::Get { responder } => {
                    observer.next(responder);
                }
                fattribution::ProviderRequest::_UnknownMethod {
                    ordinal, control_handle, ..
                } => {
                    log_error!("Invalid request to AttributionProvider: {ordinal}");
                    control_handle.shutdown_with_epitaph(zx::Status::INVALID_ARGS);
                }
            }
        }
    }
}

async fn select_first<O>(f1: impl Future<Output = O>, f2: impl Future<Output = O>) -> O {
    let f1 = f1.fuse();
    let f2 = f2.fuse();
    pin_mut!(f1, f2);
    select! {
        f1 = f1 => f1,
        f2 = f2 => f2,
    }
}

/// Serve the LutexController protocol.
pub async fn serve_lutex_controller(
    request_stream: fbinder::LutexControllerRequestStream,
    current_task: &CurrentTask,
) -> Result<(), Error> {
    let kernel = current_task.kernel();
    request_stream
        .map_err(Error::from)
        .try_for_each_concurrent(None, |event| async move {
            match event {
                fbinder::LutexControllerRequest::WaitBitset { payload, responder } => {
                    let deadline_and_receiver = (|| {
                        let mut unlocked = kernel.kthreads.unlocked_for_async();
                        let vmo = payload.vmo.ok_or_else(|| errno!(EINVAL))?;
                        let offset = payload.offset.ok_or_else(|| errno!(EINVAL))?;
                        let value = payload.value.ok_or_else(|| errno!(EINVAL))?;
                        let mask = payload.mask.unwrap_or(u32::MAX);
                        let deadline = payload.deadline.map(zx::MonotonicInstant::from_nanos);
                        kernel
                            .shared_futexes
                            .external_wait(&mut unlocked, vmo.into(), offset, value, mask)
                            .map(|receiver| (deadline, receiver))
                    })();
                    let result = match deadline_and_receiver {
                        Ok((deadline, receiver)) => {
                            let receiver = receiver.map_err(|_| errno!(EINTR));
                            if let Some(deadline) = deadline {
                                let timer = fasync::Timer::new(deadline).map(|_| error!(ETIMEDOUT));
                                select_first(timer, receiver).await
                            } else {
                                receiver.await
                            }
                        }
                        Err(e) => Err(e),
                    };
                    let result = result.map_err(|e: Errno| {
                        fposix::Errno::from_primitive(e.code.error_code() as i32)
                            .unwrap_or(fposix::Errno::Einval)
                    });
                    responder
                        .send(result)
                        .context("Unable to send LutexControllerRequest::WaitBitset response")?;
                }
                fbinder::LutexControllerRequest::WakeBitset { payload, responder } => {
                    let result = (|| {
                        let mut unlocked = kernel.kthreads.unlocked_for_async();
                        let vmo = payload.vmo.ok_or_else(|| errno!(EINVAL))?;
                        let offset = payload.offset.ok_or_else(|| errno!(EINVAL))?;
                        let count = payload.count.ok_or_else(|| errno!(EINVAL))?;
                        let mask = payload.mask.unwrap_or(u32::MAX);
                        kernel.shared_futexes.external_wake(
                            &mut unlocked,
                            vmo.into(),
                            offset,
                            count as usize,
                            mask,
                        )
                    })();
                    let result = result
                        .map(|count| fbinder::WakeResponse {
                            count: Some(count as u64),
                            ..fbinder::WakeResponse::default()
                        })
                        .map_err(|e: Errno| {
                            fposix::Errno::from_primitive(e.code.error_code() as i32)
                                .unwrap_or(fposix::Errno::Einval)
                        });
                    responder
                        .send(result)
                        .context("Unable to send LutexControllerRequest::WakeBitset response")?;
                }
                fbinder::LutexControllerRequest::_UnknownMethod { ordinal, .. } => {
                    log_warn!("Unknown LutexController ordinal: {}", ordinal);
                }
            }
            Ok(())
        })
        .await
        .context("failed fbinder::LutexController request")
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use fidl::HandleBased;
    use starnix_core::testing::*;
    use {fidl_fuchsia_posix as fposix, zx};

    #[fuchsia::test]
    async fn lutex_controller_test() {
        spawn_kernel_and_run(async |mut _locked, current_task| {
            let (sender, receiver) = oneshot::channel::<()>();
            current_task.kernel.kthreads.spawn_future({
                let kernel = current_task.kernel.clone();
                async move || {
                    let (lutex_controller, stream) = fidl::endpoints::create_proxy_and_stream::<
                        fbinder::LutexControllerMarker,
                    >();

                    // Spawn the server
                    let server_fut = serve_lutex_controller(stream, kernel.kthreads.system_task());

                    let client_fut = async move {
                        const VMO_SIZE: usize = 4 * 1024;
                        let vmo = zx::Vmo::create(VMO_SIZE as u64).expect("Vmo::create");
                        // Wait on an incorrect value.
                        let wait = lutex_controller
                            .wait_bitset(fbinder::WaitBitsetRequest {
                                vmo: Some(
                                    vmo.duplicate_handle(zx::Rights::SAME_RIGHTS)
                                        .expect("duplicate vmo"),
                                ),
                                offset: Some(0),
                                value: Some(1),
                                ..Default::default()
                            })
                            .await
                            .expect("got_answer");
                        assert_matches!(wait, Err(fposix::Errno::Eagain));

                        // Wait with a timeout
                        let wait = lutex_controller
                            .wait_bitset(fbinder::WaitBitsetRequest {
                                vmo: Some(
                                    vmo.duplicate_handle(zx::Rights::SAME_RIGHTS)
                                        .expect("duplicate vmo"),
                                ),
                                offset: Some(0),
                                value: Some(0),
                                deadline: Some(0),
                                ..Default::default()
                            })
                            .await
                            .expect("got_answer");
                        assert_matches!(wait, Err(fposix::Errno::Etimedout));

                        let mut wait = Box::pin(
                            lutex_controller.wait_bitset(fbinder::WaitBitsetRequest {
                                vmo: Some(
                                    vmo.duplicate_handle(zx::Rights::SAME_RIGHTS)
                                        .expect("duplicate vmo"),
                                ),
                                offset: Some(0),
                                value: Some(0),
                                deadline: None,
                                ..Default::default()
                            }),
                        );
                        // The wait is correct, the future should stay pending until a wake.
                        assert!(futures::poll!(&mut wait).is_pending());

                        let waken_up: fbinder::WakeResponse = lutex_controller
                            .wake_bitset(fbinder::WakeBitsetRequest {
                                vmo: Some(
                                    vmo.duplicate_handle(zx::Rights::SAME_RIGHTS)
                                        .expect("duplicate vmo"),
                                ),
                                offset: Some(0),
                                count: Some(1),
                                mask: None,
                                ..Default::default()
                            })
                            .await
                            .expect("wake_answer")
                            .expect("wake_response");
                        assert_eq!(waken_up.count, Some(1));

                        // The wait should now return.
                        assert!(wait.await.expect("await_answer").is_ok());
                    };

                    let (server_res, _) = futures::join!(server_fut, client_fut);
                    server_res.expect("server failed");
                    let _ = sender.send(());
                }
            });
            receiver.await.expect("test failed");
        })
        .await;
    }
}
