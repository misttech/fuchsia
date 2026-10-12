// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::marker::PhantomPinned;
use core::mem::offset_of;
use core::ptr::NonNull;
use core::sync::atomic::Ordering;

use crate::chainlock_transaction::{
    AtomicToken, ChainLockTransactionToken, ContentionState, Token, Transaction,
};

// # General Chain Lock Facility
//
// Chain locks are a type of spin lock designed for use cases where multiple CPUs need to acquire
// various sets of locks in different orders, such as in dynamic object graphs where nodes have
// lock-synchronized states and references to other nodes. Chain locks provide an arbitration
// mechanism that is particularly useful in operations that require locking multiple nodes
// simultaneously, such as propagating state changes or altering the graph's connectivity.
// Operations initiated from different nodes in the graph may require locks on some of the same
// nodes, but in different orders. If ordinary spin locks were used, such lock order variations
// could lead to deadlocks.
//
// Similar to other spin locks, chain locks use atomic variables for lock state and memory
// ordering, and require disabling interrupts to guarantee IRQ-safe critical sections. However,
// chain locks differ from spin locks in that they don't use CPU IDs or other static values to
// indicate the locked state and identify the owner. Instead, the locked value is derived from a
// global generation counter that provides a priority invariant used in arbitration. An operation
// that requires one or more chain locks is performed in a transaction that carries a unique
// generation count. Within a transaction, a CPU can attempt to lock multiple chain locks using the
// same generation count, giving rise to the name "chain lock."
//
// The generation count provides a fair arbitration mechanism when multiple CPUs attempt to acquire
// the same locks in potentially different orders. During arbitration, the transaction with the
// lowest generation count has the highest priority. If a CPU encounters contention and has a
// transaction with a higher priority than the lock's current owner, it simply spins until the lock
// is released. However, if the transaction has a lower priority, the CPU must back off by
// releasing all held chain locks and restarting the locking sequence. This is because the CPU with
// the higher priority transaction may be waiting to acquire one or more of these locks. Once a
// transaction is complete its generation count is retired and never reused, ensuring that all
// transactions eventually make progress, even under heavy contention.
//

/// Result of attempting to acquire a [`ChainLock`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Result {
    /// Lock successfully acquired.
    Ok,

    /// Lock is held by a lower priority transaction. Returned only by internal try-acquire logic.
    /// Other acquire methods spin internally and do not return this result.
    Retry,

    /// Lock is held by a higher priority transaction, requiring the current transaction to follow
    /// the back off protocol.
    Backoff,

    /// Lock is already held by the current transaction and would otherwise deadlock, requiring the
    /// current transaction to follow the cycle detected protocol. May be fatal in some
    /// environments.
    Cycle,
}

/// Specifies whether [`ChainLock::try_acquire_with_options`] is allowed when the active
/// transaction has already been finalized.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AllowFinalized {
    /// Disallow acquiring the lock if the active transaction is already finalized.
    No = 0,
    /// Allow acquiring the lock even if the active transaction is already finalized.
    Yes = 1,
}

/// Specifies whether [`ChainLock::try_acquire_with_options`] should record contention backoff
/// state if acquisition fails due to a higher-priority transaction holding the lock.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordBackoff {
    /// Do not record backoff state on contention.
    No = 0,
    /// Record backoff state on contention with a higher-priority transaction.
    Yes = 1,
}

#[derive(Clone, Copy)]
struct AttemptResult {
    lock_result: Result,
    observed_token: Token,
}

/// Environment-independent chain lock interface and protocol.
#[repr(C)]
pub struct ChainLock<T: Transaction> {
    state: AtomicToken,
    contention_state: T::ContentionState,
    _pin: PhantomPinned,
}

impl<T: Transaction> ChainLock<T> {
    const UNLOCKED_TOKEN: Token = Token::UNLOCKED;
    const MAX_TOKEN: Token = Token::MAX;

    /// Creates a new unlocked `ChainLock`.
    #[inline]
    pub const fn new() -> Self {
        Self {
            state: AtomicToken::new(Self::UNLOCKED_TOKEN),
            contention_state: T::ContentionState::INIT,
            _pin: PhantomPinned,
        }
    }

    /// Acquires the lock without backoff, spinning until acquired. This method may be used for the
    /// first chain lock acquired in a transaction as an optimization, since backoff is unnecessary
    /// when no other chain locks are held.
    ///
    /// Asserts if a cycle is detected or if any chain locks are currently held.
    #[inline]
    pub fn acquire_first_in_chain(&self, token: &ChainLockTransactionToken<T>) {
        let transaction = T::active_ref(token);
        transaction.assert_not_finalized();
        transaction.assert_num_locks_held(0);

        let tx_token = transaction.token();
        loop {
            let result = self.attempt_acquire_common(tx_token);
            if result.lock_result == Result::Ok {
                transaction.on_acquire(token);
                return;
            }

            transaction.on_contention(token);

            debug_assert_ne!(result.lock_result, Result::Cycle);
            T::yield_cpu();
        }
    }

    /// Acquires the lock, spinning until acquired or returning failure because the caller should
    /// back off due to contention with a higher priority transaction. Returns `true` if the lock
    /// was acquired or `false` if the current transaction should back off. Asserts if a cycle is
    /// detected.
    #[inline]
    #[must_use]
    pub fn acquire_or_backoff(&self, token: &ChainLockTransactionToken<T>) -> bool {
        let transaction = T::active_ref(token);
        transaction.assert_not_finalized();

        let tx_token = transaction.token();
        loop {
            let result = self.attempt_acquire_common(tx_token);
            if result.lock_result == Result::Ok {
                transaction.on_acquire(token);
                return true;
            }

            transaction.on_contention(token);

            // If recording the backoff fails, the transaction owning the lock changed, which could
            // be either higher or lower priority than the current transaction. In that case, retry
            // the acquisition instead of returning false and unwinding the transaction.
            if result.lock_result == Result::Backoff
                && transaction.record_backoff_or_retry(
                    result.observed_token,
                    &self.state,
                    &self.contention_state,
                    token,
                )
            {
                return false;
            }

            debug_assert_ne!(result.lock_result, Result::Cycle);
            T::yield_cpu();
        }
    }

    /// Acquires the lock, spinning until acquired or returning `Err` with [`Result::Backoff`] or
    /// [`Result::Cycle`]. Returns `Ok(())` if the lock was acquired or `Err(result)` if acquiring
    /// the lock failed.
    #[inline]
    pub fn acquire_or_result(
        &self,
        token: &ChainLockTransactionToken<T>,
    ) -> core::result::Result<(), Result> {
        let transaction = T::active_ref(token);
        transaction.assert_not_finalized();

        let tx_token = transaction.token();
        loop {
            let mut result = self.attempt_acquire_common(tx_token);
            if result.lock_result == Result::Ok {
                transaction.on_acquire(token);
                return Ok(());
            }

            transaction.on_contention(token);

            // If recording the backoff fails, the transaction owning the lock changed, which could
            // be either higher or lower priority than the current transaction. In that case, retry
            // the acquisition instead of failing and returning the result.
            if result.lock_result == Result::Backoff
                && !transaction.record_backoff_or_retry(
                    result.observed_token,
                    &self.state,
                    &self.contention_state,
                    token,
                )
            {
                result.lock_result = Result::Retry;
            }

            if result.lock_result != Result::Retry {
                return Err(result.lock_result);
            }

            T::yield_cpu();
        }
    }

    /// Attempts to acquire the lock with default options (`AllowFinalized::No`,
    /// `RecordBackoff::Yes`). Returns `true` if the lock was acquired or `false` if acquiring the
    /// lock failed. Asserts if a cycle is detected.
    #[inline]
    #[must_use]
    pub fn try_acquire(&self, token: &ChainLockTransactionToken<T>) -> bool {
        self.try_acquire_with_options(AllowFinalized::No, RecordBackoff::Yes, token)
    }

    /// Attempts to acquire the lock with the specified `allow_finalized` and `record_backoff`
    /// options. Returns `true` if the lock was acquired or `false` if acquiring the lock failed.
    /// Asserts if a cycle is detected.
    #[inline]
    #[must_use]
    pub fn try_acquire_with_options(
        &self,
        allow_finalized: AllowFinalized,
        record_backoff: RecordBackoff,
        token: &ChainLockTransactionToken<T>,
    ) -> bool {
        let transaction = T::active_ref(token);
        if allow_finalized == AllowFinalized::No {
            transaction.assert_not_finalized();
        }

        let tx_token = transaction.token();
        let result = self.attempt_acquire_common(tx_token);
        if result.lock_result == Result::Ok {
            transaction.on_acquire(token);
            return true;
        }

        transaction.on_contention(token);

        if result.lock_result == Result::Backoff && record_backoff == RecordBackoff::Yes {
            let _ = transaction.record_backoff_or_retry(
                result.observed_token,
                &self.state,
                &self.contention_state,
                token,
            );
        }

        debug_assert_ne!(result.lock_result, Result::Cycle);
        false
    }

    /// Releases the lock.
    #[inline]
    pub fn release(&self, token: &ChainLockTransactionToken<T>) {
        let transaction = T::active_ref(token);
        debug_assert_eq!(transaction.token(), self.state.load(Ordering::Relaxed));

        transaction.on_release(token);

        // The environment-specific contention backoff strategy may require a careful order of
        // operations for notifying backed-off contenders of the release and the actual lock
        // release operation to ensure that BOTH of the following properties hold:
        //
        // 1. Races between backing off and releasing the lock do not result in missed
        //    notifications.
        // 2. The contention state of the lock is not accessed after the lock is released to avoid
        //    use-after-free hazards.
        //
        // The following protocol ensures these properties hold:
        //
        // 1. The lock token is set to the maximum token value to temporarily prevent further
        //    updates to the contention state by new contenders, since the maximum token value is
        //    the lowest possible priority for a chain lock transaction.
        // 2. The lock contention state is sampled to determine which backed-off contenders to
        //    notify after the lock is released.
        // 3. The lock is released, allowing contenders to attempt to acquire the lock.
        // 4. Contenders that backed off prior to the lock release are notified.
        //
        // The environment-specific implementation of `release_and_notify` is responsible for
        // ensuring that steps 2 through 4 are implemented correctly, using the provided closure to
        // implement step 3.
        //
        // TODO(johngro): Look into relaxing the memory ordering being used here.  I
        // _think_ that we can perform this store to the lock's state using relaxed
        // semantics, but I'm not going to take that chance without having first built
        // a model and run it through CDSChecker.
        //
        self.state.store(Self::MAX_TOKEN, Ordering::Release);
        transaction.release_and_notify(
            &self.contention_state,
            || {
                self.state.store(Self::UNLOCKED_TOKEN, Ordering::Release);
            },
            token,
        );
    }

    /// Asserts in debug builds that the lock is held by the current transaction.
    #[inline]
    pub fn assert_held(&self, token: &ChainLockTransactionToken<T>) {
        let transaction = T::active_ref(token);
        let tx_token = transaction.token();
        let lock_token = self.state.load(Ordering::Relaxed);
        debug_assert_eq!(
            lock_token,
            tx_token,
            "Failed to assert ChainLock is held (lock token {:#x}, our token {:#x}).",
            lock_token.value(),
            tx_token.value()
        );
    }

    /// Asserts in debug builds that the lock is held by the current transaction and must be
    /// released by the caller's scope.
    #[inline]
    pub fn assert_acquired(&self, token: &ChainLockTransactionToken<T>) {
        self.assert_held(token);
    }

    /// Returns `true` if the lock is held by the current transaction.
    #[inline]
    pub fn is_held(&self, token: &ChainLockTransactionToken<T>) -> bool {
        let transaction = T::active_ref(token);
        let tx_token = transaction.token();
        self.state.load(Ordering::Relaxed) == tx_token
    }

    /// Returns `true` if the lock is unlocked.
    #[inline]
    pub fn is_unlocked(&self) -> bool {
        self.state.load(Ordering::Relaxed) == Self::UNLOCKED_TOKEN
    }

    /// Syncs the held token value of this lock with the current transaction token. Used for saving
    /// and restoring state in special circumstances that use reserved tokens.
    #[inline]
    pub fn sync_token(&self, token: &ChainLockTransactionToken<T>) {
        let transaction = T::active_ref(token);
        let tx_token = transaction.token();
        self.state.store(tx_token, Ordering::Release);
    }

    /// Returns the currently stored [`Token`] of this lock with `Relaxed` memory ordering for
    /// testing.
    #[inline]
    pub fn state_for_testing(&self) -> Token {
        self.state.load(Ordering::Relaxed)
    }

    #[inline]
    fn attempt_acquire_common(&self, token: Token) -> AttemptResult {
        match self.state.compare_exchange(
            Self::UNLOCKED_TOKEN,
            token,
            Ordering::Acquire,
            Ordering::Acquire,
        ) {
            Ok(expected) => AttemptResult { lock_result: Result::Ok, observed_token: expected },
            Err(expected) => {
                let lock_result = if expected > token {
                    Result::Retry
                } else if expected < token {
                    Result::Backoff
                } else {
                    Result::Cycle
                };
                AttemptResult { lock_result, observed_token: expected }
            }
        }
    }
}

impl<T: Transaction> Default for ChainLock<T> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Transaction> Drop for ChainLock<T> {
    #[inline]
    fn drop(&mut self) {
        let lock_token = self.state.load(Ordering::Relaxed);
        debug_assert_eq!(
            lock_token,
            Self::UNLOCKED_TOKEN,
            "Destroying locked ChainLock: token={}",
            lock_token.value()
        );
    }
}

/// Utility to simplify expressions where two chain locks need to be acquired with backoff.
/// Returns `true` if both locks were acquired or `false` if the current transaction needs to
/// back off.
#[inline]
#[must_use]
pub fn acquire_both_or_backoff<T: Transaction>(
    first: &ChainLock<T>,
    second: &ChainLock<T>,
    token: &ChainLockTransactionToken<T>,
) -> bool {
    if first.acquire_or_backoff(token) {
        if second.acquire_or_backoff(token) {
            return true;
        }
        first.release(token);
    }
    false
}

// Verify layout equivalence with C++ `concurrent::ChainLock<Transaction>` for both 32-bit
// (`AtomicU32`, i.e., `ktl::atomic<cpu_mask_t>` in the kernel) and 64-bit (`AtomicU64`) contention
// states.
struct DummyTxU32;
impl Transaction for DummyTxU32 {
    type ContentionState = core::sync::atomic::AtomicU32;
    fn active() -> Option<NonNull<Self>> {
        None
    }
    fn active_ref(_: &ChainLockTransactionToken<Self>) -> &Self {
        unimplemented!()
    }
    fn yield_cpu() {}
    fn token(&self) -> Token {
        Token::UNLOCKED
    }
    fn assert_not_finalized(&self) {}
    fn assert_num_locks_held(&self, _: u32) {}
    fn on_acquire(&self, _: &ChainLockTransactionToken<Self>) {}
    fn on_release(&self, _: &ChainLockTransactionToken<Self>) {}
    fn on_contention(&self, _: &ChainLockTransactionToken<Self>) {}
    fn record_backoff_or_retry(
        &self,
        _: Token,
        _: &AtomicToken,
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

struct DummyTxU64;
impl Transaction for DummyTxU64 {
    type ContentionState = core::sync::atomic::AtomicU64;
    fn active() -> Option<NonNull<Self>> {
        None
    }
    fn active_ref(_: &ChainLockTransactionToken<Self>) -> &Self {
        unimplemented!()
    }
    fn yield_cpu() {}
    fn token(&self) -> Token {
        Token::UNLOCKED
    }
    fn assert_not_finalized(&self) {}
    fn assert_num_locks_held(&self, _: u32) {}
    fn on_acquire(&self, _: &ChainLockTransactionToken<Self>) {}
    fn on_release(&self, _: &ChainLockTransactionToken<Self>) {}
    fn on_contention(&self, _: &ChainLockTransactionToken<Self>) {}
    fn record_backoff_or_retry(
        &self,
        _: Token,
        _: &AtomicToken,
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

zr::static_assert!(size_of::<ChainLock<DummyTxU32>>() == 16);
zr::static_assert!(align_of::<ChainLock<DummyTxU32>>() == 8);
zr::static_assert!(offset_of!(ChainLock<DummyTxU32>, state) == 0);
zr::static_assert!(offset_of!(ChainLock<DummyTxU32>, contention_state) == 8);

zr::static_assert!(size_of::<ChainLock<DummyTxU64>>() == 16);
zr::static_assert!(align_of::<ChainLock<DummyTxU64>>() == 8);
zr::static_assert!(offset_of!(ChainLock<DummyTxU64>, state) == 0);
zr::static_assert!(offset_of!(ChainLock<DummyTxU64>, contention_state) == 8);

#[cfg(test)]
mod tests {
    use core::cell::Cell;
    use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::boxed::Box;
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::chainlock_transaction::{
        Action, ChainLockTransaction as BaseChainLockTransaction, DONE, Option as TxOption,
        SelectAccounting, SelectTracing, StateOption, StateSaver, TransactionHooks,
        TransactionResult,
    };

    const DEBUG_ENABLED: bool = true;
    const TRACING_ENABLED: bool = true;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    struct CallsiteInfo {
        tag: &'static str,
    }

    const fn clt_tag(tag: &'static str) -> CallsiteInfo {
        CallsiteInfo { tag }
    }

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    enum Options {
        #[default]
        None,
        A,
        B,
    }

    const OPTION_NONE: TxOption<Options> = TxOption::new(Options::None);
    const OPTION_A: TxOption<Options> = TxOption::new(Options::A);
    const OPTION_B: TxOption<Options> = TxOption::new(Options::B);

    struct TestHooks;

    type ChainLockTransaction = BaseChainLockTransaction<TestHooks>;
    type ChainLockTransactionToken =
        crate::chainlock_transaction::ChainLockTransactionToken<ChainLockTransaction>;
    type ChainLock = super::ChainLock<ChainLockTransaction>;
    type ChainLockGuard<'a> = crate::chainlock_guard::ChainLockGuard<'a, ChainLockTransaction>;

    static RESERVED_TOKEN_ALLOCATOR: AtomicUsize = AtomicUsize::new(0);

    std::thread_local! {
        static LAST_CONTENTION_START_TICKS: Cell<i64> = const { Cell::new(0) };
        static LAST_CALLSITE_INFO: Cell<CallsiteInfo> = const {
            Cell::new(CallsiteInfo { tag: "" })
        };
        static ACTIVE_OPTION: Cell<Options> = const { Cell::new(Options::None) };
        static ACTIVE_TRANSACTION: Cell<Option<NonNull<ChainLockTransaction>>> = const {
            Cell::new(None)
        };
        static SIMULATED_TICKS: Cell<i64> = const { Cell::new(100) };
    }

    // SAFETY: `ACTIVE_TRANSACTION` is stored in thread-local storage (`std::thread_local!`), and
    // `active()` returns the exact pointer most recently stored by `set_active()` on the current
    // thread.
    unsafe impl TransactionHooks for TestHooks {
        type ContentionState = AtomicU64;
        type CallsiteInfo = CallsiteInfo;
        type Accounting = SelectAccounting<DEBUG_ENABLED>;
        type Tracing = SelectTracing<TRACING_ENABLED, Self::CallsiteInfo>;
        type ExtraState = ();

        fn active() -> Option<NonNull<ChainLockTransaction>> {
            ACTIVE_TRANSACTION.with(Cell::get)
        }

        unsafe fn set_active(transaction: Option<NonNull<ChainLockTransaction>>) {
            ACTIVE_TRANSACTION.with(|cell| cell.set(transaction));
        }

        fn yield_cpu() {}

        fn allocate_reserved_token() -> Token {
            let reserved_index = RESERVED_TOKEN_ALLOCATOR.fetch_add(1, Ordering::Relaxed);
            assert!((reserved_index as u64) < ChainLockTransaction::RESERVED_TOKEN_VALUES);
            ChainLockTransaction::reserved_token(reserved_index)
        }

        fn get_current_ticks() -> i64 {
            SIMULATED_TICKS.with(|cell| {
                let ticks = cell.get();
                cell.set(ticks + 1);
                ticks
            })
        }

        fn on_finalized(contention_start_ticks: i64, callsite_info: Self::CallsiteInfo) {
            LAST_CONTENTION_START_TICKS.with(|cell| cell.set(contention_start_ticks));
            LAST_CALLSITE_INFO.with(|cell| cell.set(callsite_info));
        }

        fn on_backoff(_transaction: &ChainLockTransaction) {}

        fn record_backoff_or_retry(
            _transaction: &ChainLockTransaction,
            _observed_token: Token,
            _lock_state: &AtomicToken,
            _contention_state: &Self::ContentionState,
        ) -> bool {
            true
        }

        fn release_and_notify<F: FnOnce()>(
            _transaction: &ChainLockTransaction,
            _contention_state: &Self::ContentionState,
            do_release: F,
        ) {
            do_release();
        }
    }

    impl TestHooks {
        fn active_option() -> Options {
            ACTIVE_OPTION.with(Cell::get)
        }

        fn last_contention_start_ticks() -> i64 {
            LAST_CONTENTION_START_TICKS.with(Cell::get)
        }

        fn last_callsite_info() -> CallsiteInfo {
            LAST_CALLSITE_INFO.with(Cell::get)
        }
    }

    struct TestStateSaver {
        reset_on_drop: bool,
    }

    impl Drop for TestStateSaver {
        fn drop(&mut self) {
            if self.reset_on_drop {
                ACTIVE_OPTION.with(|cell| cell.set(Options::None));
            }
        }
    }

    impl StateSaver<ChainLockTransaction> for TestStateSaver {
        fn relax(&mut self, _transaction: &ChainLockTransaction) {
            thread::yield_now();
        }
    }

    impl StateOption<ChainLockTransaction> for Options {
        type Saver = TestStateSaver;

        fn save_state(&self) -> Self::Saver {
            match self {
                Options::None => TestStateSaver { reset_on_drop: false },
                Options::A => {
                    ACTIVE_OPTION.with(|cell| cell.set(Options::A));
                    TestStateSaver { reset_on_drop: true }
                }
                Options::B => {
                    ACTIVE_OPTION.with(|cell| cell.set(Options::B));
                    TestStateSaver { reset_on_drop: true }
                }
            }
        }
    }

    struct HelperShared<S> {
        lock: Mutex<S>,
        helper_condition: Condvar,
        test_condition: Condvar,
    }

    struct Helper<S> {
        shared: Arc<HelperShared<S>>,
    }

    impl<S: Copy + PartialEq> Helper<S> {
        fn set_state_and_notify_test(&self, state: S) {
            {
                let mut guard = self.shared.lock.lock().unwrap();
                *guard = state;
            }
            self.shared.test_condition.notify_one();
        }

        fn wait_for_test_state(&self, state: S) {
            let mut guard = self.shared.lock.lock().unwrap();
            while *guard != state {
                guard = self.shared.helper_condition.wait(guard).unwrap();
            }
        }
    }

    struct HelperThread<S> {
        shared: Arc<HelperShared<S>>,
        helper_thread: Option<thread::JoinHandle<()>>,
    }

    impl<S: Copy + PartialEq + Send + 'static> HelperThread<S> {
        fn new<F>(initial: S, callable: F) -> Self
        where
            F: FnOnce(Helper<S>) + Send + 'static,
        {
            let shared = Arc::new(HelperShared {
                lock: Mutex::new(initial),
                helper_condition: Condvar::new(),
                test_condition: Condvar::new(),
            });
            let helper = Helper { shared: Arc::clone(&shared) };
            let handle = thread::spawn(move || callable(helper));
            Self { shared, helper_thread: Some(handle) }
        }

        fn set_state_and_notify_helper(&self, state: S) {
            {
                let mut guard = self.shared.lock.lock().unwrap();
                *guard = state;
            }
            self.shared.helper_condition.notify_one();
        }

        fn wait_for_helper_state(&self, state: S) {
            let mut guard = self.shared.lock.lock().unwrap();
            while *guard != state {
                guard = self.shared.test_condition.wait(guard).unwrap();
            }
        }
    }

    impl<S> Drop for HelperThread<S> {
        fn drop(&mut self) {
            if let Some(handle) = self.helper_thread.take() {
                handle.join().unwrap();
            }
        }
    }

    #[test]
    fn transaction_options() {
        assert_eq!(Options::None, TestHooks::active_option());

        ChainLockTransaction::until_done(
            OPTION_NONE,
            clt_tag("TransactionOptions"),
            |_token| -> TransactionResult<()> {
                assert_eq!(Options::None, TestHooks::active_option());
                DONE.into()
            },
        );

        assert_eq!(Options::None, TestHooks::active_option());

        ChainLockTransaction::until_done(
            OPTION_A,
            clt_tag("TransactionOptions"),
            |_token| -> TransactionResult<()> {
                assert_eq!(Options::A, TestHooks::active_option());
                DONE.into()
            },
        );

        assert_eq!(Options::None, TestHooks::active_option());

        ChainLockTransaction::until_done(
            OPTION_B,
            clt_tag("TransactionOptions"),
            |_token| -> TransactionResult<()> {
                assert_eq!(Options::B, TestHooks::active_option());
                DONE.into()
            },
        );

        assert_eq!(Options::None, TestHooks::active_option());
    }

    #[test]
    fn transaction_return_value() {
        let mut value = Some(Box::new(42));
        let value_pointer: *const i32 = &**value.as_ref().unwrap();

        let return_value: Box<i32> = ChainLockTransaction::until_done(
            OPTION_NONE,
            clt_tag("TransactionOptions"),
            |_token| -> TransactionResult<Box<i32>> {
                TransactionResult::Value(value.take().unwrap())
            },
        );

        assert_eq!(&*return_value as *const i32, value_pointer);
        assert!(value.is_none());
    }

    #[test]
    fn uncontested_acquire() {
        let lock = ChainLock::new();

        ChainLockTransaction::until_done(
            OPTION_NONE,
            clt_tag("UncontestedAcquire"),
            |token| -> TransactionResult<()> {
                assert!(!lock.is_held(token));
                assert!(lock.is_unlocked());

                lock.acquire_first_in_chain(token);

                assert!(lock.is_held(token));
                assert!(!lock.is_unlocked());
                assert_eq!(
                    lock.state_for_testing(),
                    ChainLockTransaction::active_ref(token).token()
                );

                lock.release(token);

                assert!(!lock.is_held(token));
                assert!(lock.is_unlocked());
                DONE.into()
            },
        );
    }

    #[test]
    fn cyclic_acquire() {
        let lock = ChainLock::new();
        let get_lock = || -> &ChainLock { &lock };

        ChainLockTransaction::until_done(
            OPTION_NONE,
            clt_tag("CyclicAcquire"),
            |token| -> TransactionResult<()> {
                lock.acquire_first_in_chain(token);

                assert_eq!(Err(Result::Cycle), get_lock().acquire_or_result(token));

                lock.release(token);
                DONE.into()
            },
        );
    }

    #[test]
    fn contention_priority() {
        let locks = Arc::new((ChainLock::new(), ChainLock::new()));

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum State {
            Initial,
            HelperTransactionStarted,
            TestTransactionStarted,
        }

        let helper_locks = Arc::clone(&locks);
        let helper_thread = HelperThread::new(State::Initial, move |helper| {
            let (lock_a, lock_b) = &*helper_locks;
            ChainLockTransaction::until_done(
                OPTION_NONE,
                clt_tag("ContentionPriority helper"),
                |token| -> TransactionResult<()> {
                    let _guard = ChainLockGuard::new(lock_a, token);

                    helper.set_state_and_notify_test(State::HelperTransactionStarted);
                    helper.wait_for_test_state(State::TestTransactionStarted);

                    // Acquiring lock_b should spin until the lower priority transaction backs off.
                    let acquired = lock_b.acquire_or_backoff(token);
                    if acquired {
                        lock_b.release(token);
                    } else {
                        assert!(acquired);
                    }

                    DONE.into()
                },
            );
        });

        // Wait for the helper thread to start a transaction so it has higher priority.
        helper_thread.wait_for_helper_state(State::HelperTransactionStarted);

        let (lock_a, lock_b) = &*locks;
        ChainLockTransaction::until_done(
            OPTION_NONE,
            clt_tag("ContentionPriority"),
            |token| -> TransactionResult<()> {
                let _guard = ChainLockGuard::new(lock_b, token);

                helper_thread.set_state_and_notify_helper(State::TestTransactionStarted);

                // The helper transaction should be higher priority than this transaction.
                assert!(lock_b.state_for_testing() > lock_a.state_for_testing());

                // Let the helper thread spin for a while waiting for lock_b to be released.
                thread::sleep(Duration::from_millis(50));

                if !lock_a.acquire_or_backoff(token) {
                    return Action::Backoff.into();
                }
                lock_a.release(token);

                DONE.into()
            },
        );

        // Verify contention tracing recorded the contention on the test thread.
        assert!(TestHooks::last_contention_start_ticks() > 0);
        assert_eq!(TestHooks::last_callsite_info(), clt_tag("ContentionPriority"));
    }

    struct TypeA {
        lock: ChainLock,
        value: i32,
    }

    impl TypeA {
        fn new(value: i32) -> Self {
            Self { lock: ChainLock::new(), value }
        }

        fn get_lock(&self) -> &ChainLock {
            &self.lock
        }

        fn compare(&self, other: &TypeB, token: &ChainLockTransactionToken) -> bool {
            self.get_lock().assert_held(token);
            other.get_lock().assert_held(token);
            self.value == other.value
        }
    }

    struct TypeB {
        lock: ChainLock,
        value: i32,
    }

    impl TypeB {
        fn new(value: i32) -> Self {
            Self { lock: ChainLock::new(), value }
        }

        fn get_lock(&self) -> &ChainLock {
            &self.lock
        }

        fn compare(&self, other: &TypeA, token: &ChainLockTransactionToken) -> bool {
            self.get_lock().assert_held(token);
            other.get_lock().assert_held(token);
            self.value == other.value
        }
    }

    #[test]
    fn cross_referencing_types() {
        let value_a = TypeA::new(0);
        let value_b = TypeB::new(0);

        let do_transaction = |token: &ChainLockTransactionToken| -> TransactionResult<()> {
            let _guard_a = ChainLockGuard::new(value_a.get_lock(), token);
            let mut guard_b = ChainLockGuard::defer(value_b.get_lock(), token);
            if !guard_b.acquire_or_backoff() {
                return Action::Backoff.into();
            }
            assert!(value_a.compare(&value_b, token));
            assert!(value_b.compare(&value_a, token));
            DONE.into()
        };

        ChainLockTransaction::until_done(
            OPTION_NONE,
            clt_tag("CrossReferencingTypes"),
            do_transaction,
        );
    }

    #[test]
    fn guard_operations_and_acquire_both() {
        let lock_a = ChainLock::new();
        let lock_b = ChainLock::new();

        ChainLockTransaction::until_done(OPTION_NONE, clt_tag("GuardOps"), |token| {
            assert!(acquire_both_or_backoff(&lock_a, &lock_b, token));
            let guard_a = ChainLockGuard::adopt(&lock_a, token);
            let mut guard_b = ChainLockGuard::adopt(&lock_b, token);
            assert_eq!(ChainLockTransaction::active_ref(token).locks_held(), 2);

            // Transfer guard_a to a new guard via `take` and `from_taken`.
            let mut transferred_a = ChainLockGuard::from_taken(guard_a.take(), &lock_a);
            assert!(transferred_a.is_held());

            // Test `unguard` + manual release on lock_a.
            transferred_a.unguard();
            assert!(!transferred_a.is_held());
            lock_a.release(token);
            assert_eq!(ChainLockTransaction::active_ref(token).locks_held(), 1);

            // Explicitly release guard_b and re-acquire with `try_acquire_with`.
            guard_b.release();
            assert_eq!(ChainLockTransaction::active_ref(token).locks_held(), 0);
            assert!(guard_b.try_acquire_with(|l| l.try_acquire(token)));
            assert_eq!(ChainLockTransaction::active_ref(token).locks_held(), 1);

            DONE
        });
    }

    #[test]
    fn save_and_restore_state_and_restart() {
        let lock = ChainLock::new();
        let mut restarted = false;

        ChainLockTransaction::until_done(
            OPTION_NONE,
            clt_tag("SaveRestoreAndRestart"),
            |token| -> TransactionResult<()> {
                if !restarted {
                    lock.acquire_first_in_chain(token);
                    ChainLockTransaction::finalize(token);
                    assert!(ChainLockTransaction::active_ref(token).is_finalized());

                    let original_token = ChainLockTransaction::active_ref(token).token();
                    let saved = ChainLockTransaction::save_state_and_use_reserved_token(token);
                    assert_eq!(saved.token(), original_token);
                    assert!(ChainLockTransaction::active_ref(token).token().is_reserved());

                    // Sync lock to the reserved token, then restore the original token and sync
                    // back.
                    lock.sync_token(token);
                    assert!(lock.state_for_testing().is_reserved());

                    // SAFETY: `saved` was created from the currently active transaction on this
                    // stack.
                    unsafe {
                        ChainLockTransaction::restore_state(saved);
                    }
                    assert_eq!(ChainLockTransaction::active_ref(token).token(), original_token);
                    lock.sync_token(token);
                    assert_eq!(lock.state_for_testing(), original_token);

                    lock.release(token);
                    restarted = true;
                    return Action::Restart.into();
                }

                assert!(!ChainLockTransaction::active_ref(token).is_finalized());
                DONE.into()
            },
        );
        assert!(restarted);
    }
}
