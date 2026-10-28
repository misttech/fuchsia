// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg(test)]

use crate::lockdep_tracked::LockdepTracked;
use crate::raw_lock::{ExclusiveLockPolicy, LockPolicy, RawLock};
use core::ffi::c_void;
use lock_api::RawRwLock as _;
use pin_init::{PinInit, pin_init_from_closure};

/// A userspace implementation of `RawBrwLockPiStorage` backed by `fuchsia_sync::RawRwLock`.
pub struct RawBrwLockPiStorage {
    inner: fuchsia_sync::RawRwLock,
}

// SAFETY: `RawBrwLockPiStorage` is safe to share across threads.
unsafe impl Sync for RawBrwLockPiStorage {}
// SAFETY: `RawBrwLockPiStorage` is safe to send across threads.
unsafe impl Send for RawBrwLockPiStorage {}

/// Policy for acquiring and releasing a `RawBrwLockPiStorage` in shared (read) mode.
pub struct RawBrwLockPiReadPolicy;

// SAFETY: `lock_shared` and `unlock_shared` enforce shared reader synchronization on
// `RawBrwLockPiStorage`.
unsafe impl LockPolicy<RawBrwLockPiStorage> for RawBrwLockPiReadPolicy {
    type AcquireArgs = ();
    type GuardState = ();

    #[inline]
    unsafe fn acquire(lock: &RawBrwLockPiStorage, _state: &mut Self::GuardState, _args: ()) {
        lock.inner.lock_shared();
    }

    #[inline]
    unsafe fn reacquire(lock: &RawBrwLockPiStorage, state: &mut Self::GuardState) {
        // SAFETY: Caller upholds the `LockPolicy::reacquire` contract.
        unsafe {
            Self::acquire(lock, state, ());
        }
    }

    #[inline]
    unsafe fn release(lock: &RawBrwLockPiStorage, _state: &mut Self::GuardState) {
        // SAFETY: The caller guarantees they hold the read lock on this instance.
        unsafe {
            lock.inner.unlock_shared();
        }
    }
}

/// Policy for acquiring and releasing a `RawBrwLockPiStorage` in exclusive (write) mode.
pub struct RawBrwLockPiWritePolicy;

// SAFETY: `lock_exclusive` and `unlock_exclusive` enforce exclusive mutual exclusion on
// `RawBrwLockPiStorage`.
unsafe impl LockPolicy<RawBrwLockPiStorage> for RawBrwLockPiWritePolicy {
    type AcquireArgs = ();
    type GuardState = ();

    #[inline]
    unsafe fn acquire(lock: &RawBrwLockPiStorage, _state: &mut Self::GuardState, _args: ()) {
        lock.inner.lock_exclusive();
    }

    #[inline]
    unsafe fn reacquire(lock: &RawBrwLockPiStorage, state: &mut Self::GuardState) {
        // SAFETY: Caller upholds the `LockPolicy::reacquire` contract.
        unsafe {
            Self::acquire(lock, state, ());
        }
    }

    #[inline]
    unsafe fn release(lock: &RawBrwLockPiStorage, _state: &mut Self::GuardState) {
        // SAFETY: The caller guarantees they hold the write lock on this instance.
        unsafe {
            lock.inner.unlock_exclusive();
        }
    }
}

// SAFETY: `RawBrwLockPiWritePolicy` enforces exclusive mutual exclusion on `RawBrwLockPiStorage`.
unsafe impl ExclusiveLockPolicy<RawBrwLockPiStorage> for RawBrwLockPiWritePolicy {}

// SAFETY: `RawBrwLockPiStorage` is a physical userspace rwlock with a unique storage address.
unsafe impl RawLock for RawBrwLockPiStorage {
    type DefaultPolicy = RawBrwLockPiWritePolicy;

    #[inline]
    unsafe fn init(_class_id: *const c_void) -> impl PinInit<Self, core::convert::Infallible> {
        // SAFETY: The closure correctly initializes the raw userspace lock state in-place
        // and satisfies all safety requirements of `pin_init_from_closure`.
        unsafe {
            pin_init_from_closure(|slot| {
                core::ptr::write(slot, Self { inner: fuchsia_sync::RawRwLock::INIT });
                Ok(())
            })
        }
    }

    #[inline]
    fn as_mut_ptr(&self) -> *mut c_void {
        self as *const Self as *mut Self as *mut c_void
    }
}

/// Lockdep-tracked userspace `RawBrwLockPi`.
pub type RawBrwLockPi = LockdepTracked<RawBrwLockPiStorage>;
