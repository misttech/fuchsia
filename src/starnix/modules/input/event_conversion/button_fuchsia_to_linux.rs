// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_ui_input::{MediaButtonsEvent, TouchButton, TouchButtonsEvent};
use starnix_types::time::timeval_from_time;
use starnix_uapi::uapi;

pub struct LinuxButtonEventBatch {
    pub events: Vec<uapi::input_event>,

    // Because FIDL button events do not carry a timestamp, we perform a direct
    // clock read during conversion and assign this value as the timestamp for
    // all generated Linux button events in a single batch.
    pub event_time: zx::MonotonicInstant,
    pub power_is_pressed: bool,
    pub function_is_pressed: bool,
    pub palm_is_pressed: bool,
}

impl LinuxButtonEventBatch {
    pub fn new() -> Self {
        Self {
            events: vec![],
            event_time: zx::MonotonicInstant::get(),
            power_is_pressed: false,
            function_is_pressed: false,
            palm_is_pressed: false,
        }
    }
}

pub fn parse_fidl_media_button_event(
    fidl_event: &MediaButtonsEvent,
    power_was_pressed: bool,
    function_was_pressed: bool,
) -> LinuxButtonEventBatch {
    let mut batch = LinuxButtonEventBatch::new();
    let time = timeval_from_time(batch.event_time);
    let sync_event = uapi::input_event {
        // See https://www.kernel.org/doc/Documentation/input/event-codes.rst.
        time,
        type_: uapi::EV_SYN as u16,
        code: uapi::SYN_REPORT as u16,
        value: 0,
    };

    batch.power_is_pressed = fidl_event.power.unwrap_or(false);
    batch.function_is_pressed = fidl_event.function.unwrap_or(false);
    for (then, now, key_code) in [
        (power_was_pressed, batch.power_is_pressed, uapi::KEY_POWER),
        (function_was_pressed, batch.function_is_pressed, uapi::KEY_VOLUMEDOWN),
    ] {
        // Button state changed. Send an event.
        if then != now {
            batch.events.push(uapi::input_event {
                time,
                type_: uapi::EV_KEY as u16,
                code: key_code as u16,
                value: now as i32,
            });
            batch.events.push(sync_event);
        }
    }

    batch
}

pub fn parse_fidl_touch_button_event(
    fidl_event: &TouchButtonsEvent,
    palm_was_pressed: bool,
) -> LinuxButtonEventBatch {
    let mut batch = LinuxButtonEventBatch::new();
    let time = timeval_from_time(batch.event_time);
    let sync_event = uapi::input_event {
        // See https://www.kernel.org/doc/Documentation/input/event-codes.rst.
        time,
        type_: uapi::EV_SYN as u16,
        code: uapi::SYN_REPORT as u16,
        value: 0,
    };

    if let Some(buttons) = &fidl_event.pressed_buttons {
        batch.palm_is_pressed = buttons.contains(&TouchButton::Palm)
    };

    let (then, now, key_code) = (palm_was_pressed, batch.palm_is_pressed, uapi::KEY_SLEEP);
    // Button state changed. Send an event.
    if then != now {
        batch.events.push(uapi::input_event {
            time,
            type_: uapi::EV_KEY as u16,
            code: key_code as u16,
            value: now as i32,
        });
        batch.events.push(sync_event);
    }

    batch
}
