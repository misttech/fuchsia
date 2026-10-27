// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::efi::try_activate_efi_services;
use super::power::ZirconCrashReason;
use core::cell::UnsafeCell;
use core::mem::size_of_val;
use core::ptr::NonNull;
use efi::{
    EFI_NOT_FOUND, EFI_SUCCESS, ZIRCON_CRASHLOG_EFIATTR, kZirconCrashlogEfivar, kZirconVendorGuid,
};
use kprint::kprintln;
use libc::stdio::FILE;

/// Something big enough for the panic log but not too enormous
/// to avoid excessive pressure on efi variable storage.
const MAX_EFI_CRASHLOG_LEN: usize = 4096;

/// EFI variable-backed platform crashlog implementation.
#[ksync::guarded]
pub struct EfiCrashlog {
    // Stashed crashlog-related values.
    #[mutex]
    last_crashlog_lock: ksync::KSpinlock,
    #[guarded_by(last_crashlog_lock)]
    last_crashlog: &'static [u8],
    render_target: UnsafeCell<[u8; MAX_EFI_CRASHLOG_LEN]>,
}

// SAFETY: `last_crashlog` is synchronized via `last_crashlog_lock`, and `render_target` is only
// mutated under the single-caller crashlog rendering protocol documented on `PlatformCrashlog`.
unsafe impl Sync for EfiCrashlog {}

static EFI_CRASHLOG: EfiCrashlog = EfiCrashlog::new();

impl EfiCrashlog {
    const fn new() -> Self {
        Self {
            last_crashlog_lock: ksync::KSpinlock::new(ksync::RawSpinlock::const_init(
                <EfiCrashlogLastCrashlogLockClass as ksync::LockClass>::ID,
            )),
            last_crashlog: ksync::KCell::new(&[]),
            render_target: UnsafeCell::new([0; MAX_EFI_CRASHLOG_LEN]),
        }
    }

    /// Returns a reference to the global `EfiCrashlog` instance.
    pub fn get() -> &'static Self {
        &EFI_CRASHLOG
    }

    /// Returns the buffer to render a crashlog payload into during a non-spontaneous crash.
    pub fn get_render_target(&self) -> NonNull<[u8]> {
        NonNull::new(self.render_target.get()).unwrap()
    }

    /// Finalizes a crashlog just before triggering a reboot.
    ///
    /// # Safety
    ///
    /// Must not be called concurrently with any mutable access to the buffer returned by
    /// [`Self::get_render_target`].
    #[sanitize(address = "off")]
    #[inline(never)]
    pub unsafe fn finalize(&self, _reason: ZirconCrashReason, amt: usize) {
        // Switch into the EFI address space.
        // SAFETY: `services` is scoped to this stack frame (strictly LIFO with respect to any
        // caller), the thread's previous `VmAspace` is not switched or destroyed before `services`
        // is dropped, and this function is tagged `#[sanitize(address = "off")]`.
        let services = unsafe { try_activate_efi_services() };
        if !services.valid() {
            return;
        }

        // Store the log.
        let amt = amt.min(size_of_val(&self.render_target));
        // SAFETY: The caller guarantees no concurrent mutation of `self.render_target`, which is
        // an initialized `[u8; MAX_EFI_CRASHLOG_LEN]`.
        let render_target = unsafe { &(&*self.render_target.get())[..amt] };
        // SAFETY: `kZirconCrashlogEfivar` and `kZirconVendorGuid` are immutable C constants.
        let (var_name, vendor_guid) = unsafe { (&kZirconCrashlogEfivar, &kZirconVendorGuid) };
        let result =
            services.set_variable(var_name, vendor_guid, ZIRCON_CRASHLOG_EFIATTR, render_target);

        // If we are writing a zero length crashlog then this has the meaning of deleting the
        // variable from the efi storage, if the crashlog already doesn't exist then attempting to
        // delete it is results in an error. From our point of view this error is spurious and so
        // we avoid printing a confusing error message.
        if result != EFI_SUCCESS && (result != EFI_NOT_FOUND || amt > 0) {
            kprintln!("EFI error while attempting to store crashlog: {:x}", result);
        }
    }

    /// Attempts to recover any crashlog from platform specific storage, writing the results to
    /// `tgt`.
    ///
    /// # Safety
    ///
    /// If `tgt` is non-null, it must point to a live (not freed), valid `FILE` for the duration of
    /// the call.
    pub unsafe fn recover(&self, tgt: *const FILE) -> usize {
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
        // SAFETY: If `tgt` is non-null, the caller guarantees it points to a live (not freed),
        // valid `FILE` for the duration of the call, and `last_crashlog_lock` has been released.
        if let Some(tgt) = unsafe { tgt.as_ref() } {
            return tgt.write(last_crashlog).max(0) as usize;
        }
        last_crashlog.len()
    }

    /// Enables or disables periodic updates of the uptime estimate in the crashlog header.
    pub fn enable_crashlog_uptime_updates(&self, _enabled: bool) {}

    /// Records the location of a previously recovered crashlog.
    pub fn set_last_crashlog_location(&self, last_crashlog: &'static [u8]) {
        ksync::lock!(let mut guard = self.lock_last_crashlog_lock());
        *guard.as_mut().last_crashlog_mut() = last_crashlog;
    }
}
