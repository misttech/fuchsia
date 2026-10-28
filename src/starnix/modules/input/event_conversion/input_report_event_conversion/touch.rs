// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_input_report as fir;
use sorted_vec_map::{SortedVecMap, SortedVecSet};
use starnix_types::time::timeval_from_time;
use starnix_uapi::uapi;

const DEFAULT_MAX_TOUCH_CONTACTS: usize = 10;
const LIFTED_TRACKING_ID: i32 = -1;

/// Maps a Fuchsia `TouchButton` to a Linux keycode.
pub fn touch_button_to_linux_key(button: fir::TouchButton) -> Option<u32> {
    match button {
        fir::TouchButton::Palm => Some(uapi::KEY_SLEEP),
        fir::TouchButton::SwipeUp => Some(uapi::KEY_UP),
        fir::TouchButton::SwipeLeft => Some(uapi::KEY_LEFT),
        fir::TouchButton::SwipeRight => Some(uapi::KEY_RIGHT),
        fir::TouchButton::SwipeDown => Some(uapi::KEY_DOWN),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
struct ContactState {
    pos_x: i32,
    pos_y: i32,
    pressure: Option<i32>,
    touch_major: Option<i32>,
    touch_minor: Option<i32>,
}

/// Converts `TouchInputReport` events into Linux Multi-Touch Protocol B `uapi::input_event`s.
#[derive(Debug, PartialEq)]
pub struct TouchReportConverter {
    pointer_id_to_slot_id: SortedVecMap<u32, usize>,
    slot_to_state: SortedVecMap<usize, ContactState>,
    pressed_touch_buttons: SortedVecSet<u16>,
    max_slots: usize,
}

impl Default for TouchReportConverter {
    fn default() -> Self {
        Self {
            pointer_id_to_slot_id: SortedVecMap::new(),
            slot_to_state: SortedVecMap::new(),
            pressed_touch_buttons: SortedVecSet::new(),
            max_slots: DEFAULT_MAX_TOUCH_CONTACTS,
        }
    }
}

impl TouchReportConverter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_max_slots(max_slots: usize) -> Self {
        Self {
            pointer_id_to_slot_id: SortedVecMap::new(),
            slot_to_state: SortedVecMap::new(),
            pressed_touch_buttons: SortedVecSet::new(),
            max_slots,
        }
    }

    pub fn from_touch_input_descriptor(input: &fir::TouchInputDescriptor) -> Self {
        let max_slots = input
            .max_contacts
            .map(|m| m as usize)
            .or_else(|| input.contacts.as_ref().map(|c| c.len()))
            .unwrap_or(DEFAULT_MAX_TOUCH_CONTACTS)
            .max(1);
        Self::with_max_slots(max_slots)
    }

    pub fn from_touch_descriptor(descriptor: &fir::TouchDescriptor) -> Self {
        descriptor.input.as_ref().map(Self::from_touch_input_descriptor).unwrap_or_default()
    }

    pub fn max_slots(&self) -> usize {
        self.max_slots
    }

    fn available_slot_id(&self) -> Option<usize> {
        let mut used_slots = bit_vec::BitVec::<u32>::from_elem(self.max_slots, false);
        for &slot in self.pointer_id_to_slot_id.values() {
            if slot < self.max_slots {
                used_slots.set(slot, true);
            }
        }
        used_slots.iter().position(|used| !used)
    }

    pub fn handle(
        &mut self,
        report: &fir::TouchInputReport,
        event_time: zx::MonotonicInstant,
    ) -> Vec<uapi::input_event> {
        let mut events = Vec::new();
        let time = timeval_from_time(event_time);

        let current_touch_buttons = report.pressed_buttons.as_ref().map(|buttons| {
            let mut set = SortedVecSet::new();
            for &button in buttons.iter() {
                if let Some(key) = touch_button_to_linux_key(button) {
                    set.insert(key as u16);
                }
            }
            set
        });

        if let Some(ref contacts) = report.contacts {
            let was_empty = self.pointer_id_to_slot_id.is_empty();
            let is_button_report_with_empty_contacts = contacts.is_empty()
                && current_touch_buttons
                    .as_ref()
                    .is_some_and(|current| *current != self.pressed_touch_buttons);

            let mut current_contact_ids = SortedVecSet::new();

            for contact in contacts.iter() {
                let Some(contact_id) = contact.contact_id else {
                    continue;
                };

                // Track contact_id immediately in current_contact_ids.
                // Linux MT Protocol B requires each slot/contact to appear at most once per sync frame.
                // If contact_id is duplicate in this report, drop it.
                if !current_contact_ids.insert(contact_id) {
                    continue;
                }

                // If coordinates are missing, skip emitting slot updates for this contact,
                // but keep it in current_contact_ids so it is not falsely considered lifted.
                let (Some(raw_pos_x), Some(raw_pos_y)) = (contact.position_x, contact.position_y)
                else {
                    continue;
                };
                let pos_x = crate::clamp_i64_to_i32(raw_pos_x);
                let pos_y = crate::clamp_i64_to_i32(raw_pos_y);

                let is_new = !self.pointer_id_to_slot_id.contains_key(&contact_id);
                let slot = if let Some(&existing_slot) = self.pointer_id_to_slot_id.get(&contact_id)
                {
                    existing_slot
                } else if let Some(new_slot) = self.available_slot_id() {
                    self.pointer_id_to_slot_id.insert(contact_id, new_slot);
                    new_slot
                } else {
                    // All slots exhausted, skip this contact.
                    continue;
                };

                let pressure = contact.pressure.map(crate::clamp_i64_to_i32);
                let (touch_major, touch_minor) =
                    match (contact.contact_width, contact.contact_height) {
                        (Some(w), Some(h)) => {
                            let major = std::cmp::max(w, h);
                            let minor = std::cmp::min(w, h);
                            (
                                Some(crate::clamp_i64_to_i32(major)),
                                Some(crate::clamp_i64_to_i32(minor)),
                            )
                        }
                        (Some(w), None) => (Some(crate::clamp_i64_to_i32(w)), None),
                        (None, Some(h)) => (Some(crate::clamp_i64_to_i32(h)), None),
                        (None, None) => (None, None),
                    };

                if is_new {
                    let new_state =
                        ContactState { pos_x, pos_y, pressure, touch_major, touch_minor };
                    events.push(uapi::input_event {
                        time,
                        type_: uapi::EV_ABS as u16,
                        code: uapi::ABS_MT_SLOT as u16,
                        value: slot as i32,
                    });
                    // Mask contact_id with 0x7fff_ffff so it never evaluates to negative (e.g. -1 for lift).
                    events.push(uapi::input_event {
                        time,
                        type_: uapi::EV_ABS as u16,
                        code: uapi::ABS_MT_TRACKING_ID as u16,
                        value: (contact_id & 0x7fff_ffff) as i32,
                    });
                    events.push(uapi::input_event {
                        time,
                        type_: uapi::EV_ABS as u16,
                        code: uapi::ABS_MT_POSITION_X as u16,
                        value: pos_x,
                    });
                    events.push(uapi::input_event {
                        time,
                        type_: uapi::EV_ABS as u16,
                        code: uapi::ABS_MT_POSITION_Y as u16,
                        value: pos_y,
                    });
                    if let Some(p) = pressure {
                        events.push(uapi::input_event {
                            time,
                            type_: uapi::EV_ABS as u16,
                            code: uapi::ABS_MT_PRESSURE as u16,
                            value: p,
                        });
                    }
                    if let Some(major) = touch_major {
                        events.push(uapi::input_event {
                            time,
                            type_: uapi::EV_ABS as u16,
                            code: uapi::ABS_MT_TOUCH_MAJOR as u16,
                            value: major,
                        });
                    }
                    if let Some(minor) = touch_minor {
                        events.push(uapi::input_event {
                            time,
                            type_: uapi::EV_ABS as u16,
                            code: uapi::ABS_MT_TOUCH_MINOR as u16,
                            value: minor,
                        });
                    }
                    self.slot_to_state.insert(slot, new_state);
                } else {
                    let old_state = self.slot_to_state.get(&slot);
                    let changed_x = old_state.map_or(true, |s| s.pos_x != pos_x);
                    let changed_y = old_state.map_or(true, |s| s.pos_y != pos_y);
                    let changed_pressure =
                        pressure.is_some() && old_state.map_or(true, |s| s.pressure != pressure);
                    let changed_major = touch_major.is_some()
                        && old_state.map_or(true, |s| s.touch_major != touch_major);
                    let changed_minor = touch_minor.is_some()
                        && old_state.map_or(true, |s| s.touch_minor != touch_minor);

                    if changed_x || changed_y || changed_pressure || changed_major || changed_minor
                    {
                        let new_state = ContactState {
                            pos_x,
                            pos_y,
                            pressure: pressure.or_else(|| old_state.and_then(|s| s.pressure)),
                            touch_major: touch_major
                                .or_else(|| old_state.and_then(|s| s.touch_major)),
                            touch_minor: touch_minor
                                .or_else(|| old_state.and_then(|s| s.touch_minor)),
                        };
                        events.push(uapi::input_event {
                            time,
                            type_: uapi::EV_ABS as u16,
                            code: uapi::ABS_MT_SLOT as u16,
                            value: slot as i32,
                        });
                        if changed_x {
                            events.push(uapi::input_event {
                                time,
                                type_: uapi::EV_ABS as u16,
                                code: uapi::ABS_MT_POSITION_X as u16,
                                value: pos_x,
                            });
                        }
                        if changed_y {
                            events.push(uapi::input_event {
                                time,
                                type_: uapi::EV_ABS as u16,
                                code: uapi::ABS_MT_POSITION_Y as u16,
                                value: pos_y,
                            });
                        }
                        if changed_pressure {
                            if let Some(p) = pressure {
                                events.push(uapi::input_event {
                                    time,
                                    type_: uapi::EV_ABS as u16,
                                    code: uapi::ABS_MT_PRESSURE as u16,
                                    value: p,
                                });
                            }
                        }
                        if changed_major {
                            if let Some(major) = touch_major {
                                events.push(uapi::input_event {
                                    time,
                                    type_: uapi::EV_ABS as u16,
                                    code: uapi::ABS_MT_TOUCH_MAJOR as u16,
                                    value: major,
                                });
                            }
                        }
                        if changed_minor {
                            if let Some(minor) = touch_minor {
                                events.push(uapi::input_event {
                                    time,
                                    type_: uapi::EV_ABS as u16,
                                    code: uapi::ABS_MT_TOUCH_MINOR as u16,
                                    value: minor,
                                });
                            }
                        }
                        self.slot_to_state.insert(slot, new_state);
                    }
                }
            }

            // Detect and emit lifted contacts unless this is a button report with empty contacts.
            if !is_button_report_with_empty_contacts {
                let removed_contacts: Vec<(u32, usize)> = self
                    .pointer_id_to_slot_id
                    .iter()
                    .filter(|(id, _)| !current_contact_ids.contains(id))
                    .map(|(&id, &slot)| (id, slot))
                    .collect();

                for (id, slot) in removed_contacts {
                    events.push(uapi::input_event {
                        time,
                        type_: uapi::EV_ABS as u16,
                        code: uapi::ABS_MT_SLOT as u16,
                        value: slot as i32,
                    });
                    events.push(uapi::input_event {
                        time,
                        type_: uapi::EV_ABS as u16,
                        code: uapi::ABS_MT_TRACKING_ID as u16,
                        value: LIFTED_TRACKING_ID,
                    });
                    self.pointer_id_to_slot_id.remove(&id);
                    self.slot_to_state.remove(&slot);
                }

                // Emit BTN_TOUCH transition if needed.
                let is_empty = self.pointer_id_to_slot_id.is_empty();
                if was_empty && !is_empty {
                    events.push(uapi::input_event {
                        time,
                        type_: uapi::EV_KEY as u16,
                        code: uapi::BTN_TOUCH as u16,
                        value: 1,
                    });
                } else if !was_empty && is_empty {
                    events.push(uapi::input_event {
                        time,
                        type_: uapi::EV_KEY as u16,
                        code: uapi::BTN_TOUCH as u16,
                        value: 0,
                    });
                }
            }
        }

        // Handle touch buttons (e.g. Palm, Swipes).
        if let Some(current_buttons) = current_touch_buttons {
            crate::emit_key_transitions(
                &mut self.pressed_touch_buttons,
                current_buttons,
                time,
                &mut events,
            );
        }

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
    fn test_touch_report_single_contact_lifecycle() {
        let mut converter = TouchReportConverter::new();
        let time = zx::MonotonicInstant::from_nanos(100_000_000);

        // 1. Initial contact down at (100, 200)
        let report_down = fir::TouchInputReport {
            contacts: Some(vec![fir::ContactInputReport {
                contact_id: Some(1),
                position_x: Some(100),
                position_y: Some(200),
                pressure: Some(50),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let events = converter.handle(&report_down, time);
        assert_eq!(
            events.iter().map(|e| (e.type_, e.code, e.value)).collect::<Vec<_>>(),
            vec![
                (uapi::EV_ABS as u16, uapi::ABS_MT_SLOT as u16, 0),
                (uapi::EV_ABS as u16, uapi::ABS_MT_TRACKING_ID as u16, 1),
                (uapi::EV_ABS as u16, uapi::ABS_MT_POSITION_X as u16, 100),
                (uapi::EV_ABS as u16, uapi::ABS_MT_POSITION_Y as u16, 200),
                (uapi::EV_ABS as u16, uapi::ABS_MT_PRESSURE as u16, 50),
                (uapi::EV_KEY as u16, uapi::BTN_TOUCH as u16, 1),
                (uapi::EV_SYN as u16, uapi::SYN_REPORT as u16, 0),
            ]
        );

        // 2. Contact move to (150, 250)
        let report_move = fir::TouchInputReport {
            contacts: Some(vec![fir::ContactInputReport {
                contact_id: Some(1),
                position_x: Some(150),
                position_y: Some(250),
                pressure: Some(60),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let events = converter.handle(&report_move, time);
        assert_eq!(
            events.iter().map(|e| (e.type_, e.code, e.value)).collect::<Vec<_>>(),
            vec![
                (uapi::EV_ABS as u16, uapi::ABS_MT_SLOT as u16, 0),
                (uapi::EV_ABS as u16, uapi::ABS_MT_POSITION_X as u16, 150),
                (uapi::EV_ABS as u16, uapi::ABS_MT_POSITION_Y as u16, 250),
                (uapi::EV_ABS as u16, uapi::ABS_MT_PRESSURE as u16, 60),
                (uapi::EV_SYN as u16, uapi::SYN_REPORT as u16, 0),
            ]
        );

        // 3. Contact lift (empty contacts)
        let report_up = fir::TouchInputReport { contacts: Some(vec![]), ..Default::default() };

        let events = converter.handle(&report_up, time);
        assert_eq!(
            events.iter().map(|e| (e.type_, e.code, e.value)).collect::<Vec<_>>(),
            vec![
                (uapi::EV_ABS as u16, uapi::ABS_MT_SLOT as u16, 0),
                (uapi::EV_ABS as u16, uapi::ABS_MT_TRACKING_ID as u16, -1),
                (uapi::EV_KEY as u16, uapi::BTN_TOUCH as u16, 0),
                (uapi::EV_SYN as u16, uapi::SYN_REPORT as u16, 0),
            ]
        );
    }

    #[::fuchsia::test]
    fn test_touch_report_multi_contact() {
        let mut converter = TouchReportConverter::new();
        let time = zx::MonotonicInstant::from_nanos(200_000_000);

        // Contact 1 down
        let report1 = fir::TouchInputReport {
            contacts: Some(vec![fir::ContactInputReport {
                contact_id: Some(10),
                position_x: Some(50),
                position_y: Some(60),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let events1 = converter.handle(&report1, time);
        assert_eq!(events1[0].code, uapi::ABS_MT_SLOT as u16);
        assert_eq!(events1[0].value, 0);

        // Contact 2 down alongside Contact 1
        let report2 = fir::TouchInputReport {
            contacts: Some(vec![
                fir::ContactInputReport {
                    contact_id: Some(10),
                    position_x: Some(52),
                    position_y: Some(62),
                    ..Default::default()
                },
                fir::ContactInputReport {
                    contact_id: Some(20),
                    position_x: Some(300),
                    position_y: Some(400),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };
        let events2 = converter.handle(&report2, time);
        // Slot 0 for contact 10, Slot 1 for contact 20
        assert!(events2.iter().any(|e| e.code == uapi::ABS_MT_SLOT as u16 && e.value == 0));
        assert!(events2.iter().any(|e| e.code == uapi::ABS_MT_SLOT as u16 && e.value == 1));
        assert!(events2.iter().any(|e| e.code == uapi::ABS_MT_TRACKING_ID as u16 && e.value == 20));

        // Contact 10 lifted, Contact 20 stays
        let report3 = fir::TouchInputReport {
            contacts: Some(vec![fir::ContactInputReport {
                contact_id: Some(20),
                position_x: Some(305),
                position_y: Some(405),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let events3 = converter.handle(&report3, time);
        // Slot 0 lifted
        assert!(events3.iter().any(|e| e.code == uapi::ABS_MT_SLOT as u16 && e.value == 0));
        assert!(events3.iter().any(|e| e.code == uapi::ABS_MT_TRACKING_ID as u16 && e.value == -1));
        // BTN_TOUCH remains active (not 0) because contact 20 is still down
        assert!(!events3.iter().any(|e| e.code == uapi::BTN_TOUCH as u16 && e.value == 0));

        // Contact 20 lifted
        let report4 = fir::TouchInputReport { contacts: Some(vec![]), ..Default::default() };
        let events4 = converter.handle(&report4, time);
        assert!(events4.iter().any(|e| e.code == uapi::ABS_MT_SLOT as u16 && e.value == 1));
        assert!(events4.iter().any(|e| e.code == uapi::ABS_MT_TRACKING_ID as u16 && e.value == -1));
        assert!(events4.iter().any(|e| e.code == uapi::BTN_TOUCH as u16 && e.value == 0));
    }

    #[::fuchsia::test]
    fn test_touch_report_buttons() {
        let mut converter = TouchReportConverter::new();
        let time = zx::MonotonicInstant::from_nanos(300_000_000);

        // Press Palm
        let report_press = fir::TouchInputReport {
            pressed_buttons: Some(vec![fir::TouchButton::Palm]),
            ..Default::default()
        };
        let events = converter.handle(&report_press, time);
        assert_eq!(
            events.iter().map(|e| (e.type_, e.code, e.value)).collect::<Vec<_>>(),
            vec![
                (uapi::EV_KEY as u16, uapi::KEY_SLEEP as u16, 1),
                (uapi::EV_SYN as u16, uapi::SYN_REPORT as u16, 0),
            ]
        );

        // Release Palm
        let report_release =
            fir::TouchInputReport { pressed_buttons: Some(vec![]), ..Default::default() };
        let events = converter.handle(&report_release, time);
        assert_eq!(
            events.iter().map(|e| (e.type_, e.code, e.value)).collect::<Vec<_>>(),
            vec![
                (uapi::EV_KEY as u16, uapi::KEY_SLEEP as u16, 0),
                (uapi::EV_SYN as u16, uapi::SYN_REPORT as u16, 0),
            ]
        );
    }

    #[::fuchsia::test]
    fn test_touch_report_contact_size_normalization() {
        let mut converter = TouchReportConverter::new();
        let time = zx::MonotonicInstant::from_nanos(100_000_000);

        // Case 1: height > width (should normalize major=height, minor=width)
        let report1 = fir::TouchInputReport {
            contacts: Some(vec![fir::ContactInputReport {
                contact_id: Some(1),
                position_x: Some(10),
                position_y: Some(20),
                contact_width: Some(30),
                contact_height: Some(80),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let events1 = converter.handle(&report1, time);
        assert!(events1.iter().any(|e| e.code == uapi::ABS_MT_TOUCH_MAJOR as u16 && e.value == 80));
        assert!(events1.iter().any(|e| e.code == uapi::ABS_MT_TOUCH_MINOR as u16 && e.value == 30));

        // Case 2: width > height (should normalize major=width, minor=height)
        let report2 = fir::TouchInputReport {
            contacts: Some(vec![fir::ContactInputReport {
                contact_id: Some(1),
                position_x: Some(15),
                position_y: Some(25),
                contact_width: Some(100),
                contact_height: Some(50),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let events2 = converter.handle(&report2, time);
        assert!(
            events2.iter().any(|e| e.code == uapi::ABS_MT_TOUCH_MAJOR as u16 && e.value == 100)
        );
        assert!(events2.iter().any(|e| e.code == uapi::ABS_MT_TOUCH_MINOR as u16 && e.value == 50));
    }

    #[::fuchsia::test]
    fn test_touch_report_tracking_id_sign_guard() {
        let mut converter = TouchReportConverter::new();
        let time = zx::MonotonicInstant::from_nanos(100_000_000);

        // contact_id with MSB set (u32::MAX) must be masked to non-negative and never equal -1
        let report = fir::TouchInputReport {
            contacts: Some(vec![fir::ContactInputReport {
                contact_id: Some(u32::MAX),
                position_x: Some(10),
                position_y: Some(20),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let events = converter.handle(&report, time);
        let tracking_id_event =
            events.iter().find(|e| e.code == uapi::ABS_MT_TRACKING_ID as u16).unwrap();
        assert_eq!(tracking_id_event.value, 0x7fff_ffff);
        assert!(tracking_id_event.value >= 0);
        assert_ne!(tracking_id_event.value, LIFTED_TRACKING_ID);
    }

    #[::fuchsia::test]
    fn test_touch_report_duplicate_contact_filtered() {
        let mut converter = TouchReportConverter::new();
        let time = zx::MonotonicInstant::from_nanos(100_000_000);

        // Report contains duplicate contact_id 1
        let report = fir::TouchInputReport {
            contacts: Some(vec![
                fir::ContactInputReport {
                    contact_id: Some(1),
                    position_x: Some(10),
                    position_y: Some(20),
                    ..Default::default()
                },
                fir::ContactInputReport {
                    contact_id: Some(1),
                    position_x: Some(15),
                    position_y: Some(25),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };
        let events = converter.handle(&report, time);
        // Should only have one ABS_MT_SLOT event and one ABS_MT_TRACKING_ID event
        let slot_count = events.iter().filter(|e| e.code == uapi::ABS_MT_SLOT as u16).count();
        assert_eq!(slot_count, 1);
        let tracking_count =
            events.iter().filter(|e| e.code == uapi::ABS_MT_TRACKING_ID as u16).count();
        assert_eq!(tracking_count, 1);
        assert!(events.iter().any(|e| e.code == uapi::ABS_MT_POSITION_X as u16 && e.value == 10));
        assert!(!events.iter().any(|e| e.code == uapi::ABS_MT_POSITION_X as u16 && e.value == 15));
    }

    #[::fuchsia::test]
    fn test_touch_report_missing_coordinates_does_not_lift() {
        let mut converter = TouchReportConverter::new();
        let time = zx::MonotonicInstant::from_nanos(100_000_000);

        // Step 1: Initial touch down
        let report1 = fir::TouchInputReport {
            contacts: Some(vec![fir::ContactInputReport {
                contact_id: Some(1),
                position_x: Some(10),
                position_y: Some(20),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let events1 = converter.handle(&report1, time);
        assert!(events1.iter().any(|e| e.code == uapi::BTN_TOUCH as u16 && e.value == 1));

        // Step 2: Contact present but missing coordinates
        let report2 = fir::TouchInputReport {
            contacts: Some(vec![fir::ContactInputReport {
                contact_id: Some(1),
                position_x: None,
                position_y: None,
                ..Default::default()
            }]),
            ..Default::default()
        };
        let events2 = converter.handle(&report2, time);
        // Must NOT emit lift event for contact 1 or release BTN_TOUCH
        assert!(
            !events2.iter().any(|e| e.code == uapi::ABS_MT_TRACKING_ID as u16 && e.value == -1)
        );
        assert!(!events2.iter().any(|e| e.code == uapi::BTN_TOUCH as u16 && e.value == 0));

        // Step 3: Truly lifted
        let report3 = fir::TouchInputReport { contacts: Some(vec![]), ..Default::default() };
        let events3 = converter.handle(&report3, time);
        assert!(events3.iter().any(|e| e.code == uapi::ABS_MT_TRACKING_ID as u16 && e.value == -1));
        assert!(events3.iter().any(|e| e.code == uapi::BTN_TOUCH as u16 && e.value == 0));
    }

    #[::fuchsia::test]
    fn test_touch_report_slot_exhaustion_and_reuse() {
        let mut converter = TouchReportConverter::with_max_slots(2);
        let time = zx::MonotonicInstant::from_nanos(100_000_000);

        // 3 contacts reported, but max_slots = 2
        let report1 = fir::TouchInputReport {
            contacts: Some(vec![
                fir::ContactInputReport {
                    contact_id: Some(1),
                    position_x: Some(10),
                    position_y: Some(20),
                    ..Default::default()
                },
                fir::ContactInputReport {
                    contact_id: Some(2),
                    position_x: Some(30),
                    position_y: Some(40),
                    ..Default::default()
                },
                fir::ContactInputReport {
                    contact_id: Some(3),
                    position_x: Some(50),
                    position_y: Some(60),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };
        let events1 = converter.handle(&report1, time);
        // Contact 1 gets slot 0, contact 2 gets slot 1, contact 3 is dropped
        assert!(events1.iter().any(|e| e.code == uapi::ABS_MT_SLOT as u16 && e.value == 0));
        assert!(events1.iter().any(|e| e.code == uapi::ABS_MT_SLOT as u16 && e.value == 1));
        assert!(!events1.iter().any(|e| e.code == uapi::ABS_MT_SLOT as u16 && e.value == 2));
        assert!(!events1.iter().any(|e| e.code == uapi::ABS_MT_POSITION_X as u16 && e.value == 50));

        // Contact 1 lifted, contact 2 stays
        let report2 = fir::TouchInputReport {
            contacts: Some(vec![fir::ContactInputReport {
                contact_id: Some(2),
                position_x: Some(32),
                position_y: Some(42),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let events2 = converter.handle(&report2, time);
        // Slot 0 (contact 1) is lifted and freed
        assert!(events2.iter().any(|e| e.code == uapi::ABS_MT_SLOT as u16 && e.value == 0));
        assert!(events2.iter().any(|e| e.code == uapi::ABS_MT_TRACKING_ID as u16 && e.value == -1));

        // Contact 3 is now reported alongside contact 2
        let report3 = fir::TouchInputReport {
            contacts: Some(vec![
                fir::ContactInputReport {
                    contact_id: Some(2),
                    position_x: Some(35),
                    position_y: Some(45),
                    ..Default::default()
                },
                fir::ContactInputReport {
                    contact_id: Some(3),
                    position_x: Some(52),
                    position_y: Some(62),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };
        let events3 = converter.handle(&report3, time);
        // Slot 0 (freed from contact 1) should now be reused for contact 3!
        assert!(events3.iter().any(|e| e.code == uapi::ABS_MT_SLOT as u16 && e.value == 0));
        assert!(events3.iter().any(|e| e.code == uapi::ABS_MT_TRACKING_ID as u16 && e.value == 3));
        assert!(events3.iter().any(|e| e.code == uapi::ABS_MT_POSITION_X as u16 && e.value == 52));
    }

    #[::fuchsia::test]
    fn test_empty_touch_reports_return_empty_events() {
        let time = zx::MonotonicInstant::from_nanos(400_000_000);
        assert!(
            TouchReportConverter::new().handle(&fir::TouchInputReport::default(), time).is_empty()
        );
    }

    #[::fuchsia::test]
    fn test_touch_report_clamping_out_of_range_axes() {
        let mut converter = TouchReportConverter::new();
        let time = zx::MonotonicInstant::from_nanos(100_000_000);
        let report = fir::TouchInputReport {
            contacts: Some(vec![fir::ContactInputReport {
                contact_id: Some(1),
                position_x: Some(i64::MAX),
                position_y: Some(i64::MIN),
                pressure: Some(i64::MAX),
                contact_width: Some(i32::MAX as i64 + 100),
                contact_height: Some(i64::MAX),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let events = converter.handle(&report, time);
        assert!(
            events.iter().any(|e| e.code == uapi::ABS_MT_POSITION_X as u16 && e.value == i32::MAX)
        );
        assert!(
            events.iter().any(|e| e.code == uapi::ABS_MT_POSITION_Y as u16 && e.value == i32::MIN)
        );
        assert!(
            events.iter().any(|e| e.code == uapi::ABS_MT_PRESSURE as u16 && e.value == i32::MAX)
        );
        assert!(
            events.iter().any(|e| e.code == uapi::ABS_MT_TOUCH_MAJOR as u16 && e.value == i32::MAX)
        );
        assert!(
            events.iter().any(|e| e.code == uapi::ABS_MT_TOUCH_MINOR as u16 && e.value == i32::MAX)
        );
    }

    #[::fuchsia::test]
    fn test_touch_report_deduplicate_unchanged_mt_axes() {
        let mut converter = TouchReportConverter::new();
        let time = zx::MonotonicInstant::from_nanos(100_000_000);

        // 1. Initial touch down
        let report1 = fir::TouchInputReport {
            contacts: Some(vec![fir::ContactInputReport {
                contact_id: Some(1),
                position_x: Some(10),
                position_y: Some(20),
                pressure: Some(30),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let events1 = converter.handle(&report1, time);
        assert!(!events1.is_empty());
        assert!(events1.iter().any(|e| e.code == uapi::ABS_MT_SLOT as u16 && e.value == 0));
        assert!(events1.iter().any(|e| e.code == uapi::ABS_MT_POSITION_X as u16 && e.value == 10));
        assert!(events1.iter().any(|e| e.code == uapi::ABS_MT_POSITION_Y as u16 && e.value == 20));
        assert!(events1.iter().any(|e| e.code == uapi::ABS_MT_PRESSURE as u16 && e.value == 30));

        // 2. Successive scan with unchanged state: should emit zero events!
        let events2 = converter.handle(&report1, time);
        assert!(events2.is_empty(), "Unchanged report should emit no events, got: {:?}", events2);

        // 3. Move pos_x only: should emit slot 0 and ABS_MT_POSITION_X only
        let report3 = fir::TouchInputReport {
            contacts: Some(vec![fir::ContactInputReport {
                contact_id: Some(1),
                position_x: Some(15),
                position_y: Some(20),
                pressure: Some(30),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let events3 = converter.handle(&report3, time);
        assert_eq!(
            events3.iter().map(|e| (e.type_, e.code, e.value)).collect::<Vec<_>>(),
            vec![
                (uapi::EV_ABS as u16, uapi::ABS_MT_SLOT as u16, 0),
                (uapi::EV_ABS as u16, uapi::ABS_MT_POSITION_X as u16, 15),
                (uapi::EV_SYN as u16, uapi::SYN_REPORT as u16, 0),
            ]
        );

        // 4. Change pressure only: should emit slot 0 and ABS_MT_PRESSURE only
        let report4 = fir::TouchInputReport {
            contacts: Some(vec![fir::ContactInputReport {
                contact_id: Some(1),
                position_x: Some(15),
                position_y: Some(20),
                pressure: Some(40),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let events4 = converter.handle(&report4, time);
        assert_eq!(
            events4.iter().map(|e| (e.type_, e.code, e.value)).collect::<Vec<_>>(),
            vec![
                (uapi::EV_ABS as u16, uapi::ABS_MT_SLOT as u16, 0),
                (uapi::EV_ABS as u16, uapi::ABS_MT_PRESSURE as u16, 40),
                (uapi::EV_SYN as u16, uapi::SYN_REPORT as u16, 0),
            ]
        );
    }

    #[::fuchsia::test]
    fn test_touch_report_empty_contacts_with_button_does_not_lift() {
        let mut converter = TouchReportConverter::new();
        let time = zx::MonotonicInstant::from_nanos(100_000_000);

        // 1. Initial touch down
        let report_down = fir::TouchInputReport {
            contacts: Some(vec![fir::ContactInputReport {
                contact_id: Some(1),
                position_x: Some(100),
                position_y: Some(100),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let events1 = converter.handle(&report_down, time);
        assert!(events1.iter().any(|e| e.code == uapi::BTN_TOUCH as u16 && e.value == 1));

        // 2. Palm button report with empty contacts
        let report_palm = fir::TouchInputReport {
            contacts: Some(vec![]),
            pressed_buttons: Some(vec![fir::TouchButton::Palm]),
            ..Default::default()
        };
        let events2 = converter.handle(&report_palm, time);
        assert!(events2.iter().any(|e| e.code == uapi::KEY_SLEEP as u16 && e.value == 1));
        // Contact should NOT be lifted, BTN_TOUCH = 0 should NOT be emitted
        assert!(
            !events2.iter().any(|e| e.code == uapi::ABS_MT_TRACKING_ID as u16 && e.value == -1)
        );
        assert!(!events2.iter().any(|e| e.code == uapi::BTN_TOUCH as u16 && e.value == 0));

        // 3. Release Palm button with empty contacts
        let report_release_palm = fir::TouchInputReport {
            contacts: Some(vec![]),
            pressed_buttons: Some(vec![]),
            ..Default::default()
        };
        let events3 = converter.handle(&report_release_palm, time);
        assert!(events3.iter().any(|e| e.code == uapi::KEY_SLEEP as u16 && e.value == 0));
        // Still should NOT lift contacts because previous report had buttons
        assert!(
            !events3.iter().any(|e| e.code == uapi::ABS_MT_TRACKING_ID as u16 && e.value == -1)
        );
        assert!(!events3.iter().any(|e| e.code == uapi::BTN_TOUCH as u16 && e.value == 0));

        // 4. Normal lift report (empty contacts, no buttons currently or previously)
        let report_lift = fir::TouchInputReport {
            contacts: Some(vec![]),
            pressed_buttons: None,
            ..Default::default()
        };
        let events4 = converter.handle(&report_lift, time);
        assert!(events4.iter().any(|e| e.code == uapi::ABS_MT_TRACKING_ID as u16 && e.value == -1));
        assert!(events4.iter().any(|e| e.code == uapi::BTN_TOUCH as u16 && e.value == 0));
    }

    #[::fuchsia::test]
    fn test_touch_report_lift_while_button_remains_held() {
        let mut converter = TouchReportConverter::new();
        let time = zx::MonotonicInstant::from_nanos(100_000_000);

        // 1. Contact down + Palm button pressed
        let report_down = fir::TouchInputReport {
            contacts: Some(vec![fir::ContactInputReport {
                contact_id: Some(1),
                position_x: Some(100),
                position_y: Some(100),
                ..Default::default()
            }]),
            pressed_buttons: Some(vec![fir::TouchButton::Palm]),
            ..Default::default()
        };
        let events1 = converter.handle(&report_down, time);
        assert!(events1.iter().any(|e| e.code == uapi::BTN_TOUCH as u16 && e.value == 1));
        assert!(events1.iter().any(|e| e.code == uapi::KEY_SLEEP as u16 && e.value == 1));

        // 2. Finger lifted while Palm button is still held
        let report_lift = fir::TouchInputReport {
            contacts: Some(vec![]),
            pressed_buttons: Some(vec![fir::TouchButton::Palm]),
            ..Default::default()
        };
        let events2 = converter.handle(&report_lift, time);
        // Contact should be lifted even though Palm button is still pressed
        assert!(events2.iter().any(|e| e.code == uapi::ABS_MT_TRACKING_ID as u16 && e.value == -1));
        assert!(events2.iter().any(|e| e.code == uapi::BTN_TOUCH as u16 && e.value == 0));
        // Palm button state didn't change, so no KEY_SLEEP event
        assert!(!events2.iter().any(|e| e.code == uapi::KEY_SLEEP as u16));
    }

    #[::fuchsia::test]
    fn test_touch_report_lift_with_none_pressed_buttons_after_button_pressed() {
        let mut converter = TouchReportConverter::new();
        let time = zx::MonotonicInstant::from_nanos(100_000_000);

        // 1. Contact down + Palm button pressed
        let report_down = fir::TouchInputReport {
            contacts: Some(vec![fir::ContactInputReport {
                contact_id: Some(1),
                position_x: Some(100),
                position_y: Some(100),
                ..Default::default()
            }]),
            pressed_buttons: Some(vec![fir::TouchButton::Palm]),
            ..Default::default()
        };
        let events1 = converter.handle(&report_down, time);
        assert!(events1.iter().any(|e| e.code == uapi::BTN_TOUCH as u16 && e.value == 1));
        assert!(events1.iter().any(|e| e.code == uapi::KEY_SLEEP as u16 && e.value == 1));

        // 2. Contact lifted with pressed_buttons: None
        let report_lift = fir::TouchInputReport {
            contacts: Some(vec![]),
            pressed_buttons: None,
            ..Default::default()
        };
        let events2 = converter.handle(&report_lift, time);
        // Contact should be lifted
        assert!(events2.iter().any(|e| e.code == uapi::ABS_MT_TRACKING_ID as u16 && e.value == -1));
        assert!(events2.iter().any(|e| e.code == uapi::BTN_TOUCH as u16 && e.value == 0));
    }

    #[::fuchsia::test]
    fn test_touch_report_omitted_optional_axes_preserve_cached_values() {
        let mut converter = TouchReportConverter::new();
        let time = zx::MonotonicInstant::from_nanos(100_000_000);

        // 1. Initial touch down with pressure and dimensions
        let report1 = fir::TouchInputReport {
            contacts: Some(vec![fir::ContactInputReport {
                contact_id: Some(1),
                position_x: Some(10),
                position_y: Some(20),
                pressure: Some(40),
                contact_width: Some(30),
                contact_height: Some(15),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let events1 = converter.handle(&report1, time);
        assert!(events1.iter().any(|e| e.code == uapi::ABS_MT_PRESSURE as u16 && e.value == 40));
        assert!(events1.iter().any(|e| e.code == uapi::ABS_MT_TOUCH_MAJOR as u16 && e.value == 30));
        assert!(events1.iter().any(|e| e.code == uapi::ABS_MT_TOUCH_MINOR as u16 && e.value == 15));

        // 2. Report with unchanged coordinates and omitted optional axes (None)
        // Should NOT emit events (no spurious slot + syn frame)
        let report2 = fir::TouchInputReport {
            contacts: Some(vec![fir::ContactInputReport {
                contact_id: Some(1),
                position_x: Some(10),
                position_y: Some(20),
                pressure: None,
                contact_width: None,
                contact_height: None,
                ..Default::default()
            }]),
            ..Default::default()
        };
        let events2 = converter.handle(&report2, time);
        assert!(
            events2.is_empty(),
            "Expected no events when optional axes are omitted and coords unchanged, got: {:?}",
            events2
        );

        // 3. Move position_x with omitted optional axes: should emit slot + position_x only,
        // and retain cached pressure/dimensions.
        let report3 = fir::TouchInputReport {
            contacts: Some(vec![fir::ContactInputReport {
                contact_id: Some(1),
                position_x: Some(15),
                position_y: Some(20),
                pressure: None,
                contact_width: None,
                contact_height: None,
                ..Default::default()
            }]),
            ..Default::default()
        };
        let events3 = converter.handle(&report3, time);
        assert_eq!(
            events3.iter().map(|e| (e.type_, e.code, e.value)).collect::<Vec<_>>(),
            vec![
                (uapi::EV_ABS as u16, uapi::ABS_MT_SLOT as u16, 0),
                (uapi::EV_ABS as u16, uapi::ABS_MT_POSITION_X as u16, 15),
                (uapi::EV_SYN as u16, uapi::SYN_REPORT as u16, 0),
            ]
        );

        // 4. Report with same position_x: 15 and pressure: Some(40), dimensions: Some(30, 15).
        // Since cached state was preserved, these values match cache and emit nothing!
        let report4 = fir::TouchInputReport {
            contacts: Some(vec![fir::ContactInputReport {
                contact_id: Some(1),
                position_x: Some(15),
                position_y: Some(20),
                pressure: Some(40),
                contact_width: Some(30),
                contact_height: Some(15),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let events4 = converter.handle(&report4, time);
        assert!(
            events4.is_empty(),
            "Expected no events when reporting matching cached optional axes, got: {:?}",
            events4
        );
    }

    #[::fuchsia::test]
    fn test_touch_report_converter_from_descriptors() {
        // From TouchInputDescriptor with max_contacts
        let input_desc1 = fir::TouchInputDescriptor {
            max_contacts: Some(5),
            contacts: Some(vec![fir::ContactInputDescriptor::default(); 2]),
            ..Default::default()
        };
        let converter1 = TouchReportConverter::from_touch_input_descriptor(&input_desc1);
        assert_eq!(converter1.max_slots(), 5);

        // From TouchInputDescriptor with contacts len fallback
        let input_desc2 = fir::TouchInputDescriptor {
            max_contacts: None,
            contacts: Some(vec![fir::ContactInputDescriptor::default(); 3]),
            ..Default::default()
        };
        let converter2 = TouchReportConverter::from_touch_input_descriptor(&input_desc2);
        assert_eq!(converter2.max_slots(), 3);

        // From TouchInputDescriptor with max_contacts = 0 (clamped to 1)
        let input_desc_zero = fir::TouchInputDescriptor {
            max_contacts: Some(0),
            contacts: None,
            ..Default::default()
        };
        let converter_zero = TouchReportConverter::from_touch_input_descriptor(&input_desc_zero);
        assert_eq!(converter_zero.max_slots(), 1);

        // From TouchInputDescriptor with all None (defaults to DEFAULT_MAX_TOUCH_CONTACTS)
        let input_desc_default = fir::TouchInputDescriptor::default();
        let converter_default =
            TouchReportConverter::from_touch_input_descriptor(&input_desc_default);
        assert_eq!(converter_default.max_slots(), DEFAULT_MAX_TOUCH_CONTACTS);

        // From TouchDescriptor with input
        let touch_desc = fir::TouchDescriptor {
            input: Some(fir::TouchInputDescriptor { max_contacts: Some(8), ..Default::default() }),
            ..Default::default()
        };
        let converter3 = TouchReportConverter::from_touch_descriptor(&touch_desc);
        assert_eq!(converter3.max_slots(), 8);

        // From TouchDescriptor with None input
        let touch_desc_none = fir::TouchDescriptor::default();
        let converter4 = TouchReportConverter::from_touch_descriptor(&touch_desc_none);
        assert_eq!(converter4.max_slots(), DEFAULT_MAX_TOUCH_CONTACTS);
    }
}
