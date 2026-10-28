// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::crashlog::Interface;
use super::efi::try_activate_efi_services;
use super::power::ZirconCrashReason;
use core::cell::UnsafeCell;
use core::cmp::{max, min};
use core::ptr::{null, slice_from_raw_parts as ptr_slice_from_raw_parts, slice_from_raw_parts_mut};
use efi::{
    EFI_NOT_FOUND, EFI_SUCCESS, ZIRCON_CRASHLOG_EFIATTR, kZirconCrashlogEfivar, kZirconVendorGuid,
};
use kprint::kprint;
use ksync::{KMutex, RawSpinlock, guarded};
use libc::stdio::FILE;
use zr::slice_from_raw_parts;

/// EFI variable-backed platform crashlog implementation.
#[guarded]
pub struct EfiCrashlog {
    // Stashed crashlog-related values.
    #[mutex]
    last_crashlog_lock: KMutex<RawSpinlock>,
    #[guarded_by(last_crashlog_lock)]
    last_crashlog: *const [u8],
    render_target: UnsafeCell<[u8; Self::MAX_EFI_CRASHLOG_LEN]>,
}

// SAFETY: `last_crashlog` is synchronized by `last_crashlog_lock`, and `render_target` is only
// mutated during single-threaded platform halt (`get_render_target` / `finalize`).
unsafe impl Send for EfiCrashlog {}
// SAFETY: See `Send` safety comment above.
unsafe impl Sync for EfiCrashlog {}

impl EfiCrashlog {
    /// Something big enough for the panic log but not too enormous
    /// to avoid excessive pressure on efi variable storage.
    const MAX_EFI_CRASHLOG_LEN: usize = 4096;

    /// Creates a new `EfiCrashlog`.
    pub const fn new() -> Self {
        Self {
            last_crashlog_lock: KMutex::new(RawSpinlock::const_init(
                <EfiCrashlogLastCrashlogLockClass as ksync::LockClass>::ID,
            )),
            last_crashlog: ksync::KCell::new(ptr_slice_from_raw_parts(null(), 0)),
            render_target: UnsafeCell::new([0; Self::MAX_EFI_CRASHLOG_LEN]),
        }
    }

    /// Records the location of any previous EFI crashlog handed off by the bootloader.
    ///
    /// # Safety
    ///
    /// If `!last_crashlog.is_empty()`, `last_crashlog` must point to a valid, immutable byte slice
    /// that remains valid for all subsequent calls to [`Self::recover`].
    pub unsafe fn set_last_crashlog_location(&self, last_crashlog: *const [u8]) {
        ksync::lock!(let mut guard = self.lock_last_crashlog_lock());
        *guard.as_mut().last_crashlog_mut() = last_crashlog;
    }
}

impl Default for EfiCrashlog {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: `get_render_target` returns a non-null pointer to `self.render_target`
// (`MAX_EFI_CRASHLOG_LEN` bytes in `'static` storage), which is valid for writes and unaliased
// during platform halt.
unsafe impl Interface for EfiCrashlog {
    fn get_render_target(&self) -> *mut [u8] {
        slice_from_raw_parts_mut(self.render_target.get().cast::<u8>(), Self::MAX_EFI_CRASHLOG_LEN)
    }

    #[sanitize(address = "off")]
    #[inline(never)]
    unsafe fn finalize(&self, _reason: ZirconCrashReason, amt: usize) {
        // Switch into the EFI address space.
        // SAFETY: `services` is scoped to this stack frame (strictly LIFO with respect to any
        // caller), the thread's previous `VmAspace` is not switched or destroyed before `services`
        // is dropped, and this function is tagged `#[sanitize(address = "off")]`.
        let services = unsafe { try_activate_efi_services() };
        if !services.valid() {
            return;
        }

        // Store the log.
        let amt = min(amt, Self::MAX_EFI_CRASHLOG_LEN);
        // SAFETY: The caller guarantees no concurrent mutation of `self.render_target`, which is
        // an initialized `[u8; Self::MAX_EFI_CRASHLOG_LEN]`, and `amt <= Self::MAX_EFI_CRASHLOG_LEN`.
        let render_target = unsafe { &(&*self.render_target.get())[..amt] };
        // SAFETY: `kZirconCrashlogEfivar` and `kZirconVendorGuid` are immutable C constants.
        let (var_name, vendor_guid) = unsafe { (&kZirconCrashlogEfivar, &kZirconVendorGuid) };
        let result =
            services.set_variable(var_name, vendor_guid, ZIRCON_CRASHLOG_EFIATTR, render_target);

        // If we are writing a zero length crashlog then this has the meaning of deleting the
        // variable from the efi storage, if the crashlog already doesn't exist then attempting to
        // delete it results in an error. From our point of view this error is spurious and so
        // we avoid printing a confusing error message.
        if result != EFI_SUCCESS && (result != EFI_NOT_FOUND || amt > 0) {
            kprint!("EFI error while attempting to store crashlog: {:x}\n", result);
        }
    }

    unsafe fn recover(&self, tgt: Option<&FILE>) -> usize {
        let last_crashlog = {
            ksync::lock!(let guard = self.lock_last_crashlog_lock());
            *guard.last_crashlog()
        };

        if last_crashlog.is_empty() {
            return 0;
        }

        // If the user actually supplied a target, copy the crashlog into it.
        // Otherwise, just return the length which would have been needed to hold the
        // entire log.
        if let Some(tgt) = tgt {
            // SAFETY: `last_crashlog` is non-empty and was registered via
            // `set_last_crashlog_location`, whose caller guarantees it points to a valid,
            // immutable byte slice for all calls to `recover`.
            let slice =
                unsafe { slice_from_raw_parts(last_crashlog.cast::<u8>(), last_crashlog.len()) };
            return max(tgt.write(slice), 0) as usize;
        }
        last_crashlog.len()
    }

    fn enable_crashlog_uptime_updates(&self, _enabled: bool) {}
}
