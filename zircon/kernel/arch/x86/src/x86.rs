// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

/// # Safety
///
/// Caller must ensure that:
/// 1. The I/O port address `port` is valid and mapped on the system.
/// 2. Reading from the port does not cause side effects that violate memory safety or system stability.
pub unsafe fn inp(port: u16) -> u8 {
    let value: u8;
    // SAFETY: Input from CPU I/O port as requested by the caller.
    unsafe {
        core::arch::asm!(
            "in al, dx",
            in("dx") port,
            out("al") value,
            options(nomem, nostack, preserves_flags),
        );
    }
    value
}

/// # Safety
///
/// Caller must ensure that:
/// 1. The I/O port address `port` is valid and mapped on the system.
/// 2. Reading from the port does not cause side effects that violate memory safety or system stability.
pub unsafe fn inpw(port: u16) -> u16 {
    let value: u16;
    // SAFETY: Input from CPU I/O port as requested by the caller.
    unsafe {
        core::arch::asm!(
            "in ax, dx",
            in("dx") port,
            out("ax") value,
            options(nomem, nostack, preserves_flags),
        );
    }
    value
}

/// # Safety
///
/// Caller must ensure that:
/// 1. The I/O port address `port` is valid and mapped on the system.
/// 2. Reading from the port does not cause side effects that violate memory safety or system stability.
pub unsafe fn inpd(port: u16) -> u32 {
    let value: u32;
    // SAFETY: Input from CPU I/O port as requested by the caller.
    unsafe {
        core::arch::asm!(
            "in eax, dx",
            in("dx") port,
            out("eax") value,
            options(nomem, nostack, preserves_flags),
        );
    }
    value
}

/// # Safety
///
/// Caller must ensure that:
/// 1. The I/O port address `port` is valid and mapped on the system.
/// 2. Writing `value` to the port does not cause side effects that violate memory safety or system stability.
pub unsafe fn outp(port: u16, value: u8) {
    // SAFETY: Output to CPU I/O port as requested by the caller.
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("dx") port,
            in("al") value,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// # Safety
///
/// Caller must ensure that:
/// 1. The I/O port address `port` is valid and mapped on the system.
/// 2. Writing `value` to the port does not cause side effects that violate memory safety or system stability.
pub unsafe fn outpw(port: u16, value: u16) {
    // SAFETY: Output to CPU I/O port as requested by the caller.
    unsafe {
        core::arch::asm!(
            "out dx, ax",
            in("dx") port,
            in("ax") value,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// # Safety
///
/// Caller must ensure that:
/// 1. The I/O port address `port` is valid and mapped on the system.
/// 2. Writing `value` to the port does not cause side effects that violate memory safety or system stability.
pub unsafe fn outpd(port: u16, value: u32) {
    // SAFETY: Output to CPU I/O port as requested by the caller.
    unsafe {
        core::arch::asm!(
            "out dx, eax",
            in("dx") port,
            in("eax") value,
            options(nomem, nostack, preserves_flags),
        );
    }
}
