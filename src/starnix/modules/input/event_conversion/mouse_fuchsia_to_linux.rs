// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_ui_pointer::{MouseEvent as FidlMouseEvent, MousePointerSample};
use starnix_types::time::timeval_from_time;
use starnix_uapi::uapi;
use std::collections::VecDeque;

pub struct LinuxMouseEventBatch {
    pub events: VecDeque<uapi::input_event>,
    pub count_ignored_events: u64,
    pub count_converted_events: u64,
    pub count_unexpected_events: u64,
    pub last_event_time_ns: i64,
}

pub fn parse_fidl_mouse_events(mouse_events: Vec<FidlMouseEvent>) -> LinuxMouseEventBatch {
    let mut count_ignored_events: u64 = 0;
    let mut count_converted_events: u64 = 0;
    let mut count_unexpected_events: u64 = 0;
    let mut new_events: VecDeque<uapi::input_event> = VecDeque::new();
    let mut last_event_time_ns = zx::MonotonicInstant::get();

    for event in mouse_events {
        match event {
            FidlMouseEvent {
                timestamp: Some(time),
                pointer_sample: Some(MousePointerSample { scroll_v: Some(ticks), .. }),
                ..
            } => {
                last_event_time_ns = zx::MonotonicInstant::from_nanos(time);
                // Ensure this is a mouse wheel event with delta, otherwise ignore.
                if ticks != 0 {
                    new_events.push_back(uapi::input_event {
                        time: timeval_from_time(last_event_time_ns),
                        type_: uapi::EV_REL as u16,
                        code: uapi::REL_WHEEL as u16,
                        value: ticks as i32,
                    });
                    count_converted_events += 1;
                } else {
                    count_ignored_events += 1;
                }
            }
            _ => {
                count_unexpected_events += 1;
            }
        }
    }
    if !new_events.is_empty() {
        new_events.push_back(uapi::input_event {
            time: timeval_from_time(last_event_time_ns),
            type_: uapi::EV_SYN as u16,
            code: uapi::SYN_REPORT as u16,
            value: 0,
        });
    }

    LinuxMouseEventBatch {
        events: new_events,
        count_ignored_events,
        count_converted_events,
        count_unexpected_events,
        last_event_time_ns: last_event_time_ns.into_nanos(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_mouse_wheel_event() {
        let fidl_event = FidlMouseEvent {
            timestamp: Some(1000),
            pointer_sample: Some(MousePointerSample { scroll_v: Some(1), ..Default::default() }),
            ..Default::default()
        };
        let batch = parse_fidl_mouse_events(vec![fidl_event]);

        assert_eq!(batch.events.len(), 2);
        assert_eq!(batch.events[0].type_, uapi::EV_REL as u16);
        assert_eq!(batch.events[0].code, uapi::REL_WHEEL as u16);
        assert_eq!(batch.events[0].value, 1);
        assert_eq!(batch.events[1].type_, uapi::EV_SYN as u16);
        assert_eq!(batch.events[1].code, uapi::SYN_REPORT as u16);
        assert_eq!(batch.count_converted_events, 1);
        assert_eq!(batch.count_ignored_events, 0);
        assert_eq!(batch.count_unexpected_events, 0);
        assert_eq!(batch.last_event_time_ns, 1000);
    }

    #[test]
    fn test_mouse_wheel_zero_ticks() {
        let fidl_event = FidlMouseEvent {
            timestamp: Some(1000),
            pointer_sample: Some(MousePointerSample { scroll_v: Some(0), ..Default::default() }),
            ..Default::default()
        };
        let batch = parse_fidl_mouse_events(vec![fidl_event]);

        assert_eq!(batch.events.len(), 0);
        assert_eq!(batch.count_converted_events, 0);
        assert_eq!(batch.count_ignored_events, 1);
        assert_eq!(batch.count_unexpected_events, 0);
    }

    #[test]
    fn test_mouse_unexpected_event() {
        let fidl_event = FidlMouseEvent { timestamp: Some(1000), ..Default::default() };
        let batch = parse_fidl_mouse_events(vec![fidl_event]);

        assert_eq!(batch.events.len(), 0);
        assert_eq!(batch.count_converted_events, 0);
        assert_eq!(batch.count_ignored_events, 0);
        assert_eq!(batch.count_unexpected_events, 1);
    }
}
