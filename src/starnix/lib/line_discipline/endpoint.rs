// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use starnix_uapi::uapi;

/// Identifies one of the two ends of a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TerminalSide {
    Main,
    Replica,
}

impl From<bool> for TerminalSide {
    fn from(is_main: bool) -> Self {
        if is_main { Self::Main } else { Self::Replica }
    }
}

/// Tracks active open references to one side (main or replica) of the terminal.
///
/// Starts as `None` before the side is opened for the first time; the side is considered closed
/// once all opened references have closed (`Some(0)`).
#[derive(Debug, Default)]
pub(crate) struct EndpointRefs(Option<u32>);

impl EndpointRefs {
    pub(crate) fn open(&mut self) {
        self.0 = Some(self.0.unwrap_or(0) + 1);
    }

    pub(crate) fn close(&mut self) {
        self.0 = self.0.map(|count| count.saturating_sub(1));
    }

    pub(crate) fn is_closed(&self) -> bool {
        matches!(self.0, Some(0))
    }
}

/// Manages `TIOCPKT` packet mode status events for the main side of the terminal.
#[derive(Debug, Default)]
pub(crate) struct PacketMode {
    enabled: bool,
    pending_events: u8,
}

impl PacketMode {
    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.pending_events = 0;
        }
    }

    pub(crate) fn pending_events(&self) -> Option<u8> {
        if self.enabled && self.pending_events != 0 { Some(self.pending_events) } else { None }
    }

    pub(crate) fn has_pending_events(&self) -> bool {
        self.pending_events().is_some()
    }

    pub(crate) fn clear_pending_events(&mut self) {
        self.pending_events = 0;
    }

    pub(crate) fn record(&mut self, events: u32) {
        if self.enabled {
            self.pending_events |= events as u8;
        }
    }

    pub(crate) fn on_stop(&mut self) {
        self.replace(uapi::TIOCPKT_START, uapi::TIOCPKT_STOP);
    }

    pub(crate) fn on_start(&mut self) {
        self.replace(uapi::TIOCPKT_STOP, uapi::TIOCPKT_START);
    }

    pub(crate) fn on_flow_control_changed(&mut self, standard_xon_xoff: bool) {
        let event = if standard_xon_xoff { uapi::TIOCPKT_DOSTOP } else { uapi::TIOCPKT_NOSTOP };
        self.replace(uapi::TIOCPKT_DOSTOP | uapi::TIOCPKT_NOSTOP, event);
    }

    fn replace(&mut self, clear_mask: u32, set_event: u32) {
        if self.enabled {
            self.pending_events = (self.pending_events & !(clear_mask as u8)) | (set_event as u8);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn test_terminal_side_from_bool() {
        assert_eq!(TerminalSide::from(true), TerminalSide::Main);
        assert_eq!(TerminalSide::from(false), TerminalSide::Replica);
    }

    #[fuchsia::test]
    fn test_endpoint_refs_lifecycle() {
        let mut refs = EndpointRefs::default();
        assert!(!refs.is_closed());

        refs.open();
        refs.open();
        assert!(!refs.is_closed());

        refs.close();
        assert!(!refs.is_closed());

        refs.close();
        assert!(refs.is_closed());

        refs.open();
        assert!(!refs.is_closed());
    }

    #[fuchsia::test]
    fn test_packet_mode_transitions() {
        let mut packet_mode = PacketMode::default();
        assert!(!packet_mode.is_enabled());
        packet_mode.record(uapi::TIOCPKT_FLUSHREAD);
        assert!(!packet_mode.has_pending_events());
        assert_eq!(packet_mode.pending_events(), None);

        packet_mode.set_enabled(true);
        packet_mode.on_stop();
        assert_eq!(packet_mode.pending_events(), Some(uapi::TIOCPKT_STOP as u8));

        packet_mode.on_start();
        assert_eq!(packet_mode.pending_events(), Some(uapi::TIOCPKT_START as u8));

        packet_mode.on_flow_control_changed(true);
        assert_eq!(
            packet_mode.pending_events(),
            Some((uapi::TIOCPKT_START | uapi::TIOCPKT_DOSTOP) as u8)
        );
        packet_mode.on_flow_control_changed(false);
        assert_eq!(
            packet_mode.pending_events(),
            Some((uapi::TIOCPKT_START | uapi::TIOCPKT_NOSTOP) as u8)
        );

        packet_mode.clear_pending_events();
        assert!(!packet_mode.has_pending_events());
    }
}
