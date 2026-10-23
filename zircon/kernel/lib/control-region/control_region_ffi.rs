// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::ControlRegion;
use core::mem::MaybeUninit;
use pin_init::InPlaceWrite as _;

/// Initializes a `ControlRegion` in place for C++ callers.
///
/// # Safety
///
/// `region` must point to valid, aligned, uninitialized storage for a `ControlRegion`, and the
/// initialized object must remain pinned in memory until dropped by `rust_control_region_destroy`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_control_region_init(region: *mut MaybeUninit<ControlRegion>) {
    // SAFETY: Caller guarantees `region` points to valid, aligned, uninitialized storage that
    // remains pinned after initialization.
    let slot = unsafe { &mut *region };
    let _ = slot.write_pin_init(ControlRegion::init());
}

/// Destroys a `ControlRegion` in place for C++ callers.
///
/// # Safety
///
/// `region` must point to a valid, initialized `ControlRegion` that will not be used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_control_region_destroy(region: *mut ControlRegion) {
    // SAFETY: Caller guarantees `region` points to a valid, initialized `ControlRegion`.
    unsafe {
        core::ptr::drop_in_place(region);
    }
}

/// Registers a CPU for participation in `region`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_control_region_register(region: &ControlRegion) {
    region.register();
}

/// Unregisters a CPU for participation in `region`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_control_region_unregister(region: &ControlRegion) {
    region.unregister();
}

/// Attempts to enter `region`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_control_region_try_enter(region: &ControlRegion) -> bool {
    region.try_enter()
}

/// Closes the gate and enters `region`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_control_region_close_enter(region: &ControlRegion) {
    region.close_enter();
}

/// Attempts to leave `region`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_control_region_try_leave(region: &ControlRegion) -> bool {
    region.try_leave()
}

/// Opens the gate and leaves `region`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_control_region_open_leave(region: &ControlRegion) {
    region.open_leave();
}
