// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::ioctl::WakeupTestType;
use fuchsia_trace::Scope;
use starnix_core::perf::{TraceEvent, TraceEventQueue};
use starnix_logging::{log_error, log_info};
use std::sync::Arc;
pub(crate) static POWER_CATEGORY: &str = "power";

pub(crate) fn trace_wakeup_test_type(
    trace_event_queue: Option<Arc<TraceEventQueue>>,
    current_tid: i32,
    test_type: WakeupTestType,
) {
    fuchsia_trace::instant!(
        POWER_CATEGORY,
        "WakeupTest:TestStart",
        Scope::Process,
        "type" =><WakeupTestType as Into<&'static str>>::into(test_type)
    );
    // TODO(479889862): Support custom tracepoints for these events.
    if let Some(trace_event_queue) = trace_event_queue {
        if trace_event_queue.is_enabled() {
            let evt = format!("I|{current_tid}|wakeup_test_type:{:?}\n", test_type);
            let data = evt.as_bytes();
            let event = TraceEvent::new(current_tid, data.len());
            if let Err(e) = trace_event_queue.push_event(event, data) {
                log_error!("failed to push trace event: {e:?}");
            }
        } else {
            log_info!("Trace event queue is not enabled, skipping wakeup set timer trace event");
        }
    }
}

pub(crate) fn trace_wakeup_set_timer(
    trace_event_queue: Option<Arc<TraceEventQueue>>,
    current_tid: i32,
    index: u32,
    time: i64,
) {
    fuchsia_trace::instant!(
        POWER_CATEGORY,
        "WakeupTest:TimerSet",
        Scope::Process,
        "index" => index,
        "time" => time
    );
    // TODO(479889862): Support custom tracepoints for these events.
    if let Some(trace_event_queue) = trace_event_queue {
        if trace_event_queue.is_enabled() {
            let evt = format!("I|{current_tid}|index:{index}, time:{time}\n");
            let data = evt.as_bytes();
            let event = TraceEvent::new(current_tid, data.len());
            if let Err(e) = trace_event_queue.push_event(event, data) {
                log_error!("failed to push trace event: {e:?}");
            }
        } else {
            log_info!("Trace event queue is not enabled, skipping wakeup set timer trace event");
        }
    }
}

#[allow(dead_code)]
fn trace_wakeup_send_key() {
    log_error!("todo: implement trace send key b/479889862)")
}

#[allow(dead_code)]
fn trace_wakeup_send_touch() {
    log_error!("todo: implement trace send touch b/479889862)")
}

#[allow(dead_code)]
fn trace_wakeup_send_swipe() {
    log_error!("todo: implement trace send swipe b/479889862)")
}
