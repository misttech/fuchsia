// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::power::{SuspendState, SuspendStats};
use crate::task::CurrentTask;
use crate::vfs::EpollKey;

use std::cmp::min;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Weak};

use anyhow::{Context, anyhow};
use fidl::endpoints::Proxy;
use fuchsia_component::client::connect_to_protocol_sync;
use fuchsia_inspect::{ArrayProperty, StringArrayProperty};
use fuchsia_inspect_contrib::nodes::BoundedListNode;
use futures::stream::{FusedStream, Next};
use futures::{FutureExt, StreamExt};
use itertools::Itertools;
use starnix_logging::{log_info, log_warn};
use starnix_sync::{
    EbpfSuspendLock, FileOpsCore, LockBefore, Locked, Mutex, MutexGuard, OrderedRwLock,
    RwLockReadGuard,
};
use starnix_task_command::TaskCommand;
use starnix_uapi::arc_key::WeakKey;
use starnix_uapi::errors::Errno;
use starnix_uapi::{errno, error};
use std::fmt;
use zx::{HandleBased, Peered};
use {
    fidl_fuchsia_power_observability as fobs, fidl_fuchsia_session_power as fpower,
    fidl_fuchsia_starnix_runner as frunner, fuchsia_inspect as inspect,
};

/// Manager for suspend and resume.
pub struct SuspendResumeManager {
    // The mutable state of [SuspendResumeManager].
    inner: Mutex<SuspendResumeManagerInner>,

    /// The currently registered message counters in the system whose values are exposed to inspect
    /// via a lazy node.
    message_counters: Arc<Mutex<HashSet<WeakKey<OwnedMessageCounter>>>>,

    // The lock used to to avoid suspension while holding eBPF locks.
    ebpf_suspend_lock: OrderedRwLock<(), EbpfSuspendLock>,
}

/// Manager for suspend and resume.
pub struct SuspendResumeManagerInner {
    /// The suspend counters and gauges.
    suspend_stats: SuspendStats,
    sync_on_suspend_enabled: bool,

    suspend_events_node: BoundedListNode,
    wake_locks_inspect: WakeLocksInspect,

    /// The currently active wake locks in the system. If non-empty, this prevents
    /// the container from suspending.
    active_locks: HashMap<String, LockSource>,
    inactive_locks: HashSet<String>,

    /// The currently active EPOLLWAKEUPs in the system. If non-empty, this prevents
    /// the container from suspending.
    active_epolls: HashMap<EpollKey, TaskCommand>,
    inactive_epolls: HashSet<TaskCommand>,

    /// The event pair that is passed to the Starnix runner so it can observe whether
    /// or not any wake locks are active before completing a suspend operation.
    active_lock_reader: zx::EventPair,

    /// The event pair that is used by the Starnix kernel to signal when there are
    /// active wake locks in the container. Note that the peer of the writer is the
    /// object that is signaled.
    active_lock_writer: zx::EventPair,
}

pub type EbpfSuspendGuard<'a> = RwLockReadGuard<'a, ()>;

/// State associated with logging wake lock information in Inspect.
struct WakeLocksInspect {
    /// List of active locks.
    active_wake_locks: StringArrayProperty,

    /// List of inactive locks.
    inactive_wake_locks: StringArrayProperty,

    /// List of active epolls
    active_epolls: StringArrayProperty,

    /// List of inactive epolls
    inactive_epolls: StringArrayProperty,

    /// Parent node of the above properties.
    root: inspect::Node,
}

/// The source of a wake lock.
pub enum LockSource {
    WakeLockFile,
    ContainerPowerController,
}

/// The inspect node ring buffer will keep at most this many entries.
const INSPECT_RING_BUFFER_CAPACITY: usize = 128;

/// The inspect wakelock nodes will keep at most this many entries.
const INSPECT_MAX_WAKE_LOCK_NAMES: usize = 64;
const INSPECT_MAX_EPOLLS: usize = 64;

impl Default for SuspendResumeManagerInner {
    fn default() -> Self {
        let (active_lock_reader, active_lock_writer) = zx::EventPair::create();
        let root = inspect::component::inspector().root();
        Self {
            suspend_events_node: BoundedListNode::new(
                root.create_child("suspend_events"),
                INSPECT_RING_BUFFER_CAPACITY,
            ),
            wake_locks_inspect: WakeLocksInspect::new(&root),
            suspend_stats: Default::default(),
            sync_on_suspend_enabled: Default::default(),
            active_locks: Default::default(),
            inactive_locks: Default::default(),
            active_epolls: Default::default(),
            inactive_epolls: Default::default(),
            active_lock_reader,
            active_lock_writer,
        }
    }
}

impl SuspendResumeManagerInner {
    pub fn active_wake_locks(&self) -> Vec<String> {
        Vec::from_iter(self.active_locks.keys().cloned())
    }

    pub fn inactive_wake_locks(&self) -> Vec<String> {
        Vec::from_iter(self.inactive_locks.clone())
    }

    fn active_epolls(&self) -> Vec<TaskCommand> {
        Vec::from_iter(self.active_epolls.values().cloned())
    }

    fn update_suspend_stats<UpdateFn>(&mut self, update: UpdateFn)
    where
        UpdateFn: FnOnce(&mut SuspendStats),
    {
        update(&mut self.suspend_stats);
    }

    /// Signals whether or not there are currently any active wake locks in the kernel.
    fn signal_wake_events(&mut self) {
        let (clear_mask, set_mask) =
            if self.active_locks.is_empty() && self.active_epolls.is_empty() {
                (zx::Signals::EVENT_SIGNALED, zx::Signals::empty())
            } else {
                (zx::Signals::empty(), zx::Signals::EVENT_SIGNALED)
            };
        self.active_lock_writer.signal_peer(clear_mask, set_mask).expect("Failed to signal peer");
    }

    /// Records the first INSPECT_MAX_WAKE_LOCK_NAMES active wake locks, sorted lexicographically.
    fn record_active_locks(&mut self) {
        let inspect = &mut self.wake_locks_inspect;
        let active_locks = &self.active_locks;

        let len = min(active_locks.len(), INSPECT_MAX_WAKE_LOCK_NAMES);
        inspect.active_wake_locks =
            inspect.root.create_string_array(fobs::ACTIVE_WAKE_LOCK_NAMES, len);
        for (i, name) in active_locks.keys().sorted().take(len).enumerate() {
            if let Some(src) = active_locks.get(name) {
                inspect.active_wake_locks.set(i, format!("{} (source {})", name, src));
            }
        }
    }

    /// Records the first INSPECT_MAX_WAKE_LOCK_NAMES inactive wake locks, sorted lexicographically.
    fn record_inactive_locks(&mut self) {
        let inspect = &mut self.wake_locks_inspect;
        let inactive_locks = &self.inactive_locks;

        let len = min(self.inactive_locks.len(), INSPECT_MAX_WAKE_LOCK_NAMES);
        inspect.inactive_wake_locks =
            inspect.root.create_string_array(fobs::INACTIVE_WAKE_LOCK_NAMES, len);
        for (i, name) in inactive_locks.iter().sorted().take(len).enumerate() {
            inspect.inactive_wake_locks.set(i, name);
        }
    }

    fn record_active_epolls(&mut self) {
        let inspect = &mut self.wake_locks_inspect;
        let active_epolls = &self.active_epolls;

        let len = min(active_epolls.len(), INSPECT_MAX_EPOLLS);
        inspect.active_epolls = inspect.root.create_string_array(fobs::ACTIVE_EPOLLS, len);
        for (i, key) in active_epolls.keys().sorted().rev().take(len).enumerate() {
            if let Some(name) = active_epolls.get(key) {
                inspect.active_epolls.set(i, name.to_string());
            }
        }
    }

    fn record_inactive_epolls(&mut self) {
        let inspect = &mut self.wake_locks_inspect;
        let inactive_epolls = &self.inactive_epolls;

        let len = min(inactive_epolls.len(), INSPECT_MAX_WAKE_LOCK_NAMES);
        inspect.inactive_epolls = inspect.root.create_string_array(fobs::INACTIVE_EPOLLS, len);
        for (i, name) in inactive_epolls.iter().sorted().take(len).enumerate() {
            inspect.inactive_epolls.set(i, name.to_string());
        }
    }
}

pub type SuspendResumeManagerHandle = Arc<SuspendResumeManager>;

impl Default for SuspendResumeManager {
    fn default() -> Self {
        let message_counters: Arc<Mutex<HashSet<WeakKey<OwnedMessageCounter>>>> =
            Default::default();
        let message_counters_clone = message_counters.clone();
        let root = inspect::component::inspector().root();
        root.record_lazy_values("message_counters", move || {
            let message_counters_clone = message_counters_clone.clone();
            async move {
                let inspector = fuchsia_inspect::Inspector::default();
                let root = inspector.root();
                let mut message_counters = message_counters_clone.lock();
                message_counters.retain(|c| c.0.upgrade().is_some());
                let message_counters_inspect =
                    root.create_string_array("message_counters", message_counters.len());
                for (i, c) in message_counters.iter().enumerate() {
                    let counter = c.0.upgrade().expect("lost counter should be retained");
                    message_counters_inspect.set(i, counter.to_string());
                }
                root.record(message_counters_inspect);
                Ok(inspector)
            }
            .boxed()
        });
        Self { message_counters, inner: Default::default(), ebpf_suspend_lock: Default::default() }
    }
}

impl SuspendResumeManager {
    /// Locks and returns the inner state of the manager.
    pub fn lock(&self) -> MutexGuard<'_, SuspendResumeManagerInner> {
        self.inner.lock()
    }

    /// Power on the PowerMode element and start listening to the suspend stats updates.
    pub fn init(
        self: &SuspendResumeManagerHandle,
        system_task: &CurrentTask,
    ) -> Result<(), anyhow::Error> {
        let handoff = system_task
            .kernel()
            .connect_to_protocol_at_container_svc::<fpower::HandoffMarker>()?
            .into_sync_proxy();
        match handoff.take(zx::MonotonicInstant::INFINITE) {
            Ok(parent_lease) => {
                let parent_lease = parent_lease
                    .map_err(|e| anyhow!("Failed to take lessor and lease from parent: {e:?}"))?;
                drop(parent_lease)
            }
            Err(e) => {
                if e.is_closed() {
                    log_warn!(
                        "Failed to send the fuchsia.session.power/Handoff.Take request. Assuming no Handoff protocol exists and moving on..."
                    );
                } else {
                    return Err(e).context("Handoff::Take");
                }
            }
        }
        Ok(())
    }

    /// Adds a wake lock `name` to the active wake locks.
    pub fn add_lock(&self, name: &str, src: LockSource) -> bool {
        let mut state = self.lock();
        let res = state.active_locks.insert(String::from(name), src);
        state.signal_wake_events();
        state.record_active_locks();
        res.is_none()
    }

    /// Removes a wake lock `name` from the active wake locks.
    pub fn remove_lock(&self, name: &str) -> bool {
        let mut state = self.lock();
        let res = state.active_locks.remove(name);
        if res.is_none() {
            return false;
        }

        state.inactive_locks.insert(String::from(name));
        state.signal_wake_events();
        state.record_active_locks();
        state.record_inactive_locks();
        true
    }

    /// Adds a wake lock `key` to the active epoll wake locks.
    pub fn add_epoll(&self, current_task: &CurrentTask, key: EpollKey) {
        let mut state = self.lock();
        state.active_epolls.insert(key, current_task.command());
        state.signal_wake_events();
        state.record_active_epolls();
    }

    /// Removes a wake lock `key` from the active epoll wake locks.
    pub fn remove_epoll(&self, key: EpollKey) {
        let mut state = self.lock();
        let epoll = state.active_epolls.remove(&key);
        if let Some(epoll) = epoll {
            state.inactive_epolls.insert(epoll);
        }
        state.signal_wake_events();
        state.record_active_epolls();
        state.record_inactive_epolls();
    }

    pub fn add_message_counter(
        &self,
        name: &str,
        counter: Option<zx::Counter>,
    ) -> OwnedMessageCounterHandle {
        let container_counter = OwnedMessageCounter::new(name, counter);
        let mut message_counters = self.message_counters.lock();
        message_counters.insert(WeakKey::from(&container_counter));
        message_counters.retain(|c| c.0.upgrade().is_some());
        container_counter
    }

    /// Returns a duplicate handle to the `EventPair` that is signaled when wake
    /// locks are active.
    pub fn duplicate_lock_event(&self) -> zx::EventPair {
        let state = self.lock();
        state
            .active_lock_reader
            .duplicate_handle(zx::Rights::SAME_RIGHTS)
            .expect("Failed to duplicate handle")
    }

    /// Gets the suspend statistics.
    pub fn suspend_stats(&self) -> SuspendStats {
        self.lock().suspend_stats.clone()
    }

    /// Get the contents of the power "sync_on_suspend" file in the power
    /// filesystem.  True will cause `1` to be reported, and false will cause
    /// `0` to be reported.
    pub fn sync_on_suspend_enabled(&self) -> bool {
        self.lock().sync_on_suspend_enabled.clone()
    }

    /// Get the contents of the power "sync_on_suspend" file in the power
    /// filesystem.  See also [sync_on_suspend_enabled].
    pub fn set_sync_on_suspend(&self, enable: bool) {
        self.lock().sync_on_suspend_enabled = enable;
    }

    /// Returns the supported suspend states.
    pub fn suspend_states(&self) -> HashSet<SuspendState> {
        // TODO(b/326470421): Remove the hardcoded supported state.
        HashSet::from([SuspendState::Idle])
    }

    pub fn suspend(
        &self,
        locked: &mut Locked<FileOpsCore>,
        state: SuspendState,
    ) -> Result<(), Errno> {
        let suspend_start_time = zx::BootInstant::get();

        self.lock().suspend_events_node.add_entry(|node| {
            node.record_int(fobs::SUSPEND_ATTEMPTED_AT, suspend_start_time.clone().into_nanos());
            node.record_string(fobs::SUSPEND_REQUESTED_STATE, state.to_string());
        });

        let ebpf_lock = self.ebpf_suspend_lock.write(locked);

        let manager = connect_to_protocol_sync::<frunner::ManagerMarker>()
            .expect("Failed to connect to manager");
        fuchsia_trace::duration!(c"power", c"suspend_container:fidl");
        log_info!("Asking runner to suspend container.");
        match manager.suspend_container(
            frunner::ManagerSuspendContainerRequest {
                container_job: Some(
                    fuchsia_runtime::job_default()
                        .duplicate(zx::Rights::SAME_RIGHTS)
                        .expect("Failed to dup handle"),
                ),
                wake_locks: Some(self.duplicate_lock_event()),
                ..Default::default()
            },
            zx::Instant::INFINITE,
        ) {
            Ok(Ok(res)) => {
                log_info!("Resuming from container suspension.");
                let wake_time = zx::BootInstant::get();
                let resume_reason = res.resume_reason;
                let mut state = self.lock();
                state.update_suspend_stats(|suspend_stats| {
                    suspend_stats.success_count += 1;
                    suspend_stats.last_time_in_suspend_operations =
                        (wake_time - suspend_start_time).into();
                    suspend_stats.last_time_in_sleep =
                        zx::BootDuration::from_nanos(res.suspend_time.unwrap_or(0));
                    // The "0" here is to mimic the expected power management success string,
                    // while we don't have IRQ numbers to report.
                    suspend_stats.last_resume_reason =
                        resume_reason.clone().map(|s| format!("0 {}", s));
                });
                state.suspend_events_node.add_entry(|node| {
                    node.record_int(fobs::SUSPEND_RESUMED_AT, wake_time.into_nanos());
                    node.record_string(
                        fobs::SUSPEND_RESUME_REASON,
                        resume_reason.unwrap_or_default(),
                    );
                });
                fuchsia_trace::instant!(
                    c"power",
                    c"suspend_container:done",
                    fuchsia_trace::Scope::Process
                );
            }
            e => {
                let wake_time = zx::BootInstant::get();
                let mut state = self.lock();
                state.update_suspend_stats(|suspend_stats| {
                    suspend_stats.fail_count += 1;
                    suspend_stats.last_failed_errno = Some(errno!(EINVAL));
                    suspend_stats.last_resume_reason = None;
                });

                let (wake_lock_names, epoll_names) = match e {
                    Ok(Err(frunner::SuspendError::WakeLocksExist)) => {
                        let wake_lock_names = state.active_wake_locks();
                        let epoll_names = state.active_epolls();
                        let last_resume_reason = format!(
                            "Abort: {}",
                            wake_lock_names.join(" ") + &epoll_names.iter().join(" ")
                        );
                        state.update_suspend_stats(|suspend_stats| {
                            // Power analysis tools require `Abort: ` in the case of failed suspends
                            suspend_stats.last_resume_reason = Some(last_resume_reason);
                        });
                        (Some(wake_lock_names), Some(epoll_names))
                    }
                    _ => (None, None),
                };

                log_warn!(e:?; "Container suspension failed. wake locks: {:?}, epolls: {:?}", wake_lock_names, epoll_names);
                state.suspend_events_node.add_entry(|node| {
                    node.record_int(fobs::SUSPEND_FAILED_AT, wake_time.into_nanos());
                    if let Some(names) = wake_lock_names {
                        let names_array =
                            node.create_string_array(fobs::ACTIVE_WAKE_LOCK_NAMES, names.len());
                        for (i, name) in names.iter().enumerate() {
                            names_array.set(i, name);
                        }
                        node.record(names_array);
                    }
                    if let Some(epolls) = epoll_names {
                        let epolls_array =
                            node.create_string_array(fobs::ACTIVE_EPOLLS, epolls.len());
                        for (i, name) in epolls.iter().enumerate() {
                            epolls_array.set(i, name.to_string());
                        }
                        node.record(epolls_array);
                    }
                });
                fuchsia_trace::instant!(
                    c"power",
                    c"suspend_container:error",
                    fuchsia_trace::Scope::Process
                );
                return error!(EINVAL);
            }
        }

        std::mem::drop(ebpf_lock);

        Ok(())
    }

    pub fn acquire_ebpf_suspend_lock<'a, L>(
        &'a self,
        locked: &'a mut Locked<L>,
    ) -> EbpfSuspendGuard<'a>
    where
        L: LockBefore<EbpfSuspendLock>,
    {
        self.ebpf_suspend_lock.read(locked)
    }
}

impl WakeLocksInspect {
    fn new(parent: &inspect::Node) -> Self {
        let root = parent.create_child("wake_locks");
        Self {
            active_wake_locks: root.create_string_array(fobs::ACTIVE_WAKE_LOCK_NAMES, 0),
            inactive_wake_locks: root.create_string_array(fobs::INACTIVE_WAKE_LOCK_NAMES, 0),
            active_epolls: root.create_string_array(fobs::ACTIVE_EPOLLS, 0),
            inactive_epolls: root.create_string_array(fobs::INACTIVE_EPOLLS, 0),
            root,
        }
    }
}

impl fmt::Display for LockSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LockSource::WakeLockFile => write!(f, "wake lock file"),
            LockSource::ContainerPowerController => write!(f, "container power controller"),
        }
    }
}

pub trait OnWakeOps: Send + Sync {
    fn on_wake(&self, current_task: &CurrentTask, baton_lease: &zx::Handle);
}

/// Creates a proxy between `remote_channel` and the returned `zx::Channel`.
///
/// The message counter's initial value will be set to 0.
///
/// The returned counter will be incremented each time there is an incoming message on the proxied
/// channel. The starnix_kernel is expected to decrement the counter when that incoming message is
/// handled.
///
/// Note that "message" in this context means channel message. This can be either a FIDL event, or
/// a response to a FIDL message from the platform.
///
/// For example, the starnix_kernel may issue a hanging get to retrieve input events. When that
/// hanging get returns, the counter will be incremented by 1. When the next hanging get has been
/// scheduled, the input subsystem decrements the counter by 1.
///
/// The proxying is done by the Starnix runner, and allows messages on the channel to wake
/// the container.
pub fn create_proxy_for_wake_events_counter_zero(
    remote_channel: zx::Channel,
    name: String,
) -> (zx::Channel, zx::Counter) {
    let (local_proxy, kernel_channel) = zx::Channel::create();
    let counter = zx::Counter::create();

    let local_counter =
        counter.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("Failed to duplicate counter");

    let manager = fuchsia_component::client::connect_to_protocol_sync::<frunner::ManagerMarker>()
        .expect("failed");
    manager
        .proxy_wake_channel(frunner::ManagerProxyWakeChannelRequest {
            container_job: Some(
                fuchsia_runtime::job_default()
                    .duplicate(zx::Rights::SAME_RIGHTS)
                    .expect("Failed to dup handle"),
            ),
            container_channel: Some(kernel_channel),
            remote_channel: Some(remote_channel),
            counter: Some(counter),
            name: Some(name),
            ..Default::default()
        })
        .expect("Failed to create proxy");

    (local_proxy, local_counter)
}

/// Creates a proxy between `remote_channel` and the returned `zx::Channel`.
///
/// The message counter's initial value will be set to 1, which will prevent the container from
/// suspending until the caller decrements the counter.
///
/// The returned counter will be incremented each time there is an incoming message on the proxied
/// channel. The starnix_kernel is expected to decrement the counter when that incoming message is
/// handled.
///
/// Note that "message" in this context means channel message. This can be either a FIDL event, or
/// a response to a FIDL message from the platform.
///
/// For example, the starnix_kernel may issue a hanging get to retrieve input events. When that
/// hanging get returns, the counter will be incremented by 1. When the next hanging get has been
/// scheduled, the input subsystem decrements the counter by 1.
///
/// The proxying is done by the Starnix runner, and allows messages on the channel to wake
/// the container.
pub fn create_proxy_for_wake_events_counter(
    remote_channel: zx::Channel,
    name: String,
) -> (zx::Channel, zx::Counter) {
    let (proxy, counter) = create_proxy_for_wake_events_counter_zero(remote_channel, name);

    // Increment the counter by one so that the initial incoming message to the container will
    // set the count to 0, instead of -1.
    counter.add(1).expect("Failed to add to counter");

    (proxy, counter)
}

/// Marks a message handled by decrementing `counter`.
///
/// This should be called when a proxied channel message has been handled, and the caller would
/// be ok letting the container suspend.
pub fn mark_proxy_message_handled(counter: &zx::Counter) {
    counter.add(-1).expect("Failed to decrement counter");
}

/// Marks all messages tracked by `counter` as handled.
pub fn mark_all_proxy_messages_handled(counter: &zx::Counter) {
    counter.write(0).expect("Failed to decrement counter");
}

/// Creates a watcher between clients and the Starnix runner.
///
/// Changes in the power state of the container are relayed by the event pair.
pub fn create_watcher_for_wake_events(watcher: zx::EventPair) {
    let manager = fuchsia_component::client::connect_to_protocol_sync::<frunner::ManagerMarker>()
        .expect("failed");
    manager
        .register_wake_watcher(
            frunner::ManagerRegisterWakeWatcherRequest {
                watcher: Some(watcher),
                ..Default::default()
            },
            zx::Instant::INFINITE,
        )
        .expect("Failed to register wake watcher");
}

/// Wrapper around a Weak `OwnedMessageCounter` that can be passed around to keep the container
/// awake.
///
/// Each live `SharedMessageCounter` is responsible for a pending message while it in scope,
/// and removes it from the counter when it goes out of scope.  Processes that need to cooperate
/// can pass a `SharedMessageCounter` to each other to ensure that once the work is done, the lock
/// goes out of scope as well. This allows for precise accounting of remaining work, and should
/// give us control over container suspension which is guarded by the compiler, not conventions.
#[derive(Debug)]
pub struct SharedMessageCounter(Weak<OwnedMessageCounter>);

impl Drop for SharedMessageCounter {
    fn drop(&mut self) {
        if let Some(message_counter) = self.0.upgrade() {
            message_counter.mark_handled();
        }
    }
}

/// Owns a `zx::Counter` to track pending messages that prevent the container from suspending.
///
/// This struct ensures that the counter is reset to 0 when the last strong reference is dropped,
/// effectively releasing any wake lock held by this counter.
pub struct OwnedMessageCounter {
    name: String,
    counter: Option<zx::Counter>,
}
pub type OwnedMessageCounterHandle = Arc<OwnedMessageCounter>;

impl Drop for OwnedMessageCounter {
    /// Resets the underlying `zx::Counter` to 0 when the `OwnedMessageCounter` is dropped.
    ///
    /// This ensures that all pending messages are marked as handled, allowing the system to suspend
    /// if no other wake locks are held.
    fn drop(&mut self) {
        self.counter.as_ref().map(mark_all_proxy_messages_handled);
    }
}

impl OwnedMessageCounter {
    pub fn new(name: &str, counter: Option<zx::Counter>) -> OwnedMessageCounterHandle {
        Arc::new(Self { name: name.to_string(), counter })
    }

    /// Decrements the counter, signaling that a pending message or operation has been handled.
    ///
    /// This should be called when the work associated with a previous `mark_pending` call is
    /// complete.
    pub fn mark_handled(&self) {
        self.counter.as_ref().map(mark_proxy_message_handled);
    }

    /// Increments the counter, signaling that a new message or operation is pending.
    ///
    /// This prevents the system from suspending until a corresponding `mark_handled` call is made.
    pub fn mark_pending(&self) {
        self.counter.as_ref().map(|c| c.add(1).expect("Failed to increment counter"));
    }

    /// Creates a `SharedMessageCounter` from this `OwnedMessageCounter`.
    ///
    /// `new_pending_message` - if a new pending message should be added
    pub fn share(
        self: &OwnedMessageCounterHandle,
        new_pending_message: bool,
    ) -> SharedMessageCounter {
        if new_pending_message {
            self.mark_pending();
        }
        SharedMessageCounter(Arc::downgrade(self))
    }
}

impl fmt::Display for OwnedMessageCounter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Counter({}): {:?}", self.name, self.counter.as_ref().map(|c| c.read()))
    }
}

/// A proxy wrapper that manages a `zx::Counter` to allow the container to suspend
/// after events are being processed.
///
/// When the proxy is dropped, the counter is reset to 0 to release the wake-lock.
pub struct ContainerWakingProxy<P: Proxy> {
    counter: OwnedMessageCounterHandle,
    proxy: P,
}

impl<P: Proxy> ContainerWakingProxy<P> {
    pub fn new(counter: OwnedMessageCounterHandle, proxy: P) -> Self {
        Self { counter, proxy }
    }

    /// Create a `Future` call on the proxy.
    ///
    /// The counter will be decremented as message handled after the future is created.
    pub fn call<T, F, R>(&self, future: F) -> R
    where
        F: FnOnce(&P) -> R,
        R: Future<Output = T>,
    {
        // The sequence for handling events MUST be:
        //
        // 1. create future
        // 2. decrease counter
        // 3. await future
        //
        // for allowing suspend - wake.
        let f = future(&self.proxy);
        self.counter.mark_handled();
        f
    }
}

/// A stream wrapper that manages a `zx::Counter` to allow the container to suspend
/// after events are being processed.
///
/// When the stream is dropped, the counter is reset to 0 to release the wake-lock.
pub struct ContainerWakingStream<S: FusedStream + Unpin> {
    counter: OwnedMessageCounterHandle,
    stream: S,
}

impl<S: FusedStream + Unpin> ContainerWakingStream<S> {
    pub fn new(counter: OwnedMessageCounterHandle, stream: S) -> Self {
        Self { counter, stream }
    }

    /// Create a `Next` call on the stream.poll_next().
    ///
    /// The counter will be decremented as message handled after the future is created.
    pub fn next(&mut self) -> Next<'_, S> {
        // See `ContainerWakingProxy::call` for sequence of handling events.
        let next = self.stream.next();
        self.counter.mark_handled();
        next
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use diagnostics_assertions::assert_data_tree;
    use fidl::endpoints::create_proxy_and_stream;
    use fidl_test_placeholders::{EchoMarker, EchoRequest};
    use futures::StreamExt;
    use zx::{self, HandleBased};
    use {fuchsia_async as fasync, fuchsia_inspect as inspect};

    #[::fuchsia::test]
    fn test_counter_zero_initialization() {
        let (_endpoint, endpoint) = zx::Channel::create();
        let (_channel, counter) =
            super::create_proxy_for_wake_events_counter_zero(endpoint, "test".into());
        assert_eq!(counter.read(), Ok(0));
    }

    #[::fuchsia::test]
    fn test_counter_initialization() {
        let (_endpoint, endpoint) = zx::Channel::create();
        let (_channel, counter) =
            super::create_proxy_for_wake_events_counter(endpoint, "test".into());
        assert_eq!(counter.read(), Ok(1));
    }

    #[::fuchsia::test]
    async fn test_container_waking_proxy() {
        let (proxy, mut stream) = create_proxy_and_stream::<EchoMarker>();
        let server_task = fasync::Task::spawn(async move {
            let request = stream.next().await.unwrap().unwrap();
            match request {
                EchoRequest::EchoString { value, responder } => {
                    responder.send(value.as_deref()).unwrap();
                }
            }
        });

        let counter = zx::Counter::create();
        counter.add(5).unwrap();
        assert_eq!(counter.read(), Ok(5));

        let waking_proxy = ContainerWakingProxy {
            counter: OwnedMessageCounter::new(
                "test_proxy",
                Some(counter.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap()),
            ),
            proxy,
        };

        let response_future = waking_proxy.call(|p| p.echo_string(Some("hello")));

        // The `call` method decrements the counter.
        assert_eq!(counter.read(), Ok(4));

        let response = response_future.await.unwrap();
        assert_eq!(response.as_deref(), Some("hello"));

        server_task.await;

        assert_eq!(counter.read(), Ok(4));
        drop(waking_proxy);
        assert_eq!(counter.read(), Ok(0));
    }

    #[::fuchsia::test]
    async fn test_container_waking_stream() {
        let (proxy, stream) = create_proxy_and_stream::<EchoMarker>();
        let client_task = fasync::Task::spawn(async move {
            let response = proxy.echo_string(Some("hello")).await.unwrap();
            assert_eq!(response.as_deref(), Some("hello"));
        });

        let counter = zx::Counter::create();
        counter.add(5).unwrap();
        assert_eq!(counter.read(), Ok(5));

        let mut waking_stream = ContainerWakingStream {
            counter: OwnedMessageCounter::new(
                "test_stream",
                Some(counter.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap()),
            ),
            stream,
        };

        let request_future = waking_stream.next();

        // The `next` method decrements the counter.
        assert_eq!(counter.read(), Ok(4));

        let request = request_future.await.unwrap().unwrap();
        match request {
            EchoRequest::EchoString { value, responder } => {
                assert_eq!(value.as_deref(), Some("hello"));
                responder.send(value.as_deref()).unwrap();
            }
        }

        client_task.await;

        assert_eq!(counter.read(), Ok(4));
        drop(waking_stream);
        assert_eq!(counter.read(), Ok(0));
    }

    #[::fuchsia::test]
    async fn test_message_counters_inspect() {
        let power_manager = SuspendResumeManager::default();
        let inspector = inspect::component::inspector();

        let zx_counter = zx::Counter::create();
        let counter_handle = power_manager.add_message_counter(
            "test_counter",
            Some(zx_counter.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap()),
        );

        zx_counter.add(1).unwrap();

        assert_data_tree!(inspector, root: contains {
            message_counters: vec!["Counter(test_counter): Some(Ok(1))"],
        });

        zx_counter.add(1).unwrap();
        assert_data_tree!(inspector, root: contains {
            message_counters: vec!["Counter(test_counter): Some(Ok(2))"],
        });

        drop(counter_handle);
        assert_data_tree!(inspector, root: contains {
            message_counters: Vec::<String>::new(),
        });
    }

    #[::fuchsia::test]
    fn test_shared_message_counter() {
        // Create an owned counter and set its value.
        let zx_counter = zx::Counter::create();
        let owned_counter = OwnedMessageCounter::new(
            "test_shared_counter",
            Some(zx_counter.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap()),
        );
        zx_counter.add(5).unwrap();
        assert_eq!(zx_counter.read(), Ok(5));

        // Create a shared counter with no new message. The value should be unchanged.
        let shared_counter = owned_counter.share(false);
        assert_eq!(zx_counter.read(), Ok(5));

        // Drop the shared counter. The value should be decremented.
        drop(shared_counter);
        assert_eq!(zx_counter.read(), Ok(4));

        // Create a shared counter with a new message. The value should be incremented.
        let shared_counter_2 = owned_counter.share(true);
        assert_eq!(zx_counter.read(), Ok(5));

        // Drop the shared counter. The value should be decremented.
        drop(shared_counter_2);
        assert_eq!(zx_counter.read(), Ok(4));

        // Create another shared counter.
        let shared_counter_3 = owned_counter.share(false);
        assert_eq!(zx_counter.read(), Ok(4));

        // Drop the owned counter. The value should be reset to 0.
        drop(owned_counter);
        assert_eq!(zx_counter.read(), Ok(0));

        // Drop the shared counter. The value should remain 0, and it shouldn't panic.
        drop(shared_counter_3);
        assert_eq!(zx_counter.read(), Ok(0));
    }
}
