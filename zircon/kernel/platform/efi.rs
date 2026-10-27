// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::efi_ffi::{
    cpp_efi_services_activation_reset, cpp_efi_services_activation_valid,
    cpp_efi_services_set_variable, cpp_try_activate_efi_services,
};
use core::marker::PhantomData;
use core::mem::{MaybeUninit, align_of, size_of};
use efi::{EFI_INVALID_PARAMETER, EfiGuid, EfiStatus};

/// Manages access to `efi_runtime_services` and restoration of the previous address space on drop.
///
/// `PhantomData<*mut ()>` makes this guard `!Send` and `!Sync` because `EfiServicesActivation`
/// switches the active address space on `Thread::Current` and must be dropped on the same thread.
#[repr(C, align(8))]
pub struct EfiServicesActivation(zr::OpaqueBytes<16>, PhantomData<*mut ()>);

// Mirrors the `static_assert`s in `efi_ffi.cc`.
zr::static_assert!(size_of::<EfiServicesActivation>() == 16);
zr::static_assert!(align_of::<EfiServicesActivation>() == 8);

/// Activate EFI services.
///
/// Calling this function will attempt to activate the address space containing EFI services, and
/// return a scoped [`EfiServicesActivation`] guard providing access to runtime services. When the
/// guard is dropped, the previous address space will be restored.
///
/// Returns a guard for which [`EfiServicesActivation::valid`] is `false` if no EFI services are
/// available.
///
/// WARNING: Callers accessing EFI runtime services while this guard is active must be tagged with
/// `#[sanitize(address = "off")]` (the Rust equivalent of `NO_ASAN`) to avoid crashes when running
/// under KASAN.
///
/// # Safety
///
/// - Any [`EfiServicesActivation`] guards active on the current thread must be dropped in strict
///   LIFO (stack) order without switching or destroying the thread's saved previous `VmAspace`
///   while the guard is live; otherwise, dropping an outer guard early restores the previous
///   address space while an inner guard still reports `valid() == true` and would dereference
///   unmapped 1:1 physical EFI pointers.
/// - Callers accessing EFI runtime services while the returned guard is active must disable KASAN
///   instrumentation (`#[sanitize(address = "off")]`).
#[sanitize(address = "off")]
pub unsafe fn try_activate_efi_services() -> EfiServicesActivation {
    let mut services = MaybeUninit::<EfiServicesActivation>::uninit();
    // SAFETY: `services` is valid uninitialized stack storage for `EfiServicesActivation`, and the
    // caller upholds the LIFO address-space activation and KASAN preconditions.
    unsafe {
        cpp_try_activate_efi_services(&raw mut services);
        services.assume_init()
    }
}

impl EfiServicesActivation {
    /// Returns true if there is a valid EFI services pointer.
    #[sanitize(address = "off")]
    pub fn valid(&self) -> bool {
        // SAFETY: `self` is a valid, initialized `EfiServicesActivation`.
        unsafe { cpp_efi_services_activation_valid(self) }
    }

    /// Sets the value of an EFI variable via the `SetVariable` runtime service.
    #[sanitize(address = "off")]
    pub fn set_variable(
        &self,
        var_name: &[u16],
        vendor_guid: &EfiGuid,
        attributes: u32,
        data: &[u8],
    ) -> EfiStatus {
        if !self.valid() || var_name.last() != Some(&0) {
            return EFI_INVALID_PARAMETER;
        }
        // SAFETY: `self.valid()` is true, `var_name` is NUL-terminated, and `vendor_guid` and
        // `data` are valid references for the duration of the call.
        unsafe {
            cpp_efi_services_set_variable(
                self,
                var_name.as_ptr(),
                vendor_guid,
                attributes,
                data.len(),
                data.as_ptr(),
            )
        }
    }
}

impl Drop for EfiServicesActivation {
    #[sanitize(address = "off")]
    fn drop(&mut self) {
        // SAFETY: `self` is a valid, initialized `EfiServicesActivation` on the current thread.
        unsafe { cpp_efi_services_activation_reset(self) };
    }
}
