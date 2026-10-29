// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! x86_64 spinlock implementation.
//!
//! Stores the current CPU number + 1 so that 0 represents an unheld lock.

use core::sync::atomic::{AtomicU32, Ordering};

use super::x86::{
    PERCPU_CPU_NUM_OFFSET, PERCPU_NUM_SPINLOCKS_OFFSET, x86_add_gs_offset32, x86_read_gs_offset32,
};
use crate::kernel::spin_tracing::{EncodedLockId, FinishType, Tracer};

/// Returns the spinlock value for the calling CPU (`cpu_num + 1`).
///
/// # Safety
///
/// The caller must ensure that `%gs.base` points to the current CPU's `x86_percpu` structure and
/// interrupts are disabled on the calling CPU.
#[inline(always)]
unsafe fn current_cpu_spinlock_value() -> u32 {
    // SAFETY: Caller guarantees `%gs.base` is initialized for the current CPU, and
    // `PERCPU_CPU_NUM_OFFSET` is the valid offset of `cpu_num` in `x86_percpu`.
    unsafe { x86_read_gs_offset32::<PERCPU_CPU_NUM_OFFSET>() }.wrapping_add(1)
}

#[inline(always)]
fn arch_spin_lock_core(lock: &AtomicU32, val: u32) {
    if lock.compare_exchange_weak(0, val, Ordering::Acquire, Ordering::Relaxed).is_err() {
        arch_spin_lock_core_contended(lock, val);
    }
}

/// Contended spin loop of [`arch_spin_lock_core`], outlined so the uncontended fast path falls
/// straight through without executing `pause` (`libarch::r#yield()`) or loop branches.
#[cold]
#[inline(never)]
fn arch_spin_lock_core_contended(lock: &AtomicU32, val: u32) {
    loop {
        loop {
            libarch::r#yield();
            if lock.load(Ordering::Relaxed) == 0 {
                break;
            }
        }
        if lock.compare_exchange_weak(0, val, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            return;
        }
    }
}

/// Acquires `lock` without lock trace instrumentation.
///
/// # Safety
///
/// The caller must ensure that `%gs.base` points to the current CPU's `x86_percpu` structure and
/// interrupts are disabled on the calling CPU.
#[inline(always)]
pub(super) unsafe fn arch_spin_lock_non_instrumented(lock: &AtomicU32) {
    // SAFETY: Caller guarantees `%gs.base` is initialized and interrupts are disabled.
    let val = unsafe { current_cpu_spinlock_value() };
    arch_spin_lock_core(lock, val);
    // SAFETY: Caller guarantees `%gs.base` is initialized and interrupts are disabled.
    unsafe { x86_add_gs_offset32::<PERCPU_NUM_SPINLOCKS_OFFSET, 1>() };
}

/// Acquires `lock` with lock trace instrumentation.
///
/// # Safety
///
/// The caller must ensure that `%gs.base` points to the current CPU's `x86_percpu` structure and
/// interrupts are disabled on the calling CPU.
#[inline(always)]
pub(super) unsafe fn arch_spin_lock_trace_instrumented(
    lock: &AtomicU32,
    encoded_lock_id: EncodedLockId,
) {
    // SAFETY: Caller guarantees `%gs.base` is initialized and interrupts are disabled.
    let val = unsafe { current_cpu_spinlock_value() };

    // If this lock acquisition is trace instrumented, try to obtain the lock once
    // before we decide that we need to spin and produce spin trace events.
    if lock.compare_exchange_weak(0, val, Ordering::Acquire, Ordering::Relaxed).is_err() {
        arch_spin_lock_trace_instrumented_contended(lock, val, encoded_lock_id);
    }

    // SAFETY: Caller guarantees `%gs.base` is initialized and interrupts are disabled.
    unsafe { x86_add_gs_offset32::<PERCPU_NUM_SPINLOCKS_OFFSET, 1>() };
}

/// Contended spin and trace path of [`arch_spin_lock_trace_instrumented`], outlined so the
/// uncontended fast path needs no stack frame or callee-saved register spills for [`Tracer`].
#[cold]
#[inline(never)]
fn arch_spin_lock_trace_instrumented_contended(
    lock: &AtomicU32,
    val: u32,
    encoded_lock_id: EncodedLockId,
) {
    let spin_tracer = Tracer::<true>::new();
    arch_spin_lock_core_contended(lock, val);
    spin_tracer.finish(FinishType::LockAcquired, encoded_lock_id);
}

/// Attempts to acquire `lock` without spinning.
///
/// Returns `false` if the lock was acquired, or `true` if it was already held (matching
/// `TA_TRY_ACQ(false, lock)` in `include/arch/spinlock.h`).
///
/// # Safety
///
/// The caller must ensure that `%gs.base` points to the current CPU's `x86_percpu` structure and
/// interrupts are disabled on the calling CPU.
#[inline(always)]
pub(super) unsafe fn arch_spin_trylock(lock: &AtomicU32) -> bool {
    // SAFETY: Caller guarantees `%gs.base` is initialized and interrupts are disabled.
    let val = unsafe { current_cpu_spinlock_value() };

    let failed = lock.compare_exchange(0, val, Ordering::Acquire, Ordering::Relaxed).is_err();
    if !failed {
        // SAFETY: Caller guarantees `%gs.base` is initialized and interrupts are disabled.
        unsafe { x86_add_gs_offset32::<PERCPU_NUM_SPINLOCKS_OFFSET, 1>() };
    }
    failed
}

/// Releases a previously held `lock`.
///
/// Takes a raw pointer `*const AtomicU32` rather than `&AtomicU32` because releasing the lock
/// allows another CPU to immediately acquire it and free the containing object before this
/// function returns.
///
/// # Safety
///
/// The caller must ensure that `lock` points to a live, properly aligned `AtomicU32` currently
/// held by the calling CPU, `%gs.base` points to the current CPU's `x86_percpu` structure, and
/// interrupts are disabled on the calling CPU.
#[inline(always)]
pub(super) unsafe fn arch_spin_unlock(lock: *const AtomicU32) {
    // SAFETY: Caller guarantees `%gs.base` is initialized, interrupts are disabled, and the
    // current CPU holds the live `lock`.
    unsafe {
        x86_add_gs_offset32::<PERCPU_NUM_SPINLOCKS_OFFSET, -1>();
        (*lock).store(0, Ordering::Release);
    }
}

zr::static_assert!(core::mem::size_of::<AtomicU32>() == 4);
zr::static_assert!(core::mem::align_of::<AtomicU32>() == 4);
