// Copyright 2022 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::efi::try_activate_efi_services;
use efi::{
    EFI_SUCCESS, EFI_VARIABLE_BOOTSERVICE_ACCESS, EFI_VARIABLE_NON_VOLATILE,
    EFI_VARIABLE_RUNTIME_ACCESS, kZirconVendorGuid,
};
use kprint::kprintln;

/// Default value for the EFI bootbyte.
const EFI_BOOT_NORMAL: u8 = 0x1;

/// EFI Variable for Reboot Reason.
const ZIRCON_BOOTBYTE_EFIVAR: [u16; 9] = [
    b'b' as u16,
    b'o' as u16,
    b'o' as u16,
    b't' as u16,
    b'b' as u16,
    b'y' as u16,
    b't' as u16,
    b'e' as u16,
    0,
];

const ZIRCON_BOOTBYTE_EFIATTR: u32 =
    EFI_VARIABLE_NON_VOLATILE | EFI_VARIABLE_BOOTSERVICE_ACCESS | EFI_VARIABLE_RUNTIME_ACCESS;

/// Stores the reboot `reason` in the EFI `bootbyte` variable.
#[sanitize(address = "off")]
#[inline(never)]
pub fn efi_bootbyte_set_reason(reason: u64) {
    // Switch into the EFI address space.
    // SAFETY: `services` is scoped to this stack frame (strictly LIFO with respect to any caller),
    // the thread's previous `VmAspace` is not switched or destroyed before `services` is dropped,
    // and this function is tagged `#[sanitize(address = "off")]`.
    let services = unsafe { try_activate_efi_services() };
    if !services.valid() {
        return;
    }

    // Set boot reason, clamping to be in the range of u8.
    let val: u8 = if reason <= 255 { reason as u8 } else { EFI_BOOT_NORMAL };
    // SAFETY: `kZirconVendorGuid` is an immutable C constant.
    let vendor_guid = unsafe { &kZirconVendorGuid };
    let status = services.set_variable(
        &ZIRCON_BOOTBYTE_EFIVAR,
        vendor_guid,
        ZIRCON_BOOTBYTE_EFIATTR,
        core::slice::from_ref(&val),
    );

    if status != EFI_SUCCESS {
        kprintln!("EFI error while attempting to store bootbyte: {:x}", status);
    }
}
