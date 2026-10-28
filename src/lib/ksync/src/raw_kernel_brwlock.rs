// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::lockdep_tracked::LockdepTracked;
use crate::raw_lock::{ExclusiveLockPolicy, LockPolicy, RawLock};
use core::ffi::c_void;
use pin_init::{PinInit, pin_data};

unsafe extern "C" {
    fn cpp_brwlock_pi_init(lock: *mut c_void, class_id: *const c_void);
    fn cpp_brwlock_pi_destroy(lock: *mut c_void);
    fn cpp_brwlock_pi_acquire_read(lock: *mut c_void);
    fn cpp_brwlock_pi_release_read(lock: *mut c_void);
    fn cpp_brwlock_pi_acquire_write(lock: *mut c_void);
    fn cpp_brwlock_pi_release_write(lock: *mut c_void);
}

/// Opaque layout block matching the Zircon C++ `BrwLockPi` object.
#[cfg(not(target_arch = "riscv64"))]
#[pin_data(PinnedDrop)]
#[repr(C, align(16))]
pub struct RawBrwLockPiStorage {
    storage: zr::OpaqueBytes<128>,
}

/// Opaque layout block matching the Zircon C++ `BrwLockPi` object.
#[cfg(target_arch = "riscv64")]
#[pin_data(PinnedDrop)]
#[repr(C, align(8))]
pub struct RawBrwLockPiStorage {
    storage: zr::OpaqueBytes<72>,
}

// SAFETY: `RawBrwLockPiStorage` is safe to share and access across threads because it delegates
// to the thread-safe Zircon kernel `BrwLockPi` implementation.
unsafe impl Sync for RawBrwLockPiStorage {}
// SAFETY: `RawBrwLockPiStorage` is safe to send across threads because it delegates to the
// thread-safe Zircon kernel `BrwLockPi` implementation.
unsafe impl Send for RawBrwLockPiStorage {}

zr::unsafe_pinned_drop_ffi!(RawBrwLockPiStorage, cpp_brwlock_pi_destroy);

/// Policy for acquiring and releasing a `RawBrwLockPiStorage` in shared (read) mode.
pub struct RawBrwLockPiReadPolicy;

// SAFETY: `cpp_brwlock_pi_acquire_read` and `cpp_brwlock_pi_release_read` enforce shared reader
// synchronization on `RawBrwLockPiStorage`.
unsafe impl LockPolicy<RawBrwLockPiStorage> for RawBrwLockPiReadPolicy {
    type AcquireArgs = ();
    type GuardState = ();

    #[inline]
    unsafe fn acquire(lock: &RawBrwLockPiStorage, _state: &mut Self::GuardState, _args: ()) {
        // SAFETY: The FFI call is safe because the lock is initialized.
        unsafe {
            cpp_brwlock_pi_acquire_read(lock.as_mut_ptr());
        }
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
        // SAFETY: The FFI call is safe because the lock is initialized and the caller currently
        // holds the read lock.
        unsafe {
            cpp_brwlock_pi_release_read(lock.as_mut_ptr());
        }
    }
}

/// Policy for acquiring and releasing a `RawBrwLockPiStorage` in exclusive (write) mode.
pub struct RawBrwLockPiWritePolicy;

// SAFETY: `cpp_brwlock_pi_acquire_write` and `cpp_brwlock_pi_release_write` enforce exclusive
// mutual exclusion on `RawBrwLockPiStorage`.
unsafe impl LockPolicy<RawBrwLockPiStorage> for RawBrwLockPiWritePolicy {
    type AcquireArgs = ();
    type GuardState = ();

    #[inline]
    unsafe fn acquire(lock: &RawBrwLockPiStorage, _state: &mut Self::GuardState, _args: ()) {
        // SAFETY: The FFI call is safe because the lock is initialized.
        unsafe {
            cpp_brwlock_pi_acquire_write(lock.as_mut_ptr());
        }
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
        // SAFETY: The FFI call is safe because the lock is initialized and the caller currently
        // holds the write lock.
        unsafe {
            cpp_brwlock_pi_release_write(lock.as_mut_ptr());
        }
    }
}

// SAFETY: `RawBrwLockPiWritePolicy` enforces exclusive mutual exclusion on `RawBrwLockPiStorage`.
unsafe impl ExclusiveLockPolicy<RawBrwLockPiStorage> for RawBrwLockPiWritePolicy {}

// SAFETY: `RawBrwLockPiStorage` is a physical kernel rwlock with a unique storage address.
unsafe impl RawLock for RawBrwLockPiStorage {
    type DefaultPolicy = RawBrwLockPiWritePolicy;

    #[inline]
    unsafe fn init(class_id: *const c_void) -> impl PinInit<Self, core::convert::Infallible> {
        zr::pin_init_ffi!(cpp_brwlock_pi_init, class_id)
    }

    #[inline]
    fn as_mut_ptr(&self) -> *mut c_void {
        self as *const Self as *mut Self as *mut c_void
    }
}

/// Lockdep-tracked kernel `BrwLockPi` matching the Zircon C++ `Lock<BrwLockPi>` layout.
pub type RawBrwLockPi = LockdepTracked<RawBrwLockPiStorage>;

#[cfg(all(
    not(target_arch = "riscv64"),
    not(any(feature = "lock_dep", feature = "lock_metadata_only"))
))]
const _: () = {
    assert!(core::mem::size_of::<RawBrwLockPi>() == 128);
    assert!(core::mem::align_of::<RawBrwLockPi>() == 16);
};

#[cfg(all(
    not(target_arch = "riscv64"),
    any(feature = "lock_dep", feature = "lock_metadata_only")
))]
const _: () = {
    assert!(core::mem::size_of::<RawBrwLockPi>() == 144);
    assert!(core::mem::align_of::<RawBrwLockPi>() == 16);
};

#[cfg(all(
    target_arch = "riscv64",
    not(any(feature = "lock_dep", feature = "lock_metadata_only"))
))]
const _: () = {
    assert!(core::mem::size_of::<RawBrwLockPi>() == 72);
    assert!(core::mem::align_of::<RawBrwLockPi>() == 8);
};

#[cfg(all(target_arch = "riscv64", any(feature = "lock_dep", feature = "lock_metadata_only")))]
const _: () = {
    assert!(core::mem::size_of::<RawBrwLockPi>() == 80);
    assert!(core::mem::align_of::<RawBrwLockPi>() == 8);
};
