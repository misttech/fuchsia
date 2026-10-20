// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2014 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! arm64 per-CPU state, reached through the fixed x20 register.

use crate::kernel::types::cpu_num_t;
use arm64_mp_bindings::PERCPU_IN_RESTRICTED_MODE;
use core::arch::asm;
use core::mem::{align_of, offset_of, size_of};

/// Per cpu structure, pointed to by a fixed register while in kernel mode.
/// Aligned on the maximum architectural cache line to avoid cache
/// line sharing between cpus.
pub(super) type Arm64Percpu = arm64_mp_bindings::arm64_percpu;

zr::static_assert!(size_of::<Arm64Percpu>() == 64);
zr::static_assert!(align_of::<Arm64Percpu>() == super::MAX_CACHE_LINE);
zr::static_assert!(offset_of!(Arm64Percpu, cpu_num) == 0);
zr::static_assert!(offset_of!(Arm64Percpu, blocking_disallowed) == 4);
zr::static_assert!(offset_of!(Arm64Percpu, num_spinlocks) == 8);
zr::static_assert!(offset_of!(Arm64Percpu, microarch) == 12);
zr::static_assert!(offset_of!(Arm64Percpu, should_invalidate_bp_on_context_switch) == 16);
zr::static_assert!(offset_of!(Arm64Percpu, should_invalidate_bp_on_el0_exception) == 17);
zr::static_assert!(offset_of!(Arm64Percpu, high_level_percpu) == 24);
zr::static_assert!(
    offset_of!(Arm64Percpu, in_restricted_mode) == PERCPU_IN_RESTRICTED_MODE as usize
);

// Use the x20 register to always point at the local cpu structure for fast access.
// x20 is the first available callee-saved register that clang will allow to be marked
// as fixed (via -ffixed-x20 command line, +reserve-x20 for Rust). Since it's callee saved
// when making firmware calls to PSCI or SMCC the register will be naturally saved and restored.

/// A type that an `arm64_percpu` field has, with the load and store instruction of its width.
///
/// # Safety
///
/// `read` and `write` must each be a single load or store of `size_of::<Self>()` bytes at
/// `x20 + OFFSET`, in an `asm!` block with none of the `pure`, `nomem` or `readonly` options (see
/// below).
pub(super) unsafe trait PercpuField: Copy {
    /// Loads the field at `OFFSET` of the calling CPU's `arm64_percpu`.
    ///
    /// # Safety
    ///
    /// `OFFSET` must be the offset of a field of type `Self` in `Arm64Percpu`, and x20 must point
    /// at the calling CPU's `arm64_percpu`, as it does once `arm64_init_percpu_early()` has run on
    /// that CPU.
    unsafe fn read<const OFFSET: usize>() -> Self;

    /// Stores `value` to the field at `OFFSET` of the calling CPU's `arm64_percpu`.
    ///
    /// # Safety
    ///
    /// As for `read`. Also, `value` must be valid for the field: C++ and assembly read these fields
    /// and rely on what they hold. No Rust reference to the field may be live.
    unsafe fn write<const OFFSET: usize>(value: Self);
}

// None of the loads and stores below is `pure`, to force a read of the field to
// make sure the compiler always emits a read when asked and does not cache a copy
// between.  For the same reason, this can't be done via a copy of the percpu
// pointer, since the compiler could copy x20 into another register and access it
// after a reschedule.  None is `nomem` or `readonly` either, so the compiler treats
// each as touching memory and keeps it in order with the spinlock acquire and
// release around it, as clang does for the C++ `__asm__ volatile` accessors.

// SAFETY: Each method is one `ldr w`/`str w` at `x20 + OFFSET`, and its `asm!` options are only
// `nostack` and `preserves_flags`, so it is neither `pure`, `nomem` nor `readonly`.
unsafe impl PercpuField for u32 {
    #[inline(always)]
    unsafe fn read<const OFFSET: usize>() -> Self {
        let value: u32;
        // SAFETY: The caller guarantees that x20 points at the calling CPU's `arm64_percpu` (it is
        // reloaded on exception entry and never allocated by the compiler) and that a `u32` field
        // is at `OFFSET`.
        unsafe {
            asm!(
                "ldr {value:w}, [x20, #{offset}]",
                value = lateout(reg) value,
                offset = const OFFSET,
                options(nostack, preserves_flags),
            );
        }
        value
    }

    #[inline(always)]
    unsafe fn write<const OFFSET: usize>(value: Self) {
        // SAFETY: As in `read`, the store hits the calling CPU's `u32` field at `OFFSET`. The
        // caller guarantees that `value` is valid for that field and that no Rust reference to it
        // is live.
        unsafe {
            asm!(
                "str {value:w}, [x20, #{offset}]",
                value = in(reg) value,
                offset = const OFFSET,
                options(nostack, preserves_flags),
            );
        }
    }
}

/// Reads the field of type `T` at `OFFSET` of the calling CPU's `arm64_percpu`.
/// `read_percpu_field!` derives both from a field name.
///
/// # Safety
///
/// `OFFSET` must be the offset of the field that `_field` projects to, and x20 must point at the
/// calling CPU's `arm64_percpu` (see `PercpuField::read`).
#[inline(always)]
pub(super) unsafe fn arm64_read_percpu_field<T: PercpuField, const OFFSET: usize>(
    _field: fn(&Arm64Percpu) -> &T,
) -> T {
    const { assert!((OFFSET & (align_of::<T>() - 1)) == 0, "Bad offset alignment") };
    // SAFETY: The caller guarantees that `OFFSET` is the offset of a `T` field and that x20 points
    // at the calling CPU's `arm64_percpu`.
    unsafe { T::read::<OFFSET>() }
}

/// Writes `value` to the field of type `T` at `OFFSET` of the calling CPU's `arm64_percpu`.
/// `write_percpu_field!` derives both from a field name.
///
/// # Safety
///
/// `OFFSET` must be the offset of the field that `_field` projects to, and x20 must point at the
/// calling CPU's `arm64_percpu` (see `PercpuField::read`). `value` must be valid for that field:
/// C++ and assembly read it and rely on what it holds (C++ indexes per-CPU arrays with `cpu_num`,
/// for example). No Rust reference to the field may be live.
#[inline(always)]
pub(super) unsafe fn arm64_write_percpu_field<T: PercpuField, const OFFSET: usize>(
    _field: fn(&Arm64Percpu) -> &T,
    value: T,
) {
    const { assert!((OFFSET & (align_of::<T>() - 1)) == 0, "Bad offset alignment") };
    // SAFETY: The caller guarantees that `OFFSET` is the offset of a `T` field, that x20 points at
    // the calling CPU's `arm64_percpu`, that `value` is valid for the field and that no reference
    // to the field is live.
    unsafe { T::write::<OFFSET>(value) }
}

/// Reads `field` of the calling CPU's `arm64_percpu` with one load through x20, in the width of
/// the field's type.
macro_rules! read_percpu_field {
    ($field:ident) => {
        // SAFETY: The offset and the projection both name `$field`. x20 points at the calling CPU's
        // `arm64_percpu`, the same assumption C++'s READ_PERCPU_FIELD makes.
        unsafe {
            $crate::arch_rs::arm64::mp::arm64_read_percpu_field::<
                _,
                { core::mem::offset_of!($crate::arch_rs::arm64::mp::Arm64Percpu, $field) },
            >(|percpu| &percpu.$field)
        }
    };
}
pub(super) use read_percpu_field;

/// Writes `value` to `field` of the calling CPU's `arm64_percpu` with one store through x20, in
/// the width of the field's type.
///
/// Expands to a call of the `unsafe fn` `arm64_write_percpu_field`, whose offset and projection
/// both name `field`. The caller's `unsafe` block answers for the rest of its contract: that x20
/// points at the calling CPU's `arm64_percpu`, that `value` is valid for `field`, and that no Rust
/// reference to `field` is live. Rust code reaches `arm64_percpu` fields only through these
/// macros, which form no reference to one.
macro_rules! write_percpu_field {
    ($field:ident, $value:expr) => {
        $crate::arch_rs::arm64::mp::arm64_write_percpu_field::<
            _,
            { core::mem::offset_of!($crate::arch_rs::arm64::mp::Arm64Percpu, $field) },
        >(|percpu| &percpu.$field, $value)
    };
}
pub(super) use write_percpu_field;

/// Returns the number of the calling CPU.
#[inline(always)]
pub(super) fn arch_curr_cpu_num() -> cpu_num_t {
    read_percpu_field!(cpu_num)
}
