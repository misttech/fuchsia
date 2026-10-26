// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod debug;
pub mod halt_helper;
pub mod halt_token;
pub mod power;
pub mod timer;

pub use halt_token::HaltToken;

unsafe extern "C" {
    #[link_name = "efi_bootbyte_set_reason"]
    fn c_efi_bootbyte_set_reason(reason: u64);
}

/// Sets the reboot reason in the EFI bootbyte variable, if EFI services are available.
#[inline]
pub fn efi_bootbyte_set_reason(reason: u64) {
    // SAFETY: `efi_bootbyte_set_reason` records the reboot reason in EFI variables if
    // available and has no caller preconditions.
    unsafe { c_efi_bootbyte_set_reason(reason) };
}
