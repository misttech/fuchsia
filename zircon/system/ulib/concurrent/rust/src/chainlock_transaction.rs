// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::cell::Cell;
use core::fmt;
use core::marker::{PhantomData, PhantomPinned};
use core::mem::offset_of;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

//
// # General Chain Lock Transaction Facility
//
// Chain lock transactions are a layer on top of chain locks that track the state necessary for
// entities (i.e. CPUs) to acquire a series of one or more chain locks and correctly handle
// arbitration with other entities contending for the same locks. The base transaction facility
// provides the main hooks required by the general chain lock implementation, and has compile-time
// enabled state for debugging and performance tracing to help diagnose issues with chain lock
// usage.
//

const UNLOCKED_TOKEN_VALUE: u64 = 0;
const RESERVED_TOKEN_VALUES: u64 = 10000;
const INITIAL_TOKEN_VALUE: u64 = RESERVED_TOKEN_VALUES + 1;
const MAX_TOKEN_VALUE: u64 = u64::MAX;

// SAFETY: `concurrent_chainlock_transaction_token_generator` is defined in
// `chainlock_transaction_common.cc` with C linkage (`extern "C"`) as a lock-free
// `std::atomic<uint64_t>` with static storage duration, verified by `static_assert` in C++ to have
// the exact size and alignment of `u64` / `AtomicU64`.
unsafe extern "C" {
    // Shared global atomic token generator defined in `chainlock_transaction_common.cc` so that
    // C++ and Rust transactions allocate tokens from the same monotonic generator and can
    // interoperate on the same `ChainLock` instances.
    static concurrent_chainlock_transaction_token_generator: AtomicU64;
}

/// Zero-sized capability token proving that a transaction of type `T` is currently active on the
/// calling execution context.
pub struct ChainLockTransactionToken<T> {
    // Safety invariant: A `ChainLockTransactionToken<T>` value may only exist on an execution
    // context while a transaction of type `T` is registered as the active transaction on that
    // context and remains alive and pinned at its memory address. `*mut T` makes the token
    // `!Send`, `!Sync`, and invariant in `T`.
    _private: PhantomData<*mut T>,
}

impl<T> fmt::Debug for ChainLockTransactionToken<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChainLockTransactionToken").finish()
    }
}

impl<T> ChainLockTransactionToken<T> {
    #[inline]
    pub(crate) const fn new() -> Self {
        Self { _private: PhantomData }
    }
}

/// A transaction generation token used to identify the owning transaction of a `ChainLock` and
/// arbitrate priority when contention occurs.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Token(u64);

impl Token {
    /// Token value representing an unlocked `ChainLock`.
    pub const UNLOCKED: Self = Self(UNLOCKED_TOKEN_VALUE);

    /// Maximum possible token value, representing the lowest possible transaction priority.
    pub const MAX: Self = Self(MAX_TOKEN_VALUE);

    /// Creates a new unlocked token.
    #[inline]
    pub const fn new() -> Self {
        Self::UNLOCKED
    }

    #[inline]
    pub(crate) const fn from_raw(value: u64) -> Self {
        Self(value)
    }

    /// Returns `true` if this token is valid (i.e., not the unlocked token value).
    #[inline]
    pub const fn is_valid(&self) -> bool {
        self.value() != UNLOCKED_TOKEN_VALUE
    }

    /// Returns `true` if this token is a reserved token (e.g., per-CPU token used during
    /// context switch state save/restore).
    #[inline]
    pub const fn is_reserved(&self) -> bool {
        self.is_valid() && self.value() <= RESERVED_TOKEN_VALUES
    }

    /// Returns the raw 64-bit integer value of this token.
    #[inline]
    pub const fn value(&self) -> u64 {
        self.0
    }
}

/// Atomic storage for a [`Token`].
#[repr(transparent)]
#[derive(Debug, Default)]
pub struct AtomicToken(AtomicU64);

impl AtomicToken {
    /// Creates a new [`AtomicToken`] initialized with `token`.
    #[inline]
    pub const fn new(token: Token) -> Self {
        Self(AtomicU64::new(token.value()))
    }

    /// Loads a [`Token`] from the atomic variable.
    #[inline]
    pub fn load(&self, order: Ordering) -> Token {
        Token::from_raw(self.0.load(order))
    }

    /// Stores a [`Token`] into the atomic variable.
    #[inline]
    pub fn store(&self, token: Token, order: Ordering) {
        self.0.store(token.value(), order);
    }

    /// Stores `new` into the atomic variable if the current value is `current`.
    #[inline]
    pub fn compare_exchange(
        &self,
        current: Token,
        new: Token,
        success: Ordering,
        failure: Ordering,
    ) -> Result<Token, Token> {
        self.0
            .compare_exchange(current.value(), new.value(), success, failure)
            .map(Token::from_raw)
            .map_err(Token::from_raw)
    }
}

/// Saved transaction state returned by
/// [`ChainLockTransaction::save_state_and_use_reserved_token`] and
/// [`ChainLockTransaction::save_state_initial`], used to restore an active transaction across
/// context switches.
#[repr(C)]
pub struct SavedState<E: TransactionHooks> {
    // Safety invariant: Points to the `ChainLockTransaction<E>` from which this `SavedState` was
    // created via `with_replacement`. Callers of `restore_state` must ensure that transaction is
    // still alive and pinned at this address.
    transaction: NonNull<ChainLockTransaction<E>>,
    token: Token,
}

impl<E: TransactionHooks> Clone for SavedState<E> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<E: TransactionHooks> Copy for SavedState<E> {}

impl<E: TransactionHooks> fmt::Debug for SavedState<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SavedState")
            .field("transaction", &self.transaction)
            .field("token", &self.token)
            .finish()
    }
}

impl<E: TransactionHooks> SavedState<E> {
    /// Returns the saved transaction token.
    #[inline]
    pub const fn token(&self) -> Token {
        self.token
    }

    /// Returns the non-null pointer to the saved transaction.
    #[inline]
    pub const fn transaction(&self) -> NonNull<ChainLockTransaction<E>> {
        self.transaction
    }

    #[inline]
    pub(crate) fn with_replacement(
        transaction: &ChainLockTransaction<E>,
        replacement: Token,
    ) -> Self {
        let token = transaction.token();
        transaction.replace_token(replacement);
        Self { transaction: NonNull::from(transaction), token }
    }
}

/// Non-terminal control flow action returned by a transaction attempt closure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Unwind held locks, execute the backoff protocol, and re-enter the transaction attempt.
    Backoff,
    /// Reset finalization state and re-enter the transaction attempt with the same token.
    Restart,
}

/// Marker type indicating completion of a transaction with no return value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DoneType;

/// Constant value of [`DoneType`] indicating transaction completion.
pub const DONE: DoneType = DoneType;

/// Result of a single execution of a transaction attempt closure passed to `until_done`.
#[derive(Debug, PartialEq, Eq)]
pub enum TransactionResult<T = ()> {
    /// The transaction must back off or restart and re-execute the closure.
    Action(Action),
    /// The transaction completed with the terminal value `T`.
    Value(T),
}

impl TransactionResult<()> {
    /// Convenience constant for completing a `()`-returning transaction.
    pub const DONE: Self = Self::Value(());
}

impl<T> TransactionResult<T> {
    /// Convenience constant for requesting a transaction backoff.
    pub const BACKOFF: Self = Self::Action(Action::Backoff);

    /// Convenience constant for requesting a transaction restart.
    pub const RESTART: Self = Self::Action(Action::Restart);

    /// Returns `true` if this result is an [`Action`].
    #[inline]
    pub const fn is_action(&self) -> bool {
        matches!(self, Self::Action(_))
    }

    /// Returns the [`Action`] contained in this result.
    ///
    /// # Panics
    ///
    /// Panics if `!self.is_action()`.
    #[inline]
    pub fn action(&self) -> Action {
        match self {
            Self::Action(action) => *action,
            Self::Value(_) => unreachable!(),
        }
    }

    /// Consumes this result and returns the terminal value `T`.
    ///
    /// # Panics
    ///
    /// Panics if `self.is_action()`.
    #[inline]
    pub fn take_value(self) -> T {
        match self {
            Self::Value(value) => value,
            Self::Action(_) => unreachable!(),
        }
    }
}

impl<T> From<Action> for TransactionResult<T> {
    #[inline]
    fn from(action: Action) -> Self {
        Self::Action(action)
    }
}

impl From<DoneType> for TransactionResult<()> {
    #[inline]
    fn from(_: DoneType) -> Self {
        Self::Value(())
    }
}

/// Trait implemented by return types of closures passed to `ChainLockTransaction::until_done`.
///
/// The callable passed to [`ChainLockTransaction::until_done`] may return:
/// 1. [`TransactionResult<()>`] (a `()`-returning transaction),
/// 2. [`TransactionResult<T>`] (a transaction returning user-defined type `T`), or
/// 3. [`DoneType`] (an infallible transaction with implied return type `()`).
pub trait IntoTransactionResult {
    /// The terminal value type returned by `until_done` when the transaction completes.
    type Output;

    /// Converts the closure return value into a [`TransactionResult`].
    fn into_transaction_result(self) -> TransactionResult<Self::Output>;
}

impl IntoTransactionResult for DoneType {
    type Output = ();

    #[inline]
    fn into_transaction_result(self) -> TransactionResult<()> {
        TransactionResult::Value(())
    }
}

impl<T> IntoTransactionResult for TransactionResult<T> {
    type Output = T;

    #[inline]
    fn into_transaction_result(self) -> TransactionResult<T> {
        self
    }
}

/// Lifecycle state of a [`ChainLockTransaction`].
#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Transaction has started and may acquire locks or back off.
    Started = 0,
    /// Transaction has acquired all needed locks and been finalized.
    Finalized = 1,
    /// Transaction is complete.
    Complete = 2,
}

/// Trait for contention state stored inside a [`ChainLock`](crate::ChainLock).
pub trait ContentionState: Default {
    /// Constant initial value for `const fn` construction of `ChainLock`.
    const INIT: Self;
}

impl ContentionState for AtomicU32 {
    const INIT: Self = AtomicU32::new(0);
}

impl ContentionState for AtomicU64 {
    const INIT: Self = AtomicU64::new(0);
}

impl ContentionState for () {
    const INIT: Self = ();
}

/// Trait representing a chain lock transaction type used by [`ChainLock`](crate::ChainLock) and
/// [`ChainLockGuard`](crate::ChainLockGuard).
///
/// This is automatically implemented by [`ChainLockTransaction<E>`] for any [`TransactionHooks`]
/// implementation `E`.
pub trait Transaction: Sized {
    /// State used by `ChainLock` to store contended lock state for backoff mitigation.
    type ContentionState: ContentionState;

    /// Returns a pointer to the active transaction for the current execution context, or `None` if
    /// no transaction is active.
    fn active() -> core::option::Option<NonNull<Self>>;

    /// Returns a shared reference to the active transaction, asserting in debug builds that a
    /// transaction is currently active.
    fn active_ref(token: &ChainLockTransactionToken<Self>) -> &Self;

    /// Performs an architecture-specific CPU relax/yield operation.
    fn yield_cpu();

    /// Returns the current [`Token`] of this transaction.
    fn token(&self) -> Token;

    /// Asserts that this transaction has not been finalized (when debug accounting is enabled).
    fn assert_not_finalized(&self);

    /// Asserts that this transaction currently holds `expected` locks (when debug accounting is
    /// enabled).
    fn assert_num_locks_held(&self, expected: u32);

    /// Accounting hook invoked when a `ChainLock` is acquired by this transaction.
    fn on_acquire(&self, token: &ChainLockTransactionToken<Self>);

    /// Accounting hook invoked when a `ChainLock` is released by this transaction.
    fn on_release(&self, token: &ChainLockTransactionToken<Self>);

    /// Accounting/tracing hook invoked when contention is encountered while acquiring a
    /// `ChainLock`.
    fn on_contention(&self, token: &ChainLockTransactionToken<Self>);

    /// Contention hook invoked when a `ChainLock` encounters a higher-priority owner.
    ///
    /// Returns `true` if backoff was recorded and the caller should unwind, or `false` if the
    /// lock's token changed and acquisition should be retried.
    fn record_backoff_or_retry(
        &self,
        observed_token: Token,
        lock_state: &AtomicToken,
        contention_state: &Self::ContentionState,
        token: &ChainLockTransactionToken<Self>,
    ) -> bool;

    /// Contention hook invoked when releasing a `ChainLock` to sample contention state, invoke
    /// `do_release`, and notify backed-off contenders.
    fn release_and_notify<F: FnOnce()>(
        &self,
        contention_state: &Self::ContentionState,
        do_release: F,
        token: &ChainLockTransactionToken<Self>,
    );
}

/// Storage trait for debug lock accounting in [`ChainLockTransaction`].
pub trait AccountingStorage: Default {
    /// Whether debug lock accounting is enabled.
    const ENABLED: bool;

    /// Creates a new accounting storage instance with the given initial `locks_held` and
    /// `finalized` state.
    fn new(locks_held: u32, finalized: bool) -> Self;

    /// Returns the number of locks currently held by the transaction.
    fn locks_held(&self) -> u32;

    /// Returns whether the transaction is finalized.
    fn finalized(&self) -> bool;

    /// Increments the count of locks held by the transaction.
    fn on_acquire(&self);

    /// Decrements the count of locks held by the transaction.
    fn on_release(&self);

    /// Updates whether the transaction is finalized.
    fn set_finalized(&self, finalized: bool);
}

/// Storage for debug accounting when `DebugAccountingEnabled` is `true`.
#[repr(C)]
#[derive(Default)]
pub struct AccountingStorageEnabled {
    locks_held: Cell<u32>,
    finalized: Cell<bool>,
}

impl AccountingStorage for AccountingStorageEnabled {
    const ENABLED: bool = true;

    #[inline]
    fn new(locks_held: u32, finalized: bool) -> Self {
        Self { locks_held: Cell::new(locks_held), finalized: Cell::new(finalized) }
    }

    #[inline]
    fn locks_held(&self) -> u32 {
        self.locks_held.get()
    }

    #[inline]
    fn finalized(&self) -> bool {
        self.finalized.get()
    }

    #[inline]
    fn on_acquire(&self) {
        self.locks_held.set(self.locks_held.get() + 1);
    }

    #[inline]
    fn on_release(&self) {
        self.locks_held.set(self.locks_held.get() - 1);
    }

    #[inline]
    fn set_finalized(&self, finalized: bool) {
        self.finalized.set(finalized);
    }
}

/// Zero-sized storage for debug accounting when `DebugAccountingEnabled` is `false`.
#[repr(C)]
#[derive(Default)]
pub struct AccountingStorageDisabled;

impl AccountingStorage for AccountingStorageDisabled {
    const ENABLED: bool = false;

    #[inline]
    fn new(_locks_held: u32, _finalized: bool) -> Self {
        Self
    }

    #[inline]
    fn locks_held(&self) -> u32 {
        0
    }

    #[inline]
    fn finalized(&self) -> bool {
        false
    }

    #[inline]
    fn on_acquire(&self) {}

    #[inline]
    fn on_release(&self) {}

    #[inline]
    fn set_finalized(&self, _finalized: bool) {}
}

/// Storage trait for contention spin tracing in `ChainLockTransaction`.
pub trait TraceStorage<C: Copy> {
    /// Whether contention spin tracing is enabled.
    const ENABLED: bool;

    /// Creates a new trace storage instance with the given `callsite_info`.
    fn new(callsite_info: C) -> Self;

    /// Records the start of contention if not already recorded.
    fn on_contention<E: TransactionHooks<CallsiteInfo = C>>(&self);

    /// Emits the contention trace event if contention occurred during this transaction.
    fn on_finalized<E: TransactionHooks<CallsiteInfo = C>>(&self);

    /// Updates the callsite info when restarting a transaction.
    fn set_callsite_info(&self, callsite_info: C);
}

/// Extra state when contention tracing is enabled.
#[repr(C)]
pub struct TraceStorageEnabled<C: Copy> {
    contention_start_ticks: Cell<i64>,
    callsite_info: Cell<C>,
}

impl<C: Copy> TraceStorageEnabled<C> {
    const INVALID_TICKS: i64 = 0;
}

impl<C: Copy + Default> Default for TraceStorageEnabled<C> {
    fn default() -> Self {
        Self {
            contention_start_ticks: Cell::new(Self::INVALID_TICKS),
            callsite_info: Cell::new(C::default()),
        }
    }
}

impl<C: Copy> TraceStorage<C> for TraceStorageEnabled<C> {
    const ENABLED: bool = true;

    #[inline]
    fn new(callsite_info: C) -> Self {
        Self {
            contention_start_ticks: Cell::new(Self::INVALID_TICKS),
            callsite_info: Cell::new(callsite_info),
        }
    }

    #[inline]
    fn on_contention<E: TransactionHooks<CallsiteInfo = C>>(&self) {
        if self.contention_start_ticks.get() == Self::INVALID_TICKS {
            self.contention_start_ticks.set(E::get_current_ticks());
        }
    }

    #[inline]
    fn on_finalized<E: TransactionHooks<CallsiteInfo = C>>(&self) {
        let start_ticks = self.contention_start_ticks.get();
        if start_ticks != Self::INVALID_TICKS {
            E::on_finalized(start_ticks, self.callsite_info.get());
            self.contention_start_ticks.set(Self::INVALID_TICKS);
        }
    }

    #[inline]
    fn set_callsite_info(&self, callsite_info: C) {
        self.callsite_info.set(callsite_info);
    }
}

/// Zero-sized storage when contention tracing is disabled.
#[repr(C)]
#[derive(Default)]
pub struct TraceStorageDisabled;

impl<C: Copy> TraceStorage<C> for TraceStorageDisabled {
    const ENABLED: bool = false;

    #[inline]
    fn new(_callsite_info: C) -> Self {
        Self
    }

    #[inline]
    fn on_contention<E: TransactionHooks<CallsiteInfo = C>>(&self) {}

    #[inline]
    fn on_finalized<E: TransactionHooks<CallsiteInfo = C>>(&self) {}

    #[inline]
    fn set_callsite_info(&self, _callsite_info: C) {}
}

/// Helper type for selecting storage types from compile-time `bool` constants.
pub struct Bool<const B: bool>;

/// Maps a compile-time `bool` to [`AccountingStorageEnabled`] or [`AccountingStorageDisabled`].
pub trait AccountingSelector {
    /// Selected accounting storage type.
    type Storage: AccountingStorage;
}

impl AccountingSelector for Bool<true> {
    type Storage = AccountingStorageEnabled;
}

impl AccountingSelector for Bool<false> {
    type Storage = AccountingStorageDisabled;
}

/// Type alias selecting [`AccountingStorageEnabled`] when `ENABLED` is `true`, or
/// [`AccountingStorageDisabled`] when `ENABLED` is `false`.
pub type SelectAccounting<const ENABLED: bool> = <Bool<ENABLED> as AccountingSelector>::Storage;

/// Maps a compile-time `bool` to [`TraceStorageEnabled<C>`] or [`TraceStorageDisabled`].
pub trait TracingSelector<C: Copy> {
    /// Selected trace storage type.
    type Storage: TraceStorage<C>;
}

impl<C: Copy> TracingSelector<C> for Bool<true> {
    type Storage = TraceStorageEnabled<C>;
}

impl<C: Copy> TracingSelector<C> for Bool<false> {
    type Storage = TraceStorageDisabled;
}

/// Type alias selecting [`TraceStorageEnabled<C>`] when `ENABLED` is `true`, or
/// [`TraceStorageDisabled`] when `ENABLED` is `false`.
pub type SelectTracing<const ENABLED: bool, C> = <Bool<ENABLED> as TracingSelector<C>>::Storage;

/// Trait implemented by environment-specific state savers (such as IRQ-save and preempt-disable
/// guards) managed by [`ChainLockTransaction::until_done`].
pub trait StateSaver<T> {
    /// Relaxes the saved state (e.g., unregistering the transaction, re-enabling interrupts /
    /// preemption, waiting for conflict resolution, re-disabling interrupts / preemption, and
    /// re-registering the transaction) during a backoff cycle.
    fn relax(&mut self, transaction: &T);
}

/// Trait implemented by compile-time state option tags passed to
/// [`ChainLockTransaction::until_done`].
pub trait StateOption<T> {
    /// The RAII [`StateSaver`] type instantiated for this option.
    type Saver: StateSaver<T>;

    /// Saves the environment state for the duration of the transaction and returns the RAII
    /// [`StateSaver`].
    fn save_state(&self) -> Self::Saver;
}

/// Wrapper for a transaction state option tag.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Option<O>(pub O);

impl<O> Option<O> {
    /// Creates a new `Option` wrapping `option`.
    #[inline]
    pub const fn new(option: O) -> Self {
        Self(option)
    }
}

impl<T, O: StateOption<T>> StateOption<T> for Option<O> {
    type Saver = O::Saver;

    #[inline]
    fn save_state(&self) -> Self::Saver {
        self.0.save_state()
    }
}

/// Environment-specific hooks required by [`ChainLockTransaction`].
///
/// # Safety
///
/// Implementors must ensure that:
/// - [`Self::active`] returns the exact pointer most recently passed to [`Self::set_active`] on
///   the current execution context (or `None` if [`Self::set_active`] has not been called or was
///   most recently called with `None` on the current execution context).
/// - The storage backing [`Self::active`] and [`Self::set_active`] is strictly local to the
///   current execution context (e.g., per-CPU or thread-local storage) and is never shared across
///   concurrently executing contexts.
pub unsafe trait TransactionHooks: Sized + 'static {
    /// State used by `ChainLock` to store contended lock state for backoff mitigation
    /// (e.g., `AtomicU32` for `cpu_mask_t` in the Zircon kernel).
    type ContentionState: ContentionState;

    /// Debugging/tracing information that identifies the code location of the transaction.
    type CallsiteInfo: Copy;

    /// Storage for debug lock accounting (`SelectAccounting<ENABLED>`).
    type Accounting: AccountingStorage;

    /// Storage for contention spin tracing (`SelectTracing<ENABLED, Self::CallsiteInfo>`).
    type Tracing: TraceStorage<Self::CallsiteInfo>;

    /// Additional environment-specific state stored in each transaction instance
    /// (e.g., `Cell<u64>` for `conflict_id_` in the Zircon kernel, or `()` when none is needed).
    type ExtraState: Default;

    /// Returns a pointer to the active chain lock transaction for the current execution context
    /// (e.g., current CPU), or `None` if none is active.
    fn active() -> core::option::Option<NonNull<ChainLockTransaction<Self>>>;

    /// Sets the active chain lock transaction for the current execution context.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `transaction` is either `None` or points to a valid
    /// `ChainLockTransaction<Self>` that remains alive and pinned at its memory address for as
    /// long as it remains registered as the active transaction on the current execution context.
    unsafe fn set_active(transaction: core::option::Option<NonNull<ChainLockTransaction<Self>>>);

    /// Performs an architecture-specific CPU relax/yield operation.
    fn yield_cpu();

    /// Allocates a reserved token for use with
    /// [`ChainLockTransaction::save_state_and_use_reserved_token`] and
    /// [`ChainLockTransaction::save_state_initial`].
    fn allocate_reserved_token() -> Token;

    /// Gets the current monotonic ticks for contention spin tracing.
    #[inline]
    fn get_current_ticks() -> i64 {
        0
    }

    /// Emits a trace event or updates counters when a contended transaction is finalized.
    #[inline]
    fn on_finalized(_contention_start_ticks: i64, _callsite_info: Self::CallsiteInfo) {}

    /// Updates counters or state when a transaction backs off (invoked after incrementing
    /// `backoff_count`).
    #[inline]
    fn on_backoff(_transaction: &ChainLockTransaction<Self>) {}

    /// Handles backoff mitigation when contention with a higher-priority transaction is observed.
    ///
    /// Returns `true` if the backoff was recorded and the caller should unwind held locks, or
    /// `false` if the lock's token changed and the caller should retry acquisition.
    fn record_backoff_or_retry(
        transaction: &ChainLockTransaction<Self>,
        observed_token: Token,
        lock_state: &AtomicToken,
        contention_state: &Self::ContentionState,
    ) -> bool;

    /// Carefully sequences the operations required to release a `ChainLock` and notify backed-off
    /// contenders.
    fn release_and_notify<F: FnOnce()>(
        transaction: &ChainLockTransaction<Self>,
        contention_state: &Self::ContentionState,
        do_release: F,
    );
}

/// Environment-independent framework for chain lock transactions.
///
/// Chain lock transactions are a layer on top of chain locks that track the state necessary for
/// entities (i.e. CPUs) to acquire a series of one or more chain locks and correctly handle
/// arbitration with other entities contending for the same locks.
#[repr(C)]
pub struct ChainLockTransaction<E: TransactionHooks> {
    token: Cell<Token>,
    state: Cell<State>,
    accounting: E::Accounting,
    tracing: E::Tracing,
    backoff_count: Cell<u64>,
    extra: E::ExtraState,
    _pin: PhantomData<(*mut (), PhantomPinned)>,
}

impl<E: TransactionHooks> ChainLockTransaction<E> {
    /// Token value representing an unlocked `ChainLock`.
    pub const UNLOCKED_TOKEN_VALUE: u64 = UNLOCKED_TOKEN_VALUE;

    /// Number of reserved token values (`1..=RESERVED_TOKEN_VALUES`).
    pub const RESERVED_TOKEN_VALUES: u64 = RESERVED_TOKEN_VALUES;

    /// Initial value for the global dynamic transaction token generator.
    pub const INITIAL_TOKEN_VALUE: u64 = INITIAL_TOKEN_VALUE;

    /// Maximum possible token value, used transiently during lock release.
    pub const MAX_TOKEN_VALUE: u64 = MAX_TOKEN_VALUE;

    /// Whether debug lock accounting is enabled for this transaction type.
    pub const DEBUG_ACCOUNTING_ENABLED: bool = E::Accounting::ENABLED;

    /// Whether contention spin tracing is enabled for this transaction type.
    pub const CONTENTION_TRACING_ENABLED: bool = E::Tracing::ENABLED;

    /// Constant value of [`DoneType`] for completing a `()`-returning transaction.
    pub const DONE: DoneType = DONE;

    /// Returns the current [`Token`] of this transaction.
    #[inline]
    pub fn token(&self) -> Token {
        self.token.get()
    }

    /// Allocates the next monotonically increasing dynamic transaction [`Token`] from the global
    /// token generator.
    #[inline]
    pub fn next_token() -> Token {
        // SAFETY: Accessing the extern static `concurrent_chainlock_transaction_token_generator`
        // and calling `fetch_add` on it is sound because it is defined in C++ with static storage
        // duration as a lock-free 64-bit atomic integer.
        let raw = unsafe {
            concurrent_chainlock_transaction_token_generator.fetch_add(1, Ordering::Relaxed)
        };
        Token::from_raw(raw)
    }

    /// Returns the reserved [`Token`] for `reserved_index` (e.g., a CPU number).
    ///
    /// Asserts in debug builds that `reserved_index < Self::RESERVED_TOKEN_VALUES`.
    #[inline]
    pub fn reserved_token(reserved_index: usize) -> Token {
        debug_assert!((reserved_index as u64) < Self::RESERVED_TOKEN_VALUES);
        Token::from_raw((reserved_index as u64) + 1)
    }

    /// Returns the current [`State`] of this transaction.
    #[inline]
    pub fn state(&self) -> State {
        self.state.get()
    }

    /// Updates the [`State`] of this transaction.
    #[inline]
    pub(crate) fn set_state(&self, state: State) {
        self.state.set(state);
    }

    // Replace a token value when saving or restoring transaction state. Both tokens must be valid
    // and one or the other must be reserved.
    #[inline]
    pub(crate) fn replace_token(&self, replacement: Token) {
        let current = self.token.get();
        debug_assert!(current.is_valid());
        debug_assert!(replacement.is_valid());
        debug_assert_ne!(current.is_reserved(), replacement.is_reserved());
        self.token.set(replacement);
    }

    /// Provides a common sequence for executing a chain lock transaction and the required
    /// backoff arbitration logic.
    ///
    /// The caller is responsible for providing three arguments:
    /// 1. An implementation of [`StateOption<Self>`] (such as [`Option<O>`]): Used to instantiate
    ///    the environment-provided [`StateSaver`] type, indicating the relevant states to save,
    ///    such as IRQ enable flags, preemption flags, etc., and restore after each transaction
    ///    attempt.
    /// 2. An instance of [`E::CallsiteInfo`](TransactionHooks::CallsiteInfo): Used for debugging
    ///    and tracing to identify the code location of the transaction.
    /// 3. A callable implementing a single transaction attempt: This can be a function or closure
    ///    accepting a [`&ChainLockTransactionToken<Self>`](ChainLockTransactionToken) and
    ///    returning a type implementing [`IntoTransactionResult`].
    ///
    /// The callable should match one of the following signatures:
    /// a. `FnMut(&ChainLockTransactionToken<Self>) -> TransactionResult<()>`: A transaction that
    ///    has a return type `()`.
    /// b. `FnMut(&ChainLockTransactionToken<Self>) -> TransactionResult<T>`: A transaction that
    ///    has a user-defined return type `T`.
    /// c. `FnMut(&ChainLockTransactionToken<Self>) -> DoneType`: A transaction that has an implied
    ///    return type `()`.
    ///
    /// The overall return type of `until_done` is either `T` or `()`, depending on the signature
    /// of `callable`, as described above.
    ///
    /// A basic transaction is structured as follows:
    ///
    /// 1. Define a callable that will be executed until the transaction is complete. In this
    ///    example, the callable will return `zx_status_t` when the transaction is complete, which
    ///    will become the return value of `until_done`.
    ///
    /// ```ignore
    /// let do_transaction =
    ///     |token: &ChainLockTransactionToken| -> TransactionResult<zx_status_t> {
    ///         if !object.get_lock().acquire_or_backoff(token) {
    ///             return Action::Backoff.into(); // Execute the backoff protocol and re-enter.
    ///         }
    ///
    ///         if restart_transaction_condition {
    ///             object.get_lock().release(token);
    ///             return Action::Restart.into(); // Restart the transaction and re-enter.
    ///         }
    ///
    ///         if !locked_operation_on_object(object, token) {
    ///             object.get_lock().release(token);
    ///             // Complete the transaction, returning the error to the caller.
    ///             return TransactionResult::Value(ZX_ERR_BAD_STATE);
    ///         }
    ///
    ///         object.get_lock().release(token);
    ///         // Complete the transaction, returning success to the caller.
    ///         TransactionResult::Value(ZX_OK)
    ///     };
    /// ```
    ///
    /// In cases where the user return type is `()`, the callable can return
    /// [`ChainLockTransaction::DONE`] (or `DONE.into()` / [`TransactionResult::DONE`]) to complete
    /// the transaction.
    ///
    /// 2. Execute the transaction using the given options and callable. In this example, the
    ///    constant `STATE_OPTION_A` is a value of type [`Option<O>`] with an environment-specific
    ///    option value to pass to the state saver. The callable is invoked cyclically, with
    ///    intervening backoffs, until a terminal value (of a user-defined return type in this
    ///    case) is returned.
    ///
    /// ```ignore
    /// return ChainLockTransaction::until_done(
    ///     STATE_OPTION_A,
    ///     clt_callsite!("foo"),
    ///     do_transaction,
    /// );
    /// ```
    #[inline]
    pub fn until_done<O, F, R>(
        option: O,
        callsite_info: E::CallsiteInfo,
        mut callable: F,
    ) -> R::Output
    where
        O: StateOption<Self>,
        F: FnMut(&ChainLockTransactionToken<Self>) -> R,
        R: IntoTransactionResult,
    {
        // Instantiate the state saver type with the supplied option to take care of state save,
        // restore, and relax operations.
        //
        // Declared before `transaction` so that in Rust's reverse-declaration drop order,
        // `transaction` is dropped (and unregistered) BEFORE `state_saver` restores interrupts /
        // preemption state.
        let mut state_saver = option.save_state();

        // Instantiate the transaction to store the transaction state and register it as the
        // active transaction once placed in its stack slot.
        let transaction = Self::new_unregistered(callsite_info, 0, false);
        // SAFETY: `transaction` remains alive and unmoved in its stack slot for the duration of
        // `until_done`, and `token` does not escape `until_done`. `register` asserts that no other
        // transaction is currently active on the current execution context.
        let token = unsafe { transaction.register() };

        // Execute the callable until it returns a terminal value indicating the transaction is
        // complete.
        loop {
            // Execute the transaction and get the returned result.
            let result = callable(&token).into_transaction_result();

            // If the result is not an action it is a terminal value. Complete the transaction and
            // return the user defined value, if any.
            match result {
                TransactionResult::Value(value) => {
                    if transaction.state() == State::Started {
                        Self::finalize(&token);
                    }
                    transaction.set_state(State::Complete);
                    return value;
                }
                TransactionResult::Action(Action::Backoff) => {
                    debug_assert_eq!(transaction.state(), State::Started);
                    transaction.assert_num_locks_held(0);
                    transaction.assert_not_finalized();
                    transaction.backoff_count.set(transaction.backoff_count.get() + 1);
                    E::on_backoff(&transaction); // Notify after updating backoff_count.
                    state_saver.relax(&transaction);
                    debug_assert_eq!(E::active(), Some(NonNull::from(&transaction)));
                }
                TransactionResult::Action(Action::Restart) => {
                    debug_assert_eq!(transaction.state(), State::Finalized);
                    transaction.restart(callsite_info, &token);
                }
            }
        }
    }

    /// Returns the number of times this transaction has backed off.
    #[inline]
    pub fn backoff_count(&self) -> u64 {
        self.backoff_count.get()
    }

    /// Returns a reference to the environment-specific extra state (e.g., `conflict_id_` in the
    /// kernel).
    #[inline]
    pub const fn extra(&self) -> &E::ExtraState {
        &self.extra
    }

    /// Asserts that at least one lock is currently held by this transaction (when debug accounting
    /// is enabled).
    #[inline]
    pub fn assert_at_least_one_lock_held(&self) {
        if Self::DEBUG_ACCOUNTING_ENABLED {
            assert!(self.accounting.locks_held() > 0);
        }
    }

    /// Asserts that at most one lock is currently held by this transaction (when debug accounting
    /// is enabled).
    #[inline]
    pub fn assert_at_most_one_lock_held(&self) {
        if Self::DEBUG_ACCOUNTING_ENABLED {
            assert!(self.accounting.locks_held() <= 1);
        }
    }

    /// Asserts that exactly `expected` locks are currently held by this transaction (when debug
    /// accounting is enabled).
    #[inline]
    pub fn assert_num_locks_held(&self, expected: u32) {
        if Self::DEBUG_ACCOUNTING_ENABLED {
            let actual = self.accounting.locks_held();
            assert_eq!(expected, actual, "expected={expected}, actual={actual}");
        }
    }

    /// Asserts that this transaction has been finalized (when debug accounting is enabled).
    #[inline]
    pub fn assert_finalized(&self) {
        if Self::DEBUG_ACCOUNTING_ENABLED {
            assert!(self.accounting.finalized());
        }
    }

    /// Asserts that this transaction has not been finalized (when debug accounting is enabled).
    #[inline]
    pub fn assert_not_finalized(&self) {
        if Self::DEBUG_ACCOUNTING_ENABLED {
            assert!(!self.accounting.finalized());
        }
    }

    /// Asserts in debug builds that a transaction is currently active and mints a
    /// [`ChainLockTransactionToken<Self>`].
    ///
    /// # Safety
    ///
    /// The caller must ensure that a `ChainLockTransaction<E>` is currently registered as active
    /// on the current execution context, and that the returned token is not used while that
    /// transaction is unregistered or after it is dropped.
    #[inline]
    pub unsafe fn assert_active() -> ChainLockTransactionToken<Self> {
        debug_assert!(E::active().is_some());
        ChainLockTransactionToken::new()
    }

    /// Mints a [`ChainLockTransactionToken<Self>`] without checking at runtime for efficiency in
    /// trivial cases.
    ///
    /// # Safety
    ///
    /// The caller must ensure that a `ChainLockTransaction<E>` is currently registered as active
    /// on the current execution context, and that the returned token is not used while that
    /// transaction is unregistered or after it is dropped.
    #[inline]
    pub unsafe fn mark_active() -> ChainLockTransactionToken<Self> {
        ChainLockTransactionToken::new()
    }

    /// Returns a pointer to the active transaction, or `None` if none is active.
    #[inline]
    pub fn active() -> core::option::Option<NonNull<Self>> {
        E::active()
    }

    /// Returns a reference to the active transaction. Asserts in debug builds that a transaction
    /// is active.
    #[inline]
    pub fn active_ref(_token: &ChainLockTransactionToken<Self>) -> &Self {
        let ptr = E::active();
        debug_assert!(ptr.is_some());
        // SAFETY: `_token` is a valid `ChainLockTransactionToken<Self>`, which by its safety
        // invariant guarantees that `E::active()` is `Some` and points to a valid, initialized
        // `ChainLockTransaction<E>` that remains alive and pinned at its memory address on the
        // current execution context, with all interior mutations mediated through `Cell`.
        unsafe { ptr.unwrap_unchecked().as_ref() }
    }

    /// Saves the pointer to the current transaction and allocates a reserved token to sync leaf
    /// chain locks to during critical sequences.
    #[inline]
    pub fn save_state_and_use_reserved_token(
        token: &ChainLockTransactionToken<Self>,
    ) -> SavedState<E> {
        let transaction = Self::active_ref(token);
        debug_assert!(!transaction.token().is_reserved());
        SavedState::with_replacement(transaction, E::allocate_reserved_token())
    }

    /// Creates a saved state for a bare transaction used to kick off a new thread.
    #[inline]
    pub fn save_state_initial(&self, _token: &ChainLockTransactionToken<Self>) -> SavedState<E> {
        debug_assert_ne!(E::active(), Some(NonNull::from(self)));
        SavedState::with_replacement(self, E::allocate_reserved_token())
    }

    /// Restores the active transaction using the given saved state.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `saved_state` was produced by
    /// [`Self::save_state_and_use_reserved_token`] or [`Self::save_state_initial`] for a
    /// `ChainLockTransaction<E>` instance that remains alive and pinned at its memory address for
    /// as long as it remains registered as the active transaction on the current execution
    /// context.
    #[inline]
    pub unsafe fn restore_state(saved_state: SavedState<E>) {
        let transaction_ptr = saved_state.transaction();
        // SAFETY: The caller guarantees `saved_state` points to a valid, live `Self` instance on
        // the current execution context.
        let transaction = unsafe { transaction_ptr.as_ref() };
        debug_assert!(transaction.token().is_reserved());
        transaction.replace_token(saved_state.token());
        // SAFETY: The caller guarantees `transaction_ptr` points to a valid `Self` instance that
        // remains alive and pinned while registered as active.
        unsafe { E::set_active(Some(transaction_ptr)) };
    }

    /// Directly finalizes the active transaction, ending an active contention tracing duration,
    /// if any.
    #[inline]
    pub fn finalize(token: &ChainLockTransactionToken<Self>) {
        let transaction = Self::active_ref(token);
        transaction.assert_not_finalized();
        transaction.set_finalized(true);
    }

    /// Restarts this transaction, resetting key state including the callsite info.
    #[inline]
    pub fn restart(
        &self,
        callsite_info: E::CallsiteInfo,
        _token: &ChainLockTransactionToken<Self>,
    ) {
        self.assert_finalized();
        self.assert_at_most_one_lock_held();
        debug_assert!(self.token().is_valid());
        self.set_finalized(false);
        if Self::CONTENTION_TRACING_ENABLED {
            self.tracing.set_callsite_info(callsite_info);
        }
    }

    /// Returns the number of locks currently held by this transaction.
    ///
    /// Requires `Self::DEBUG_ACCOUNTING_ENABLED` to be `true` at compile time.
    #[inline]
    pub fn locks_held(&self) -> u32 {
        const {
            assert!(Self::DEBUG_ACCOUNTING_ENABLED);
        }
        self.accounting.locks_held()
    }

    /// Returns whether this transaction is currently finalized.
    ///
    /// Requires `Self::DEBUG_ACCOUNTING_ENABLED` to be `true` at compile time.
    #[inline]
    pub fn is_finalized(&self) -> bool {
        const {
            assert!(Self::DEBUG_ACCOUNTING_ENABLED);
        }
        self.accounting.finalized()
    }

    /// Creates an unregistered bare transaction with the given initial state.
    ///
    /// Because Rust does not guarantee copy elision (RVO) when returning by value, callers
    /// creating a bare transaction outside [`Self::until_done`] must place the returned
    /// transaction in its stack variable first, and then call [`Self::register`] (or
    /// [`Self::restore_state`] with [`Self::save_state_initial`]). Note that `Drop` for
    /// [`ChainLockTransaction`] unconditionally calls [`Self::unregister`], so the transaction
    /// must be registered as active before it is dropped.
    #[inline]
    pub fn new_unregistered(
        callsite_info: E::CallsiteInfo,
        locks_held: u32,
        finalized: bool,
    ) -> Self {
        let state = if finalized { State::Finalized } else { State::Started };
        Self {
            token: Cell::new(Self::next_token()),
            state: Cell::new(state),
            accounting: E::Accounting::new(locks_held, finalized),
            tracing: E::Tracing::new(callsite_info),
            backoff_count: Cell::new(0),
            extra: E::ExtraState::default(),
            _pin: PhantomData,
        }
    }

    /// Registers `self` as the active transaction for the current execution context and mints a
    /// [`ChainLockTransactionToken<Self>`].
    ///
    /// Asserts that no transaction is currently active.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `self` remains alive and pinned at its current memory address (i.e. not moved or
    ///   forgotten without unregistering) for as long as it remains registered as the active
    ///   transaction on the current execution context.
    /// - The returned [`ChainLockTransactionToken<Self>`] is not used while `self` is unregistered
    ///   or after `self` is dropped.
    #[inline]
    pub unsafe fn register(&self) -> ChainLockTransactionToken<Self> {
        assert!(E::active().is_none());
        // SAFETY: The caller guarantees `self` remains alive and pinned at its memory address
        // while registered as the active transaction on the current execution context.
        unsafe { E::set_active(Some(NonNull::from(self))) };
        ChainLockTransactionToken::new()
    }

    /// Unregisters `self` as the active transaction for the current execution context.
    ///
    /// Asserts in debug builds that `self` is the currently active transaction.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `self` is currently the active transaction on the current execution context.
    /// - No [`ChainLockTransactionToken<Self>`] associated with `self` is used to access the
    ///   active transaction while `self` is unregistered.
    #[inline]
    pub unsafe fn unregister(&self) {
        debug_assert_eq!(E::active(), Some(NonNull::from(self)));
        // SAFETY: Passing `None` to `E::set_active` clears the active transaction pointer on the
        // current execution context.
        unsafe { E::set_active(None) };
    }

    #[inline]
    pub(crate) fn set_finalized(&self, finalized: bool) {
        self.set_state(if finalized { State::Finalized } else { State::Started });
        if Self::DEBUG_ACCOUNTING_ENABLED {
            self.accounting.set_finalized(finalized);
        }
        if Self::CONTENTION_TRACING_ENABLED && finalized {
            self.tracing.on_finalized::<E>();
        }
    }
}

impl<E: TransactionHooks> Drop for ChainLockTransaction<E> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: Unconditionally unregister `self` on drop (matching C++
        // `~ChainLockTransaction`), asserting in debug builds that `self` is the active
        // transaction. Because `self` is being dropped, no valid
        // `ChainLockTransactionToken<Self>` may access it after this point.
        unsafe {
            self.unregister();
        }
    }
}

impl<E: TransactionHooks> Transaction for ChainLockTransaction<E> {
    type ContentionState = E::ContentionState;

    #[inline]
    fn active() -> core::option::Option<NonNull<Self>> {
        E::active()
    }

    #[inline]
    fn active_ref(token: &ChainLockTransactionToken<Self>) -> &Self {
        Self::active_ref(token)
    }

    #[inline]
    fn yield_cpu() {
        E::yield_cpu();
    }

    #[inline]
    fn token(&self) -> Token {
        Self::token(self)
    }

    #[inline]
    fn assert_not_finalized(&self) {
        Self::assert_not_finalized(self);
    }

    #[inline]
    fn assert_num_locks_held(&self, expected: u32) {
        Self::assert_num_locks_held(self, expected);
    }

    #[inline]
    fn on_acquire(&self, _token: &ChainLockTransactionToken<Self>) {
        if Self::DEBUG_ACCOUNTING_ENABLED {
            self.accounting.on_acquire();
        }
    }

    #[inline]
    fn on_release(&self, _token: &ChainLockTransactionToken<Self>) {
        if Self::DEBUG_ACCOUNTING_ENABLED {
            self.accounting.on_release();
        }
    }

    #[inline]
    fn on_contention(&self, _token: &ChainLockTransactionToken<Self>) {
        if Self::CONTENTION_TRACING_ENABLED {
            self.tracing.on_contention::<E>();
        }
    }

    #[inline]
    fn record_backoff_or_retry(
        &self,
        observed_token: Token,
        lock_state: &AtomicToken,
        contention_state: &Self::ContentionState,
        _token: &ChainLockTransactionToken<Self>,
    ) -> bool {
        E::record_backoff_or_retry(self, observed_token, lock_state, contention_state)
    }

    #[inline]
    fn release_and_notify<F: FnOnce()>(
        &self,
        contention_state: &Self::ContentionState,
        do_release: F,
        _token: &ChainLockTransactionToken<Self>,
    ) {
        E::release_and_notify(self, contention_state, do_release);
    }
}

// Verify layout parity across all 4 combinations of `(DEBUG_ACCOUNTING_ENABLED,
// CONTENTION_TRACING_ENABLED)` for both empty `ExtraState` (`concurrent::ChainLockTransaction`)
// and 8-byte `ExtraState` (`::ChainLockTransaction` in `kconcurrent` with `uint64_t
// conflict_id_`).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DummyKernelCallsiteInfo {
    label: *const u8,
    line_number: u32,
}

macro_rules! impl_dummy_hooks {
    ($name:ident, $debug:expr, $trace:expr, $extra:ty) => {
        struct $name;
        // SAFETY: `$name` is an uninstantiated type used solely for compile-time layout
        // assertions; `active()` always returns `None` and `set_active()` is never called at
        // runtime.
        unsafe impl TransactionHooks for $name {
            type ContentionState = AtomicU32;
            type CallsiteInfo = DummyKernelCallsiteInfo;
            type Accounting = SelectAccounting<$debug>;
            type Tracing = SelectTracing<$trace, Self::CallsiteInfo>;
            type ExtraState = $extra;

            fn active() -> core::option::Option<NonNull<ChainLockTransaction<Self>>> {
                None
            }
            unsafe fn set_active(_: core::option::Option<NonNull<ChainLockTransaction<Self>>>) {}
            fn yield_cpu() {}
            fn allocate_reserved_token() -> Token {
                Token::UNLOCKED
            }
            fn record_backoff_or_retry(
                _: &ChainLockTransaction<Self>,
                _: Token,
                _: &AtomicToken,
                _: &Self::ContentionState,
            ) -> bool {
                true
            }
            fn release_and_notify<F: FnOnce()>(
                _: &ChainLockTransaction<Self>,
                _: &Self::ContentionState,
                do_release: F,
            ) {
                do_release();
            }
        }
    };
}

impl_dummy_hooks!(BaseFalseFalse, false, false, ());
impl_dummy_hooks!(BaseTrueFalse, true, false, ());
impl_dummy_hooks!(BaseFalseTrue, false, true, ());
impl_dummy_hooks!(BaseTrueTrue, true, true, ());

impl_dummy_hooks!(KernelFalseFalse, false, false, Cell<u64>);
impl_dummy_hooks!(KernelTrueFalse, true, false, Cell<u64>);
impl_dummy_hooks!(KernelFalseTrue, false, true, Cell<u64>);
impl_dummy_hooks!(KernelTrueTrue, true, true, Cell<u64>);

zr::static_assert!(
    size_of::<ChainLockTransactionToken<ChainLockTransaction<BaseFalseFalse>>>() == 0
);
zr::static_assert!(
    align_of::<ChainLockTransactionToken<ChainLockTransaction<BaseFalseFalse>>>() == 1
);
zr::static_assert!(size_of::<Token>() == 8);
zr::static_assert!(align_of::<Token>() == 8);
zr::static_assert!(size_of::<AtomicToken>() == 8);
zr::static_assert!(align_of::<AtomicToken>() == 8);
zr::static_assert!(size_of::<SavedState<BaseFalseFalse>>() == 16);
zr::static_assert!(align_of::<SavedState<BaseFalseFalse>>() == 8);
zr::static_assert!(offset_of!(SavedState<BaseFalseFalse>, transaction) == 0);
zr::static_assert!(offset_of!(SavedState<BaseFalseFalse>, token) == 8);
zr::static_assert!(size_of::<State>() == 4);
zr::static_assert!(align_of::<State>() == 4);

zr::static_assert!(size_of::<DummyKernelCallsiteInfo>() == 16);
zr::static_assert!(align_of::<DummyKernelCallsiteInfo>() == 8);

zr::static_assert!(size_of::<AccountingStorageEnabled>() == 8);
zr::static_assert!(align_of::<AccountingStorageEnabled>() == 4);
zr::static_assert!(offset_of!(AccountingStorageEnabled, locks_held) == 0);
zr::static_assert!(offset_of!(AccountingStorageEnabled, finalized) == 4);

// Base `concurrent::ChainLockTransaction` layout assertions:
zr::static_assert!(size_of::<ChainLockTransaction<BaseFalseFalse>>() == 24);
zr::static_assert!(align_of::<ChainLockTransaction<BaseFalseFalse>>() == 8);
zr::static_assert!(offset_of!(ChainLockTransaction<BaseFalseFalse>, token) == 0);
zr::static_assert!(offset_of!(ChainLockTransaction<BaseFalseFalse>, state) == 8);
zr::static_assert!(offset_of!(ChainLockTransaction<BaseFalseFalse>, backoff_count) == 16);

zr::static_assert!(size_of::<ChainLockTransaction<BaseTrueFalse>>() == 32);
zr::static_assert!(align_of::<ChainLockTransaction<BaseTrueFalse>>() == 8);
zr::static_assert!(offset_of!(ChainLockTransaction<BaseTrueFalse>, token) == 0);
zr::static_assert!(offset_of!(ChainLockTransaction<BaseTrueFalse>, state) == 8);
zr::static_assert!(offset_of!(ChainLockTransaction<BaseTrueFalse>, accounting) == 12);
zr::static_assert!(offset_of!(ChainLockTransaction<BaseTrueFalse>, backoff_count) == 24);

zr::static_assert!(size_of::<ChainLockTransaction<BaseFalseTrue>>() == 48);
zr::static_assert!(align_of::<ChainLockTransaction<BaseFalseTrue>>() == 8);
zr::static_assert!(offset_of!(ChainLockTransaction<BaseFalseTrue>, token) == 0);
zr::static_assert!(offset_of!(ChainLockTransaction<BaseFalseTrue>, state) == 8);
zr::static_assert!(offset_of!(ChainLockTransaction<BaseFalseTrue>, tracing) == 16);
zr::static_assert!(offset_of!(ChainLockTransaction<BaseFalseTrue>, backoff_count) == 40);

zr::static_assert!(size_of::<ChainLockTransaction<BaseTrueTrue>>() == 56);
zr::static_assert!(align_of::<ChainLockTransaction<BaseTrueTrue>>() == 8);
zr::static_assert!(offset_of!(ChainLockTransaction<BaseTrueTrue>, token) == 0);
zr::static_assert!(offset_of!(ChainLockTransaction<BaseTrueTrue>, state) == 8);
zr::static_assert!(offset_of!(ChainLockTransaction<BaseTrueTrue>, accounting) == 12);
zr::static_assert!(offset_of!(ChainLockTransaction<BaseTrueTrue>, tracing) == 24);
zr::static_assert!(offset_of!(ChainLockTransaction<BaseTrueTrue>, backoff_count) == 48);

// Kernel `::ChainLockTransaction` (with `uint64_t conflict_id_`) layout assertions:
zr::static_assert!(size_of::<ChainLockTransaction<KernelFalseFalse>>() == 32);
zr::static_assert!(offset_of!(ChainLockTransaction<KernelFalseFalse>, extra) == 24);

zr::static_assert!(size_of::<ChainLockTransaction<KernelTrueFalse>>() == 40);
zr::static_assert!(offset_of!(ChainLockTransaction<KernelTrueFalse>, extra) == 32);

zr::static_assert!(size_of::<ChainLockTransaction<KernelFalseTrue>>() == 56);
zr::static_assert!(offset_of!(ChainLockTransaction<KernelFalseTrue>, extra) == 48);

zr::static_assert!(size_of::<ChainLockTransaction<KernelTrueTrue>>() == 64);
zr::static_assert!(offset_of!(ChainLockTransaction<KernelTrueTrue>, extra) == 56);
