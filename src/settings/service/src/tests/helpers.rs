// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_ui_input::MediaButtonsEvent;

pub fn clone_media_buttons_event_without_wake_lease(
    event: &MediaButtonsEvent,
) -> MediaButtonsEvent {
    MediaButtonsEvent {
        volume: event.volume,
        mic_mute: event.mic_mute,
        pause: event.pause,
        camera_disable: event.camera_disable,
        power: event.power,
        function: event.function,
        device_id: event.device_id,
        ..Default::default()
    }
}
