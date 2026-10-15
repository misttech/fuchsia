// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{LockPolicy, LockToken, RawLock, RawMutex};
use core::marker::PhantomData;
use core::pin::Pin;
use core::ptr::addr_of_mut;
use lockdep::LockClass;
use pin_init::{PinInit, pin_data, pin_init, pin_init_from_closure, pinned_drop};

#[cfg(feature = "kernel")]
unsafe extern "C" {
    fn cpp_lock_validate_release(entry_storage: *mut core::ffi::c_void);
    fn cpp_lock_validate_acquire(entry_storage: *mut core::ffi::c_void);
}

/// A safe, Zircon-compatible mutual exclusion lock supporting compile-time order validation.
///
/// `KMutex` wraps a platform-specific `RawLock` abstraction. It is pinned in memory to support FFI
/// loop-detector active list registrations safely under the lock class `Class`.
#[repr(transparent)] // Ensure KMutex has the same layout as the underlying RawLock M.
#[pin_data]
pub struct KMutex<Class: LockClass, M = RawMutex> {
    #[pin]
    mutex: M,
    _marker: PhantomData<Class>,
}

impl<Class: LockClass, M> KMutex<Class, M> {
    /// Create a new KMutex with a pre-initialized raw lock.
    pub const fn new(mutex: M) -> Self {
        Self { mutex, _marker: PhantomData }
    }
}

impl<Class: LockClass, M: RawLock> KMutex<Class, M> {
    /// Safe dynamic initialization of the validation lock inside pin context.
    pub fn init() -> impl PinInit<Self, core::convert::Infallible> {
        pin_init!(Self {
            // SAFETY: `Self::class_id()` is either null or a valid static `LockClassId`.
            mutex <- unsafe { M::init(Self::class_id()) },
            _marker: PhantomData,
        })
    }

    /// Acquires the lock, using the default policy, and registers the active loop node.
    #[inline]
    pub fn lock(&self) -> impl PinInit<KMutexGuard<'_, Class, M>, core::convert::Infallible>
    where
        <M as RawLock>::DefaultPolicy: LockPolicy<M, AcquireArgs = ()>,
    {
        KMutexGuard::new(self)
    }

    /// Acquires the lock with arguments, using the default policy, and registers the active loop
    /// node.
    #[inline]
    pub fn lock_with(
        &self,
        args: <<M as RawLock>::DefaultPolicy as LockPolicy<M>>::AcquireArgs,
    ) -> impl PinInit<KMutexGuard<'_, Class, M>, core::convert::Infallible> {
        KMutexGuard::new_with_args(self, args)
    }

    /// Acquires the lock, using the specified policy, and registers the active loop node.
    #[inline]
    pub fn lock_policy<P: LockPolicy<M, AcquireArgs = ()>>(
        &self,
    ) -> impl PinInit<KMutexGuard<'_, Class, M, P>, core::convert::Infallible> {
        KMutexGuard::new(self)
    }

    /// Acquires the lock with arguments, using the specified policy, and registers the active loop
    /// node.
    #[inline]
    pub fn lock_policy_with<P: LockPolicy<M>>(
        &self,
        args: P::AcquireArgs,
    ) -> impl PinInit<KMutexGuard<'_, Class, M, P>, core::convert::Infallible> {
        KMutexGuard::new_with_args(self, args)
    }

    /// Acquires this lock and aliases it with `alias`, returning a guard that proves ownership
    /// of both `Class` and `AliasClass`.
    #[inline]
    pub fn aliased_lock<'a, AliasClass: LockClass, M2: RawLock>(
        &'a self,
        alias: &'a KMutex<AliasClass, M2>,
    ) -> impl PinInit<KMutexAliasedGuard<'a, Class, AliasClass, M, M2>, core::convert::Infallible>
    where
        <M as RawLock>::DefaultPolicy: LockPolicy<M, AcquireArgs = ()>,
    {
        KMutexAliasedGuard::new(self, alias)
    }

    /// Acquires this lock with arguments and aliases it with `alias`, returning a guard that proves
    /// ownership of both `Class` and `AliasClass`.
    #[inline]
    pub fn aliased_lock_with<'a, AliasClass: LockClass, M2: RawLock>(
        &'a self,
        alias: &'a KMutex<AliasClass, M2>,
        args: <<M as RawLock>::DefaultPolicy as LockPolicy<M>>::AcquireArgs,
    ) -> impl PinInit<KMutexAliasedGuard<'a, Class, AliasClass, M, M2>, core::convert::Infallible>
    {
        KMutexAliasedGuard::new_with_args(self, alias, args)
    }

    /// Acquires this lock with policy `P` and aliases it with `alias`.
    #[inline]
    pub fn aliased_lock_policy<
        'a,
        AliasClass: LockClass,
        M2: RawLock,
        P: LockPolicy<M, AcquireArgs = ()>,
    >(
        &'a self,
        alias: &'a KMutex<AliasClass, M2>,
    ) -> impl PinInit<KMutexAliasedGuard<'a, Class, AliasClass, M, M2, P>, core::convert::Infallible>
    {
        KMutexAliasedGuard::new(self, alias)
    }

    /// Acquires this lock with policy `P` and arguments and aliases it with `alias`.
    #[inline]
    pub fn aliased_lock_policy_with<'a, AliasClass: LockClass, M2: RawLock, P: LockPolicy<M>>(
        &'a self,
        alias: &'a KMutex<AliasClass, M2>,
        args: P::AcquireArgs,
    ) -> impl PinInit<KMutexAliasedGuard<'a, Class, AliasClass, M, M2, P>, core::convert::Infallible>
    {
        KMutexAliasedGuard::new_with_args(self, alias, args)
    }

    /// Acquires multiple locks of the same class and type in ascending memory address order,
    /// using the default policy.
    #[inline]
    pub fn lock_multiple<'a, const N: usize>(
        locks: [&'a Self; N],
    ) -> impl PinInit<KMutexGuardMultiple<'a, N, Class, M>, core::convert::Infallible>
    where
        <M as RawLock>::DefaultPolicy: LockPolicy<M, AcquireArgs = ()>,
    {
        KMutexGuardMultiple::new(locks)
    }

    /// Acquires multiple locks of the same class and type in ascending memory address order with
    /// arguments, using the default policy.
    #[inline]
    pub fn lock_multiple_with<'a, const N: usize>(
        locks: [&'a Self; N],
        args: <<M as RawLock>::DefaultPolicy as LockPolicy<M>>::AcquireArgs,
    ) -> impl PinInit<KMutexGuardMultiple<'a, N, Class, M>, core::convert::Infallible> {
        KMutexGuardMultiple::new_with_args(locks, args)
    }

    /// Acquires multiple locks of the same class and type in ascending memory address order,
    /// using policy `P`.
    #[inline]
    pub fn lock_multiple_policy<'a, const N: usize, P: LockPolicy<M, AcquireArgs = ()>>(
        locks: [&'a Self; N],
    ) -> impl PinInit<KMutexGuardMultiple<'a, N, Class, M, P>, core::convert::Infallible> {
        KMutexGuardMultiple::new(locks)
    }

    /// Acquires multiple locks of the same class and type in ascending memory address order with
    /// policy `P` and arguments.
    #[inline]
    pub fn lock_multiple_policy_with<'a, const N: usize, P: LockPolicy<M>>(
        locks: [&'a Self; N],
        args: P::AcquireArgs,
    ) -> impl PinInit<KMutexGuardMultiple<'a, N, Class, M, P>, core::convert::Infallible> {
        KMutexGuardMultiple::new_with_args(locks, args)
    }

    /// Returns a reference to the underlying raw lock.
    #[inline]
    pub fn raw_mutex(&self) -> &M {
        &self.mutex
    }

    const fn class_id() -> *const core::ffi::c_void {
        if cfg!(any(feature = "lock_dep", feature = "lock_metadata_only")) {
            Class::ID
        } else {
            core::ptr::null()
        }
    }
}

impl<Class: LockClass, M> core::fmt::Debug for KMutex<Class, M> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("KMutex").field("class", &core::any::type_name::<Class>()).finish()
    }
}

/// A validation guard representing exclusive lock ownership and active list participation.
///
/// The guard is pinned in memory to ensure that its `lock_entry` pointer remains safe and valid
/// inside the C++ loop detector active thread list.
#[repr(C)]
#[pin_data(PinnedDrop)]
pub struct KMutexGuard<
    'a,
    Class: LockClass,
    M: RawLock = RawMutex,
    P: LockPolicy<M> = <M as RawLock>::DefaultPolicy,
> {
    mutex: Option<&'a KMutex<Class, M>>,

    #[pin]
    lock_entry: M::LockEntry,

    state: P::GuardState,

    token: LockToken<'a, Class>,
}

impl<'a, Class: LockClass, M: RawLock, P: LockPolicy<M>> KMutexGuard<'a, Class, M, P> {
    /// Creates a new stack-pinned validation guard initialization block.
    pub fn new(mutex: &'a KMutex<Class, M>) -> impl PinInit<Self, core::convert::Infallible>
    where
        P: LockPolicy<M, AcquireArgs = ()>,
    {
        Self::new_with_args(mutex, ())
    }

    /// Creates a new stack-pinned validation guard initialization block with policy acquire
    /// arguments.
    pub fn new_with_args(
        mutex: &'a KMutex<Class, M>,
        args: P::AcquireArgs,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        // SAFETY: The closure correctly initializes all fields of the allocated `KMutexGuard`
        // and satisfies all safety requirements of `pin_init_from_closure`.
        unsafe {
            pin_init_from_closure(move |this: *mut Self| -> Result<(), core::convert::Infallible> {
                // SAFETY: `this` is a valid pointer to uninitialized memory allocated for
                // `KMutexGuard`.

                let mutex_addr = addr_of_mut!((*this).mutex);
                core::ptr::write(mutex_addr, Some(mutex));

                let entry_addr = addr_of_mut!((*this).lock_entry);
                core::ptr::write(entry_addr, M::LockEntry::default());

                let state = P::acquire(&mutex.mutex, entry_addr, args);

                let state_addr = addr_of_mut!((*this).state);
                core::ptr::write(state_addr, state);

                let token_addr = addr_of_mut!((*this).token);
                core::ptr::write(token_addr, LockToken::new());

                Ok(())
            })
        }
    }

    /// Returns a shared reference to the lock proof `LockToken`.
    #[inline]
    pub fn token(&self) -> &LockToken<'a, Class> {
        assert!(self.mutex.is_some(), "cannot access token of a released KMutexGuard");
        &self.token
    }

    /// Returns a mutable reference to the lock proof `LockToken` inside this pinned projection.
    #[inline]
    pub fn token_mut(self: Pin<&mut Self>) -> &mut LockToken<'a, Class> {
        // SAFETY: Modifying the non-pinned raw `token` field does not violate pinning invariants
        // since the token has no drop logic or pointer-location sensitivity.
        let me = unsafe { self.get_unchecked_mut() };
        assert!(me.mutex.is_some(), "cannot access token of a released KMutexGuard");
        &mut me.token
    }

    /// Returns true if this guard currently holds the given `target` mutex instance.
    #[inline]
    pub fn is_locked_by(&self, target: &KMutex<Class, M>) -> bool {
        core::mem::size_of::<M>() > 0 && self.mutex.is_some_and(|m| core::ptr::eq(m, target))
    }

    /// Explicitly releases the lock before the guard is dropped.
    ///
    /// If the lock has already been released, this is a no-op.
    #[inline]
    pub fn release(self: Pin<&mut Self>) {
        // SAFETY: `lock_entry` is pinned on the stack and valid.
        unsafe {
            let me = self.get_unchecked_mut();
            if let Some(mutex) = me.mutex.take() {
                let entry_addr = addr_of_mut!(me.lock_entry);
                P::release(&mutex.mutex, entry_addr, me.state);
            }
        }
    }

    /// Temporarily releases the lock before executing the given callable `f` and then
    /// re-acquires the lock.
    #[inline]
    pub fn call_unlocked<R, F: FnOnce() -> R>(self: Pin<&mut Self>, f: F) -> R {
        // SAFETY: `lock_entry` is pinned on the stack and valid.
        unsafe {
            let me = self.get_unchecked_mut();
            let mutex = me.mutex.expect("cannot call_unlocked on a released KMutexGuard");
            let entry_addr = addr_of_mut!(me.lock_entry);
            P::release(&mutex.mutex, entry_addr, me.state);
            let result = f();
            P::reacquire(&mutex.mutex, entry_addr, &mut me.state);
            result
        }
    }

    /// Calls a closure while temporarily disabling lockdep tracking for the lock held by this
    /// guard.
    #[inline]
    pub fn call_untracked<R, F: FnOnce(&mut LockToken<'a, Class>) -> R>(
        self: Pin<&mut Self>,
        f: F,
    ) -> R {
        #[cfg(feature = "kernel")]
        // SAFETY: `lock_entry` is pinned on the stack and valid.
        unsafe {
            let me = self.get_unchecked_mut();
            assert!(me.mutex.is_some(), "cannot call_untracked on a released KMutexGuard");
            let entry_addr = addr_of_mut!(me.lock_entry) as *mut core::ffi::c_void;
            cpp_lock_validate_release(entry_addr);
            let result = f(&mut me.token);
            cpp_lock_validate_acquire(entry_addr);
            result
        }
        #[cfg(not(feature = "kernel"))]
        {
            // SAFETY: Accessing `me.token` does not move any pinned fields.
            let me = unsafe { self.get_unchecked_mut() };
            assert!(me.mutex.is_some(), "cannot call_untracked on a released KMutexGuard");
            f(&mut me.token)
        }
    }
}

#[pinned_drop]
impl<'a, Class: LockClass, M: RawLock, P: LockPolicy<M>> PinnedDrop
    for KMutexGuard<'a, Class, M, P>
{
    fn drop(self: Pin<&mut Self>) {
        self.release();
    }
}

/// Guards simultaneous acquisitions of multiple locks of the same class and type.
///
/// Locks are automatically sorted in-place by ascending memory address before acquisition to
/// preserve the intra-class ordering invariant and prevent AB/BA deadlocks, matching C++
/// `lockdep::GuardMultiple`.
#[repr(C)]
#[pin_data(PinnedDrop)]
pub struct KMutexGuardMultiple<
    'a,
    const N: usize,
    Class: LockClass,
    M: RawLock = RawMutex,
    P: LockPolicy<M> = <M as RawLock>::DefaultPolicy,
> {
    mutexes: [Option<&'a KMutex<Class, M>>; N],

    #[pin]
    lock_entries: [M::LockEntry; N],

    states: [P::GuardState; N],

    tokens: [LockToken<'a, Class>; N],
}

impl<'a, const N: usize, Class: LockClass, M: RawLock, P: LockPolicy<M>>
    KMutexGuardMultiple<'a, N, Class, M, P>
{
    /// Sorts an array of lock references in-place by ascending memory address using insertion sort,
    /// matching C++ `GuardMultiple::InsertionSortPointers`.
    #[inline]
    fn insertion_sort_locks(locks: &mut [&'a KMutex<Class, M>; N]) {
        let mut i = 1;
        while i < N {
            let x = locks[i];
            let x_addr = x.mutex.as_mut_ptr() as usize;
            let mut j = i;
            while j > 0 && (locks[j - 1].mutex.as_mut_ptr() as usize) > x_addr {
                locks[j] = locks[j - 1];
                j -= 1;
            }
            locks[j] = x;
            i += 1;
        }
    }

    /// Creates a new stack-pinned multi-lock validation guard initialization block.
    pub fn new(locks: [&'a KMutex<Class, M>; N]) -> impl PinInit<Self, core::convert::Infallible>
    where
        P: LockPolicy<M, AcquireArgs = ()>,
    {
        Self::new_with_args(locks, ())
    }

    /// Creates a new stack-pinned multi-lock validation guard initialization block with policy
    /// acquire arguments.
    pub fn new_with_args(
        mut locks: [&'a KMutex<Class, M>; N],
        args: P::AcquireArgs,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        Self::insertion_sort_locks(&mut locks);
        assert!(
            N <= 1 || core::mem::size_of::<M>() > 0,
            "KMutexGuardMultiple requires physical locks with non-zero size"
        );
        for i in 1..N {
            assert_ne!(
                locks[i - 1].mutex.as_mut_ptr(),
                locks[i].mutex.as_mut_ptr(),
                "KMutexGuardMultiple requires all locks to be distinct instances"
            );
        }
        // SAFETY: The closure initializes all fields of `KMutexGuardMultiple` in-place.
        unsafe {
            pin_init_from_closure(move |this: *mut Self| -> Result<(), core::convert::Infallible> {
                let mutexes_ptr =
                    addr_of_mut!((*this).mutexes) as *mut Option<&'a KMutex<Class, M>>;
                let entries_ptr = addr_of_mut!((*this).lock_entries) as *mut M::LockEntry;
                let states_ptr = addr_of_mut!((*this).states) as *mut P::GuardState;

                for (i, lock) in locks.into_iter().enumerate() {
                    core::ptr::write(mutexes_ptr.add(i), Some(lock));
                    let entry_addr = entries_ptr.add(i);
                    core::ptr::write(entry_addr, M::LockEntry::default());
                    let state = P::acquire(&lock.mutex, entry_addr, args);
                    core::ptr::write(states_ptr.add(i), state);
                }

                core::ptr::write(
                    addr_of_mut!((*this).tokens),
                    core::array::from_fn(|_| LockToken::new()),
                );

                Ok(())
            })
        }
    }

    /// Returns shared references to the lock proof tokens.
    #[inline]
    pub fn tokens(&self) -> &[LockToken<'a, Class>; N] {
        assert!(
            N == 0 || self.mutexes[0].is_some(),
            "cannot access tokens of a released KMutexGuardMultiple"
        );
        &self.tokens
    }

    /// Returns mutable references to the lock proof tokens.
    #[inline]
    pub fn tokens_mut(self: Pin<&mut Self>) -> &mut [LockToken<'a, Class>; N] {
        // SAFETY: Modifying the non-pinned `tokens` field does not violate pinning invariants.
        let me = unsafe { self.get_unchecked_mut() };
        assert!(
            N == 0 || me.mutexes[0].is_some(),
            "cannot access tokens of a released KMutexGuardMultiple"
        );
        &mut me.tokens
    }

    /// Explicitly releases all held locks in reverse order before the guard is dropped.
    ///
    /// If the locks have already been released, this is a no-op.
    #[inline]
    pub fn release(self: Pin<&mut Self>) {
        // SAFETY: `lock_entries` are pinned on the stack and valid. Locks are released in reverse
        // acquisition order (matching C++ `GuardMultiple` destruction order).
        unsafe {
            let me = self.get_unchecked_mut();
            let entries_ptr = addr_of_mut!(me.lock_entries) as *mut M::LockEntry;
            for i in (0..N).rev() {
                if let Some(mutex) = me.mutexes[i].take() {
                    P::release(&mutex.mutex, entries_ptr.add(i), me.states[i]);
                }
            }
        }
    }
}

#[pinned_drop]
impl<'a, const N: usize, Class: LockClass, M: RawLock, P: LockPolicy<M>> PinnedDrop
    for KMutexGuardMultiple<'a, N, Class, M, P>
{
    fn drop(self: Pin<&mut Self>) {
        self.release();
    }
}

/// Acquires multiple `KMutex` instances of the same class and type in ascending memory address
/// order, using the default policy.
#[inline]
pub fn guard_multiple<'a, const N: usize, Class: LockClass, M: RawLock>(
    locks: [&'a KMutex<Class, M>; N],
) -> impl PinInit<KMutexGuardMultiple<'a, N, Class, M>, core::convert::Infallible>
where
    <M as RawLock>::DefaultPolicy: LockPolicy<M, AcquireArgs = ()>,
{
    KMutexGuardMultiple::new(locks)
}

/// Acquires multiple `KMutex` instances of the same class and type in ascending memory address
/// order with arguments, using the default policy.
#[inline]
pub fn guard_multiple_with<'a, const N: usize, Class: LockClass, M: RawLock>(
    locks: [&'a KMutex<Class, M>; N],
    args: <<M as RawLock>::DefaultPolicy as LockPolicy<M>>::AcquireArgs,
) -> impl PinInit<KMutexGuardMultiple<'a, N, Class, M>, core::convert::Infallible> {
    KMutexGuardMultiple::new_with_args(locks, args)
}

/// Acquires multiple `KMutex` instances of the same class and type in ascending memory address
/// order, using policy `P`.
#[inline]
pub fn guard_multiple_policy<
    'a,
    const N: usize,
    Class: LockClass,
    M: RawLock,
    P: LockPolicy<M, AcquireArgs = ()>,
>(
    locks: [&'a KMutex<Class, M>; N],
) -> impl PinInit<KMutexGuardMultiple<'a, N, Class, M, P>, core::convert::Infallible> {
    KMutexGuardMultiple::new(locks)
}

/// Acquires multiple `KMutex` instances of the same class and type in ascending memory address
/// order with policy `P` and arguments.
#[inline]
pub fn guard_multiple_policy_with<
    'a,
    const N: usize,
    Class: LockClass,
    M: RawLock,
    P: LockPolicy<M>,
>(
    locks: [&'a KMutex<Class, M>; N],
    args: P::AcquireArgs,
) -> impl PinInit<KMutexGuardMultiple<'a, N, Class, M, P>, core::convert::Infallible> {
    KMutexGuardMultiple::new_with_args(locks, args)
}

/// Type tag to indicate aliased lock acquisition.
pub struct AliasedLock;

/// Acquires an aliased lock on two `KMutex` instances that reference the same underlying lock.
///
/// Only `lock1` is physically acquired, but the resulting guard holds proof tokens for both
/// `Class1` and `Class2`.
#[inline]
pub fn aliased_lock<'a, Class1: LockClass, Class2: LockClass, M1: RawLock, M2: RawLock>(
    lock1: &'a KMutex<Class1, M1>,
    lock2: &'a KMutex<Class2, M2>,
) -> impl PinInit<KMutexAliasedGuard<'a, Class1, Class2, M1, M2>, core::convert::Infallible>
where
    <M1 as RawLock>::DefaultPolicy: LockPolicy<M1, AcquireArgs = ()>,
{
    KMutexAliasedGuard::new(lock1, lock2)
}

/// Acquires an aliased lock with arguments on two `KMutex` instances that reference the same
/// underlying lock.
#[inline]
pub fn aliased_lock_with<'a, Class1: LockClass, Class2: LockClass, M1: RawLock, M2: RawLock>(
    lock1: &'a KMutex<Class1, M1>,
    lock2: &'a KMutex<Class2, M2>,
    args: <<M1 as RawLock>::DefaultPolicy as LockPolicy<M1>>::AcquireArgs,
) -> impl PinInit<KMutexAliasedGuard<'a, Class1, Class2, M1, M2>, core::convert::Infallible> {
    KMutexAliasedGuard::new_with_args(lock1, lock2, args)
}

/// Acquires an aliased lock with a specific policy on two `KMutex` instances that reference the
/// same underlying lock.
#[inline]
pub fn aliased_lock_policy<
    'a,
    Class1: LockClass,
    Class2: LockClass,
    M1: RawLock,
    M2: RawLock,
    P: LockPolicy<M1, AcquireArgs = ()>,
>(
    lock1: &'a KMutex<Class1, M1>,
    lock2: &'a KMutex<Class2, M2>,
) -> impl PinInit<KMutexAliasedGuard<'a, Class1, Class2, M1, M2, P>, core::convert::Infallible> {
    KMutexAliasedGuard::new(lock1, lock2)
}

/// Acquires an aliased lock with a specific policy and arguments on two `KMutex` instances that
/// reference the same underlying lock.
#[inline]
pub fn aliased_lock_policy_with<
    'a,
    Class1: LockClass,
    Class2: LockClass,
    M1: RawLock,
    M2: RawLock,
    P: LockPolicy<M1>,
>(
    lock1: &'a KMutex<Class1, M1>,
    lock2: &'a KMutex<Class2, M2>,
    args: P::AcquireArgs,
) -> impl PinInit<KMutexAliasedGuard<'a, Class1, Class2, M1, M2, P>, core::convert::Infallible> {
    KMutexAliasedGuard::new_with_args(lock1, lock2, args)
}

/// A validation guard representing ownership of two aliased locks simultaneously.
///
/// Only the first lock is physically acquired, but proof tokens for both lock classes (`Class1` and
/// `Class2`) are provided.
#[pin_data]
pub struct KMutexAliasedGuard<
    'a,
    Class1: LockClass,
    Class2: LockClass,
    M1: RawLock = RawMutex,
    M2: RawLock = M1,
    P: LockPolicy<M1> = <M1 as RawLock>::DefaultPolicy,
> {
    #[pin]
    inner: KMutexGuard<'a, Class1, M1, P>,

    token2: LockToken<'a, Class2>,

    _phantom: PhantomData<&'a KMutex<Class2, M2>>,
}

impl<'a, Class1: LockClass, Class2: LockClass, M1: RawLock, M2: RawLock, P: LockPolicy<M1>>
    KMutexAliasedGuard<'a, Class1, Class2, M1, M2, P>
{
    /// Creates a new stack-pinned aliased validation guard initialization block.
    pub fn new(
        lock1: &'a KMutex<Class1, M1>,
        lock2: &'a KMutex<Class2, M2>,
    ) -> impl PinInit<Self, core::convert::Infallible>
    where
        P: LockPolicy<M1, AcquireArgs = ()>,
    {
        Self::new_with_args(lock1, lock2, ())
    }

    /// Creates a new stack-pinned aliased validation guard initialization block with policy acquire
    /// arguments.
    pub fn new_with_args(
        lock1: &'a KMutex<Class1, M1>,
        _lock2: &'a KMutex<Class2, M2>,
        args: P::AcquireArgs,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        assert!(
            core::mem::size_of::<M1>() > 0 && core::mem::size_of::<M2>() > 0,
            "AliasedLock requires physical locks with non-zero size"
        );
        assert_eq!(
            lock1.mutex.as_mut_ptr(),
            _lock2.mutex.as_mut_ptr(),
            "AliasedLock requires lock1 and lock2 to point to the same physical lock"
        );
        pin_init!(Self {
            inner <- KMutexGuard::new_with_args(lock1, args),
            // SAFETY: `inner` holds the underlying mutex, which is aliased to represent `Class2`.
            token2: unsafe { LockToken::new() },
            _phantom: PhantomData,
        })
    }

    /// Returns shared references to both lock proof tokens `(Class1, Class2)`.
    #[inline]
    pub fn tokens(&self) -> (&LockToken<'a, Class1>, &LockToken<'a, Class2>) {
        (self.inner.token(), &self.token2)
    }

    /// Returns simultaneous mutable references to both proof tokens `(Class1, Class2)`.
    ///
    /// # Safety
    ///
    /// Both returned tokens are backed by a single physical lock acquisition. The caller must
    /// ensure that the two mutable tokens are not used to obtain aliasing mutable references to
    /// the same `KCell` or `KOnceCell` (for example, by passing both tokens to `guard_<lock>_mut`
    /// on the same struct instance).
    #[inline]
    pub unsafe fn tokens_mut(
        self: Pin<&mut Self>,
    ) -> (&mut LockToken<'a, Class1>, &mut LockToken<'a, Class2>) {
        // SAFETY: `get_unchecked_mut` and `Pin::new_unchecked` project the pinned `inner` field.
        let (inner_pin, token2) = unsafe {
            let me = self.get_unchecked_mut();
            (Pin::new_unchecked(&mut me.inner), &mut me.token2)
        };
        (inner_pin.token_mut(), token2)
    }

    /// Returns a shared reference to the primary lock proof token (`Class1`).
    #[inline]
    pub fn token(&self) -> &LockToken<'a, Class1> {
        self.inner.token()
    }

    /// Returns a mutable reference to the primary lock proof token (`Class1`).
    #[inline]
    pub fn token_mut(self: Pin<&mut Self>) -> &mut LockToken<'a, Class1> {
        self.inner_guard().token_mut()
    }

    /// Returns a pinned mutable reference to the primary inner `KMutexGuard`.
    #[inline]
    pub fn inner_guard(self: Pin<&mut Self>) -> Pin<&mut KMutexGuard<'a, Class1, M1, P>> {
        // SAFETY: `inner` is structurally pinned inside `Self`.
        unsafe { Pin::new_unchecked(&mut self.get_unchecked_mut().inner) }
    }

    /// Explicitly releases the underlying lock before the guard is dropped.
    ///
    /// If the lock has already been released, this is a no-op.
    #[inline]
    pub fn release(self: Pin<&mut Self>) {
        self.inner_guard().release();
    }

    /// Temporarily releases the lock before executing the given callable `f` and then
    /// re-acquires the lock.
    #[inline]
    pub fn call_unlocked<R, F: FnOnce() -> R>(self: Pin<&mut Self>, f: F) -> R {
        self.inner_guard().call_unlocked(f)
    }

    /// Calls a closure while temporarily disabling lockdep tracking for the lock held by this
    /// guard.
    #[inline]
    pub fn call_untracked<R, F: FnOnce(&mut LockToken<'a, Class1>) -> R>(
        self: Pin<&mut Self>,
        f: F,
    ) -> R {
        self.inner_guard().call_untracked(f)
    }
}

#[cfg(not(feature = "kernel"))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{KCell, guarded};
    use lockdep::LockClass;
    use pin_init::{pin_init, stack_pin_init};

    struct MyClass;
    impl LockClass for MyClass {
        const ID: *mut core::ffi::c_void = core::ptr::null_mut();
    }

    #[pin_init::pin_data]
    struct MyStruct {
        #[pin]
        mu: KMutex<MyClass>,
        data1: KCell<u32, MyClass>,
        data2: KCell<i32, MyClass>,
    }

    #[test]
    fn test_basic_token_access() {
        stack_pin_init!(let s = pin_init!(MyStruct {
            mu <- KMutex::init(),
            data1: KCell::new(10),
            data2: KCell::new(-5),
        }));

        lock!(let mut guard = s.mu.lock());

        // SAFETY: `guard` holds `s.mu`, which guards `s.data1` and `s.data2`.
        unsafe {
            assert_eq!(*s.data1.get(guard.token()), 10);
            assert_eq!(*s.data2.get(guard.token()), -5);
        }
        // SAFETY: `guard` holds `s.mu`, which guards `s.data1`.
        unsafe {
            let token_mut = guard.as_mut().token_mut();
            *s.data1.get_mut(token_mut) = 20;
            assert_eq!(*s.data1.get(guard.token()), 20);
        }
    }

    #[guarded]
    struct MyGuardedStruct {
        #[mutex]
        mu: KMutex,
        #[guarded_by(mu)]
        data1: u32,
        #[guarded_by(mu)]
        data2: i32,
    }

    #[test]
    fn test_macro_guarded() {
        stack_pin_init!(let s = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 100.into(),
            data2: (-50).into(),
        }));

        {
            lock!(let mut guard = s.lock_mu());

            // Safe individual field access
            assert_eq!(*guard.data1(), 100);
            assert_eq!(*guard.data2(), -50);

            *guard.as_mut().data1_mut() = 200;
            assert_eq!(*guard.data1(), 200);

            // Safe disjoint/split access
            let fields = guard.as_mut().fields_mut();
            *fields.data1 += 50;
            *fields.data2 += 50;
        }

        // Verify fields
        lock!(let guard = s.lock_mu());
        assert_eq!(*guard.data1(), 250);
        assert_eq!(*guard.data2(), 0);
    }

    #[test]
    fn test_kmutex_init() {
        stack_pin_init!(let mu = KMutex::<MyClass>::init());
        lock!(mu.lock());
    }

    #[test]
    fn test_kmutex_debug() {
        extern crate std;
        stack_pin_init!(let mu = KMutex::<MyClass>::init());
        let debug_str = std::format!("{:?}", mu);
        assert!(debug_str.contains("KMutex"));
    }

    #[guarded]
    struct MyMultiGuardedStruct {
        #[mutex]
        mu1: KMutex,
        #[mutex]
        mu2: KMutex,
        #[guarded_by(mu1)]
        data1: u32,
        #[guarded_by(mu2)]
        data2: i32,
    }

    #[test]
    fn test_macro_multi_guarded() {
        stack_pin_init!(let s = pin_init!(MyMultiGuardedStruct {
            mu1 <- KMutex::init(),
            mu2 <- KMutex::init(),
            data1: 10.into(),
            data2: 20.into(),
        }));

        lock!(let mut guard1 = s.lock_mu1());
        lock!(let mut guard2 = s.lock_mu2());

        assert_eq!(*guard1.data1(), 10);
        assert_eq!(*guard2.data2(), 20);
        *guard1.as_mut().data1_mut() = 15;
        *guard2.as_mut().data2_mut() = 25;
        assert_eq!(*guard1.data1(), 15);
        assert_eq!(*guard2.data2(), 25);
    }

    #[guarded]
    struct MyDefaultGuardedStruct {
        #[mutex]
        mu: KMutex,
        #[guarded_by(mu)]
        data: u32,
    }

    #[test]
    fn test_derive_default_guarded() {
        stack_pin_init!(let s = pin_init!(MyDefaultGuardedStruct {
            mu <- KMutex::init(),
            data: 0.into(),
        }));
        lock!(let guard = s.lock_mu());
        assert_eq!(*guard.data(), 0);
    }

    #[guarded]
    struct MyGenericGuardedStruct<T> {
        #[mutex]
        mu: KMutex,
        #[guarded_by(mu)]
        data: T,
    }

    #[test]
    fn test_macro_generic_guarded() {
        stack_pin_init!(let s = pin_init!(MyGenericGuardedStruct::<u32> {
            mu <- KMutex::init(),
            data: 0.into(),
        }));
        lock!(let mut guard = s.lock_mu());
        assert_eq!(*guard.data(), 0);

        *guard.as_mut().data_mut() = 42;
        assert_eq!(*guard.data(), 42);

        let fields = guard.as_mut().fields_mut();
        *fields.data = 100;

        let fields_shared = guard.fields();
        assert_eq!(*fields_shared.data, 100);
    }

    #[guarded]
    struct MyExplicitParentGuardedStruct {
        #[mutex]
        mu: KMutex,
        #[guarded_by(mu)]
        data: u32,
        pub label: &'static str,
    }

    impl MyExplicitParentGuardedStruct {
        pub fn has_label(&self) -> bool {
            !self.label.is_empty()
        }
    }

    impl<'a> MyExplicitParentGuardedStructMuGuard<'a> {
        pub fn process_with_context(mut self: Pin<&mut Self>) {
            let has_label = self.parent.has_label();
            let label = self.parent.label;
            if has_label && label == "apply_update" {
                let fields = self.as_mut().fields_mut();
                *fields.data = 100;
            }
        }
    }

    #[test]
    fn test_macro_guard_explicit_parent_access() {
        stack_pin_init!(let s = pin_init!(MyExplicitParentGuardedStruct {
            mu <- KMutex::init(),
            data: 0.into(),
            label: "apply_update",
        }));

        {
            lock!(let mut guard = s.lock_mu());
            guard.as_mut().process_with_context();
        }

        lock!(let guard = s.lock_mu());
        assert_eq!(*guard.data(), 100);
    }

    #[test]
    fn test_call_unlocked() {
        stack_pin_init!(let s = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 10.into(),
            data2: 20.into(),
        }));

        lock!(let mut guard = s.lock_mu());
        assert_eq!(*guard.data1(), 10);

        let unlocked_result = guard.as_mut().call_unlocked(|| {
            // During call_unlocked, the lock is temporarily released.
            42
        });
        assert_eq!(unlocked_result, 42);

        // After call_unlocked returns, the lock is held again and fields can be accessed/modified.
        *guard.as_mut().data1_mut() = 99;
        assert_eq!(*guard.data1(), 99);
    }

    #[guarded]
    struct AliasedTargetStruct {
        #[mutex(MyGuardedStructMuClass)]
        mu: KMutex<crate::PhantomMutex>,
        parent: *const MyGuardedStruct,
        #[guarded_by(mu)]
        value: u32,
    }

    impl AliasedTargetStruct {
        fn lock(&self) -> &KMutex<MyGuardedStructMuClass> {
            // SAFETY: `self.parent` points to a live `MyGuardedStruct` in the test scope.
            unsafe { &(*self.parent).mu }
        }
    }

    #[test]
    fn test_aliased_lock_basic() {
        stack_pin_init!(let s = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 100.into(),
            data2: 200.into(),
        }));

        let s_ptr: *const MyGuardedStruct = &*s;
        stack_pin_init!(let target = pin_init!(AliasedTargetStruct {
            mu: KMutex::new(crate::PhantomMutex),
            parent: s_ptr,
            value: 300.into(),
        }));

        {
            lock!(let mut guard = aliased_lock(&s.mu, target.lock()));

            // Access fields of s and target using tokens() pair
            let (t1, t2) = guard.tokens();
            // SAFETY: `t1` and `t2` are backed by `s.mu`, which guards `s` and `target`.
            unsafe {
                assert_eq!(*s.guard_mu(t1).data1(), 100);
                assert_eq!(*s.guard_mu(t1).data2(), 200);
                assert_eq!(*target.guard_mu(t2).value(), 300);
            }

            // Disjoint simultaneous mutable access to both structs
            // SAFETY: `s` and `target` are distinct structs with disjoint `KCell` fields.
            unsafe {
                let (t1_mut, t2_mut) = guard.as_mut().tokens_mut();
                *s.guard_mu_mut(t1_mut).data1_mut() = 101;
                *target.guard_mu_mut(t2_mut).value_mut() = 301;
            }

            // Call unlocked on aliased guard
            let res = guard.as_mut().call_unlocked(|| 1234);
            assert_eq!(res, 1234);

            // Verify mutations persist
            let (t1, t2) = guard.tokens();
            // SAFETY: `t1` and `t2` are backed by `s.mu`, which guards `s` and `target`.
            unsafe {
                assert_eq!(*s.guard_mu(t1).data1(), 101);
                assert_eq!(*target.guard_mu(t2).value(), 301);
            }
        }

        // Verify state with regular lock
        lock!(let guard = s.lock_mu());
        assert_eq!(*guard.data1(), 101);
    }

    #[test]
    fn test_aliased_lock_macro_method() {
        stack_pin_init!(let s = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 10.into(),
            data2: 20.into(),
        }));

        let s_ptr: *const MyGuardedStruct = &*s;
        stack_pin_init!(let target = pin_init!(AliasedTargetStruct {
            mu: KMutex::new(crate::PhantomMutex),
            parent: s_ptr,
            value: 30.into(),
        }));

        {
            lock!(let mut guard = s.lock_mu_aliased(target.lock()));
            let (_t1, t2) = guard.tokens();
            // SAFETY: `target` is guarded by `s.mu` and `_t1_mut` is unused.
            unsafe {
                assert_eq!(*target.guard_mu(t2).value(), 30);
                let (_t1_mut, t2_mut) = guard.as_mut().tokens_mut();
                *target.guard_mu_mut(t2_mut).value_mut() = 35;
            }
        }

        lock!(let guard = s.lock_mu());
        // SAFETY: `guard` holds `s.mu`, which protects `target`.
        unsafe {
            assert_eq!(*target.guard_mu(guard.token()).value(), 35);
        }
    }

    #[guarded]
    struct FlaggedMutexStruct {
        #[mutex(flags = lockdep::LOCK_FLAGS_ACTIVE_LIST_DISABLED)]
        seek_lock: KMutex,
        #[guarded_by(seek_lock)]
        seek: u64,
    }

    #[test]
    fn test_flagged_mutex_struct() {
        stack_pin_init!(let s = pin_init!(FlaggedMutexStruct {
            seek_lock <- KMutex::init(),
            seek: 0.into(),
        }));

        lock!(let mut guard = s.lock_seek_lock());
        assert_eq!(*guard.seek(), 0);
        *guard.as_mut().seek_mut() = 1024;
        assert_eq!(*guard.seek(), 1024);
    }

    #[test]
    fn test_guard_release() {
        stack_pin_init!(let s = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 10.into(),
            data2: 20.into(),
        }));

        lock!(let mut guard = s.lock_mu());
        *guard.as_mut().data1_mut() = 50;
        guard.as_mut().release();
        // Calling release again is a no-op.
        guard.as_mut().release();

        // Since `guard` was explicitly released, we can re-acquire the lock in the same scope.
        lock!(let guard2 = s.lock_mu());
        assert_eq!(*guard2.data1(), 50);
    }

    #[test]
    fn test_guard_multiple() {
        stack_pin_init!(let s1 = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 1.into(),
            data2: 2.into(),
        }));
        stack_pin_init!(let s2 = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 3.into(),
            data2: 4.into(),
        }));

        // Acquire in [s1, s2] order.
        {
            lock!(let mut guards = MyGuardedStruct::lock_mu_multiple([&*s1, &*s2]));
            let [t1, t2] = guards.as_mut().tokens_mut();
            // SAFETY: Both `s1.mu` and `s2.mu` are held by `guards`, and `s1` and `s2` are
            // distinct instances.
            unsafe {
                *s1.guard_mu_mut(t1).data1_mut() = 10;
                *s2.guard_mu_mut(t2).data1_mut() = 30;
            }
        }

        // Acquire in reverse [s2, s1] order and release early.
        {
            lock!(let mut guards = MyGuardedStruct::lock_mu_multiple([&*s2, &*s1]));
            let [t1, t2] = guards.tokens();
            // SAFETY: Both `s1.mu` and `s2.mu` are held by `guards`.
            unsafe {
                assert_eq!(*s1.guard_mu(t1).data1(), 10);
                assert_eq!(*s2.guard_mu(t2).data1(), 30);
            }
            guards.as_mut().release();

            // Verify both locks were released by re-acquiring them individually.
            lock!(let g1 = s1.lock_mu());
            lock!(let g2 = s2.lock_mu());
            assert_eq!(*g1.data1(), 10);
            assert_eq!(*g2.data1(), 30);
        }
    }

    #[test]
    #[should_panic(expected = "cannot access token of a released KMutexGuard")]
    fn test_released_guard_token_panics() {
        stack_pin_init!(let s = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 1.into(),
            data2: 2.into(),
        }));

        lock!(let mut guard = s.mu.lock());
        guard.as_mut().release();
        let _ = guard.token();
    }

    #[test]
    #[should_panic(expected = "cannot access token of a released KMutexGuard")]
    fn test_released_guard_field_access_panics() {
        stack_pin_init!(let s = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 1.into(),
            data2: 2.into(),
        }));

        lock!(let mut guard = s.lock_mu());
        guard.as_mut().release();
        let _ = guard.as_mut().data1_mut();
    }

    #[test]
    #[should_panic(expected = "cannot access tokens of a released KMutexGuardMultiple")]
    fn test_released_guard_multiple_tokens_panics() {
        stack_pin_init!(let s1 = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 1.into(),
            data2: 2.into(),
        }));
        stack_pin_init!(let s2 = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 3.into(),
            data2: 4.into(),
        }));

        lock!(let mut guards = MyGuardedStruct::lock_mu_multiple([&*s1, &*s2]));
        guards.as_mut().release();
        let _ = guards.tokens();
    }

    #[test]
    #[forbid(unsafe_code)]
    fn test_safe_guard_from_guard_and_token_guard_methods() {
        stack_pin_init!(let s = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 10.into(),
            data2: 20.into(),
        }));

        {
            lock!(let mut guard = s.mu.lock());
            {
                let tg = s.guard_mu_from_guard(&guard);
                assert_eq!(*tg.data1(), 10);
                assert_eq!(*tg.data2(), 20);
                assert!(core::ptr::eq(tg.parent(), &*s));
            }
            {
                let mut tg_mut = s.guard_mu_mut_from_guard(guard.as_mut());
                assert!(core::ptr::eq(tg_mut.parent(), &*s));
                *tg_mut.reborrow().data1_mut() = 11;
                assert_eq!(*tg_mut.as_token_guard().data1(), 11);
            }
        }

        {
            lock!(let mut guard = s.lock_mu());
            assert!(core::ptr::eq(guard.parent(), &*s));
            assert_eq!(*guard.as_token_guard().data1(), 11);
            let mut tg_mut = guard.as_mut().as_token_guard_mut();
            *tg_mut.reborrow().data2_mut() = 22;
            assert_eq!(*tg_mut.as_token_guard().data2(), 22);
        }
    }

    #[test]
    #[should_panic(expected = "KMutexGuard does not belong to this struct's mutex")]
    fn test_guard_mu_from_guard_mismatched_panics() {
        stack_pin_init!(let s1 = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 1.into(),
            data2: 2.into(),
        }));
        stack_pin_init!(let s2 = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 10.into(),
            data2: 20.into(),
        }));

        lock!(let guard2 = s2.mu.lock());
        let _ = s1.guard_mu_from_guard(&guard2);
    }

    #[test]
    #[should_panic(
        expected = "AliasedLock requires lock1 and lock2 to point to the same physical lock"
    )]
    fn test_aliased_lock_mismatched_panics() {
        stack_pin_init!(let s1 = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 1.into(),
            data2: 2.into(),
        }));
        stack_pin_init!(let s2 = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 10.into(),
            data2: 20.into(),
        }));

        lock!(let _guard = aliased_lock(&s1.mu, &s2.mu));
    }

    #[test]
    #[should_panic(expected = "KMutexGuardMultiple requires all locks to be distinct instances")]
    fn test_guard_multiple_duplicate_panics() {
        stack_pin_init!(let s1 = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 1.into(),
            data2: 2.into(),
        }));

        lock!(let _guards = MyGuardedStruct::lock_mu_multiple([&*s1, &*s1]));
    }

    #[guarded]
    struct SharedLockGuardedStruct {
        #[mutex]
        mu1: KMutex,
        #[mutex]
        mu2: KMutex,
        #[mutex]
        mu3: KMutex,
        #[guarded_by(mu1)]
        only1: u32,
        #[guarded_by(mu2)]
        only2: u32,
        #[guarded_by(mu1, mu2)]
        shared12: u32,
        #[guarded_by(mu1)]
        #[guarded_by(mu2)]
        #[pin]
        pinned_shared12: u32,
        #[guarded_by(mu1, mu2)]
        once_shared12: crate::KOnceCell<u32>,
        #[guarded_by(mu1, mu2, mu3)]
        shared123: u32,
    }

    impl SharedLockGuardedStruct {
        fn init() -> impl pin_init::PinInit<Self, core::convert::Infallible> {
            pin_init!(Self {
                mu1 <- KMutex::init(),
                mu2 <- KMutex::init(),
                mu3 <- KMutex::init(),
                only1: 1.into(),
                only2: 2.into(),
                shared12: 12.into(),
                pinned_shared12: 120.into(),
                once_shared12: crate::KOnceCell::new(),
                shared123: 123.into(),
            })
        }
    }

    #[test]
    #[forbid(unsafe_code)]
    fn test_guarded_by_multiple_locks() {
        stack_pin_init!(let s = SharedLockGuardedStruct::init());

        // Reading `shared12`, `pinned_shared12`, `once_shared12`, and `shared123` is allowed when
        // holding only `mu1`.
        {
            lock!(let mut g1 = s.lock_mu1());
            assert_eq!(*g1.only1(), 1);
            assert_eq!(*g1.shared12(), 12);
            assert_eq!(*g1.pinned_shared12(), 120);
            assert_eq!(g1.once_shared12(), None);
            assert_eq!(*g1.shared123(), 123);

            let f1 = g1.fields();
            assert_eq!(*f1.only1, 1);
            assert_eq!(*f1.shared12, 12);
            assert_eq!(*f1.pinned_shared12, 120);
            assert_eq!(f1.once_shared12, None);
            assert_eq!(*f1.shared123, 123);

            // In `fields_mut()`, `only1` is mutable while multi-lock fields are read-only.
            let f1_mut = g1.as_mut().fields_mut();
            *f1_mut.only1 = 10;
            assert_eq!(*f1_mut.shared12, 12);
            assert_eq!(*f1_mut.pinned_shared12, 120);
            assert_eq!(f1_mut.once_shared12, None);
            assert_eq!(*f1_mut.shared123, 123);
        }

        // Reading is also allowed when holding only `mu2`.
        {
            lock!(let g2 = s.lock_mu2());
            assert_eq!(*g2.only2(), 2);
            assert_eq!(*g2.shared12(), 12);
            assert_eq!(*g2.pinned_shared12(), 120);
            assert_eq!(g2.once_shared12(), None);
            assert_eq!(*g2.shared123(), 123);
        }

        // Mutating `shared12`, `pinned_shared12`, and `once_shared12` when holding both `mu1` and
        // `mu2` (via either guard or token guards).
        {
            lock!(let mut g1 = s.lock_mu1());
            lock!(let mut g2 = s.lock_mu2());

            *g1.as_mut().shared12_mut(&mut g2) = 42;
            assert_eq!(*g1.shared12(), 42);
            assert_eq!(*g2.shared12(), 42);

            *g2.as_mut().shared12_mut(&mut g1) = 84;
            assert_eq!(*g1.shared12(), 84);

            *g1.as_mut().pinned_shared12_mut(&mut g2) = 240;
            assert_eq!(*g2.pinned_shared12(), 240);

            let mut cell = g2.as_mut().once_shared12_cell(&mut g1);
            let val = cell.get_or_init(|| 777);
            assert_eq!(*val, 777);
            assert_eq!(g1.once_shared12(), Some(&777));
            assert_eq!(g2.once_shared12(), Some(&777));

            if let Some(v) = g1.as_mut().once_shared12_mut(&mut g2) {
                *v = 888;
            }
            assert_eq!(g2.once_shared12(), Some(&888));

            // TokenGuardMut access for standard, pinned, and KOnceCell multi-lock fields.
            let mut tg1 = g1.as_mut().as_token_guard_mut();
            let mut tg2 = g2.as_mut().as_token_guard_mut();
            *tg1.reborrow().shared12_mut(&mut tg2) = 99;
            assert_eq!(*tg2.shared12(), 99);
            assert_eq!(*tg2.as_token_guard().shared12(), 99);
            *tg2.reborrow().shared12_mut(&mut tg1) = 100;
            assert_eq!(*tg1.shared12(), 100);
            assert_eq!(*tg1.as_token_guard().shared12(), 100);

            assert_eq!(*tg1.pinned_shared12(), 240);
            *tg1.reborrow().pinned_shared12_mut(&mut tg2) = 360;
            assert_eq!(*tg2.pinned_shared12(), 360);
            assert_eq!(*tg2.as_token_guard().pinned_shared12(), 360);

            assert_eq!(tg1.once_shared12(), Some(&888));
            assert_eq!(tg1.as_token_guard().once_shared12(), Some(&888));
            if let Some(v) = tg1.reborrow().once_shared12_mut(&mut tg2) {
                *v = 999;
            }
            assert_eq!(tg2.reborrow().once_shared12_cell(&mut tg1).get(), Some(&999));
            assert_eq!(tg1.fields().once_shared12, Some(&999));
            assert_eq!(tg1.reborrow().fields_mut().once_shared12, Some(&999));
            assert_eq!(g1.as_mut().fields_mut().once_shared12, Some(&999));

            // Mutating 3-lock field `shared123` requires all 3 locks.
            lock!(let mut g3 = s.lock_mu3());
            *g1.as_mut().shared123_mut(&mut g2, &mut g3) = 321;
            assert_eq!(*g3.shared123(), 321);
            *g3.as_mut().shared123_mut(&mut g1, &mut g2) = 654;
            assert_eq!(*g1.shared123(), 654);
        }
    }

    #[test]
    #[should_panic(expected = "Lock guard belongs to a different struct instance")]
    fn test_multi_guarded_wrong_instance_panics() {
        stack_pin_init!(let s1 = SharedLockGuardedStruct::init());
        stack_pin_init!(let s2 = SharedLockGuardedStruct::init());

        lock!(let mut g1 = s1.lock_mu1());
        lock!(let mut g2_wrong = s2.lock_mu2());

        // Passing `g2_wrong` (locked on `s2`) to `g1` (locked on `s1`) must panic.
        *g1.as_mut().shared12_mut(&mut g2_wrong) = 42;
    }

    #[test]
    #[should_panic(expected = "Lock guard belongs to a different struct instance")]
    fn test_multi_guarded_token_guard_wrong_instance_panics() {
        stack_pin_init!(let s1 = SharedLockGuardedStruct::init());
        stack_pin_init!(let s2 = SharedLockGuardedStruct::init());

        lock!(let mut g1 = s1.lock_mu1());
        lock!(let mut g2_wrong = s2.lock_mu2());

        let mut tg1 = g1.as_mut().as_token_guard_mut();
        let mut tg2_wrong = g2_wrong.as_mut().as_token_guard_mut();

        // Passing `tg2_wrong` (bound to `s2`) to `tg1` (bound to `s1`) must panic.
        *tg1.shared12_mut(&mut tg2_wrong) = 42;
    }

    #[ksync::guarded]
    struct GenericMultiLockStruct<'lt, T, const N: usize> {
        #[mutex]
        mu1: KMutex,
        #[mutex]
        mu2: KMutex,
        #[guarded_by(mu1)]
        slice_ref: &'lt [T; N],
        #[guarded_by(mu1, mu2)]
        shared: [T; N],
    }

    #[test]
    #[forbid(unsafe_code)]
    fn test_generic_multi_lock_struct() {
        static BACKING: [u32; 2] = [10, 20];
        stack_pin_init!(let s = pin_init!(GenericMultiLockStruct::<u32, 2> {
            mu1 <- KMutex::init(),
            mu2 <- KMutex::init(),
            slice_ref: (&BACKING).into(),
            shared: [1u32, 2u32].into(),
        }));

        lock!(let mut g1 = s.lock_mu1());
        assert_eq!(*g1.slice_ref(), &[10, 20]);
        assert_eq!(*g1.shared(), [1, 2]);
        assert_eq!(*g1.fields().shared, [1, 2]);
        assert_eq!(*g1.as_mut().fields_mut().shared, [1, 2]);

        lock!(let mut g2 = s.lock_mu2());
        assert_eq!(*g2.shared(), [1, 2]);
        *g1.as_mut().shared_mut(&mut g2) = [30, 40];
        assert_eq!(*g2.shared(), [30, 40]);
    }

    #[ksync::guarded]
    struct PhantomMultiLockStruct {
        #[mutex(MyGuardedStructMuClass)]
        phantom_mu: KMutex<crate::PhantomMutex>,
        #[mutex]
        local_mu: KMutex,
        #[guarded_by(phantom_mu, local_mu)]
        shared: u32,
    }

    #[test]
    fn test_phantom_and_custom_class_multi_lock() {
        stack_pin_init!(let real_obj = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 0.into(),
            data2: 0.into(),
        }));
        stack_pin_init!(let s = pin_init!(PhantomMultiLockStruct {
            phantom_mu: KMutex::new(crate::PhantomMutex),
            local_mu <- KMutex::init(),
            shared: 50.into(),
        }));

        // SAFETY: `real_obj.mu` is the physical lock synchronizing `s.phantom_mu`.
        lock!(let mut pg = unsafe { s.lock_phantom_mu(&real_obj.mu) });
        assert_eq!(*pg.shared(), 50);

        lock!(let mut lg = s.lock_local_mu());
        assert_eq!(*lg.shared(), 50);

        *pg.as_mut().shared_mut(&mut lg) = 60;
        assert_eq!(*lg.shared(), 60);

        *lg.as_mut().shared_mut(&mut pg) = 70;
        assert_eq!(*pg.shared(), 70);
    }
}
