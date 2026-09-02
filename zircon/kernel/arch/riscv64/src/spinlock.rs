// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! RISC-V 64 spinlock implementation.
//!
//! Simple spinning lock, using LR/SC CAS instructions.  Stores the current cpu
//! number + 1 for debugging purposes, so that 0 means unheld.

use super::arch::arch_yield;
use core::sync::atomic::{AtomicU32, Ordering};

unsafe extern "C" {
    fn cpp_arch_curr_cpu_num() -> u32;
    fn cpp_percpu_inc_num_spinlocks();
    fn cpp_percpu_dec_num_spinlocks();
}

/// Acquire a spinlock without lock trace instrumentation.
fn arch_spin_lock_non_instrumented(lock: &AtomicU32) {
    let new_val = (unsafe { cpp_arch_curr_cpu_num() }) + 1;
    loop {
        if lock.compare_exchange_weak(0, new_val, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            break;
        }
        arch_yield();
    }
    unsafe { cpp_percpu_inc_num_spinlocks() };
}

/// Try to acquire a spinlock without blocking.
///
/// Returns `true` if the lock was acquired, or `false` if it was already held.
fn arch_spin_trylock(lock: &AtomicU32) -> bool {
    let new_val = (unsafe { cpp_arch_curr_cpu_num() }) + 1;
    if lock.compare_exchange(0, new_val, Ordering::Acquire, Ordering::Relaxed).is_ok() {
        unsafe { cpp_percpu_inc_num_spinlocks() };
        true
    } else {
        false
    }
}

/// Release a previously held spinlock.
fn arch_spin_unlock(lock: &AtomicU32) {
    unsafe { cpp_percpu_dec_num_spinlocks() };
    lock.store(0, Ordering::Release);
}

// C FFI exports.
//
// These take `&AtomicU32` rather than `*mut u32`: C++ always passes the address of
// an `arch_spin_lock_t`, which is non-null and suitably aligned, and saying so in
// the signature removes both the cast and the null/alignment checks the raw
// pointer dereference generated on every lock acquisition.

#[unsafe(no_mangle)]
pub extern "C" fn rust_arch_spin_lock_non_instrumented(lock: &AtomicU32) {
    arch_spin_lock_non_instrumented(lock);
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_arch_spin_trylock(lock: &AtomicU32) -> bool {
    arch_spin_trylock(lock)
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_arch_spin_unlock(lock: &AtomicU32) {
    arch_spin_unlock(lock);
}

#[cfg(ktest)]
/// Tests for RISC-V 64 atomic spinlock acquisition and release.
#[unittest::suite(name = "riscv64_spinlock")]
mod tests {
    use super::{arch_spin_lock_non_instrumented, arch_spin_trylock, arch_spin_unlock};
    use core::sync::atomic::{AtomicU32, Ordering};
    use unittest::assert_true;

    /// Test spinlock basic locking, trylocking, and unlocking state.
    #[test]
    fn test_spinlock_basic_flow() {
        let lock = AtomicU32::new(0);
        assert_true!(lock.load(Ordering::Relaxed) == 0);
        arch_spin_lock_non_instrumented(&lock);
        assert_true!(lock.load(Ordering::Relaxed) == 1);
        arch_spin_unlock(&lock);
        assert_true!(lock.load(Ordering::Relaxed) == 0);
    }

    /// Test arch_spin_trylock success and failure on contending lock.
    #[test]
    fn test_spinlock_trylock_behavior() {
        let lock = AtomicU32::new(0);
        // First trylock should succeed.
        assert_true!(arch_spin_trylock(&lock));
        // Contending trylock while held should fail.
        assert_true!(!arch_spin_trylock(&lock));
        // Release the lock.
        arch_spin_unlock(&lock);
        assert_true!(lock.load(Ordering::Relaxed) == 0);
    }
}

const _: () = assert!(core::mem::size_of::<core::sync::atomic::AtomicU32>() == 4);
