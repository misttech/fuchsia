// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::sync::atomic::AtomicU32;

use super::spinlock::{
    arch_spin_lock_non_instrumented, arch_spin_lock_trace_instrumented, arch_spin_trylock,
    arch_spin_unlock,
};
use crate::kernel::spin_tracing::EncodedLockId;

/// FFI entry point for [`arch_spin_lock_non_instrumented`].
///
/// # Safety
///
/// The caller must ensure that `lock` is a valid shared reference to an `arch_spin_lock_t::value`,
/// `%gs.base` points to the current CPU's `x86_percpu` structure, and interrupts are disabled on
/// the calling CPU.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arch_spin_lock_non_instrumented(lock: &AtomicU32) {
    // SAFETY: Caller guarantees `lock`, `%gs.base`, and disabled interrupts.
    unsafe { arch_spin_lock_non_instrumented(lock) };
}

/// FFI entry point for [`arch_spin_lock_trace_instrumented`].
///
/// # Safety
///
/// The caller must ensure that `lock` is a valid shared reference to an `arch_spin_lock_t::value`,
/// `%gs.base` points to the current CPU's `x86_percpu` structure, and interrupts are disabled on
/// the calling CPU.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arch_spin_lock_trace_instrumented(
    lock: &AtomicU32,
    encoded_lock_id: EncodedLockId,
) {
    // SAFETY: Caller guarantees `lock`, `%gs.base`, and disabled interrupts.
    unsafe { arch_spin_lock_trace_instrumented(lock, encoded_lock_id) };
}

/// FFI entry point for [`arch_spin_trylock`].
///
/// # Safety
///
/// The caller must ensure that `lock` is a valid shared reference to an `arch_spin_lock_t::value`,
/// `%gs.base` points to the current CPU's `x86_percpu` structure, and interrupts are disabled on
/// the calling CPU.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arch_spin_trylock(lock: &AtomicU32) -> bool {
    // SAFETY: Caller guarantees `lock`, `%gs.base`, and disabled interrupts.
    unsafe { arch_spin_trylock(lock) }
}

/// FFI entry point for [`arch_spin_unlock`].
///
/// # Safety
///
/// The caller must ensure that `lock` points to a live, properly aligned `arch_spin_lock_t::value`
/// currently held by the calling CPU, `%gs.base` points to the current CPU's `x86_percpu`
/// structure, and interrupts are disabled on the calling CPU.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arch_spin_unlock(lock: *const AtomicU32) {
    // SAFETY: Caller guarantees `lock` points to a live lock held by the current CPU, `%gs.base`
    // is initialized, and interrupts are disabled.
    unsafe { arch_spin_unlock(lock) };
}
