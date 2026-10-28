// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::efi::EfiServicesActivation;
use super::efi_crashlog::EfiCrashlog;
use super::power::ZirconCrashReason;
use core::ffi::c_char;
use core::mem::MaybeUninit;
use efi::{EfiGuid, EfiStatus};
use libc::stdio::FILE;

unsafe extern "C" {
    pub(super) fn cpp_try_activate_efi_services(out: *mut MaybeUninit<EfiServicesActivation>);
    pub(super) fn cpp_efi_services_activation_valid(services: *const EfiServicesActivation)
    -> bool;
    pub(super) fn cpp_efi_services_activation_reset(services: *mut EfiServicesActivation);
    pub(super) fn cpp_efi_services_set_variable(
        services: *const EfiServicesActivation,
        var_name: *const u16,
        vendor_guid: *const EfiGuid,
        attributes: u32,
        data_size: usize,
        data: *const u8,
    ) -> EfiStatus;
}

/// FFI entry point for [`super::efi_bootbyte::efi_bootbyte_set_reason`].
#[sanitize(address = "off")]
#[unsafe(no_mangle)]
pub extern "C" fn efi_bootbyte_set_reason(reason: u64) {
    super::efi_bootbyte::efi_bootbyte_set_reason(reason);
}

/// Render target pointer and length returned across FFI for [`EfiCrashlog::get_render_target`].
#[repr(C)]
pub struct EfiCrashlogRenderTarget {
    pub data: *mut c_char,
    pub len: usize,
}

/// Returns the render target buffer and length for the global [`EfiCrashlog`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_efi_crashlog_get_render_target() -> EfiCrashlogRenderTarget {
    let target = EfiCrashlog::get().get_render_target();
    EfiCrashlogRenderTarget { data: target.as_ptr().cast::<c_char>(), len: target.len() }
}

/// Finalizes the global [`EfiCrashlog`] with `reason` and `amt` bytes written.
///
/// # Safety
///
/// Must not be called concurrently with any mutable access to the render target buffer.
#[sanitize(address = "off")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_efi_crashlog_finalize(reason: ZirconCrashReason, amt: usize) {
    // SAFETY: The caller guarantees no concurrent mutation of the render target buffer.
    unsafe { EfiCrashlog::get().finalize(reason, amt) };
}

/// Recovers the crashlog from the global [`EfiCrashlog`], writing to `tgt` if non-null.
///
/// # Safety
///
/// If `tgt` is non-null, it must point to a live (not freed), valid `FILE` for the duration of the
/// call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_efi_crashlog_recover(tgt: *const FILE) -> usize {
    // SAFETY: If `tgt` is non-null, the caller guarantees it points to a live (not freed), valid
    // `FILE` for the duration of the call.
    unsafe { EfiCrashlog::get().recover(tgt) }
}

/// Enables or disables periodic crashlog uptime updates on the global [`EfiCrashlog`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_efi_crashlog_enable_crashlog_uptime_updates(enabled: bool) {
    EfiCrashlog::get().enable_crashlog_uptime_updates(enabled);
}

/// Records the location of a previously recovered crashlog on the global [`EfiCrashlog`].
///
/// # Safety
///
/// If `len > 0`, `data` must be non-null and point to `len` live (not freed), valid, immutable
/// bytes that remain valid for `'static`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_efi_crashlog_set_last_crashlog_location(
    data: *const c_char,
    len: usize,
) {
    // SAFETY: If `len > 0`, the caller guarantees `data` is non-null and points to `len` live (not
    // freed), valid, immutable bytes with `'static` lifetime; if `len == 0`,
    // `zr::slice_from_raw_parts` returns `&[]` without dereferencing `data`.
    let slice = unsafe { zr::slice_from_raw_parts(data.cast::<u8>(), len) };
    EfiCrashlog::get().set_last_crashlog_location(slice);
}
