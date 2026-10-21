// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{DynamicLockDepMutex, LockDepGuard, LockLevel, RcuLockLevel};
use fuchsia_rcu::{
    RcuArc, RcuBox, RcuDroppable, RcuDroppableArc, RcuOptionArc, RcuOptionBox, RcuReadGuard,
    RcuReadScope, RcuWeak,
};
use fuchsia_rcu_collections::rcu_array::RcuArray;
use std::cell::Cell;
use std::marker::PhantomData;
use std::ops::Deref;
use std::sync::{Arc, Weak};

/// A synchronization wrapper that combines an RCU primitive `T` with a [`DynamicLockDepMutex`] to
/// serialize updates while permitting lock-free RCU reads, without using types for lock levels.
///
/// Readers access the underlying RCU value without acquiring the mutex via the read methods on
/// `DynamicRcuLock`. Writers must acquire the mutex via [`DynamicRcuLock::lock`] or
/// [`DynamicRcuLock::try_lock`] to obtain an [`RcuLockGuard`], which grants exclusive writer
/// access to `T`.
#[derive(RcuDroppable)]
pub struct DynamicRcuLock<T> {
    rcu: T,
    lock: DynamicLockDepMutex<()>,
}

/// A synchronization wrapper that combines an RCU primitive `T` with a lock at level `L` to
/// serialize updates while permitting lock-free RCU reads.
///
/// Readers access the underlying RCU value without acquiring the mutex via the read methods on
/// [`DynamicRcuLock`]. Writers must acquire the mutex via [`DynamicRcuLock::lock`] or
/// [`DynamicRcuLock::try_lock`] to obtain an [`RcuLockGuard`], which grants exclusive writer
/// access to `T`.
#[derive(RcuDroppable)]
pub struct RcuLock<T, L = RcuLockLevel> {
    inner: DynamicRcuLock<T>,
    _level: PhantomData<L>,
}

/// A guard that holds the update lock for an [`RcuLock`] or [`DynamicRcuLock`] and provides
/// exclusive writer access to the underlying RCU primitive `T`.
pub struct RcuLockGuard<'a, T> {
    rcu: &'a T,
    _guard: LockDepGuard<'a, ()>,
    _not_sync: PhantomData<Cell<()>>,
}

impl<T> DynamicRcuLock<T> {
    /// Creates a new `DynamicRcuLock` wrapping `rcu` at lock level `L`.
    pub const fn new<L: LockLevel>(rcu: T) -> Self {
        Self { rcu, lock: DynamicLockDepMutex::new::<L>(()) }
    }

    /// Acquires the update lock and returns a guard that allows updating the RCU cell.
    #[inline(always)]
    #[track_caller]
    pub fn lock(&self) -> RcuLockGuard<'_, T> {
        RcuLockGuard { rcu: &self.rcu, _guard: self.lock.lock(), _not_sync: PhantomData }
    }

    /// Attempts to acquire the update lock without blocking.
    #[inline(always)]
    #[track_caller]
    pub fn try_lock(&self) -> Option<RcuLockGuard<'_, T>> {
        Some(RcuLockGuard { rcu: &self.rcu, _guard: self.lock.try_lock()?, _not_sync: PhantomData })
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for DynamicRcuLock<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DynamicRcuLock").field("rcu", &self.rcu).field("lock", &self.lock).finish()
    }
}

impl<T> crate::MutexLike for DynamicRcuLock<T> {
    type Guard<'a>
        = RcuLockGuard<'a, T>
    where
        T: 'a;
    type Context = <DynamicLockDepMutex<()> as crate::MutexLike>::Context;

    #[inline(always)]
    fn context() -> Self::Context {
        <DynamicLockDepMutex<()> as crate::MutexLike>::context()
    }

    #[inline(always)]
    fn lock(&self, context: &mut Self::Context) -> Self::Guard<'_> {
        RcuLockGuard {
            rcu: &self.rcu,
            _guard: <DynamicLockDepMutex<()> as crate::MutexLike>::lock(&self.lock, context),
            _not_sync: PhantomData,
        }
    }
}

impl<T> fuchsia_sync::ResetDependencies for DynamicRcuLock<T> {
    #[inline(always)]
    unsafe fn reset_dependencies(&self) {
        // SAFETY: The caller must uphold the safety requirements of `ResetDependencies`.
        unsafe { fuchsia_sync::ResetDependencies::reset_dependencies(&self.lock) }
    }
}

impl<T, L: LockLevel> RcuLock<T, L> {
    /// Creates a new `RcuLock` wrapping `rcu`.
    pub const fn new(rcu: T) -> Self {
        Self { inner: DynamicRcuLock::new::<L>(rcu), _level: PhantomData }
    }
}

impl<T, L> Deref for RcuLock<T, L> {
    type Target = DynamicRcuLock<T>;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T: Default, L: LockLevel> Default for RcuLock<T, L> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T, L: LockLevel> From<T> for RcuLock<T, L> {
    fn from(rcu: T) -> Self {
        Self::new(rcu)
    }
}

impl<T: std::fmt::Debug, L> std::fmt::Debug for RcuLock<T, L> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RcuLock")
            .field("rcu", &self.inner.rcu)
            .field("lock", &self.inner.lock)
            .field("level", &std::any::type_name::<L>())
            .finish()
    }
}

impl<T, L> crate::MutexLike for RcuLock<T, L> {
    type Guard<'a>
        = RcuLockGuard<'a, T>
    where
        T: 'a,
        L: 'a;
    type Context = <DynamicRcuLock<T> as crate::MutexLike>::Context;

    #[inline(always)]
    fn context() -> Self::Context {
        <DynamicRcuLock<T> as crate::MutexLike>::context()
    }

    #[inline(always)]
    fn lock(&self, context: &mut Self::Context) -> Self::Guard<'_> {
        <DynamicRcuLock<T> as crate::MutexLike>::lock(&self.inner, context)
    }
}

impl<T, L> fuchsia_sync::ResetDependencies for RcuLock<T, L> {
    #[inline(always)]
    unsafe fn reset_dependencies(&self) {
        // SAFETY: The caller must uphold the safety requirements of `ResetDependencies`.
        unsafe { fuchsia_sync::ResetDependencies::reset_dependencies(&self.inner) }
    }
}

impl<T> Deref for RcuLockGuard<'_, T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        self.rcu
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for RcuLockGuard<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RcuLockGuard").field("rcu", &self.rcu).finish()
    }
}

impl<T: Send + Sync + 'static> DynamicRcuLock<RcuArc<T>> {
    /// Reads the contents of the underlying [`RcuArc`] without acquiring the update lock.
    ///
    /// Returns `None` if the wrapped `Arc` is `None` or is no longer valid.
    pub fn upgrade(&self) -> Option<Arc<T>> {
        self.rcu.upgrade()
    }

    /// Returns `true` if the underlying [`RcuArc`] currently contains a value.
    pub fn is_some(&self) -> bool {
        self.rcu.is_some()
    }

    /// Returns `true` if the underlying [`RcuArc`] does not contain a value.
    pub fn is_none(&self) -> bool {
        self.rcu.is_none()
    }
}

impl<T: RcuDroppable + Sync> DynamicRcuLock<RcuArray<T>> {
    /// Returns a reference to the element at `index`, or `None` if `index` is out of bounds.
    pub fn get<'a>(&self, scope: &'a RcuReadScope, index: usize) -> Option<&'a T> {
        self.rcu.get(scope, index)
    }

    /// Returns a slice containing the entire array bound to `scope`.
    pub fn as_slice<'a>(&self, scope: &'a RcuReadScope) -> &'a [T] {
        self.rcu.as_slice(scope)
    }
}

impl<T: RcuDroppable + Sync> RcuLockGuard<'_, RcuArray<T>> {
    /// Ensures that the array has at least `requested_size` elements, filling with
    /// `T::default()` if the array needs to be grown.
    pub fn ensure_at_least(&self, requested_size: usize)
    where
        T: Clone + Default,
    {
        // SAFETY: `RcuLockGuard` holds the update lock and is `!Sync`, guaranteeing that no other
        // writer can concurrently modify the underlying `RcuArray`.
        unsafe { self.rcu.ensure_at_least(requested_size) };
    }
}

impl<T: RcuDroppable + Sync> DynamicRcuLock<RcuBox<T>> {
    /// Reads the value of the wrapped [`RcuBox`] without acquiring the update lock.
    pub fn read(&self) -> RcuReadGuard<T> {
        self.rcu.read()
    }

    /// Returns a reference to the value of the wrapped [`RcuBox`] bound to `scope`.
    pub fn as_ref<'a>(&self, scope: &'a RcuReadScope) -> &'a T {
        self.rcu.as_ref(scope)
    }

    /// Returns a clone of the value of the wrapped [`RcuBox`].
    pub fn cloned(&self) -> T
    where
        T: Clone,
    {
        self.rcu.cloned()
    }
}

impl<T: RcuDroppable + Sync> DynamicRcuLock<RcuDroppableArc<T>> {
    /// Reads the value of the wrapped [`RcuDroppableArc`] without acquiring the update lock.
    pub fn read(&self) -> RcuReadGuard<T> {
        self.rcu.read()
    }

    /// Returns a reference to the value of the wrapped [`RcuDroppableArc`] bound to `scope`.
    pub fn as_ref<'a>(&self, scope: &'a RcuReadScope) -> &'a T {
        self.rcu.as_ref(scope)
    }

    /// Creates a new `Arc<T>` referencing the same object as the underlying [`RcuDroppableArc`].
    pub fn to_arc(&self) -> Arc<T> {
        self.rcu.to_arc()
    }
}

impl<T: RcuDroppable + Sync> DynamicRcuLock<RcuOptionArc<T>> {
    /// Reads the value of the wrapped [`RcuOptionArc`] without acquiring the update lock.
    pub fn read(&self) -> Option<RcuReadGuard<T>> {
        self.rcu.read()
    }

    /// Returns a reference to the value of the wrapped [`RcuOptionArc`] bound to `scope`.
    pub fn as_ref<'a>(&self, scope: &'a RcuReadScope) -> Option<&'a T> {
        self.rcu.as_ref(scope)
    }

    /// Creates a new `Option<Arc<T>>` referencing the same object as the underlying
    /// [`RcuOptionArc`].
    pub fn to_option_arc(&self) -> Option<Arc<T>> {
        self.rcu.to_option_arc()
    }
}

impl<T: RcuDroppable + Sync> DynamicRcuLock<RcuOptionBox<T>> {
    /// Reads the value of the wrapped [`RcuOptionBox`] without acquiring the update lock.
    pub fn read(&self) -> Option<RcuReadGuard<T>> {
        self.rcu.read()
    }

    /// Returns a reference to the value of the wrapped [`RcuOptionBox`] bound to `scope`.
    pub fn as_ref<'a>(&self, scope: &'a RcuReadScope) -> Option<&'a T> {
        self.rcu.as_ref(scope)
    }

    /// Returns `true` if the underlying [`RcuOptionBox`] currently contains a value.
    pub fn is_some(&self, scope: &RcuReadScope) -> bool {
        self.rcu.is_some(scope)
    }

    /// Returns `true` if the underlying [`RcuOptionBox`] does not contain a value.
    pub fn is_none(&self, scope: &RcuReadScope) -> bool {
        self.rcu.is_none(scope)
    }

    /// Returns a clone of the value of the wrapped [`RcuOptionBox`].
    pub fn cloned(&self) -> Option<T>
    where
        T: Clone,
    {
        self.rcu.cloned()
    }
}

impl<T: Send + Sync + 'static> DynamicRcuLock<RcuWeak<T>> {
    /// Attempts to upgrade the wrapped [`Weak`] pointer to an [`Arc`] without acquiring the update
    /// lock.
    pub fn upgrade(&self) -> Option<Arc<T>> {
        self.rcu.upgrade()
    }

    /// Creates a new [`Weak`] pointer to the object referenced by the underlying [`RcuWeak`].
    pub fn to_weak(&self) -> Weak<T> {
        self.rcu.to_weak()
    }

    /// Returns `true` if the wrapped weak pointer points to the same allocation as `other`.
    pub fn ptr_eq(&self, other: &Weak<T>) -> bool {
        self.rcu.ptr_eq(other)
    }

    /// Returns the number of strong (`Arc`) pointers pointing to the allocation.
    pub fn strong_count(&self, scope: &RcuReadScope) -> usize {
        self.rcu.strong_count(scope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LockDepMutex, lock_ordering, ordered_lock, ordered_lock_vec};
    use fuchsia_rcu::rcu_run_callbacks;
    use fuchsia_sync::ResetDependencies;
    use std::sync::atomic::{AtomicUsize, Ordering};

    lock_ordering! {
        Unlocked => LevelA,
        LevelA => LevelB,
    }

    struct DropCounter {
        value: usize,
        drops: Arc<AtomicUsize>,
    }

    impl DropCounter {
        fn new(value: usize, drops: Arc<AtomicUsize>) -> Arc<Self> {
            Arc::new(Self { value, drops })
        }
    }

    impl Drop for DropCounter {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[::fuchsia::test]
    fn test_rcu_lock_arc() {
        let drops = Arc::new(AtomicUsize::new(0));
        let cell: RcuLock<RcuArc<DropCounter>, LevelA> = RcuLock::default();

        assert!(cell.is_none());
        assert!(!cell.is_some());
        assert!(cell.upgrade().is_none());

        cell.lock().update(Some(DropCounter::new(42, drops.clone())));
        assert!(cell.is_some());
        assert!(!cell.is_none());
        assert_eq!(cell.upgrade().unwrap().value, 42);
        assert_eq!(drops.load(Ordering::Relaxed), 0);

        {
            let guard = cell.lock();
            assert!(guard.is_some());
            assert!(!guard.is_none());
            assert_eq!(guard.upgrade().unwrap().value, 42);
            guard.update(Some(DropCounter::new(99, drops.clone())));
        }
        assert_eq!(cell.upgrade().unwrap().value, 99);
        assert_eq!(drops.load(Ordering::Relaxed), 1);

        cell.lock().update(None);
        assert!(cell.is_none());
        assert_eq!(drops.load(Ordering::Relaxed), 2);

        rcu_run_callbacks();
    }

    #[::fuchsia::test]
    fn test_rcu_lock_array() {
        let array: RcuLock<RcuArray<u32>> = RcuLock::default();
        {
            let scope = RcuReadScope::new();
            assert!(array.as_slice(&scope).is_empty());
            assert_eq!(array.get(&scope, 0), None);
        }

        array.lock().update(vec![10, 20, 30]);
        {
            let scope = RcuReadScope::new();
            assert_eq!(array.as_slice(&scope), &[10, 20, 30]);
            assert_eq!(array.get(&scope, 1), Some(&20));
            assert_eq!(array.get(&scope, 3), None);
        }

        {
            let guard = array.lock();
            guard.ensure_at_least(5);
            let scope = RcuReadScope::new();
            assert!(guard.as_slice(&scope).len() >= 5);
            assert_eq!(&guard.as_slice(&scope)[..3], &[10, 20, 30]);
            assert_eq!(guard.get(&scope, 2), Some(&30));
        }

        rcu_run_callbacks();
    }

    #[::fuchsia::test]
    fn test_rcu_lock_box_and_options() {
        let boxed: RcuLock<RcuBox<u32>> = RcuLock::new(RcuBox::new(1));
        assert_eq!(*boxed.read(), 1);
        assert_eq!(boxed.cloned(), 1);
        {
            let guard = boxed.lock();
            assert_eq!(*guard.read(), 1);
            assert_eq!(guard.cloned(), 1);
            guard.update(2);
            let scope = RcuReadScope::new();
            assert_eq!(guard.as_ref(&scope), &2);
            assert_eq!(boxed.as_ref(&scope), &2);
        }

        let droppable_arc: RcuLock<RcuDroppableArc<u32>> =
            RcuLock::from(RcuDroppableArc::new(5.into()));
        assert_eq!(*droppable_arc.read(), 5);
        assert_eq!(*droppable_arc.to_arc(), 5);
        {
            let scope = RcuReadScope::new();
            assert_eq!(droppable_arc.as_ref(&scope), &5);
            let guard = droppable_arc.lock();
            assert_eq!(*guard.read(), 5);
            assert_eq!(*guard.to_arc(), 5);
            assert_eq!(guard.as_ref(&scope), &5);
            let old = guard.update_swap(&scope, Arc::new(6));
            assert_eq!(*old, 5);
            assert_eq!(droppable_arc.as_ref(&scope), &6);
            guard.update(Arc::new(7));
            assert_eq!(droppable_arc.as_ref(&scope), &7);
        }

        let option_arc: RcuLock<RcuOptionArc<u32>> = RcuLock::default();
        assert!(option_arc.read().is_none());
        {
            let guard = option_arc.lock();
            assert!(guard.read().is_none());
            assert!(guard.to_option_arc().is_none());
            guard.update(Some(Arc::new(7)));
            assert_eq!(*guard.to_option_arc().unwrap(), 7);
            let scope = RcuReadScope::new();
            assert_eq!(guard.as_ref(&scope), Some(&7));
            assert_eq!(option_arc.as_ref(&scope), Some(&7));
        }
        assert_eq!(*option_arc.to_option_arc().unwrap(), 7);

        let option_box: RcuLock<RcuOptionBox<u32>> = RcuLock::default();
        assert!(option_box.read().is_none());
        {
            let scope = RcuReadScope::new();
            assert!(option_box.is_none(&scope));
            assert!(!option_box.is_some(&scope));
        }
        {
            let guard = option_box.lock();
            let scope = RcuReadScope::new();
            assert!(guard.read().is_none());
            assert!(guard.is_none(&scope));
            assert!(!guard.is_some(&scope));
            guard.update(Some(8));
            assert_eq!(guard.cloned(), Some(8));
            assert!(guard.is_some(&scope));
            assert!(!guard.is_none(&scope));
            assert_eq!(guard.as_ref(&scope), Some(&8));
        }
        assert_eq!(option_box.cloned(), Some(8));
        {
            let scope = RcuReadScope::new();
            assert!(option_box.is_some(&scope));
            assert!(!option_box.is_none(&scope));
            assert_eq!(option_box.as_ref(&scope), Some(&8));
        }

        let target = Arc::new(11u32);
        let weak_target = Arc::downgrade(&target);
        let rcu_weak: RcuLock<RcuWeak<u32>> = RcuLock::default();
        assert!(rcu_weak.upgrade().is_none());
        {
            let guard = rcu_weak.lock();
            assert!(guard.upgrade().is_none());
            guard.update(weak_target.clone());
            assert_eq!(*guard.upgrade().unwrap(), 11);
            assert!(guard.ptr_eq(&weak_target));
            assert!(guard.to_weak().upgrade().is_some());
            let scope = RcuReadScope::new();
            assert_eq!(guard.strong_count(&scope), 1);
        }
        assert_eq!(*rcu_weak.upgrade().unwrap(), 11);
        assert!(rcu_weak.ptr_eq(&weak_target));
        assert!(rcu_weak.to_weak().upgrade().is_some());
        {
            let scope = RcuReadScope::new();
            assert_eq!(rcu_weak.strong_count(&scope), 1);
        }

        rcu_run_callbacks();
    }

    #[::fuchsia::test]
    fn test_rcu_lock_conversions_and_accessors() {
        let cell: RcuLock<RcuBox<u32>, LevelA> = RcuBox::new(20).into();
        assert_eq!(cell.cloned(), 20);

        assert!(!format!("{cell:?}").is_empty());
        {
            let guard = cell.lock();
            assert!(!format!("{guard:?}").is_empty());
        }

        // SAFETY: No locks are held while resetting dependencies on `cell`.
        unsafe {
            cell.reset_dependencies();
        }
        rcu_run_callbacks();
    }

    #[::fuchsia::test]
    fn test_rcu_lock_try_lock_and_ordered_lock() {
        let cell1: RcuLock<RcuArc<u32>, LevelA> = RcuLock::default();
        let cell2: RcuLock<RcuArc<u32>, LevelA> = RcuLock::default();

        let guard = cell1.try_lock().expect("try_lock should succeed when unlocked");
        std::thread::scope(|scope| {
            scope.spawn(|| {
                assert!(cell1.try_lock().is_none());
            });
        });
        drop(guard);

        let (guard1, guard2) = ordered_lock(&cell1, &cell2);
        guard1.update(Some(Arc::new(1)));
        guard2.update(Some(Arc::new(2)));
        drop((guard1, guard2));

        let (guard2, guard1) = ordered_lock(&cell2, &cell1);
        guard1.update(Some(Arc::new(3)));
        guard2.update(Some(Arc::new(4)));
        drop((guard2, guard1));

        let guards = ordered_lock_vec(&[&cell2, &cell1]);
        guards[0].update(Some(Arc::new(20)));
        guards[1].update(Some(Arc::new(10)));
        drop(guards);

        assert_eq!(*cell1.upgrade().unwrap(), 10);
        assert_eq!(*cell2.upgrade().unwrap(), 20);
        rcu_run_callbacks();
    }

    #[::fuchsia::test]
    fn test_rcu_lock_while_holding_outer_lock() {
        let outer: LockDepMutex<u32, LevelA> = LockDepMutex::new(1);
        let inner_rcu: RcuLock<RcuArray<u32>> = RcuLock::default();

        let _outer_guard = outer.lock();
        inner_rcu.lock().update(vec![1, 2, 3]);

        let scope = RcuReadScope::new();
        assert_eq!(inner_rcu.as_slice(&scope), &[1, 2, 3]);
        drop(scope);
        drop(_outer_guard);
        rcu_run_callbacks();
    }

    fn assert_rcu_droppable<T: RcuDroppable>() {}

    #[::fuchsia::test]
    fn test_rcu_lock_droppable() {
        assert_rcu_droppable::<DynamicRcuLock<RcuArray<u32>>>();
        assert_rcu_droppable::<RcuLock<RcuArray<u32>>>();
        assert_rcu_droppable::<RcuLock<RcuBox<u32>, LevelA>>();
        assert_rcu_droppable::<RcuLock<RcuDroppableArc<u32>, LevelA>>();
        assert_rcu_droppable::<RcuLock<RcuOptionArc<u32>, LevelA>>();
        assert_rcu_droppable::<RcuLock<RcuOptionBox<u32>, LevelA>>();
    }

    #[::fuchsia::test]
    fn test_dynamic_rcu_lock() {
        let dyn_cell = DynamicRcuLock::new::<LevelA>(RcuArc::new(Some(Arc::new(10u32))));
        assert!(dyn_cell.is_some());
        assert_eq!(*dyn_cell.upgrade().unwrap(), 10);

        assert!(dyn_cell.try_lock().is_some());
        dyn_cell.lock().update(Some(Arc::new(30)));
        assert_eq!(*dyn_cell.upgrade().unwrap(), 30);

        let typed_cell: RcuLock<RcuArc<u32>, LevelA> = RcuLock::new(RcuArc::default());
        let dyn_ref: &DynamicRcuLock<RcuArc<u32>> = &typed_cell;
        let (g1, g2) = ordered_lock(&dyn_cell, dyn_ref);
        g1.update(Some(Arc::new(40)));
        g2.update(Some(Arc::new(50)));
        drop((g1, g2));

        assert_eq!(*dyn_cell.upgrade().unwrap(), 40);
        assert_eq!(*typed_cell.upgrade().unwrap(), 50);
        assert!(!format!("{dyn_cell:?}").is_empty());

        // SAFETY: No locks are held while resetting dependencies on `dyn_cell`.
        unsafe {
            dyn_cell.reset_dependencies();
        }
        rcu_run_callbacks();
    }

    #[cfg(feature = "detect_lock_dep_cycles")]
    #[test]
    #[should_panic(expected = "Invalid lock ordering cycle detected")]
    fn test_dynamic_rcu_lock_ordering_cycle_panics() {
        let dyn_a = DynamicRcuLock::new::<LevelA>(RcuArc::<u32>::default());
        let dyn_b = DynamicRcuLock::new::<LevelB>(RcuArc::<u32>::default());

        let _guard_b = dyn_b.lock();
        let _guard_a = dyn_a.lock();
    }

    #[cfg(feature = "detect_lock_dep_cycles")]
    #[test]
    #[should_panic(expected = "Invalid lock ordering cycle detected")]
    fn test_rcu_lock_ordering_cycle_panics() {
        let rcu_a: RcuLock<RcuArc<u32>, LevelA> = RcuLock::default();
        let mutex_b: LockDepMutex<u32, LevelB> = LockDepMutex::new(0);

        let _guard_b = mutex_b.lock();
        let _guard_a = rcu_a.lock();
    }

    #[cfg(feature = "detect_lock_dep_cycles")]
    #[test]
    #[should_panic(expected = "LockDep: Self-deadlock detected")]
    fn test_rcu_lock_self_deadlock_panics() {
        let rcu_a: RcuLock<RcuArc<u32>, LevelA> = RcuLock::default();

        let _guard1 = rcu_a.lock();
        let _guard2 = rcu_a.lock();
    }

    #[cfg(feature = "detect_lock_dep_cycles")]
    #[test]
    #[should_panic(expected = "Invalid lock ordering cycle detected")]
    fn test_rcu_lock_try_lock_ordering_cycle_panics() {
        let rcu_a: RcuLock<RcuArc<u32>, LevelA> = RcuLock::default();
        let mutex_b: LockDepMutex<u32, LevelB> = LockDepMutex::new(0);

        let _guard_b = mutex_b.try_lock();
        let _guard_a = rcu_a.try_lock();
    }
}
