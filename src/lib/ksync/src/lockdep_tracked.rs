// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::raw_lock::{ConstRawLock, ExclusiveLockPolicy, LockPolicy, RawLock};
use core::ffi::c_void;
use core::marker::PhantomData;
use pin_init::{PinInit, pin_data, pin_init};

#[cfg(feature = "lock_dep")]
unsafe extern "C" {
    fn cpp_lock_dep_acquire(
        lock: *mut c_void,
        class_id: *const c_void,
        entry_storage: *mut c_void,
        flags: lockdep::LockFlags,
    );
    fn cpp_lock_dep_release(entry_storage: *mut c_void, flags: lockdep::LockFlags);
    fn cpp_lock_validate_release(entry_storage: *mut c_void, flags: lockdep::LockFlags);
    fn cpp_lock_validate_acquire(entry_storage: *mut c_void, flags: lockdep::LockFlags);
}

/// Opaque stack entry storage for a lockdep `AcquiredLockEntry`.
#[cfg(feature = "lock_dep")]
#[derive(Default)]
#[repr(C, align(8))]
struct LockEntryStorage(zr::OpaqueBytes<40>);

/// Guard state for a [`LockdepTracked`] lock acquisition, combining lockdep's stack entry storage
/// with the underlying policy's `GuardState`.
#[derive(Default)]
#[repr(C)]
pub struct LockdepTrackedGuardState<S> {
    #[cfg(feature = "lock_dep")]
    entry: LockEntryStorage,
    state: S,
}

/// Wraps a [`RawLock`] and [`LockPolicy`] with optional lockdep tracking state.
#[pin_data]
#[repr(C)]
pub struct LockdepTracked<
    L: RawLock,
    P = <L as RawLock>::DefaultPolicy,
    const LOCK_FLAGS: lockdep::LockFlags = { lockdep::LOCK_FLAGS_NONE },
> {
    #[cfg(any(feature = "lock_dep", feature = "lock_metadata_only"))]
    class_id: *const c_void,
    #[pin]
    lock: L,
    _policy: PhantomData<P>,
}

impl<L: RawLock, P, const LOCK_FLAGS: lockdep::LockFlags> LockdepTracked<L, P, LOCK_FLAGS> {
    /// Flags specifying validation rules for the lock class.
    pub const LOCK_FLAGS: lockdep::LockFlags = LOCK_FLAGS;
}

impl<L: ConstRawLock, P, const LOCK_FLAGS: lockdep::LockFlags> LockdepTracked<L, P, LOCK_FLAGS> {
    #[allow(clippy::declare_interior_mutable_const)]
    pub const INIT: Self = Self::const_init(core::ptr::null());

    /// Statically initializes a `LockdepTracked` lock in constant context.
    pub const fn const_init(_class_id: *const c_void) -> Self {
        Self {
            #[cfg(any(feature = "lock_dep", feature = "lock_metadata_only"))]
            class_id: _class_id,
            lock: L::INIT,
            _policy: PhantomData,
        }
    }
}

impl<L: ConstRawLock, P, const LOCK_FLAGS: lockdep::LockFlags> Default
    for LockdepTracked<L, P, LOCK_FLAGS>
{
    fn default() -> Self {
        Self::INIT
    }
}

impl<L: RawLock, P, const LOCK_FLAGS: lockdep::LockFlags> core::ops::Deref
    for LockdepTracked<L, P, LOCK_FLAGS>
{
    type Target = L;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.lock
    }
}

// SAFETY: `LockdepTracked` is safe to share across threads when the underlying lock `L` is `Sync`.
unsafe impl<L: RawLock + Sync, P, const LOCK_FLAGS: lockdep::LockFlags> Sync
    for LockdepTracked<L, P, LOCK_FLAGS>
{
}
// SAFETY: `LockdepTracked` is safe to send across threads when the underlying lock `L` is `Send`.
unsafe impl<L: RawLock + Send, P, const LOCK_FLAGS: lockdep::LockFlags> Send
    for LockdepTracked<L, P, LOCK_FLAGS>
{
}

// SAFETY: `LockdepTracked` delegates mutual exclusion to `P` (which implements `LockPolicy<L>`)
// while recording lockdep validation state on acquire and release.
unsafe impl<
    L: RawLock,
    P: LockPolicy<L>,
    LP: ExclusiveLockPolicy<L>,
    const LOCK_FLAGS: lockdep::LockFlags,
> LockPolicy<LockdepTracked<L, LP, LOCK_FLAGS>> for LockdepTracked<L, P, LOCK_FLAGS>
{
    type AcquireArgs = P::AcquireArgs;
    type GuardState = LockdepTrackedGuardState<P::GuardState>;

    #[inline]
    unsafe fn pre_validate(
        lock: &LockdepTracked<L, LP, LOCK_FLAGS>,
        state: &mut Self::GuardState,
        args: Self::AcquireArgs,
    ) {
        // SAFETY: Caller upholds the `LockPolicy::pre_validate` contract.
        unsafe {
            P::pre_validate(&lock.lock, &mut state.state, args);
        }
    }

    #[inline]
    unsafe fn pre_reacquire(
        lock: &LockdepTracked<L, LP, LOCK_FLAGS>,
        state: &mut Self::GuardState,
    ) {
        // SAFETY: Caller upholds the `LockPolicy::pre_reacquire` contract.
        unsafe {
            P::pre_reacquire(&lock.lock, &mut state.state);
        }
    }

    #[inline]
    unsafe fn acquire(
        lock: &LockdepTracked<L, LP, LOCK_FLAGS>,
        state: &mut Self::GuardState,
        args: Self::AcquireArgs,
    ) {
        // SAFETY: Caller upholds the `LockPolicy::acquire` contract.
        unsafe {
            Self::pre_validate(lock, state, args);
            Self::post_validate_acquire(lock, state, args);
        }
    }

    #[inline]
    unsafe fn post_validate_acquire(
        lock: &LockdepTracked<L, LP, LOCK_FLAGS>,
        state: &mut Self::GuardState,
        args: Self::AcquireArgs,
    ) {
        #[cfg(feature = "lock_dep")]
        // SAFETY: `lock` is initialized, `lock.class_id` is null or a valid `LockClassId`, and the
        // caller guarantees `state` is pinned on the stack for the duration of the lock hold.
        unsafe {
            cpp_lock_dep_acquire(
                lock.lock.as_mut_ptr(),
                lock.class_id,
                state.entry.0.get() as *mut c_void,
                LOCK_FLAGS,
            );
        }
        // SAFETY: Caller upholds the `LockPolicy::post_validate_acquire` contract.
        unsafe {
            P::post_validate_acquire(&lock.lock, &mut state.state, args);
        }
    }

    #[inline]
    unsafe fn reacquire(lock: &LockdepTracked<L, LP, LOCK_FLAGS>, state: &mut Self::GuardState) {
        // SAFETY: Caller upholds the `LockPolicy::reacquire` contract.
        unsafe {
            Self::pre_reacquire(lock, state);
            Self::post_validate_reacquire(lock, state);
        }
    }

    #[inline]
    unsafe fn post_validate_reacquire(
        lock: &LockdepTracked<L, LP, LOCK_FLAGS>,
        state: &mut Self::GuardState,
    ) {
        #[cfg(feature = "lock_dep")]
        // SAFETY: `lock` is initialized, `lock.class_id` is null or a valid `LockClassId`, and the
        // caller guarantees `state` is pinned on the stack for the duration of the lock hold.
        unsafe {
            cpp_lock_dep_acquire(
                lock.lock.as_mut_ptr(),
                lock.class_id,
                state.entry.0.get() as *mut c_void,
                LOCK_FLAGS,
            );
        }
        // SAFETY: Caller upholds the `LockPolicy::post_validate_reacquire` contract.
        unsafe {
            P::post_validate_reacquire(&lock.lock, &mut state.state);
        }
    }

    #[inline]
    unsafe fn untrack(lock: &LockdepTracked<L, LP, LOCK_FLAGS>, state: &mut Self::GuardState) {
        #[cfg(feature = "lock_dep")]
        // SAFETY: Caller guarantees `state` was initialized by `acquire` and is currently held.
        unsafe {
            cpp_lock_validate_release(state.entry.0.get() as *mut c_void, LOCK_FLAGS);
        }
        // SAFETY: Caller upholds the `LockPolicy::untrack` contract.
        unsafe {
            P::untrack(&lock.lock, &mut state.state);
        }
    }

    #[inline]
    unsafe fn retrack(lock: &LockdepTracked<L, LP, LOCK_FLAGS>, state: &mut Self::GuardState) {
        // SAFETY: Caller upholds the `LockPolicy::retrack` contract.
        unsafe {
            P::retrack(&lock.lock, &mut state.state);
        }
        #[cfg(feature = "lock_dep")]
        // SAFETY: Caller guarantees `state` was previously untracked by `untrack` while held.
        unsafe {
            cpp_lock_validate_acquire(state.entry.0.get() as *mut c_void, LOCK_FLAGS);
        }
    }

    #[inline]
    unsafe fn release(lock: &LockdepTracked<L, LP, LOCK_FLAGS>, state: &mut Self::GuardState) {
        #[cfg(feature = "lock_dep")]
        // SAFETY: The caller guarantees `state` matches the slot initialized in `acquire` and
        // that the current thread holds the lock.
        unsafe {
            cpp_lock_dep_release(state.entry.0.get() as *mut c_void, LOCK_FLAGS);
        }
        // SAFETY: Caller upholds the `LockPolicy::release` contract.
        unsafe {
            P::release(&lock.lock, &mut state.state);
        }
    }
}

// SAFETY: `LockdepTracked` delegates lock acquisition and release to `P`, which guarantees
// exclusive mutual exclusion on `L`.
unsafe impl<
    L: RawLock,
    P: ExclusiveLockPolicy<L>,
    LP: ExclusiveLockPolicy<L>,
    const LOCK_FLAGS: lockdep::LockFlags,
> ExclusiveLockPolicy<LockdepTracked<L, LP, LOCK_FLAGS>> for LockdepTracked<L, P, LOCK_FLAGS>
{
}

/// Implements `LockPolicy<LockdepTracked<L, LP, LOCK_FLAGS>>` and
/// `ExclusiveLockPolicy<LockdepTracked<L, LP, LOCK_FLAGS>>` for a concrete policy type `$policy`
/// by delegating to `LockdepTracked<L, $policy, LOCK_FLAGS>`.
#[cfg(feature = "kernel")]
macro_rules! impl_lockdep_tracked_lock_policy {
    ($policy:ty) => {
        // SAFETY: Delegates to `LockdepTracked<L, $policy, LOCK_FLAGS>`, which performs lockdep
        // validation and forwards mutual exclusion to `$policy`.
        unsafe impl<
            L: RawLock,
            LP: $crate::ExclusiveLockPolicy<L>,
            const LOCK_FLAGS: lockdep::LockFlags,
        > LockPolicy<LockdepTracked<L, LP, LOCK_FLAGS>> for $policy
        where
            $policy: LockPolicy<L>,
        {
            type AcquireArgs = <$policy as LockPolicy<L>>::AcquireArgs;
            type GuardState =
                $crate::LockdepTrackedGuardState<<$policy as LockPolicy<L>>::GuardState>;

            #[inline]
            unsafe fn pre_validate(
                lock: &LockdepTracked<L, LP, LOCK_FLAGS>,
                state: &mut Self::GuardState,
                args: Self::AcquireArgs,
            ) {
                // SAFETY: Caller upholds the `LockPolicy::pre_validate` contract.
                unsafe {
                    <LockdepTracked<L, $policy, LOCK_FLAGS> as LockPolicy<
                        LockdepTracked<L, LP, LOCK_FLAGS>,
                    >>::pre_validate(lock, state, args);
                }
            }

            #[inline]
            unsafe fn pre_reacquire(
                lock: &LockdepTracked<L, LP, LOCK_FLAGS>,
                state: &mut Self::GuardState,
            ) {
                // SAFETY: Caller upholds the `LockPolicy::pre_reacquire` contract.
                unsafe {
                    <LockdepTracked<L, $policy, LOCK_FLAGS> as LockPolicy<
                        LockdepTracked<L, LP, LOCK_FLAGS>,
                    >>::pre_reacquire(lock, state);
                }
            }

            #[inline]
            unsafe fn acquire(
                lock: &LockdepTracked<L, LP, LOCK_FLAGS>,
                state: &mut Self::GuardState,
                args: Self::AcquireArgs,
            ) {
                // SAFETY: Caller upholds the `LockPolicy::acquire` contract.
                unsafe {
                    <LockdepTracked<L, $policy, LOCK_FLAGS> as LockPolicy<
                        LockdepTracked<L, LP, LOCK_FLAGS>,
                    >>::acquire(lock, state, args);
                }
            }

            #[inline]
            unsafe fn post_validate_acquire(
                lock: &LockdepTracked<L, LP, LOCK_FLAGS>,
                state: &mut Self::GuardState,
                args: Self::AcquireArgs,
            ) {
                // SAFETY: Caller upholds the `LockPolicy::post_validate_acquire` contract.
                unsafe {
                    <LockdepTracked<L, $policy, LOCK_FLAGS> as LockPolicy<
                        LockdepTracked<L, LP, LOCK_FLAGS>,
                    >>::post_validate_acquire(lock, state, args);
                }
            }

            #[inline]
            unsafe fn reacquire(
                lock: &LockdepTracked<L, LP, LOCK_FLAGS>,
                state: &mut Self::GuardState,
            ) {
                // SAFETY: Caller upholds the `LockPolicy::reacquire` contract.
                unsafe {
                    <LockdepTracked<L, $policy, LOCK_FLAGS> as LockPolicy<
                        LockdepTracked<L, LP, LOCK_FLAGS>,
                    >>::reacquire(lock, state);
                }
            }

            #[inline]
            unsafe fn post_validate_reacquire(
                lock: &LockdepTracked<L, LP, LOCK_FLAGS>,
                state: &mut Self::GuardState,
            ) {
                // SAFETY: Caller upholds the `LockPolicy::post_validate_reacquire` contract.
                unsafe {
                    <LockdepTracked<L, $policy, LOCK_FLAGS> as LockPolicy<
                        LockdepTracked<L, LP, LOCK_FLAGS>,
                    >>::post_validate_reacquire(lock, state);
                }
            }

            #[inline]
            unsafe fn untrack(
                lock: &LockdepTracked<L, LP, LOCK_FLAGS>,
                state: &mut Self::GuardState,
            ) {
                // SAFETY: Caller upholds the `LockPolicy::untrack` contract.
                unsafe {
                    <LockdepTracked<L, $policy, LOCK_FLAGS> as LockPolicy<
                        LockdepTracked<L, LP, LOCK_FLAGS>,
                    >>::untrack(lock, state);
                }
            }

            #[inline]
            unsafe fn retrack(
                lock: &LockdepTracked<L, LP, LOCK_FLAGS>,
                state: &mut Self::GuardState,
            ) {
                // SAFETY: Caller upholds the `LockPolicy::retrack` contract.
                unsafe {
                    <LockdepTracked<L, $policy, LOCK_FLAGS> as LockPolicy<
                        LockdepTracked<L, LP, LOCK_FLAGS>,
                    >>::retrack(lock, state);
                }
            }

            #[inline]
            unsafe fn release(
                lock: &LockdepTracked<L, LP, LOCK_FLAGS>,
                state: &mut Self::GuardState,
            ) {
                // SAFETY: Caller upholds the `LockPolicy::release` contract.
                unsafe {
                    <LockdepTracked<L, $policy, LOCK_FLAGS> as LockPolicy<
                        LockdepTracked<L, LP, LOCK_FLAGS>,
                    >>::release(lock, state);
                }
            }
        }

        // SAFETY: Delegates to `LockdepTracked<L, $policy, LOCK_FLAGS>`, and `$policy` guarantees
        // exclusive mutual exclusion on `L`.
        unsafe impl<
            L: RawLock,
            LP: $crate::ExclusiveLockPolicy<L>,
            const LOCK_FLAGS: lockdep::LockFlags,
        > $crate::ExclusiveLockPolicy<LockdepTracked<L, LP, LOCK_FLAGS>> for $policy
        where
            $policy: $crate::ExclusiveLockPolicy<L>,
        {
        }
    };
}

#[cfg(feature = "kernel")]
pub(crate) use impl_lockdep_tracked_lock_policy;

// SAFETY: `LockdepTracked` wraps physical lock `L` and forwards `as_mut_ptr` to uniquely identify
// it.
unsafe impl<L: RawLock, P: ExclusiveLockPolicy<L>, const LOCK_FLAGS: lockdep::LockFlags> RawLock
    for LockdepTracked<L, P, LOCK_FLAGS>
{
    type DefaultPolicy = Self;

    #[inline]
    unsafe fn init(class_id: *const c_void) -> impl PinInit<Self, core::convert::Infallible> {
        pin_init!(Self {
            // SAFETY: The caller guarantees `class_id` is null or a valid static `LockClassId`.
            lock <- unsafe { L::init(class_id) },
            #[cfg(any(feature = "lock_dep", feature = "lock_metadata_only"))]
            class_id,
            _policy: PhantomData,
        })
    }

    #[inline]
    fn as_mut_ptr(&self) -> *mut c_void {
        self.lock.as_mut_ptr()
    }
}

impl<L: ConstRawLock, P: ExclusiveLockPolicy<L>, const LOCK_FLAGS: lockdep::LockFlags> ConstRawLock
    for LockdepTracked<L, P, LOCK_FLAGS>
{
    const INIT: Self = Self::INIT;
}
