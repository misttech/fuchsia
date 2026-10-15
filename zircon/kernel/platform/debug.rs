// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::ffi::c_char;
use zx_types::zx_status_t;

unsafe extern "C" {
    fn cpp_platform_dgetc(c: *mut c_char, wait: bool) -> i32;
    fn cpp_platform_dputs_thread(str: *const c_char, len: usize) -> zx_status_t;
    fn cpp_platform_pputc(c: c_char);
    fn cpp_platform_pgetc(c: *mut c_char) -> i32;
    fn cpp_platform_serial_suspend() -> zx_status_t;
    fn cpp_platform_serial_resume() -> zx_status_t;
}

/// Call the platform console input hook.
pub fn platform_dgetc(c: &mut c_char, wait: bool) -> i32 {
    // SAFETY: The mutable reference `c` guarantees that the pointer is valid, non-null, and properly aligned.
    unsafe { cpp_platform_dgetc(c as *mut c_char, wait) }
}

/// Call the platform console output hook.
///
/// Returns `ZX_ERR_SHOULD_WAIT` without writing anything if the debug serial port is suspended.
/// See `platform_dputs_thread` in `<platform/debug.h>`.
pub fn platform_dputs_thread(s: &[u8]) -> zx_status_t {
    // SAFETY: The slice `s` guarantees that the pointer is valid for `s.len()` bytes.
    unsafe { cpp_platform_dputs_thread(s.as_ptr() as *const c_char, s.len()) }
}

/// Write a single character to the platform's panic print port.
pub fn platform_pputc(c: u8) {
    // SAFETY: No pointer is passed, so this is inherently safe.
    unsafe { cpp_platform_pputc(c as c_char) };
}

/// Read a single character from the platform's panic read port.
pub fn platform_pgetc(c: &mut c_char) -> i32 {
    // SAFETY: The mutable reference `c` guarantees that the pointer is valid, non-null, and properly aligned.
    unsafe { cpp_platform_pgetc(c as *mut c_char) }
}

/// Suspend kernel access to the debug serial port.  See `platform_serial_suspend` in
/// `<platform.h>`.
///
/// Must be called from thread context (e.g. from a syscall), as it may block.
pub fn platform_serial_suspend() -> zx_status_t {
    // SAFETY: `cpp_platform_serial_suspend` takes no arguments and has no memory-safety
    // preconditions.  Its only requirement, that it be called from a context which may block, is
    // documented above and is not a memory-safety concern.
    unsafe { cpp_platform_serial_suspend() }
}

/// Resume kernel access to the debug serial port.  See `platform_serial_resume` in
/// `<platform.h>`.
///
/// Must be called from thread context (e.g. from a syscall), as it may block.  The caller is
/// responsible for ensuring the debug serial port hardware is powered and clocked.
pub fn platform_serial_resume() -> zx_status_t {
    // SAFETY: `cpp_platform_serial_resume` takes no arguments and has no memory-safety
    // preconditions.  Its requirements, that it be called from a context which may block and that
    // the hardware be powered, are documented above and are not memory-safety concerns.
    unsafe { cpp_platform_serial_resume() }
}
