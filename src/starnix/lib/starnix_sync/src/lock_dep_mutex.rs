// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_sync::{MutexGuard, RwLockReadGuard, RwLockWriteGuard};
use std::marker::PhantomData;

#[cfg(feature = "detect_lock_dep_cycles")]
mod tracking {
    use std::cell::RefCell;

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
    pub fn check_and_push_lock(target_value: usize, name: &'static str) {
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
    pub fn pop_lock(target_value: usize) {
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
    pub fn get_subclass(lock_id: usize) -> u8 {
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
    pub fn enable_subclass_for_maximal() -> usize {
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
    pub fn disable_subclass(encoded_value: usize) {
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
}

#[cfg(not(feature = "detect_lock_dep_cycles"))]
mod tracking {
    #[inline(always)]
    pub fn check_and_push_lock(_target_value: usize, _name: &'static str) {}
    #[inline(always)]
    pub fn pop_lock(_target_value: usize) {}
    #[inline(always)]
    pub fn get_subclass(_lock_id: usize) -> u8 {
        0
    }
    #[inline(always)]
    pub fn enable_subclass_for_maximal() -> usize {
        usize::MAX
    }
    #[inline(always)]
    pub fn disable_subclass(_encoded_value: usize) {}
}

/// A Mutex that dynamically enforces lock ordering at runtime.
pub struct LockDepMutex<T, L> {
    inner: fuchsia_sync::Mutex<T>,
    _level: PhantomData<L>,
}

impl<T, L: crate::LockLevel> LockDepMutex<T, L> {
    pub const fn new(value: T) -> Self {
        Self { inner: fuchsia_sync::Mutex::new(value), _level: PhantomData }
    }

    #[inline(always)]
    pub fn lock(&self) -> LockDepGuard<'_, T, L> {
        let subclass = tracking::get_subclass(L::LOCK_ID);
        assert!(subclass < 16, "subclass must be between 0 and 15");
        let target_value = L::LOCK_ID | (subclass as usize & 0xF);
        tracking::check_and_push_lock(target_value, L::name());
        LockDepGuard { inner: self.inner.lock(), target_value, _level: PhantomData }
    }
}

pub struct LockDepGuard<'a, T, L> {
    inner: MutexGuard<'a, T>,
    target_value: usize,
    _level: PhantomData<L>,
}

impl<'a, T, L> std::ops::Deref for LockDepGuard<'a, T, L> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<'a, T, L> std::ops::DerefMut for LockDepGuard<'a, T, L> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<'a, T, L> Drop for LockDepGuard<'a, T, L> {
    fn drop(&mut self) {
        tracking::pop_lock(self.target_value);
    }
}

/// An RwLock that dynamically enforces lock ordering at runtime.
pub struct LockDepRwLock<T, L> {
    inner: fuchsia_sync::RwLock<T>,
    _level: PhantomData<L>,
}

impl<T, L: crate::LockLevel> LockDepRwLock<T, L> {
    pub const fn new(value: T) -> Self {
        Self { inner: fuchsia_sync::RwLock::new(value), _level: PhantomData }
    }

    #[inline(always)]
    pub fn read(&self) -> LockDepReadGuard<'_, T, L> {
        let subclass = tracking::get_subclass(L::LOCK_ID);
        assert!(subclass < 16, "subclass must be between 0 and 15");
        let target_value = L::LOCK_ID | (subclass as usize & 0xF);
        tracking::check_and_push_lock(target_value, L::name());
        LockDepReadGuard { inner: self.inner.read(), target_value, _level: PhantomData }
    }

    #[inline(always)]
    pub fn write(&self) -> LockDepWriteGuard<'_, T, L> {
        let subclass = tracking::get_subclass(L::LOCK_ID);
        assert!(subclass < 16, "subclass must be between 0 and 15");
        let target_value = L::LOCK_ID | (subclass as usize & 0xF);
        tracking::check_and_push_lock(target_value, L::name());
        LockDepWriteGuard { inner: self.inner.write(), target_value, _level: PhantomData }
    }
}

pub struct LockDepReadGuard<'a, T, L> {
    inner: RwLockReadGuard<'a, T>,
    target_value: usize,
    _level: PhantomData<L>,
}

impl<'a, T, L> std::ops::Deref for LockDepReadGuard<'a, T, L> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<'a, T, L> Drop for LockDepReadGuard<'a, T, L> {
    fn drop(&mut self) {
        tracking::pop_lock(self.target_value);
    }
}

pub struct LockDepWriteGuard<'a, T, L> {
    inner: RwLockWriteGuard<'a, T>,
    target_value: usize,
    _level: PhantomData<L>,
}

impl<'a, T, L> std::ops::Deref for LockDepWriteGuard<'a, T, L> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<'a, T, L> std::ops::DerefMut for LockDepWriteGuard<'a, T, L> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<'a, T, L> Drop for LockDepWriteGuard<'a, T, L> {
    fn drop(&mut self) {
        tracking::pop_lock(self.target_value);
    }
}

/// A token that allows the next lock acquisition of the same level as the currently maximal
/// held lock to use an incremented subclass.
pub struct SubclassToken {
    encoded_value: usize,
}

impl SubclassToken {
    pub fn new() -> Self {
        let encoded_value = tracking::enable_subclass_for_maximal();
        Self { encoded_value }
    }
}

impl Drop for SubclassToken {
    fn drop(&mut self) {
        tracking::disable_subclass(self.encoded_value);
    }
}

/// Allows subclassing of the currently maximal held lock.
pub fn allow_subclass() -> SubclassToken {
    SubclassToken::new()
}

#[cfg(test)]
#[cfg(feature = "detect_lock_dep_cycles")]
mod tests {
    use super::*;
    use crate::{Unlocked, lock_ordering};

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
    #[should_panic(expected = "Invalid lock ordering cycle detected: attempted to acquire 'LevelA' after 'LevelB'")]
    fn test_panic_message_contains_names() {
        tracking::clear_state();
        {
            let lock_a: LockDepMutex<i32, LevelA> = LockDepMutex::new(0);
            let lock_b: LockDepMutex<i32, LevelB> = LockDepMutex::new(0);

            let _guard_b = lock_b.lock();
            let _guard_a = lock_a.lock(); // Should panic
        }
    }
}
