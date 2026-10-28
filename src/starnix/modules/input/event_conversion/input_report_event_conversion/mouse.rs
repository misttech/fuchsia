// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_input_report as fir;
use sorted_vec_map::SortedVecSet;
use starnix_types::time::timeval_from_time;
use starnix_uapi::uapi;

/// Maps a Fuchsia mouse button ID to a Linux input event keycode.
pub fn mouse_button_to_linux_button(button: u8) -> Option<u32> {
    match button {
        1 => Some(uapi::BTN_LEFT),
        2 => Some(uapi::BTN_RIGHT),
        3 => Some(uapi::BTN_MIDDLE),
        4 => Some(uapi::BTN_SIDE),
        5 => Some(uapi::BTN_EXTRA),
        6 => Some(uapi::BTN_FORWARD),
        7 => Some(uapi::BTN_BACK),
        8 => Some(uapi::BTN_TASK),
        _ => None,
    }
}

/// Converts `MouseInputReport` events into Linux relative motion and button `uapi::input_event`s.
#[derive(Debug, Default, PartialEq)]
pub struct MouseReportConverter {
    pressed_buttons: SortedVecSet<u16>,
}

impl MouseReportConverter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn handle(
        &mut self,
        report: &fir::MouseInputReport,
        event_time: zx::MonotonicInstant,
    ) -> Vec<uapi::input_event> {
        let mut events = Vec::new();
        let time = timeval_from_time(event_time);

        if let Some(movement_x) = report.movement_x {
            if movement_x != 0 {
                events.push(uapi::input_event {
                    time,
                    type_: uapi::EV_REL as u16,
                    code: uapi::REL_X as u16,
                    value: crate::clamp_i64_to_i32(movement_x),
                });
            }
        }

        if let Some(movement_y) = report.movement_y {
            if movement_y != 0 {
                events.push(uapi::input_event {
                    time,
                    type_: uapi::EV_REL as u16,
                    code: uapi::REL_Y as u16,
                    value: crate::clamp_i64_to_i32(movement_y),
                });
            }
        }

        if let Some(scroll_v) = report.scroll_v {
            if scroll_v != 0 {
                events.push(uapi::input_event {
                    time,
                    type_: uapi::EV_REL as u16,
                    code: uapi::REL_WHEEL as u16,
                    value: crate::clamp_i64_to_i32(scroll_v),
                });
            }
        }

        if let Some(scroll_h) = report.scroll_h {
            if scroll_h != 0 {
                events.push(uapi::input_event {
                    time,
                    type_: uapi::EV_REL as u16,
                    code: uapi::REL_HWHEEL as u16,
                    value: crate::clamp_i64_to_i32(scroll_h),
                });
            }
        }

        let pressed_buttons = report.pressed_buttons.as_deref().unwrap_or(&[]);
        let mut current_buttons = SortedVecSet::new();
        for &btn in pressed_buttons.iter() {
            if let Some(linux_btn) = mouse_button_to_linux_button(btn) {
                current_buttons.insert(linux_btn as u16);
            }
        }

        crate::emit_key_transitions(&mut self.pressed_buttons, current_buttons, time, &mut events);

        if !events.is_empty() {
            events.push(uapi::input_event {
                time,
                type_: uapi::EV_SYN as u16,
                code: uapi::SYN_REPORT as u16,
                value: 0,
            });
        }

        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::fuchsia::test]
    fn test_mouse_report_motion_and_buttons() {
        let mut converter = MouseReportConverter::new();
        let time = zx::MonotonicInstant::from_nanos(400_000_000);

        // Relative move + scroll + Left button down
        let report = fir::MouseInputReport {
            movement_x: Some(15),
            movement_y: Some(-25),
            scroll_v: Some(3),
            scroll_h: Some(-1),
            pressed_buttons: Some(vec![1]),
            ..Default::default()
        };

        let events = converter.handle(&report, time);
        assert_eq!(
            events.iter().map(|e| (e.type_, e.code, e.value)).collect::<Vec<_>>(),
            vec![
                (uapi::EV_REL as u16, uapi::REL_X as u16, 15),
                (uapi::EV_REL as u16, uapi::REL_Y as u16, -25),
                (uapi::EV_REL as u16, uapi::REL_WHEEL as u16, 3),
                (uapi::EV_REL as u16, uapi::REL_HWHEEL as u16, -1),
                (uapi::EV_KEY as u16, uapi::BTN_LEFT as u16, 1),
                (uapi::EV_SYN as u16, uapi::SYN_REPORT as u16, 0),
            ]
        );

        // Left button up + Right button down
        let report2 =
            fir::MouseInputReport { pressed_buttons: Some(vec![2]), ..Default::default() };
        let events2 = converter.handle(&report2, time);
        assert_eq!(
            events2.iter().map(|e| (e.type_, e.code, e.value)).collect::<Vec<_>>(),
            vec![
                (uapi::EV_KEY as u16, uapi::BTN_LEFT as u16, 0),
                (uapi::EV_KEY as u16, uapi::BTN_RIGHT as u16, 1),
                (uapi::EV_SYN as u16, uapi::SYN_REPORT as u16, 0),
            ]
        );
    }

    #[::fuchsia::test]
    fn test_mouse_report_clamping_out_of_range_axes() {
        let mut converter = MouseReportConverter::new();
        let time = zx::MonotonicInstant::from_nanos(100_000_000);
        let report = fir::MouseInputReport {
            movement_x: Some(i64::MAX),
            movement_y: Some(i64::MIN),
            scroll_v: Some(i64::MAX),
            scroll_h: Some(i64::MIN),
            ..Default::default()
        };
        let events = converter.handle(&report, time);
        assert!(events.iter().any(|e| e.code == uapi::REL_X as u16 && e.value == i32::MAX));
        assert!(events.iter().any(|e| e.code == uapi::REL_Y as u16 && e.value == i32::MIN));
        assert!(events.iter().any(|e| e.code == uapi::REL_WHEEL as u16 && e.value == i32::MAX));
        assert!(events.iter().any(|e| e.code == uapi::REL_HWHEEL as u16 && e.value == i32::MIN));
    }

    #[::fuchsia::test]
    fn test_empty_mouse_reports_return_empty_events() {
        let time = zx::MonotonicInstant::from_nanos(400_000_000);
        assert!(
            MouseReportConverter::new().handle(&fir::MouseInputReport::default(), time).is_empty()
        );
    }

    #[::fuchsia::test]
    fn test_mouse_report_none_pressed_buttons_releases_held_buttons() {
        let mut converter = MouseReportConverter::new();
        let time = zx::MonotonicInstant::from_nanos(300_000_000);

        // Press Left button
        let report1 =
            fir::MouseInputReport { pressed_buttons: Some(vec![1]), ..Default::default() };
        let events1 = converter.handle(&report1, time);
        assert_eq!(
            events1.iter().map(|e| (e.type_, e.code, e.value)).collect::<Vec<_>>(),
            vec![
                (uapi::EV_KEY as u16, uapi::BTN_LEFT as u16, 1),
                (uapi::EV_SYN as u16, uapi::SYN_REPORT as u16, 0),
            ]
        );

        // Subsequent report with pressed_buttons: None should release Left button
        let report2 = fir::MouseInputReport { pressed_buttons: None, ..Default::default() };
        let events2 = converter.handle(&report2, time);
        assert_eq!(
            events2.iter().map(|e| (e.type_, e.code, e.value)).collect::<Vec<_>>(),
            vec![
                (uapi::EV_KEY as u16, uapi::BTN_LEFT as u16, 0),
                (uapi::EV_SYN as u16, uapi::SYN_REPORT as u16, 0),
            ]
        );

        // Another report with pressed_buttons: None when no buttons held should emit nothing
        let report3 = fir::MouseInputReport { pressed_buttons: None, ..Default::default() };
        let events3 = converter.handle(&report3, time);
        assert!(events3.is_empty());
    }
}
