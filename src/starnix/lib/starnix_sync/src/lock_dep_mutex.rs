// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{MutexLike, RwLockLike};
use fuchsia_sync::{
    MappedMutexGuard, MappedRwLockReadGuard, MappedRwLockWriteGuard, MutexGuard, RwLockReadGuard,
    RwLockWriteGuard,
};
use std::marker::PhantomData;

#[cfg(feature = "detect_lock_dep_cycles")]
mod tracking {
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Represents a lock held by the current thread.
    struct HeldLock {
        /// The encoded value of the lock (Lock ID | Subclass).
        encoded_value: usize,
        /// The count of active subclass tokens for this lock.
        active_subclass_tokens: usize,
        /// The name of the lock level.
        name: &'static str,
    }

    /// Centralized thread-local state for lockdep tracking.
    struct ThreadState {
        /// The stack of currently held locks on this thread.
        held_locks: Vec<HeldLock>,
    }

    thread_local! {
        static STATE: RefCell<ThreadState> = const { RefCell::new(ThreadState {
            held_locks: Vec::new(),
        }) };
    }

    /// Verifies that acquiring a lock with `target_value` does not violate lock ordering.
    /// If valid, pushes the lock onto the thread-local stack.
    ///
    /// Panics if a self-deadlock or lock cycle is detected.
    #[inline(always)]
    fn check_and_push_lock(target_value: usize, name: &'static str) {
        STATE.with(|state| {
            let mut s = state.borrow_mut();
            if let Some(last) = s.held_locks.last() {
                let last_value = last.encoded_value;
                let last_level = last_value & !0xF;
                let target_level = target_value & !0xF;

                if target_value == last_value {
                    panic!(
                        "LockDep: Self-deadlock detected on lock '{name}' (level {target_value})!"
                    );
                }
                if target_level < last_level {
                    panic!(
                        "Invalid lock ordering cycle detected: attempted to acquire '{name}' \
                        after '{}' ({target_level} < {last_level})!",
                        last.name
                    );
                }
                if target_level == last_level {
                    // We are acquiring a sublock!
                    if last.active_subclass_tokens == 0 {
                        panic!(
                            "LockDep: Subclassing not allowed or already consumed for lock '{}'",
                            last.name
                        );
                    }
                }
            }
            s.held_locks.push(HeldLock {
                encoded_value: target_value,
                active_subclass_tokens: 0,
                name,
            });
        });
    }

    /// Removes a lock from the thread-local stack when it is released.
    #[inline(always)]
    fn pop_lock(target_value: usize) {
        STATE.with(|state| {
            let mut s = state.borrow_mut();
            let Some(pos) = s.held_locks.iter().rposition(|v| v.encoded_value == target_value)
            else {
                panic!(
                    "LockDep: Attempted to pop a tracked lock that was not tracked. \
                    Discrepancy detected. Target Lock : {target_value}"
                );
            };
            let lock = &s.held_locks[pos];
            if lock.active_subclass_tokens > 0 {
                let stack_str = s
                    .held_locks
                    .iter()
                    .map(|v| format!("{:X}:{}", v.encoded_value, v.active_subclass_tokens))
                    .collect::<Vec<_>>()
                    .join(", ");
                panic!(
                    "LockDep: Attempted to drop a lock with active subclass tokens! \
                        Target: {:X}, tokens: {}, Stack: [{}]",
                    target_value, lock.active_subclass_tokens, stack_str
                );
            }
            s.held_locks.remove(pos);
        });
    }

    #[cfg(test)]
    pub fn clear_state() {
        STATE.with(|state| state.borrow_mut().held_locks.clear());
    }

    /// Retrieves the allowed subclass for a given lock ID.
    ///
    /// Returns `0` if no subclass is currently authorized.
    #[inline(always)]
    fn get_subclass(lock_id: usize) -> u8 {
        STATE.with(|state| {
            let s = state.borrow();
            if let Some(last) = s.held_locks.last() {
                let last_lock_id = last.encoded_value & !0xF;
                if last_lock_id == lock_id && last.active_subclass_tokens > 0 {
                    return (last.encoded_value & 0xF) as u8 + 1;
                }
            }
            0
        })
    }

    /// Authorizes an incremented subclass for the currently maximal held lock.
    ///
    /// Returns the lock ID and the new subclass level.
    #[inline(always)]
    fn enable_subclass_for_maximal() -> usize {
        STATE.with(|state| {
            let mut s = state.borrow_mut();
            if let Some(last) = s.held_locks.last_mut() {
                last.active_subclass_tokens += 1;
                last.encoded_value
            } else {
                // No locks held. Return placeholder.
                usize::MAX
            }
        })
    }

    /// Revokes the subclass authorization for the given lock ID when a `SubclassToken` is dropped.
    #[inline(always)]
    fn disable_subclass(encoded_value: usize) {
        if encoded_value == usize::MAX {
            return;
        }
        STATE.with(|state| {
            let mut s = state.borrow_mut();
            let Some(pos) = s.held_locks.iter().rposition(|v| v.encoded_value == encoded_value)
            else {
                panic!(
                    "LockDep: Attempted to disable subclass for a lock that is not on the stack! \
                    Value: {:X}",
                    encoded_value
                );
            };
            let lock = &mut s.held_locks[pos];
            if lock.active_subclass_tokens == 0 {
                panic!(
                    "LockDep: Attempted to disable subclass for a lock with no active tokens! \
                    Value: {:X}",
                    encoded_value
                );
            }
            lock.active_subclass_tokens -= 1;
        });
    }

    /// A token that represents a lock level being held for lockdep purposes.
    /// This does not actually hold a lock, but updates the lockdep state as if it did.
    #[derive(Clone)]
    pub struct LockLevelToken {
        inner: Rc<InternalLockLevelToken>,
    }

    struct InternalLockLevelToken {
        target_value: usize,
    }

    impl LockLevelToken {
        pub(super) fn new(lock_id: usize, name: &'static str) -> Self {
            let subclass = get_subclass(lock_id);
            assert!(subclass < 16, "subclass must be between 0 and 15");
            let target_value = lock_id | (subclass as usize & 0xF);
            check_and_push_lock(target_value, name);
            Self { inner: Rc::new(InternalLockLevelToken { target_value }) }
        }

        fn target_value(&self) -> usize {
            self.inner.target_value
        }

        pub(super) fn check_maximal(&self) {
            STATE.with(|state| {
                if let Some(last) = state.borrow().held_locks.last() {
                    assert_eq!(
                        last.encoded_value, self.inner.target_value,
                        "Condvar wait requires the lock to be the latest acquired lock.",
                    );
                }
            })
        }
    }

    /// Tracking information for dynamic locks.
    pub struct DynamicLockTracking {
        lock_id: usize,
        name: &'static str,
    }

    impl DynamicLockTracking {
        pub(super) const fn new(lock_id: usize, name: &'static str) -> Self {
            Self { lock_id, name }
        }

        pub(super) fn lock_id(&self) -> usize {
            self.lock_id
        }

        pub(super) fn name(&self) -> &'static str {
            self.name
        }
    }

    impl Drop for InternalLockLevelToken {
        fn drop(&mut self) {
            pop_lock(self.target_value);
        }
    }

    /// A token that allows the next lock acquisition of the same level as the currently maximal
    /// held lock to use an incremented subclass.
    pub struct SubclassToken {
        encoded_value: usize,
    }

    impl SubclassToken {
        pub(super) fn new() -> Self {
            let encoded_value = enable_subclass_for_maximal();
            Self { encoded_value }
        }
    }

    impl Drop for SubclassToken {
        fn drop(&mut self) {
            disable_subclass(self.encoded_value);
        }
    }

    #[derive(Default)]
    pub struct LockDepContext {
        token: Option<LockLevelToken>,
    }

    pub(super) fn lock_with_context<'a, T>(
        mutex: &'a crate::DynamicLockDepMutex<T>,
        context: &mut LockDepContext,
    ) -> crate::LockDepGuard<'a, T> {
        match &mut context.token {
            token @ None => {
                let guard = mutex.lock();
                *token = Some(guard.token.clone());
                guard
            }
            Some(token) => {
                assert_eq!(
                    mutex.tracking.lock_id(),
                    token.target_value() & !0xF,
                    "LockDep: Cannot mix different lock levels in ordered_lock_vec"
                );
                let inner = mutex.inner.lock();
                crate::LockDepGuard { inner, token: token.clone() }
            }
        }
    }

    pub(super) fn read_with_context<'a, T>(
        rwlock: &'a crate::DynamicLockDepRwLock<T>,
        context: &mut LockDepContext,
    ) -> crate::LockDepReadGuard<'a, T> {
        match &mut context.token {
            token @ None => {
                let guard = rwlock.read();
                *token = Some(guard.token.clone());
                guard
            }
            Some(token) => {
                assert_eq!(
                    rwlock.tracking.lock_id(),
                    token.target_value() & !0xF,
                    "LockDep: Cannot mix different lock levels in ordered_lock_vec"
                );
                let inner = rwlock.inner.read();
                crate::LockDepReadGuard { inner, token: token.clone() }
            }
        }
    }

    pub(super) fn write_with_context<'a, T>(
        rwlock: &'a crate::DynamicLockDepRwLock<T>,
        context: &mut LockDepContext,
    ) -> crate::LockDepWriteGuard<'a, T> {
        match &mut context.token {
            token @ None => {
                let guard = rwlock.write();
                *token = Some(guard.token.clone());
                guard
            }
            Some(token) => {
                assert_eq!(
                    rwlock.tracking.lock_id(),
                    token.target_value() & !0xF,
                    "LockDep: Cannot mix different lock levels in ordered_lock_vec"
                );
                let inner = rwlock.inner.write();
                crate::LockDepWriteGuard { inner, token: token.clone() }
            }
        }
    }
}

#[cfg(not(feature = "detect_lock_dep_cycles"))]
mod tracking {
    /// A token that represents a lock level being held for lockdep purposes.
    /// This does not actually hold a lock, but updates the lockdep state as if it did.
    #[derive(Clone)]
    pub struct LockLevelToken {}

    impl LockLevelToken {
        #[inline(always)]
        pub(super) fn new(_lock_id: usize, _name: &'static str) -> Self {
            Self {}
        }

        pub(super) fn check_maximal(&self) {}
    }

    /// Tracking information for dynamic locks.
    pub struct DynamicLockTracking {}

    impl DynamicLockTracking {
        pub(super) const fn new(_lock_id: usize, _name: &'static str) -> Self {
            Self {}
        }

        pub(super) fn lock_id(&self) -> usize {
            0
        }

        pub(super) fn name(&self) -> &'static str {
            ""
        }
    }

    pub struct SubclassToken {}

    impl SubclassToken {
        #[inline(always)]
        pub(super) fn new() -> Self {
            Self {}
        }
    }

    pub type LockDepContext = ();

    pub(super) fn lock_with_context<'a, T>(
        mutex: &'a crate::DynamicLockDepMutex<T>,
        _context: &mut LockDepContext,
    ) -> crate::LockDepGuard<'a, T> {
        mutex.lock()
    }

    pub(super) fn read_with_context<'a, T>(
        rwlock: &'a crate::DynamicLockDepRwLock<T>,
        _context: &mut LockDepContext,
    ) -> crate::LockDepReadGuard<'a, T> {
        rwlock.read()
    }

    pub(super) fn write_with_context<'a, T>(
        rwlock: &'a crate::DynamicLockDepRwLock<T>,
        _context: &mut LockDepContext,
    ) -> crate::LockDepWriteGuard<'a, T> {
        rwlock.write()
    }
}

/// A Mutex that dynamically enforces lock ordering at runtime, without using types for levels.
pub struct DynamicLockDepMutex<T> {
    inner: fuchsia_sync::Mutex<T>,
    tracking: tracking::DynamicLockTracking,
}

impl<T: std::fmt::Debug> std::fmt::Debug for DynamicLockDepMutex<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DynamicLockDepMutex({:?})", self.inner)
    }
}

impl<T> DynamicLockDepMutex<T> {
    pub const fn new<L: crate::LockLevel>(value: T) -> Self {
        Self {
            inner: fuchsia_sync::Mutex::new(value),
            tracking: tracking::DynamicLockTracking::new(L::LOCK_ID, L::NAME),
        }
    }

    #[inline(always)]
    pub fn lock(&self) -> LockDepGuard<'_, T> {
        let token = tracking::LockLevelToken::new(self.tracking.lock_id(), self.tracking.name());
        LockDepGuard { inner: self.inner.lock(), token }
    }

    /// Returns a mutable reference to the underlying data.
    ///
    /// Since this call borrows the `DynamicLockDepMutex` mutably, no actual locking takes place -- the
    /// borrow checker statically ensures no other threads have access to the `DynamicLockDepMutex`.
    pub fn get_mut(&mut self) -> &mut T {
        self.inner.get_mut()
    }

    /// Consumes the `DynamicLockDepMutex`, returning the underlying data.
    pub fn into_inner(self) -> T {
        self.inner.into_inner()
    }
}

impl<T> MutexLike for DynamicLockDepMutex<T> {
    type Guard<'a>
        = LockDepGuard<'a, T>
    where
        T: 'a;
    type Context = tracking::LockDepContext;

    #[inline(always)]
    fn context() -> Self::Context {
        Default::default()
    }

    #[inline(always)]
    fn lock(&self, context: &mut Self::Context) -> Self::Guard<'_> {
        tracking::lock_with_context(self, context)
    }
}

pub struct LockDepGuard<'a, T> {
    inner: MutexGuard<'a, T>,
    token: tracking::LockLevelToken,
}

impl<'a, T> std::ops::Deref for LockDepGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.inner.deref()
    }
}

impl<'a, T> std::ops::DerefMut for LockDepGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.inner.deref_mut()
    }
}

impl<'a, T> LockDepGuard<'a, T> {
    pub(super) fn check_maximal(&self) {
        self.token.check_maximal();
    }
}

impl<'a, T> crate::condvar::WaitableMutexGuard<'a, T> for LockDepGuard<'a, T> {
    fn inner_guard(&mut self, _token: crate::condvar::WaitToken) -> &mut MutexGuard<'a, T> {
        self.check_maximal();
        &mut self.inner
    }
}

/// A Mutex that dynamically enforces lock ordering at runtime using types for levels.
pub struct LockDepMutex<T, L> {
    inner: DynamicLockDepMutex<T>,
    _level: PhantomData<L>,
}

impl<T: std::fmt::Debug, L> std::fmt::Debug for LockDepMutex<T, L> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LockDepMutex({:?}, {})", self.inner.inner, std::any::type_name::<L>())
    }
}

impl<T, L: crate::LockLevel> LockDepMutex<T, L> {
    pub const fn new(value: T) -> Self {
        Self { inner: DynamicLockDepMutex::new::<L>(value), _level: PhantomData }
    }

    #[inline(always)]
    pub fn lock(&self) -> LockDepGuard<'_, T> {
        self.inner.lock()
    }

    /// Returns a mutable reference to the underlying data.
    ///
    /// Since this call borrows the `LockDepMutex` mutably, no actual locking takes place -- the
    /// borrow checker statically ensures no other threads have access to the `LockDepMutex`.
    pub fn get_mut(&mut self) -> &mut T {
        self.inner.get_mut()
    }

    /// Consumes the `LockDepMutex`, returning the underlying data.
    pub fn into_inner(self) -> T {
        self.inner.into_inner()
    }
}

impl<T, L> MutexLike for LockDepMutex<T, L> {
    type Guard<'a>
        = LockDepGuard<'a, T>
    where
        T: 'a,
        L: 'a;
    type Context = <DynamicLockDepMutex<T> as MutexLike>::Context;

    #[inline(always)]
    fn context() -> Self::Context {
        DynamicLockDepMutex::<T>::context()
    }

    #[inline(always)]
    fn lock(&self, context: &mut Self::Context) -> Self::Guard<'_> {
        MutexLike::lock(&self.inner, context)
    }
}

impl<T, L: crate::LockLevel> From<T> for LockDepMutex<T, L> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

impl<T: Default, L: crate::LockLevel> Default for LockDepMutex<T, L> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

pub struct MappedLockDepGuard<'a, T: ?Sized> {
    inner: MappedMutexGuard<'a, T>,
    _token: tracking::LockLevelToken,
}

impl<'a, T: ?Sized> std::ops::Deref for MappedLockDepGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<'a, T: ?Sized> std::ops::DerefMut for MappedLockDepGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<'a, T> LockDepGuard<'a, T> {
    pub fn map<U: ?Sized, F>(guard: Self, f: F) -> MappedLockDepGuard<'a, U>
    where
        F: FnOnce(&mut T) -> &mut U,
    {
        let token = guard.token;
        let inner = MutexGuard::map(guard.inner, f);
        MappedLockDepGuard { inner, _token: token }
    }
}

/// An RwLock that dynamically enforces lock ordering at runtime, without using types for levels.
pub struct DynamicLockDepRwLock<T> {
    inner: fuchsia_sync::RwLock<T>,
    tracking: tracking::DynamicLockTracking,
}

impl<T: std::fmt::Debug> std::fmt::Debug for DynamicLockDepRwLock<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DynamicLockDepRwLock({:?})", self.inner)
    }
}

impl<T> DynamicLockDepRwLock<T> {
    pub const fn new<L: crate::LockLevel>(value: T) -> Self {
        Self {
            inner: fuchsia_sync::RwLock::new(value),
            tracking: tracking::DynamicLockTracking::new(L::LOCK_ID, L::NAME),
        }
    }

    #[inline(always)]
    pub fn read(&self) -> LockDepReadGuard<'_, T> {
        let token = tracking::LockLevelToken::new(self.tracking.lock_id(), self.tracking.name());
        LockDepReadGuard { inner: self.inner.read(), token }
    }

    #[inline(always)]
    pub fn write(&self) -> LockDepWriteGuard<'_, T> {
        let token = tracking::LockLevelToken::new(self.tracking.lock_id(), self.tracking.name());
        LockDepWriteGuard { inner: self.inner.write(), token }
    }

    /// Returns a mutable reference to the underlying data.
    ///
    /// Since this call borrows the `DynamicLockDepRwLock` mutably, no actual locking takes place -- the
    /// borrow checker statically ensures no other threads have access to the `DynamicLockDepRwLock`.
    pub fn get_mut(&mut self) -> &mut T {
        self.inner.get_mut()
    }

    /// Consumes the `DynamicLockDepRwLock`, returning the underlying data.
    pub fn into_inner(self) -> T {
        self.inner.into_inner()
    }
}

impl<T> RwLockLike for DynamicLockDepRwLock<T> {
    type ReadGuard<'a>
        = LockDepReadGuard<'a, T>
    where
        T: 'a;
    type WriteGuard<'a>
        = LockDepWriteGuard<'a, T>
    where
        T: 'a;
    type Context = tracking::LockDepContext;

    #[inline(always)]
    fn context() -> Self::Context {
        Default::default()
    }

    #[inline(always)]
    fn read(&self, context: &mut Self::Context) -> Self::ReadGuard<'_> {
        tracking::read_with_context(self, context)
    }

    #[inline(always)]
    fn write(&self, context: &mut Self::Context) -> Self::WriteGuard<'_> {
        tracking::write_with_context(self, context)
    }
}

pub struct LockDepReadGuard<'a, T> {
    inner: RwLockReadGuard<'a, T>,
    token: tracking::LockLevelToken,
}

impl<'a, T> std::ops::Deref for LockDepReadGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.inner.deref()
    }
}

pub struct LockDepWriteGuard<'a, T> {
    inner: RwLockWriteGuard<'a, T>,
    token: tracking::LockLevelToken,
}

impl<'a, T> std::ops::Deref for LockDepWriteGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.inner.deref()
    }
}

impl<'a, T> std::ops::DerefMut for LockDepWriteGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.inner.deref_mut()
    }
}

impl<'a, T> LockDepWriteGuard<'a, T> {
    pub fn downgrade(guard: Self) -> LockDepReadGuard<'a, T> {
        let token = guard.token;
        let inner = RwLockWriteGuard::downgrade(guard.inner);
        LockDepReadGuard { inner, token }
    }
}

/// An RwLock that dynamically enforces lock ordering at runtime using types for levels.
pub struct LockDepRwLock<T, L> {
    inner: DynamicLockDepRwLock<T>,
    _level: PhantomData<L>,
}

impl<T: std::fmt::Debug, L> std::fmt::Debug for LockDepRwLock<T, L> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LockDepRwLock({:?}, {})", self.inner.inner, std::any::type_name::<L>())
    }
}

impl<T, L: crate::LockLevel> LockDepRwLock<T, L> {
    pub const fn new(value: T) -> Self {
        Self { inner: DynamicLockDepRwLock::new::<L>(value), _level: PhantomData }
    }

    /// Returns a mutable reference to the underlying data.
    ///
    /// Since this call borrows the `LockDepRwLock` mutably, no actual locking takes place -- the
    /// borrow checker statically ensures no other threads have access to the `LockDepRwLock`.
    pub fn get_mut(&mut self) -> &mut T {
        self.inner.get_mut()
    }

    /// Consumes the `LockDepRwLock`, returning the underlying data.
    pub fn into_inner(self) -> T {
        self.inner.into_inner()
    }

    #[inline(always)]
    pub fn read(&self) -> LockDepReadGuard<'_, T> {
        self.inner.read()
    }

    #[inline(always)]
    pub fn write(&self) -> LockDepWriteGuard<'_, T> {
        self.inner.write()
    }
}

impl<T, L> RwLockLike for LockDepRwLock<T, L> {
    type ReadGuard<'a>
        = LockDepReadGuard<'a, T>
    where
        T: 'a,
        L: 'a;
    type WriteGuard<'a>
        = LockDepWriteGuard<'a, T>
    where
        T: 'a,
        L: 'a;
    type Context = <DynamicLockDepRwLock<T> as RwLockLike>::Context;

    #[inline(always)]
    fn context() -> Self::Context {
        DynamicLockDepRwLock::<T>::context()
    }

    #[inline(always)]
    fn read(&self, context: &mut Self::Context) -> Self::ReadGuard<'_> {
        RwLockLike::read(&self.inner, context)
    }

    #[inline(always)]
    fn write(&self, context: &mut Self::Context) -> Self::WriteGuard<'_> {
        RwLockLike::write(&self.inner, context)
    }
}

impl<T: Default, L: crate::LockLevel> Default for LockDepRwLock<T, L> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T, L: crate::LockLevel> From<T> for LockDepRwLock<T, L> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

pub struct MappedLockDepReadGuard<'a, T: ?Sized> {
    inner: MappedRwLockReadGuard<'a, T>,
    _token: tracking::LockLevelToken,
}

impl<'a, T: ?Sized> std::ops::Deref for MappedLockDepReadGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

pub struct MappedLockDepWriteGuard<'a, T: ?Sized> {
    inner: MappedRwLockWriteGuard<'a, T>,
    _token: tracking::LockLevelToken,
}

impl<'a, T: ?Sized> std::ops::Deref for MappedLockDepWriteGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<'a, T: ?Sized> std::ops::DerefMut for MappedLockDepWriteGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<'a, T> LockDepReadGuard<'a, T> {
    pub fn map<U: ?Sized, F>(guard: Self, f: F) -> MappedLockDepReadGuard<'a, U>
    where
        F: FnOnce(&T) -> &U,
    {
        let token = guard.token;
        let inner = RwLockReadGuard::map(guard.inner, f);
        MappedLockDepReadGuard { inner, _token: token }
    }
}

impl<'a, T> LockDepWriteGuard<'a, T> {
    pub fn map<U: ?Sized, F>(guard: Self, f: F) -> MappedLockDepWriteGuard<'a, U>
    where
        F: FnOnce(&mut T) -> &mut U,
    {
        let token = guard.token;
        let inner = RwLockWriteGuard::map(guard.inner, f);
        MappedLockDepWriteGuard { inner, _token: token }
    }
}

/// A token that allows the next lock acquisition of the same level as the currently maximal
/// held lock to use an incremented subclass.
/// Allows subclassing of the currently maximal held lock.
pub fn allow_subclass() -> tracking::SubclassToken {
    tracking::SubclassToken::new()
}

/// Asserts that the current thread can acquire locks at level `L`.
/// Returns a token that, when held, forces subsequent locks to be after `L`.
pub fn assert_lock_level<L: crate::LockLevel>() -> tracking::LockLevelToken {
    tracking::LockLevelToken::new(L::LOCK_ID, L::NAME)
}

#[cfg(test)]
#[cfg(feature = "detect_lock_dep_cycles")]
mod tests {
    use super::*;
    use crate::{Unlocked, lock_ordering, ordered_lock, ordered_lock_vec};

    lock_ordering! {
        Unlocked => LevelA,
        LevelA => LevelB,
    }

    #[test]
    fn test_valid_lock_ordering() {
        tracking::clear_state();
        let lock_a: LockDepMutex<i32, LevelA> = LockDepMutex::new(0);
        let lock_b: LockDepMutex<i32, LevelB> = LockDepMutex::new(0);

        let _guard_a = lock_a.lock();
        let _guard_b = lock_b.lock();
    }

    #[test]
    fn test_subclass_no_lock() {
        tracking::clear_state();
        let _token1 = allow_subclass();
    }

    #[test]
    fn test_valid_lock_subclass_ordering() {
        tracking::clear_state();
        let lock_a1: LockDepMutex<i32, LevelA> = LockDepMutex::new(0);
        let lock_a2: LockDepMutex<i32, LevelA> = LockDepMutex::new(0);
        let lock_a3: LockDepMutex<i32, LevelA> = LockDepMutex::new(0);

        let _guard_a1 = lock_a1.lock();
        let _token1 = allow_subclass();
        let _guard_a2 = lock_a2.lock();
        let _token2 = allow_subclass();
        let _guard_a3 = lock_a3.lock();
    }

    #[test]
    fn test_raii_subclass_guard() {
        tracking::clear_state();
        {
            let lock_a1: LockDepMutex<i32, LevelA> = LockDepMutex::new(0);
            let lock_a2: LockDepMutex<i32, LevelA> = LockDepMutex::new(0);

            let _guard_a1 = lock_a1.lock();
            let _token = allow_subclass();
            let _guard_a2 = lock_a2.lock(); // Should succeed with subclass 1
        }
    }

    #[test]
    fn test_subclass_guard_dropped_and_reacquired() {
        tracking::clear_state();
        {
            let lock_a1: LockDepMutex<i32, LevelA> = LockDepMutex::new(0);
            let lock_a2: LockDepMutex<i32, LevelA> = LockDepMutex::new(0);

            let _guard_a1 = lock_a1.lock();
            let _token1 = allow_subclass();
            for _ in 0..2 {
                let _guard_a2 = lock_a2.lock(); // Should succeed with subclass 1
            }
        }
    }

    #[test]
    fn test_multiple_subclass_same_level() {
        tracking::clear_state();
        let lock_a1: LockDepMutex<i32, LevelA> = LockDepMutex::new(0);
        let lock_a2: LockDepMutex<i32, LevelA> = LockDepMutex::new(0);

        let _guard_a1 = lock_a1.lock();
        let _token1 = allow_subclass();
        for _ in 0..2 {
            let _token2 = allow_subclass();
            let _guard_a2 = lock_a2.lock();
        }
    }

    #[test]
    #[should_panic(expected = "Subclassing not allowed or already consumed")]
    fn test_raii_subclass_guard_limit() {
        tracking::clear_state();
        {
            let lock_a1: LockDepMutex<i32, LevelA> = LockDepMutex::new(0);
            let lock_a2: LockDepMutex<i32, LevelA> = LockDepMutex::new(0);
            let lock_a3: LockDepMutex<i32, LevelA> = LockDepMutex::new(0);

            let _guard_a1 = lock_a1.lock();
            let _token = allow_subclass();
            let _guard_a2 = lock_a2.lock();

            let _guard_a3 = lock_a3.lock();
        }
    }

    #[test]
    fn test_raii_subclass_guard_multiple() {
        tracking::clear_state();
        {
            let lock_a1: LockDepMutex<i32, LevelA> = LockDepMutex::new(0);
            let lock_a2: LockDepMutex<i32, LevelA> = LockDepMutex::new(0);
            let lock_a3: LockDepMutex<i32, LevelA> = LockDepMutex::new(0);

            let _guard_a1 = lock_a1.lock();
            let _token1 = allow_subclass();
            let _guard_a2 = lock_a2.lock();

            let _token2 = allow_subclass();
            let _guard_a3 = lock_a3.lock(); // Should succeed with subclass 2
        }
    }

    #[test]
    #[should_panic(expected = "Invalid lock ordering cycle detected")]
    fn test_invalid_lock_ordering_cycle() {
        tracking::clear_state();
        {
            let lock_a: LockDepMutex<i32, LevelA> = LockDepMutex::new(0);
            let lock_b: LockDepMutex<i32, LevelB> = LockDepMutex::new(0);

            let _guard_b = lock_b.lock();
            let _guard_a = lock_a.lock(); // Should panic because B > A
        }
    }

    #[test]
    #[should_panic(expected = "LockDep: Self-deadlock detected")]
    fn test_self_deadlock() {
        tracking::clear_state();
        {
            let lock_a: LockDepMutex<i32, LevelA> = LockDepMutex::new(0);

            let _guard_a1 = lock_a.lock();
            let _guard_a2 = lock_a.lock();
        }
    }

    #[test]
    fn test_subclass_drop_out_of_order() {
        tracking::clear_state();
        let lock_a1: LockDepMutex<i32, LevelA> = LockDepMutex::new(0);
        let lock_a2: LockDepMutex<i32, LevelA> = LockDepMutex::new(0);
        let lock_a3: LockDepMutex<i32, LevelA> = LockDepMutex::new(0);

        let _guard_a1 = lock_a1.lock();
        let _token1 = allow_subclass();
        let _guard_a2 = lock_a2.lock();
        let _token2 = allow_subclass();
        let _guard_a3 = lock_a3.lock();
        std::mem::drop(_token2);
        std::mem::drop(_guard_a2);
        std::mem::drop(_guard_a3);
        let _guard_a2 = lock_a2.lock();
    }

    #[test]
    #[should_panic(expected = "LockDep: Attempted to drop a lock with active subclass tokens!")]
    fn test_drop_lock_with_active_tokens() {
        tracking::clear_state();
        let lock_a: LockDepMutex<i32, LevelA> = LockDepMutex::new(0);
        let guard = lock_a.lock();
        let _token = allow_subclass();
        std::mem::drop(guard);
    }

    #[test]
    #[should_panic(
        expected = "Invalid lock ordering cycle detected: attempted to acquire 'LevelA' after 'LevelB'"
    )]
    fn test_panic_message_contains_names() {
        tracking::clear_state();
        let lock_a: LockDepMutex<i32, LevelA> = LockDepMutex::new(0);
        let lock_b: LockDepMutex<i32, LevelB> = LockDepMutex::new(0);

        let _guard_b = lock_b.lock();
        let _guard_a = lock_a.lock();
    }

    #[test]
    #[should_panic(expected = "Invalid lock ordering cycle detected")]
    fn test_assert_lock_level_panic() {
        tracking::clear_state();
        let lock_b: LockDepMutex<i32, LevelB> = LockDepMutex::new(0);

        let _guard_b = lock_b.lock();
        // LevelA is before LevelB in the ordering.
        // So asserting LevelA after holding LevelB should panic!
        let _token = assert_lock_level::<LevelA>();
    }

    #[test]
    fn test_ordered_lock() {
        tracking::clear_state();
        let lock1: LockDepMutex<i32, LevelA> = LockDepMutex::new(1);
        let lock2: LockDepMutex<i32, LevelA> = LockDepMutex::new(2);

        {
            let (g1, g2) = ordered_lock(&lock1, &lock2);
            assert_eq!(*g1, 1);
            assert_eq!(*g2, 2);
        }

        {
            let (g2, g1) = ordered_lock(&lock2, &lock1);
            assert_eq!(*g1, 1);
            assert_eq!(*g2, 2);
        }
    }

    #[test]
    fn test_ordered_lock_vec() {
        tracking::clear_state();
        let l0: LockDepMutex<i32, LevelA> = LockDepMutex::new(0);
        let l1: LockDepMutex<i32, LevelA> = LockDepMutex::new(1);
        let l2: LockDepMutex<i32, LevelA> = LockDepMutex::new(2);

        {
            let guards = ordered_lock_vec(&[&l0, &l1, &l2]);
            assert_eq!(*guards[0], 0);
            assert_eq!(*guards[1], 1);
            assert_eq!(*guards[2], 2);
        }

        {
            let guards = ordered_lock_vec(&[&l2, &l1, &l0]);
            assert_eq!(*guards[0], 2);
            assert_eq!(*guards[1], 1);
            assert_eq!(*guards[2], 0);
        }
    }

    #[test]
    fn test_ordered_lock_vec_many_locks() {
        tracking::clear_state();
        let locks: Vec<LockDepMutex<i32, LevelA>> = (0..20).map(|i| LockDepMutex::new(i)).collect();
        let lock_refs: Vec<&LockDepMutex<i32, LevelA>> = locks.iter().collect();

        let guards = ordered_lock_vec(&lock_refs);
        assert_eq!(guards.len(), 20);
        for i in 0..20 {
            assert_eq!(*guards[i], i as i32);
        }
    }

    #[test]
    fn test_dynamic_lockdep_success() {
        tracking::clear_state();
        let l1 = DynamicLockDepMutex::new::<LevelA>(1);
        let l2 = DynamicLockDepMutex::new::<LevelB>(2);
        let _g1 = l1.lock();
        let _g2 = l2.lock();
    }

    #[test]
    #[should_panic(expected = "Invalid lock ordering cycle detected")]
    fn test_dynamic_lockdep_failure() {
        tracking::clear_state();
        let l1 = DynamicLockDepMutex::new::<LevelA>(1);
        let l2 = DynamicLockDepMutex::new::<LevelB>(2);
        let _g2 = l2.lock();
        let _g1 = l1.lock();
    }

    #[test]
    fn test_dynamic_lockdep_subclass() {
        tracking::clear_state();
        let l1 = DynamicLockDepMutex::new::<LevelA>(1);
        let l2 = DynamicLockDepMutex::new::<LevelA>(2);
        let _g1 = l1.lock();
        let _subclass = tracking::SubclassToken::new();
        let _g2 = l2.lock();
    }
}
