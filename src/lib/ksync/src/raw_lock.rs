// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::ffi::c_void;
use pin_init::PinInit;

/// Trait defining a policy for how a lock should be acquired and released. A given `RawLock` may
/// have multiple policy implementations allowing a specific instance of a lock to be acquired, at
/// different times, in different ways.
///
/// # Safety
///
/// Implementors must guarantee that:
/// 1. [`acquire`](Self::acquire) and [`reacquire`](Self::reacquire) establish synchronization
///    (shared or exclusive mutual exclusion) on `lock` until the matching
///    [`release`](Self::release) call completes.
/// 2. The split sequences ([`pre_validate`](Self::pre_validate) followed by
///    [`post_validate_acquire`](Self::post_validate_acquire), and
///    [`pre_reacquire`](Self::pre_reacquire) followed by
///    [`post_validate_reacquire`](Self::post_validate_reacquire)) establish the exact same
///    synchronization guarantees as [`acquire`](Self::acquire) and
///    [`reacquire`](Self::reacquire), respectively.
pub unsafe trait LockPolicy<L: RawLock> {
    /// Arguments passed when acquiring the lock under this policy.
    type AcquireArgs: Copy;

    /// State stored in the pinned guard across lock acquisition and release.
    type GuardState: Default;

    /// Performs any pre-validation setup (such as disabling and saving interrupts) before
    /// lockdep validation runs.
    ///
    /// # Safety
    ///
    /// Any state changes made by `pre_validate` (such as disabling interrupts) must be paired with
    /// a subsequent [`post_validate_acquire`](Self::post_validate_acquire) and
    /// [`release`](Self::release) on the same thread.
    #[inline]
    unsafe fn pre_validate(_lock: &L, _state: &mut Self::GuardState, _args: Self::AcquireArgs) {}

    /// Re-runs pre-validation on an existing `GuardState` before re-acquiring the lock (e.g. in
    /// `call_unlocked`).
    ///
    /// # Safety
    ///
    /// Same safety requirements as [`pre_validate`](Self::pre_validate).
    #[inline]
    unsafe fn pre_reacquire(_lock: &L, _state: &mut Self::GuardState) {}

    /// Acquires the raw synchronization lock.
    ///
    /// # Safety
    ///
    /// 1. `state` must reference a valid, exclusive, stack-pinned `GuardState` slot initialized
    ///    with `Self::GuardState::default()`.
    /// 2. The caller must ensure that `state` remains pinned on the stack and is not dropped or
    ///    moved until the matching [`release`](Self::release) call completes.
    unsafe fn acquire(lock: &L, state: &mut Self::GuardState, args: Self::AcquireArgs);

    /// Acquires the raw synchronization lock after [`pre_validate`](Self::pre_validate) and
    /// lockdep validation have completed.
    ///
    /// # Safety
    ///
    /// Same safety requirements as [`acquire`](Self::acquire), after
    /// [`pre_validate`](Self::pre_validate) has run on `state`.
    #[inline]
    unsafe fn post_validate_acquire(
        lock: &L,
        state: &mut Self::GuardState,
        args: Self::AcquireArgs,
    ) {
        // SAFETY: Caller upholds the `LockPolicy::acquire` contract.
        unsafe {
            Self::acquire(lock, state, args);
        }
    }

    /// Re-acquires the raw synchronization lock using existing guard state (e.g. after
    /// `call_unlocked`).
    ///
    /// # Safety
    ///
    /// Same safety requirements as [`acquire`](Self::acquire).
    unsafe fn reacquire(lock: &L, state: &mut Self::GuardState);

    /// Re-acquires the raw synchronization lock after [`pre_reacquire`](Self::pre_reacquire) and
    /// lockdep validation have completed.
    ///
    /// # Safety
    ///
    /// Same safety requirements as [`reacquire`](Self::reacquire), after
    /// [`pre_reacquire`](Self::pre_reacquire) has run on `state`.
    #[inline]
    unsafe fn post_validate_reacquire(lock: &L, state: &mut Self::GuardState) {
        // SAFETY: Caller upholds the `LockPolicy::reacquire` contract.
        unsafe {
            Self::reacquire(lock, state);
        }
    }

    /// Temporarily untracks the held lock in lockdep (used by `call_untracked`).
    ///
    /// # Safety
    ///
    /// `state` must be the active `GuardState` initialized by [`acquire`](Self::acquire) for a
    /// lock currently held by the calling thread, and must be paired with a matching
    /// [`retrack`](Self::retrack) call before the lock is released.
    #[inline]
    unsafe fn untrack(_lock: &L, _state: &mut Self::GuardState) {}

    /// Re-tracks the held lock in lockdep after [`untrack`](Self::untrack) (used by
    /// `call_untracked`).
    ///
    /// # Safety
    ///
    /// Must be called on the same `state` after a preceding [`untrack`](Self::untrack) call while
    /// the lock remains held by the calling thread.
    #[inline]
    unsafe fn retrack(_lock: &L, _state: &mut Self::GuardState) {}

    /// Releases the raw synchronization lock, restoring the state.
    ///
    /// # Safety
    ///
    /// 1. `state` must reference the exact same pinned stack slot passed to the corresponding
    ///    [`acquire`](Self::acquire) call.
    /// 2. The caller must guarantee that the current thread actually holds this lock (i.e. we are
    ///    releasing a lock we currently own).
    unsafe fn release(lock: &L, state: &mut Self::GuardState);
}

/// Marker subtrait for [`LockPolicy`] implementations that enforce exclusive mutual exclusion.
///
/// # Safety
///
/// In addition to [`LockPolicy`], implementors must guarantee exclusive mutual exclusion
/// (at most one thread may hold the lock at a time) between acquisition and
/// [`release`](LockPolicy::release).
pub unsafe trait ExclusiveLockPolicy<L: RawLock>: LockPolicy<L> {}

/// Trait defining a raw, un-instrumented synchronization lock abstraction.
///
/// Implementors of `RawLock` supply the platform-specific lock storage, in-place pinning
/// initialization logic, and raw synchronization entry points.
///
/// # Safety
///
/// Implementors must represent a physical synchronization primitive with non-zero size whose
/// [`as_mut_ptr`](Self::as_mut_ptr) uniquely identifies the underlying lock storage for the
/// lifetime of `&self`.
pub unsafe trait RawLock: Sized {
    /// Default policy that should be used when acquiring this lock.
    type DefaultPolicy: ExclusiveLockPolicy<Self>;

    /// Returns a PinInit block to initialize the raw lock in-place.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `class_id` is either null or points to a valid,
    /// static `LockClassId` that remains valid for the lifetime of the lock.
    unsafe fn init(class_id: *const c_void) -> impl PinInit<Self, core::convert::Infallible>;

    /// Convert the raw lock reference to a standard c_void pointer for FFI.
    fn as_mut_ptr(&self) -> *mut c_void;
}

/// Trait for [`RawLock`] types that can be statically initialized in a `const` context (e.g. for
/// global singleton locks).
pub trait ConstRawLock: RawLock {
    /// Constant-initialized value for the lock.
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: Self;
}
