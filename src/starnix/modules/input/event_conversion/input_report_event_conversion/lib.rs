// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod consumer_control;
pub mod keyboard;
pub mod mouse;
pub mod touch;

pub use consumer_control::*;
pub use keyboard::*;
pub use mouse::*;
pub use touch::*;

use fidl_fuchsia_input_report as fir;
use starnix_uapi::uapi;

#[inline]
pub(crate) fn clamp_i64_to_i32(val: i64) -> i32 {
    i32::try_from(val).unwrap_or_else(|_| val.clamp(i32::MIN as i64, i32::MAX as i64) as i32)
}

pub(crate) fn emit_key_transitions(
    previous: &mut sorted_vec_map::SortedVecSet<u16>,
    current: sorted_vec_map::SortedVecSet<u16>,
    time: uapi::timeval,
    events: &mut Vec<uapi::input_event>,
) {
    for &key in previous.iter() {
        if !current.contains(&key) {
            events.push(uapi::input_event {
                time,
                type_: uapi::EV_KEY as u16,
                code: key,
                value: 0,
            });
        }
    }
    for &key in current.iter() {
        if !previous.contains(&key) {
            events.push(uapi::input_event {
                time,
                type_: uapi::EV_KEY as u16,
                code: key,
                value: 1,
            });
        }
    }
    *previous = current;
}

/// Unified converter translating any `fuchsia.input.report.InputReport` into Linux `uapi::input_event`s.
#[derive(Debug, Default, PartialEq)]
pub struct InputReportConverter {
    pub touch: TouchReportConverter,
    pub mouse: MouseReportConverter,
    pub keyboard: KeyboardReportConverter,
    pub consumer_control: ConsumerControlReportConverter,
}

impl InputReportConverter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_device_descriptor(descriptor: &fir::DeviceDescriptor) -> Self {
        let touch = descriptor
            .touch
            .as_ref()
            .map(TouchReportConverter::from_touch_descriptor)
            .unwrap_or_default();
        Self { touch, ..Default::default() }
    }

    pub fn handle_report(&mut self, report: &fir::InputReport) -> Vec<uapi::input_event> {
        let event_time = report
            .event_time
            .filter(|&t| t > 0)
            .map(zx::MonotonicInstant::from_nanos)
            .unwrap_or_else(zx::MonotonicInstant::get);

        let mut events = Vec::new();

        if let Some(ref touch) = report.touch {
            events.extend(self.touch.handle(touch, event_time));
        }

        if let Some(ref mouse) = report.mouse {
            events.extend(self.mouse.handle(mouse, event_time));
        }

        if let Some(ref keyboard) = report.keyboard {
            events.extend(self.keyboard.handle(keyboard, event_time));
        }

        if let Some(ref consumer_control) = report.consumer_control {
            events.extend(self.consumer_control.handle(consumer_control, event_time));
        }

        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl_fuchsia_input::Key;

    #[::fuchsia::test]
    fn test_unified_input_report_converter() {
        let mut converter = InputReportConverter::new();
        let time_nanos = 700_000_000;

        let report = fir::InputReport {
            event_time: Some(time_nanos),
            keyboard: Some(fir::KeyboardInputReport {
                pressed_keys3: Some(vec![Key::Escape]),
                ..Default::default()
            }),
            mouse: Some(fir::MouseInputReport {
                movement_x: Some(10),
                movement_y: Some(-5),
                ..Default::default()
            }),
            ..Default::default()
        };

        let events = converter.handle_report(&report);
        assert!(events.iter().any(|e| e.code == uapi::KEY_ESC as u16 && e.value == 1));
        assert!(events.iter().any(|e| e.code == uapi::REL_X as u16 && e.value == 10));
        assert!(events.iter().any(|e| e.code == uapi::REL_Y as u16 && e.value == -5));
        assert_eq!(events[0].time.tv_sec, 0);
        assert_eq!(events[0].time.tv_usec, (time_nanos / 1_000) as i64);
    }

    #[::fuchsia::test]
    fn test_empty_reports_return_empty_events() {
        assert!(InputReportConverter::new().handle_report(&fir::InputReport::default()).is_empty());
    }

    #[::fuchsia::test]
    fn test_event_time_filtering() {
        // Report with event_time = 0
        let mut converter_zero = InputReportConverter::new();
        let report_zero = fir::InputReport {
            event_time: Some(0),
            keyboard: Some(fir::KeyboardInputReport {
                pressed_keys3: Some(vec![Key::Escape]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let events_zero = converter_zero.handle_report(&report_zero);
        assert!(!events_zero.is_empty());
        assert!(events_zero[0].time.tv_sec > 0 || events_zero[0].time.tv_usec > 0);

        // Report with event_time = -100
        let mut converter_neg = InputReportConverter::new();
        let report_neg = fir::InputReport {
            event_time: Some(-100),
            keyboard: Some(fir::KeyboardInputReport {
                pressed_keys3: Some(vec![Key::Escape]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let events_neg = converter_neg.handle_report(&report_neg);
        assert!(!events_neg.is_empty());
        assert!(events_neg[0].time.tv_sec > 0 || events_neg[0].time.tv_usec > 0);
        assert!(events_neg[0].time.tv_usec >= 0);
    }

    #[::fuchsia::test]
    fn test_clamp_i64_to_i32() {
        assert_eq!(clamp_i64_to_i32(0), 0);
        assert_eq!(clamp_i64_to_i32(42), 42);
        assert_eq!(clamp_i64_to_i32(-42), -42);
        assert_eq!(clamp_i64_to_i32(i32::MAX as i64), i32::MAX);
        assert_eq!(clamp_i64_to_i32(i32::MIN as i64), i32::MIN);
        assert_eq!(clamp_i64_to_i32(i32::MAX as i64 + 100), i32::MAX);
        assert_eq!(clamp_i64_to_i32(i32::MIN as i64 - 100), i32::MIN);
        assert_eq!(clamp_i64_to_i32(i64::MAX), i32::MAX);
        assert_eq!(clamp_i64_to_i32(i64::MIN), i32::MIN);
    }

    #[::fuchsia::test]
    fn test_input_report_converter_from_device_descriptor() {
        let desc = fir::DeviceDescriptor {
            touch: Some(fir::TouchDescriptor {
                input: Some(fir::TouchInputDescriptor {
                    max_contacts: Some(7),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let converter = InputReportConverter::from_device_descriptor(&desc);
        assert_eq!(converter.touch.max_slots(), 7);

        let desc_empty = fir::DeviceDescriptor::default();
        let converter_empty = InputReportConverter::from_device_descriptor(&desc_empty);
        assert_eq!(converter_empty.touch.max_slots(), 10);
    }
}
