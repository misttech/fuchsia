// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::{KERNEL_ASPACE_BASE, KERNEL_ASPACE_SIZE};
use arch_arm64_vm_bindings as vm_bindings;

pub(crate) const USER_BIT_MASK: usize = 0x0080_0000_0000_0000;
zr::static_assert!(USER_BIT_MASK == vm_bindings::kUserBitMask as usize);

/// Mask of the top byte of a virtual address, which is ignored by the MMU (Top Byte Ignore).
pub(crate) const TBI_MASK: usize = 0xff00_0000_0000_0000;
zr::static_assert!(TBI_MASK == vm_bindings::kTbiMask as usize);

/// Removes the tag (top byte) from a possibly tagged pointer value.
///
/// As per RFC 143 (Userspace Top-Byte-Ignore), tagged user pointers are treated the same as their
/// untagged equivalents.
#[inline]
pub const fn detag_ptr(ptr: usize) -> usize {
    ptr & !TBI_MASK
}

/// Check if a virtual address is accessible to user space on aarch64.
///
/// [arm/v8]: D5.2.6 Virtual address splits / TTBR0_EL1 selection bit (VA[55] == 0).
#[inline]
pub const fn is_user_accessible(va: usize) -> bool {
    // This address refers to userspace if bit 55 is zero.
    (va & USER_BIT_MASK) == 0
}

/// Check that the continuous range of addresses in `[va, va+len)` are all
/// accessible to the user.
#[inline]
pub const fn is_user_accessible_range(va: usize, len: usize) -> bool {
    // Check for normal overflow which implies the range is not continuous.
    let Some(end) = va.checked_add(len) else {
        return false;
    };

    // Check that the start and end are accessible to userspace.
    if !is_user_accessible(va) || (len != 0 && !is_user_accessible(end - 1)) {
        return false;
    }

    // Cover the corner case where the start and end are accessible
    // (bit 55 == 0), but there could be a value within the range that could have
    // bit 55 == 1. In this case, the difference between start and end must be
    // at least 2^55.
    if len >= USER_BIT_MASK {
        return false;
    }

    true
}

/// Returns whether `va` is within the kernel address space.
#[inline]
pub const fn is_kernel_address(va: usize) -> bool {
    va >= KERNEL_ASPACE_BASE && va.wrapping_sub(KERNEL_ASPACE_BASE) < KERNEL_ASPACE_SIZE
}

/// Userspace threads can only set an entry point to userspace addresses, or
/// the null pointer (for testing a thread that will always fail).
#[inline]
pub const fn is_valid_user_pc(pc: usize) -> bool {
    (pc == 0) || (is_user_accessible(pc) && !is_kernel_address(pc))
}
