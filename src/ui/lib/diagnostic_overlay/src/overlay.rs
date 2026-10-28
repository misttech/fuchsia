// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl::endpoints::create_request_stream;
use fidl_fuchsia_ui_input::MediaButtonsEvent;
use fidl_fuchsia_ui_policy::{
    DeviceListenerRegistryMarker, DeviceListenerRegistryProxy, MediaButtonsListenerRequest,
};
use fuchsia_async as fasync;
use fuchsia_component::client::connect_to_protocol;
use futures::StreamExt;
use futures::channel::mpsc::UnboundedSender;
use log::{info, warn};
use std::fmt::Write as _;

use crate::OverlayEvent;

/// Total simultaneous Volume Up + Volume Down presses required to trigger a reboot.
pub const REQUIRED_PRESS_COUNT: u8 = 5;

/// Press threshold at which the diagnostic and countdown overlay becomes visible.
pub const OVERLAY_VISIBLE_PRESS_COUNT: u8 = 3;

/// Maximum interval (500ms) between consecutive presses, matching `session_manager`.
pub const MAX_PRESS_INTERVAL_NS: i64 = 500_000_000;

/// Maximum ASCII character count rendered per diagnostic HUD row.
pub const MAX_DIAGNOSTIC_LINE_CHARS: usize = 56;

const _: () = assert!(MAX_DIAGNOSTIC_LINE_CHARS <= u8::MAX as usize);
const _: () = assert!(REQUIRED_PRESS_COUNT - OVERLAY_VISIBLE_PRESS_COUNT == 2);

/// Number of formatted diagnostic rows displayed in the HUD body.
pub const DIAGNOSTIC_LINE_COUNT: usize = 8;

/// Fixed-capacity UTF-8 HUD row (`<= 56` bytes) formatted completely on the stack without heap
/// allocations.
#[derive(Clone, Copy, Eq)]
pub struct HudLine {
    buf: [u8; MAX_DIAGNOSTIC_LINE_CHARS],
    len: u8,
    truncated: bool,
}

impl HudLine {
    /// Creates an empty [`HudLine`].
    #[inline]
    pub const fn new() -> Self {
        Self { buf: [0; MAX_DIAGNOSTIC_LINE_CHARS], len: 0, truncated: false }
    }

    /// Returns the valid UTF-8 contents of this line (`<= 56` bytes).
    #[inline]
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.buf[..self.len as usize]).unwrap_or("")
    }
}

impl Default for HudLine {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl AsRef<str> for HudLine {
    #[inline]
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq for HudLine {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl std::fmt::Debug for HudLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.as_str(), f)
    }
}

impl std::fmt::Display for HudLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PartialEq<&str> for HudLine {
    #[inline]
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<str> for HudLine {
    #[inline]
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl std::fmt::Write for HudLine {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        if self.truncated || s.is_empty() {
            return Ok(());
        }
        let current_len = self.len as usize;
        let remaining = MAX_DIAGNOSTIC_LINE_CHARS.saturating_sub(current_len);
        let copy_len = s.floor_char_boundary(remaining);
        self.buf[current_len..current_len + copy_len].copy_from_slice(&s.as_bytes()[..copy_len]);
        self.len = (current_len + copy_len) as u8;
        if copy_len < s.len() {
            self.truncated = true;
        }
        Ok(())
    }
}

/// Visual state of the diagnostic overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayDisplayMode {
    /// Overlay is not shown on screen.
    Hidden,
    /// User has pressed 3 or 4 times; countdown banner and diagnostics are shown.
    ActiveCountdown { remaining_presses: u8 },
    /// User reached 5 presses; reboot notification banner and diagnostics are shown.
    Rebooting,
    /// User stopped pressing after Press 3 or 4 and the 500ms window elapsed; overlay stays open
    /// until the user confirms closing via `[X]` or presses Power.
    DiagnosticsIdle,
}

impl OverlayDisplayMode {
    /// Returns `true` when the overlay should be rendered and attached on screen.
    pub fn is_visible(&self) -> bool {
        !matches!(self, OverlayDisplayMode::Hidden)
    }

    /// Returns the primary header banner title for this display mode.
    pub fn banner_title(&self) -> &'static str {
        match self {
            OverlayDisplayMode::Hidden => "",
            OverlayDisplayMode::ActiveCountdown { remaining_presses: 1 } => {
                "Will reboot after 1 more press"
            }
            OverlayDisplayMode::ActiveCountdown { .. } => "Will reboot after 2 more presses",
            OverlayDisplayMode::Rebooting => "Filing crash report & rebooting...",
            OverlayDisplayMode::DiagnosticsIdle => {
                "Press 5 times to file a crash report and reboot"
            }
        }
    }

    /// Returns the secondary header banner subtitle for this display mode.
    pub fn banner_subtitle(&self) -> &'static str {
        match self {
            OverlayDisplayMode::Hidden => "",
            OverlayDisplayMode::ActiveCountdown { .. } => {
                "Keep pressing VolUp+VolDn | Drag bar | [X] close"
            }
            OverlayDisplayMode::Rebooting => "Collecting crash report via session_manager...",
            OverlayDisplayMode::DiagnosticsIdle => "Drag top bar to move | Tap [X] to close",
        }
    }
}

/// Tracks the 5-press simultaneous Volume Up + Volume Down state machine and overlay lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortcutTracker {
    count: u8,
    last_press_time_ns: Option<i64>,
    volume_up_pressed: bool,
    volume_down_pressed: bool,
    power_pressed: bool,
    was_both_pressed: bool,
    mode: OverlayDisplayMode,
    confirm_close: bool,
}

impl Default for ShortcutTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ShortcutTracker {
    /// Creates a new [`ShortcutTracker`] in the initial [`OverlayDisplayMode::Hidden`] state.
    pub fn new() -> Self {
        Self {
            count: 0,
            last_press_time_ns: None,
            volume_up_pressed: false,
            volume_down_pressed: false,
            power_pressed: false,
            was_both_pressed: false,
            mode: OverlayDisplayMode::Hidden,
            confirm_close: false,
        }
    }

    /// Returns the number of simultaneous presses recorded in the current sequence.
    #[inline]
    pub fn count(&self) -> u8 {
        self.count
    }

    /// Returns the current [`OverlayDisplayMode`].
    #[inline]
    pub fn mode(&self) -> OverlayDisplayMode {
        self.mode
    }

    /// Returns `true` when the secondary close confirmation screen is active.
    #[inline]
    pub fn confirm_close(&self) -> bool {
        self.confirm_close
    }

    /// Opens (or toggles) the secondary close confirmation screen when `[X]` is tapped.
    pub fn request_close(&mut self) -> OverlayDisplayMode {
        if self.mode.is_visible() {
            self.confirm_close = !self.confirm_close;
        }
        self.mode
    }

    /// Cancels the secondary close confirmation screen and returns to the diagnostic view.
    pub fn cancel_close(&mut self) -> OverlayDisplayMode {
        self.confirm_close = false;
        self.mode
    }

    /// Dismisses the overlay and resets the press counter (when confirmed via `[X]` -> `[Close]`).
    pub fn dismiss(&mut self) -> OverlayDisplayMode {
        self.count = 0;
        self.last_press_time_ns = None;
        self.confirm_close = false;
        self.mode = OverlayDisplayMode::Hidden;
        self.mode
    }

    /// Aborts any in-progress 5-press reboot sequence without hiding an already-visible diagnostic
    /// overlay (transitions `ActiveCountdown` to `DiagnosticsIdle`).
    fn abort_press_sequence(&mut self) {
        if matches!(self.mode, OverlayDisplayMode::ActiveCountdown { .. }) {
            self.mode = OverlayDisplayMode::DiagnosticsIdle;
        }
        self.count = 0;
        self.last_press_time_ns = None;
    }

    fn check_press_timeout(&mut self, now_ns: i64) {
        if let Some(last_time_ns) = self.last_press_time_ns
            && now_ns.saturating_sub(last_time_ns) > MAX_PRESS_INTERVAL_NS
        {
            self.abort_press_sequence();
        }
    }

    /// Processes a `MediaButtonsEvent` at boot timestamp `now_ns` (`zx::BootInstant`, matching
    /// `session_manager`) and returns the updated `OverlayDisplayMode`.
    pub fn on_media_buttons_event(
        &mut self,
        event: &MediaButtonsEvent,
        now_ns: i64,
    ) -> OverlayDisplayMode {
        if matches!(self.mode, OverlayDisplayMode::Rebooting) {
            return self.mode;
        }

        self.check_press_timeout(now_ns);

        if let Some(v_up) = event.volume_up {
            self.volume_up_pressed = v_up;
        }
        if let Some(v_down) = event.volume_down {
            self.volume_down_pressed = v_down;
        }

        let is_both_pressed = self.volume_up_pressed && self.volume_down_pressed;
        let is_new_press = is_both_pressed && !self.was_both_pressed;
        self.was_both_pressed = is_both_pressed;

        let was_power_pressed = self.power_pressed;
        if let Some(power) = event.power {
            self.power_pressed = power;
        }
        if self.power_pressed || was_power_pressed {
            return self.dismiss();
        }

        if is_new_press {
            self.confirm_close = false;
            self.count = self.count.saturating_add(1);
            self.last_press_time_ns = Some(now_ns);

            if self.count >= REQUIRED_PRESS_COUNT {
                info!(
                    "Volume up and down pressed simultaneously {} times; showing reboot overlay.",
                    REQUIRED_PRESS_COUNT
                );
                self.mode = OverlayDisplayMode::Rebooting;
                self.count = 0;
                self.last_press_time_ns = None;
            } else if self.count >= OVERLAY_VISIBLE_PRESS_COUNT {
                self.mode = OverlayDisplayMode::ActiveCountdown {
                    remaining_presses: REQUIRED_PRESS_COUNT - self.count,
                };
            }
        }

        self.mode
    }

    /// Advances time-based transitions (active countdown timeout -> `DiagnosticsIdle`) at `now_ns`.
    pub fn on_timer_tick(&mut self, now_ns: i64) -> OverlayDisplayMode {
        self.check_press_timeout(now_ns);
        self.mode
    }

    /// Returns the remaining delay in nanoseconds before an active multi-press countdown expires
    /// (`count >= 2` or `ActiveCountdown`), or `None` when no press-sequence timeout timer is
    /// needed (accidental single chord presses do not arm this timer).
    pub fn next_timer_delay_ns(&self, now_ns: i64) -> Option<i64> {
        if self.count >= OVERLAY_VISIBLE_PRESS_COUNT - 1
            || matches!(self.mode, OverlayDisplayMode::ActiveCountdown { .. })
        {
            let last_ns = self.last_press_time_ns?;
            let elapsed_ns = now_ns.saturating_sub(last_ns).max(0);
            Some(MAX_PRESS_INTERVAL_NS.saturating_sub(elapsed_ns).max(1))
        } else {
            None
        }
    }
}

/// Returns the default HUD body rows before live telemetry snapshots arrive.
pub fn default_hud_lines() -> [HudLine; DIAGNOSTIC_LINE_COUNT] {
    let mut lines = [HudLine::new(); DIAGNOSTIC_LINE_COUNT];
    let _ = lines[0].write_str("Telemetry: awaiting diagnostics collector...");
    lines
}

/// Connects to `fuchsia.ui.policy.DeviceListenerRegistry` and returns the background listener task
/// forwarding `MediaButtonsEvent`s as [`OverlayEvent::MediaButtons`].
pub fn spawn_media_buttons_listener(
    sender: UnboundedSender<OverlayEvent>,
) -> Option<fasync::Task<()>> {
    let Ok(registry) = connect_to_protocol::<DeviceListenerRegistryMarker>() else {
        warn!("fuchsia.ui.policy.DeviceListenerRegistry not available for diagnostic_overlay");
        return None;
    };
    Some(spawn_media_buttons_listener_with_registry(registry, sender))
}

/// Registers a media-buttons listener on an explicit `registry` proxy and forwards incoming
/// events to `sender`.
pub fn spawn_media_buttons_listener_with_registry(
    registry: DeviceListenerRegistryProxy,
    sender: UnboundedSender<OverlayEvent>,
) -> fasync::Task<()> {
    fasync::Task::local(async move {
        let (listener_client, mut listener_stream) = create_request_stream();
        match registry.register_listener(listener_client).await {
            Ok(()) => info!("Registered diagnostic_overlay media buttons listener."),
            Err(e) => {
                warn!("Failed to register diagnostic_overlay media buttons listener: {:?}", e);
                return;
            }
        }
        drop(registry);

        while let Some(item) = listener_stream.next().await {
            let request = match item {
                Ok(request) => request,
                Err(e) => {
                    warn!("MediaButtonsListener stream error: {:?}", e);
                    break;
                }
            };
            if let MediaButtonsListenerRequest::OnEvent { mut event, responder } = request {
                let _ = event.wake_lease.take();
                let _ = responder.send();
                if sender.unbounded_send(OverlayEvent::MediaButtons(event)).is_err() {
                    break;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl::endpoints::create_proxy_and_stream;
    use fidl_fuchsia_ui_policy::DeviceListenerRegistryRequest;
    use futures::TryStreamExt;

    fn both_pressed_event() -> MediaButtonsEvent {
        MediaButtonsEvent { volume_up: Some(true), volume_down: Some(true), ..Default::default() }
    }

    fn released_event() -> MediaButtonsEvent {
        MediaButtonsEvent { volume_up: Some(false), volume_down: Some(false), ..Default::default() }
    }

    #[fuchsia::test]
    fn test_countdown_and_reboot_transitions_within_500ms() {
        let mut tracker = ShortcutTracker::default();
        let mut t = 1_000_000_000i64;

        // Presses 1 and 2 remain Hidden; accidental Press 1 arms no timer, while Press 2 arms the
        // remaining 500ms press-sequence timeout.
        assert_eq!(
            tracker.on_media_buttons_event(&both_pressed_event(), t),
            OverlayDisplayMode::Hidden
        );
        assert!(!tracker.mode().is_visible());
        assert_eq!(tracker.mode().banner_title(), "");
        assert_eq!(tracker.mode().banner_subtitle(), "");
        assert_eq!(tracker.count(), 1);
        assert_eq!(tracker.next_timer_delay_ns(t), None);
        tracker.on_media_buttons_event(&released_event(), t + 10_000_000);

        t += 200_000_000;
        assert_eq!(
            tracker.on_media_buttons_event(&both_pressed_event(), t),
            OverlayDisplayMode::Hidden
        );
        assert_eq!(tracker.count(), 2);
        assert_eq!(tracker.next_timer_delay_ns(t + 50_000_000), Some(450_000_000));
        tracker.on_media_buttons_event(&released_event(), t + 10_000_000);

        // Press 3 shows overlay with "Will reboot after 2 more presses".
        t += 200_000_000;
        let mode3 = tracker.on_media_buttons_event(&both_pressed_event(), t);
        assert_eq!(mode3, OverlayDisplayMode::ActiveCountdown { remaining_presses: 2 });
        assert_eq!(mode3.banner_title(), "Will reboot after 2 more presses");
        assert_eq!(mode3.banner_subtitle(), "Keep pressing VolUp+VolDn | Drag bar | [X] close");
        tracker.on_media_buttons_event(&released_event(), t + 10_000_000);

        // Press 4 within 500ms updates banner to "Will reboot after 1 more press".
        t += 300_000_000;
        let mode4 = tracker.on_media_buttons_event(&both_pressed_event(), t);
        assert_eq!(mode4, OverlayDisplayMode::ActiveCountdown { remaining_presses: 1 });
        assert_eq!(mode4.banner_title(), "Will reboot after 1 more press");
        tracker.on_media_buttons_event(&released_event(), t + 10_000_000);

        // Press 5 within 500ms transitions to Rebooting.
        t += 300_000_000;
        let mode5 = tracker.on_media_buttons_event(&both_pressed_event(), t);
        assert_eq!(mode5, OverlayDisplayMode::Rebooting);
        assert_eq!(mode5.banner_title(), "Filing crash report & rebooting...");
        assert_eq!(mode5.banner_subtitle(), "Collecting crash report via session_manager...");
        assert_eq!(tracker.count(), 0);
    }

    #[fuchsia::test]
    fn test_stopping_after_press_3_stays_open_until_dismissed() {
        let mut tracker = ShortcutTracker::new();
        let mut t = 1_000_000_000i64;

        for _ in 0..3 {
            tracker.on_media_buttons_event(&both_pressed_event(), t);
            tracker.on_media_buttons_event(&released_event(), t + 10_000_000);
            t += 100_000_000;
        }
        let press_3_time = t - 100_000_000;
        assert_eq!(tracker.mode(), OverlayDisplayMode::ActiveCountdown { remaining_presses: 2 });

        // At +500ms after Press 3 (<= 500ms), still in ActiveCountdown.
        assert_eq!(
            tracker.on_timer_tick(press_3_time + MAX_PRESS_INTERVAL_NS),
            OverlayDisplayMode::ActiveCountdown { remaining_presses: 2 }
        );

        // At +600ms after Press 3 (> 500ms), transitions to DiagnosticsIdle and resets count.
        let idle_mode = tracker.on_timer_tick(press_3_time + 600_000_000);
        assert_eq!(tracker.count(), 0);
        assert_eq!(idle_mode, OverlayDisplayMode::DiagnosticsIdle);
        assert_eq!(tracker.next_timer_delay_ns(press_3_time + 600_000_000), None);
        assert_eq!(idle_mode.banner_title(), "Press 5 times to file a crash report and reboot");
        assert_eq!(idle_mode.banner_subtitle(), "Drag top bar to move | Tap [X] to close");
        assert!(idle_mode.is_visible());

        // Remains open indefinitely across subsequent timer ticks (does not auto-close).
        assert_eq!(
            tracker.on_timer_tick(press_3_time + 60_000_000_000),
            OverlayDisplayMode::DiagnosticsIdle
        );

        // Tapping [X] once opens the close confirmation screen (overlay remains visible).
        assert_eq!(tracker.request_close(), OverlayDisplayMode::DiagnosticsIdle);
        assert!(tracker.confirm_close());

        // Tapping [Cancel] cancels the confirmation screen and keeps overlay open.
        assert_eq!(tracker.cancel_close(), OverlayDisplayMode::DiagnosticsIdle);
        assert!(!tracker.confirm_close());

        // Pressing the power button immediately closes and dismisses the overlay.
        let power_down = MediaButtonsEvent { power: Some(true), ..Default::default() };
        let power_up = MediaButtonsEvent { power: Some(false), ..Default::default() };
        assert_eq!(
            tracker.on_media_buttons_event(&power_down, press_3_time + 61_000_000_000),
            OverlayDisplayMode::Hidden
        );
        assert_eq!(
            tracker.on_media_buttons_event(&power_up, press_3_time + 61_100_000_000),
            OverlayDisplayMode::Hidden
        );

        // Re-open to Press 3 and verify tapping [X] then confirming [Close] also dismisses.
        let mut t2 = press_3_time + 62_000_000_000;
        for _ in 0..3 {
            tracker.on_media_buttons_event(&both_pressed_event(), t2);
            tracker.on_media_buttons_event(&released_event(), t2 + 10_000_000);
            t2 += 100_000_000;
        }
        assert_eq!(tracker.on_timer_tick(t2 + 600_000_000), OverlayDisplayMode::DiagnosticsIdle);
        assert_eq!(tracker.request_close(), OverlayDisplayMode::DiagnosticsIdle);
        assert!(tracker.confirm_close());
        assert_eq!(tracker.dismiss(), OverlayDisplayMode::Hidden);
        assert!(!tracker.confirm_close());
    }

    #[fuchsia::test]
    fn test_staggered_and_held_button_presses() {
        let mut tracker = ShortcutTracker::new();
        let mut t = 1_000_000_000i64;

        // Pressing only volume_up does not trigger a chord press.
        tracker.on_media_buttons_event(
            &MediaButtonsEvent { volume_up: Some(true), ..Default::default() },
            t,
        );
        assert_eq!(tracker.count(), 0);

        // Pressing volume_down while volume_up is held triggers Press 1.
        t += 50_000_000;
        tracker.on_media_buttons_event(
            &MediaButtonsEvent { volume_down: Some(true), ..Default::default() },
            t,
        );
        assert_eq!(tracker.count(), 1);

        // Repeated events while both buttons remain held must not increment count.
        t += 50_000_000;
        tracker.on_media_buttons_event(&both_pressed_event(), t);
        assert_eq!(tracker.count(), 1);

        // Releasing one button and re-pressing it triggers Press 2.
        t += 50_000_000;
        tracker.on_media_buttons_event(
            &MediaButtonsEvent { volume_up: Some(false), ..Default::default() },
            t,
        );
        t += 50_000_000;
        tracker.on_media_buttons_event(
            &MediaButtonsEvent { volume_up: Some(true), ..Default::default() },
            t,
        );
        assert_eq!(tracker.count(), 2);
    }

    #[fuchsia::test]
    fn test_timeout_expiration_on_next_media_button_press() {
        let mut tracker = ShortcutTracker::new();
        let mut t = 1_000_000_000i64;

        for _ in 0..3 {
            tracker.on_media_buttons_event(&both_pressed_event(), t);
            tracker.on_media_buttons_event(&released_event(), t + 10_000_000);
            t += 100_000_000;
        }
        let press_3_time = t - 100_000_000;
        assert_eq!(tracker.mode(), OverlayDisplayMode::ActiveCountdown { remaining_presses: 2 });

        // Next press arrives > 500ms later without an intermediate on_timer_tick: aborts the old
        // countdown to DiagnosticsIdle and starts a new sequence at count == 1.
        let late_press = press_3_time + MAX_PRESS_INTERVAL_NS + 1;
        assert_eq!(
            tracker.on_media_buttons_event(&both_pressed_event(), late_press),
            OverlayDisplayMode::DiagnosticsIdle
        );
        assert_eq!(tracker.count(), 1);
    }

    #[fuchsia::test]
    fn test_additional_presses_while_rebooting_are_ignored() {
        let mut tracker = ShortcutTracker::new();
        let mut t = 1_000_000_000i64;

        for _ in 0..5 {
            tracker.on_media_buttons_event(&both_pressed_event(), t);
            tracker.on_media_buttons_event(&released_event(), t + 10_000_000);
            t += 100_000_000;
        }
        assert_eq!(tracker.mode(), OverlayDisplayMode::Rebooting);
        assert_eq!(tracker.count(), 0);

        // Additional presses (6th, 7th, 8th) and timer ticks must keep the overlay in Rebooting.
        for _ in 0..3 {
            assert_eq!(
                tracker.on_media_buttons_event(&both_pressed_event(), t),
                OverlayDisplayMode::Rebooting
            );
            assert_eq!(
                tracker.on_media_buttons_event(&released_event(), t + 10_000_000),
                OverlayDisplayMode::Rebooting
            );
            t += 100_000_000;
        }
        assert_eq!(tracker.on_timer_tick(t + 1_000_000_000), OverlayDisplayMode::Rebooting);
        assert_eq!(tracker.count(), 0);
    }

    #[fuchsia::test]
    fn test_hud_line_utf8_truncation_and_formatting() {
        let defaults = default_hud_lines();
        assert_eq!(defaults.len(), DIAGNOSTIC_LINE_COUNT);
        assert_eq!(defaults[0].as_str(), "Telemetry: awaiting diagnostics collector...");
        assert_eq!(defaults[1].as_ref(), "");
        assert_eq!(format!("{}", defaults[0]), "Telemetry: awaiting diagnostics collector...");

        let mut utf8_line = HudLine::default();
        let _ = write!(&mut utf8_line, "{}°C", "x".repeat(MAX_DIAGNOSTIC_LINE_CHARS - 1));
        assert_eq!(utf8_line.as_str().len(), MAX_DIAGNOSTIC_LINE_CHARS - 1);
        assert_eq!(utf8_line.as_str(), "x".repeat(MAX_DIAGNOSTIC_LINE_CHARS - 1).as_str());

        let mut line_a = HudLine::new();
        let mut line_b = HudLine::new();
        let _ = write!(&mut line_a, "abcdef");
        let _ = write!(&mut line_b, "ab");
        let _ = write!(&mut line_a, "\rxyz");
        let mut line_c = HudLine::new();
        let _ = write!(&mut line_c, "ab");
        assert_eq!(line_b, line_c);
    }

    #[fuchsia::test]
    async fn test_media_buttons_listener_forwards_events() {
        let (registry_proxy, mut registry_stream) =
            create_proxy_and_stream::<DeviceListenerRegistryMarker>();
        let (sender, mut receiver) = futures::channel::mpsc::unbounded();
        let _task = spawn_media_buttons_listener_with_registry(registry_proxy, sender);

        let Some(DeviceListenerRegistryRequest::RegisterListener { listener, responder }) =
            registry_stream.try_next().await.expect("register request")
        else {
            panic!("Expected RegisterListener request");
        };
        responder.send().expect("ack register");

        let listener_proxy = listener.into_proxy();
        let (lease_a, _lease_b) = fidl::EventPair::create();
        let mut event = both_pressed_event();
        event.wake_lease = Some(lease_a);
        listener_proxy.on_event(event).await.expect("send media button event");

        let Some(OverlayEvent::MediaButtons(ev)) = receiver.next().await else {
            panic!("Expected OverlayEvent::MediaButtons");
        };
        assert_eq!(ev.volume_up, Some(true));
        assert_eq!(ev.volume_down, Some(true));
        assert!(ev.wake_lease.is_none());
    }
}
