// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_input_report as fir;
use sorted_vec_map::SortedVecSet;
use starnix_types::time::timeval_from_time;
use starnix_uapi::uapi;

/// Maps a Fuchsia `ConsumerControlButton` to a Linux keycode.
pub fn consumer_control_button_to_linux_key(button: fir::ConsumerControlButton) -> Option<u32> {
    match button {
        fir::ConsumerControlButton::VolumeUp => Some(uapi::KEY_VOLUMEUP),
        fir::ConsumerControlButton::VolumeDown => Some(uapi::KEY_VOLUMEDOWN),
        fir::ConsumerControlButton::Power => Some(uapi::KEY_POWER),
        fir::ConsumerControlButton::Pause => Some(uapi::KEY_PAUSE),
        fir::ConsumerControlButton::MicMute => Some(uapi::KEY_MICMUTE),
        fir::ConsumerControlButton::Reboot => Some(uapi::KEY_RESTART),
        fir::ConsumerControlButton::CameraDisable => Some(uapi::KEY_CAMERA),
        fir::ConsumerControlButton::Function => Some(uapi::KEY_VOLUMEDOWN),
        _ => None,
    }
}

/// Converts `ConsumerControlInputReport` events into Linux button `uapi::input_event`s.
#[derive(Debug, Default, PartialEq)]
pub struct ConsumerControlReportConverter {
    pressed_buttons: SortedVecSet<u16>,
}

impl ConsumerControlReportConverter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn handle(
        &mut self,
        report: &fir::ConsumerControlInputReport,
        event_time: zx::MonotonicInstant,
    ) -> Vec<uapi::input_event> {
        let mut events = Vec::new();
        let time = timeval_from_time(event_time);

        let pressed_buttons = report.pressed_buttons.as_deref().unwrap_or(&[]);
        let mut current_buttons = SortedVecSet::new();
        for &btn in pressed_buttons.iter() {
            if let Some(lkey) = consumer_control_button_to_linux_key(btn) {
                current_buttons.insert(lkey as u16);
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
    fn test_consumer_control_report_buttons() {
        let mut converter = ConsumerControlReportConverter::new();
        let time = zx::MonotonicInstant::from_nanos(600_000_000);

        // Press VolumeUp + Power
        let report1 = fir::ConsumerControlInputReport {
            pressed_buttons: Some(vec![
                fir::ConsumerControlButton::VolumeUp,
                fir::ConsumerControlButton::Power,
            ]),
            ..Default::default()
        };
        let events1 = converter.handle(&report1, time);
        assert!(events1.iter().any(|e| e.code == uapi::KEY_VOLUMEUP as u16 && e.value == 1));
        assert!(events1.iter().any(|e| e.code == uapi::KEY_POWER as u16 && e.value == 1));
        assert!(events1.iter().any(|e| e.code == uapi::SYN_REPORT as u16));

        // Release VolumeUp, press VolumeDown
        let report2 = fir::ConsumerControlInputReport {
            pressed_buttons: Some(vec![
                fir::ConsumerControlButton::Power,
                fir::ConsumerControlButton::VolumeDown,
            ]),
            ..Default::default()
        };
        let events2 = converter.handle(&report2, time);
        assert!(events2.iter().any(|e| e.code == uapi::KEY_VOLUMEUP as u16 && e.value == 0));
        assert!(events2.iter().any(|e| e.code == uapi::KEY_VOLUMEDOWN as u16 && e.value == 1));
        // Power is still pressed, so no Power event
        assert!(!events2.iter().any(|e| e.code == uapi::KEY_POWER as u16));

        // Release all
        let report3 =
            fir::ConsumerControlInputReport { pressed_buttons: Some(vec![]), ..Default::default() };
        let events3 = converter.handle(&report3, time);
        assert!(events3.iter().any(|e| e.code == uapi::KEY_POWER as u16 && e.value == 0));
        assert!(events3.iter().any(|e| e.code == uapi::KEY_VOLUMEDOWN as u16 && e.value == 0));
    }

    #[::fuchsia::test]
    fn test_empty_consumer_control_reports_return_empty_events() {
        let time = zx::MonotonicInstant::from_nanos(400_000_000);
        assert!(
            ConsumerControlReportConverter::new()
                .handle(&fir::ConsumerControlInputReport::default(), time)
                .is_empty()
        );
    }

    #[::fuchsia::test]
    fn test_consumer_control_function_button_maps_to_volumedown() {
        let mut converter = ConsumerControlReportConverter::new();
        let time = zx::MonotonicInstant::from_nanos(500_000_000);

        let report = fir::ConsumerControlInputReport {
            pressed_buttons: Some(vec![fir::ConsumerControlButton::Function]),
            ..Default::default()
        };
        let events = converter.handle(&report, time);
        assert!(events.iter().any(|e| e.code == uapi::KEY_VOLUMEDOWN as u16 && e.value == 1));
        assert!(events.iter().any(|e| e.code == uapi::SYN_REPORT as u16));
    }

    #[::fuchsia::test]
    fn test_consumer_control_report_none_pressed_buttons_releases_held_buttons() {
        let mut converter = ConsumerControlReportConverter::new();
        let time = zx::MonotonicInstant::from_nanos(300_000_000);

        // Press VolumeUp
        let report1 = fir::ConsumerControlInputReport {
            pressed_buttons: Some(vec![fir::ConsumerControlButton::VolumeUp]),
            ..Default::default()
        };
        let events1 = converter.handle(&report1, time);
        assert_eq!(
            events1.iter().map(|e| (e.type_, e.code, e.value)).collect::<Vec<_>>(),
            vec![
                (uapi::EV_KEY as u16, uapi::KEY_VOLUMEUP as u16, 1),
                (uapi::EV_SYN as u16, uapi::SYN_REPORT as u16, 0),
            ]
        );

        // Subsequent report with pressed_buttons: None releases VolumeUp
        let report2 =
            fir::ConsumerControlInputReport { pressed_buttons: None, ..Default::default() };
        let events2 = converter.handle(&report2, time);
        assert_eq!(
            events2.iter().map(|e| (e.type_, e.code, e.value)).collect::<Vec<_>>(),
            vec![
                (uapi::EV_KEY as u16, uapi::KEY_VOLUMEUP as u16, 0),
                (uapi::EV_SYN as u16, uapi::SYN_REPORT as u16, 0),
            ]
        );

        // Subsequent report with pressed_buttons: None when no buttons held emits nothing
        let report3 =
            fir::ConsumerControlInputReport { pressed_buttons: None, ..Default::default() };
        let events3 = converter.handle(&report3, time);
        assert!(events3.is_empty());
    }
}
