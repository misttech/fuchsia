// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_power_observability as fobs;
use fuchsia_inspect::{ArrayProperty, LazyNode as ILazyNode, Node as INode};
use fuchsia_sync::Mutex;
use futures::FutureExt;
use state_recorder::{EnumStateRecorder, RecorderOptions};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use strum_macros::{Display, EnumIter};

const SUSPEND_EVENT_BUFFER_SIZE: usize = 6144;

static INSPECT_FIELD_EVENT_CAPACITY: &str = "event_capacity";
static INSPECT_FIELD_HISTORY_DURATION: &str = "history_duration_seconds";
static INSPECT_FIELD_HISTORY_DURATION_WHEN_FULL: &str = "at_capacity_history_duration_seconds";

/// An event logged by system-activity-governor.
#[derive(Clone, Debug)]
pub enum SagEvent {
    /// Suspend is being attempted.
    SuspendAttempted,
    /// Suspend was entered and exited successfully and the system is resuming.
    SuspendResumed { suspend_duration: i64, cumulative_duration: i64 },
    /// Suspend attempt was requested but is not allowed due to an unmet
    /// precondition, e.g. active wake leases, CPU power element is active.
    SuspendAttemptBlocked,
    /// Suspend attempt failed.
    SuspendFailed,
    /// A suspend blocker has been acquired, so suspend is blocked.
    SuspendBlockerAcquired,
    /// A suspend blocker has been dropped, so suspend is no longer blocked.
    SuspendBlockerDropped,
    /// A suspend lock has been acquired, so an uninterruptible suspend attempt
    /// is imminent.
    SuspendLockAcquired,
    /// A suspend lock has been dropped, so a suspend attempt has completed.
    SuspendLockDropped,
    /// A wake lease was created.
    WakeLeaseCreated { name: String, id: u64 },
    /// The underlying power broker lease for a wake lease failed to be satisfied.
    WakeLeaseSatisfactionFailed { name: String, id: u64, error: String },
    /// The underlying power broker lease for a wake lease was satisfied.
    WakeLeaseSatisfied { name: String, id: u64 },
    /// A wake lease was dropped and is no longer active.
    WakeLeaseDropped { name: String, id: u64 },
    /// Reported reasons of the last wake, or prevented sleep.
    WakeReasons { reasons: Vec<String> },
    /// Suspend callback processing started.
    SuspendCallbackPhaseStarted,
    /// Suspend callback processing ended.
    SuspendCallbackPhaseEnded,
    /// Resume callback processing started.
    ResumeCallbackPhaseStarted,
    /// Resume callback processing ended.
    ResumeCallbackPhaseEnded,
}

/// The state of the system with respect to suspend.
#[derive(Clone, Copy, Debug, Display, Eq, Hash, PartialEq, EnumIter)]
#[repr(u8)]
pub enum SystemSuspendState {
    Suspended = 0,
    Active = 1,
}

impl From<SystemSuspendState> for u64 {
    fn from(s: SystemSuspendState) -> u64 {
        s as u64
    }
}

#[derive(Clone, Debug)]
struct SagWakeLeaseEvent {
    event_number: u64,    // Event number
    event_log_time: i64,  // Timestamp for the event
    event_info: SagEvent, // The event itself
}

/// A logger for SagEvent objects that inserts the event into a circular buffer
/// in inspect.
#[derive(Clone, Debug)]
pub struct SagEventLogger {
    /// Internal ring buffer for event logging
    internal_event_log: Arc<Mutex<VecDeque<SagWakeLeaseEvent>>>,
    internal_event_number: Arc<Mutex<u64>>,

    /// Inspect node that tracks wall-time history duration.
    /// Schema follows Power Broker's topology stats:
    ///   event_capacity: u64
    ///   history_duration_seconds: i64
    ///   at_capacity_history_duration_seconds: i64
    _event_log_stats: Rc<RefCell<ILazyNode>>,

    /// Inspect node that reports internally-logged wake lease events
    _internal_event_log_stats: Rc<RefCell<ILazyNode>>,

    /// Total time that the device has spent suspended since boot (or at least
    /// since this logger was created), in nanoseconds.
    cumulative_suspend_duration: Arc<AtomicI64>,

    /// State recorder for `SystemSuspendState`.
    system_suspend_state: Arc<Mutex<EnumStateRecorder<SystemSuspendState>>>,
}

impl SagEventLogger {
    pub fn new(node: &INode) -> Self {
        let internal_event_log = Arc::new(Mutex::new(
            VecDeque::<SagWakeLeaseEvent>::with_capacity(SUSPEND_EVENT_BUFFER_SIZE),
        ));

        let weak_arc_of_internal_event_log = Arc::downgrade(&internal_event_log);

        // Create inspect node for logging suspend events. Events are stored in an internal ring
        // buffer and lazily converted/logged to Inspect to reduce Inspect memory usage.
        let value = weak_arc_of_internal_event_log.clone();
        let internal_event_log_stats =
            node.create_lazy_child(fobs::SUSPEND_EVENTS_NODE, move || {
                let weak_internal_log = value.clone();

                async move {
                    let inspector = fuchsia_inspect::Inspector::default();
                    let root = inspector.root();

                    // Convert internally-logged events into Inspect nodes
                    if let Some(internal_event_log) = weak_internal_log.upgrade() {
                        let events = internal_event_log.lock();

                        for internal_event in events.iter() {
                            let time = internal_event.event_log_time;
                            let event = internal_event.event_info.clone();

                            root.record_child(internal_event.event_number.to_string(), |root| {
                                match event {
                                    SagEvent::SuspendAttempted => {
                                        root.record_int(fobs::SUSPEND_ATTEMPTED_AT, time);
                                    }
                                    SagEvent::SuspendResumed {
                                        suspend_duration,
                                        cumulative_duration,
                                    } => {
                                        root.record_int(fobs::SUSPEND_RESUMED_AT, time);
                                        root.record_int(
                                            fobs::SUSPEND_LAST_TIMESTAMP,
                                            suspend_duration,
                                        );
                                        root.record_int(
                                            fobs::SUSPEND_CUMULATIVE_DURATION,
                                            cumulative_duration,
                                        );
                                    }
                                    SagEvent::SuspendFailed => {
                                        root.record_int(fobs::SUSPEND_FAILED_AT, time);
                                    }
                                    SagEvent::SuspendAttemptBlocked => {
                                        root.record_int(fobs::SUSPEND_ATTEMPT_BLOCKED_AT, time);
                                    }
                                    SagEvent::SuspendBlockerAcquired => {
                                        root.record_int(fobs::SUSPEND_BLOCKER_ACQUIRED_AT, time);
                                    }
                                    SagEvent::SuspendBlockerDropped => {
                                        root.record_int(fobs::SUSPEND_BLOCKER_DROPPED_AT, time);
                                    }
                                    SagEvent::SuspendLockAcquired => {
                                        root.record_int(fobs::SUSPEND_LOCK_ACQUIRED_AT, time);
                                    }
                                    SagEvent::SuspendLockDropped => {
                                        root.record_int(fobs::SUSPEND_LOCK_DROPPED_AT, time);
                                    }
                                    SagEvent::WakeLeaseCreated { name, id } => {
                                        root.record_int(fobs::WAKE_LEASE_CREATED_AT, time);
                                        root.record_uint(fobs::WAKE_LEASE_ITEM_ID, id);
                                        root.record_string(fobs::WAKE_LEASE_ITEM_NAME, name);
                                    }
                                    SagEvent::WakeLeaseSatisfactionFailed { name, id, error } => {
                                        root.record_int(
                                            fobs::WAKE_LEASE_SATISFACTION_FAILED_AT,
                                            time,
                                        );
                                        root.record_uint(fobs::WAKE_LEASE_ITEM_ID, id);
                                        root.record_string(fobs::WAKE_LEASE_ITEM_NAME, name);
                                        root.record_string(fobs::WAKE_LEASE_ITEM_ERROR, error);
                                    }
                                    SagEvent::WakeLeaseSatisfied { name, id } => {
                                        root.record_int(fobs::WAKE_LEASE_SATISFIED_AT, time);
                                        root.record_uint(fobs::WAKE_LEASE_ITEM_ID, id);
                                        root.record_string(fobs::WAKE_LEASE_ITEM_NAME, name);
                                    }
                                    SagEvent::WakeLeaseDropped { name, id } => {
                                        root.record_int(fobs::WAKE_LEASE_DROPPED_AT, time);
                                        root.record_uint(fobs::WAKE_LEASE_ITEM_ID, id);
                                        root.record_string(fobs::WAKE_LEASE_ITEM_NAME, name);
                                    }
                                    SagEvent::SuspendCallbackPhaseStarted => {
                                        root.record_int(
                                            fobs::SUSPEND_CALLBACK_PHASE_START_AT,
                                            time,
                                        );
                                    }
                                    SagEvent::SuspendCallbackPhaseEnded => {
                                        root.record_int(fobs::SUSPEND_CALLBACK_PHASE_END_AT, time);
                                    }
                                    SagEvent::ResumeCallbackPhaseStarted => {
                                        root.record_int(fobs::RESUME_CALLBACK_PHASE_START_AT, time);
                                    }
                                    SagEvent::ResumeCallbackPhaseEnded => {
                                        root.record_int(fobs::RESUME_CALLBACK_PHASE_END_AT, time);
                                    }
                                    SagEvent::WakeReasons { reasons } => {
                                        root.record_int(fobs::WAKE_REASONS_REPORTED_AT, time);
                                        let reason_array = root.create_string_array(
                                            fobs::WAKE_REASONS_WAKE_VECTOR_PREFIX,
                                            reasons.len(),
                                        );
                                        reasons.iter().enumerate().for_each(|(i, reason)| {
                                            reason_array.set(i, reason);
                                        });
                                        root.record(reason_array);
                                    }
                                };
                            });
                        }
                    }
                    Ok(inspector)
                }
                .boxed()
            });

        // Create Inspect node for suspend event stats
        let event_log_stats = node.create_lazy_child("suspend_events_stats", move || {
            let weak_internal_log = weak_arc_of_internal_event_log.clone();

            async move {
                let inspector = fuchsia_inspect::Inspector::default();
                let root = inspector.root();

                root.record_uint(INSPECT_FIELD_EVENT_CAPACITY, SUSPEND_EVENT_BUFFER_SIZE as u64);

                if let Some(internal_event_log) = weak_internal_log.upgrade() {
                    let timestamps = internal_event_log.lock();

                    if !timestamps.is_empty() {
                        let head_ns = timestamps.front().unwrap().event_log_time;
                        let tail_ns = timestamps.back().unwrap().event_log_time;
                        let duration =
                            zx::BootDuration::from_nanos(tail_ns - head_ns).into_seconds();
                        root.record_int(INSPECT_FIELD_HISTORY_DURATION, duration);

                        if timestamps.len() == SUSPEND_EVENT_BUFFER_SIZE {
                            root.record_int(INSPECT_FIELD_HISTORY_DURATION_WHEN_FULL, duration);
                        }
                    } else {
                        root.record_int(INSPECT_FIELD_HISTORY_DURATION, 0i64);
                    }
                }
                Ok(inspector)
            }
            .boxed()
        });

        // Create the system_suspend_state recorder
        let system_suspend_state = Arc::new(Mutex::new(
            EnumStateRecorder::new(
                "system_suspend_state".to_string(),
                c"power",
                // Current capacity would cover 3 hours with one suspend/resume cycle per
                // minute.
                RecorderOptions { lazy_record: true, capacity: 360, ..Default::default() },
            )
            .expect("Failed to create system_suspend_state recorder"),
        ));
        system_suspend_state.lock().record(SystemSuspendState::Active);

        Self {
            internal_event_log,
            internal_event_number: Arc::new(Mutex::new(0u64)),
            _internal_event_log_stats: Rc::new(RefCell::new(internal_event_log_stats)),
            _event_log_stats: Rc::new(RefCell::new(event_log_stats)),
            cumulative_suspend_duration: Arc::new(AtomicI64::new(0)),
            system_suspend_state,
        }
    }

    pub fn update_cumulative_suspend_duration(&self, suspend_duration: i64) -> i64 {
        let new_cumulative =
            self.cumulative_suspend_duration.fetch_add(suspend_duration, Ordering::SeqCst)
                + suspend_duration;
        new_cumulative
    }

    pub fn log(&self, event: SagEvent) {
        // Log event to internal ring buffer
        {
            let time = zx::BootInstant::get().into_nanos();
            let mut internal_events = self.internal_event_log.lock();
            let mut event_number = self.internal_event_number.lock();

            if internal_events.len() == SUSPEND_EVENT_BUFFER_SIZE {
                internal_events.pop_front();
            }
            internal_events.push_back(SagWakeLeaseEvent {
                event_number: *event_number,
                event_log_time: time,
                event_info: event.clone(),
            });

            *event_number += 1;
        }

        // For Suspend events, additionally update the logged suspend state
        match event {
            SagEvent::SuspendAttempted => {
                self.system_suspend_state.lock().record(SystemSuspendState::Suspended);
            }
            SagEvent::SuspendResumed { suspend_duration: _, cumulative_duration: _ } => {
                self.system_suspend_state.lock().record(SystemSuspendState::Active);
            }
            SagEvent::SuspendFailed => {
                self.system_suspend_state.lock().record(SystemSuspendState::Active);
            }
            _ => {} // Ignore other events
        };
    }
}
