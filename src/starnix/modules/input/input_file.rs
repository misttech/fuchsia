// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crossbeam::queue::SegQueue;
use fuchsia_inspect::Inspector;
use futures::FutureExt;
use starnix_core::fileops_impl_nonseekable;
use starnix_core::mm::{MemoryAccessor, MemoryAccessorExt};
use starnix_core::task::{CurrentTask, EventHandler, WaitCanceler, WaitQueue, Waiter};
use starnix_core::vfs::buffers::{InputBuffer, OutputBuffer};
use starnix_core::vfs::{FileObject, FileOps, fileops_impl_noop_sync};
use starnix_logging::{log_info, track_stub};
use starnix_sync::{FileOpsCore, Locked, Mutex, Unlocked};
use starnix_syscalls::{SUCCESS, SyscallArg, SyscallResult};
use starnix_types::time::duration_from_timeval;
use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::user_address::{ArchSpecific, MultiArchUserRef, UserAddress, UserRef};
use starnix_uapi::vfs::FdEvents;
use starnix_uapi::{
    ABS_CNT, ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID, BTN_MISC,
    BTN_TOUCH, EV_CNT, FF_CNT, INPUT_PROP_CNT, INPUT_PROP_DIRECT, KEY_CNT, KEY_DOWN, KEY_LEFT,
    KEY_POWER, KEY_RIGHT, KEY_SLEEP, KEY_UP, KEY_VOLUMEDOWN, LED_CNT, MSC_CNT, REL_CNT, REL_WHEEL,
    SW_CNT, errno, error, uapi,
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
    pub input_file: Mutex<Weak<InputFile>>,
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
            input_file: Mutex::new(Weak::new()),
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
    input_id: uapi::input_id,
    supported_event_types: BitSet<{ min_bytes(EV_CNT) }>,
    supported_keys: BitSet<{ min_bytes(KEY_CNT) }>,
    supported_position_attributes: BitSet<{ min_bytes(ABS_CNT) }>, // ABSolute position
    supported_motion_attributes: BitSet<{ min_bytes(REL_CNT) }>,   // RELative motion
    supported_switches: BitSet<{ min_bytes(SW_CNT) }>,
    supported_leds: BitSet<{ min_bytes(LED_CNT) }>,
    supported_haptics: BitSet<{ min_bytes(FF_CNT) }>, // 'F'orce 'F'eedback
    supported_misc_features: BitSet<{ min_bytes(MSC_CNT) }>,
    properties: BitSet<{ min_bytes(INPUT_PROP_CNT) }>,
    mt_slot_axis_info: uapi::input_absinfo,
    mt_tracking_id_axis_info: uapi::input_absinfo,
    x_axis_info: uapi::input_absinfo,
    y_axis_info: uapi::input_absinfo,
    events: SegQueue<LinuxEventWithTraceId>,
    waiters: WaitQueue,
    // InputFile will be initialized with an InputFileStatus that holds Inspect data
    // `None` for Uinput InputFiles
    pub inspect_status: Option<Arc<InputFileStatus>>,

    // A descriptive device name. Should contain only alphanumerics and `_`.
    device_name: String,
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
const fn min_bytes(n_bits: u32) -> usize {
    ((n_bits as usize) + 7) / 8
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
    attrs.set(BTN_MISC);
    attrs.set(KEY_POWER);
    attrs.set(KEY_VOLUMEDOWN);
    attrs
}

/// Returns appropriate `ABS`-olute position related flags for a keyboard device.
fn keyboard_position_attributes() -> BitSet<{ min_bytes(ABS_CNT) }> {
    BitSet::new()
}

fn mouse_wheel_attributes() -> BitSet<{ min_bytes(REL_CNT) }> {
    let mut attrs = BitSet::new();
    attrs.set(REL_WHEEL);
    attrs
}

/// Makes a device name string from a name and device ID details.
///
/// For practical reasons the device name should contain alphanumerics and `_`.
fn get_device_name(name: &str, input_id: &uapi::input_id) -> String {
    format!("{}_{:04x}_{:04x}_v{}", name, input_id.vendor, input_id.product, input_id.version)
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
    /// - `input_id`: device's bustype, vendor id, product id, and version.
    /// - `width`: width of screen.
    /// - `height`: height of screen.
    /// - `inspect_status`: The inspect status for the parent device of "touch_input_file".
    pub fn new_touch(
        input_id: uapi::input_id,
        width: i32,
        height: i32,
        node: &fuchsia_inspect::Node,
    ) -> Self {
        let device_name = get_device_name("starnix_touch", &input_id);
        // Fuchsia scales the position reported by the touch sensor to fit view coordinates.
        // Hence, the range of touch positions is exactly the same as the range of view
        // coordinates.
        Self {
            driver_version: Self::DRIVER_VERSION,
            input_id,
            supported_event_types: BitSet::list([uapi::EV_ABS]),
            supported_keys: touch_key_attributes(),
            supported_position_attributes: touch_position_attributes(),
            supported_motion_attributes: BitSet::new(), // None supported, not a mouse.
            supported_switches: BitSet::new(),          // None supported
            supported_leds: BitSet::new(),              // None supported
            supported_haptics: BitSet::new(),           // None supported
            supported_misc_features: BitSet::new(),     // None supported
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
            events: SegQueue::new(),
            waiters: WaitQueue::default(),
            inspect_status: Some(InputFileStatus::new(node)),
            device_name,
        }
    }

    /// Creates an `InputFile` instance suitable for emulating a keyboard.
    ///
    /// # Parameters
    /// - `input_id`: device's bustype, vendor id, product id, and version.
    /// - `inspect_status`: The inspect status for the parent device of "keyboard_input_file".
    pub fn new_keyboard(input_id: uapi::input_id, node: &fuchsia_inspect::Node) -> Self {
        let device_name = get_device_name("starnix_buttons", &input_id);
        Self {
            driver_version: Self::DRIVER_VERSION,
            input_id,
            supported_event_types: BitSet::list([uapi::EV_KEY]),
            supported_keys: keyboard_key_attributes(),
            supported_position_attributes: keyboard_position_attributes(),
            supported_motion_attributes: BitSet::new(), // None supported, not a mouse.
            supported_switches: BitSet::new(),          // None supported
            supported_leds: BitSet::new(),              // None supported
            supported_haptics: BitSet::new(),           // None supported
            supported_misc_features: BitSet::new(),     // None supported
            properties: keyboard_properties(),
            mt_slot_axis_info: uapi::input_absinfo::default(),
            mt_tracking_id_axis_info: uapi::input_absinfo::default(),
            x_axis_info: uapi::input_absinfo::default(),
            y_axis_info: uapi::input_absinfo::default(),
            events: SegQueue::new(),
            waiters: WaitQueue::default(),
            inspect_status: Some(InputFileStatus::new(node)),
            device_name,
        }
    }

    /// Creates an `InputFile` instance suitable for emulating a mouse wheel.
    ///
    /// # Parameters
    /// - `input_id`: device's bustype, vendor id, product id, and version.
    /// - `inspect_status`: The inspect status for the parent device of "mouse_input_file".
    pub fn new_mouse(input_id: uapi::input_id, node: &fuchsia_inspect::Node) -> Self {
        let device_name = get_device_name("starnix_mouse", &input_id);
        Self {
            driver_version: Self::DRIVER_VERSION,
            input_id,
            supported_event_types: BitSet::list([uapi::EV_REL]),
            supported_keys: BitSet::new(), // None supported, scroll only
            supported_position_attributes: BitSet::new(), // None supported, scroll only
            supported_motion_attributes: mouse_wheel_attributes(),
            supported_switches: BitSet::new(), // None supported
            supported_leds: BitSet::new(),     // None supported
            supported_haptics: BitSet::new(),  // None supported
            supported_misc_features: BitSet::new(), // None supported
            properties: BitSet::new(),         // None supported, scroll only
            mt_slot_axis_info: uapi::input_absinfo::default(),
            mt_tracking_id_axis_info: uapi::input_absinfo::default(),
            x_axis_info: uapi::input_absinfo::default(),
            y_axis_info: uapi::input_absinfo::default(),
            events: SegQueue::new(),
            waiters: WaitQueue::default(),
            inspect_status: Some(InputFileStatus::new(node)),
            device_name,
        }
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

// The bit-mask that removes the variable parts of the EVIOCGNAME ioctl
// request.
const EVIOCGNAME_MASK: u32 = 0b11_00_0000_0000_0000_1111_1111_1111_1111;

impl FileOps for InputFile {
    fileops_impl_nonseekable!();
    fileops_impl_noop_sync!();

    fn open(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        file: &FileObject,
        _current_task: &CurrentTask,
    ) -> Result<(), Errno> {
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
        _locked: &mut Locked<FileOpsCore>,
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
        _locked: &mut Locked<Unlocked>,
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
                current_task.write_object(UserRef::new(user_addr), &self.input_id)?;
                Ok(SUCCESS)
            }
            uapi::EVIOCGBIT_0 => {
                current_task
                    .write_object(UserRef::new(user_addr), &self.supported_event_types.bytes)?;
                Ok(SUCCESS)
            }
            uapi::EVIOCGBIT_EV_KEY => {
                current_task.write_object(UserRef::new(user_addr), &self.supported_keys.bytes)?;
                Ok(SUCCESS)
            }
            uapi::EVIOCGBIT_EV_ABS => {
                current_task.write_object(
                    UserRef::new(user_addr),
                    &self.supported_position_attributes.bytes,
                )?;
                Ok(SUCCESS)
            }
            uapi::EVIOCGBIT_EV_REL => {
                current_task.write_object(
                    UserRef::new(user_addr),
                    &self.supported_motion_attributes.bytes,
                )?;
                Ok(SUCCESS)
            }
            uapi::EVIOCGBIT_EV_SW => {
                current_task
                    .write_object(UserRef::new(user_addr), &self.supported_switches.bytes)?;
                Ok(SUCCESS)
            }
            uapi::EVIOCGBIT_EV_LED => {
                current_task.write_object(UserRef::new(user_addr), &self.supported_leds.bytes)?;
                Ok(SUCCESS)
            }
            uapi::EVIOCGBIT_EV_FF => {
                current_task
                    .write_object(UserRef::new(user_addr), &self.supported_haptics.bytes)?;
                Ok(SUCCESS)
            }
            uapi::EVIOCGBIT_EV_MSC => {
                current_task
                    .write_object(UserRef::new(user_addr), &self.supported_misc_features.bytes)?;
                Ok(SUCCESS)
            }
            uapi::EVIOCGPROP => {
                current_task.write_object(UserRef::new(user_addr), &self.properties.bytes)?;
                Ok(SUCCESS)
            }
            uapi::EVIOCGABS_MT_SLOT => {
                current_task.write_object(UserRef::new(user_addr), &self.mt_slot_axis_info)?;
                Ok(SUCCESS)
            }
            uapi::EVIOCGABS_MT_TRACKING_ID => {
                current_task
                    .write_object(UserRef::new(user_addr), &self.mt_tracking_id_axis_info)?;
                Ok(SUCCESS)
            }
            uapi::EVIOCGABS_MT_POSITION_X => {
                current_task.write_object(UserRef::new(user_addr), &self.x_axis_info)?;
                Ok(SUCCESS)
            }
            uapi::EVIOCGABS_MT_POSITION_Y => {
                current_task.write_object(UserRef::new(user_addr), &self.y_axis_info)?;
                Ok(SUCCESS)
            }

            request_with_params => {
                // Remove the variable part of the request with params, so
                // we can identify it.
                match request_with_params & EVIOCGNAME_MASK {
                    uapi::EVIOCGNAME_0 => {
                        // Request to report the device name.
                        //
                        // An EVIOCGNAME request comes with the response buffer size encoded in
                        // bits 29..16 of the request's `u32` code.  This is in contrast to
                        // most other ioctl request codes in this file, which are fully known
                        // at compile time, so we need to decode it a bit differently from
                        // other ioctl codes.
                        //
                        // See [here][hh] the macros that do this.
                        //
                        // [hh]: https://cs.opensource.google/fuchsia/fuchsia/+/main:third_party/android/platform/bionic/libc/kernel/uapi/linux/input.h;l=82;drc=0f0c18f695543b15b852f68f297744d03d642a26
                        let device_name = &self.device_name;

                        // The lowest 14 bits of the top 16 bits are the unsigned buffer
                        // length in bytes.  While we don't use multibyte characters,
                        // make sure that all sizes below are expressed in terms of
                        // bytes, not characters.
                        let buffer_bytes_count =
                            ((request_with_params >> 16) & ((1 << 14) - 1)) as usize;

                        // Zero out the entire user buffer in case the user reads too much.
                        // Probably not needed, but I don't think it hurts.
                        current_task.zero(user_addr, buffer_bytes_count)?;
                        let device_name_as_bytes = device_name.as_bytes();

                        // Copy all bytes from device name if the buffer is large enough.
                        // If not, copy one less than the buffer size, to leave space
                        // for the final NUL.
                        let to_copy_bytes_count =
                            std::cmp::min(device_name_as_bytes.len(), buffer_bytes_count - 1);
                        current_task.write_memory(
                            user_addr,
                            &device_name_as_bytes[..to_copy_bytes_count],
                        )?;
                        // EVIOCGNAME ioctl returns the number of bytes written.
                        // Do not forget the trailing NUL.
                        Ok((to_copy_bytes_count + 1).into())
                    }
                    _ => {
                        track_stub!(
                            TODO("https://fxbug.dev/322873200"),
                            "input ioctl",
                            request_with_params
                        );
                        error!(EOPNOTSUPP)
                    }
                }
            }
        }
    }

    fn read(
        &self,
        _locked: &mut Locked<FileOpsCore>,
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
        _locked: &mut Locked<FileOpsCore>,
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
        _locked: &mut Locked<FileOpsCore>,
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
        _locked: &mut Locked<FileOpsCore>,
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

    fn open(
        &self,
        locked: &mut Locked<FileOpsCore>,
        file: &FileObject,
        current_task: &CurrentTask,
    ) -> Result<(), Errno> {
        self.0.as_ref().open(locked, file, current_task)
    }

    fn close(
        self: Box<Self>,
        _locked: &mut Locked<FileOpsCore>,
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
        locked: &mut Locked<Unlocked>,
        file: &FileObject,
        current_task: &CurrentTask,
        request: u32,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        self.0.as_ref().ioctl(locked, file, current_task, request, arg)
    }

    fn read(
        &self,
        locked: &mut Locked<FileOpsCore>,
        file: &FileObject,
        current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        self.0.as_ref().read(locked, file, current_task, offset, data)
    }

    fn write(
        &self,
        locked: &mut Locked<FileOpsCore>,
        file: &FileObject,
        current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        self.0.as_ref().write(locked, file, current_task, offset, data)
    }

    fn wait_async(
        &self,
        locked: &mut Locked<FileOpsCore>,
        file: &FileObject,
        current_task: &CurrentTask,
        waiter: &Waiter,
        events: FdEvents,
        handler: EventHandler,
    ) -> Option<WaitCanceler> {
        self.0.as_ref().wait_async(locked, file, current_task, waiter, events, handler)
    }

    fn query_events(
        &self,
        locked: &mut Locked<FileOpsCore>,
        file: &FileObject,
        current_task: &CurrentTask,
    ) -> Result<FdEvents, Errno> {
        self.0.as_ref().query_events(locked, file, current_task)
    }
}

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn test_read_events_no_notify_when_buffer_full() {
        let inspector = fuchsia_inspect::Inspector::default();
        let node = inspector.root();
        let input_file = InputFile::new_touch(
            uapi::input_id { bustype: 0, vendor: 0, product: 0, version: 0 },
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
}
