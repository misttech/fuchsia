// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::RawLock;
use super::lockdep_tracked::LockdepTracked;
use super::raw_lock::{ConstRawLock, ExclusiveLockPolicy};
use lock_api::RawMutex as _;
use pin_init::{PinInit, pin_init_from_closure};

#[repr(transparent)]
pub struct RawMutexStorage(fuchsia_sync::RawMutex);

impl Default for RawMutexStorage {
    fn default() -> Self {
        Self::const_init(core::ptr::null())
    }
}

impl RawMutexStorage {
    #[allow(clippy::declare_interior_mutable_const)]
    pub const INIT: Self = Self::const_init(core::ptr::null());

    pub const fn const_init(_class_id: *const core::ffi::c_void) -> Self {
        Self(<fuchsia_sync::RawMutex as lock_api::RawMutex>::INIT)
    }

    #[inline]
    pub fn lock(&self) {
        self.0.lock();
    }

    /// Unlocks the raw mutex.
    ///
    /// # Safety
    ///
    /// The caller must ensure the raw lock is currently held by the calling thread.
    #[inline]
    pub unsafe fn unlock(&self) {
        // SAFETY: The raw lock is held by the current thread
        unsafe {
            self.0.unlock();
        }
    }
}

// SAFETY: `RawMutexStorage` forwards directly to `fuchsia_sync::RawMutex` which implements
// `lock_api::RawMutex`.
unsafe impl lock_api::RawMutex for RawMutexStorage {
    const INIT: Self = Self::const_init(core::ptr::null());
    type GuardMarker = <fuchsia_sync::RawMutex as lock_api::RawMutex>::GuardMarker;

    #[inline]
    fn lock(&self) {
        self.0.lock();
    }

    #[inline]
    fn try_lock(&self) -> bool {
        self.0.try_lock()
    }

    #[inline]
    unsafe fn unlock(&self) {
        // SAFETY: The raw lock is held by the current thread.
        unsafe {
            self.0.unlock();
        }
    }

    #[inline]
    fn is_locked(&self) -> bool {
        self.0.is_locked()
    }
}

pub struct RawMutexPolicy;

// SAFETY: `RawMutexStorage::lock` and `RawMutexStorage::unlock` enforce mutual exclusion on
// `RawMutexStorage`.
unsafe impl super::LockPolicy<RawMutexStorage> for RawMutexPolicy {
    type AcquireArgs = ();
    type GuardState = ();

    #[inline]
    unsafe fn acquire(lock: &RawMutexStorage, _state: &mut Self::GuardState, _args: ()) {
        lock.lock();
    }

    #[inline]
    unsafe fn reacquire(lock: &RawMutexStorage, state: &mut Self::GuardState) {
        // SAFETY: Caller upholds the `LockPolicy::reacquire` contract.
        unsafe {
            Self::acquire(lock, state, ());
        }
    }

    #[inline]
    unsafe fn release(lock: &RawMutexStorage, _state: &mut Self::GuardState) {
        // SAFETY: The raw lock is held by the current thread
        unsafe {
            lock.unlock();
        }
    }
}

// SAFETY: `RawMutexPolicy` enforces exclusive mutual exclusion on `RawMutexStorage`.
unsafe impl ExclusiveLockPolicy<RawMutexStorage> for RawMutexPolicy {}

// SAFETY: `RawMutexStorage` is a physical userspace mutex with a unique storage address.
unsafe impl RawLock for RawMutexStorage {
    type DefaultPolicy = RawMutexPolicy;

    #[inline]
    unsafe fn init(
        class_id: *const core::ffi::c_void,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        // SAFETY: Closure initializes `slot` in place before returning `Ok(())`.
        unsafe {
            pin_init_from_closure(move |slot| {
                core::ptr::write(slot, Self::const_init(class_id));
                Ok(())
            })
        }
    }

    #[inline]
    fn as_mut_ptr(&self) -> *mut core::ffi::c_void {
        self as *const Self as *mut core::ffi::c_void
    }
}

impl ConstRawLock for RawMutexStorage {
    const INIT: Self = Self::const_init(core::ptr::null());
}

/// Lockdep-tracked userspace `RawMutex`.
pub type RawMutex = LockdepTracked<RawMutexStorage>;
