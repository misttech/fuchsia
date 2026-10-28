// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Diagnostic and reboot-countdown HUD overlay library for Flatland window managers.

pub mod overlay;

use fidl_fuchsia_ui_input::MediaButtonsEvent;

pub use overlay::{
    DIAGNOSTIC_LINE_COUNT, HudLine, MAX_DIAGNOSTIC_LINE_CHARS, MAX_PRESS_INTERVAL_NS,
    OVERLAY_VISIBLE_PRESS_COUNT, OverlayDisplayMode, REQUIRED_PRESS_COUNT, ShortcutTracker,
    default_hud_lines, spawn_media_buttons_listener, spawn_media_buttons_listener_with_registry,
};

/// Events processed by the diagnostic overlay state machine and controller.
#[derive(Debug, PartialEq)]
pub enum OverlayEvent {
    /// Hardware media-button state update from `fuchsia.ui.policy.DeviceListenerRegistry`.
    MediaButtons(MediaButtonsEvent),
    /// Multi-press shortcut timeout tick.
    PressTimeoutTick,
}

/// Returns `true` when the diagnostic overlay listener should be enabled for the given session
/// configuration flags.
#[inline]
pub fn is_overlay_enabled(enable_debug_shortcut: bool, visual_debugging_level: u8) -> bool {
    enable_debug_shortcut || visual_debugging_level > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn test_is_overlay_enabled() {
        assert!(!is_overlay_enabled(false, 0));
        assert!(is_overlay_enabled(true, 0));
        assert!(is_overlay_enabled(false, 1));
        assert!(is_overlay_enabled(true, 2));
        assert!(matches!(OverlayEvent::PressTimeoutTick, OverlayEvent::PressTimeoutTick));
    }
}
