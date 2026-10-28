// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::input_device::InputDeviceInfoHandle;
use crossbeam::queue::SegQueue;
use fuchsia_inspect::Inspector;
use futures::FutureExt;
use starnix_core::fileops_impl_nonseekable;
use starnix_core::mm::{MemoryAccessor, MemoryAccessorExt};
use starnix_core::task::{CurrentTask, EventHandler, WaitCanceler, WaitQueue, Waiter};
use starnix_core::vfs::buffers::{InputBuffer, OutputBuffer};
use starnix_core::vfs::{FileObject, FileOps, fileops_impl_noop_sync};
use starnix_logging::{log_info, track_stub};
use starnix_sync::{InputFileInputFileLock, LockDepMutex};
use starnix_syscalls::{SUCCESS, SyscallArg, SyscallResult};
use starnix_types::time::duration_from_timeval;
use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::user_address::{ArchSpecific, MultiArchUserRef, UserAddress, UserRef};
use starnix_uapi::vfs::FdEvents;
use starnix_uapi::{
    ABS_CNT, ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_PRESSURE, ABS_MT_SLOT,
    ABS_MT_TOUCH_MAJOR, ABS_MT_TOUCH_MINOR, ABS_MT_TRACKING_ID, BTN_EXTRA, BTN_LEFT, BTN_MIDDLE,
    BTN_MISC, BTN_RIGHT, BTN_SIDE, BTN_TOUCH, EV_CNT, FF_CNT, INPUT_PROP_CNT, INPUT_PROP_DIRECT,
    INPUT_PROP_POINTER, KEY_CNT, KEY_DOWN, KEY_LEFT, KEY_OK, KEY_POWER, KEY_RIGHT, KEY_SLEEP,
    KEY_UP, KEY_VOLUMEDOWN, KEY_VOLUMEUP, LED_CNT, MSC_CNT, REL_CNT, REL_HWHEEL, REL_WHEEL, REL_X,
    REL_Y, SW_CNT, errno, error, uapi,
};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use zerocopy::IntoBytes as _; // for `as_bytes()`

uapi::check_arch_independent_layout! {
    input_id {}
    input_absinfo {}
}

type InputEventPtr = MultiArchUserRef<uapi::input_event, uapi::arch32::input_event>;

pub struct InputFileStatus {
    /// The number of FIDL events received by this file from Fuchsia input system.
    ///
    /// We expect:
    /// fidl_events_received_count = fidl_events_ignored_count +
    ///                              fidl_events_unexpected_count +
    ///                              fidl_events_converted_count
    /// otherwise starnix ignored events unexpectedly.
    ///
    /// fidl_events_unexpected_count should be 0, if not it hints issues from upstream of ui stack.
    pub fidl_events_received_count: AtomicU64,

    /// The number of FIDL events ignored to this module’s representation of TouchEvent.
    pub fidl_events_ignored_count: AtomicU64,

    /// The unexpected number of FIDL events reached to this module should be filtered out
    /// earlier in the UI stack.
    /// It maybe unexpected format or unexpected order.
    pub fidl_events_unexpected_count: AtomicU64,

    /// The number of FIDL events converted to this module’s representation of TouchEvent.
    pub fidl_events_converted_count: AtomicU64,

    /// The number of uapi::input_events generated from TouchEvents.
    pub uapi_events_generated_count: AtomicU64,

    /// The event time of the last generated uapi::input_event.
    pub last_generated_uapi_event_timestamp_ns: AtomicI64,

    /// The number of uapi::input_events read from this input file by external process.
    pub uapi_events_read_count: AtomicU64,

    /// The event time of the last uapi::input_event read by external process.
    pub last_read_uapi_event_timestamp_ns: AtomicI64,

    /// Number of read calls.
    pub fd_read_count: AtomicU64,

    /// Number of notify calls.
    pub fd_notify_count: AtomicU64,

    /// Whether the file was opened without the NONBLOCK flag.
    pub opened_without_nonblock: AtomicBool,

    /// The timestamp when the file was opened.
    pub open_timestamp_ns: AtomicI64,

    /// Whether the file was closed (dropped).
    pub closed: AtomicBool,

    /// The timestamp when the file was closed.
    pub close_timestamp_ns: AtomicI64,

    /// The weak pointer to the InputFile.
    pub input_file: LockDepMutex<Weak<InputFile>, InputFileInputFileLock>,
}

impl InputFileStatus {
    fn new(node: &fuchsia_inspect::Node) -> Arc<Self> {
        let status = Arc::new(Self {
            fidl_events_received_count: AtomicU64::new(0),
            fidl_events_ignored_count: AtomicU64::new(0),
            fidl_events_unexpected_count: AtomicU64::new(0),
            fidl_events_converted_count: AtomicU64::new(0),
            uapi_events_generated_count: AtomicU64::new(0),
            last_generated_uapi_event_timestamp_ns: AtomicI64::new(0),
            uapi_events_read_count: AtomicU64::new(0),
            last_read_uapi_event_timestamp_ns: AtomicI64::new(0),
            fd_read_count: AtomicU64::new(0),
            fd_notify_count: AtomicU64::new(0),
            opened_without_nonblock: AtomicBool::new(false),
            open_timestamp_ns: AtomicI64::new(0),
            closed: AtomicBool::new(false),
            close_timestamp_ns: AtomicI64::new(0),
            input_file: Default::default(),
        });

        let cloned_status = status.clone();
        node.record_lazy_values("status", move || {
            let cloned_cloned_status = cloned_status.clone();
            async move {
                let is_dropped = cloned_cloned_status.input_file.lock().upgrade().is_none();
                if is_dropped && !cloned_cloned_status.closed.load(Ordering::Relaxed) {
                    cloned_cloned_status.closed.store(true, Ordering::Relaxed);
                }

                let inspector = Inspector::default();
                let root = inspector.root();
                root.record_uint(
                    "fd_read_count",
                    cloned_cloned_status.fd_read_count.load(Ordering::Relaxed),
                );
                root.record_uint(
                    "fd_notify_count",
                    cloned_cloned_status.fd_notify_count.load(Ordering::Relaxed),
                );
                root.record_bool(
                    "opened_without_nonblock",
                    cloned_cloned_status.opened_without_nonblock.load(Ordering::Relaxed),
                );
                root.record_int(
                    "open_timestamp_ns",
                    cloned_cloned_status.open_timestamp_ns.load(Ordering::Relaxed),
                );
                root.record_bool("closed", cloned_cloned_status.closed.load(Ordering::Relaxed));
                root.record_int(
                    "close_timestamp_ns",
                    cloned_cloned_status.close_timestamp_ns.load(Ordering::Relaxed),
                );
                root.record_uint(
                    "fidl_events_received_count",
                    cloned_cloned_status.fidl_events_received_count.load(Ordering::Relaxed),
                );
                root.record_uint(
                    "fidl_events_ignored_count",
                    cloned_cloned_status.fidl_events_ignored_count.load(Ordering::Relaxed),
                );
                root.record_uint(
                    "fidl_events_unexpected_count",
                    cloned_cloned_status.fidl_events_unexpected_count.load(Ordering::Relaxed),
                );
                root.record_uint(
                    "fidl_events_converted_count",
                    cloned_cloned_status.fidl_events_converted_count.load(Ordering::Relaxed),
                );
                root.record_uint(
                    "uapi_events_generated_count",
                    cloned_cloned_status.uapi_events_generated_count.load(Ordering::Relaxed),
                );
                root.record_int(
                    "last_generated_uapi_event_timestamp_ns",
                    cloned_cloned_status
                        .last_generated_uapi_event_timestamp_ns
                        .load(Ordering::Relaxed),
                );
                root.record_uint(
                    "uapi_events_read_count",
                    cloned_cloned_status.uapi_events_read_count.load(Ordering::Relaxed),
                );
                root.record_int(
                    "last_read_uapi_event_timestamp_ns",
                    cloned_cloned_status.last_read_uapi_event_timestamp_ns.load(Ordering::Relaxed),
                );
                Ok(inspector)
            }
            .boxed()
        });

        status
    }

    pub fn count_received_events(&self, count: u64) {
        self.fidl_events_received_count.fetch_add(count, Ordering::Relaxed);
    }

    pub fn count_ignored_events(&self, count: u64) {
        self.fidl_events_ignored_count.fetch_add(count, Ordering::Relaxed);
    }

    pub fn count_unexpected_events(&self, count: u64) {
        self.fidl_events_unexpected_count.fetch_add(count, Ordering::Relaxed);
    }

    pub fn count_converted_events(&self, count: u64) {
        self.fidl_events_converted_count.fetch_add(count, Ordering::Relaxed);
    }

    pub fn count_generated_events(&self, count: u64, event_time_ns: i64) {
        self.uapi_events_generated_count.fetch_add(count, Ordering::Relaxed);
        self.last_generated_uapi_event_timestamp_ns.store(event_time_ns, Ordering::Relaxed);
    }

    pub fn count_read_events(&self, count: u64, event_time_ns: i64) {
        self.uapi_events_read_count.fetch_add(count, Ordering::Relaxed);
        self.last_read_uapi_event_timestamp_ns.store(event_time_ns, Ordering::Relaxed);
    }

    pub fn count_fd_read_calls(&self) {
        self.fd_read_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn count_fd_notify_calls(&self) {
        self.fd_notify_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_opened_without_nonblock(&self) {
        self.opened_without_nonblock.store(true, Ordering::Relaxed);
    }

    pub fn set_open_timestamp(&self, timestamp: i64) {
        self.open_timestamp_ns.store(timestamp, Ordering::Relaxed);
    }

    pub fn set_closed(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }

    pub fn set_closed_timestamp(&self, timestamp: i64) {
        self.close_timestamp_ns.store(timestamp, Ordering::Relaxed);
    }
}

pub struct InputFile {
    driver_version: u32,
    info: InputDeviceInfoHandle,
    pub config: InputFileConfig,
    events: SegQueue<LinuxEventWithTraceId>,
    waiters: WaitQueue,
    // InputFile will be initialized with an InputFileStatus that holds Inspect data
    // `None` for Uinput InputFiles
    pub inspect_status: Option<Arc<InputFileStatus>>,
}

pub struct LinuxEventWithTraceId {
    pub event: uapi::input_event,
    pub trace_id: Option<fuchsia_trace::Id>,
}

impl LinuxEventWithTraceId {
    pub fn new(event: uapi::input_event) -> Self {
        match event.type_ as u32 {
            uapi::EV_SYN => {
                let trace_id = fuchsia_trace::Id::new();
                fuchsia_trace::duration!("input", "linux_event_create");
                fuchsia_trace::flow_begin!("input", "linux_event", trace_id);
                LinuxEventWithTraceId { event: event, trace_id: Some(trace_id) }
            }
            // EV_SYN marks the end of a complete input event. Other event types are its properties,
            // so they don't initiate a trace.
            _ => LinuxEventWithTraceId { event: event, trace_id: None },
        }
    }
}

/// Returns the minimum number of bytes required to store `n_bits` bits.
pub const fn min_bytes(n_bits: u32) -> usize {
    ((n_bits as usize) + 7) / 8
}

/// Parses a Fuchsia `Axis` into Linux `uapi::input_absinfo`.
pub fn parse_axis_info(axis: &fidl_fuchsia_input_report::Axis) -> uapi::input_absinfo {
    let minimum = axis.range.min.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    let maximum = axis.range.max.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    let resolution = match axis.unit.type_ {
        fidl_fuchsia_input_report::UnitType::Meters => {
            // Linux evdev resolution is in units per mm (10^-3 m).
            // resolution = 10^(-3 - exponent).
            match (-3i32).checked_sub(axis.unit.exponent) {
                Some(p) if (0..=9).contains(&p) => 10i32.pow(p as u32),
                _ => 0,
            }
        }
        _ => 0,
    };
    uapi::input_absinfo { minimum, maximum, resolution, ..Default::default() }
}

/// Dynamic configuration of capabilities and axis ranges for an [`InputFile`].
#[derive(Clone, Debug)]
pub struct InputFileConfig {
    pub supported_event_types: BitSet<{ min_bytes(EV_CNT) }>,
    pub supported_keys: BitSet<{ min_bytes(KEY_CNT) }>,
    pub supported_position_attributes: BitSet<{ min_bytes(ABS_CNT) }>,
    pub supported_motion_attributes: BitSet<{ min_bytes(REL_CNT) }>,
    pub supported_switches: BitSet<{ min_bytes(SW_CNT) }>,
    pub supported_leds: BitSet<{ min_bytes(LED_CNT) }>,
    pub supported_haptics: BitSet<{ min_bytes(FF_CNT) }>,
    pub supported_misc_features: BitSet<{ min_bytes(MSC_CNT) }>,
    pub properties: BitSet<{ min_bytes(INPUT_PROP_CNT) }>,
    pub mt_slot_axis_info: uapi::input_absinfo,
    pub mt_tracking_id_axis_info: uapi::input_absinfo,
    pub mt_pressure_axis_info: uapi::input_absinfo,
    pub mt_touch_major_axis_info: uapi::input_absinfo,
    pub mt_touch_minor_axis_info: uapi::input_absinfo,
    pub x_axis_info: uapi::input_absinfo,
    pub y_axis_info: uapi::input_absinfo,
}

fn absinfo_eq(a: &uapi::input_absinfo, b: &uapi::input_absinfo) -> bool {
    a.value == b.value
        && a.minimum == b.minimum
        && a.maximum == b.maximum
        && a.fuzz == b.fuzz
        && a.flat == b.flat
        && a.resolution == b.resolution
}

impl PartialEq for InputFileConfig {
    fn eq(&self, other: &Self) -> bool {
        self.supported_event_types == other.supported_event_types
            && self.supported_keys == other.supported_keys
            && self.supported_position_attributes == other.supported_position_attributes
            && self.supported_motion_attributes == other.supported_motion_attributes
            && self.supported_switches == other.supported_switches
            && self.supported_leds == other.supported_leds
            && self.supported_haptics == other.supported_haptics
            && self.supported_misc_features == other.supported_misc_features
            && self.properties == other.properties
            && absinfo_eq(&self.mt_slot_axis_info, &other.mt_slot_axis_info)
            && absinfo_eq(&self.mt_tracking_id_axis_info, &other.mt_tracking_id_axis_info)
            && absinfo_eq(&self.mt_pressure_axis_info, &other.mt_pressure_axis_info)
            && absinfo_eq(&self.mt_touch_major_axis_info, &other.mt_touch_major_axis_info)
            && absinfo_eq(&self.mt_touch_minor_axis_info, &other.mt_touch_minor_axis_info)
            && absinfo_eq(&self.x_axis_info, &other.x_axis_info)
            && absinfo_eq(&self.y_axis_info, &other.y_axis_info)
    }
}

impl Default for InputFileConfig {
    fn default() -> Self {
        Self {
            supported_event_types: BitSet::new(),
            supported_keys: BitSet::new(),
            supported_position_attributes: BitSet::new(),
            supported_motion_attributes: BitSet::new(),
            supported_switches: BitSet::new(),
            supported_leds: BitSet::new(),
            supported_haptics: BitSet::new(),
            supported_misc_features: BitSet::new(),
            properties: BitSet::new(),
            mt_slot_axis_info: uapi::input_absinfo::default(),
            mt_tracking_id_axis_info: uapi::input_absinfo::default(),
            mt_pressure_axis_info: uapi::input_absinfo::default(),
            mt_touch_major_axis_info: uapi::input_absinfo::default(),
            mt_touch_minor_axis_info: uapi::input_absinfo::default(),
            x_axis_info: uapi::input_absinfo::default(),
            y_axis_info: uapi::input_absinfo::default(),
        }
    }
}

impl InputFileConfig {
    pub fn from_device_descriptor(
        descriptor: &fidl_fuchsia_input_report::DeviceDescriptor,
    ) -> Self {
        let mut config = Self::default();

        if let Some(touch) = &descriptor.touch {
            config.supported_event_types.set(uapi::EV_ABS);
            config.supported_event_types.set(uapi::EV_KEY);

            let touch_type = touch.input.as_ref().and_then(|i| i.touch_type);
            match touch_type {
                Some(fidl_fuchsia_input_report::TouchType::Touchpad) => {
                    config.properties.set(INPUT_PROP_POINTER);
                    config.supported_keys.set(uapi::BTN_TOOL_FINGER);
                }
                _ => {
                    config.properties.set(INPUT_PROP_DIRECT);
                }
            }

            // Standard touch keys: only BTN_TOUCH unconditionally.
            config.supported_keys.set(BTN_TOUCH);

            // Multitouch and absolute coordinates matching touch_position_attributes().
            config.supported_position_attributes = touch_position_attributes();

            let mut max_contacts = 10;
            if let Some(input) = &touch.input {
                if let Some(contacts) = &input.contacts {
                    if let Some(first) = contacts.first() {
                        if let Some(x) = &first.position_x {
                            config.x_axis_info = parse_axis_info(x);
                        }
                        if let Some(y) = &first.position_y {
                            config.y_axis_info = parse_axis_info(y);
                        }
                        if let Some(pressure) = &first.pressure {
                            config.supported_position_attributes.set(ABS_MT_PRESSURE);
                            config.mt_pressure_axis_info = parse_axis_info(pressure);
                        }
                        match (&first.contact_width, &first.contact_height) {
                            (Some(w), Some(h)) => {
                                config.supported_position_attributes.set(ABS_MT_TOUCH_MAJOR);
                                config.supported_position_attributes.set(ABS_MT_TOUCH_MINOR);
                                if w.range.max >= h.range.max {
                                    config.mt_touch_major_axis_info = parse_axis_info(w);
                                    config.mt_touch_minor_axis_info = parse_axis_info(h);
                                } else {
                                    config.mt_touch_major_axis_info = parse_axis_info(h);
                                    config.mt_touch_minor_axis_info = parse_axis_info(w);
                                }
                            }
                            (Some(w), None) => {
                                config.supported_position_attributes.set(ABS_MT_TOUCH_MAJOR);
                                config.mt_touch_major_axis_info = parse_axis_info(w);
                            }
                            (None, Some(h)) => {
                                config.supported_position_attributes.set(ABS_MT_TOUCH_MAJOR);
                                config.mt_touch_major_axis_info = parse_axis_info(h);
                            }
                            (None, None) => {}
                        }
                    }
                }
                if let Some(contacts_limit) = input.max_contacts {
                    if contacts_limit > 0 {
                        max_contacts = contacts_limit;
                    }
                }
                if let Some(buttons) = &input.buttons {
                    for &button in buttons {
                        if let Some(key) =
                            starnix_modules_input_report_event_conversion::touch_button_to_linux_key(
                                button,
                            )
                        {
                            config.supported_keys.set(key);
                        }
                    }
                }
            }

            config.mt_slot_axis_info = uapi::input_absinfo {
                minimum: 0,
                maximum: i32::try_from(
                    max_contacts
                        .min(fidl_fuchsia_input_report::TOUCH_MAX_CONTACTS)
                        .saturating_sub(1),
                )
                .unwrap_or(i32::MAX),
                ..Default::default()
            };
            config.mt_tracking_id_axis_info =
                uapi::input_absinfo { minimum: 0, maximum: i32::MAX, ..Default::default() };
        }

        if let Some(keyboard) = &descriptor.keyboard {
            config.supported_event_types.set(uapi::EV_KEY);

            let mut has_keys = false;
            if let Some(input) = &keyboard.input {
                if let Some(keys3) = &input.keys3 {
                    for key in keys3 {
                        let keycode = starnix_modules_input_app_event_conversion::keymap::KEY_MAP
                            .fuchsia_input_key_to_linux_keycode(*key);
                        if keycode >= BTN_MISC && keycode < KEY_OK {
                            continue;
                        }
                        if keycode != uapi::KEY_RESERVED && (keycode as usize) < KEY_CNT as usize {
                            config.supported_keys.set(keycode);
                            has_keys = true;
                        }
                    }
                }
            }
            if !has_keys {
                config.supported_keys |= keyboard_key_attributes();
            }

            if let Some(output) = &keyboard.output {
                if let Some(leds) = &output.leds {
                    for led in leds {
                        let led_code = match led {
                            fidl_fuchsia_input_report::LedType::NumLock => Some(uapi::LED_NUML),
                            fidl_fuchsia_input_report::LedType::CapsLock => Some(uapi::LED_CAPSL),
                            fidl_fuchsia_input_report::LedType::ScrollLock => {
                                Some(uapi::LED_SCROLLL)
                            }
                            fidl_fuchsia_input_report::LedType::Compose => Some(uapi::LED_COMPOSE),
                            fidl_fuchsia_input_report::LedType::Kana => Some(uapi::LED_KANA),
                            _ => None,
                        };
                        if let Some(c) = led_code {
                            config.supported_event_types.set(uapi::EV_LED);
                            config.supported_leds.set(c);
                        }
                    }
                }
            }
        }

        if let Some(mouse) = &descriptor.mouse {
            config.supported_event_types.set(uapi::EV_KEY);
            config.supported_event_types.set(uapi::EV_REL);
            if !config.properties.get(INPUT_PROP_DIRECT) {
                config.properties.set(INPUT_PROP_POINTER);
            }

            let mut has_buttons = false;
            let mut has_rel = false;
            if let Some(input) = &mouse.input {
                if let Some(buttons) = &input.buttons {
                    for &button in buttons {
                        if let Some(btn) =
                            starnix_modules_input_report_event_conversion::mouse_button_to_linux_button(
                                button,
                            )
                        {
                            config.supported_keys.set(btn);
                            has_buttons = true;
                        }
                    }
                }
                if input.movement_x.is_some() {
                    config.supported_motion_attributes.set(REL_X);
                    has_rel = true;
                }
                if input.movement_y.is_some() {
                    config.supported_motion_attributes.set(REL_Y);
                    has_rel = true;
                }
                if input.scroll_v.is_some() {
                    config.supported_motion_attributes.set(REL_WHEEL);
                    has_rel = true;
                }
                if input.scroll_h.is_some() {
                    config.supported_motion_attributes.set(REL_HWHEEL);
                    has_rel = true;
                }
                if input.scroll_v.is_some() || input.scroll_h.is_some() {
                    config.supported_motion_attributes.set(REL_X);
                    config.supported_motion_attributes.set(REL_Y);
                }
            }

            if !has_buttons {
                config.supported_keys |= mouse_key_attributes();
            }

            if !has_rel {
                config.supported_motion_attributes |= mouse_motion_attributes();
            }
        }

        if let Some(consumer_control) = &descriptor.consumer_control {
            config.supported_event_types.set(uapi::EV_KEY);
            let mut has_buttons = false;
            if let Some(input) = &consumer_control.input {
                if let Some(buttons) = &input.buttons {
                    for button in buttons {
                        if let Some(key) =
                            starnix_modules_input_report_event_conversion::consumer_control_button_to_linux_key(
                                *button,
                            )
                        {
                            config.supported_keys.set(key);
                            has_buttons = true;
                        }
                    }
                }
            }
            if !has_buttons {
                config.supported_keys.set(KEY_POWER);
                config.supported_keys.set(KEY_VOLUMEUP);
                config.supported_keys.set(KEY_VOLUMEDOWN);
            }
        }

        config
    }
}

/// Returns appropriate `INPUT_PROP`-erties for a keyboard device.
fn keyboard_properties() -> BitSet<{ min_bytes(INPUT_PROP_CNT) }> {
    let mut attrs = BitSet::new();
    attrs.set(INPUT_PROP_DIRECT);
    attrs
}

/// Returns appropriate `KEY`-board related flags for a touchscreen device.
fn touch_key_attributes() -> BitSet<{ min_bytes(KEY_CNT) }> {
    let mut attrs = BitSet::new();
    attrs.set(BTN_TOUCH);
    attrs.set(BTN_MISC); // Include BTN_MISC as a catchall key event.
    attrs.set(KEY_SLEEP);
    attrs.set(KEY_UP);
    attrs.set(KEY_LEFT);
    attrs.set(KEY_RIGHT);
    attrs.set(KEY_DOWN);

    attrs
}

/// Returns appropriate `ABS`-olute position related flags for a touchscreen device.
fn touch_position_attributes() -> BitSet<{ min_bytes(ABS_CNT) }> {
    let mut attrs = BitSet::new();
    attrs.set(ABS_MT_SLOT);
    attrs.set(ABS_MT_TRACKING_ID);
    attrs.set(ABS_MT_POSITION_X);
    attrs.set(ABS_MT_POSITION_Y);
    attrs
}

/// Returns appropriate `INPUT_PROP`-erties for a touchscreen device.
fn touch_properties() -> BitSet<{ min_bytes(INPUT_PROP_CNT) }> {
    let mut attrs = BitSet::new();
    attrs.set(INPUT_PROP_DIRECT);
    attrs
}

/// Returns appropriate `KEY`-board related flags for a keyboard device.
fn keyboard_key_attributes() -> BitSet<{ min_bytes(KEY_CNT) }> {
    let mut attrs = BitSet::new();
    for keycode in starnix_modules_input_app_event_conversion::keymap::KEY_MAP.all_linux_keycodes()
    {
        // `KEY_MAP` also carries a block of test-only keycodes (see b/311425670), 31 of which
        // fall in the `BTN_*` range. Advertising those would make evdev clients classify this
        // device as a gamepad in addition to a keyboard, which is the opposite of what this
        // capability list is for. No real `KEY_*` constant lives in `BTN_MISC..KEY_OK`.
        if keycode >= BTN_MISC && keycode < KEY_OK {
            continue;
        }
        if (keycode as usize) < KEY_CNT as usize {
            attrs.set(keycode);
        }
    }
    attrs.set(BTN_MISC);
    attrs.set(KEY_POWER);
    attrs.set(KEY_VOLUMEUP);
    attrs.set(KEY_VOLUMEDOWN);
    attrs
}

/// Returns appropriate `ABS`-olute position related flags for a keyboard device.
fn keyboard_position_attributes() -> BitSet<{ min_bytes(ABS_CNT) }> {
    BitSet::new()
}

/// Returns appropriate `KEY`-board/button related flags for a mouse device.
fn mouse_key_attributes() -> BitSet<{ min_bytes(KEY_CNT) }> {
    let mut attrs = BitSet::new();
    // In Linux UAPI, BTN_LEFT has the numeric value 0x110, which is also aliased to BTN_MOUSE.
    // Evdev and libevdev treat BTN_MOUSE as the primary indicator that a device is a mouse:
    // https://cs.opensource.google/fuchsia/fuchsia/+/main:third_party/android/platform/external/libevdev/libevdev/libevdev.c;l=1134-1140;drc=7007dd6442654da8be96df23cf632ae0e87d7b30
    attrs.set(BTN_LEFT);
    attrs.set(BTN_RIGHT);
    attrs.set(BTN_MIDDLE);
    attrs.set(BTN_SIDE);
    attrs.set(BTN_EXTRA);
    attrs
}

/// Returns appropriate `REL`-ative motion related flags for a mouse device.
fn mouse_motion_attributes() -> BitSet<{ min_bytes(REL_CNT) }> {
    let mut attrs = BitSet::new();
    attrs.set(REL_X);
    attrs.set(REL_Y);
    attrs.set(REL_WHEEL);
    attrs.set(REL_HWHEEL);
    attrs
}

/// Returns appropriate `INPUT_PROP`-erties for a mouse device.
fn mouse_properties() -> BitSet<{ min_bytes(INPUT_PROP_CNT) }> {
    let mut attrs = BitSet::new();
    // INPUT_PROP_POINTER indicates a mouse/pointer device.
    // INPUT_PROP_DIRECT is for direct touchscreens and must NOT be set for a mouse.
    attrs.set(INPUT_PROP_POINTER);
    attrs
}

impl InputFile {
    // Per https://www.linuxjournal.com/article/6429, the driver version is 32-bits wide,
    // and interpreted as:
    // * [31-16]: version
    // * [15-08]: minor
    // * [07-00]: patch level
    const DRIVER_VERSION: u32 = 0;

    /// Creates an `InputFile` instance suitable for emulating a touchscreen.
    ///
    /// # Parameters
    /// - `info`: input device information.
    /// - `width`: width of screen.
    /// - `height`: height of screen.
    /// - `node`: The inspect node for the parent device of "touch_input_file".
    pub fn new_touch(
        info: InputDeviceInfoHandle,
        width: i32,
        height: i32,
        node: &fuchsia_inspect::Node,
    ) -> Self {
        // Fuchsia scales the position reported by the touch sensor to fit view coordinates.
        // Hence, the range of touch positions is exactly the same as the range of view
        // coordinates.
        let config = InputFileConfig {
            supported_event_types: BitSet::list([uapi::EV_ABS]),
            supported_keys: touch_key_attributes(),
            supported_position_attributes: touch_position_attributes(),
            properties: touch_properties(),
            mt_slot_axis_info: uapi::input_absinfo {
                minimum: 0,
                maximum: 10,
                ..uapi::input_absinfo::default()
            },
            mt_tracking_id_axis_info: uapi::input_absinfo {
                minimum: 0,
                maximum: i32::MAX,
                ..uapi::input_absinfo::default()
            },
            x_axis_info: uapi::input_absinfo {
                minimum: 0,
                maximum: i32::from(width),
                // TODO(https://fxbug.dev/42075436): `value` field should contain the most recent
                // X position.
                ..uapi::input_absinfo::default()
            },
            y_axis_info: uapi::input_absinfo {
                minimum: 0,
                maximum: i32::from(height),
                // TODO(https://fxbug.dev/42075436): `value` field should contain the most recent
                // Y position.
                ..uapi::input_absinfo::default()
            },
            ..Default::default()
        };
        Self::new_from_config(info, config, node)
    }

    /// Creates an `InputFile` instance suitable for emulating a keyboard.
    ///
    /// # Parameters
    /// - `info`: input device information.
    /// - `node`: The inspect node for the parent device of "keyboard_input_file".
    pub fn new_keyboard(info: InputDeviceInfoHandle, node: &fuchsia_inspect::Node) -> Self {
        let config = InputFileConfig {
            supported_event_types: BitSet::list([uapi::EV_KEY]),
            supported_keys: keyboard_key_attributes(),
            supported_position_attributes: keyboard_position_attributes(),
            properties: keyboard_properties(),
            ..Default::default()
        };
        Self::new_from_config(info, config, node)
    }

    /// Creates an `InputFile` instance suitable for emulating a mouse.
    ///
    /// # Parameters
    /// - `info`: input device information.
    /// - `node`: The inspect node for the parent device of "mouse_input_file".
    pub fn new_mouse(info: InputDeviceInfoHandle, node: &fuchsia_inspect::Node) -> Self {
        let config = InputFileConfig {
            // Mice report relative motion via EV_REL and buttons via EV_KEY.
            // Absolute motion (EV_ABS) is not supported to avoid misclassification
            // as a touch digitizer by libinput/Android EventHub.
            supported_event_types: BitSet::list([uapi::EV_KEY, uapi::EV_REL]),
            supported_keys: mouse_key_attributes(),
            supported_motion_attributes: mouse_motion_attributes(),
            properties: mouse_properties(),
            ..Default::default()
        };
        Self::new_from_config(info, config, node)
    }

    /// Creates an `InputFile` instance from a pre-configured [`InputFileConfig`].
    pub fn new_from_config(
        info: InputDeviceInfoHandle,
        config: InputFileConfig,
        node: &fuchsia_inspect::Node,
    ) -> Self {
        Self {
            driver_version: Self::DRIVER_VERSION,
            info,
            config,
            events: SegQueue::new(),
            waiters: WaitQueue::default(),
            inspect_status: Some(InputFileStatus::new(node)),
        }
    }

    /// Creates an `InputFile` instance by parsing a Fuchsia [`fidl_fuchsia_input_report::DeviceDescriptor`].
    #[cfg(test)]
    pub fn from_device_descriptor(
        info: InputDeviceInfoHandle,
        descriptor: &fidl_fuchsia_input_report::DeviceDescriptor,
        node: &fuchsia_inspect::Node,
    ) -> Self {
        let config = InputFileConfig::from_device_descriptor(descriptor);
        Self::new_from_config(info, config, node)
    }

    pub fn init_inspect_status(self: &Arc<Self>) {
        if let Some(inspect) = &self.inspect_status {
            *inspect.input_file.lock() = Arc::downgrade(self);
        }
    }

    pub fn add_events(&self, events: Vec<uapi::input_event>) {
        if events.is_empty() {
            return;
        }
        if let Some(inspect) = &self.inspect_status {
            inspect.count_fd_notify_calls();
        }
        for event in events {
            self.events.push(LinuxEventWithTraceId::new(event));
        }
        self.waiters.notify_fd_events(FdEvents::POLLIN);
    }

    pub fn read_events(&self, limit: usize) -> Vec<LinuxEventWithTraceId> {
        if let Some(inspect) = &self.inspect_status {
            inspect.count_fd_read_calls();
        }
        let mut events = vec![];
        for _ in 0..limit {
            if let Some(event) = self.events.pop() {
                events.push(event);
            } else {
                break;
            }
        }
        // We do not notify if the buffer was not enough to read all events.
        // `query_events` will still return `FdEvents::POLLIN` if there are remaining events,
        // so the caller can continue reading or poll again.
        events
    }
}

// Remove the variable part of the request with params, so we can identify it.
// Lowest 14 bits of the top 16 bits encode the buffer length in bytes.
// See https://cs.opensource.google/fuchsia/fuchsia/+/main:third_party/android/platform/bionic/libc/kernel/uapi/linux/input.h;l=82;drc=0f0c18f695543b15b852f68f297744d03d642a26
pub(crate) const EVIOC_VAR_LEN_MASK: u32 = !(uapi::_IOC_SIZEMASK << uapi::_IOC_SIZESHIFT);

pub(crate) const EVIOCGNAME_BASE: u32 = uapi::EVIOCGNAME_0 & EVIOC_VAR_LEN_MASK;
pub(crate) const EVIOCGPHYS_BASE: u32 = uapi::EVIOCGPHYS_0 & EVIOC_VAR_LEN_MASK;
pub(crate) const EVIOCGUNIQ_BASE: u32 = uapi::EVIOCGUNIQ_0 & EVIOC_VAR_LEN_MASK;
pub(crate) const EVIOCGKEY_BASE: u32 = uapi::EVIOCGKEY_0 & EVIOC_VAR_LEN_MASK;
pub(crate) const EVIOCGLED_BASE: u32 = uapi::EVIOCGLED_0 & EVIOC_VAR_LEN_MASK;
pub(crate) const EVIOCGSND_BASE: u32 = uapi::EVIOCGSND_0 & EVIOC_VAR_LEN_MASK;
pub(crate) const EVIOCGSW_BASE: u32 = uapi::EVIOCGSW_0 & EVIOC_VAR_LEN_MASK;
pub(crate) const EVIOCGPROP_BASE: u32 = uapi::EVIOCGPROP & EVIOC_VAR_LEN_MASK;
pub(crate) const EVIOCGBIT_0_BASE: u32 = uapi::EVIOCGBIT_0 & EVIOC_VAR_LEN_MASK;
pub(crate) const EVIOCGBIT_EV_KEY_BASE: u32 = uapi::EVIOCGBIT_EV_KEY & EVIOC_VAR_LEN_MASK;
pub(crate) const EVIOCGBIT_EV_ABS_BASE: u32 = uapi::EVIOCGBIT_EV_ABS & EVIOC_VAR_LEN_MASK;
pub(crate) const EVIOCGBIT_EV_REL_BASE: u32 = uapi::EVIOCGBIT_EV_REL & EVIOC_VAR_LEN_MASK;
pub(crate) const EVIOCGBIT_EV_SW_BASE: u32 = uapi::EVIOCGBIT_EV_SW & EVIOC_VAR_LEN_MASK;
pub(crate) const EVIOCGBIT_EV_LED_BASE: u32 = uapi::EVIOCGBIT_EV_LED & EVIOC_VAR_LEN_MASK;
pub(crate) const EVIOCGBIT_EV_FF_BASE: u32 = uapi::EVIOCGBIT_EV_FF & EVIOC_VAR_LEN_MASK;
pub(crate) const EVIOCGBIT_EV_MSC_BASE: u32 = uapi::EVIOCGBIT_EV_MSC & EVIOC_VAR_LEN_MASK;
pub(crate) const EVIOCGBIT_EV_SND_BASE: u32 = uapi::EVIOCGBIT_EV_SND & EVIOC_VAR_LEN_MASK;

pub(crate) const fn eviocgabs(axis: u32) -> u32 {
    uapi::EVIOCGABS_X + axis
}
pub(crate) const EVIOCGABS_MT_PRESSURE: u32 = eviocgabs(uapi::ABS_MT_PRESSURE);
pub(crate) const EVIOCGABS_MT_TOUCH_MAJOR: u32 = eviocgabs(uapi::ABS_MT_TOUCH_MAJOR);
pub(crate) const EVIOCGABS_MT_TOUCH_MINOR: u32 = eviocgabs(uapi::ABS_MT_TOUCH_MINOR);

impl FileOps for InputFile {
    fileops_impl_nonseekable!();
    fileops_impl_noop_sync!();

    fn open(&self, file: &FileObject, _current_task: &CurrentTask) -> Result<(), Errno> {
        if let Some(inspect) = &self.inspect_status {
            inspect.set_open_timestamp(zx::MonotonicInstant::get().into_nanos());
            if (file.flags() & OpenFlags::NONBLOCK) != OpenFlags::NONBLOCK {
                inspect.set_opened_without_nonblock();
            }
        }
        Ok(())
    }

    fn close(
        self: Box<Self>,
        _file: &starnix_core::vfs::FileObjectState,
        _current_task: &CurrentTask,
    ) {
        if let Some(inspect) = &self.inspect_status {
            inspect.set_closed();
            inspect.set_closed_timestamp(zx::MonotonicInstant::get().into_nanos());
        }
    }

    fn ioctl(
        &self,
        _file: &FileObject,
        current_task: &CurrentTask,
        request: u32,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        let user_addr = UserAddress::from(arg);
        match request {
            uapi::EVIOCGVERSION => {
                current_task.write_object(UserRef::new(user_addr), &self.driver_version)?;
                Ok(SUCCESS)
            }
            uapi::EVIOCGID => {
                let input_id = self.info.lock().input_id;
                current_task.write_object(UserRef::new(user_addr), &input_id)?;
                Ok(SUCCESS)
            }
            uapi::EVIOCGABS_MT_SLOT => {
                current_task
                    .write_object(UserRef::new(user_addr), &self.config.mt_slot_axis_info)?;
                Ok(SUCCESS)
            }
            uapi::EVIOCGABS_MT_TRACKING_ID => {
                current_task
                    .write_object(UserRef::new(user_addr), &self.config.mt_tracking_id_axis_info)?;
                Ok(SUCCESS)
            }
            uapi::EVIOCGABS_X | uapi::EVIOCGABS_MT_POSITION_X => {
                current_task.write_object(UserRef::new(user_addr), &self.config.x_axis_info)?;
                Ok(SUCCESS)
            }
            uapi::EVIOCGABS_Y | uapi::EVIOCGABS_MT_POSITION_Y => {
                current_task.write_object(UserRef::new(user_addr), &self.config.y_axis_info)?;
                Ok(SUCCESS)
            }
            EVIOCGABS_MT_PRESSURE => {
                current_task
                    .write_object(UserRef::new(user_addr), &self.config.mt_pressure_axis_info)?;
                Ok(SUCCESS)
            }
            EVIOCGABS_MT_TOUCH_MAJOR => {
                current_task
                    .write_object(UserRef::new(user_addr), &self.config.mt_touch_major_axis_info)?;
                Ok(SUCCESS)
            }
            EVIOCGABS_MT_TOUCH_MINOR => {
                current_task
                    .write_object(UserRef::new(user_addr), &self.config.mt_touch_minor_axis_info)?;
                Ok(SUCCESS)
            }
            uapi::EVIOCGRAB => {
                // Xorg and libevdev use EVIOCGRAB to obtain exclusive access to the device.
                track_stub!(TODO("https://fxbug.dev/322873200"), "EVIOCGRAB");
                Ok(SUCCESS)
            }

            request_with_params => {
                // The lowest 14 bits of the top 16 bits are the unsigned buffer length in bytes.
                let buffer_bytes_count =
                    ((request_with_params >> uapi::_IOC_SIZESHIFT) & uapi::_IOC_SIZEMASK) as usize;

                let dir = (request_with_params >> uapi::_IOC_DIRSHIFT) & uapi::_IOC_DIRMASK;
                let ioc_type = (request_with_params >> uapi::_IOC_TYPESHIFT) & uapi::_IOC_TYPEMASK;

                // Validate that this is an evdev read ioctl ('E').
                if dir != uapi::_IOC_READ || ioc_type != (b'E' as u32) {
                    track_stub!(
                        TODO("https://fxbug.dev/322873200"),
                        "input ioctl invalid type or dir",
                        request_with_params
                    );
                    return error!(EINVAL);
                }

                // Helper to copy a bitset slice to user memory.
                // Note: Linux evdev's bits_to_user returns the number of bytes copied, whereas
                // existing Starnix ioctl handlers and tests expect 0 (SUCCESS).
                let write_bits = |bits: &[u8]| -> Result<SyscallResult, Errno> {
                    if buffer_bytes_count == 0 {
                        return Ok(SUCCESS);
                    }
                    // Zero out the entire user buffer in case the user reads too much.
                    if current_task.zero(user_addr, buffer_bytes_count)? != buffer_bytes_count {
                        return error!(EFAULT);
                    }
                    let to_copy = std::cmp::min(bits.len(), buffer_bytes_count);
                    current_task.write_memory(user_addr, &bits[..to_copy])?;
                    Ok(SUCCESS)
                };

                // Helper to copy a NUL-terminated string to user memory and return bytes written
                // including the trailing NUL.
                let write_string = |bytes: &[u8]| -> Result<SyscallResult, Errno> {
                    if buffer_bytes_count == 0 {
                        return Ok(SUCCESS);
                    }
                    // Zero out the entire user buffer in case the user reads too much.
                    if current_task.zero(user_addr, buffer_bytes_count)? != buffer_bytes_count {
                        return error!(EFAULT);
                    }
                    let to_copy = std::cmp::min(bytes.len(), buffer_bytes_count.saturating_sub(1));
                    current_task.write_memory(user_addr, &bytes[..to_copy])?;
                    // String queries (EVIOCGNAME, EVIOCGPHYS) return the number of bytes written,
                    // including the trailing NUL.
                    Ok((to_copy + 1).into())
                };

                match request_with_params & EVIOC_VAR_LEN_MASK {
                    EVIOCGBIT_0_BASE => write_bits(&self.config.supported_event_types.bytes),
                    EVIOCGBIT_EV_KEY_BASE => write_bits(&self.config.supported_keys.bytes),
                    EVIOCGBIT_EV_ABS_BASE => {
                        write_bits(&self.config.supported_position_attributes.bytes)
                    }
                    EVIOCGBIT_EV_REL_BASE => {
                        write_bits(&self.config.supported_motion_attributes.bytes)
                    }
                    EVIOCGBIT_EV_SW_BASE => write_bits(&self.config.supported_switches.bytes),
                    EVIOCGBIT_EV_LED_BASE => write_bits(&self.config.supported_leds.bytes),
                    EVIOCGBIT_EV_FF_BASE => write_bits(&self.config.supported_haptics.bytes),
                    EVIOCGBIT_EV_MSC_BASE => write_bits(&self.config.supported_misc_features.bytes),
                    EVIOCGBIT_EV_SND_BASE => write_bits(&[]),
                    EVIOCGPROP_BASE => write_bits(&self.config.properties.bytes),
                    EVIOCGNAME_BASE => {
                        let name = self.info.lock().name.clone();
                        write_string(name.as_bytes())
                    }
                    EVIOCGPHYS_BASE => {
                        let phys = format!("starnix/{}", self.info.lock().name);
                        write_string(phys.as_bytes())
                    }
                    EVIOCGUNIQ_BASE => {
                        // Starnix synthetic devices do not have a unique identifier (serial/MAC).
                        // Linux returns -ENOENT when no uniq string is set.
                        error!(ENOENT)
                    }
                    EVIOCGKEY_BASE | EVIOCGLED_BASE | EVIOCGSND_BASE | EVIOCGSW_BASE => {
                        // Current-state queries: which keys are held down, which LEDs are
                        // lit, which sounds are playing, and which switches are toggled.
                        // These are distinct from the EVIOCGBIT capability queries above,
                        // which report what the device *can* do rather than what it is
                        // doing right now.
                        //
                        // Starnix does not track any of this per-device state, so report
                        // all-zeros: nothing held, lit, playing, or toggled. That is an
                        // honest answer for these devices rather than a fabrication, but
                        // it is still an approximation, hence the stub.
                        //
                        // These must not fall through to EINVAL. `xf86-input-evdev` issues
                        // all four unconditionally during PreInit and treats any failure as
                        // fatal, so returning an error here prevents X11 from bringing up
                        // the keyboard, mouse, and touchscreen entirely.
                        track_stub!(
                            TODO("https://fxbug.dev/322873200"),
                            "evdev current-state query",
                            request_with_params
                        );
                        write_bits(&[])
                    }
                    _ => {
                        track_stub!(
                            TODO("https://fxbug.dev/322873200"),
                            "input ioctl",
                            request_with_params
                        );
                        error!(EINVAL)
                    }
                }
            }
        }
    }

    fn read(
        &self,
        _file: &FileObject,
        current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        fuchsia_trace::duration!("input", "InputFile::read");
        debug_assert!(offset == 0);
        let input_event_size = InputEventPtr::size_of_object_for(current_task);

        // The limit of the buffer is determined by taking the available bytes
        // and using integer division on the size of uapi::input_event in bytes.
        let limit = data.available() / input_event_size;
        let events = self.read_events(limit);
        if events.is_empty() {
            // Returns `EAGAIN` for file is opened with or without `O_NONBLOCK`.
            log_info!("read() returning EAGAIN");
            return error!(EAGAIN);
        }

        let last_event_timeval = events.last().expect("events is nonempty").event.time;
        let last_event_time_ns = duration_from_timeval::<zx::MonotonicTimeline>(last_event_timeval)
            .unwrap()
            .into_nanos();
        self.inspect_status
            .clone()
            .map(|status| status.count_read_events(events.len() as u64, last_event_time_ns));

        for event in &events {
            if let Some(trace_id) = event.trace_id {
                fuchsia_trace::duration!("input", "linux_event_read");
                fuchsia_trace::flow_end!("input", "linux_event", trace_id);
            }
        }

        if current_task.is_arch32() {
            let events: Result<Vec<uapi::arch32::input_event>, _> =
                events.iter().map(|e| uapi::arch32::input_event::try_from(e.event)).collect();
            let events = events.map_err(|_| errno!(EINVAL))?;
            data.write_all(events.as_bytes())
        } else {
            let events: Vec<uapi::input_event> = events.iter().map(|e| e.event).collect();
            data.write_all(events.as_bytes())
        }
    }

    fn write(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        offset: usize,
        _data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        debug_assert!(offset == 0);
        track_stub!(TODO("https://fxbug.dev/322874385"), "write() on input device");
        error!(EOPNOTSUPP)
    }

    fn wait_async(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        waiter: &Waiter,
        events: FdEvents,
        handler: EventHandler,
    ) -> Option<WaitCanceler> {
        Some(self.waiters.wait_async_fd_events(waiter, events, handler))
    }

    fn query_events(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
    ) -> Result<FdEvents, Errno> {
        Ok(if self.events.is_empty() { FdEvents::empty() } else { FdEvents::POLLIN })
    }
}

pub struct ArcInputFile(pub Arc<InputFile>);

impl FileOps for ArcInputFile {
    fileops_impl_nonseekable!();
    fileops_impl_noop_sync!();

    fn open(&self, file: &FileObject, current_task: &CurrentTask) -> Result<(), Errno> {
        self.0.as_ref().open(file, current_task)
    }

    fn close(
        self: Box<Self>,
        _file: &starnix_core::vfs::FileObjectState,
        _current_task: &CurrentTask,
    ) {
        let arc_file = *self;
        if let Some(inspect) = &arc_file.0.inspect_status {
            inspect.set_closed();
            inspect.set_closed_timestamp(zx::MonotonicInstant::get().into_nanos());
        }
    }

    fn ioctl(
        &self,
        file: &FileObject,
        current_task: &CurrentTask,
        request: u32,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        self.0.as_ref().ioctl(file, current_task, request, arg)
    }

    fn read(
        &self,
        file: &FileObject,
        current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        self.0.as_ref().read(file, current_task, offset, data)
    }

    fn write(
        &self,
        file: &FileObject,
        current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        self.0.as_ref().write(file, current_task, offset, data)
    }

    fn wait_async(
        &self,
        file: &FileObject,
        current_task: &CurrentTask,
        waiter: &Waiter,
        events: FdEvents,
        handler: EventHandler,
    ) -> Option<WaitCanceler> {
        self.0.as_ref().wait_async(file, current_task, waiter, events, handler)
    }

    fn query_events(
        &self,
        file: &FileObject,
        current_task: &CurrentTask,
    ) -> Result<FdEvents, Errno> {
        self.0.as_ref().query_events(file, current_task)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BitSet<const NUM_BYTES: usize> {
    bytes: [u8; NUM_BYTES],
}

impl<const NUM_BYTES: usize> BitSet<{ NUM_BYTES }> {
    pub const fn new() -> Self {
        Self { bytes: [0; NUM_BYTES] }
    }

    pub const fn list<const N: usize>(bits: [u32; N]) -> Self {
        let mut bitset = Self::new();
        let mut i = 0;
        while i < bits.len() {
            bitset.set(bits[i]);
            i += 1;
        }
        bitset
    }

    pub const fn set(&mut self, bitnum: u32) {
        let bitnum = bitnum as usize;
        let byte = bitnum / 8;
        let bit = bitnum % 8;
        self.bytes[byte] |= 1 << bit;
    }

    pub fn get(&self, bitnum: u32) -> bool {
        let bitnum = bitnum as usize;
        let byte = bitnum / 8;
        let bit = bitnum % 8;
        (self.bytes[byte] & (1 << bit)) != 0
    }
}

impl<const NUM_BYTES: usize> std::ops::BitOrAssign for BitSet<{ NUM_BYTES }> {
    fn bitor_assign(&mut self, rhs: Self) {
        for i in 0..NUM_BYTES {
            self.bytes[i] |= rhs.bytes[i];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input_device::InputDeviceInfo;
    use std::sync::atomic::Ordering;

    #[test]
    fn test_keyboard_input_file_attributes() {
        let inspector = fuchsia_inspect::Inspector::default();
        let node = inspector.root();
        let keyboard_file = InputFile::new_keyboard(
            InputDeviceInfo::new(
                uapi::input_id { bustype: 0, vendor: 0, product: 0, version: 0 },
                "keyboard_test".to_string(),
            ),
            node,
        );

        // Event types: EV_KEY
        assert!(keyboard_file.config.supported_event_types.get(uapi::EV_KEY));
        assert!(!keyboard_file.config.supported_event_types.get(uapi::EV_REL));
        assert!(!keyboard_file.config.supported_event_types.get(uapi::EV_ABS));

        // Mapped Linux keys from KEY_MAP
        assert!(keyboard_file.config.supported_keys.get(uapi::KEY_A));
        assert!(keyboard_file.config.supported_keys.get(uapi::KEY_Z));
        assert!(keyboard_file.config.supported_keys.get(uapi::KEY_1));
        assert!(keyboard_file.config.supported_keys.get(uapi::KEY_ENTER));
        assert!(keyboard_file.config.supported_keys.get(uapi::KEY_SPACE));
        assert!(keyboard_file.config.supported_keys.get(uapi::KEY_ESC));
        assert!(keyboard_file.config.supported_keys.get(uapi::KEY_TAB));
        assert!(keyboard_file.config.supported_keys.get(uapi::KEY_BACKSPACE));
        assert!(keyboard_file.config.supported_keys.get(uapi::KEY_LEFTSHIFT));
        assert!(keyboard_file.config.supported_keys.get(uapi::KEY_LEFTCTRL));
        assert!(keyboard_file.config.supported_keys.get(uapi::KEY_LEFTALT));
        assert!(keyboard_file.config.supported_keys.get(uapi::KEY_POWER));
        assert!(keyboard_file.config.supported_keys.get(uapi::KEY_VOLUMEUP));
        assert!(keyboard_file.config.supported_keys.get(uapi::KEY_VOLUMEDOWN));
        assert!(keyboard_file.config.supported_keys.get(uapi::BTN_MISC));

        // Properties: INPUT_PROP_DIRECT
        assert!(keyboard_file.config.properties.get(uapi::INPUT_PROP_DIRECT));
        assert!(!keyboard_file.config.properties.get(uapi::INPUT_PROP_POINTER));
    }

    #[test]
    fn test_read_events_no_notify_when_buffer_full() {
        let inspector = fuchsia_inspect::Inspector::default();
        let node = inspector.root();
        let input_file = InputFile::new_touch(
            InputDeviceInfo::new(
                uapi::input_id { bustype: 0, vendor: 0, product: 0, version: 0 },
                "touch_test".to_string(),
            ),
            100,
            100,
            node,
        );

        // Add some events.
        let event1 = uapi::input_event {
            type_: uapi::EV_KEY as u16,
            code: 1,
            value: 1,
            ..Default::default()
        };
        let event2 = uapi::input_event {
            type_: uapi::EV_KEY as u16,
            code: 2,
            value: 1,
            ..Default::default()
        };
        input_file.add_events(vec![event1, event2]);

        // Verify that adding events triggered a notification.
        let notify_count =
            input_file.inspect_status.as_ref().unwrap().fd_notify_count.load(Ordering::Relaxed);
        assert_eq!(notify_count, 1);

        // Read with a limit of 1.
        let events = input_file.read_events(1);
        assert_eq!(events.len(), 1);

        // Verify that no additional notification was sent despite more events remaining.
        let notify_count =
            input_file.inspect_status.as_ref().unwrap().fd_notify_count.load(Ordering::Relaxed);
        assert_eq!(notify_count, 1);
    }

    #[test]
    fn test_bitset_set_and_get() {
        let mut bitset = BitSet::<4>::new();
        assert!(!bitset.get(0));
        assert!(!bitset.get(15));
        assert!(!bitset.get(31));

        bitset.set(0);
        bitset.set(15);
        bitset.set(31);

        assert!(bitset.get(0));
        assert!(bitset.get(15));
        assert!(bitset.get(31));
        assert!(!bitset.get(1));
        assert!(!bitset.get(14));
        assert!(!bitset.get(30));
    }

    #[test]
    fn test_mouse_capabilities() {
        let inspector = fuchsia_inspect::Inspector::default();
        let node = inspector.root();
        let mouse_file = InputFile::new_mouse(
            InputDeviceInfo::new(
                uapi::input_id { bustype: 0, vendor: 0, product: 0, version: 0 },
                "mouse_test".to_string(),
            ),
            node,
        );

        // EV_KEY and EV_REL supported, EV_ABS not supported.
        assert!(mouse_file.config.supported_event_types.get(uapi::EV_KEY));
        assert!(mouse_file.config.supported_event_types.get(uapi::EV_REL));
        assert!(!mouse_file.config.supported_event_types.get(uapi::EV_ABS));

        // Buttons supported: BTN_LEFT (BTN_MOUSE), BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA.
        assert!(mouse_file.config.supported_keys.get(BTN_LEFT));
        assert!(mouse_file.config.supported_keys.get(BTN_RIGHT));
        assert!(mouse_file.config.supported_keys.get(BTN_MIDDLE));
        assert!(mouse_file.config.supported_keys.get(BTN_SIDE));
        assert!(mouse_file.config.supported_keys.get(BTN_EXTRA));
        assert!(!mouse_file.config.supported_keys.get(BTN_TOUCH));

        // Relative motion: REL_X, REL_Y, REL_WHEEL, REL_HWHEEL.
        assert!(mouse_file.config.supported_motion_attributes.get(REL_X));
        assert!(mouse_file.config.supported_motion_attributes.get(REL_Y));
        assert!(mouse_file.config.supported_motion_attributes.get(REL_WHEEL));
        assert!(mouse_file.config.supported_motion_attributes.get(REL_HWHEEL));

        // Properties: INPUT_PROP_POINTER supported, INPUT_PROP_DIRECT not supported.
        assert!(mouse_file.config.properties.get(INPUT_PROP_POINTER));
        assert!(!mouse_file.config.properties.get(INPUT_PROP_DIRECT));
    }

    #[::fuchsia::test]
    fn test_parse_axis_info() {
        let axis_mm = fidl_fuchsia_input_report::Axis {
            range: fidl_fuchsia_input_report::Range { min: 0, max: 1080 },
            unit: fidl_fuchsia_input_report::Unit {
                type_: fidl_fuchsia_input_report::UnitType::Meters,
                exponent: -3,
            },
        };
        let absinfo = parse_axis_info(&axis_mm);
        assert_eq!(absinfo.minimum, 0);
        assert_eq!(absinfo.maximum, 1080);
        assert_eq!(absinfo.resolution, 1);

        let axis_um = fidl_fuchsia_input_report::Axis {
            range: fidl_fuchsia_input_report::Range { min: 0, max: 2400 },
            unit: fidl_fuchsia_input_report::Unit {
                type_: fidl_fuchsia_input_report::UnitType::Meters,
                exponent: -6,
            },
        };
        let absinfo_um = parse_axis_info(&axis_um);
        assert_eq!(absinfo_um.minimum, 0);
        assert_eq!(absinfo_um.maximum, 2400);
        assert_eq!(absinfo_um.resolution, 1000);

        let axis_none = fidl_fuchsia_input_report::Axis {
            range: fidl_fuchsia_input_report::Range { min: -100, max: 100 },
            unit: fidl_fuchsia_input_report::Unit {
                type_: fidl_fuchsia_input_report::UnitType::None,
                exponent: 0,
            },
        };
        let absinfo_none = parse_axis_info(&axis_none);
        assert_eq!(absinfo_none.minimum, -100);
        assert_eq!(absinfo_none.maximum, 100);
        assert_eq!(absinfo_none.resolution, 0);

        let axis_clamp = fidl_fuchsia_input_report::Axis {
            range: fidl_fuchsia_input_report::Range { min: i64::MIN, max: i64::MAX },
            unit: fidl_fuchsia_input_report::Unit {
                type_: fidl_fuchsia_input_report::UnitType::None,
                exponent: 0,
            },
        };
        let absinfo_clamp = parse_axis_info(&axis_clamp);
        assert_eq!(absinfo_clamp.minimum, i32::MIN);
        assert_eq!(absinfo_clamp.maximum, i32::MAX);

        let axis_extreme_max = fidl_fuchsia_input_report::Axis {
            range: fidl_fuchsia_input_report::Range { min: 0, max: 100 },
            unit: fidl_fuchsia_input_report::Unit {
                type_: fidl_fuchsia_input_report::UnitType::Meters,
                exponent: i32::MAX,
            },
        };
        let absinfo_extreme_max = parse_axis_info(&axis_extreme_max);
        assert_eq!(absinfo_extreme_max.resolution, 0);

        let axis_extreme_min = fidl_fuchsia_input_report::Axis {
            range: fidl_fuchsia_input_report::Range { min: 0, max: 100 },
            unit: fidl_fuchsia_input_report::Unit {
                type_: fidl_fuchsia_input_report::UnitType::Meters,
                exponent: i32::MIN,
            },
        };
        let absinfo_extreme_min = parse_axis_info(&axis_extreme_min);
        assert_eq!(absinfo_extreme_min.resolution, 0);
    }

    #[::fuchsia::test]
    fn test_input_file_from_touch_descriptor() {
        let inspector = fuchsia_inspect::Inspector::default();
        let descriptor = fidl_fuchsia_input_report::DeviceDescriptor {
            touch: Some(fidl_fuchsia_input_report::TouchDescriptor {
                input: Some(fidl_fuchsia_input_report::TouchInputDescriptor {
                    contacts: Some(vec![fidl_fuchsia_input_report::ContactInputDescriptor {
                        position_x: Some(fidl_fuchsia_input_report::Axis {
                            range: fidl_fuchsia_input_report::Range { min: 0, max: 1080 },
                            unit: fidl_fuchsia_input_report::Unit {
                                type_: fidl_fuchsia_input_report::UnitType::Meters,
                                exponent: -3,
                            },
                        }),
                        position_y: Some(fidl_fuchsia_input_report::Axis {
                            range: fidl_fuchsia_input_report::Range { min: 0, max: 2400 },
                            unit: fidl_fuchsia_input_report::Unit {
                                type_: fidl_fuchsia_input_report::UnitType::Meters,
                                exponent: -3,
                            },
                        }),
                        pressure: Some(fidl_fuchsia_input_report::Axis {
                            range: fidl_fuchsia_input_report::Range { min: 0, max: 255 },
                            unit: fidl_fuchsia_input_report::Unit {
                                type_: fidl_fuchsia_input_report::UnitType::None,
                                exponent: 0,
                            },
                        }),
                        contact_width: Some(fidl_fuchsia_input_report::Axis {
                            range: fidl_fuchsia_input_report::Range { min: 0, max: 100 },
                            unit: fidl_fuchsia_input_report::Unit {
                                type_: fidl_fuchsia_input_report::UnitType::None,
                                exponent: 0,
                            },
                        }),
                        contact_height: Some(fidl_fuchsia_input_report::Axis {
                            range: fidl_fuchsia_input_report::Range { min: 0, max: 100 },
                            unit: fidl_fuchsia_input_report::Unit {
                                type_: fidl_fuchsia_input_report::UnitType::None,
                                exponent: 0,
                            },
                        }),
                        ..Default::default()
                    }]),
                    max_contacts: Some(5),
                    touch_type: Some(fidl_fuchsia_input_report::TouchType::Touchscreen),
                    buttons: Some(vec![fidl_fuchsia_input_report::TouchButton::Palm]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let info = InputDeviceInfo::new(
            uapi::input_id { bustype: 3, vendor: 0x18d1, product: 0x5678, version: 1 },
            "test_touch".to_string(),
        );
        let file = InputFile::from_device_descriptor(info, &descriptor, inspector.root());

        assert!(file.config.supported_event_types.get(uapi::EV_ABS));
        assert!(file.config.supported_event_types.get(uapi::EV_KEY));
        assert!(file.config.properties.get(uapi::INPUT_PROP_DIRECT));
        assert!(!file.config.properties.get(uapi::INPUT_PROP_POINTER));

        assert_eq!(file.config.x_axis_info.minimum, 0);
        assert_eq!(file.config.x_axis_info.maximum, 1080);
        assert_eq!(file.config.x_axis_info.resolution, 1);

        assert_eq!(file.config.y_axis_info.minimum, 0);
        assert_eq!(file.config.y_axis_info.maximum, 2400);
        assert_eq!(file.config.y_axis_info.resolution, 1);

        assert_eq!(file.config.mt_slot_axis_info.minimum, 0);
        assert_eq!(file.config.mt_slot_axis_info.maximum, 4);
        assert_eq!(file.config.mt_tracking_id_axis_info.maximum, i32::MAX);

        assert_eq!(file.config.mt_pressure_axis_info.maximum, 255);
        assert_eq!(file.config.mt_touch_major_axis_info.maximum, 100);
        assert_eq!(file.config.mt_touch_minor_axis_info.maximum, 100);

        assert!(file.config.supported_position_attributes.get(uapi::ABS_MT_SLOT));
        assert!(file.config.supported_position_attributes.get(uapi::ABS_MT_TRACKING_ID));
        assert!(file.config.supported_position_attributes.get(uapi::ABS_MT_POSITION_X));
        assert!(file.config.supported_position_attributes.get(uapi::ABS_MT_POSITION_Y));
        assert!(file.config.supported_position_attributes.get(uapi::ABS_MT_PRESSURE));
        assert!(file.config.supported_position_attributes.get(uapi::ABS_MT_TOUCH_MAJOR));
        assert!(file.config.supported_position_attributes.get(uapi::ABS_MT_TOUCH_MINOR));
        assert!(!file.config.supported_position_attributes.get(uapi::ABS_X));
        assert!(!file.config.supported_position_attributes.get(uapi::ABS_Y));

        assert!(file.config.supported_keys.get(uapi::BTN_TOUCH));
        assert!(file.config.supported_keys.get(uapi::KEY_SLEEP));
        assert!(!file.config.supported_keys.get(uapi::KEY_UP));
    }

    #[::fuchsia::test]
    fn test_input_file_from_touch_descriptor_no_buttons() {
        let inspector = fuchsia_inspect::Inspector::default();
        let descriptor = fidl_fuchsia_input_report::DeviceDescriptor {
            touch: Some(fidl_fuchsia_input_report::TouchDescriptor {
                input: Some(fidl_fuchsia_input_report::TouchInputDescriptor {
                    contacts: Some(vec![fidl_fuchsia_input_report::ContactInputDescriptor {
                        position_x: Some(fidl_fuchsia_input_report::Axis {
                            range: fidl_fuchsia_input_report::Range { min: 0, max: 1080 },
                            unit: fidl_fuchsia_input_report::Unit {
                                type_: fidl_fuchsia_input_report::UnitType::None,
                                exponent: 0,
                            },
                        }),
                        position_y: Some(fidl_fuchsia_input_report::Axis {
                            range: fidl_fuchsia_input_report::Range { min: 0, max: 2400 },
                            unit: fidl_fuchsia_input_report::Unit {
                                type_: fidl_fuchsia_input_report::UnitType::None,
                                exponent: 0,
                            },
                        }),
                        ..Default::default()
                    }]),
                    buttons: None,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let info = InputDeviceInfo::new(
            uapi::input_id { bustype: 3, vendor: 0x18d1, product: 0x5678, version: 1 },
            "test_touch_no_buttons".to_string(),
        );
        let file = InputFile::from_device_descriptor(info, &descriptor, inspector.root());

        assert!(file.config.supported_keys.get(uapi::BTN_TOUCH));
        assert!(!file.config.supported_keys.get(uapi::KEY_SLEEP));
        assert!(!file.config.supported_keys.get(uapi::KEY_UP));
    }

    #[::fuchsia::test]
    fn test_input_file_from_touchpad_descriptor() {
        let inspector = fuchsia_inspect::Inspector::default();
        let descriptor = fidl_fuchsia_input_report::DeviceDescriptor {
            touch: Some(fidl_fuchsia_input_report::TouchDescriptor {
                input: Some(fidl_fuchsia_input_report::TouchInputDescriptor {
                    contacts: Some(vec![fidl_fuchsia_input_report::ContactInputDescriptor {
                        position_x: Some(fidl_fuchsia_input_report::Axis {
                            range: fidl_fuchsia_input_report::Range { min: 0, max: 1920 },
                            unit: fidl_fuchsia_input_report::Unit {
                                type_: fidl_fuchsia_input_report::UnitType::Meters,
                                exponent: -3,
                            },
                        }),
                        position_y: Some(fidl_fuchsia_input_report::Axis {
                            range: fidl_fuchsia_input_report::Range { min: 0, max: 1080 },
                            unit: fidl_fuchsia_input_report::Unit {
                                type_: fidl_fuchsia_input_report::UnitType::Meters,
                                exponent: -3,
                            },
                        }),
                        ..Default::default()
                    }]),
                    max_contacts: Some(2),
                    touch_type: Some(fidl_fuchsia_input_report::TouchType::Touchpad),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let info = InputDeviceInfo::new(
            uapi::input_id { bustype: 3, vendor: 0x18d1, product: 0x1234, version: 1 },
            "test_touchpad".to_string(),
        );
        let file = InputFile::from_device_descriptor(info, &descriptor, inspector.root());

        assert!(file.config.supported_event_types.get(uapi::EV_ABS));
        assert!(file.config.properties.get(uapi::INPUT_PROP_POINTER));
        assert!(!file.config.properties.get(uapi::INPUT_PROP_DIRECT));
        assert!(file.config.supported_keys.get(uapi::BTN_TOOL_FINGER));
        assert_eq!(file.config.mt_slot_axis_info.maximum, 1);
    }

    #[::fuchsia::test]
    fn test_input_file_from_keyboard_descriptor() {
        let inspector = fuchsia_inspect::Inspector::default();
        let descriptor = fidl_fuchsia_input_report::DeviceDescriptor {
            keyboard: Some(fidl_fuchsia_input_report::KeyboardDescriptor {
                input: Some(fidl_fuchsia_input_report::KeyboardInputDescriptor {
                    keys3: Some(vec![
                        fidl_fuchsia_input::Key::A,
                        fidl_fuchsia_input::Key::B,
                        fidl_fuchsia_input::Key::Enter,
                    ]),
                    ..Default::default()
                }),
                output: Some(fidl_fuchsia_input_report::KeyboardOutputDescriptor {
                    leds: Some(vec![fidl_fuchsia_input_report::LedType::CapsLock]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let info = InputDeviceInfo::new(
            uapi::input_id { bustype: 3, vendor: 1, product: 2, version: 3 },
            "test_keyboard".to_string(),
        );
        let file = InputFile::from_device_descriptor(info, &descriptor, inspector.root());

        assert!(file.config.supported_event_types.get(uapi::EV_KEY));
        assert!(file.config.supported_event_types.get(uapi::EV_LED));
        assert!(!file.config.properties.get(uapi::INPUT_PROP_DIRECT));

        assert!(file.config.supported_keys.get(uapi::KEY_A));
        assert!(file.config.supported_keys.get(uapi::KEY_B));
        assert!(file.config.supported_keys.get(uapi::KEY_ENTER));
        assert!(file.config.supported_leds.get(uapi::LED_CAPSL));
    }

    #[::fuchsia::test]
    fn test_input_file_from_keyboard_descriptor_fallback_keys() {
        let inspector = fuchsia_inspect::Inspector::default();
        let descriptor = fidl_fuchsia_input_report::DeviceDescriptor {
            keyboard: Some(fidl_fuchsia_input_report::KeyboardDescriptor {
                input: Some(fidl_fuchsia_input_report::KeyboardInputDescriptor {
                    keys3: None,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let info = InputDeviceInfo::new(
            uapi::input_id { bustype: 3, vendor: 1, product: 2, version: 3 },
            "test_keyboard".to_string(),
        );
        let file = InputFile::from_device_descriptor(info, &descriptor, inspector.root());

        assert!(file.config.supported_event_types.get(uapi::EV_KEY));
        assert!(!file.config.properties.get(uapi::INPUT_PROP_DIRECT));

        // When !has_keys, standard keycodes from KEY_MAP plus media keys should be set.
        assert!(file.config.supported_keys.get(uapi::KEY_A));
        assert!(file.config.supported_keys.get(uapi::KEY_Z));
        assert!(file.config.supported_keys.get(uapi::KEY_ENTER));
        assert!(file.config.supported_keys.get(uapi::KEY_POWER));
        assert!(file.config.supported_keys.get(uapi::KEY_VOLUMEUP));
        assert!(file.config.supported_keys.get(uapi::KEY_VOLUMEDOWN));
    }

    #[::fuchsia::test]
    fn test_input_file_from_keyboard_descriptor_unknown_led_does_not_set_ev_led() {
        let inspector = fuchsia_inspect::Inspector::default();
        let descriptor = fidl_fuchsia_input_report::DeviceDescriptor {
            keyboard: Some(fidl_fuchsia_input_report::KeyboardDescriptor {
                input: Some(fidl_fuchsia_input_report::KeyboardInputDescriptor {
                    keys3: Some(vec![fidl_fuchsia_input::Key::A]),
                    ..Default::default()
                }),
                output: Some(fidl_fuchsia_input_report::KeyboardOutputDescriptor {
                    leds: Some(vec![
                        fidl_fuchsia_input_report::LedType::from_primitive_allow_unknown(999),
                    ]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let info = InputDeviceInfo::new(
            uapi::input_id { bustype: 3, vendor: 1, product: 2, version: 3 },
            "test_keyboard_unknown_led".to_string(),
        );
        let file = InputFile::from_device_descriptor(info, &descriptor, inspector.root());

        assert!(file.config.supported_event_types.get(uapi::EV_KEY));
        assert!(!file.config.supported_event_types.get(uapi::EV_LED));
        assert_eq!(file.config.supported_leds, BitSet::new());
    }

    #[::fuchsia::test]
    fn test_input_file_from_mouse_descriptor() {
        let inspector = fuchsia_inspect::Inspector::default();
        let descriptor = fidl_fuchsia_input_report::DeviceDescriptor {
            mouse: Some(fidl_fuchsia_input_report::MouseDescriptor {
                input: Some(fidl_fuchsia_input_report::MouseInputDescriptor {
                    movement_x: Some(fidl_fuchsia_input_report::Axis {
                        range: fidl_fuchsia_input_report::Range { min: -127, max: 127 },
                        unit: fidl_fuchsia_input_report::Unit {
                            type_: fidl_fuchsia_input_report::UnitType::None,
                            exponent: 0,
                        },
                    }),
                    movement_y: Some(fidl_fuchsia_input_report::Axis {
                        range: fidl_fuchsia_input_report::Range { min: -127, max: 127 },
                        unit: fidl_fuchsia_input_report::Unit {
                            type_: fidl_fuchsia_input_report::UnitType::None,
                            exponent: 0,
                        },
                    }),
                    scroll_v: Some(fidl_fuchsia_input_report::Axis {
                        range: fidl_fuchsia_input_report::Range { min: -1, max: 1 },
                        unit: fidl_fuchsia_input_report::Unit {
                            type_: fidl_fuchsia_input_report::UnitType::None,
                            exponent: 0,
                        },
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let info = InputDeviceInfo::new(
            uapi::input_id { bustype: 3, vendor: 1, product: 2, version: 3 },
            "test_mouse".to_string(),
        );
        let file = InputFile::from_device_descriptor(info, &descriptor, inspector.root());

        assert!(file.config.supported_event_types.get(uapi::EV_KEY));
        assert!(file.config.supported_event_types.get(uapi::EV_REL));
        assert!(file.config.properties.get(uapi::INPUT_PROP_POINTER));

        assert!(file.config.supported_keys.get(uapi::BTN_LEFT));
        assert!(file.config.supported_keys.get(uapi::BTN_RIGHT));
        assert!(file.config.supported_motion_attributes.get(uapi::REL_X));
        assert!(file.config.supported_motion_attributes.get(uapi::REL_Y));
        assert!(file.config.supported_motion_attributes.get(uapi::REL_WHEEL));
    }

    #[::fuchsia::test]
    fn test_input_file_from_mouse_descriptor_scroll_only() {
        let inspector = fuchsia_inspect::Inspector::default();
        let descriptor = fidl_fuchsia_input_report::DeviceDescriptor {
            mouse: Some(fidl_fuchsia_input_report::MouseDescriptor {
                input: Some(fidl_fuchsia_input_report::MouseInputDescriptor {
                    scroll_v: Some(fidl_fuchsia_input_report::Axis {
                        range: fidl_fuchsia_input_report::Range { min: -1, max: 1 },
                        unit: fidl_fuchsia_input_report::Unit {
                            type_: fidl_fuchsia_input_report::UnitType::None,
                            exponent: 0,
                        },
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let info = InputDeviceInfo::new(
            uapi::input_id { bustype: 3, vendor: 1, product: 2, version: 3 },
            "test_mouse_scroll".to_string(),
        );
        let file = InputFile::from_device_descriptor(info, &descriptor, inspector.root());

        assert!(file.config.supported_event_types.get(uapi::EV_REL));
        assert!(file.config.supported_motion_attributes.get(uapi::REL_X));
        assert!(file.config.supported_motion_attributes.get(uapi::REL_Y));
        assert!(file.config.supported_motion_attributes.get(uapi::REL_WHEEL));
    }

    #[::fuchsia::test]
    fn test_input_file_from_mouse_descriptor_with_buttons() {
        let inspector = fuchsia_inspect::Inspector::default();
        let descriptor = fidl_fuchsia_input_report::DeviceDescriptor {
            mouse: Some(fidl_fuchsia_input_report::MouseDescriptor {
                input: Some(fidl_fuchsia_input_report::MouseInputDescriptor {
                    buttons: Some(vec![1, 2]),
                    movement_x: Some(fidl_fuchsia_input_report::Axis {
                        range: fidl_fuchsia_input_report::Range { min: -10, max: 10 },
                        unit: fidl_fuchsia_input_report::Unit {
                            type_: fidl_fuchsia_input_report::UnitType::None,
                            exponent: 0,
                        },
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let info = InputDeviceInfo::new(
            uapi::input_id { bustype: 3, vendor: 1, product: 2, version: 3 },
            "test_mouse_buttons".to_string(),
        );
        let file = InputFile::from_device_descriptor(info, &descriptor, inspector.root());

        assert!(file.config.supported_keys.get(uapi::BTN_LEFT));
        assert!(file.config.supported_keys.get(uapi::BTN_RIGHT));
        assert!(!file.config.supported_keys.get(uapi::BTN_MIDDLE));
    }

    #[::fuchsia::test]
    fn test_input_file_from_mouse_descriptor_fallback_rel() {
        let inspector = fuchsia_inspect::Inspector::default();
        let descriptor = fidl_fuchsia_input_report::DeviceDescriptor {
            mouse: Some(fidl_fuchsia_input_report::MouseDescriptor {
                input: Some(fidl_fuchsia_input_report::MouseInputDescriptor {
                    movement_x: None,
                    movement_y: None,
                    scroll_v: None,
                    scroll_h: None,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let info = InputDeviceInfo::new(
            uapi::input_id { bustype: 3, vendor: 1, product: 2, version: 3 },
            "test_mouse".to_string(),
        );
        let file = InputFile::from_device_descriptor(info, &descriptor, inspector.root());

        // When !has_rel on a standalone mouse, it falls back to standard relative axes.
        assert!(file.config.supported_event_types.get(uapi::EV_REL));
        assert!(file.config.supported_motion_attributes.get(uapi::REL_X));
        assert!(file.config.supported_motion_attributes.get(uapi::REL_Y));
        assert!(file.config.supported_motion_attributes.get(uapi::REL_WHEEL));
        assert!(file.config.supported_motion_attributes.get(uapi::REL_HWHEEL));
    }

    #[::fuchsia::test]
    fn test_input_file_from_mouse_descriptor_absolute_coordinates() {
        let inspector = fuchsia_inspect::Inspector::default();
        let descriptor = fidl_fuchsia_input_report::DeviceDescriptor {
            mouse: Some(fidl_fuchsia_input_report::MouseDescriptor {
                input: Some(fidl_fuchsia_input_report::MouseInputDescriptor {
                    position_x: Some(fidl_fuchsia_input_report::Axis {
                        range: fidl_fuchsia_input_report::Range { min: 0, max: 1920 },
                        unit: fidl_fuchsia_input_report::Unit {
                            type_: fidl_fuchsia_input_report::UnitType::None,
                            exponent: 0,
                        },
                    }),
                    position_y: Some(fidl_fuchsia_input_report::Axis {
                        range: fidl_fuchsia_input_report::Range { min: 0, max: 1080 },
                        unit: fidl_fuchsia_input_report::Unit {
                            type_: fidl_fuchsia_input_report::UnitType::None,
                            exponent: 0,
                        },
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let info = InputDeviceInfo::new(
            uapi::input_id { bustype: 3, vendor: 1, product: 2, version: 3 },
            "test_abs_mouse".to_string(),
        );
        let file = InputFile::from_device_descriptor(info, &descriptor, inspector.root());

        // Mice never report EV_ABS or BTN_TOUCH; even when position axes are
        // present in the descriptor, mice fall back to relative motion (EV_REL).
        assert!(!file.config.supported_event_types.get(uapi::EV_ABS));
        assert!(file.config.supported_event_types.get(uapi::EV_REL));
        assert!(!file.config.supported_position_attributes.get(uapi::ABS_X));
        assert!(!file.config.supported_position_attributes.get(uapi::ABS_Y));
        assert!(!file.config.supported_keys.get(uapi::BTN_TOUCH));
        assert!(file.config.supported_motion_attributes.get(uapi::REL_X));
        assert!(file.config.supported_motion_attributes.get(uapi::REL_Y));
    }

    #[::fuchsia::test]
    fn test_input_file_from_composite_touch_and_mouse() {
        let inspector = fuchsia_inspect::Inspector::default();
        let descriptor = fidl_fuchsia_input_report::DeviceDescriptor {
            touch: Some(fidl_fuchsia_input_report::TouchDescriptor {
                input: Some(fidl_fuchsia_input_report::TouchInputDescriptor {
                    contacts: Some(vec![fidl_fuchsia_input_report::ContactInputDescriptor {
                        position_x: Some(fidl_fuchsia_input_report::Axis {
                            range: fidl_fuchsia_input_report::Range { min: 0, max: 1080 },
                            unit: fidl_fuchsia_input_report::Unit {
                                type_: fidl_fuchsia_input_report::UnitType::None,
                                exponent: 0,
                            },
                        }),
                        position_y: Some(fidl_fuchsia_input_report::Axis {
                            range: fidl_fuchsia_input_report::Range { min: 0, max: 2400 },
                            unit: fidl_fuchsia_input_report::Unit {
                                type_: fidl_fuchsia_input_report::UnitType::None,
                                exponent: 0,
                            },
                        }),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            mouse: Some(fidl_fuchsia_input_report::MouseDescriptor {
                input: Some(fidl_fuchsia_input_report::MouseInputDescriptor {
                    position_x: Some(fidl_fuchsia_input_report::Axis {
                        range: fidl_fuchsia_input_report::Range { min: 0, max: 500 },
                        unit: fidl_fuchsia_input_report::Unit {
                            type_: fidl_fuchsia_input_report::UnitType::None,
                            exponent: 0,
                        },
                    }),
                    movement_x: Some(fidl_fuchsia_input_report::Axis {
                        range: fidl_fuchsia_input_report::Range { min: -10, max: 10 },
                        unit: fidl_fuchsia_input_report::Unit {
                            type_: fidl_fuchsia_input_report::UnitType::None,
                            exponent: 0,
                        },
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let info = InputDeviceInfo::new(
            uapi::input_id { bustype: 3, vendor: 1, product: 2, version: 3 },
            "composite".to_string(),
        );
        let file = InputFile::from_device_descriptor(info, &descriptor, inspector.root());

        // Touch sets INPUT_PROP_DIRECT; mouse must NOT overwrite or add INPUT_PROP_POINTER.
        assert!(file.config.properties.get(uapi::INPUT_PROP_DIRECT));
        assert!(!file.config.properties.get(uapi::INPUT_PROP_POINTER));

        // Touch axes (0..1080) must NOT be overwritten by mouse position_x (0..500).
        assert_eq!(file.config.x_axis_info.maximum, 1080);
        assert!(file.config.supported_event_types.get(uapi::EV_REL));
        assert!(file.config.supported_event_types.get(uapi::EV_ABS));
    }

    #[::fuchsia::test]
    fn test_input_file_from_composite_touch_keyboard_descriptor() {
        let inspector = fuchsia_inspect::Inspector::default();
        let descriptor = fidl_fuchsia_input_report::DeviceDescriptor {
            touch: Some(fidl_fuchsia_input_report::TouchDescriptor {
                input: Some(fidl_fuchsia_input_report::TouchInputDescriptor {
                    touch_type: Some(fidl_fuchsia_input_report::TouchType::Touchpad),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            keyboard: Some(fidl_fuchsia_input_report::KeyboardDescriptor {
                input: Some(fidl_fuchsia_input_report::KeyboardInputDescriptor {
                    keys3: None,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let info = InputDeviceInfo::new(
            uapi::input_id { bustype: 3, vendor: 1, product: 2, version: 3 },
            "composite_touch_keyboard".to_string(),
        );
        let file = InputFile::from_device_descriptor(info, &descriptor, inspector.root());

        // Touch keys (BTN_TOUCH, BTN_TOOL_FINGER) must not be overwritten by keyboard fallback keys.
        assert!(file.config.supported_keys.get(uapi::BTN_TOUCH));
        assert!(file.config.supported_keys.get(uapi::BTN_TOOL_FINGER));

        // Keyboard fallback keys must also be present.
        assert!(file.config.supported_keys.get(uapi::KEY_A));
        assert!(file.config.supported_keys.get(uapi::KEY_Z));
        assert!(file.config.supported_keys.get(uapi::KEY_ENTER));
    }

    #[::fuchsia::test]
    fn test_input_file_from_consumer_control_descriptor() {
        let inspector = fuchsia_inspect::Inspector::default();
        let descriptor = fidl_fuchsia_input_report::DeviceDescriptor {
            consumer_control: Some(fidl_fuchsia_input_report::ConsumerControlDescriptor {
                input: Some(fidl_fuchsia_input_report::ConsumerControlInputDescriptor {
                    buttons: Some(vec![
                        fidl_fuchsia_input_report::ConsumerControlButton::VolumeUp,
                        fidl_fuchsia_input_report::ConsumerControlButton::VolumeDown,
                        fidl_fuchsia_input_report::ConsumerControlButton::Power,
                        fidl_fuchsia_input_report::ConsumerControlButton::Pause,
                        fidl_fuchsia_input_report::ConsumerControlButton::MicMute,
                    ]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let info = InputDeviceInfo::new(
            uapi::input_id { bustype: 3, vendor: 1, product: 2, version: 3 },
            "test_buttons".to_string(),
        );
        let file = InputFile::from_device_descriptor(info, &descriptor, inspector.root());

        assert!(file.config.supported_event_types.get(uapi::EV_KEY));
        assert!(file.config.supported_keys.get(uapi::KEY_VOLUMEUP));
        assert!(file.config.supported_keys.get(uapi::KEY_VOLUMEDOWN));
        assert!(file.config.supported_keys.get(uapi::KEY_POWER));
        assert!(file.config.supported_keys.get(uapi::KEY_PAUSE));
        assert!(file.config.supported_keys.get(uapi::KEY_MICMUTE));
    }

    #[::fuchsia::test]
    fn test_input_file_from_consumer_control_descriptor_fallback() {
        let inspector = fuchsia_inspect::Inspector::default();
        let descriptor = fidl_fuchsia_input_report::DeviceDescriptor {
            consumer_control: Some(fidl_fuchsia_input_report::ConsumerControlDescriptor {
                input: Some(fidl_fuchsia_input_report::ConsumerControlInputDescriptor {
                    buttons: None,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let info = InputDeviceInfo::new(
            uapi::input_id { bustype: 3, vendor: 1, product: 2, version: 3 },
            "test_buttons_fallback".to_string(),
        );
        let file = InputFile::from_device_descriptor(info, &descriptor, inspector.root());

        assert!(file.config.supported_event_types.get(uapi::EV_KEY));
        assert!(file.config.supported_keys.get(uapi::KEY_POWER));
        assert!(file.config.supported_keys.get(uapi::KEY_VOLUMEUP));
        assert!(file.config.supported_keys.get(uapi::KEY_VOLUMEDOWN));
        assert!(!file.config.supported_keys.get(uapi::KEY_PAUSE));
    }
}
