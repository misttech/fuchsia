// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::{AckBehavior, Diagnostics, WakeEvent, WakeResult, WakeVector};
use crate::user_copy::UserOutPtr;
use core::pin::Pin;
use core::ptr::NonNull;
use zx_status::Status;
use zx_types::{
    zx_instant_boot_t, zx_status_t, zx_wake_source_report_entry_t, zx_wake_source_report_header_t,
};

unsafe extern "C" {
    pub(super) fn cpp_wake_vector_get_diagnostics(
        wake_vector: &WakeVector,
        diagnostics_out: &mut Diagnostics,
    );
}

/// Returns true if there are any pending wake events in the system.
#[unsafe(no_mangle)]
pub extern "C" fn rust_wake_event_has_pending_wake_events() -> bool {
    WakeEvent::has_pending_wake_events()
}

/// Initializes a `WakeEvent` in-place at `event`.
///
/// # Safety
///
/// `wake_vector` must remain valid for all calls to `rust_wake_event_initialize`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_wake_event_new(
    event: &mut core::mem::MaybeUninit<WakeEvent>,
    wake_vector: NonNull<WakeVector>,
) {
    // SAFETY: `wake_vector` is valid for all calls to `initialize`.
    event.write(unsafe { WakeEvent::new(wake_vector) });
}

/// Drops a `WakeEvent` in-place at `event`.
///
/// # Safety
///
/// `event` must point to a valid, initialized `WakeEvent` that has already been destroyed via
/// `rust_wake_event_destroy` if it was initialized.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_wake_event_drop(event: *mut WakeEvent) {
    // SAFETY: Caller guarantees `event` is a valid initialized `WakeEvent` being destructed.
    unsafe {
        core::ptr::drop_in_place(event);
    }
}

/// Registers `event` on the global wake event list.
#[unsafe(no_mangle)]
pub extern "C" fn rust_wake_event_initialize(event: Pin<&WakeEvent>) {
    event.initialize();
}

/// Removes `event` from the pending and global wake event lists.
#[unsafe(no_mangle)]
pub extern "C" fn rust_wake_event_destroy(event: &WakeEvent) {
    event.destroy();
}

/// Triggers `event` at `trigger_time`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_wake_event_trigger(
    event: &WakeEvent,
    trigger_time: zx_instant_boot_t,
) -> WakeResult {
    event.trigger(trigger_time)
}

/// Acknowledges `event` with `ack_behavior`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_wake_event_acknowledge(event: &WakeEvent, ack_behavior: AckBehavior) {
    event.acknowledge(ack_behavior);
}

/// Atomically triggers and acknowledges `event` at `trigger_time`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_wake_event_strobe(
    event: &WakeEvent,
    trigger_time: zx_instant_boot_t,
) -> WakeResult {
    event.strobe(trigger_time)
}

/// Dumps diagnostic information for all pending or recently triggered wake events to `f`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_wake_event_dump(
    f: &mut libc::stdio::FILE,
    log_triggered_after_boot_time: zx_instant_boot_t,
) {
    WakeEvent::dump(f, log_triggered_after_boot_time);
}

/// Generates a wake event report and copies it to userspace.
#[unsafe(no_mangle)]
pub extern "C" fn rust_wake_event_generate_wake_event_report(
    suspend_start_time: zx_instant_boot_t,
    out_header: *mut zx_wake_source_report_header_t,
    out_entries: *mut zx_wake_source_report_entry_t,
    num_entries: u32,
    actual_entries: *mut u32,
) -> zx_status_t {
    Status::result_into_raw(WakeEvent::generate_wake_event_report(
        suspend_start_time,
        UserOutPtr::new(out_header),
        UserOutPtr::new(out_entries),
        num_entries,
        UserOutPtr::new(actual_entries),
    ))
}

/// Discards acknowledged wake event entries from the pending list.
#[unsafe(no_mangle)]
pub extern "C" fn rust_wake_event_discard_wake_event_report() {
    WakeEvent::discard_wake_event_report();
}
