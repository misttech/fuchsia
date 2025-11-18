// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::power::{
    OnWakeOps, OwnedMessageCounterHandle, SharedMessageCounter,
    create_proxy_for_wake_events_counter_zero,
};
use crate::task::{CurrentTask, Kernel, TargetTime};
use crate::vfs::timer::{TimelineChangeObserver, TimerOps};
use anyhow::{Context, Result};
use fuchsia_inspect::ArrayProperty;
use fuchsia_inspect_contrib::nodes::BoundedListNode;
use futures::channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use futures::{FutureExt, SinkExt, StreamExt, select};
use scopeguard::defer;
use starnix_logging::{log_debug, log_error, log_info, log_warn};
use starnix_sync::{Mutex, MutexGuard};
use starnix_uapi::errors::Errno;
use starnix_uapi::{errno, from_status_like_fdio};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, Weak};
use zx::{self as zx, AsHandleRef, HandleBased, HandleRef};
use {fidl_fuchsia_time_alarms as fta, fuchsia_async as fasync, fuchsia_trace as ftrace};

/// Max value for inspect event history.
const INSPECT_GRAPH_EVENT_BUFFER_SIZE: usize = 128;

fn to_errno_with_log<T: std::fmt::Debug>(v: T) -> Errno {
    log_error!("hr_timer_manager internal error: {v:?}");
    from_status_like_fdio!(zx::Status::IO)
}

fn signal_handle<H: HandleBased>(
    handle: &H,
    clear_mask: zx::Signals,
    set_mask: zx::Signals,
) -> Result<(), zx::Status> {
    handle.signal_handle(clear_mask, set_mask).map_err(|err| {
        log_error!("while signaling handle: {err:?}: clear: {clear_mask:?}, set: {set_mask:?}");
        err
    })
}

fn duplicate_handle<H: HandleBased>(h: &H) -> Result<H, Errno> {
    h.duplicate_handle(zx::Rights::SAME_RIGHTS).map_err(|status| from_status_like_fdio!(status))
}

const TIMEOUT_SECONDS: i64 = 40;
//
/// Waits forever synchronously for EVENT_SIGNALED.
///
/// For us there is no useful scenario where this wait times out and we can continue operating.
fn wait_signaled_sync<H: HandleBased>(handle: &H) -> zx::WaitResult {
    let mut logged = false;
    loop {
        let timeout =
            zx::MonotonicInstant::after(zx::MonotonicDuration::from_seconds(TIMEOUT_SECONDS));
        let result = handle.wait_handle(zx::Signals::EVENT_SIGNALED, timeout);
        if let zx::WaitResult::Ok(_) = result {
            if logged {
                log_error!(
                    "wait_signaled_sync: signal resolved. See HrTimer bug: b/428223204: result={result:?}",
                );
            }
            return result;
        }
        fuchsia_trace::instant!(
            c"alarms",
            c"starnix:hrtimer:wait_timeout",
            fuchsia_trace::Scope::Process
        );
        // This is bad and should never happen. If it does, it's a bug that has to be found and
        // fixed. There is no good way to proceed if these signals are not being signaled properly.
        log_error!(
            // LINT.IfChange(hrtimer_wait_signaled_sync_tefmo)
            "wait_signaled_sync: not signaled yet. See HrTimer bug: b/428223204: result={result:?}",
            // LINT.ThenChange(//tools/testing/tefmocheck/string_in_log_check.go:hrtimer_wait_signaled_sync_tefmo)
        );
        if !logged {
            #[cfg(all(target_os = "fuchsia", not(doc)))]
            ::debug::backtrace_request_all_threads();
            logged = true;
        }
    }
}

/// A macro that waits on a future, but if the future takes longer than
/// `TIMEOUT_SECONDS`, we log a warning and a stack trace.
macro_rules! log_long_op {
    ($fut:expr) => {{
        use futures::FutureExt;
        let fut = $fut;
        futures::pin_mut!(fut);
        let mut logged = false;
        loop {
            let timeout = fasync::Timer::new(zx::MonotonicDuration::from_seconds(TIMEOUT_SECONDS));
            futures::select! {
                res = fut.as_mut().fuse() => {
                    if logged {
                        log_warn!("unexpected blocking is now resolved: long-running async operation at {}:{}. See HrTimer bug: b/428223204", file!(), line!());
                    }
                    break res;
                }
                _ = timeout.fuse() => {
                    log_warn!("unexpected blocking: long-running async operation at {}:{}. See HrTimer bug: b/428223204",
                        file!(), line!());
                    if !logged {
                        #[cfg(all(target_os = "fuchsia", not(doc)))]
                        ::debug::backtrace_request_all_threads();
                    }
                    logged = true;
                }
            }
        }
    }};
}

/// Waits forever asynchronously for EVENT_SIGNALED.
async fn wait_signaled<H: HandleBased>(handle: &H) -> Result<()> {
    log_long_op!(fasync::OnSignals::new(handle, zx::Signals::EVENT_SIGNALED))
        .context("hr_timer_manager:wait_signaled")?;
    Ok(())
}

/// Cancels an alarm by ID.
async fn cancel_by_id(
    _message_counter: &SharedMessageCounter,
    timer_state: Option<TimerState>,
    timer_id: &zx::Koid,
    proxy: &fta::WakeAlarmsProxy,
    interval_timers_pending_reschedule: &mut HashMap<zx::Koid, SharedMessageCounter>,
    task_by_timer_id: &mut HashMap<zx::Koid, fasync::Task<()>>,
    alarm_id: &str,
) {
    if let Some(task) = task_by_timer_id.remove(timer_id) {
        // Let this task complete and get removed.
        task.detach();
    }
    if let Some(timer_state) = timer_state {
        ftrace::duration!(c"alarms", c"starnix:hrtimer:cancel_by_id", "timer_id" => *timer_id);
        log_debug!("cancel_by_id: START canceling timer: {:?}: alarm_id: {}", timer_id, alarm_id);
        proxy.cancel(&alarm_id).expect("infallible");
        log_debug!("cancel_by_id: 1/2 canceling timer: {:?}: alarm_id: {}", timer_id, alarm_id);

        // Let the timer closure complete before continuing.
        let _ = log_long_op!(timer_state.task);

        // If this timer is an interval timer, we must remove it from the pending reschedule list.
        // This does not affect container suspend, since `_message_counter` is live. It's a no-op
        // for other timers.
        interval_timers_pending_reschedule.remove(timer_id);
        log_debug!("cancel_by_id: 2/2 DONE canceling timer: {timer_id:?}: alarm_id: {alarm_id}");
    }
}

/// Called when the underlying wake alarms manager reports a fta::WakeAlarmsError
/// as a result of a call to set_and_wait.
fn process_alarm_protocol_error(
    pending: &mut HashMap<zx::Koid, TimerState>,
    timer_id: &zx::Koid,
    error: fta::WakeAlarmsError,
) -> Option<TimerState> {
    match error {
        fta::WakeAlarmsError::Unspecified => {
            log_warn!(
                "watch_new_hrtimer_loop: Cmd::AlarmProtocolFail: unspecified error: {error:?}"
            );
            pending.remove(timer_id)
        }
        fta::WakeAlarmsError::Dropped => {
            log_debug!("watch_new_hrtimer_loop: Cmd::AlarmProtocolFail: alarm dropped: {error:?}");
            // Do not remove a Dropped timer here, in contrast to other error states: a Dropped
            // timer is a result of a Stop or a Cancel ahead of a reschedule. In both cases, that
            // code takes care of removing the timer from the pending timers list.
            None
        }
        error => {
            log_warn!(
                "watch_new_hrtimer_loop: Cmd::AlarmProtocolFail: unspecified error: {error:?}"
            );
            pending.remove(timer_id)
        }
    }
}

// This function is swapped out for an injected proxy in tests.
fn connect_to_wake_alarms_async() -> Result<zx::Channel, Errno> {
    log_debug!("connecting to wake alarms");
    let (client, server) = zx::Channel::create();
    fuchsia_component::client::connect_channel_to_protocol::<fta::WakeAlarmsMarker>(server)
        .map(|()| client)
        .map_err(|err| {
            errno!(EINVAL, format!("Failed to connect to fuchsia.time.alarms/Wake: {err}"))
        })
}

#[derive(Debug)]
enum InspectHrTimerEvent {
    Add,
    Update,
    Remove,
    // The String inside will be used in fmt. But the compiler does not recognize the use when
    // formatting with the Debug derivative.
    Error(#[allow(dead_code)] String),
}

impl InspectHrTimerEvent {
    fn retain_err(prev_len: usize, after_len: usize, context: &str) -> InspectHrTimerEvent {
        InspectHrTimerEvent::Error(format!(
            "retain the timer incorrectly, before len: {prev_len}, after len: {after_len}, context: {context}",
        ))
    }
}

impl std::fmt::Display for InspectHrTimerEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(Debug)]
struct TimerState {
    /// The task that waits for the timer to expire.
    task: fasync::Task<()>,
    /// The desired deadline for the timer.
    deadline: TargetTime,
}

impl std::fmt::Display for TimerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TimerState[deadline:{:?}]", self.deadline)
    }
}

struct HrTimerManagerState {
    /// All pending timers are stored here.
    pending_timers: HashMap<zx::Koid, TimerState>,

    /// The event that is registered with runner to allow the hrtimer to wake the kernel.
    /// Optional, because we want the ability to inject a counter in tests.
    message_counter: Option<OwnedMessageCounterHandle>,

    /// For recording timer events.
    inspect_node: BoundedListNode,

    /// The last timestamp at which the hrtimer loop was started.
    last_loop_started_timestamp: zx::BootInstant,

    /// The last timestamp at which the hrtimer loop was completed.
    last_loop_completed_timestamp: zx::BootInstant,

    // Debug progress counter for Cmd::Start.
    // TODO: b/454085350 - remove once diagnosed.
    debug_start_stage_counter: u64,
}

impl HrTimerManagerState {
    fn new(parent_node: &fuchsia_inspect::Node) -> Self {
        Self {
            pending_timers: HashMap::new(),
            // Initialized later in the State's lifecycle because it only becomes
            // available after making a connection to the wake proxy.
            message_counter: None,
            inspect_node: BoundedListNode::new(
                parent_node.create_child("events"),
                INSPECT_GRAPH_EVENT_BUFFER_SIZE,
            ),
            last_loop_started_timestamp: zx::BootInstant::INFINITE_PAST,
            last_loop_completed_timestamp: zx::BootInstant::INFINITE_PAST,
            debug_start_stage_counter: 0,
        }
    }

    fn get_pending_timers_count(&self) -> usize {
        self.pending_timers.len()
    }

    /// Gets a new shareable instance of the message counter.
    fn share_message_counter(&self, new_pending_message: bool) -> SharedMessageCounter {
        let counter_ref =
            self.message_counter.as_ref().expect("message_counter is None, but should not be.");
        counter_ref.share(new_pending_message)
    }
}

/// Asynchronous commands sent to `watch_new_hrtimer_loop`.
///
/// The synchronous methods on HrTimerManager use these commands to communicate
/// with the alarm manager actor that loops about in `watch_new_hrtimer_loop`.
///
/// This allows us to not have to share state between the synchronous and async
/// methods of `HrTimerManager`.
#[derive(Debug)]
enum Cmd {
    // Start the timer contained in `new_timer_node`.
    // The processing loop will signal `done` to allow synchronous
    // return from scheduling an async Cmd::Start.
    Start {
        new_timer_node: HrTimerNode,
        /// Signaled once the timer is started.
        done: zx::Event,
        /// The Starnix container suspend lock. Keep it alive until no more
        /// work is necessary.
        message_counter: SharedMessageCounter,
    },
    /// Stop the timer noted below. `done` is similar to above.
    Stop {
        /// The timer to stop.
        timer: HrTimerHandle,
        /// Signaled once the timer is stopped.
        done: zx::Event,
        /// The Starnix container suspend lock. Keep it alive until no more
        /// work is necessary.
        message_counter: SharedMessageCounter,
    },
    /// A wake alarm occurred.
    Alarm {
        /// The affected timer's node.
        new_timer_node: HrTimerNode,
        /// The wake lease provided by the underlying API.
        lease: zx::EventPair,
        /// The Starnix container suspend lock. Keep it alive until no more
        /// work is necessary.
        message_counter: SharedMessageCounter,
    },
    /// Install a timeline change monitor
    MonitorUtc { timer: HrTimerHandle, counter: zx::Counter, recv: mpsc::UnboundedReceiver<bool> },
}

// Increments `counter` every time the UTC timeline changes.
//
// This counter is shared with UTC timers to provide UTC timeline change notification.
//
// Use cases:
//
// 1. Counter's value counts the number of times the UTC timeline changed, which is used in timer
//    `read` calls to report the number of encountered changes, as required by `read`.
//
// 2. Counter's `COUNTER_POSITIVE` signal is used in `wait_async` calls on timers, as Starnix must
//    wake such timers whenever a timeline change happens. The counter reader must reset the
//    counter to zero after reading its value to allow for a next wake.
//
// Other primitives are not appropriate to use here: an Event does not remember how many times it
// has been signaled, so does not fulfill (1). Atomics don't generate a signal on increment, so
// don't satisfy (2). Conversely, the `wait_async` machinery on timers can already deal with
// HandleBased objects, so a Counter can be readily used there.
async fn run_utc_timeline_monitor(counter: zx::Counter, mut recv: mpsc::UnboundedReceiver<bool>) {
    log_debug!("run_utc_timeline_monitor: monitoring UTC clock timeline changes: enter");
    let utc_handle = crate::time::utc::duplicate_real_utc_clock_handle().inspect_err(|err| {
        log_error!("run_utc_timeline_monitor: could not monitor UTC timeline: {err:?}")
    });
    if let Ok(utc_handle) = utc_handle {
        let koid = utc_handle.as_handle_ref().get_koid();
        log_debug!(
            "run_utc_timeline_monitor: monitoring UTC clock timeline: enter: UTC clock koid={koid:?}, counter={counter:?}"
        );
        let utc_handle = std::rc::Rc::new(utc_handle);
        let utc_handle_fn = || utc_handle.clone();
        let mut interested = false;
        loop {
            let utc_handle = utc_handle_fn();
            // CLOCK_UPDATED is auto-cleared.
            let mut updated_fut =
                fasync::OnSignals::new(utc_handle.as_handle_ref(), zx::Signals::CLOCK_UPDATED)
                    .fuse();
            let mut interest_fut = recv.next();
            select! {
                result = updated_fut => {
                    if result.is_err() {
                        log_warn!("run_utc_timeline_monitor: could not wait on signals: {:?}, counter={counter:?}", result);
                        break;
                    }
                    if interested {
                        log_debug!("run_utc_timeline_monitor: UTC timeline updated, counter: {counter:?}");
                        // The consumer of this `counter` should wait for COUNTER_POSITIVE, and
                        // once it observes the value of the counter, subtract the read value from
                        // counter.
                        counter
                            .add(1)
                            // Ignore the error after logging it. Should we exit the loop here?
                            .inspect_err(|err| {
                                log_error!("run_utc_timeline_monitor: could not increment counter: {err:?}")
                            })
                            .unwrap_or(());
                    }
                },
                result = interest_fut => {
                    if let Some(interest) = result {
                        log_debug!("interest change: {counter:?}, interest: {interest:?}");
                        interested = interest;
                    }
                },
            };
        }
    }
    log_debug!("run_utc_timeline_monitor: monitoring UTC clock timeline changes: exit");
}

/// The manager for high-resolution timers.
///
/// This manager is responsible for creating and managing high-resolution timers.
pub struct HrTimerManager {
    state: Mutex<HrTimerManagerState>,

    /// The channel sender that notifies the worker thread that HrTimer driver needs to be
    /// (re)started with a new deadline.
    start_next_sender: OnceLock<UnboundedSender<Cmd>>,
}
pub type HrTimerManagerHandle = Arc<HrTimerManager>;

impl HrTimerManager {
    pub fn new(parent_node: &fuchsia_inspect::Node) -> HrTimerManagerHandle {
        let inspect_node = parent_node.create_child("hr_timer_manager");
        let new_manager = Arc::new(Self {
            state: Mutex::new(HrTimerManagerState::new(&inspect_node)),
            start_next_sender: Default::default(),
        });
        let manager_weak = Arc::downgrade(&new_manager);

        // Create a lazy inspect node to get HrTimerManager info at read-time.
        inspect_node.record_lazy_child("hr_timer_manager", move || {
            let manager_ref = manager_weak.upgrade().expect("inner HrTimerManager");
            async move {
                // This gets the clock value directly from the kernel, it is not subject
                // to the local runner's clock.
                let now = zx::BootInstant::get();

                let inspector = fuchsia_inspect::Inspector::default();
                inspector.root().record_int("now_ns", now.into_nanos());

                let (
                    timers,
                    pending_timers_count,
                    message_counter,
                    loop_started,
                    loop_completed,
                    debug_start_stage_counter,
                ) = {
                    let guard = manager_ref.lock();
                    (
                        guard
                            .pending_timers
                            .iter()
                            .map(|(k, v)| (*k, v.deadline))
                            .collect::<Vec<_>>(),
                        guard.get_pending_timers_count(),
                        guard.message_counter.as_ref().map(|c| c.to_string()).unwrap_or_default(),
                        guard.last_loop_started_timestamp,
                        guard.last_loop_completed_timestamp,
                        guard.debug_start_stage_counter,
                    )
                };
                inspector.root().record_uint("pending_timers_count", pending_timers_count as u64);
                inspector.root().record_string("message_counter", message_counter);

                // These are the deadlines we are currently waiting for. The format is:
                // `alarm koid` -> `deadline nanos` (remains: `duration until alarm nanos`)
                let deadlines = inspector.root().create_string_array("timers", timers.len());
                for (i, (k, v)) in timers.into_iter().enumerate() {
                    let remaining = v.estimate_boot().unwrap() - now;
                    deadlines.set(
                        i,
                        format!(
                            "{k:?} -> {v} ns (remains: {})",
                            time_pretty::format_duration(remaining)
                        ),
                    );
                }
                inspector.root().record(deadlines);

                inspector.root().record_int("last_loop_started_at_ns", loop_started.into_nanos());
                inspector
                    .root()
                    .record_int("last_loop_completed_at_ns", loop_completed.into_nanos());
                inspector
                    .root()
                    .record_uint("debug_start_stage_counter", debug_start_stage_counter);

                Ok(inspector)
            }
            .boxed()
        });
        parent_node.record(inspect_node);
        new_manager
    }

    /// Get a copy of a sender channel used for passing async command to the
    /// event processing loop.
    fn get_sender(&self) -> UnboundedSender<Cmd> {
        self.start_next_sender.get().expect("start_next_sender is initialized").clone()
    }

    /// Returns the counter that tallies the timeline changes of the UTC timeline.
    ///
    /// # Args
    /// - `timer`: the handle of the timer that needs monitoring of timeline changes.
    pub fn get_timeline_change_observer(
        &self,
        timer: &HrTimerHandle,
    ) -> Result<TimelineChangeObserver, Errno> {
        let timer_id = timer.get_id();
        let counter = zx::Counter::create();
        let counter_clone = duplicate_handle(&counter).map_err(|err| {
            log_error!("could not duplicate handle: {err:?}");
            errno!(EINVAL, format!("could not duplicate handle: {err}, {timer_id:?}"))
        })?;
        let (send, recv) = mpsc::unbounded();
        self.get_sender()
            .unbounded_send(Cmd::MonitorUtc { timer: timer.clone(), counter, recv })
            .map_err(|err| {
            log_error!("could not send: {err:?}");
            errno!(EINVAL, format!("could not send Cmd::Monitor: {err}, {timer_id:?}"))
        })?;
        Ok(TimelineChangeObserver::new(counter_clone, send))
    }

    /// Initialize the [HrTimerManager] in the context of the current system task.
    pub fn init(self: &HrTimerManagerHandle, system_task: &CurrentTask) -> Result<(), Errno> {
        self.init_internal(
            system_task,
            /*wake_channel_for_test=*/ None,
            /*message_counter_for_test=*/ None,
        )
    }

    // Call this init for testing instead of the one above.
    fn init_internal(
        self: &HrTimerManagerHandle,
        system_task: &CurrentTask,
        // Can be injected for testing.
        wake_channel_for_test: Option<zx::Channel>,
        // Can be injected for testing.
        message_counter_for_test: Option<zx::Counter>,
    ) -> Result<(), Errno> {
        let (start_next_sender, start_next_receiver) = mpsc::unbounded();
        self.start_next_sender.set(start_next_sender).map_err(|_| errno!(EEXIST))?;

        let self_ref = self.clone();

        // Ensure that all internal init has completed in `watch_new_hrtimer_loop`
        // before proceeding from here.
        let setup_done = zx::Event::create();
        let setup_done_clone = duplicate_handle(&setup_done)?;

        system_task.kernel().kthreads.spawn_async(async move |locked_and_task| {
            let current_thread = std::thread::current();
            // Helps find the thread in backtraces, see wait_signaled_sync.
            log_info!(
                "hr_timer_manager thread: {:?} ({:?})",
                current_thread.name(),
                current_thread.id()
            );
            if let Err(e) = self_ref
                .watch_new_hrtimer_loop(
                    locked_and_task.current_task(),
                    start_next_receiver,
                    wake_channel_for_test,
                    message_counter_for_test,
                    Some(setup_done_clone),
                )
                .await
            {
                log_error!("while running watch_new_hrtimer_loop: {e:?}");
            }
            log_warn!("hr_timer_manager: finished kernel thread. should never happen in prod code");
        });
        wait_signaled_sync(&setup_done)
            .to_result()
            .map_err(|status| from_status_like_fdio!(status))?;

        Ok(())
    }

    // Notifies `timer` and wake sources about a triggered alarm.
    fn notify_timer(
        self: &HrTimerManagerHandle,
        system_task: &CurrentTask,
        timer: &HrTimerNode,
        lease: impl HandleBased,
    ) -> Result<()> {
        let timer_id = timer.hr_timer.get_id();
        log_debug!("watch_new_hrtimer_loop: Cmd::Alarm: triggered alarm: {:?}", timer_id);
        ftrace::duration!(c"alarms", c"starnix:hrtimer:notify_timer", "timer_id" => timer_id);
        self.lock().pending_timers.remove(&timer_id).map(|s| s.task.detach());
        signal_handle(&timer.hr_timer.event(), zx::Signals::NONE, zx::Signals::TIMER_SIGNALED)
            .context("notify_timer: hrtimer signal handle")?;

        // Handle wake source here.
        let wake_source = timer.wake_source.clone();
        if let Some(wake_source) = wake_source.as_ref().and_then(|f| f.upgrade()) {
            let lease_token = lease.into_handle();
            wake_source.on_wake(system_task, &lease_token);
            // Drop the baton lease after wake leases in associated epfd
            // are activated.
            drop(lease_token);
        }
        ftrace::instant!(c"alarms", c"starnix:hrtimer:notify_timer:drop_lease", ftrace::Scope::Process, "timer_id" => timer_id);
        Ok(())
    }

    // If no counter has been injected for tests, set provided `counter` to serve as that
    // counter. Used to inject a fake counter in tests.
    fn inject_or_set_message_counter(
        self: &HrTimerManagerHandle,
        message_counter: OwnedMessageCounterHandle,
    ) {
        let mut guard = self.lock();
        if guard.message_counter.is_none() {
            guard.message_counter = Some(message_counter);
        }
    }

    fn record_inspect_on_stop(
        self: &HrTimerManagerHandle,
        guard: &mut MutexGuard<'_, HrTimerManagerState>,
        prev_len: usize,
    ) {
        let after_len = guard.get_pending_timers_count();
        let inspect_event_type = if after_len == prev_len {
            None
        } else if after_len == prev_len - 1 {
            Some(InspectHrTimerEvent::Remove)
        } else {
            Some(InspectHrTimerEvent::retain_err(prev_len, after_len, "removing timer"))
        };
        if let Some(inspect_event_type) = inspect_event_type {
            self.record_event(guard, inspect_event_type, None);
        }
    }

    fn record_inspect_on_start(
        self: &HrTimerManagerHandle,
        guard: &mut MutexGuard<'_, HrTimerManagerState>,
        timer_id: zx::Koid,
        task: fasync::Task<()>,
        deadline: TargetTime,
        prev_len: usize,
    ) {
        guard
            .pending_timers
            .insert(timer_id, TimerState { task, deadline })
            .map(|timer_state| {
                // This should not happen, at this point we already canceled
                // any previous instances of the same wake alarm.
                log_debug!(
                    "watch_new_hrtimer_loop: removing timer task in Cmd::Start: {:?}",
                    timer_state
                );
                timer_state
            })
            .map(|v| v.task.detach());

        // Record the inspect event
        let after_len = guard.get_pending_timers_count();
        let inspect_event_type = if after_len == prev_len {
            InspectHrTimerEvent::Update
        } else if after_len == prev_len + 1 {
            InspectHrTimerEvent::Add
        } else {
            InspectHrTimerEvent::retain_err(prev_len, after_len, "adding timer")
        };
        self.record_event(guard, inspect_event_type, Some(deadline));
    }

    /// Timer handler loop.
    ///
    /// # Args:
    /// - `wake_channel_for_test`: a channel implementing `fuchsia.time.alarms/Wake`
    ///   injected by tests only.
    /// - `message_counter_for_test`: a zx::Counter injected only by tests, to
    ///   emulate the wake proxy message counter.
    /// - `setup_done`: signaled once the initial loop setup is complete. Allows
    ///   pausing any async callers until this loop is in a runnable state.
    async fn watch_new_hrtimer_loop(
        self: &HrTimerManagerHandle,
        system_task: &CurrentTask,
        mut start_next_receiver: UnboundedReceiver<Cmd>,
        mut wake_channel_for_test: Option<zx::Channel>,
        message_counter_for_test: Option<zx::Counter>,
        setup_done: Option<zx::Event>,
    ) -> Result<()> {
        ftrace::instant!(c"alarms", c"watch_new_hrtimer_loop:init", ftrace::Scope::Process);
        defer! {
            log_warn!("watch_new_hrtimer_loop: exiting. This should only happen in tests.");
        }

        let wake_channel = wake_channel_for_test.take().unwrap_or_else(|| {
            connect_to_wake_alarms_async().expect("connection to wake alarms async proxy")
        });

        let counter_name = "wake-alarms";
        let (device_channel, counter) = if let Some(message_counter) = message_counter_for_test {
            // For tests only.
            (wake_channel, message_counter)
        } else {
            create_proxy_for_wake_events_counter_zero(wake_channel, counter_name.to_string())
        };
        let message_counter = system_task
            .kernel()
            .suspend_resume_manager
            .add_message_counter(counter_name, Some(counter));
        self.inject_or_set_message_counter(message_counter.clone());
        setup_done
            .as_ref()
            .map(|e| signal_handle(e, zx::Signals::NONE, zx::Signals::EVENT_SIGNALED));

        let device_async_proxy =
            fta::WakeAlarmsProxy::new(fidl::AsyncChannel::from_channel(device_channel));

        // Contains suspend locks for interval (periodic) timers that expired, but have not been
        // rescheduled yet. This allows us to defer container suspend until all such timers have
        // been rescheduled.
        // TODO: b/418813184 - Remove in favor of Fuchsia-specific interval timer support
        // once it is available.
        let mut interval_timers_pending_reschedule: HashMap<zx::Koid, SharedMessageCounter> =
            HashMap::new();

        // Per timer tasks.
        let mut task_by_timer_id: HashMap<zx::Koid, fasync::Task<()>> = HashMap::new();

        ftrace::instant!(c"alarms", c"watch_new_hrtimer_loop:init_done", ftrace::Scope::Process);
        while let Some(cmd) = start_next_receiver.next().await {
            self.lock().last_loop_started_timestamp = zx::BootInstant::get();
            ftrace::duration!(c"alarms", c"start_next_receiver:loop");

            log_debug!("watch_new_hrtimer_loop: got command: {cmd:?}");
            self.lock().debug_start_stage_counter = 0;
            match cmd {
                // A new timer needs to be started.  The timer node for the timer
                // is provided, and `done` must be signaled once the setup is
                // complete.
                Cmd::Start { new_timer_node, done, message_counter } => {
                    self.lock().debug_start_stage_counter = 1;
                    defer! {
                        // Allow add_timer to proceed once command processing is done.
                        signal_handle(&done, zx::Signals::NONE, zx::Signals::EVENT_SIGNALED).map_err(|err| to_errno_with_log(err)).expect("event can be signaled");
                    }

                    let hr_timer = &new_timer_node.hr_timer;
                    let timer_id = hr_timer.get_id();
                    let wake_alarm_id = hr_timer.wake_alarm_id();
                    let trace_id = hr_timer.trace_id();
                    log_debug!(
                        "watch_new_hrtimer_loop: Cmd::Start: timer_id: {:?}, wake_alarm_id: {}",
                        timer_id,
                        wake_alarm_id
                    );
                    ftrace::duration!(c"alarms", c"starnix:hrtimer:start", "timer_id" => timer_id);
                    ftrace::flow_begin!(c"alarms", c"hrtimer_lifecycle", trace_id);

                    self.lock().debug_start_stage_counter = 2;
                    let maybe_cancel = self.lock().pending_timers.remove(&timer_id);
                    log_long_op!(cancel_by_id(
                        &message_counter,
                        maybe_cancel,
                        &timer_id,
                        &device_async_proxy,
                        &mut interval_timers_pending_reschedule,
                        &mut task_by_timer_id,
                        &wake_alarm_id,
                    ));
                    ftrace::instant!(c"alarms", c"starnix:hrtimer:cancel_pre_start", ftrace::Scope::Process, "timer_id" => timer_id);

                    // Signaled when the timer completed setup. We can not forward `done` because
                    // we have post-schedule work as well.
                    let setup_event = zx::Event::create();
                    let deadline = new_timer_node.deadline;

                    ftrace::duration!(c"alarms", c"starnix:hrtimer:signaled", "timer_id" => timer_id);

                    self.lock().debug_start_stage_counter = 3;
                    // Make a request here. Move it into the closure after. Current FIDL semantics
                    // ensure that even though we do not `.await` on this future, a request to
                    // schedule a wake alarm based on this timer will be sent.
                    let request_fut = match deadline {
                        TargetTime::Monotonic(_) => {
                            // If we hit this, it's a Starnix bug.
                            panic!("can not schedule wake alarm on monotonic timeline")
                        }
                        TargetTime::BootInstant(boot_instant) => device_async_proxy.set_and_wait(
                            boot_instant,
                            fta::SetMode::NotifySetupDone(duplicate_handle(&setup_event)?),
                            &wake_alarm_id,
                        ),
                        TargetTime::RealTime(utc_instant) => device_async_proxy.set_and_wait_utc(
                            &fta::InstantUtc { timestamp_utc: utc_instant.into_nanos() },
                            fta::SetMode::NotifySetupDone(duplicate_handle(&setup_event)?),
                            &wake_alarm_id,
                        ),
                    };
                    let mut done_sender = self.get_sender();
                    let prev_len = self.lock().get_pending_timers_count();

                    self.lock().debug_start_stage_counter = 4;
                    let self_clone = self.clone();
                    let task = fasync::Task::local(async move {
                        log_debug!(
                            "wake_alarm_future: set_and_wait will block here: {wake_alarm_id:?}"
                        );
                        ftrace::instant!(c"alarms", c"starnix:hrtimer:wait", ftrace::Scope::Process, "timer_id" => timer_id);
                        ftrace::flow_step!(c"alarms", c"hrtimer_lifecycle", trace_id);

                        // Wait for this timer to expire. This wait can be arbitrarily long.
                        let response = request_fut.await;

                        // The counter was already incremented by the wake proxy when the alarm fired.
                        let message_counter = self_clone.lock().share_message_counter(false);
                        ftrace::instant!(c"alarms", c"starnix:hrtimer:wake", ftrace::Scope::Process, "timer_id" => timer_id);

                        log_debug!("wake_alarm_future: set_and_wait over: {:?}", response);
                        match response {
                            // Alarm.  This must be processed in the main loop because notification
                            // requires access to &CurrentTask, which is not available here. So we
                            // only forward it.
                            Ok(Ok(lease)) => {
                                log_long_op!(done_sender.send(Cmd::Alarm {
                                    new_timer_node,
                                    lease,
                                    message_counter
                                }))
                                .expect("infallible");
                            }
                            Ok(Err(error)) => {
                                ftrace::duration!(c"alarms", c"starnix:hrtimer:wake_error", "timer_id" => timer_id);
                                log_debug!(
                                    "wake_alarm_future: protocol error: {error:?}: timer_id: {timer_id:?}"
                                );
                                let mut guard = self_clone.lock();
                                let pending = &mut guard.pending_timers;
                                process_alarm_protocol_error(pending, &timer_id, error);
                            }
                            Err(error) => {
                                ftrace::duration!(c"alarms", c"starnix:hrtimer:fidl_error", "timer_id" => timer_id);
                                log_debug!(
                                    "wake_alarm_future: FIDL error: {error:?}: timer_id: {timer_id:?}"
                                );
                                self_clone.lock().pending_timers.remove(&timer_id);
                            }
                        }
                        log_debug!("wake_alarm_future: closure done for timer_id: {timer_id:?}");
                    });
                    self.lock().debug_start_stage_counter = 5;
                    ftrace::instant!(c"alarms", c"starnix:hrtimer:pre_setup_event_signal", ftrace::Scope::Process, "timer_id" => timer_id);

                    // This should be almost instantaneous.  Blocking for a long time here is a
                    // bug.
                    log_long_op!(wait_signaled(&setup_event)).map_err(|e| to_errno_with_log(e))?;
                    ftrace::instant!(c"alarms", c"starnix:hrtimer:setup_event_signaled", ftrace::Scope::Process, "timer_id" => timer_id);
                    let mut guard = self.lock();
                    guard.debug_start_stage_counter = 6;
                    self.record_inspect_on_start(&mut guard, timer_id, task, deadline, prev_len);
                    log_debug!("Cmd::Start scheduled: timer_id: {:?}", timer_id);
                    guard.debug_start_stage_counter = 999;
                }
                Cmd::Alarm { new_timer_node, lease, message_counter } => {
                    let timer = &new_timer_node.hr_timer;
                    let timer_id = timer.get_id();
                    ftrace::duration!(c"alarms", c"starnix:hrtimer:alarm", "timer_id" => timer_id);
                    ftrace::flow_step!(c"alarms", c"hrtimer_lifecycle", timer.trace_id());
                    self.notify_timer(system_task, &new_timer_node, lease)
                        .map_err(|e| to_errno_with_log(e))?;

                    // Interval timers currently need special handling: we must not suspend the
                    // container until the interval timer in question gets re-scheduled. To
                    // ensure that we stay awake, we store the suspend lock for a while. This
                    // prevents container suspend.
                    //
                    // This map entry and its MessageCounterHandle is removed in one of the following cases:
                    //
                    // (1) When the interval timer eventually gets rescheduled. We
                    // assume that for interval timers the reschedule will be imminent and that
                    // therefore not suspending until that re-schedule happens will not unreasonably
                    // extend the awake period.
                    //
                    // (2) When the timer is canceled.
                    if *timer.is_interval.lock() {
                        interval_timers_pending_reschedule.insert(timer_id, message_counter);
                    }
                    log_debug!("Cmd::Alarm done: timer_id: {timer_id:?}");
                }
                Cmd::Stop { timer, done, message_counter } => {
                    defer! {
                        signal_handle(&done, zx::Signals::NONE, zx::Signals::EVENT_SIGNALED).expect("can signal");
                    }
                    let timer_id = timer.get_id();
                    log_debug!("watch_new_hrtimer_loop: Cmd::Stop: timer_id: {:?}", timer_id);
                    ftrace::duration!(c"alarms", c"starnix:hrtimer:stop", "timer_id" => timer_id);
                    ftrace::flow_step!(c"alarms", c"hrtimer_lifecycle", timer.trace_id());

                    let (maybe_cancel, prev_len) = {
                        let mut guard = self.lock();
                        let prev_len = guard.get_pending_timers_count();
                        (guard.pending_timers.remove(&timer_id), prev_len)
                    };

                    let wake_alarm_id = timer.wake_alarm_id();
                    log_long_op!(cancel_by_id(
                        &message_counter,
                        maybe_cancel,
                        &timer_id,
                        &device_async_proxy,
                        &mut interval_timers_pending_reschedule,
                        &mut task_by_timer_id,
                        &wake_alarm_id,
                    ));
                    ftrace::instant!(c"alarms", c"starnix:hrtimer:cancel_at_stop", ftrace::Scope::Process, "timer_id" => timer_id);

                    {
                        let mut guard = self.lock();
                        self.record_inspect_on_stop(&mut guard, prev_len);
                    }
                    log_debug!("Cmd::Stop done: {timer_id:?}");
                }
                Cmd::MonitorUtc { timer, counter, recv } => {
                    ftrace::duration!(c"alarms", c"starnix:hrtimer:monitor_utc", "timer_id" => timer.get_id());
                    ftrace::flow_step!(c"alarms", c"hrtimer_lifecycle", timer.trace_id());
                    let monitor_task = fasync::Task::local(async move {
                        run_utc_timeline_monitor(counter, recv).await;
                    });
                    task_by_timer_id.insert(timer.get_id(), monitor_task);
                }
            }
            let mut guard = self.lock();

            log_debug!(
                "watch_new_hrtimer_loop: pending timers count: {}",
                guard.pending_timers.len()
            );
            log_debug!("watch_new_hrtimer_loop: pending timers:       {:?}", guard.pending_timers);
            log_debug!(
                "watch_new_hrtimer_loop: message counter:      {:?}",
                message_counter.to_string(),
            );
            log_debug!(
                "watch_new_hrtimer_loop: interval timers:      {:?}",
                interval_timers_pending_reschedule.len(),
            );

            guard.last_loop_completed_timestamp = zx::BootInstant::get();
        } // while

        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, HrTimerManagerState> {
        self.state.lock()
    }

    fn record_event(
        self: &HrTimerManagerHandle,
        guard: &mut MutexGuard<'_, HrTimerManagerState>,
        event_type: InspectHrTimerEvent,
        deadline: Option<TargetTime>,
    ) {
        guard.inspect_node.add_entry(move |node| {
            node.record_string("type", event_type.to_string());
            node.record_int("created_at", zx::BootInstant::get().into_nanos());
            if let Some(deadline) = deadline {
                node.record_int("deadline", deadline.estimate_boot().unwrap().into_nanos());
            }
        });
    }

    /// Add a new timer.
    ///
    /// A wake alarm is scheduled for the timer.
    pub fn add_timer(
        self: &HrTimerManagerHandle,
        wake_source: Option<Weak<dyn OnWakeOps>>,
        new_timer: &HrTimerHandle,
        deadline: TargetTime,
    ) -> Result<(), Errno> {
        log_debug!("add_timer: entry: {new_timer:?}, deadline: {deadline:?}");
        ftrace::duration!(c"alarms", c"starnix:add_timer", "deadline" => deadline.estimate_boot().unwrap().into_nanos());
        ftrace::flow_step!(c"alarms", c"hrtimer_lifecycle", new_timer.trace_id());

        // Keep system awake until timer is scheduled.
        let message_counter_until_timer_scheduled = self.lock().share_message_counter(true);

        let sender = self.get_sender();
        let new_timer_node = HrTimerNode::new(deadline, wake_source, new_timer.clone());
        let wake_alarm_scheduled = zx::Event::create();
        let wake_alarm_scheduled_clone = duplicate_handle(&wake_alarm_scheduled)?;
        let timer_id = new_timer.get_id();
        sender
            .unbounded_send(Cmd::Start {
                new_timer_node,
                message_counter: message_counter_until_timer_scheduled,
                done: wake_alarm_scheduled_clone,
            })
            .map_err(|_| errno!(EINVAL, "add_timer: could not send Cmd::Start"))?;

        // Block until the wake alarm for this timer is scheduled.
        wait_signaled_sync(&wake_alarm_scheduled)
            .map_err(|_| errno!(EINVAL, "add_timer: wait_signaled_sync failed"))?;

        log_debug!("add_timer: exit : timer_id: {timer_id:?}");
        Ok(())
    }

    /// Remove a timer.
    ///
    /// The timer is removed if scheduled, nothing is changed if it is not.
    pub fn remove_timer(self: &HrTimerManagerHandle, timer: &HrTimerHandle) -> Result<(), Errno> {
        log_debug!("remove_timer: entry:  {timer:?}");
        ftrace::duration!(c"alarms", c"starnix:remove_timer");
        // Keep system awake until timer is removed.
        let message_counter_until_removed = self.lock().share_message_counter(true);

        let sender = self.get_sender();
        let done = zx::Event::create();
        let done_clone = duplicate_handle(&done)?;
        let timer_id = timer.get_id();
        sender
            .unbounded_send(Cmd::Stop {
                timer: timer.clone(),
                message_counter: message_counter_until_removed,
                done: done_clone,
            })
            .map_err(|_| errno!(EINVAL, "remove_timer: could not send Cmd::Stop"))?;

        // Block until the alarm for this timer is scheduled.
        wait_signaled_sync(&done)
            .map_err(|_| errno!(EINVAL, "add_timer: wait_signaled_sync failed"))?;
        log_debug!("remove_timer: exit:  {timer_id:?}");
        Ok(())
    }
}

#[derive(Debug)]
pub struct HrTimer {
    event: zx::Event,

    /// True iff the timer is currently set to trigger at an interval.
    ///
    /// This is used to determine at which point the hrtimer event (not
    /// `HrTimer::event` but the one that is shared with the actual driver)
    /// should be cleared.
    ///
    /// If this is true, the timer manager will wait to clear the timer event
    /// until the next timer request has been sent to the driver. This prevents
    /// lost wake ups where the container happens to suspend between two instances
    /// of an interval timer triggering.
    pub is_interval: Mutex<bool>,
}
pub type HrTimerHandle = Arc<HrTimer>;

impl Drop for HrTimer {
    fn drop(&mut self) {
        let wake_alarm_id = self.wake_alarm_id();
        ftrace::duration!(c"alarms", c"hrtimer::drop", "timer_id" => self.get_id(), "wake_alarm_id" => &wake_alarm_id[..]);
        ftrace::flow_end!(c"alarms", c"hrtimer_lifecycle", self.trace_id());
    }
}

impl HrTimer {
    pub fn new() -> HrTimerHandle {
        let ret = Arc::new(Self { event: zx::Event::create(), is_interval: Mutex::new(false) });
        let wake_alarm_id = ret.wake_alarm_id();
        ftrace::duration!(c"alarms", c"hrtimer::new", "timer_id" => ret.get_id(), "wake_alarm_id" => &wake_alarm_id[..]);
        ftrace::flow_begin!(c"alarms", c"hrtimer_lifecycle", ret.trace_id(), "wake_alarm_id" => &wake_alarm_id[..]);
        ret
    }

    pub fn event(&self) -> zx::Event {
        self.event
            .duplicate_handle(zx::Rights::SAME_RIGHTS)
            .expect("Duplicate hrtimer event handle")
    }

    /// Returns the unique identifier of this [HrTimer].
    ///
    /// All holders of the same [HrTimerHandle] will see the same value here.
    pub fn get_id(&self) -> zx::Koid {
        self.event.as_handle_ref().get_koid().expect("infallible")
    }

    /// Returns the unique alarm ID for this [HrTimer].
    ///
    /// The naming pattern is: `starnix:Koid(NNNNN):iB`, where `NNNNN` is a koid
    /// and B is `1` if the timer is an interval timer, or `0` otherwise.
    fn wake_alarm_id(&self) -> String {
        let i = if *self.is_interval.lock() { "i1" } else { "i0" };
        let koid = self.get_id();
        format!("starnix:{koid:?}:{i}")
    }

    fn trace_id(&self) -> ftrace::Id {
        self.get_id().raw_koid().into()
    }
}

impl TimerOps for HrTimerHandle {
    fn start(
        &self,
        current_task: &CurrentTask,
        source: Option<Weak<dyn OnWakeOps>>,
        deadline: TargetTime,
    ) -> Result<(), Errno> {
        // Before (re)starting the timer, ensure the signal is cleared.
        signal_handle(&self.event, zx::Signals::TIMER_SIGNALED, zx::Signals::NONE)
            .map_err(|status| from_status_like_fdio!(status))?;
        current_task.kernel().hrtimer_manager.add_timer(
            source,
            self,
            deadline.into_resolved_utc_deadline(),
        )?;
        Ok(())
    }

    fn stop(&self, kernel: &Arc<Kernel>) -> Result<(), Errno> {
        // Clear the signal when removing the hrtimer.
        signal_handle(&self.event, zx::Signals::TIMER_SIGNALED, zx::Signals::NONE)
            .map_err(|status| from_status_like_fdio!(status))?;
        Ok(kernel.hrtimer_manager.remove_timer(self)?)
    }

    fn as_handle_ref(&self) -> HandleRef<'_> {
        self.event.as_handle_ref()
    }

    fn get_timeline_change_observer(
        &self,
        current_task: &CurrentTask,
    ) -> Option<TimelineChangeObserver> {
        // Should this return errno instead?
        current_task
            .kernel()
            .hrtimer_manager
            .get_timeline_change_observer(self)
            .inspect_err(|err| {
                log_error!("hr_timer_manager: could not create timeline change counter: {err:?}")
            })
            .ok()
    }
}

/// Represents a node of `HrTimer`.
#[derive(Clone, Debug)]
struct HrTimerNode {
    /// The deadline of the associated `HrTimer`.
    deadline: TargetTime,

    /// The source where initiated this `HrTimer`.
    ///
    /// When the timer expires, the system will be woken up if necessary. The `on_wake` callback
    /// will be triggered with a baton lease to prevent further suspend while Starnix handling the
    /// wake event.
    wake_source: Option<Weak<dyn OnWakeOps>>,

    /// The underlying HrTimer.
    hr_timer: HrTimerHandle,
}

impl HrTimerNode {
    fn new(
        deadline: TargetTime,
        wake_source: Option<Weak<dyn OnWakeOps>>,
        hr_timer: HrTimerHandle,
    ) -> Self {
        Self { deadline, wake_source, hr_timer }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::HrTimer;
    use crate::testing::spawn_kernel_and_run;
    use fake_wake_alarms::{MAGIC_EXPIRE_DEADLINE, Response, serve_fake_wake_alarms};
    use fuchsia_runtime::UtcInstant;
    use std::thread;
    use {fidl_fuchsia_time_alarms as fta, fuchsia_async as fasync};

    impl HrTimerManagerState {
        fn new_for_test() -> Self {
            Self {
                inspect_node: BoundedListNode::new(
                    fuchsia_inspect::component::inspector().root().create_child("events"),
                    INSPECT_GRAPH_EVENT_BUFFER_SIZE,
                ),
                pending_timers: Default::default(),
                message_counter: None,
                last_loop_started_timestamp: zx::BootInstant::INFINITE_PAST,
                last_loop_completed_timestamp: zx::BootInstant::INFINITE_PAST,
                debug_start_stage_counter: 0,
            }
        }
    }

    // Injected for testing.
    fn connect_factory(message_counter: zx::Counter, response_type: Response) -> zx::Channel {
        let (client, server) = zx::Channel::create();

        // A separate thread is needed to allow independent execution of the server.
        let _detached = thread::spawn(move || {
            fasync::LocalExecutor::default().run_singlethreaded(async move {
                let stream =
                    fidl::endpoints::ServerEnd::<fta::WakeAlarmsMarker>::new(server).into_stream();
                serve_fake_wake_alarms(message_counter, response_type, stream, /*once*/ false)
                    .await;
            });
        });
        client
    }

    // Initializes HrTimerManager for tests.
    //
    // # Returns
    //
    // A tuple of:
    // - `HrTimerManagerHandle` the unit under test
    // - `zx::Counter` a message counter to use in tests to observe suspend state
    fn init_hr_timer_manager(
        current_task: &CurrentTask,
        response_type: Response,
    ) -> (HrTimerManagerHandle, zx::Counter) {
        let manager = Arc::new(HrTimerManager {
            state: Mutex::new(HrTimerManagerState::new_for_test()),
            start_next_sender: Default::default(),
        });
        let counter = zx::Counter::create();
        let counter_clone = duplicate_handle(&counter).unwrap();
        let wake_channel = connect_factory(counter_clone, response_type);
        let counter_clone = duplicate_handle(&counter).unwrap();
        manager
            .init_internal(&current_task, Some(wake_channel), Some(counter_clone))
            .expect("infallible");
        (manager, counter)
    }

    #[fuchsia::test]
    async fn test_triggering() {
        spawn_kernel_and_run(async |_, current_task| {
            let (manager, counter) = init_hr_timer_manager(current_task, Response::Immediate);

            let timer1 = HrTimer::new();
            let timer2 = HrTimer::new();
            let timer3 = HrTimer::new();

            manager.add_timer(None, &timer1, zx::BootInstant::from_nanos(1).into()).unwrap();
            manager.add_timer(None, &timer2, zx::BootInstant::from_nanos(2).into()).unwrap();
            manager.add_timer(None, &timer3, zx::BootInstant::from_nanos(3).into()).unwrap();

            wait_signaled_sync(&timer1.event()).to_result().unwrap();
            wait_signaled_sync(&timer2.event()).to_result().unwrap();
            wait_signaled_sync(&timer3.event()).to_result().unwrap();

            assert_eq!(
                counter
                    .wait_handle(zx::Signals::COUNTER_NON_POSITIVE, zx::MonotonicInstant::INFINITE),
                zx::WaitResult::Ok(zx::Signals::COUNTER_NON_POSITIVE)
            );
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_triggering_utc() {
        spawn_kernel_and_run(async |_, current_task| {
            let (manager, counter) = init_hr_timer_manager(current_task, Response::Immediate);

            let timer1 = HrTimer::new();
            let timer2 = HrTimer::new();
            let timer3 = HrTimer::new();

            // All these are normally already expired as scheduled.
            manager.add_timer(None, &timer1, UtcInstant::from_nanos(1).into()).unwrap();
            manager.add_timer(None, &timer2, UtcInstant::from_nanos(2).into()).unwrap();
            manager.add_timer(None, &timer3, UtcInstant::from_nanos(3).into()).unwrap();

            wait_signaled_sync(&timer1.event()).to_result().unwrap();
            wait_signaled_sync(&timer2.event()).to_result().unwrap();
            wait_signaled_sync(&timer3.event()).to_result().unwrap();

            assert_eq!(
                counter
                    .wait_handle(zx::Signals::COUNTER_NON_POSITIVE, zx::MonotonicInstant::INFINITE),
                zx::WaitResult::Ok(zx::Signals::COUNTER_NON_POSITIVE)
            );
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_delayed_response() {
        spawn_kernel_and_run(async |_, current_task| {
            let (manager, counter) = init_hr_timer_manager(current_task, Response::Immediate);

            let timer = HrTimer::new();

            manager.add_timer(None, &timer, zx::BootInstant::from_nanos(1).into()).unwrap();

            assert_eq!(
                counter
                    .wait_handle(zx::Signals::COUNTER_NON_POSITIVE, zx::MonotonicInstant::INFINITE),
                zx::WaitResult::Ok(zx::Signals::COUNTER_NON_POSITIVE)
            );
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_protocol_error_response() {
        spawn_kernel_and_run(async |_, current_task| {
            let (manager, counter) = init_hr_timer_manager(current_task, Response::Error);

            let timer = HrTimer::new();
            manager.add_timer(None, &timer, zx::BootInstant::from_nanos(1).into()).unwrap();
            assert_eq!(
                counter
                    .wait_handle(zx::Signals::COUNTER_NON_POSITIVE, zx::MonotonicInstant::INFINITE),
                zx::WaitResult::Ok(zx::Signals::COUNTER_NON_POSITIVE)
            );
        })
        .await;
    }

    #[fuchsia::test]
    async fn reschedule_same_timer() {
        spawn_kernel_and_run(async |_, current_task| {
            let (manager, counter) = init_hr_timer_manager(current_task, Response::Delayed);

            let timer = HrTimer::new();

            manager.add_timer(None, &timer, zx::BootInstant::from_nanos(1).into()).unwrap();
            manager.add_timer(None, &timer, zx::BootInstant::from_nanos(2).into()).unwrap();

            // Force alarm expiry.
            manager
                .add_timer(None, &timer, zx::BootInstant::from_nanos(MAGIC_EXPIRE_DEADLINE).into())
                .unwrap();
            wait_signaled_sync(&timer.event()).to_result().unwrap();

            assert_eq!(
                counter
                    .wait_handle(zx::Signals::COUNTER_NON_POSITIVE, zx::MonotonicInstant::INFINITE),
                zx::WaitResult::Ok(zx::Signals::COUNTER_NON_POSITIVE)
            );
        })
        .await;
    }

    #[fuchsia::test]
    async fn rescheduling_interval_timers_forbids_suspend() {
        spawn_kernel_and_run(async |_, current_task| {
            let (hrtimer_manager, counter) = init_hr_timer_manager(current_task, Response::Delayed);

            // Schedule an interval timer and let it expire.
            let timer1 = HrTimer::new();
            *timer1.is_interval.lock() = true;
            hrtimer_manager
                .add_timer(None, &timer1, zx::BootInstant::from_nanos(MAGIC_EXPIRE_DEADLINE).into())
                .unwrap();
            wait_signaled_sync(&timer1.event()).to_result().unwrap();

            // Schedule a regular timer and let it expire.
            let timer2 = HrTimer::new();
            hrtimer_manager
                .add_timer(None, &timer2, zx::BootInstant::from_nanos(MAGIC_EXPIRE_DEADLINE).into())
                .unwrap();
            wait_signaled_sync(&timer2.event()).to_result().unwrap();

            // When we have an expired but not rescheduled interval timer (`timer1`), and we have
            // an intervening timer that gets scheduled and expires (`timer2`) before `timer1` is
            // rescheduled, then suspend should be disallowed (counter > 0) to allow `timer1` to
            // be scheduled eventually.
            assert_eq!(
                counter.wait_handle(zx::Signals::COUNTER_POSITIVE, zx::MonotonicInstant::INFINITE),
                zx::WaitResult::Ok(zx::Signals::COUNTER_POSITIVE)
            );
        })
        .await;
    }

    #[fuchsia::test]
    async fn canceling_interval_timer_allows_suspend() {
        spawn_kernel_and_run(async |_, current_task| {
            let (hrtimer_manager, counter) = init_hr_timer_manager(current_task, Response::Delayed);

            let timer1 = HrTimer::new();
            *timer1.is_interval.lock() = true;
            hrtimer_manager
                .add_timer(None, &timer1, zx::BootInstant::from_nanos(MAGIC_EXPIRE_DEADLINE).into())
                .unwrap();
            wait_signaled_sync(&timer1.event()).to_result().unwrap();

            // When an interval timer expires, we should not be allowed to suspend.
            assert_eq!(
                counter.wait_handle(zx::Signals::COUNTER_POSITIVE, zx::MonotonicInstant::INFINITE),
                zx::WaitResult::Ok(zx::Signals::COUNTER_POSITIVE)
            );

            // Schedule the same timer again. This time around we do not wait for it to expire,
            // but cancel the timer instead.
            const DURATION_100S: zx::BootDuration = zx::BootDuration::from_seconds(100);
            let deadline2: zx::BootInstant = zx::BootInstant::after(DURATION_100S.into());
            hrtimer_manager.add_timer(None, &timer1, deadline2.into()).unwrap();

            hrtimer_manager.remove_timer(&timer1).unwrap();

            // When we cancel an interval timer, we should be allowed to suspend.
            assert_eq!(
                counter
                    .wait_handle(zx::Signals::COUNTER_NON_POSITIVE, zx::MonotonicInstant::INFINITE),
                zx::WaitResult::Ok(zx::Signals::COUNTER_NON_POSITIVE)
            );
        })
        .await;
    }

    #[fuchsia::test]
    async fn canceling_interval_timer_allows_suspend_with_flake() {
        spawn_kernel_and_run(async |_, current_task| {
            let (hrtimer_manager, counter) = init_hr_timer_manager(current_task, Response::Delayed);

            let timer1 = HrTimer::new();
            *timer1.is_interval.lock() = true;
            hrtimer_manager
                .add_timer(None, &timer1, zx::BootInstant::from_nanos(MAGIC_EXPIRE_DEADLINE).into())
                .unwrap();
            wait_signaled_sync(&timer1.event()).to_result().unwrap();

            assert_eq!(
                counter.wait_handle(zx::Signals::COUNTER_POSITIVE, zx::MonotonicInstant::INFINITE),
                zx::WaitResult::Ok(zx::Signals::COUNTER_POSITIVE)
            );
            const DURATION_100S: zx::BootDuration = zx::BootDuration::from_seconds(100);
            let deadline2: zx::BootInstant = zx::BootInstant::after(DURATION_100S.into());
            hrtimer_manager.add_timer(None, &timer1, deadline2.into()).unwrap();
            // No pause between start and stop has led to flakes before.
            hrtimer_manager.remove_timer(&timer1).unwrap();

            assert_eq!(
                counter
                    .wait_handle(zx::Signals::COUNTER_NON_POSITIVE, zx::MonotonicInstant::INFINITE),
                zx::WaitResult::Ok(zx::Signals::COUNTER_NON_POSITIVE)
            );
        })
        .await;
    }
}
