// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::mem::offset_of;
use core::ptr::NonNull;

use crate::chainlock::{AllowFinalized, ChainLock, RecordBackoff};
use crate::chainlock_transaction::{ChainLockTransactionToken, Transaction};

// TODO(eieio): Figure out if this type can be merged with lockdep::Guard to provide some level of
// runtime consistency analysis between chain locks and other lock classes. Chain locks are
// essentially externally ordered locks, where the acquisition of any number of chain locks behaves
// as acquiring a single lock class, having its order dependencies with other lock classes tracked.

/// RAII-style acquire/release guard for [`ChainLock`].
#[repr(C)]
#[must_use = "if unused, the ChainLock will immediately be released"]
pub struct ChainLockGuard<'a, T: Transaction> {
    lock: &'a ChainLock<T>,
    held: bool,
    token: ChainLockTransactionToken<T>,
}

impl<'a, T: Transaction> ChainLockGuard<'a, T> {
    /// Unconditionally acquires the given chain lock as the first lock in the chain.
    #[inline(always)]
    pub fn new(lock: &'a ChainLock<T>, token: &'a ChainLockTransactionToken<T>) -> Self {
        lock.acquire_first_in_chain(token);
        Self { lock, held: true, token: ChainLockTransactionToken::new() }
    }

    /// Adopts the given chain lock that is already acquired via some other mechanism.
    #[inline(always)]
    pub const fn adopt(lock: &'a ChainLock<T>, _token: &'a ChainLockTransactionToken<T>) -> Self {
        Self { lock, held: true, token: ChainLockTransactionToken::new() }
    }

    /// Associates the given lock capability with this scoped guard, but does not acquire it.
    /// This is useful in conjunction with the conditional acquire methods below.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut guard = ChainLockGuard::defer(&lock, token);
    /// if guard.acquire_or_backoff() {
    ///     // Lock is acquired and guarded. It will be released when guard goes out of scope.
    /// } else {
    ///     // Lock is not acquired or guarded. It will not be released when guard goes out of
    ///     // scope.
    /// }
    /// ```
    #[inline(always)]
    pub const fn defer(lock: &'a ChainLock<T>, _token: &'a ChainLockTransactionToken<T>) -> Self {
        Self { lock, held: false, token: ChainLockTransactionToken::new() }
    }

    /// Takes responsibility for `lock` from `other`. This is useful for moving the scope of a
    /// lock acquisition deeper into the call stack in certain locking patterns.
    ///
    /// Asserts in debug builds that `other` actually holds `lock`.
    #[inline(always)]
    pub fn from_taken(mut other: Self, lock: &'a ChainLock<T>) -> Self {
        debug_assert!(other.held);
        debug_assert!(core::ptr::eq(other.lock, lock));
        other.held = false;
        Self { lock, held: true, token: ChainLockTransactionToken::new() }
    }

    /// Releases the capability guarded by this instance without actually releasing the lock,
    /// returning a moved guard intended to be passed to [`Self::from_taken`] or a callee.
    ///
    /// Asserts in debug builds that this guard currently holds the lock.
    #[inline(always)]
    pub fn take(self) -> Self {
        debug_assert!(self.held);
        self
    }

    /// Conditionally acquires the lock or returns `false` because of backoff.
    ///
    /// Asserts in debug builds that the lock is not already held by this guard.
    #[inline(always)]
    #[must_use]
    pub fn acquire_or_backoff(&mut self) -> bool {
        debug_assert!(!self.held);
        self.held = self.lock.acquire_or_backoff(&self.token);
        self.held
    }

    /// Conditionally acquires the lock with default options (`AllowFinalized::No`,
    /// `RecordBackoff::Yes`) or returns `false` if failed.
    ///
    /// Asserts in debug builds that the lock is not already held by this guard.
    #[inline(always)]
    #[must_use]
    pub fn try_acquire(&mut self) -> bool {
        self.try_acquire_with_options(AllowFinalized::No, RecordBackoff::Yes)
    }

    /// Conditionally acquires the lock with the specified options or returns `false` if failed.
    ///
    /// Asserts in debug builds that the lock is not already held by this guard.
    #[inline(always)]
    #[must_use]
    pub fn try_acquire_with_options(
        &mut self,
        allow_finalized: AllowFinalized,
        record_backoff: RecordBackoff,
    ) -> bool {
        debug_assert!(!self.held);
        self.held =
            self.lock.try_acquire_with_options(allow_finalized, record_backoff, &self.token);
        self.held
    }

    /// Conditionally acquires the lock using the given `callable`, or returns `false` if failed.
    /// This is useful in certain locking patterns where an additional predicate needs to be
    /// injected into the lock condition.
    ///
    /// Asserts in debug builds that the lock is not already held by this guard.
    #[inline(always)]
    #[must_use]
    pub fn try_acquire_with<F>(&mut self, callable: F) -> bool
    where
        F: FnOnce(&'a ChainLock<T>) -> bool,
    {
        debug_assert!(!self.held);
        self.held = callable(self.lock);
        self.held
    }

    /// Releases the guarded lock and its capability.
    ///
    /// Asserts in debug builds that the lock is currently held by this guard.
    #[inline(always)]
    pub fn release(&mut self) {
        debug_assert!(self.held);
        self.lock.release(&self.token);
        self.held = false;
    }

    /// Prevents this guard from releasing the lock when it goes out of scope, but does not
    /// release the lock or its capability. This is a temporary workaround to allow guards to be
    /// used in scopes that will release the guarded lock in a function or method called in the
    /// guarded scope.
    ///
    /// # Example
    ///
    /// ```ignore
    /// {
    ///     let mut guard = ChainLockGuard::new(object.get_lock(), token);
    ///
    ///     // Validation with lock held ...
    ///
    ///     guard.unguard(); // Avoid double release when guard destructs.
    ///     do_operation_and_release_lock(object, token);
    /// }
    /// ```
    ///
    /// Possible alternatives to explore that could replace this workaround:
    ///
    /// 1. Move the guard into the callee that will release the lock:
    ///
    /// ```ignore
    /// do_operation_and_release_lock(object, guard.take());
    /// ```
    ///
    /// 2. Make the guard destructor check that the lock is actually held to avoid the double
    ///    release:
    ///
    /// ```ignore
    /// impl<T: Transaction> Drop for ChainLockGuard<'_, T> {
    ///     fn drop(&mut self) {
    ///         if self.held && self.lock.is_held(&self.token) {
    ///             self.lock.release(&self.token);
    ///         }
    ///     }
    /// }
    /// ```
    ///
    /// All of these approaches have potential advantages and disadvantages that need to be
    /// considered.
    ///
    /// Asserts in debug builds that the lock is currently held by this guard.
    #[inline(always)]
    pub fn unguard(&mut self) {
        debug_assert!(self.held);
        self.held = false;
    }

    /// Returns `true` if this guard currently holds the lock.
    #[inline(always)]
    pub const fn is_held(&self) -> bool {
        self.held
    }

    /// Returns a reference to the underlying [`ChainLock`].
    #[inline(always)]
    pub const fn lock(&self) -> &'a ChainLock<T> {
        self.lock
    }
}

impl<T: Transaction> Drop for ChainLockGuard<'_, T> {
    #[inline(always)]
    fn drop(&mut self) {
        if self.held {
            // `release` already asserts in debug builds that `self.lock` is held by the active
            // transaction.
            self.lock.release(&self.token);
        }
    }
}

// Verify layout equivalence with C++ `concurrent::ChainLockGuard<Transaction>`.
// In C++: `ChainLock<Transaction>& lock_` (offset 0, size 8) + `bool held_` (offset 8, size 1) ->
// total size 16, align 8.
struct DummyTx;
impl Transaction for DummyTx {
    type ContentionState = core::sync::atomic::AtomicU32;
    fn active() -> Option<NonNull<Self>> {
        None
    }
    fn active_ref(_: &ChainLockTransactionToken<Self>) -> &Self {
        unimplemented!()
    }
    fn yield_cpu() {}
    fn token(&self) -> crate::Token {
        crate::Token::UNLOCKED
    }
    fn assert_not_finalized(&self) {}
    fn assert_num_locks_held(&self, _: u32) {}
    fn on_acquire(&self, _: &ChainLockTransactionToken<Self>) {}
    fn on_release(&self, _: &ChainLockTransactionToken<Self>) {}
    fn on_contention(&self, _: &ChainLockTransactionToken<Self>) {}
    fn record_backoff_or_retry(
        &self,
        _: crate::Token,
        _: &crate::AtomicToken,
        _: &Self::ContentionState,
        _: &ChainLockTransactionToken<Self>,
    ) -> bool {
        true
    }
    fn release_and_notify<F: FnOnce()>(
        &self,
        _: &Self::ContentionState,
        do_release: F,
        _: &ChainLockTransactionToken<Self>,
    ) {
        do_release();
    }
}

zr::static_assert!(size_of::<ChainLockGuard<'static, DummyTx>>() == 16);
zr::static_assert!(align_of::<ChainLockGuard<'static, DummyTx>>() == 8);
zr::static_assert!(offset_of!(ChainLockGuard<'static, DummyTx>, lock) == 0);
zr::static_assert!(offset_of!(ChainLockGuard<'static, DummyTx>, held) == 8);
