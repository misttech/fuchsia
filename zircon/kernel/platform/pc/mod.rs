// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#[path = "../../lib/acpi_lite/rust/zircon/src/mod.rs"]
pub mod acpi_lite_zircon;

pub mod acpi;
pub mod cmos_bootbyte;
pub mod hpet;
pub mod interrupt_manager;
pub mod interrupts;
pub mod keyboard;
pub mod memory;
pub mod pic;
pub mod power;
pub mod smbios;
pub mod timer;

use core::ffi::{CStr, c_char};

unsafe extern "C" {
    #[link_name = "manufacturer"]
    static mut MANUFACTURER: *const c_char;
}

/// Manufacturer information, fetched from SMBIOS during system startup.
#[inline]
pub fn manufacturer() -> &'static CStr {
    // SAFETY: `manufacturer` is a valid NUL-terminated static C string initialized to
    // `"unknown"` and updated once during early single-threaded SMBIOS init before
    // secondary CPUs are brought online.
    unsafe { CStr::from_ptr(core::ptr::addr_of!(MANUFACTURER).read()) }
}
