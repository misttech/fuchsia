// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![recursion_limit = "256"]

use fidl::endpoints::Proxy as _;
use fidl_fuchsia_hardware_adb as fadb;
use fuchsia_async as fasync;
use fuchsia_component::client::Service;
use fuchsia_inspect as inspect;
use futures::{FutureExt, StreamExt, pin_mut};
use futures_util as _;
use starnix_core::power::{create_proxy_for_wake_events_counter_zero, mark_proxy_message_handled};
use starnix_core::task::{CurrentTask, EventHandler, Kernel, WaitCanceler, WaitQueue, Waiter};
use starnix_core::vfs::pseudo::vec_directory::{VecDirectory, VecDirectoryEntry};
use starnix_core::vfs::{
    CacheMode, DirEntry, DirectoryEntryType, FileObject, FileObjectState, FileOps, FileSystem,
    FileSystemHandle, FileSystemOps, FileSystemOptions, FsNode, FsNodeInfo, FsNodeOps, FsStr,
    InputBuffer, OutputBuffer, fileops_impl_noop_sync, fileops_impl_seekless, fs_args,
    fs_node_impl_dir_readonly, fs_node_impl_not_dir,
};
use starnix_logging::{log_info, log_warn, track_stub};
use starnix_sync::{FunctionFsResultLock, FunctionFsStateLock, InterruptibleEvent, LockDepMutex};
use starnix_types::vfs::default_statfs;
use starnix_uapi::auth::FsCred;
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::mode;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::vfs::FdEvents;
use starnix_uapi::{
    errno, error, gid_t, ino_t, statfs, uid_t, usb_functionfs_event,
    usb_functionfs_event_type_FUNCTIONFS_BIND, usb_functionfs_event_type_FUNCTIONFS_DISABLE,
    usb_functionfs_event_type_FUNCTIONFS_ENABLE, usb_functionfs_event_type_FUNCTIONFS_UNBIND,
};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use zerocopy::IntoBytes;

// The node identifiers of different nodes in FunctionFS.
const ROOT_NODE_ID: ino_t = 1;

// Control endpoint is always present in a mounted FunctionFS.
const CONTROL_ENDPOINT: &str = "ep0";
const CONTROL_ENDPOINT_NODE_ID: ino_t = 2;

const OUTPUT_ENDPOINT: &str = "ep1";
const OUTPUT_ENDPOINT_NODE_ID: ino_t = 3;

const INPUT_ENDPOINT: &str = "ep2";
const INPUT_ENDPOINT_NODE_ID: ino_t = 4;

// Magic number of the file system, different from the magic used for Descriptors and Strings.
// Set to the same value as Linux.
const FUNCTIONFS_MAGIC: u32 = 0xa647361;

const ADB_DIRECTORY: &str = "/svc/fuchsia.hardware.adb.Service";

// How long to keep Starnix awake after an ADB interaction. If no ADB reads or
// writes occur within this time period, Starnix will be allowed to suspend.
const ADB_INTERACTION_TIMEOUT: zx::Duration<zx::MonotonicTimeline> = zx::Duration::from_seconds(2);

#[derive(Default)]
struct PendingResult<T: Default> {
    event: Arc<InterruptibleEvent>,
    result: LockDepMutex<Option<Result<T, Errno>>, FunctionFsResultLock>,
}

impl<T: Default> PendingResult<T> {
    fn set_result(&self, res: Result<T, Errno>) {
        let mut result = self.result.lock();
        debug_assert!(result.is_none(), "PendingResult set more than once");

        result.replace(res);
        self.event.notify();
    }
}

struct ReadCommand {
    pending: Arc<PendingResult<Vec<u8>>>,
}

struct WriteCommand {
    data: Vec<u8>,
    pending: Arc<PendingResult<usize>>,
}

const MAX_INSPECT_EVENTS: usize = 32;

#[derive(Clone)]
struct EventRecord {
    id: usize,
    timestamp: zx::BootInstant,
    event: Cow<'static, str>,
}

#[derive(Default)]
struct FunctionFsStats {
    ep1_total_bytes_read: AtomicU64,
    ep1_read_count: AtomicU64,
    ep1_read_errors: AtomicU64,
    ep1_buffer_overflow_errors: AtomicU64,

    ep2_total_bytes_written: AtomicU64,
    ep2_write_count: AtomicU64,
    ep2_write_errors: AtomicU64,
}

fn create_lazy_inspect_node(
    parent: &inspect::Node,
    state: &Arc<LockDepMutex<FunctionFsState, FunctionFsStateLock>>,
    stats: &Arc<FunctionFsStats>,
) -> inspect::LazyNode {
    let state_weak = Arc::downgrade(state);
    let stats_weak = Arc::downgrade(stats);
    parent.create_lazy_child("usb-functionfs", move || {
        let state_weak = state_weak.clone();
        let stats_weak = stats_weak.clone();
        Box::pin(async move {
            let inspector = inspect::Inspector::default();
            let root = inspector.root();

            if let Some(state_arc) = state_weak.upgrade() {
                let (
                    is_online,
                    num_control_file_objects,
                    event_queue_depth,
                    has_input_output_endpoints,
                    control_resets,
                    events,
                ) = {
                    let state = state_arc.lock();
                    (
                        state.is_online,
                        state.num_control_file_objects as u64,
                        state.event_queue.len() as u64,
                        state.has_input_output_endpoints,
                        state.control_resets,
                        state.event_history.iter().cloned().collect::<Vec<_>>(),
                    )
                };

                root.record_bool("is_online", is_online);
                root.record_uint("num_control_file_objects", num_control_file_objects);
                root.record_uint("event_queue_depth", event_queue_depth);
                root.record_bool("has_input_output_endpoints", has_input_output_endpoints);
                root.record_uint("control_resets", control_resets);

                root.record_child("event_history", |history_node| {
                    for record in events {
                        history_node.record_child(record.id.to_string(), |entry_node| {
                            entry_node.record_int("@time", record.timestamp.into_nanos());
                            entry_node.record_string("event", record.event.as_ref());
                        });
                    }
                });
            }

            if let Some(stats) = stats_weak.upgrade() {
                root.record_child("ep1_bulk_out", |ep1_node| {
                    ep1_node.record_uint(
                        "total_bytes_read",
                        stats.ep1_total_bytes_read.load(Ordering::Relaxed),
                    );
                    ep1_node
                        .record_uint("read_count", stats.ep1_read_count.load(Ordering::Relaxed));
                    ep1_node
                        .record_uint("read_errors", stats.ep1_read_errors.load(Ordering::Relaxed));
                    ep1_node.record_uint(
                        "buffer_overflow_errors",
                        stats.ep1_buffer_overflow_errors.load(Ordering::Relaxed),
                    );
                });

                root.record_child("ep2_bulk_in", |ep2_node| {
                    ep2_node.record_uint(
                        "total_bytes_written",
                        stats.ep2_total_bytes_written.load(Ordering::Relaxed),
                    );
                    ep2_node
                        .record_uint("write_count", stats.ep2_write_count.load(Ordering::Relaxed));
                    ep2_node.record_uint(
                        "write_errors",
                        stats.ep2_write_errors.load(Ordering::Relaxed),
                    );
                });
            }

            Ok(inspector)
        })
    })
}

/// Handle all of the ADB messages in an async context.
/// We receive commands from the main thread and then proxy them into the ADB channel.
/// We want to hold the wakelock until we have at least one outstanding read, because we
/// are always woken up on a new message. (If we have no outstanding reads we will not
/// receive any new messages).
///
/// At the same time we still need to handle writes and events. These are handled by always
/// clearing the proxy signal, but only clearing the kernel signal if we have an outstanding read.
async fn handle_adb(
    proxy: fadb::UsbAdbImpl_Proxy,
    message_counter: Option<zx::Counter>,
    read_commands: async_channel::Receiver<ReadCommand>,
    write_commands: async_channel::Receiver<WriteCommand>,
    state: Arc<LockDepMutex<FunctionFsState, FunctionFsStateLock>>,
    stats: Arc<FunctionFsStats>,
    session_id: u64,
) {
    /// Handle all of the events coming from the ADB device.
    ///
    /// adbd expects to receive events FUNCTIONFS_BIND, FUNCTIONFS_ENABLE, FUNCTION_DISABLE, and
    /// FUNCTIONFS_UNBIND in that order. If it receives these events out of order or does not
    /// receive some of the adb events, it may behave unexpectedly. In particular, please reference
    /// the `StartMonitor` function in `UsbFfsConnection` of `adb/daemon/usb.cpp`.
    ///
    /// A FUNCTIONFS_BIND event is enqueued synchronously by `create_endpoints` as soon as
    /// descriptors are written to ep0 and endpoints are established. When the driver is ready to
    /// take input it will send an `OnStatusChanged{ ONLINE }` event, which is when this module
    /// sends the FUNCTIONFS_ENABLE event to indicate that adbd should start processing data.
    ///
    /// When the driver sends an `OnStatusChanged{}` event, meaning that it's not online anymore.
    /// The module will send a FUNCTIONFS_DISABLE event to stop processing data. When the stream
    /// closes, we've unbound from the driver, and the module sends a FUNCTIONFS_UNBIND event.
    async fn handle_events(
        mut stream: fadb::UsbAdbImpl_EventStream,
        message_counter: &Option<zx::Counter>,
        state: Arc<LockDepMutex<FunctionFsState, FunctionFsStateLock>>,
        session_id: u64,
    ) {
        while let Some(Ok(fadb::UsbAdbImpl_Event::OnStatusChanged { status })) = stream.next().await
        {
            let is_online = status == fadb::StatusFlags::ONLINE;
            {
                let mut state_locked = state.lock();
                if state_locked.session_id != session_id {
                    return;
                }
                state_locked.is_online = is_online;
                state_locked.event_queue.push_back(usb_functionfs_event {
                    type_: if is_online {
                        usb_functionfs_event_type_FUNCTIONFS_ENABLE
                    } else {
                        usb_functionfs_event_type_FUNCTIONFS_DISABLE
                    } as u8,
                    ..Default::default()
                });
                state_locked.record_event(if is_online { "ENABLE" } else { "DISABLE" });
                state_locked.waiters.notify_fd_events(FdEvents::POLLIN);
                state_locked.waiters.notify_all();
            }

            // We can simply clear this after getting a response because we care about
            // reads. Allow new FIDL messages to come through and only go to sleep if
            // we have an outstanding read.
            message_counter.as_ref().map(mark_proxy_message_handled);
        }

        let mut state_locked = state.lock();
        if state_locked.session_id == session_id {
            state_locked.is_online = false;
            state_locked.has_input_output_endpoints = false;
            state_locked.adb_read_channel = None;
            state_locked.adb_write_channel = None;
            state_locked.event_queue.push_back(usb_functionfs_event {
                type_: usb_functionfs_event_type_FUNCTIONFS_UNBIND as u8,
                ..Default::default()
            });
            state_locked.record_event("UNBIND");
            state_locked.waiters.notify_fd_events(FdEvents::POLLIN);
            state_locked.waiters.notify_all();
        }
    }

    /// Consumes a stream of instants and decrements `message_counter` after
    /// each one. As long as one of the instants written to this channel is
    /// still in the future, we want to keep the container awake.
    ///
    /// NOTE: We're reusing `message_counter` in a way that's perhaps confusing:
    /// both as the number of "in flight" requests, and to track whether the ADB
    /// session seems to be idle or not. It may be clearer to have two separate
    /// counters.
    async fn handle_idle_timeouts(
        timeouts: async_channel::Receiver<zx::MonotonicInstant>,
        message_counter: &Option<zx::Counter>,
    ) {
        timeouts
            .for_each(|timeout| async move {
                use fasync::WakeupTime;
                timeout.into_timer().await;
                message_counter.as_ref().map(mark_proxy_message_handled);
            })
            .await
    }

    /// Handle the commands coming from the main thread.
    async fn handle_read_commands(
        proxy: &fadb::UsbAdbImpl_Proxy,
        timeouts_sender: async_channel::Sender<zx::MonotonicInstant>,
        commands: async_channel::Receiver<ReadCommand>,
        stats: &FunctionFsStats,
    ) {
        let timeouts_sender = &timeouts_sender;
        commands
            .for_each(|ReadCommand { pending }| async move {
                // Queue up our receive future. We want to do this before we decrement the counter,
                // which potentially allows the container to suspend.
                let receive_future = proxy.receive();

                // Don't decrement the message counter immediately. Instead, we
                // keep the container awake for some amount of time to allow
                // Starnix to react to the message. Otherwise, the container
                // might go directly to sleep without doing anything.
                timeouts_sender
                    .send(zx::MonotonicInstant::after(ADB_INTERACTION_TIMEOUT))
                    .await
                    .expect("Should be able to send timeout");

                let response = match receive_future.await {
                    Err(err) => {
                        if err.is_closed() {
                            log_info!("Receive failed due to connection shutdown: {err}");
                        } else {
                            log_warn!("Failed to call UsbAdbImpl.Receive: {err}");
                            stats.ep1_read_errors.fetch_add(1, Ordering::Relaxed);
                        }
                        error!(EINVAL)
                    }
                    Ok(Err(err)) => {
                        let status = zx::Status::err_from_raw(err);
                        if matches!(
                            status,
                            zx::Status::BAD_STATE | zx::Status::CANCELED | zx::Status::PEER_CLOSED
                        ) {
                            log_info!("Receive failed due to connection shutdown: {status}");
                        } else {
                            log_warn!("Failed to receive data from adb driver: {status}");
                            stats.ep1_read_errors.fetch_add(1, Ordering::Relaxed);
                        }
                        // TODO(b/536021189): Fix POSIX error mapping. We should return ESHUTDOWN
                        // on endpoint disable.
                        error!(EINVAL)
                    }
                    Ok(Ok(payload)) => {
                        stats
                            .ep1_total_bytes_read
                            .fetch_add(payload.len() as u64, Ordering::Relaxed);
                        stats.ep1_read_count.fetch_add(1, Ordering::Relaxed);
                        Ok(payload)
                    }
                };

                pending.set_result(response);
            })
            .await;
    }

    /// Handle the commands coming from the main thread.
    async fn handle_write_commands(
        proxy: &fadb::UsbAdbImpl_Proxy,
        timeouts_sender: async_channel::Sender<zx::MonotonicInstant>,
        commands: async_channel::Receiver<WriteCommand>,
        stats: &FunctionFsStats,
    ) {
        let timeouts_sender = &timeouts_sender;
        commands
            .for_each(|WriteCommand { data, pending }| async move {
                let response = match proxy.queue_tx(&data).await {
                    Err(err) => {
                        if err.is_closed() {
                            log_info!("QueueTx failed due to connection shutdown: {err}");
                        } else {
                            log_warn!("Failed to call UsbAdbImpl.QueueTx: {err}");
                            stats.ep2_write_errors.fetch_add(1, Ordering::Relaxed);
                        }
                        error!(EINVAL)
                    }
                    Ok(Err(err)) => {
                        let status = zx::Status::err_from_raw(err);
                        if matches!(
                            status,
                            zx::Status::BAD_STATE | zx::Status::CANCELED | zx::Status::PEER_CLOSED
                        ) {
                            log_info!("QueueTx failed due to connection shutdown: {status}");
                        } else {
                            log_warn!("Failed to queue data to adb driver: {status}");
                            stats.ep2_write_errors.fetch_add(1, Ordering::Relaxed);
                        }
                        error!(EINVAL)
                    }
                    Ok(Ok(_)) => {
                        stats
                            .ep2_total_bytes_written
                            .fetch_add(data.len() as u64, Ordering::Relaxed);
                        stats.ep2_write_count.fetch_add(1, Ordering::Relaxed);
                        Ok(data.len())
                    }
                };

                // Don't decrement the message counter immediately. We use the
                // ADB output as a signal that the ADB session is still
                // interactive.
                timeouts_sender
                    .send(zx::MonotonicInstant::after(ADB_INTERACTION_TIMEOUT))
                    .await
                    .expect("Should be able to send timeout");

                pending.set_result(response);
            })
            .await;
    }

    let (timeouts_sender, timeouts_receiver) = async_channel::unbounded();
    let event_future =
        handle_events(proxy.take_event_stream(), &message_counter, state, session_id);
    let write_commands_future =
        handle_write_commands(&proxy, timeouts_sender.clone(), write_commands, &stats);
    let read_commands_future = handle_read_commands(&proxy, timeouts_sender, read_commands, &stats);
    let timeout_future = handle_idle_timeouts(timeouts_receiver, &message_counter);
    futures::join!(event_future, write_commands_future, read_commands_future, timeout_future);
}

pub struct FunctionFs;
impl FunctionFs {
    pub fn new_fs(
        current_task: &CurrentTask,
        options: FileSystemOptions,
    ) -> Result<FileSystemHandle, Errno> {
        if options.source != "adb" {
            track_stub!(TODO("https://fxbug.dev/329699340"), "FunctionFS supports other uses");
            return error!(ENODEV);
        }

        // ADB daemon assumes that ADB works over USB if FunctionFS is able to mount.
        // Check that the ADB directory capability is provided to the kernel, and fail to mount
        // if it is not.
        if let Err(e) = std::fs::read_dir(ADB_DIRECTORY) {
            log_warn!(
                "Attempted to mount FunctionFS for adb, but could not read {ADB_DIRECTORY}: {e}"
            );
            return error!(ENODEV);
        }

        let uid = if let Some(uid) = options.params.get(b"uid") {
            fs_args::parse::<uid_t>(uid.as_ref())?
        } else {
            0
        };
        let gid = if let Some(gid) = options.params.get(b"gid") {
            fs_args::parse::<gid_t>(gid.as_ref())?
        } else {
            0
        };

        let fs = FileSystem::new(current_task.kernel(), CacheMode::Uncached, FunctionFs, options)?;

        let creds = FsCred { uid, gid };
        let info = FsNodeInfo::new(mode!(IFDIR, 0o777), creds);
        fs.create_root_with_info(
            ROOT_NODE_ID,
            FunctionFsRootDir::new(&current_task.kernel().inspect_node),
            info,
        );
        Ok(fs)
    }
}

impl FileSystemOps for FunctionFs {
    fn statfs(&self, _fs: &FileSystem, _current_task: &CurrentTask) -> Result<statfs, Errno> {
        Ok(default_statfs(FUNCTIONFS_MAGIC))
    }

    fn name(&self) -> &'static FsStr {
        b"functionfs".into()
    }
}

#[derive(Default)]
struct FunctionFsState {
    // Keeps track of the number of FileObject's created for the control endpoint.
    // When all FileObjects are closed, the filesystem resets to its initial state.
    // See https://docs.kernel.org/usb/functionfs.html.
    num_control_file_objects: usize,

    // Monotonically increasing session ID to prevent stale async event streams from
    // injecting FUNCTIONFS_UNBIND/DISABLE into a newly opened control session.
    session_id: u64,

    // Whether the FunctionFS has input/output endpoints, which are /ep2 and /ep1
    // respectively. /ep0 is the control endpoint and is always available.
    has_input_output_endpoints: bool,

    // Whether the FunctionFS is currently online (host connected).
    is_online: bool,

    // Number of times all control endpoints have closed and reset the filesystem.
    control_resets: u64,

    // Bounded ring buffer of lifecycle events exposed via lazy Inspect.
    event_history: VecDeque<EventRecord>,
    next_event_id: usize,

    adb_read_channel: Option<async_channel::Sender<ReadCommand>>,
    adb_write_channel: Option<async_channel::Sender<WriteCommand>>,

    // FIDL binding to the adb driver, for start and stop calls.
    device_proxy: Option<fadb::DeviceSynchronousProxy>,

    // FunctionFs events that indicate the connection state, to be read through
    // the control endpoint.
    event_queue: VecDeque<usb_functionfs_event>,

    waiters: WaitQueue,
}

impl FunctionFsState {
    fn record_event(&mut self, event: impl Into<Cow<'static, str>>) {
        if self.event_history.len() == MAX_INSPECT_EVENTS {
            self.event_history.pop_front();
        }
        let id = self.next_event_id;
        self.next_event_id += 1;
        self.event_history.push_back(EventRecord {
            id,
            timestamp: zx::BootInstant::get(),
            event: event.into(),
        });
    }
}

pub enum AdbProxyMode {
    /// Don't proxy events at all.
    None,

    /// Have the Starnix runner proxy events such that the container
    /// will wake up if events are received while the container is
    /// suspended.
    WakeContainer,
}

async fn connect_to_device(
    proxy: AdbProxyMode,
) -> Result<(fadb::DeviceSynchronousProxy, fadb::UsbAdbImpl_Proxy, Option<zx::Counter>), Errno> {
    let service = Service::open(fadb::ServiceMarker).map_err(|_| errno!(EINVAL))?;
    let instances = service.enumerate().await.map_err(|_| errno!(EINVAL))?;

    for instance in instances {
        let Ok(device_proxy) = instance.connect_to_adb() else {
            continue;
        };

        let (adb_proxy, server_end) =
            fidl::endpoints::create_sync_proxy::<fadb::UsbAdbImpl_Marker>();
        let (adb_channel, message_counter) = match proxy {
            AdbProxyMode::None => (adb_proxy.into_channel(), None),
            AdbProxyMode::WakeContainer => {
                let (channel, message_counter) = create_proxy_for_wake_events_counter_zero(
                    adb_proxy.into_channel(),
                    "adb".to_string(),
                );
                (channel, Some(message_counter))
            }
        };

        let timer =
            fasync::Timer::new(zx::MonotonicInstant::after(zx::Duration::from_seconds(5))).fuse();
        let start_fut = device_proxy.start_adb(server_end).fuse();
        pin_mut!(timer, start_fut);
        let start_ok = futures::select! {
            res = start_fut => matches!(res, Ok(Ok(()))),
            _ = timer => false,
        };

        if start_ok {
            let Ok(channel) = device_proxy.into_channel() else {
                continue;
            };
            let device_proxy = fadb::DeviceSynchronousProxy::new(channel.into_zx_channel());
            let adb_proxy =
                fadb::UsbAdbImpl_Proxy::new(fidl::AsyncChannel::from_channel(adb_channel));
            return Ok((device_proxy, adb_proxy, message_counter));
        }
    }

    error!(EBUSY)
}

async fn manage_adb_connection(
    read_commands: async_channel::Receiver<ReadCommand>,
    write_commands: async_channel::Receiver<WriteCommand>,
    state: Arc<LockDepMutex<FunctionFsState, FunctionFsStateLock>>,
    stats: Arc<FunctionFsStats>,
    session_id: u64,
) {
    let mut watcher = match Service::open(fadb::ServiceMarker) {
        Ok(service) => service.watch().await.ok(),
        Err(_) => None,
    };

    loop {
        // Verify that the session is still active.
        {
            let state_locked = state.lock();
            if state_locked.session_id != session_id
                || !state_locked.has_input_output_endpoints
                || state_locked.num_control_file_objects == 0
            {
                return;
            }
        }

        // Try connecting to the ADB device asynchronously.
        if let Ok((device_proxy, adb_proxy, message_counter)) =
            connect_to_device(AdbProxyMode::WakeContainer).await
        {
            {
                let mut state_locked = state.lock();
                if state_locked.session_id != session_id || !state_locked.has_input_output_endpoints
                {
                    let deadline = zx::MonotonicInstant::after(zx::Duration::from_seconds(5));
                    if let Err(e) = device_proxy.stop_adb(deadline) {
                        log_warn!("Failed to stop adb driver after session cancelled: {e:?}");
                    }
                    return;
                }
                state_locked.device_proxy = Some(device_proxy);
            }

            handle_adb(
                adb_proxy,
                message_counter,
                read_commands.clone(),
                write_commands.clone(),
                Arc::clone(&state),
                Arc::clone(&stats),
                session_id,
            )
            .await;

            {
                let mut state_locked = state.lock();
                let _ = state_locked.device_proxy.take();
            }
        } else {
            // Sleep 500ms unconditionally when retrying a failed connection.
            fasync::Timer::new(zx::MonotonicInstant::after(zx::Duration::from_millis(500))).await;
        }

        // Check if session ended before waiting for the next connection.
        {
            let state_locked = state.lock();
            if state_locked.session_id != session_id
                || !state_locked.has_input_output_endpoints
                || state_locked.num_control_file_objects == 0
            {
                return;
            }
        }

        // If no ADB instances currently exist, park on the watcher until an instance appears.
        let has_instances = match Service::open(fadb::ServiceMarker) {
            Ok(service) => {
                service.enumerate().await.map(|instances| !instances.is_empty()).unwrap_or(false)
            }
            Err(_) => false,
        };
        if !has_instances {
            if let Some(w) = watcher.as_mut() {
                match w.next().await {
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => {
                        // Watcher stream ended or errored; re-open service watcher.
                        watcher = match Service::open(fadb::ServiceMarker) {
                            Ok(service) => service.watch().await.ok(),
                            Err(_) => None,
                        };
                    }
                }
            } else {
                fasync::Timer::new(zx::MonotonicInstant::after(zx::Duration::from_millis(500)))
                    .await;
                watcher = match Service::open(fadb::ServiceMarker) {
                    Ok(service) => service.watch().await.ok(),
                    Err(_) => None,
                };
            }
        }
    }
}

#[derive(Default)]
struct FunctionFsRootDir {
    state: Arc<LockDepMutex<FunctionFsState, FunctionFsStateLock>>,
    stats: Arc<FunctionFsStats>,
    _inspect_node: inspect::LazyNode,
}

impl FunctionFsRootDir {
    fn new(parent_inspect_node: &inspect::Node) -> Self {
        let stats = Arc::new(FunctionFsStats::default());
        let state = Arc::new(LockDepMutex::new(FunctionFsState {
            event_history: VecDeque::with_capacity(MAX_INSPECT_EVENTS),
            ..Default::default()
        }));
        let inspect_node = create_lazy_inspect_node(parent_inspect_node, &state, &stats);
        Self { state, stats, _inspect_node: inspect_node }
    }

    fn create_endpoints(&self, kernel: &Kernel) -> Result<(), Errno> {
        let mut state = self.state.lock();

        // create_endpoints can be called multiple times as descriptors are written
        // to the control endpoint.
        if state.has_input_output_endpoints {
            return Ok(());
        }

        state.session_id = state.session_id.wrapping_add(1);
        let session_id = state.session_id;

        let (read_command_sender, read_command_receiver) = async_channel::unbounded();
        state.adb_read_channel = Some(read_command_sender);

        let (write_command_sender, write_command_receiver) = async_channel::unbounded();
        state.adb_write_channel = Some(write_command_sender);

        state.event_queue.clear();
        state.event_queue.push_back(usb_functionfs_event {
            type_: usb_functionfs_event_type_FUNCTIONFS_BIND as u8,
            ..Default::default()
        });
        state.record_event("BIND");
        state.waiters.notify_fd_events(FdEvents::POLLIN);
        state.waiters.notify_all();
        state.has_input_output_endpoints = true;
        state.record_event("ENDPOINTS_CREATED");

        let state_copy = Arc::clone(&self.state);
        let stats_copy = Arc::clone(&self.stats);
        // Spawn background worker to watch for USB ADB hardware service and manage connection lifecycle.
        kernel.kthreads.spawn_future(
            move || async move {
                manage_adb_connection(
                    read_command_receiver,
                    write_command_receiver,
                    state_copy,
                    stats_copy,
                    session_id,
                )
                .await
            },
            "functionfs_adb_worker",
        );

        Ok(())
    }

    fn from_fs(fs: &FileSystem) -> &Self {
        fs.root()
            .node
            .downcast_ops::<FunctionFsRootDir>()
            .expect("failed to downcast functionfs root dir")
    }

    fn from_file(file: &FileObject) -> &Self {
        Self::from_fs(&file.fs)
    }

    fn on_control_opened(&self) {
        let mut state = self.state.lock();
        state.num_control_file_objects += 1;
        state.record_event("CONTROL_OPENED");
    }

    fn on_control_closed(&self) {
        let mut state = self.state.lock();
        state.num_control_file_objects -= 1;
        state.record_event("CONTROL_CLOSED");
        if state.num_control_file_objects == 0 {
            state.session_id = state.session_id.wrapping_add(1);
            // When all control endpoints are closed, the filesystem resets to its initial state.
            if let Some(device_proxy) = state.device_proxy.take() {
                // Use a bounded 5-second deadline to prevent close_control_file / adbd exit
                // from hanging indefinitely if the ADB driver daemon or FIDL channel is hung
                // or unbinding during a role switch.
                let deadline = zx::MonotonicInstant::after(zx::Duration::from_seconds(5));
                match device_proxy.stop_adb(deadline) {
                    Ok(Ok(())) => {}
                    Ok(Err(status)) => {
                        log_warn!(
                            "Failed to stop adb driver on control reset: {}",
                            zx::Status::err_from_raw(status)
                        );
                    }
                    Err(err) => {
                        log_warn!("FIDL error calling StopAdb on control reset: {err}");
                    }
                }
            }

            state.has_input_output_endpoints = false;
            state.is_online = false;
            state.adb_read_channel = None;
            state.adb_write_channel = None;
            state.event_queue.clear();
            state.control_resets += 1;
            state.record_event("CONTROL_RESET");
            state.waiters.notify_all();
        }
    }

    fn wait_until_online(
        &self,
        current_task: &CurrentTask,
        file: &FileObject,
    ) -> Result<(), Errno> {
        let initial_session_id = self.state.lock().session_id;
        loop {
            let waiter = {
                let state = self.state.lock();
                if !state.has_input_output_endpoints || state.session_id != initial_session_id {
                    return error!(ESHUTDOWN);
                }
                if state.is_online {
                    return Ok(());
                }
                if file.flags().contains(OpenFlags::NONBLOCK) {
                    return error!(EAGAIN);
                }
                let waiter = Waiter::new();
                state.waiters.wait_async(&waiter);
                waiter
            };
            waiter.wait(current_task)?;
        }
    }

    fn available(&self) -> usize {
        let state = self.state.lock();
        state.event_queue.len()
    }

    fn write(
        &self,
        current_task: &CurrentTask,
        file: &FileObject,
        data: Vec<u8>,
    ) -> Result<usize, Errno> {
        self.wait_until_online(current_task, file)?;

        let pending = Arc::<PendingResult<usize>>::default();
        let guard = pending.event.begin_wait();

        if let Some(channel) = self.state.lock().adb_write_channel.as_ref() {
            channel.send_blocking(WriteCommand { data, pending: pending.clone() }).map_err(
                |err| {
                    log_warn!(
                        "FunctionFsRootDir::write (ep2 bulk IN) failed to send command for task {} (pid {}): {err}",
                        current_task.command(),
                        current_task.get_pid()
                    );
                    errno!(EINVAL)
                },
            )?;
        } else {
            log_warn!(
                "FunctionFsRootDir::write (ep2 bulk IN) called by task {} (pid {}) with {} bytes, but adb_write_channel is None (ENODEV)",
                current_task.command(),
                current_task.get_pid(),
                data.len()
            );
            return error!(ENODEV);
        }

        current_task.block_until(guard, zx::MonotonicInstant::INFINITE)?;

        let mut result = pending.result.lock();
        result.take().ok_or_else(|| errno!(EINTR))?
    }

    fn read(&self, current_task: &CurrentTask, file: &FileObject) -> Result<Vec<u8>, Errno> {
        self.wait_until_online(current_task, file)?;

        let pending = Arc::<PendingResult<Vec<u8>>>::default();
        let guard = pending.event.begin_wait();
        if let Some(channel) = self.state.lock().adb_read_channel.as_ref() {
            channel.send_blocking(ReadCommand { pending: pending.clone() }).map_err(|err| {
                log_warn!(
                    "FunctionFsRootDir::read (ep1 bulk OUT) failed to send command for task {} (pid {}): {err}",
                    current_task.command(),
                    current_task.get_pid()
                );
                errno!(EINVAL)
            })?;
        } else {
            log_warn!(
                "FunctionFsRootDir::read (ep1 bulk OUT) called by task {} (pid {}), but adb_read_channel is None (ENODEV)",
                current_task.command(),
                current_task.get_pid()
            );
            return error!(ENODEV);
        }

        current_task.block_until(guard, zx::MonotonicInstant::INFINITE)?;

        let mut result = pending.result.lock();
        result.take().ok_or_else(|| errno!(EINTR))?
    }
}

impl FsNodeOps for FunctionFsRootDir {
    fs_node_impl_dir_readonly!();

    fn create_file_ops(
        &self,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        let mut entries = vec![];
        entries.push(VecDirectoryEntry {
            entry_type: DirectoryEntryType::REG,
            name: CONTROL_ENDPOINT.into(),
            inode: Some(CONTROL_ENDPOINT_NODE_ID),
        });

        let state = self.state.lock();
        if state.has_input_output_endpoints {
            entries.push(VecDirectoryEntry {
                entry_type: DirectoryEntryType::REG,
                name: INPUT_ENDPOINT.into(),
                inode: Some(INPUT_ENDPOINT_NODE_ID),
            });
            entries.push(VecDirectoryEntry {
                entry_type: DirectoryEntryType::REG,
                name: OUTPUT_ENDPOINT.into(),
                inode: Some(OUTPUT_ENDPOINT_NODE_ID),
            });
        }

        Ok(VecDirectory::new_file(entries))
    }

    fn lookup(
        &self,
        entry: &DirEntry,
        _current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<starnix_core::vfs::FsNodeHandle, Errno> {
        let name = std::str::from_utf8(name).map_err(|_| errno!(ENOENT))?;
        let cred = entry.node.info().cred();
        match name {
            CONTROL_ENDPOINT => Ok(entry.node.fs().create_node(
                CONTROL_ENDPOINT_NODE_ID,
                FunctionFsControlEndpoint,
                FsNodeInfo::new(mode!(IFREG, 0o600), cred),
            )),
            OUTPUT_ENDPOINT => Ok(entry.node.fs().create_node(
                OUTPUT_ENDPOINT_NODE_ID,
                FunctionFsOutputEndpoint,
                FsNodeInfo::new(mode!(IFREG, 0o600), cred),
            )),
            INPUT_ENDPOINT => Ok(entry.node.fs().create_node(
                INPUT_ENDPOINT_NODE_ID,
                FunctionFsInputEndpoint,
                FsNodeInfo::new(mode!(IFREG, 0o600), cred),
            )),
            _ => error!(ENOENT),
        }
    }
}

// FunctionFS Control Endpoint is both readable and writable.
// Clients should write USB descriptors to the endpoint to setup the USB connection.
// Clients can read `usb_functionfs_event`s to know about the USB connection state.
struct FunctionFsControlEndpoint;
impl FsNodeOps for FunctionFsControlEndpoint {
    fs_node_impl_not_dir!();

    fn create_file_ops(
        &self,
        node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        let fs = node.fs();
        let rootdir = fs
            .root()
            .node
            .downcast_ops::<FunctionFsRootDir>()
            .expect("failed to downcast functionfs root dir");
        rootdir.on_control_opened();
        Ok(Box::new(FunctionFsControlEndpoint))
    }
}

impl FileOps for FunctionFsControlEndpoint {
    fileops_impl_seekless!();
    fileops_impl_noop_sync!();

    fn close(self: Box<Self>, file: &FileObjectState, _current_task: &CurrentTask) {
        let rootdir = FunctionFsRootDir::from_fs(&file.fs);
        rootdir.on_control_closed();
    }

    fn read(
        &self,
        file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        // The control endpoint does not currently implement blocking read.
        // ADB would only read from this endpoint after polling it.
        track_stub!(
            TODO("https://fxbug.dev/329699340"),
            "FunctionFS blocking read on control endpoint"
        );

        let rootdir = FunctionFsRootDir::from_file(file);

        let mut state = rootdir.state.lock();
        if !state.event_queue.is_empty() {
            if data.available() < std::mem::size_of::<usb_functionfs_event>() {
                return error!(EINVAL);
            }
        } else {
            return error!(EAGAIN);
        }
        let front = state.event_queue.pop_front().expect("pop from non-empty event queue");
        data.write(front.as_bytes())
    }

    fn write(
        &self,
        file: &FileObject,
        current_task: &CurrentTask,
        _offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        // The ADB driver creates and passes its own descriptors to the host system over the wire,
        // and so, Starnix does not need to parse the descriptors that Android sends.
        // Here we directly attempt to connect to the driver via FIDL, and create endpoints for data transfer.
        track_stub!(TODO("https://fxbug.dev/329699340"), "FunctionFS should parse descriptors");

        let rootdir = FunctionFsRootDir::from_file(file);
        rootdir.create_endpoints(current_task.kernel().deref())?;

        Ok(data.drain())
    }

    fn wait_async(
        &self,
        file: &FileObject,
        _current_task: &CurrentTask,
        waiter: &Waiter,
        events: FdEvents,
        handler: EventHandler,
    ) -> Option<WaitCanceler> {
        let rootdir = FunctionFsRootDir::from_file(file);
        let state = rootdir.state.lock();
        Some(state.waiters.wait_async_fd_events(waiter, events, handler))
    }

    fn query_events(
        &self,
        file: &FileObject,
        _current_task: &CurrentTask,
    ) -> Result<FdEvents, Errno> {
        let rootdir = FunctionFsRootDir::from_file(file);
        if rootdir.available() > 0 { Ok(FdEvents::POLLIN) } else { Ok(FdEvents::empty()) }
    }
}

// FunctionFSInputEndpoint is device to host communication, a.k.a. the "IN" USB direction.
// This endpoint is writable, and not readable.
struct FunctionFsInputEndpoint;
impl FsNodeOps for FunctionFsInputEndpoint {
    fs_node_impl_not_dir!();

    fn create_file_ops(
        &self,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        Ok(Box::new(FunctionFsInputEndpoint))
    }
}

impl FileOps for FunctionFsInputEndpoint {
    fileops_impl_seekless!();
    fileops_impl_noop_sync!();

    fn read(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        _data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        error!(EINVAL)
    }

    fn write(
        &self,
        file: &FileObject,
        current_task: &CurrentTask,
        _offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        let bytes = data.read_all().map_err(|err| {
            log_warn!(
                "FunctionFsInputEndpoint (ep2 bulk IN) input buffer read failed for task {} (pid {}): {err}",
                current_task.command(),
                current_task.get_pid()
            );
            err
        })?;
        let rootdir = FunctionFsRootDir::from_file(file);
        rootdir.write(current_task, file, bytes)
    }
}

// FunctionFSOutputEndpoint is host to device communication, a.k.a. the "OUT" USB direction.
// This endpoint is readable, and not writable.
struct FunctionFsOutputEndpoint;
impl FsNodeOps for FunctionFsOutputEndpoint {
    fs_node_impl_not_dir!();

    fn create_file_ops(
        &self,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        Ok(Box::new(FunctionFsOutputFileObject))
    }
}

struct FunctionFsOutputFileObject;

impl FileOps for FunctionFsOutputFileObject {
    fileops_impl_seekless!();
    fileops_impl_noop_sync!();

    fn read(
        &self,
        file: &FileObject,
        current_task: &CurrentTask,
        _offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        let rootdir = FunctionFsRootDir::from_file(file);
        let payload = rootdir.read(current_task, file)?;
        if payload.len() > data.available() {
            // This means the data will only be partially written, with the rest discarded.
            // Instead of attempting this, we'll instead return error to the client.
            log_warn!(
                "FunctionFsOutputFileObject (ep1 bulk OUT) buffer overflow for task {} (pid {}): payload len {} > buffer avail {}",
                current_task.command(),
                current_task.get_pid(),
                payload.len(),
                data.available()
            );
            rootdir.stats.ep1_buffer_overflow_errors.fetch_add(1, Ordering::Relaxed);
            return error!(EINVAL);
        }

        data.write(&payload).map_err(|err| {
            log_warn!(
                "FunctionFsOutputFileObject (ep1 bulk OUT) output buffer write failed for task {} (pid {}): {err}",
                current_task.command(),
                current_task.get_pid()
            );
            err
        })
    }

    fn write(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        _data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        error!(EINVAL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diagnostics_assertions::{AnyProperty, TreeAssertion, assert_data_tree};
    use fidl::endpoints::RequestStream;
    use std::pin::pin;

    #[fuchsia::test]
    async fn test_inspect_initial_hierarchy() {
        let inspector = inspect::Inspector::default();
        let _rootdir = FunctionFsRootDir::new(inspector.root());

        assert_data_tree!(inspector, root: {
            "usb-functionfs": {
                is_online: false,
                num_control_file_objects: 0u64,
                event_queue_depth: 0u64,
                has_input_output_endpoints: false,
                control_resets: 0u64,
                event_history: {},
                ep1_bulk_out: {
                    total_bytes_read: 0u64,
                    read_count: 0u64,
                    read_errors: 0u64,
                    buffer_overflow_errors: 0u64,
                },
                ep2_bulk_in: {
                    total_bytes_written: 0u64,
                    write_count: 0u64,
                    write_errors: 0u64,
                },
            }
        });
    }

    #[fuchsia::test]
    async fn test_control_endpoint_lifecycle_and_reset() {
        let inspector = inspect::Inspector::default();
        let rootdir = FunctionFsRootDir::new(inspector.root());

        // Open two control file objects.
        rootdir.on_control_opened();
        rootdir.on_control_opened();

        // Simulate active endpoints, online state, and a queued event before closing.
        {
            let mut state_locked = rootdir.state.lock();
            state_locked.has_input_output_endpoints = true;
            state_locked.is_online = true;
            state_locked.event_queue.push_back(usb_functionfs_event::default());
        }

        assert_data_tree!(inspector, root: {
            "usb-functionfs": contains {
                num_control_file_objects: 2u64,
                has_input_output_endpoints: true,
                is_online: true,
                event_queue_depth: 1u64,
                control_resets: 0u64,
                event_history: {
                    "0": {
                        "@time": AnyProperty,
                        event: "CONTROL_OPENED",
                    },
                    "1": {
                        "@time": AnyProperty,
                        event: "CONTROL_OPENED",
                    },
                },
            }
        });

        // Closing the first control file object decrements the count to 1 without resetting state.
        rootdir.on_control_closed();
        {
            let state_locked = rootdir.state.lock();
            assert_eq!(state_locked.num_control_file_objects, 1);
            assert!(state_locked.has_input_output_endpoints);
            assert!(state_locked.is_online);
            assert_eq!(state_locked.event_queue.len(), 1);
        }

        assert_data_tree!(inspector, root: {
            "usb-functionfs": contains {
                num_control_file_objects: 1u64,
                has_input_output_endpoints: true,
                is_online: true,
                event_queue_depth: 1u64,
                control_resets: 0u64,
                event_history: {
                    "0": {
                        "@time": AnyProperty,
                        event: "CONTROL_OPENED",
                    },
                    "1": {
                        "@time": AnyProperty,
                        event: "CONTROL_OPENED",
                    },
                    "2": {
                        "@time": AnyProperty,
                        event: "CONTROL_CLOSED",
                    },
                },
            }
        });

        // Closing the last control file object resets state and logs CONTROL_RESET.
        rootdir.on_control_closed();
        {
            let state_locked = rootdir.state.lock();
            assert_eq!(state_locked.num_control_file_objects, 0);
            assert!(!state_locked.has_input_output_endpoints);
            assert!(!state_locked.is_online);
            assert!(state_locked.event_queue.is_empty());
        }

        assert_data_tree!(inspector, root: {
            "usb-functionfs": contains {
                num_control_file_objects: 0u64,
                has_input_output_endpoints: false,
                is_online: false,
                event_queue_depth: 0u64,
                control_resets: 1u64,
                event_history: {
                    "0": {
                        "@time": AnyProperty,
                        event: "CONTROL_OPENED",
                    },
                    "1": {
                        "@time": AnyProperty,
                        event: "CONTROL_OPENED",
                    },
                    "2": {
                        "@time": AnyProperty,
                        event: "CONTROL_CLOSED",
                    },
                    "3": {
                        "@time": AnyProperty,
                        event: "CONTROL_CLOSED",
                    },
                    "4": {
                        "@time": AnyProperty,
                        event: "CONTROL_RESET",
                    },
                },
            }
        });
    }

    #[fuchsia::test]
    async fn test_event_history_ring_buffer_eviction() {
        let inspector = inspect::Inspector::default();
        let rootdir = FunctionFsRootDir::new(inspector.root());

        {
            let mut state = rootdir.state.lock();
            for i in 0..35 {
                state.record_event(format!("event_{i}"));
            }
        }

        let mut event_history_assertion = TreeAssertion::new("event_history", true);
        for i in 3..35 {
            let mut child = TreeAssertion::new(&i.to_string(), true);
            child.add_property_assertion("@time", Arc::new(AnyProperty));
            child.add_property_assertion("event", Arc::new(format!("event_{i}")));
            event_history_assertion.add_child_assertion(child);
        }

        assert_data_tree!(inspector, root: {
            "usb-functionfs": contains {
                event_history_assertion,
            }
        });
    }

    #[fuchsia::test]
    fn test_handle_adb_inspect_updates_and_error_filtering() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();

        let mut test_fut = pin!(async {
            let inspector = inspect::Inspector::default();
            let rootdir = FunctionFsRootDir::new(inspector.root());

            let (proxy, mut stream) =
                fidl::endpoints::create_proxy_and_stream::<fadb::UsbAdbImpl_Marker>();
            let (read_sender, read_receiver) = async_channel::unbounded();
            let (write_sender, write_receiver) = async_channel::unbounded();

            // Simulate the BIND event and endpoints established by create_endpoints().
            {
                let mut state = rootdir.state.lock();
                state.has_input_output_endpoints = true;
                state.event_queue.push_back(usb_functionfs_event {
                    type_: usb_functionfs_event_type_FUNCTIONFS_BIND as u8,
                    ..Default::default()
                });
                state.record_event("BIND");
            }

            let adb_fut = handle_adb(
                proxy,
                None,
                read_receiver,
                write_receiver,
                Arc::clone(&rootdir.state),
                Arc::clone(&rootdir.stats),
                0,
            );

            let driver_fut = async move {
                // Transition to ONLINE.
                stream
                    .control_handle()
                    .send_on_status_changed(fadb::StatusFlags::ONLINE)
                    .expect("send ONLINE status");

                // 1. Successful Receive (4 bytes).
                let read_ok = Arc::<PendingResult<Vec<u8>>>::default();
                read_sender
                    .send(ReadCommand { pending: read_ok.clone() })
                    .await
                    .expect("send read command");
                match stream.next().await.expect("stream item").expect("fidl request") {
                    fadb::UsbAdbImpl_Request::Receive { responder } => {
                        responder.send(Ok(&[1, 2, 3, 4])).expect("send Receive Ok");
                    }
                    other => panic!("Unexpected request: {other:?}"),
                }

                // 2. Genuine driver error on Receive (ZX_ERR_IO) -> increments read_errors.
                let read_io_err = Arc::<PendingResult<Vec<u8>>>::default();
                read_sender
                    .send(ReadCommand { pending: read_io_err.clone() })
                    .await
                    .expect("send read command");
                match stream.next().await.expect("stream item").expect("fidl request") {
                    fadb::UsbAdbImpl_Request::Receive { responder } => {
                        responder
                            .send(Err(zx::Status::IO.into_raw()))
                            .expect("send Receive IO error");
                    }
                    other => panic!("Unexpected request: {other:?}"),
                }

                // 3. Expected shutdown errors on Receive -> must NOT increment read_errors.
                for status in [zx::Status::BAD_STATE, zx::Status::CANCELED, zx::Status::PEER_CLOSED]
                {
                    let pending = Arc::<PendingResult<Vec<u8>>>::default();
                    read_sender.send(ReadCommand { pending }).await.expect("send read command");
                    match stream.next().await.expect("stream item").expect("fidl request") {
                        fadb::UsbAdbImpl_Request::Receive { responder } => {
                            responder
                                .send(Err(status.into_raw()))
                                .expect("send Receive shutdown error");
                        }
                        other => panic!("Unexpected request: {other:?}"),
                    }
                }

                // 4. Successful QueueTx (5 bytes).
                let write_ok = Arc::<PendingResult<usize>>::default();
                write_sender
                    .send(WriteCommand {
                        data: vec![10, 20, 30, 40, 50],
                        pending: write_ok.clone(),
                    })
                    .await
                    .expect("send write command");
                match stream.next().await.expect("stream item").expect("fidl request") {
                    fadb::UsbAdbImpl_Request::QueueTx { data, responder } => {
                        assert_eq!(data, vec![10, 20, 30, 40, 50]);
                        responder.send(Ok(())).expect("send QueueTx Ok");
                    }
                    other => panic!("Unexpected request: {other:?}"),
                }

                // 5. Genuine driver error on QueueTx (ZX_ERR_IO) -> increments write_errors.
                let write_io_err = Arc::<PendingResult<usize>>::default();
                write_sender
                    .send(WriteCommand { data: vec![1, 2], pending: write_io_err.clone() })
                    .await
                    .expect("send write command");
                match stream.next().await.expect("stream item").expect("fidl request") {
                    fadb::UsbAdbImpl_Request::QueueTx { responder, .. } => {
                        responder
                            .send(Err(zx::Status::IO.into_raw()))
                            .expect("send QueueTx IO error");
                    }
                    other => panic!("Unexpected request: {other:?}"),
                }

                // 6. Expected shutdown errors on QueueTx -> must NOT increment write_errors.
                for status in [zx::Status::BAD_STATE, zx::Status::CANCELED, zx::Status::PEER_CLOSED]
                {
                    let pending = Arc::<PendingResult<usize>>::default();
                    write_sender
                        .send(WriteCommand { data: vec![1], pending })
                        .await
                        .expect("send write command");
                    match stream.next().await.expect("stream item").expect("fidl request") {
                        fadb::UsbAdbImpl_Request::QueueTx { responder, .. } => {
                            responder
                                .send(Err(status.into_raw()))
                                .expect("send QueueTx shutdown error");
                        }
                        other => panic!("Unexpected request: {other:?}"),
                    }
                }

                // Transition to offline before closing the FIDL channel.
                stream
                    .control_handle()
                    .send_on_status_changed(fadb::StatusFlags::empty())
                    .expect("send offline status");

                // Drop the FIDL request stream to close the server channel.
                drop(stream);

                // 7. Closed FIDL channel on Receive and QueueTx -> must NOT increment error counters.
                let read_closed = Arc::<PendingResult<Vec<u8>>>::default();
                read_sender
                    .send(ReadCommand { pending: read_closed.clone() })
                    .await
                    .expect("send read command on closed channel");
                let write_closed = Arc::<PendingResult<usize>>::default();
                write_sender
                    .send(WriteCommand { data: vec![9], pending: write_closed.clone() })
                    .await
                    .expect("send write command on closed channel");

                drop(read_sender);
                drop(write_sender);

                (read_ok, read_io_err, write_ok, write_io_err, read_closed, write_closed)
            };

            let ((), (read_ok, read_io_err, write_ok, write_io_err, read_closed, write_closed)) =
                futures::join!(adb_fut, driver_fut);

            assert_eq!(read_ok.result.lock().take(), Some(Ok(vec![1, 2, 3, 4])));
            assert_eq!(read_io_err.result.lock().take(), Some(error!(EINVAL)));
            assert_eq!(write_ok.result.lock().take(), Some(Ok(5)));
            assert_eq!(write_io_err.result.lock().take(), Some(error!(EINVAL)));
            assert_eq!(read_closed.result.lock().take(), Some(error!(EINVAL)));
            assert_eq!(write_closed.result.lock().take(), Some(error!(EINVAL)));

            assert_data_tree!(inspector, root: {
                "usb-functionfs": {
                    is_online: false,
                    num_control_file_objects: 0u64,
                    event_queue_depth: 4u64,
                    has_input_output_endpoints: false,
                    control_resets: 0u64,
                    event_history: {
                        "0": {
                            "@time": AnyProperty,
                            event: "BIND",
                        },
                        "1": {
                            "@time": AnyProperty,
                            event: "ENABLE",
                        },
                        "2": {
                            "@time": AnyProperty,
                            event: "DISABLE",
                        },
                        "3": {
                            "@time": AnyProperty,
                            event: "UNBIND",
                        },
                    },
                    ep1_bulk_out: {
                        total_bytes_read: 4u64,
                        read_count: 1u64,
                        read_errors: 1u64,
                        buffer_overflow_errors: 0u64,
                    },
                    ep2_bulk_in: {
                        total_bytes_written: 5u64,
                        write_count: 1u64,
                        write_errors: 1u64,
                    },
                }
            });
        });

        while exec.run_until_stalled(&mut test_fut).is_pending() {
            assert!(exec.wake_next_timer().is_some(), "Executor stalled with no pending timers");
        }
    }

    #[fuchsia::test]
    fn test_stale_session_stream_closure_does_not_inject_unbind() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();

        let mut test_fut = pin!(async {
            let inspector = inspect::Inspector::default();
            let rootdir = FunctionFsRootDir::new(inspector.root());

            let (proxy1, stream1) =
                fidl::endpoints::create_proxy_and_stream::<fadb::UsbAdbImpl_Marker>();
            let (read_sender1, read_receiver1) = async_channel::unbounded();
            let (write_sender1, write_receiver1) = async_channel::unbounded();

            // Start session 1.
            rootdir.state.lock().session_id = 1;
            rootdir.state.lock().is_online = true;

            let adb_fut1 = handle_adb(
                proxy1,
                None,
                read_receiver1,
                write_receiver1,
                Arc::clone(&rootdir.state),
                Arc::clone(&rootdir.stats),
                1,
            );

            // Simulate opening session 2 while session 1's async task is still running.
            // The session ID is incremented and active state is updated.
            {
                let mut state = rootdir.state.lock();
                state.session_id = 2;
                state.is_online = true;
                state.event_queue.clear();
            }

            // Drop command senders and stream1 to allow adb_fut1 to complete.
            drop(read_sender1);
            drop(write_sender1);
            drop(stream1);

            // Run adb_fut1 to completion.
            adb_fut1.await;

            // Verify that the stale stream1 did NOT set is_online to false or push UNBIND
            // into session 2's event queue.
            let state = rootdir.state.lock();
            assert!(state.is_online, "Stale session termination must not set is_online to false");
            assert!(
                state.event_queue.is_empty(),
                "Stale session termination must not push UNBIND into the active session queue"
            );
        });

        while exec.run_until_stalled(&mut test_fut).is_pending() {
            assert!(exec.wake_next_timer().is_some(), "Executor stalled with no pending timers");
        }
    }

    #[fuchsia::test]
    fn test_stale_session_status_changed_is_ignored() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();

        let mut test_fut = pin!(async {
            let inspector = inspect::Inspector::default();
            let rootdir = FunctionFsRootDir::new(inspector.root());

            let (proxy1, stream1) =
                fidl::endpoints::create_proxy_and_stream::<fadb::UsbAdbImpl_Marker>();
            let (read_sender1, read_receiver1) = async_channel::unbounded();
            let (write_sender1, write_receiver1) = async_channel::unbounded();

            rootdir.state.lock().session_id = 1;

            let adb_fut1 = handle_adb(
                proxy1,
                None,
                read_receiver1,
                write_receiver1,
                Arc::clone(&rootdir.state),
                Arc::clone(&rootdir.stats),
                1,
            );

            let driver_state = Arc::clone(&rootdir.state);
            let driver_fut = async move {
                // Advance session_id to 2 before sending event from stream1.
                {
                    let mut state = driver_state.lock();
                    state.session_id = 2;
                    state.is_online = true;
                    state.event_queue.clear();
                }

                // Send OnStatusChanged on the stale stream1.
                stream1
                    .control_handle()
                    .send_on_status_changed(fadb::StatusFlags::empty())
                    .expect("send offline status");

                drop(read_sender1);
                drop(write_sender1);
                drop(stream1);
            };

            futures::join!(adb_fut1, driver_fut);

            let state = rootdir.state.lock();
            assert!(state.is_online, "Stale status change must not overwrite is_online");
            assert!(
                state.event_queue.is_empty(),
                "Stale status change must not push events to active session"
            );
        });

        while exec.run_until_stalled(&mut test_fut).is_pending() {
            assert!(exec.wake_next_timer().is_some(), "Executor stalled with no pending timers");
        }
    }

    #[fuchsia::test]
    fn test_unbind_clears_endpoints_and_channels() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();

        let mut test_fut = pin!(async {
            let inspector = inspect::Inspector::default();
            let rootdir = FunctionFsRootDir::new(inspector.root());

            let (proxy, stream) =
                fidl::endpoints::create_proxy_and_stream::<fadb::UsbAdbImpl_Marker>();
            let (read_sender, read_receiver) = async_channel::unbounded();
            let (write_sender, write_receiver) = async_channel::unbounded();

            // Simulate state established by create_endpoints() for session 1.
            {
                let mut state = rootdir.state.lock();
                state.session_id = 1;
                state.has_input_output_endpoints = true;
                state.adb_read_channel = Some(read_sender);
                state.adb_write_channel = Some(write_sender);
                state.event_queue.push_back(usb_functionfs_event {
                    type_: usb_functionfs_event_type_FUNCTIONFS_BIND as u8,
                    ..Default::default()
                });
                state.record_event("BIND");
            }

            // Drop the driver stream to simulate USB driver unbind (e.g. role switch to Host mode).
            drop(stream);

            handle_adb(
                proxy,
                None,
                read_receiver,
                write_receiver,
                Arc::clone(&rootdir.state),
                Arc::clone(&rootdir.stats),
                1,
            )
            .await;

            let state = rootdir.state.lock();
            assert!(!state.is_online);
            assert!(
                !state.has_input_output_endpoints,
                "UNBIND must clear has_input_output_endpoints so wait_until_online aborts"
            );
            assert!(
                state.adb_read_channel.is_none(),
                "UNBIND must clear adb_read_channel so ep1 reads do not block on dead session"
            );
            assert!(
                state.adb_write_channel.is_none(),
                "UNBIND must clear adb_write_channel so ep2 writes do not block on dead session"
            );
            let events: Vec<u8> = state.event_queue.iter().map(|e| e.type_).collect();
            assert_eq!(
                events,
                vec![
                    usb_functionfs_event_type_FUNCTIONFS_BIND as u8,
                    usb_functionfs_event_type_FUNCTIONFS_UNBIND as u8,
                ],
                "Exactly one BIND and one UNBIND event should be queued"
            );
        });

        while exec.run_until_stalled(&mut test_fut).is_pending() {
            assert!(exec.wake_next_timer().is_some(), "Executor stalled with no pending timers");
        }
    }

    #[fuchsia::test]
    async fn test_create_endpoints_without_hardware_service() {
        starnix_core::testing::spawn_kernel_and_run_sync(|current_task| {
            let inspector = inspect::Inspector::default();
            let rootdir = FunctionFsRootDir::new(inspector.root());

            let result = rootdir.create_endpoints(current_task.kernel());
            assert_eq!(result, Ok(()));
            assert!(rootdir.state.lock().has_input_output_endpoints);
            assert!(rootdir.state.lock().adb_read_channel.is_some());
            assert!(rootdir.state.lock().adb_write_channel.is_some());
            assert_eq!(
                rootdir.state.lock().event_queue.front().map(|e| e.type_),
                Some(usb_functionfs_event_type_FUNCTIONFS_BIND as u8)
            );
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_connect_to_device_without_hardware_service() {
        let result = connect_to_device(AdbProxyMode::None).await;
        assert!(result.is_err());
    }

    #[fuchsia::test]
    async fn test_on_control_closed_clears_device_proxy() {
        let inspector = inspect::Inspector::default();
        let rootdir = FunctionFsRootDir::new(inspector.root());

        let (client_channel, _server_channel) = zx::Channel::create();
        let device_proxy = fadb::DeviceSynchronousProxy::new(client_channel);
        {
            let mut state = rootdir.state.lock();
            state.num_control_file_objects = 1;
            state.device_proxy = Some(device_proxy);
        }

        rootdir.on_control_closed();

        let state = rootdir.state.lock();
        assert_eq!(state.num_control_file_objects, 0);
        assert!(state.device_proxy.is_none(), "device_proxy should be taken by on_control_closed");
    }

    #[fuchsia::test]
    async fn test_session_change_stops_device_proxy() {
        let inspector = inspect::Inspector::default();
        let rootdir = FunctionFsRootDir::new(inspector.root());

        let (client_channel, server_channel) = zx::Channel::create();
        let device_proxy = fadb::DeviceSynchronousProxy::new(client_channel);

        // Simulate session_id changed while connecting
        let session_id = 1;
        {
            let mut state = rootdir.state.lock();
            state.session_id = 2; // session_id changed!
            state.has_input_output_endpoints = true;
        }

        // When checking session_id after connect_to_device:
        {
            let mut state_locked = rootdir.state.lock();
            if state_locked.session_id != session_id || !state_locked.has_input_output_endpoints {
                let deadline = zx::MonotonicInstant::after(zx::Duration::from_seconds(1));
                let _ = device_proxy.stop_adb(deadline);
            } else {
                state_locked.device_proxy = Some(device_proxy);
            }
        }

        // Verify stop_adb request was sent on server_channel
        let mut stream = fadb::DeviceRequestStream::from_channel(fidl::AsyncChannel::from_channel(
            server_channel,
        ));
        match stream.next().await {
            Some(Ok(fadb::DeviceRequest::StopAdb { responder })) => {
                responder.send(Ok(())).expect("send StopAdb response");
            }
            other => panic!("Expected StopAdb request, got {other:?}"),
        }
        assert!(rootdir.state.lock().device_proxy.is_none());
    }
}
