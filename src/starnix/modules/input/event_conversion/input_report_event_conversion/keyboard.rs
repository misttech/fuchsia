// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_input_report as fir;
use sorted_vec_map::SortedVecSet;
use starnix_modules_input_app_event_conversion::keymap::KEY_MAP;
use starnix_types::time::timeval_from_time;
use starnix_uapi::uapi;

/// Converts `KeyboardInputReport` events into Linux `uapi::input_event`s (EV_KEY press and release).
#[derive(Debug, Default, PartialEq)]
pub struct KeyboardReportConverter {
    pressed_keys: SortedVecSet<u16>,
}

impl KeyboardReportConverter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn handle(
        &mut self,
        report: &fir::KeyboardInputReport,
        event_time: zx::MonotonicInstant,
    ) -> Vec<uapi::input_event> {
        let mut events = Vec::new();
        let time = timeval_from_time(event_time);

        let pressed_keys3 = report.pressed_keys3.as_deref().unwrap_or(&[]);
        let mut current_keys = SortedVecSet::new();
        for &key in pressed_keys3.iter() {
            let lkey = KEY_MAP.fuchsia_input_key_to_linux_keycode(key);
            if lkey != uapi::KEY_RESERVED {
                current_keys.insert(lkey as u16);
            }
        }

        crate::emit_key_transitions(&mut self.pressed_keys, current_keys, time, &mut events);

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
    use fidl_fuchsia_input::Key;

    #[::fuchsia::test]
    fn test_keyboard_report_keys() {
        let mut converter = KeyboardReportConverter::new();
        let time = zx::MonotonicInstant::from_nanos(500_000_000);

        // Press Key A
        let report1 =
            fir::KeyboardInputReport { pressed_keys3: Some(vec![Key::A]), ..Default::default() };
        let events1 = converter.handle(&report1, time);
        assert_eq!(
            events1.iter().map(|e| (e.type_, e.code, e.value)).collect::<Vec<_>>(),
            vec![
                (uapi::EV_KEY as u16, uapi::KEY_A as u16, 1),
                (uapi::EV_SYN as u16, uapi::SYN_REPORT as u16, 0),
            ]
        );

        // Hold Key A and press Key B
        let report2 = fir::KeyboardInputReport {
            pressed_keys3: Some(vec![Key::A, Key::B]),
            ..Default::default()
        };
        let events2 = converter.handle(&report2, time);
        assert_eq!(
            events2.iter().map(|e| (e.type_, e.code, e.value)).collect::<Vec<_>>(),
            vec![
                (uapi::EV_KEY as u16, uapi::KEY_B as u16, 1),
                (uapi::EV_SYN as u16, uapi::SYN_REPORT as u16, 0),
            ]
        );

        // Release Key A, hold Key B
        let report3 =
            fir::KeyboardInputReport { pressed_keys3: Some(vec![Key::B]), ..Default::default() };
        let events3 = converter.handle(&report3, time);
        assert_eq!(
            events3.iter().map(|e| (e.type_, e.code, e.value)).collect::<Vec<_>>(),
            vec![
                (uapi::EV_KEY as u16, uapi::KEY_A as u16, 0),
                (uapi::EV_SYN as u16, uapi::SYN_REPORT as u16, 0),
            ]
        );

        // Release all
        let report4 =
            fir::KeyboardInputReport { pressed_keys3: Some(vec![]), ..Default::default() };
        let events4 = converter.handle(&report4, time);
        assert_eq!(
            events4.iter().map(|e| (e.type_, e.code, e.value)).collect::<Vec<_>>(),
            vec![
                (uapi::EV_KEY as u16, uapi::KEY_B as u16, 0),
                (uapi::EV_SYN as u16, uapi::SYN_REPORT as u16, 0),
            ]
        );
    }

    #[::fuchsia::test]
    fn test_keyboard_report_unmapped_key_filtered() {
        let mut converter = KeyboardReportConverter::new();
        let time = zx::MonotonicInstant::from_nanos(300_000_000);

        let report = fir::KeyboardInputReport {
            pressed_keys3: Some(vec![Key::Unknown]),
            ..Default::default()
        };
        let events = converter.handle(&report, time);
        // Key::Unknown maps to KEY_RESERVED, which is filtered out, yielding no events
        assert!(events.is_empty());
    }

    #[::fuchsia::test]
    fn test_empty_keyboard_reports_return_empty_events() {
        let time = zx::MonotonicInstant::from_nanos(400_000_000);
        assert!(
            KeyboardReportConverter::new()
                .handle(&fir::KeyboardInputReport::default(), time)
                .is_empty()
        );
    }

    #[::fuchsia::test]
    fn test_keyboard_report_none_pressed_keys_releases_held_keys() {
        let mut converter = KeyboardReportConverter::new();
        let time = zx::MonotonicInstant::from_nanos(400_000_000);

        // Press Key A
        let report1 =
            fir::KeyboardInputReport { pressed_keys3: Some(vec![Key::A]), ..Default::default() };
        let events1 = converter.handle(&report1, time);
        assert_eq!(
            events1.iter().map(|e| (e.type_, e.code, e.value)).collect::<Vec<_>>(),
            vec![
                (uapi::EV_KEY as u16, uapi::KEY_A as u16, 1),
                (uapi::EV_SYN as u16, uapi::SYN_REPORT as u16, 0),
            ]
        );

        // Subsequent report with pressed_keys3: None releases Key A
        let report2 = fir::KeyboardInputReport { pressed_keys3: None, ..Default::default() };
        let events2 = converter.handle(&report2, time);
        assert_eq!(
            events2.iter().map(|e| (e.type_, e.code, e.value)).collect::<Vec<_>>(),
            vec![
                (uapi::EV_KEY as u16, uapi::KEY_A as u16, 0),
                (uapi::EV_SYN as u16, uapi::SYN_REPORT as u16, 0),
            ]
        );

        // Subsequent report with pressed_keys3: None when no keys held emits nothing
        let report3 = fir::KeyboardInputReport { pressed_keys3: None, ..Default::default() };
        let events3 = converter.handle(&report3, time);
        assert!(events3.is_empty());
    }
}
