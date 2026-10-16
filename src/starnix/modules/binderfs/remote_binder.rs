// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::binder::{BinderDevice, BinderDriver, RemoteBinderConnection};
use crate::trace::{
    CATEGORY_STARNIX_BINDER, NAME_REMOTE_BINDER_IOCTL, NAME_REMOTE_BINDER_IOCTL_FIDL_REPLY,
    NAME_REMOTE_BINDER_IOCTL_SEND_WORK, NAME_REMOTE_BINDER_IOCTL_WORKER_PROCESS,
};
use anyhow::{Context, Error};
use derivative::Derivative;
use fidl::AsHandleRef;
use fidl::endpoints::{ClientEnd, ControlHandle, RequestStream, ServerEnd};
use fidl_fuchsia_posix as fposix;
use fidl_fuchsia_starnix_binder as fbinder;
use fuchsia_async as fasync;
use futures::channel::oneshot;
use futures::future::FutureExt;
use futures::task::Poll;
use futures::{Future, Stream, StreamExt, TryStreamExt, pin_mut, select};
use starnix_core::device::{DeviceMode, DeviceOps};
use starnix_core::mm::memory::MemoryObject;
use starnix_core::mm::{DesiredAddress, MappingOptions, MemoryAccessorExt, ProtectionFlags};
use starnix_core::power::{ContainerWakingStream, OwnedMessageCounterHandle, WakeupSourceOrigin};
use starnix_core::task::dynamic_thread_spawner::SpawnRequestBuilder;
use starnix_core::task::{CurrentTask, Kernel, Pid, ThreadGroup, WaitQueue, Waiter};
use starnix_core::vfs::buffers::{InputBuffer, OutputBuffer};
use starnix_core::vfs::{
    FileObject, FileObjectState, FileOps, FsString, NamespaceNode, fileops_impl_nonseekable,
    fileops_impl_noop_sync,
};
use starnix_lifecycle::DropWaiter;
use starnix_logging::{log_error, log_warn};
use starnix_sync::{
    LockDepGuard, LockDepMutex, RemoteBinderHandleLevel, RemoteBinderResponderLock,
};
use starnix_syscalls::{SUCCESS, SyscallArg, SyscallResult};
use starnix_uapi::device_id::DeviceId;
use starnix_uapi::errors::{EAGAIN, EINTR, Errno, ErrnoCode};
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::user_address::{UserAddress, UserCStringPtr, UserRef};
use starnix_uapi::vfs::FdEvents;
use starnix_uapi::{errno, errno_from_code, error, uapi};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::{Arc, Weak};
use zx::{self, Peered};

const EXECUTOR_THREAD_ROLE: &str = "fuchsia.starnix.remote_binder.executor";

const WAKE_LOCK_ACQUIRED_SIGNAL: zx::Signals = zx::Signals::USER_0;

/// Wait key used by the thread group leader in `RemoteBinderHandle::waiters`.
/// Linux TIDs are strictly positive (`>= 1`), so `0` uniquely identifies the leader thread.
const MAIN_THREAD_WAIT_KEY: u64 = 0;

trait RemoteControllerConnector: Send + Sync + 'static {
    fn connect_to_remote_controller(
        current_task: &CurrentTask,
        service_name: &str,
    ) -> Result<ClientEnd<fbinder::RemoteControllerMarker>, Errno>;
}

struct DefaultRemoteControllerConnector;

impl RemoteControllerConnector for DefaultRemoteControllerConnector {
    fn connect_to_remote_controller(
        current_task: &CurrentTask,
        service_name: &str,
    ) -> Result<ClientEnd<fbinder::RemoteControllerMarker>, Errno> {
        current_task
            .kernel()
            .connect_to_named_protocol_at_container_svc::<fbinder::RemoteControllerMarker>(
                service_name,
            )
    }
}

/// Device for starting a remote fuchsia component with access to the binder drivers on the starnix
/// container.
#[derive(Clone)]
pub struct RemoteBinderDevice {}

impl DeviceOps for RemoteBinderDevice {
    fn open(
        &self,
        current_task: &CurrentTask,
        _id: DeviceId,
        _node: &NamespaceNode,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        Ok(RemoteBinderFileOps::new(current_task))
    }
}

struct RemoteBinderFileOps(Arc<RemoteBinderHandle<DefaultRemoteControllerConnector>>);

impl RemoteBinderFileOps {
    fn new(current_task: &CurrentTask) -> Box<Self> {
        Box::new(Self(RemoteBinderHandle::<DefaultRemoteControllerConnector>::new(current_task)))
    }
}

impl FileOps for RemoteBinderFileOps {
    fileops_impl_nonseekable!();
    fileops_impl_noop_sync!();

    fn query_events(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
    ) -> Result<FdEvents, Errno> {
        Ok(FdEvents::empty())
    }

    fn close(self: Box<Self>, _file: &FileObjectState, _current_task: &CurrentTask) {
        self.0.close();
    }

    fn ioctl(
        &self,
        _file: &FileObject,
        current_task: &CurrentTask,
        request: u32,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        self.0.ioctl(current_task, request, arg)
    }

    fn get_memory(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _length: Option<usize>,
        _prot: ProtectionFlags,
    ) -> Result<Arc<MemoryObject>, Errno> {
        error!(EOPNOTSUPP)
    }

    fn mmap(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _addr: DesiredAddress,
        _memory_offset: u64,
        _length: usize,
        _prot_flags: ProtectionFlags,
        _mapping_options: MappingOptions,
    ) -> Result<UserAddress, Errno> {
        error!(EOPNOTSUPP)
    }

    fn read(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        offset: usize,
        _data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        debug_assert!(offset == 0);
        error!(EOPNOTSUPP)
    }

    fn write(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        offset: usize,
        _data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        debug_assert!(offset == 0);
        error!(EOPNOTSUPP)
    }
}

/// The type of the responder function used in `TaskRequest` to send the result of a FIDL request
/// directly from the handler thread.
type SynchronousResponder<T> = Box<dyn FnOnce(Result<T, Errno>) -> Result<(), fidl::Error> + Send>;

/// Request sent from the FIDL server thread to the running tasks. The requests that require a
/// response send a `Sender` to let the task return the response.
#[derive(Derivative)]
#[derivative(Debug)]
enum TaskRequest {
    /// Set the associated vmo for the binder connection. See the SetVmo and MapVmo methods in the
    /// Binder FIDL protocol.
    SetVmo {
        #[derivative(Debug = "ignore")]
        remote_binder_connection: Arc<RemoteBinderConnection>,
        vmo: fidl::Vmo,
        mapped_address: u64,
        // a synchronous function avoids thread hops.
        #[derivative(Debug = "ignore")]
        responder: SynchronousResponder<()>,
    },
    /// Execute the given ioctl. See the Ioctl method in the Binder FIDL
    /// protocol.
    Ioctl {
        // remote_binder_connection,
        #[derivative(Debug = "ignore")]
        remote_binder_connection: Arc<RemoteBinderConnection>,
        request: u32,
        arg: u64,
        koid: u64,
        vmo: zx::Vmo,
        ioctl_reads: Vec<fbinder::IoctlReadWrite>,
        files: Vec<fbinder::FileHandle>,
        // a synchronous function avoids thread hops.
        #[derivative(Debug = "ignore")]
        responder: SynchronousResponder<Vec<fbinder::IoctlReadWrite>>,
    },
    /// Open the binder device driver situated at `path` in the Task filesystem namespace.
    Open {
        path: FsString,
        process_accessor: ClientEnd<fbinder::ProcessAccessorMarker>,
        process: zx::Process,
        responder: oneshot::Sender<Result<Arc<RemoteBinderConnection>, Errno>>,
    },
    /// Have the task return to userspace. `spawn_thread` must be returned to the caller through
    /// the ioctl command arg.
    Return { spawn_thread: bool },
}

/// A `TaskRequest` that is executed by a dedicated worker task. When `koid` is `Some`, the thread
/// koid is associated 1 to 1 with a Starnix task and only that task must handle requests for it.
#[derive(Debug)]
struct BoundTaskRequest {
    koid: Option<u64>,
    request: TaskRequest,
}

/// Returns the Errno in result if it is either EINTR or EAGAIN, None otherwise.
fn must_interrupt<R>(result: &Result<R, Errno>) -> Option<Errno> {
    match result {
        Err(e) if *e == EINTR => Some(errno!(EINTR)),
        Err(e) if *e == EAGAIN => Some(errno!(EAGAIN)),
        _ => None,
    }
}

#[must_use]
enum NotificationType {
    /// Wake up all the tasks.
    All,
    /// Wake up the thread group leader.
    Leader,
    /// Wake up the worker task with the given tid.
    Task(Pid),
}

/// Connection to the remote binder device. One connection is associated to one instance of a
/// remote fuchsia component.
struct RemoteBinderHandle<F: RemoteControllerConnector> {
    kernel: Arc<Kernel>,
    state: LockDepMutex<RemoteBinderHandleState, RemoteBinderHandleLevel>,
    waiters: WaitQueue,

    /// Marker struct, needed because the struct is parametrized by `F`.
    _phantom: std::marker::PhantomData<F>,
}

/// The state of the current request for a given task.
#[derive(Debug)]
enum PendingRequest {
    /// No request pending, the task is ready to accept a new one.
    None,
    /// A request is currently running. The task should not receive a new request.
    Running,
    /// The request the task should run. The task should not receive a new request.
    Some(BoundTaskRequest),
}

impl PendingRequest {
    /// Take the current pending request, if there is one. In this case, the state will move to
    /// `Running`.
    fn take(&mut self) -> Option<BoundTaskRequest> {
        match self {
            Self::Some(_) => {
                let value = std::mem::replace(self, PendingRequest::Running);
                if let Self::Some(v) = value {
                    Some(v)
                } else {
                    panic!();
                }
            }
            _ => None,
        }
    }

    /// Whether a request is currently waiting or running.
    fn is_pending(this: Option<Self>) -> bool {
        matches!(this, Some(Self::Running | Self::Some(_)))
    }
}

/// Internal state of RemoteBinderHandle.
///
/// This struct keeps the state of the local starnix tasks and the remote process and its threads.
/// The thread group leader is kept as an interruptible controller thread that handles taskless
/// requests (`Open`, `SetVmo`, `Return { spawn_thread: true }`) and process signals.
/// Each remote worker thread is associated to a single non-leader starnix worker task so that all
/// ioctls from the remote thread are executed by the same starnix task.
#[derive(Derivative)]
#[derivative(Debug)]
struct RemoteBinderHandleState {
    /// The thread_group of the tasks that interact with this remote binder.
    thread_group: Weak<ThreadGroup>,

    /// Mapping from the koid of the remote thread to the local task.
    koid_to_task: HashMap<u64, Pid>,

    /// Set of worker tasks that contacted the remote binder device driver but are not yet
    /// associated to a remote thread. Once associated, a task will have an entry in
    /// `pending_requests`.
    unassigned_tasks: HashSet<Pid>,

    /// Pending request for each associated worker task. Once a task is registered and associated
    /// with a remote thread, it will have an entry in this map. If the entry is `None`, it has no
    /// work to do, otherwise, it must execute the given request.
    pending_requests: HashMap<Pid, PendingRequest>,

    /// Queue of requests that must be executed by a worker task and for which no assigned task
    /// exists yet. The next time an unassigned worker task requires a new request, the first
    /// request will be retrieved and associated with that task.
    unassigned_requests: VecDeque<BoundTaskRequest>,

    /// Queue of requests handled by the thread group leader (`Open`, `SetVmo`, `Return`).
    taskless_requests: VecDeque<TaskRequest>,

    /// If present, any ioctl should immediately return the given value. Used to end the userspace
    /// process.
    exit: Option<Result<(), ErrnoCode>>,

    /// Channels that must receive an element at the time the handle exits.
    exit_notifiers: Vec<oneshot::Sender<()>>,
}

impl<F: RemoteControllerConnector> RemoteBinderHandle<F> {
    fn lock(&self) -> LockDepGuard<'_, RemoteBinderHandleState> {
        self.state.lock()
    }
}

impl RemoteBinderHandleState {
    /// Signal all task that they must exit.
    fn exit(&mut self, result: Result<(), Errno>) -> NotificationType {
        // The task requests in state may refer to async FIDL streams and must be dropped before
        // dropping the executor.
        self.koid_to_task.clear();
        self.unassigned_tasks.clear();
        self.pending_requests.clear();
        self.unassigned_requests.clear();
        self.taskless_requests.clear();

        self.exit = Some(result.map_err(|e| e.code));
        for notifier in std::mem::take(&mut self.exit_notifiers) {
            let _ = notifier.send(());
        }
        NotificationType::All
    }

    /// Enqueue a request for a worker task.
    fn enqueue_task_request(&mut self, request: BoundTaskRequest) -> NotificationType {
        debug_assert!(
            request.koid.is_none()
                || self.unassigned_requests.iter().all(|r| r.koid != request.koid)
        );
        if let Some(tid) = request.koid.and_then(|koid| self.koid_to_task.get(&koid).cloned()) {
            // Find the task associated with the given koid. If one exists, we enqueue the request
            // for the task. The task should never already have a task enqueued, as otherwise, it
            // should be blocked on a syscall, and should not be able to send another one.
            if PendingRequest::is_pending(
                self.pending_requests.insert(tid.clone(), PendingRequest::Some(request)),
            ) {
                log_error!("A single thread received 2 concurrent requests.");
                return self.exit(error!(EINVAL));
            }
            NotificationType::Task(tid)
        } else if let Some(tid) = self.unassigned_tasks.iter().next().cloned() {
            // There was no task associated with the koid, but there exists an unassigned task.
            // Associate the task with the koid (if any), and insert the pending request.
            self.unassigned_tasks.remove(&tid);
            if let Some(koid) = request.koid {
                self.koid_to_task.insert(koid, tid.clone());
            }
            if PendingRequest::is_pending(
                self.pending_requests.insert(tid.clone(), PendingRequest::Some(request)),
            ) {
                log_error!("A single thread received 2 concurrent requests.");
                return self.exit(error!(EINVAL));
            }
            NotificationType::Task(tid)
        } else {
            // Add the request to the unassigned queue.
            self.unassigned_requests.push_back(request);
            // No unassigned task ready. Request the leader thread to spawn a new worker in
            // userspace.
            self.enqueue_taskless_request(TaskRequest::Return { spawn_thread: true })
        }
    }

    /// Enqueue a request to be run by the thread group leader.
    fn enqueue_taskless_request(&mut self, request: TaskRequest) -> NotificationType {
        self.taskless_requests.push_back(request);
        NotificationType::Leader
    }

    /// Called when a worker task starts waiting.
    fn register_waiting_task(&mut self, tid: &Pid) {
        if self.pending_requests.contains_key(tid) || self.unassigned_tasks.contains(tid) {
            // The task is already registered.
            return;
        }
        // This is the first time the task is seen.
        if let Some(request) = self.unassigned_requests.pop_front() {
            // There is an unassigned request. Associate it to the task.
            if let Some(koid) = request.koid {
                self.koid_to_task.insert(koid, tid.clone());
            }
            self.pending_requests.insert(tid.clone(), PendingRequest::Some(request));
        } else {
            // Otherwise, mark the task as unassigned and available.
            self.unassigned_tasks.insert(tid.clone());
        }
    }
}

impl<F: RemoteControllerConnector> RemoteBinderHandle<F> {
    fn new(current_task: &CurrentTask) -> Arc<Self> {
        Arc::new(Self {
            kernel: current_task.kernel().clone(),
            state: RemoteBinderHandleState {
                thread_group: Arc::downgrade(&current_task.thread_group()),
                koid_to_task: Default::default(),
                unassigned_tasks: Default::default(),
                unassigned_requests: Default::default(),
                pending_requests: Default::default(),
                taskless_requests: Default::default(),
                exit: Default::default(),
                exit_notifiers: Default::default(),
            }
            .into(),
            waiters: Default::default(),
            _phantom: Default::default(),
        })
    }

    fn notify(&self, notification: NotificationType) {
        match notification {
            NotificationType::All => self.waiters.notify_all(),
            NotificationType::Leader => self.waiters.notify_value(MAIN_THREAD_WAIT_KEY),
            NotificationType::Task(tid) => self.waiters.notify_value(tid.id as u64),
        }
    }

    fn exit(&self, result: Result<(), Errno>) {
        let notification = self.lock().exit(result);
        self.notify(notification);
    }

    fn close(self: &Arc<Self>) {
        self.exit(Ok(()))
    }

    fn ioctl(
        self: &Arc<Self>,
        current_task: &CurrentTask,
        request: u32,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        let user_addr = UserAddress::from(arg);
        match request {
            #[allow(unreachable_patterns)]
            uapi::REMOTE_BINDER_START | uapi::arch32::REMOTE_BINDER_START => {
                self.start(current_task, UserCStringPtr::new(current_task, user_addr))?
            }
            uapi::REMOTE_BINDER_WAIT => self.wait(current_task, user_addr.into())?,
            _ => return error!(ENOTSUP),
        }
        Ok(SUCCESS)
    }

    /// Make a callback that can delegate to a FIDL async channel from a non-executor thread
    /// without crashing if the executor is dropped before the callback.
    ///
    /// For this, this builds a pair of a `SynchronousResponder` and a future such that:
    /// - The future resolve once the `SynchronousResponder` is called on another thread.
    /// - The responder is passed to the `SynchronousResponder` but is guaranteed to always be
    ///   dropped from the executor the future is bound to.
    /// - The responder is not called if the future is dropped.
    ///
    /// To use this, one should use the returned responder when they want `f` to be run, and ensure
    /// that the returned future is either waited, or dropped on the executor thread.
    fn make_synchronous_responder<T, R: Send + 'static, C>(
        responder: R,
        f: C,
    ) -> (SynchronousResponder<T>, impl Future<Output = ()>)
    where
        C: FnOnce(R, Result<T, Errno>) -> Result<(), fidl::Error> + Send + 'static,
    {
        let (tx, rx) = futures::channel::oneshot::channel();
        let responder =
            Arc::new(LockDepMutex::<_, RemoteBinderResponderLock>::new(Some(responder)));
        let closure = Box::new({
            let responder = Arc::downgrade(&responder);
            move |e| {
                scopeguard::defer! {
                    let _ = tx.send(());
                }
                if let Some(responder) = responder.upgrade() {
                    let mut guard = responder.lock();
                    // Keep the guard lock when the responder is on the stack to ensure that the
                    // executor is not dropped while the responder is still alive.
                    if let Some(responder) = guard.take() {
                        return f(responder, e);
                    }
                }
                Ok(())
            }
        });
        let waiter = async move {
            // Drop the responder in a scopeguard to ensure the responder is dropped even if the
            // future is cancelled.
            scopeguard::defer! {
                std::mem::drop(responder.lock().take());
            }
            let _ = rx.await;
        };
        (closure, waiter)
    }

    async fn serve_binder_request(
        self: &Arc<Self>,
        remote_binder_connection: Arc<RemoteBinderConnection>,
        request: fbinder::BinderRequest,
    ) -> Result<(), Error> {
        match request {
            fbinder::BinderRequest::SetVmo { vmo, mapped_address, control_handle } => {
                let (responder, waiter) =
                    Self::make_synchronous_responder(control_handle, |control_handle, e| {
                        if e.is_err() {
                            control_handle.shutdown();
                        }
                        Ok(())
                    });
                self.enqueue_taskless_request(TaskRequest::SetVmo {
                    remote_binder_connection: remote_binder_connection.clone(),
                    vmo,
                    mapped_address,
                    responder,
                });
                waiter.await;
            }
            fbinder::BinderRequest::MapVmo { vmo, mapped_address, responder } => {
                // Respond once the VMO is mapped, so that the client can wait for it before
                // sending ioctls, which the driver handles concurrently with this request.
                let (responder, waiter) =
                    Self::make_synchronous_responder(responder, |responder, result| {
                        responder.send(result.map_err(|e| {
                            fposix::Errno::from_primitive(e.code.error_code() as i32)
                                .unwrap_or(fposix::Errno::Einval)
                        }))
                    });
                self.enqueue_taskless_request(TaskRequest::SetVmo {
                    remote_binder_connection: remote_binder_connection.clone(),
                    vmo,
                    mapped_address,
                    responder,
                });
                waiter.await;
            }
            fbinder::BinderRequest::Ioctl {
                tid,
                request,
                arg,
                vmo,
                ioctl_reads,
                files,
                responder,
            } => {
                let waiter = {
                    fuchsia_trace::duration!(
                        CATEGORY_STARNIX_BINDER,
                        NAME_REMOTE_BINDER_IOCTL_SEND_WORK,
                        "request" => request
                    );
                    fuchsia_trace::flow_begin!(
                        CATEGORY_STARNIX_BINDER,
                        NAME_REMOTE_BINDER_IOCTL,
                        tid.into(),
                        "request" => request
                    );

                    let (responder, waiter) =
                        Self::make_synchronous_responder::<Vec<fbinder::IoctlReadWrite>, _, _>(
                            responder,
                            move |responder, e| {
                                fuchsia_trace::duration!(
                                    CATEGORY_STARNIX_BINDER,
                                    NAME_REMOTE_BINDER_IOCTL_FIDL_REPLY
                                );
                                fuchsia_trace::flow_end!(
                                    CATEGORY_STARNIX_BINDER,
                                    NAME_REMOTE_BINDER_IOCTL,
                                    tid.into()
                                );

                                match e {
                                    Ok(user_writes) => responder.send(Ok(user_writes.as_slice())),
                                    Err(e) => responder.send(Err(fposix::Errno::from_primitive(
                                        e.code.error_code() as i32,
                                    )
                                    .unwrap_or(fposix::Errno::Einval))),
                                }
                            },
                        );
                    self.enqueue_task_request(BoundTaskRequest {
                        koid: Some(tid),
                        request: TaskRequest::Ioctl {
                            remote_binder_connection: remote_binder_connection.clone(),
                            request,
                            arg,
                            koid: tid,
                            vmo,
                            ioctl_reads,
                            files,
                            responder,
                        },
                    });
                    waiter
                };
                waiter.await;
            }
            fbinder::BinderRequest::RegisterThreadChannel { responder, .. } => {
                // Not supported yet. Clients fall back to the `Ioctl` method of this protocol.
                let _ = responder.send(Err(fposix::Errno::Eopnotsupp));
            }
            fbinder::BinderRequest::_UnknownMethod { ordinal, .. } => {
                log_warn!("Unknown Binder ordinal: {}", ordinal);
            }
        }
        Ok(())
    }

    fn enqueue_taskless_request(&self, request: TaskRequest) {
        let notification = self.lock().enqueue_taskless_request(request);
        self.notify(notification);
    }

    fn enqueue_task_request(&self, request: BoundTaskRequest) {
        let notification = self.lock().enqueue_task_request(request);
        self.notify(notification);
    }

    /// Serve the ContainerPowerController protocol.
    async fn serve_container_power_controller(
        server_end: ServerEnd<fbinder::ContainerPowerControllerMarker>,
        message_counter: OwnedMessageCounterHandle,
        kernel: Arc<Kernel>,
        service_name: &str,
    ) -> Result<(), Error> {
        async fn handle_request(
            event: fbinder::ContainerPowerControllerRequest,
            kernel: &Arc<Kernel>,
            wake_lock_name: &str,
        ) -> Result<(), Error> {
            match event {
                fbinder::ContainerPowerControllerRequest::Wake { payload, .. } => {
                    if let Some(wake_lock) = payload.wake_lock {
                        kernel.suspend_resume_manager.activate_wakeup_source(
                            WakeupSourceOrigin::HAL(wake_lock_name.to_string()),
                        );
                        // The client is responsible for lowering this signal if it reuses the same
                        // event pair across calls.
                        let _ =
                            wake_lock.signal_peer(zx::Signals::empty(), WAKE_LOCK_ACQUIRED_SIGNAL);
                        let kernel_clone = kernel.clone();
                        let wake_lock_name = wake_lock_name.to_owned();
                        kernel.kthreads.spawn_future(
                            move || async move {
                                fasync::OnSignals::new(
                                    &wake_lock,
                                    zx::Signals::EVENTPAIR_PEER_CLOSED,
                                )
                                .await
                                .unwrap();
                                kernel_clone.suspend_resume_manager.deactivate_wakeup_source(
                                    &WakeupSourceOrigin::HAL(wake_lock_name),
                                );
                            },
                            "remote_binder_wake_handler",
                        );
                    }

                    if let Some(baton) = payload.power_baton {
                        // Since we received this message, we know that the kernel is awake and
                        // it's acceptable to drop the baton.
                        // Manually call `drop` just to be explicit (and leave a comment).
                        drop(baton);
                    }
                }
                fbinder::ContainerPowerControllerRequest::RegisterWakeWatcher {
                    payload, ..
                } => {
                    if let Some(watcher) = payload.watcher {
                        starnix_core::power::create_watcher_for_wake_events(watcher);
                    }
                }
                unknown => log_warn!("Unknown ContainerPowerController request: {:#?}", unknown),
            };
            Ok(())
        }

        let mut waking_stream =
            ContainerWakingStream::new(message_counter, server_end.into_stream());

        while let Some(res) = waking_stream.next().await {
            handle_request(res?, &kernel, service_name).await?;
        }
        Ok(())
    }

    /// Serve the given `binder` handle, by opening `path`.
    async fn open_binder(
        self: Arc<Self>,
        path: FsString,
        process_accessor: ClientEnd<fbinder::ProcessAccessorMarker>,
        process: zx::Process,
        binder: ServerEnd<fbinder::BinderMarker>,
    ) -> Result<(), Error> {
        // Open the device.
        let (sender, receiver) = oneshot::channel::<Result<Arc<RemoteBinderConnection>, Errno>>();
        self.enqueue_taskless_request(TaskRequest::Open {
            path,
            process_accessor,
            process,
            responder: sender,
        });
        let remote_binder_connection: Arc<RemoteBinderConnection> = receiver.await??;
        let remote_binder_connection_for_close = remote_binder_connection.clone();

        scopeguard::defer! {
            // When leaving the current scope, close the connection, even if some operation are in
            // progress. This should kick the tasks back with an error.
            remote_binder_connection_for_close.close(&self.kernel);
        }

        // Register a receiver to be notified of exit
        let (sender, receiver) = oneshot::channel::<()>();
        {
            let mut state = self.lock();
            if state.exit.is_some() {
                return Ok(());
            }
            state.exit_notifiers.push(sender);
        }

        // The stream for the Binder protocol
        let stream = fbinder::BinderRequestStream::from_channel(fasync::Channel::from_channel(
            binder.into_channel(),
        ));

        pin_mut!(receiver, stream);
        // The stream that will cancel once receiver returns a value.
        let stream = futures::stream::poll_fn(move |context| {
            if receiver.as_mut().poll(context).is_ready() {
                return Poll::Ready(None);
            }
            stream.as_mut().poll_next(context)
        });

        stream
            .map(|result| result.context("failed request"))
            .try_for_each_concurrent(usize::MAX, |event| {
                self.serve_binder_request(remote_binder_connection.clone(), event)
            })
            .await
    }

    /// Serve the DevBinder protocol.
    async fn serve_dev_binder(
        self: Arc<Self>,
        server_end: ServerEnd<fbinder::DevBinderMarker>,
    ) -> Result<(), Error> {
        let mut stream = fbinder::DevBinderRequestStream::from_channel(
            fasync::Channel::from_channel(server_end.into_channel()),
        );
        // Keep track of the current task serving the different Binder protocol. When a given
        // Binder is closed, this task will actually wait for the associated Binder task to finish,
        // to ensure that the same device is not opened multiple times because of concurrency.
        let binder_tasks =
            Rc::new(std::cell::RefCell::new(HashMap::<zx::Koid, fasync::Task<()>>::new()));
        while let Some(event) = stream.try_next().await? {
            // The tasks must be freed when this method returns, binder_tasks should always have a
            // single owner, and the RC is only used temporarily to let tasks clean themselves.
            debug_assert_eq!(Rc::strong_count(&binder_tasks), 1);
            match event {
                fbinder::DevBinderRequest::Open { payload, control_handle } => {
                    // Extract the path, process_accessor and binder_server from the `payload`, and
                    // start serving the binder protocol.
                    // Returns the task serving the binder protocol, as well as the koid to the
                    // client handle for the binder protocol.
                    //
                    // This is wrapped in a closure so that any error can be evaluated.
                    let result: Result<_, Error> = (|| {
                        let path = payload.path.ok_or_else(|| errno!(EINVAL))?;
                        let process_accessor =
                            payload.process_accessor.ok_or_else(|| errno!(EINVAL))?;
                        let process = payload.process.ok_or_else(|| errno!(EINVAL))?;
                        let binder = payload.binder.ok_or_else(|| errno!(EINVAL))?;
                        let koid = binder.as_handle_ref().basic_info()?.related_koid;
                        let handle = self.clone();
                        Ok((
                            fasync::Task::local(handle.open_binder(
                                path.into(),
                                process_accessor,
                                process,
                                binder,
                            )),
                            koid,
                        ))
                    })();
                    match result {
                        // The request was valid. task is the local task currently serving the
                        // binder protocol, koid is the koid of the client handle for the binder
                        // protocol.
                        Ok((task, koid)) => {
                            // Wrap the task into a new local task that on exit will:
                            // 1. Unregister the task from `binder_tasks`
                            // 2. If the tasks ends up in error, disconnecting the binder protocol.
                            let mut task = fasync::Task::local({
                                // Keep a weak references to the tasks to unregister. Do not keep a
                                // strong reference as otherwise it creates a reference count loop.
                                let binder_tasks = Rc::downgrade(&binder_tasks);
                                async move {
                                    let result = task.await;
                                    if let Some(binder_tasks) = binder_tasks.upgrade() {
                                        binder_tasks.borrow_mut().remove(&koid);
                                    }
                                    if let Err(err) = result {
                                        log_warn!("DevBinder::Open failed: {err:?}");
                                        control_handle.shutdown();
                                    }
                                }
                            });
                            // If the task is not pending, it must not be registered into
                            // `binder_tasks`, as it will never be removed.
                            if futures::poll!(&mut task).is_pending() {
                                // Register the task associated with the koid of the remote handle.
                                binder_tasks.borrow_mut().insert(koid, task);
                            }
                        }
                        Err(err) => {
                            log_warn!("DevBinder::Open failed: {err:?}");
                            control_handle.shutdown();
                        }
                    }
                }
                fbinder::DevBinderRequest::Close { payload, control_handle } => {
                    // Retrieve the task using the koid of the remote handle. If the task is still
                    // registered, wait for it to terminate. This will happen promptly, because the
                    // remote handle is closed by this closure.
                    let result: Result<_, Error> = (|| {
                        let binder = payload.binder.ok_or_else(|| errno!(EINVAL))?;
                        let koid = binder.as_handle_ref().koid()?;
                        Ok(binder_tasks.borrow_mut().remove(&koid))
                    })();
                    match result {
                        Err(err) => {
                            log_warn!("DevBinder::Close failed: {err:?}");
                            control_handle.shutdown();
                        }
                        Ok(Some(task)) => {
                            task.await;
                        }
                        Ok(None) => {}
                    }
                }
                fbinder::DevBinderRequest::_UnknownMethod { ordinal, .. } => {
                    log_warn!("Unknown DevBinder ordinal: {}", ordinal);
                }
            }
        }
        Ok(())
    }

    /// Returns the next TaskRequest that `current_task` must handle, waiting if none is available.
    fn get_next_task(&self, current_task: &CurrentTask) -> Result<TaskRequest, Errno> {
        loop {
            let is_interrupted = current_task.is_interrupt_pending();
            let waiter = {
                let mut state = self.lock();
                // Exit immediately if requested.
                if let Some(result) = state.exit.as_ref() {
                    return result
                        .map_err(|c| errno_from_code!(c.error_code() as i16))
                        .map(|_| TaskRequest::Return { spawn_thread: false });
                }
                if is_interrupted {
                    return error!(EINTR);
                }
                if current_task.is_leader() {
                    if let Some(request) = state.taskless_requests.pop_front() {
                        return Ok(request);
                    }
                    let waiter = Waiter::new();
                    self.waiters.wait_async_value(&waiter, MAIN_THREAD_WAIT_KEY);
                    waiter
                } else {
                    let tid = &current_task.tid;
                    if let Some(request) = state.pending_requests.get_mut(tid) {
                        // This task is already associated with a remote koid. Check if some request
                        // is available for this task.
                        if let Some(request) = request.take() {
                            return Ok(request.request);
                        }
                    } else {
                        // The task is not associated with any remote koid yet. When it registered,
                        // `register_waiting_task` gave it the first unassigned request if there
                        // was one, and otherwise added it to `unassigned_tasks`. While there are
                        // unassigned tasks, `enqueue_task_request` gives them the new requests
                        // rather than queuing them in `unassigned_requests`.
                        debug_assert!(state.unassigned_tasks.contains(tid));
                        debug_assert!(state.unassigned_requests.is_empty());
                    }
                    // Wait until some request is available.
                    let waiter = Waiter::new();
                    self.waiters.wait_async_value(&waiter, tid.id as u64);
                    waiter
                }
            };
            waiter.wait(current_task)?;
        }
    }

    /// Open a remote connection with the binder device at `path`.
    fn open(
        &self,
        current_task: &CurrentTask,
        path: FsString,
        process_accessor: ClientEnd<fbinder::ProcessAccessorMarker>,
        process: zx::Process,
    ) -> Result<Arc<RemoteBinderConnection>, Errno> {
        let node = current_task.lookup_path_from_root(path.as_ref())?;
        let device_type = node.entry.node.info().rdev;
        let device = current_task
            .kernel()
            .device_registry
            .get_device(device_type, DeviceMode::Char)
            .or_else(|_| error!(ENOTSUP))?;
        let device_ref: &BinderDevice = device
            .as_ref()
            .as_any()
            .downcast_ref::<BinderDevice>()
            .ok_or_else(|| errno!(ENOTSUP))?;

        let connection =
            BinderDriver::open_remote(device_ref, current_task, process_accessor, process);
        Ok(connection)
    }

    /// Implementation of the REMOTE_BINDER_START ioctl.
    fn start(
        self: &Arc<Self>,
        current_task: &CurrentTask,
        service_address_ref: UserCStringPtr,
    ) -> Result<(), Errno> {
        let service_address = current_task.read_multi_arch_ptr(service_address_ref)?;
        let service = current_task.read_path(service_address)?;
        let service_name = String::from_utf8(service.to_vec()).map_err(|_| errno!(EINVAL))?;
        let remote_controller_client =
            F::connect_to_remote_controller(current_task, &service_name)?;
        let remote_controller =
            fbinder::RemoteControllerSynchronousProxy::new(remote_controller_client.into_channel());
        let (dev_binder_server_end, dev_binder_client_end) = zx::Channel::create();

        let (power_controller_server_end, power_controller_client_end) = zx::Channel::create();
        let counter_name = format!("hal: {}", service_name);
        let (power_controller_server_end, counter) =
            starnix_core::power::create_proxy_for_wake_events_counter(
                power_controller_server_end,
                counter_name.clone(),
            );

        remote_controller
            .start(fbinder::RemoteControllerStartRequest {
                dev_binder: Some(dev_binder_client_end.into()),
                container_power_controller: Some(power_controller_client_end.into()),
                ..Default::default()
            })
            .map_err(|_| errno!(EINVAL))?;
        let handle = self.clone();
        let closure = async move |_current_task: &CurrentTask| {
            // Retrieve the Kernel and a `DropWaiter` for the thread_group, taking care not
            // to keep a strong reference to the thread_group itself.
            let kernel_and_drop_waiter = handle
                .state
                .lock()
                .thread_group
                .upgrade()
                .map(|tg| (tg.kernel.clone(), tg.drop_notifier.waiter()));
            let Some((kernel, drop_waiter)) = kernel_and_drop_waiter else {
                return;
            };

            let message_counter = kernel
                .suspend_resume_manager
                .add_message_counter(counter_name.as_str(), Some(counter));

            // Start the 3 servers.
            let binder_fut = handle.clone().serve_dev_binder(dev_binder_server_end.into());
            let power_fut = Self::serve_container_power_controller(
                power_controller_server_end.into(),
                message_counter,
                kernel,
                &service_name,
            );
            // Wait until all are done, or the task exits.
            let (binder_res, power_res) = futures::join!(
                future_or_task_end(&drop_waiter, binder_fut),
                future_or_task_end(&drop_waiter, power_fut),
            );
            let result = binder_res.and(power_res);
            if let Err(e) = &result {
                log_error!("Error when servicing the DevBinder protocol: {e:#}");
            }
            handle.exit(result.map_err(|_| errno!(ENOENT)));
        };
        let req = SpawnRequestBuilder::new()
            .with_debug_name("remote-binder-start")
            .with_role(EXECUTOR_THREAD_ROLE)
            .with_async_closure(closure)
            .build();
        current_task.kernel().kthreads.spawner().spawn_from_request(req);

        error!(EAGAIN)
    }

    /// Implementation of the REMOTE_BINDER_WAIT ioctl.
    fn wait(
        &self,
        current_task: &CurrentTask,
        wait_command_ref: UserRef<uapi::remote_binder_wait_command>,
    ) -> Result<(), Errno> {
        if !current_task.is_leader() {
            self.lock().register_waiting_task(&current_task.tid);
        }
        loop {
            let interruption = match self.get_next_task(current_task)? {
                TaskRequest::Open { path, process_accessor, process, responder } => {
                    let result = self.open(current_task, path, process_accessor, process);
                    let interruption = must_interrupt(&result);
                    responder.send(result).map_err(|_| errno!(EINVAL))?;
                    interruption
                }
                TaskRequest::SetVmo {
                    remote_binder_connection,
                    vmo,
                    mapped_address,
                    responder,
                } => {
                    let result = remote_binder_connection.map_external_vmo(
                        current_task,
                        vmo,
                        mapped_address,
                    );
                    let interruption = must_interrupt(&result);
                    responder(result).map_err(|_| errno!(EINVAL))?;
                    interruption
                }
                TaskRequest::Ioctl {
                    remote_binder_connection,
                    request,
                    arg,
                    koid,
                    vmo,
                    ioctl_reads,
                    files,
                    responder,
                } => {
                    fuchsia_trace::duration!(
                        CATEGORY_STARNIX_BINDER,
                        NAME_REMOTE_BINDER_IOCTL_WORKER_PROCESS
                    );
                    fuchsia_trace::flow_step!(
                        CATEGORY_STARNIX_BINDER,
                        NAME_REMOTE_BINDER_IOCTL,
                        koid.into()
                    );
                    let result = remote_binder_connection.ioctl(
                        current_task,
                        request,
                        arg.into(),
                        ioctl_reads,
                        files,
                        vmo,
                    );
                    // Once the potentially blocking calls is made, the task is ready to handle the
                    // next request.
                    self.lock()
                        .pending_requests
                        .insert(current_task.tid.clone(), PendingRequest::None);
                    let interruption = must_interrupt(&result);
                    responder(result).map_err(|_| errno!(EINVAL))?;
                    interruption
                }
                TaskRequest::Return { spawn_thread } => {
                    let wait_command = uapi::remote_binder_wait_command {
                        spawn_thread: if spawn_thread { 1 } else { 0 },
                    };
                    current_task.write_object(wait_command_ref, &wait_command)?;
                    return Ok(());
                }
            };
            if let Some(errno) = interruption {
                return Err(errno);
            }
        }
    }
}

async fn future_or_task_end(
    drop_waiter: &DropWaiter,
    fut: impl Future<Output = Result<(), Error>>,
) -> Result<(), Error> {
    let on_task_end = drop_waiter.on_closed().map(|r| r.map(|_| ()).map_err(anyhow::Error::from));
    select_first(fut, on_task_end).await
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BinderFs;
    use crate::tests::tests::run_process_accessor;
    use fidl::endpoints::{Proxy, create_endpoints, create_proxy};
    use rand::distr::{Alphanumeric, SampleString};
    use starnix_core::mm::MemoryAccessor;
    use starnix_core::power::{OwnedMessageCounter, WakeupSourceOrigin};
    use starnix_core::task::{RunState, Task};
    use starnix_core::testing::*;
    use starnix_core::vfs::{FileSystemOptions, WhatToMount};
    use starnix_sync::RemoteBinderControllerLock;
    use starnix_task_command::TaskCommand;
    use starnix_types::PAGE_SIZE;
    use starnix_uapi::auth::Credentials;
    use starnix_uapi::file_mode::mode;
    use starnix_uapi::mount_flags::MountpointFlags;
    use starnix_uapi::restricted_aspace::RESTRICTED_ASPACE_RANGE;
    use std::collections::BTreeMap;
    use std::ffi::CString;
    use std::sync::LazyLock;

    static REMOTE_CONTROLLER_CLIENT: LazyLock<
        LockDepMutex<
            BTreeMap<String, ClientEnd<fbinder::RemoteControllerMarker>>,
            RemoteBinderControllerLock,
        >,
    > = LazyLock::new(Default::default);

    struct TestRemoteControllerConnector {}

    impl RemoteControllerConnector for TestRemoteControllerConnector {
        fn connect_to_remote_controller(
            _current_task: &CurrentTask,
            service_name: &str,
        ) -> Result<ClientEnd<fbinder::RemoteControllerMarker>, Errno> {
            REMOTE_CONTROLLER_CLIENT.lock().remove(service_name).ok_or_else(|| errno!(ENOENT))
        }
    }
    use starnix_core::execution::{TaskInfo, create_task};

    /// Runs the `REMOTE_BINDER_WAIT` loop for `current_task`, spawning worker threads in the same
    /// thread group whenever `wait_command.spawn_thread != 0` and joining them before exit. When
    /// the thread group leader stops waiting, it closes `remote_binder_handle` so that the workers
    /// stop too.
    fn run_wait_loop(
        current_task: CurrentTask,
        remote_binder_handle: Arc<RemoteBinderHandle<TestRemoteControllerConnector>>,
    ) -> Result<SyscallResult, Errno> {
        let wait_command_address = map_memory(
            &current_task,
            UserAddress::default(),
            std::mem::size_of::<uapi::remote_binder_wait_command>() as u64,
        );
        let wait_command_ref =
            UserRef::<uapi::remote_binder_wait_command>::new(wait_command_address);
        let mut worker_threads = Vec::new();
        let result = loop {
            let result = remote_binder_handle.ioctl(
                &current_task,
                uapi::REMOTE_BINDER_WAIT,
                wait_command_address.into(),
            );
            if must_interrupt(&result).is_some() {
                continue;
            }
            if result.is_err() {
                break result;
            }
            let wait_command = current_task.read_object(wait_command_ref).expect("read_object");
            if wait_command.spawn_thread == 0 {
                break result;
            }
            let builder = current_task.clone_task_builder_for_test(
                (uapi::CLONE_THREAD | uapi::CLONE_VM | uapi::CLONE_SIGHAND) as u64,
                None,
            );
            let handle = remote_binder_handle.clone();
            worker_threads.push(
                std::thread::Builder::new()
                    .name("user-worker-thread".to_string())
                    .spawn(move || {
                        let worker_task: CurrentTask = builder.into();
                        run_wait_loop(worker_task, handle)
                    })
                    .expect("Failed to create worker thread"),
            );
        };
        if current_task.is_leader() {
            // Make the workers stop waiting, even if the leader stopped because of an error that
            // did not exit the handle.
            remote_binder_handle.close();
        }
        let worker_results: Vec<_> =
            worker_threads.into_iter().map(|worker| worker.join()).collect();
        current_task.release(());
        for worker_result in worker_results {
            worker_result.expect("worker join").expect("worker result");
        }
        result
    }

    /// Setup and run a test against the remote binder. The closure that is passed to this function
    /// will be called with a binder proxy that can be used to access the remote binder.
    async fn run_remote_binder_test<F, Fut>(f: F)
    where
        Fut: Future<Output = fbinder::BinderProxy>,
        F: FnOnce(fbinder::BinderProxy) -> Fut + Sync + Send + 'static,
    {
        run_remote_binder_test_with_thread_group(|binder, _| f(binder)).await
    }

    /// Same as `run_remote_binder_test`, but the closure is also given the thread group of the
    /// simulated remote binder process.
    async fn run_remote_binder_test_with_thread_group<F, Fut>(f: F)
    where
        Fut: Future<Output = fbinder::BinderProxy>,
        F: FnOnce(fbinder::BinderProxy, Arc<ThreadGroup>) -> Fut + Sync + Send + 'static,
    {
        spawn_kernel_and_run(async |init_task| {
            let service_name = Alphanumeric.sample_string(&mut rand::rng(), 16);
            let (remote_controller_client, remote_controller_server) =
                create_endpoints::<fbinder::RemoteControllerMarker>();
            REMOTE_CONTROLLER_CLIENT.lock().insert(service_name.clone(), remote_controller_client);
            let fs = init_task.fs().clone();
            let kernel = init_task.kernel().clone();
            let memory_manager = init_task.mm().ok();
            let init_thread_group = init_task.thread_group().clone();
            let (thread_group_sender, thread_group_receiver) = oneshot::channel();

            // Simulate the remote binder user process.
            let starnix_thread = std::thread::Builder::new()
                .name("user-thread".to_string())
                .spawn(move || {
                    let builder = create_task(
                        &kernel,
                        TaskCommand::new(b"kthreadd"),
                        fs,
                        |pid| {
                            let process = fuchsia_runtime::process_self()
                                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                                .expect("process");
                            let thread_group = ThreadGroup::for_test(
                                kernel.clone(),
                                process,
                                init_thread_group.write(),
                                pid,
                            );
                            Ok(TaskInfo { thread_group, memory_manager }.into())
                        },
                        Credentials::root(),
                    )
                    .expect("failed");
                    let current_task: CurrentTask = builder.into();
                    let _ = thread_group_sender.send(current_task.thread_group().clone());

                    current_task
                        .fs()
                        .root()
                        .create_node(
                            &current_task,
                            "dev".into(),
                            mode!(IFDIR, 0o755),
                            DeviceId::NONE,
                        )
                        .expect("mkdir dev");
                    let dev = current_task
                        .lookup_path_from_root("/dev".into())
                        .expect("lookup_path_from_root");
                    dev.mount(
                        WhatToMount::Fs(
                            BinderFs::new_fs(&current_task, FileSystemOptions::default())
                                .expect("new_fs"),
                        ),
                        MountpointFlags::empty(),
                    )
                    .expect("mount");

                    let remote_binder_handle =
                        RemoteBinderHandle::<TestRemoteControllerConnector>::new(&current_task);

                    let service_name_string =
                        CString::new(service_name.as_bytes()).expect("CString::new");
                    let service_name_bytes = service_name_string.as_bytes_with_nul();
                    let base_address = UserAddress::from(RESTRICTED_ASPACE_RANGE.start as u64);
                    let service_name_address = map_memory(
                        &current_task,
                        (base_address + *PAGE_SIZE).expect("failed to compute address"),
                        service_name_bytes.len() as u64,
                    );
                    current_task
                        .write_memory(service_name_address, service_name_bytes)
                        .expect("write_memory");

                    let start_command_address = map_memory(
                        &current_task,
                        (base_address + (*PAGE_SIZE * 2)).expect("failed to compute address"),
                        std::mem::size_of::<u64>() as u64,
                    );
                    current_task
                        .write_object(start_command_address.into(), &service_name_address.ptr())
                        .expect("write_object");

                    let start_result = remote_binder_handle.ioctl(
                        &current_task,
                        uapi::REMOTE_BINDER_START,
                        start_command_address.into(),
                    );
                    if must_interrupt(&start_result).is_none() {
                        panic!("Unexpected result for start ioctl: {start_result:?}");
                    }
                    run_wait_loop(current_task, remote_binder_handle)
                })
                .expect("Failed to create thread");

            // Wait for the Start request
            let mut remote_controller_stream = fbinder::RemoteControllerRequestStream::from_channel(
                fasync::Channel::from_channel(remote_controller_server.into_channel()),
            );
            let dev_binder_client_end = match remote_controller_stream.try_next().await {
                Ok(Some(fbinder::RemoteControllerRequest::Start { payload, .. })) => {
                    payload.dev_binder.expect("dev_binder")
                }
                x => panic!("Expected a start request, got: {x:?}"),
            };

            let (process_accessor_client_end, process_accessor_server_end) =
                create_endpoints::<fbinder::ProcessAccessorMarker>();
            let process_accessor_task =
                fasync::Task::local(run_process_accessor(process_accessor_server_end));

            let (binder, binder_server_end) = create_proxy::<fbinder::BinderMarker>();

            let process = fuchsia_runtime::process_self()
                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                .expect("process");
            let dev_binder =
                fbinder::DevBinderSynchronousProxy::new(dev_binder_client_end.into_channel());
            dev_binder
                .open(fbinder::DevBinderOpenRequest {
                    path: Some(b"/dev/binder".to_vec()),
                    process_accessor: Some(process_accessor_client_end),
                    process: Some(process),
                    binder: Some(binder_server_end),
                    ..Default::default()
                })
                .expect("open");

            // Do the test.
            let thread_group = thread_group_receiver.await.expect("thread group");
            let binder = f(binder, thread_group).await;

            // Notify of the close binder
            dev_binder
                .close(fbinder::DevBinderCloseRequest {
                    binder: Some(
                        binder.into_channel().expect("into_channel").into_zx_channel().into(),
                    ),
                    ..Default::default()
                })
                .expect("close");

            std::mem::drop(dev_binder);
            starnix_thread.join().expect("thread join").expect("thread result");
            process_accessor_task.await.expect("process accessor wait");
        })
        .await;
    }

    #[::fuchsia::test]
    async fn external_binder_connection() {
        run_remote_binder_test(|binder| async move {
            const VMO_SIZE: usize = 10 * 1024 * 1024;
            let vmo = zx::Vmo::create(VMO_SIZE as u64).expect("Vmo::create");
            let addr = fuchsia_runtime::vmar_root_self()
                .map(0, &vmo, 0, VMO_SIZE, zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE)
                .expect("map");
            scopeguard::defer! {
              // SAFETY: This is a ffi call to a kernel syscall.
              unsafe { fuchsia_runtime::vmar_root_self().unmap(addr, VMO_SIZE).expect("unmap"); }
            }

            binder.set_vmo(vmo, addr as u64).expect("set_vmo");
            let mut version = uapi::binder_version { protocol_version: 0 };
            let version_ref = &mut version as *mut uapi::binder_version;
            let vmo = zx::Vmo::create(VMO_SIZE as u64).expect("Vmo::create");
            let dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("duplicate_handle");
            let files = Vec::new();
            let user_writes = binder
                .ioctl(42, uapi::BINDER_VERSION, version_ref as u64, vmo, &[], files)
                .await
                .expect("ioctl")
                .expect("ioctl");
            for user_write in user_writes.iter() {
                // SAFETY: This is required to emulate the scattered writes for tests.
                unsafe {
                    dup.read_raw(
                        user_write.address as *mut u8,
                        user_write.length as usize,
                        user_write.offset,
                    )
                    .expect("read_raw")
                }
            }
            // SAFETY: This is safe, because version is repr(C)
            let version = unsafe { std::ptr::read_volatile(version_ref) };
            assert_eq!(version.protocol_version, uapi::BINDER_CURRENT_PROTOCOL_VERSION as i32);
            binder
        })
        .await;
    }

    #[::fuchsia::test]
    async fn map_vmo_responds_once_mapped() {
        run_remote_binder_test(|binder| async move {
            const VMO_SIZE: usize = 10 * 1024 * 1024;
            let vmo = zx::Vmo::create(VMO_SIZE as u64).expect("Vmo::create");
            let addr = fuchsia_runtime::vmar_root_self()
                .map(0, &vmo, 0, VMO_SIZE, zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE)
                .expect("map");
            scopeguard::defer! {
              // SAFETY: No Rust reference points into the mapping. The test only gives its address
              // to the binder driver, which accesses the memory through the VMO.
              unsafe { fuchsia_runtime::vmar_root_self().unmap(addr, VMO_SIZE).expect("unmap"); }
            }

            let dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("duplicate_handle");
            binder.map_vmo(dup, addr as u64).await.expect("map_vmo").expect("map_vmo");

            // The VMO is mapped once `map_vmo` responds, so the driver rejects another one, and
            // returns the error to the client.
            assert_eq!(
                binder.map_vmo(vmo, addr as u64).await.expect("map_vmo"),
                Err(fposix::Errno::Einval)
            );
            binder
        })
        .await;
    }

    /// Returns the thread group leader of the simulated remote binder process, and its worker
    /// tasks.
    fn leader_and_workers(thread_group: &ThreadGroup) -> (Arc<Task>, Vec<Arc<Task>>) {
        let (leaders, workers): (Vec<_>, Vec<_>) =
            thread_group.read().tasks().into_iter().partition(|task| task.is_leader());
        (leaders.into_iter().next().expect("leader"), workers)
    }

    /// Returns the run state of `task`, if it is blocked in a run state other than
    /// `previous_run_state`.
    fn new_blocked_run_state(
        task: &Task,
        previous_run_state: Option<&RunState>,
    ) -> Option<RunState> {
        let run_state = task.read().run_state();
        (run_state.is_blocked() && previous_run_state != Some(&run_state)).then_some(run_state)
    }

    /// Waits until `task` is blocked in a run state other than `previous_run_state`. Returns the
    /// run state it is blocked in.
    async fn wait_for_blocked_task(task: &Task, previous_run_state: Option<&RunState>) -> RunState {
        loop {
            if let Some(run_state) = new_blocked_run_state(task, previous_run_state) {
                return run_state;
            }
            fasync::Timer::new(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Waits until the simulated remote binder process has a single worker task, and that task is
    /// blocked in a run state other than `previous_run_state`. Returns the worker and the run state
    /// it is blocked in.
    async fn wait_for_blocked_worker(
        thread_group: &ThreadGroup,
        previous_run_state: Option<&RunState>,
    ) -> (Arc<Task>, RunState) {
        loop {
            let (_, workers) = leader_and_workers(thread_group);
            assert!(workers.len() <= 1, "Expected at most one worker task, got {}", workers.len());
            if let Some(worker) = workers.into_iter().next() {
                if let Some(run_state) = new_blocked_run_state(&worker, previous_run_state) {
                    return (worker, run_state);
                }
            }
            fasync::Timer::new(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Returns a `binder_write_read` with an empty write buffer and `read_buffer` as read buffer.
    /// The worker task handling a `BINDER_WRITE_READ` ioctl with it blocks in `handle_thread_read`,
    /// until the remote thread receives some work or the worker task is interrupted.
    fn read_only_binder_write_read(read_buffer: &[u8]) -> uapi::binder_write_read {
        uapi::binder_write_read {
            write_size: 0,
            write_consumed: 0,
            write_buffer: 0,
            read_size: read_buffer.len() as u64,
            read_consumed: 0,
            read_buffer: read_buffer.as_ptr() as u64,
        }
    }

    #[::fuchsia::test]
    async fn leader_is_interruptible_while_worker_blocks_in_read() {
        run_remote_binder_test_with_thread_group(|binder, thread_group| async move {
            // A read from remote thread 1, with nothing to read, blocks its worker task in
            // `handle_thread_read`.
            let read_buffer = [0u8; 64];
            let bwr = read_only_binder_write_read(&read_buffer);
            let mut read = binder.ioctl(
                1,
                uapi::BINDER_WRITE_READ,
                &bwr as *const uapi::binder_write_read as u64,
                zx::Vmo::create(*PAGE_SIZE).expect("Vmo::create"),
                &[],
                Vec::new(),
            );
            let (worker, worker_run_state) = wait_for_blocked_worker(&thread_group, None).await;
            assert_matches::assert_matches!(worker_run_state, RunState::Event(_));
            assert!(futures::poll!(&mut read).is_pending());

            // The leader does not handle ioctls: it keeps waiting on the `WaitQueue` of the handle,
            // and an interrupt wakes it up right away. It then returns from `REMOTE_BINDER_WAIT`
            // with `EINTR`, and waits again.
            let (leader, _) = leader_and_workers(&thread_group);
            let leader_run_state = wait_for_blocked_task(&leader, None).await;
            assert_matches::assert_matches!(leader_run_state, RunState::Waiter(_));
            leader.interrupt();
            let leader_run_state = wait_for_blocked_task(&leader, Some(&leader_run_state)).await;
            assert_matches::assert_matches!(leader_run_state, RunState::Waiter(_));

            // Interrupting the leader did not interrupt the read.
            assert_eq!(worker.read().run_state(), worker_run_state);
            assert!(futures::poll!(&mut read).is_pending());

            // Interrupt the worker to end the read.
            worker.interrupt();
            assert_eq!(read.await.expect("ioctl"), Err(fposix::Errno::Eintr));
            binder
        })
        .await;
    }

    #[::fuchsia::test]
    async fn concurrent_ioctls_spawn_workers_without_interrupting_blocked_workers() {
        run_remote_binder_test_with_thread_group(|binder, thread_group| async move {
            // A read from remote thread 1, with nothing to read, blocks its worker task in
            // `handle_thread_read`.
            let read_buffer = [0u8; 64];
            let bwr = read_only_binder_write_read(&read_buffer);
            let mut read = binder.ioctl(
                1,
                uapi::BINDER_WRITE_READ,
                &bwr as *const uapi::binder_write_read as u64,
                zx::Vmo::create(*PAGE_SIZE).expect("Vmo::create"),
                &[],
                Vec::new(),
            );
            let (worker, worker_run_state) = wait_for_blocked_worker(&thread_group, None).await;
            assert_matches::assert_matches!(worker_run_state, RunState::Event(_));

            // The only worker is busy with remote thread 1, so an ioctl from remote thread 2 makes
            // the leader spawn a new worker task, which handles it.
            let mut version = uapi::binder_version { protocol_version: 0 };
            let version_ref = &mut version as *mut uapi::binder_version;
            let vmo = zx::Vmo::create(*PAGE_SIZE).expect("Vmo::create");
            let dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("duplicate_handle");
            let user_writes = binder
                .ioctl(2, uapi::BINDER_VERSION, version_ref as u64, vmo, &[], Vec::new())
                .await
                .expect("ioctl")
                .expect("ioctl");
            for user_write in user_writes.iter() {
                // SAFETY: This is required to emulate the scattered writes for tests.
                unsafe {
                    dup.read_raw(
                        user_write.address as *mut u8,
                        user_write.length as usize,
                        user_write.offset,
                    )
                    .expect("read_raw")
                }
            }
            // SAFETY: This is safe, because version is repr(C)
            let version = unsafe { std::ptr::read_volatile(version_ref) };
            assert_eq!(version.protocol_version, uapi::BINDER_CURRENT_PROTOCOL_VERSION as i32);
            let (_, workers) = leader_and_workers(&thread_group);
            assert_eq!(workers.len(), 2);

            // Spawning the new worker task did not interrupt the read.
            assert_eq!(worker.read().run_state(), worker_run_state);
            assert!(futures::poll!(&mut read).is_pending());

            // Interrupt the worker to end the read.
            worker.interrupt();
            assert_eq!(read.await.expect("ioctl"), Err(fposix::Errno::Eintr));
            binder
        })
        .await;
    }

    async fn wait_for_message(counter: &zx::Counter) {
        fasync::OnSignals::new(&counter, zx::Signals::COUNTER_NON_POSITIVE).await.unwrap();
    }

    #[::fuchsia::test]
    async fn container_power_controller() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let (power_controller, power_controller_server_end) = fidl::endpoints::create_proxy();
            let counter = zx::Counter::create();
            let message_counter = OwnedMessageCounter::new(
                "test",
                Some(counter.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("Failed handle dup")),
            );
            // Simulate the proxy incrementing the message counter.
            counter.add(1).expect("Failed to add to counter");

            let _server_task = fasync::Task::local(async move {
                let result = RemoteBinderHandle::<TestRemoteControllerConnector>::serve_container_power_controller(
                    power_controller_server_end,
                    message_counter,
                    kernel,
                    "test",
                ).await;
                assert_matches::assert_matches!(result, Ok(_));
            });

            power_controller
                .wake(fbinder::ContainerPowerControllerWakeRequest { ..Default::default() })
                .unwrap();
            wait_for_message(&counter).await;
        }).await;
    }

    #[::fuchsia::test]
    async fn container_power_controller_drop_wake_lock() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();

            let (power_controller, power_controller_server_end) = fidl::endpoints::create_proxy();
            let counter = zx::Counter::create();
            let message_counter = OwnedMessageCounter::new(
                "test",
                Some(counter.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("Failed handle dup")),
            );
            // Simulate the proxy incrementing the message counter.
            counter.add(1).expect("Failed to add to counter");

            let kernel_clone = kernel.clone();
            let _server_task = fasync::Task::local(async move {
                let result =
                    RemoteBinderHandle::<TestRemoteControllerConnector>::serve_container_power_controller(
                        power_controller_server_end,
                        message_counter,
                        kernel_clone,
                        "test"
                    ).await;
                assert_matches::assert_matches!(result, Ok(_));
            });

            let (wake_lock, wake_lock_remote) = zx::EventPair::create();
            power_controller
                .wake(fbinder::ContainerPowerControllerWakeRequest {
                    wake_lock: Some(wake_lock_remote),
                    ..Default::default()
                })
                .unwrap();
            wait_for_message(&counter).await;

            // Check that the wake lock event pair has been signalled to indicate the wake lock being
            // acquired.
            fasync::OnSignals::new(&wake_lock, WAKE_LOCK_ACQUIRED_SIGNAL).await.unwrap();

            // Check that we already have the lock.
            assert!(
                !kernel
                    .suspend_resume_manager
                    .activate_wakeup_source(WakeupSourceOrigin::HAL("test".to_string()))
            );

            // Drop our lock, and wait for the lock to be released by the background task.
            drop(wake_lock);

            while !kernel
                .suspend_resume_manager
                .activate_wakeup_source(WakeupSourceOrigin::HAL("test".to_string()))
            {
                fuchsia_async::Timer::new(std::time::Duration::from_millis(10)).await;
            }
        }).await;
    }

    #[::fuchsia::test]
    async fn get_next_task_while_stopping_returns_eintr() {
        spawn_kernel_and_run(async move |current_task| {
            let remote_binder_handle =
                RemoteBinderHandle::<TestRemoteControllerConnector>::new(current_task);
            remote_binder_handle
                .lock()
                .taskless_requests
                .push_back(TaskRequest::Return { spawn_thread: true });

            current_task.write().set_stopped(
                starnix_core::ptrace::StopState::GroupStopping,
                None,
                Some(current_task),
                None,
            );

            assert_matches::assert_matches!(
                remote_binder_handle.get_next_task(current_task),
                Err(e) if e == errno!(EINTR)
            );
            assert_eq!(remote_binder_handle.lock().taskless_requests.len(), 1);

            let worker_task = current_task.clone_task_for_test(
                (uapi::CLONE_THREAD | uapi::CLONE_VM | uapi::CLONE_SIGHAND) as u64,
                None,
            );
            remote_binder_handle.lock().pending_requests.insert(
                worker_task.tid.clone(),
                PendingRequest::Some(BoundTaskRequest {
                    koid: Some(1),
                    request: TaskRequest::Return { spawn_thread: false },
                }),
            );
            worker_task.thread_group().set_stopped(
                starnix_core::ptrace::StopState::GroupStopping,
                None,
                false,
            );
            assert_eq!(worker_task.load_stopped(), starnix_core::ptrace::StopState::Awake);

            assert_matches::assert_matches!(
                remote_binder_handle.get_next_task(&worker_task),
                Err(e) if e == errno!(EINTR)
            );
            assert_matches::assert_matches!(
                remote_binder_handle.lock().pending_requests.get(&worker_task.tid),
                Some(PendingRequest::Some(_))
            );

            remote_binder_handle.close();
            assert_matches::assert_matches!(
                remote_binder_handle.get_next_task(&worker_task),
                Ok(TaskRequest::Return { spawn_thread: false })
            );
        })
        .await;
    }
}
