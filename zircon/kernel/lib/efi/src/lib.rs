// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#![no_std]

pub use efi_bindings::{
    EFI_VARIABLE_BOOTSERVICE_ACCESS, EFI_VARIABLE_NON_VOLATILE, EFI_VARIABLE_RUNTIME_ACCESS,
    ZIRCON_CRASHLOG_EFIATTR, kEfiZirconCrashlogEfivar as kZirconCrashlogEfivar,
    kEfiZirconVendorGuid as kZirconVendorGuid,
};

pub type EfiGuid = efi_bindings::efi_guid;
pub type EfiStatus = efi_bindings::efi_status;

pub const EFI_SUCCESS: EfiStatus = efi_bindings::kEfiSuccess;
pub const EFI_INVALID_PARAMETER: EfiStatus = efi_bindings::kEfiInvalidParameter;
pub const EFI_NOT_FOUND: EfiStatus = efi_bindings::kEfiNotFound;
