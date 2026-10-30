// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::efi::EfiServicesActivation;
use core::mem::MaybeUninit;
use efi::{EfiGuid, EfiStatus};

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
