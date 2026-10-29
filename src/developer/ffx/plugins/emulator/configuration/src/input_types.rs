// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Input events for the QEMU Machine Protocol (QMP) `input-send-event` command.
//!
//! The types mirror the QMP `InputEvent` union, so they are QEMU-specific. Only the QEMU engine
//! implements [`EmulatorEngine::send_input`].
//!
//! The types live in this crate because the [`EmulatorEngine`] trait uses them. The engines
//! crate depends on this crate, so placing the types there would create a dependency cycle.
//!
//! QMP reference: <https://www.qemu.org/docs/master/interop/qemu-qmp-ref.html#command-QMP-ui.input-send-event>
//!
//! [`EmulatorEngine`]: crate::EmulatorEngine
//! [`EmulatorEngine::send_input`]: crate::EmulatorEngine::send_input

use std::time::Duration;

/// One QMP `InputEvent`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputEvent {
    /// A QMP `InputEvent` object, sent unchanged.
    Raw(serde_json::Value),
}

impl InputEvent {
    /// Returns the QMP `InputEvent` JSON object for this event.
    pub fn to_qmp(&self) -> serde_json::Value {
        match self {
            InputEvent::Raw(value) => value.clone(),
        }
    }
}

/// Events sent in one QMP `input-send-event` command.
///
/// QEMU follows each command with one sync event, so the guest receives the events as one input
/// report. The exception is key events given by scancode `number`: QEMU syncs each of them on
/// its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputStep {
    pub events: Vec<InputEvent>,
    /// Time to wait after this step, before the next step.
    pub delay_after: Duration,
}

impl InputStep {
    /// Returns the QMP `input-send-event` arguments for this step.
    pub fn to_qmp_arguments(&self) -> serde_json::Value {
        let events: Vec<serde_json::Value> = self.events.iter().map(InputEvent::to_qmp).collect();
        serde_json::json!({ "events": events })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_raw_to_qmp() {
        let raw = serde_json::json!({ "type": "abs", "data": { "axis": "x", "value": 100 } });
        assert_eq!(InputEvent::Raw(raw.clone()).to_qmp(), raw);
    }

    #[test]
    fn test_step_to_qmp_arguments() {
        let first = serde_json::json!({ "type": "rel", "data": { "axis": "x", "value": 3 } });
        let second = serde_json::json!({ "type": "rel", "data": { "axis": "y", "value": -4 } });
        let step = InputStep {
            events: vec![InputEvent::Raw(first.clone()), InputEvent::Raw(second.clone())],
            delay_after: Duration::from_millis(20),
        };
        assert_eq!(step.to_qmp_arguments(), serde_json::json!({ "events": [first, second] }));
    }
}
