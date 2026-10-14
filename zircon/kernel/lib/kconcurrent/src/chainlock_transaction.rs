// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel-specific [`ChainLockTransaction`] hooks, kcounters, state savers, and
//! [`SingleChainLockGuard`].

use core::cell::Cell;
use core::marker::PhantomData;
use core::mem::{MaybeUninit, offset_of};
use core::ops::Deref;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, Ordering};

use concurrent::{
    AtomicToken, SelectAccounting, SelectTracing, StateOption, StateSaver, Token, TransactionHooks,
};

use crate::arch_rs::{
    InterruptSavedState, arch_interrupt_restore, arch_interrupt_save, curr_cpu_num, ints_disabled,
};
use crate::counters::define_kcounter;
use crate::kernel::percpu::PerCpu;
use crate::kernel::thread::{
    eager_resched_disable, eager_resched_reenable, preempt_disable, preempt_reenable,
};
use crate::kernel::types::cpu_num_to_mask;
use crate::ktrace_rs::InstantBootTicks;
use crate::platform_rs::timer::{
    DurationMonoTicks, timer_current_mono_ticks, timer_get_ticks_to_time_ratio,
};

use super::chainlock::{
    CLT_DEBUG_CHECKS_ENABLED, CLT_TRACING_ENABLED, CallsiteInfo, ChainLock, ChainLockGuard,
    ChainLockTransaction, ChainLockTransactionToken, EagerReschedDisable, IrqSave, NoIrqSave,
    PreemptDisable,
};

define_kcounter!(CHAINLOCK_BACKOFFS, "chainlock.backoffs", Sum);
define_kcounter!(CHAINLOCK_BACKOFFS_MAX, "chainlock.backoffs_max", Max);
define_kcounter!(CHAINLOCK_CONTENTION_NS, "chainlock.contention_ns", Sum);
define_kcounter!(CHAINLOCK_CONTENTION_NS_MAX, "chainlock.contention_ns_max", Max);

#[inline]
fn on_backoff_counters(backoff_count: u64) {
    CHAINLOCK_BACKOFFS.add(1);
    CHAINLOCK_BACKOFFS_MAX.max(backoff_count as i64);
}

#[inline]
fn update_contention_counters(contention_ticks: DurationMonoTicks) {
    let ticks_to_time = timer_get_ticks_to_time_ratio();
    let contention_ns = ticks_to_time * contention_ticks.0;
    CHAINLOCK_CONTENTION_NS.add(contention_ns);
    CHAINLOCK_CONTENTION_NS_MAX.max(contention_ns);
}

/// FFI entry point called by C++ `ChainLockTransaction::OnBackoff()`.
// SAFETY:
// - `rust_chainlock_on_backoff` is a globally unique symbol name in the kernel image, declared
//   only in `zircon/kernel/lib/kconcurrent/chainlock_transaction.cc`.
// - The `extern "C" fn(u64)` signature matches the C++ `extern "C" void
//   rust_chainlock_on_backoff(uint64_t)` declaration.
#[unsafe(no_mangle)]
pub extern "C" fn rust_chainlock_on_backoff(backoff_count: u64) {
    on_backoff_counters(backoff_count);
}

/// FFI entry point called by C++ `ChainLockTransaction::UpdateContentionCounters()`.
// SAFETY:
// - `rust_chainlock_update_contention_counters` is a globally unique symbol name in the kernel
//   image, declared only in `zircon/kernel/lib/kconcurrent/chainlock_transaction.cc`.
// - `DurationMonoTicks` is `#[repr(transparent)]` over `i64`, so the `extern "C"
//   fn(DurationMonoTicks)` signature has the exact same C ABI as the C++ `extern "C" void
//   rust_chainlock_update_contention_counters(zx_duration_mono_ticks_t)` (`int64_t`) declaration.
#[unsafe(no_mangle)]
pub extern "C" fn rust_chainlock_update_contention_counters(contention_ticks: DurationMonoTicks) {
    update_contention_counters(contention_ticks);
}

/// Kernel environment hooks for [`ChainLockTransaction`].
///
/// Adapts the environment-independent `concurrent` chain lock library to the Zircon kernel
/// environment.
pub struct KernelTransactionHooks;

// SAFETY:
// - `KernelTransactionHooks::active()` returns the exact pointer most recently passed to
//   `KernelTransactionHooks::set_active()` on the current CPU via
//   `PerCpu::get_current().active_cl_transaction()` (or `None` when null).
// - All kernel transaction state savers ensure interrupts are disabled (`IrqSave`,
//   `PreemptDisable`, and `EagerReschedDisable` disable interrupts via `arch_interrupt_save()`,
//   and `NoIrqSave::save_state()` enforces `assert!(ints_disabled())`) and keep interrupts
//   disabled while a transaction is registered. Because interrupts are disabled on the current CPU
//   while a transaction is active, neither same-CPU interrupt reentrancy nor cross-CPU thread
//   migration can occur while registered, ensuring the backing `PerCpu` slot is strictly local to
//   the current execution context.
unsafe impl TransactionHooks for KernelTransactionHooks {
    type ContentionState = AtomicU32;
    type CallsiteInfo = CallsiteInfo;
    type Accounting = SelectAccounting<CLT_DEBUG_CHECKS_ENABLED>;
    type Tracing = SelectTracing<CLT_TRACING_ENABLED, CallsiteInfo>;
    type ExtraState = Cell<u64>;

    #[inline]
    fn active() -> core::option::Option<NonNull<ChainLockTransaction>> {
        debug_assert!(ints_disabled());
        NonNull::new(PerCpu::get_current().active_cl_transaction())
    }

    /// Sets the active [`ChainLockTransaction`] pointer for the current CPU.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - Interrupts are disabled on the current CPU (`ints_disabled()`).
    /// - `transaction` is either `None` or points to a valid, initialized, stack-pinned
    ///   [`ChainLockTransaction`] owned by the current execution context that remains valid and
    ///   free of mutable aliases (with interrupts disabled) until cleared or replaced by a
    ///   subsequent call to `set_active`.
    #[inline]
    unsafe fn set_active(transaction: core::option::Option<NonNull<ChainLockTransaction>>) {
        debug_assert!(ints_disabled());
        let ptr = transaction.map_or(core::ptr::null_mut(), NonNull::as_ptr);
        // SAFETY: The caller of `TransactionHooks::set_active` guarantees that interrupts are
        // disabled on the current CPU and that `transaction` is either `None` or points to a live,
        // stack-pinned `ChainLockTransaction` without mutable aliases until cleared or replaced by
        // a subsequent `set_active` call.
        unsafe {
            PerCpu::get_current().set_active_cl_transaction(ptr);
        }
    }

    #[inline]
    fn yield_cpu() {
        core::hint::spin_loop();
    }

    #[inline]
    fn allocate_reserved_token() -> Token {
        debug_assert!(ints_disabled());
        ChainLockTransaction::reserved_token(curr_cpu_num() as usize)
    }

    #[inline]
    fn get_current_ticks() -> i64 {
        timer_current_mono_ticks().0
    }

    #[inline]
    fn on_finalized(contention_start_ticks: i64, callsite_info: CallsiteInfo) {
        // Note: `get_current_ticks()` returns monotonic ticks (`InstantMonoTicks`), which is
        // intentionally wrapped here as `InstantBootTicks` to match C++
        // `ChainLockTransaction::GetCurrentTicks()` / `OnFinalized()`.
        ktrace_macro::complete!(
            "kernel:sched",
            "lock_spin",
            InstantBootTicks(contention_start_ticks),
            "lock_id" => callsite_info.line_number,
            "lock_class" => callsite_info
                .label
                .unwrap_or_else(|| ktrace_macro::resolve_string!("<unnamed>")),
            "lock_type" => ktrace_macro::resolve_string!("ChainLockTransaction"),
            "backoff_count" => {
                assert!(ints_disabled());
                let active = ChainLockTransaction::active()
                    .expect("on_finalized called without active transaction");
                // SAFETY: `ints_disabled()` is asserted above and `active` was obtained from
                // `ChainLockTransaction::active()`, which by `PerCpu::active_cl_transaction`'s
                // safety invariant points to a valid, initialized, stack-pinned
                // `ChainLockTransaction` on the current CPU with no mutable aliases.
                unsafe { active.as_ref().backoff_count() }
            },
        );
        update_contention_counters(DurationMonoTicks(
            timer_current_mono_ticks().0.saturating_sub(contention_start_ticks),
        ));
    }

    #[inline]
    fn on_backoff(transaction: &ChainLockTransaction) {
        on_backoff_counters(transaction.backoff_count());
    }

    #[inline]
    fn record_backoff_or_retry(
        transaction: &ChainLockTransaction,
        observed_token: Token,
        lock_state: &AtomicToken,
        contention_state: &Self::ContentionState,
    ) -> bool {
        let percpu = PerCpu::get_current();
        let current_cpu = curr_cpu_num();
        let current_cpu_mask = cpu_num_to_mask(current_cpu);

        // Observe the conflict id for the current cpu and add this cpu to the contention state of
        // the chain lock instigating the backoff operation.
        let conflict_id = percpu.chain_lock_conflict_id.load(Ordering::Acquire);
        contention_state.fetch_or(current_cpu_mask, Ordering::AcqRel);

        // If the lock state has the same token after observing the conflict id, this transaction
        // needs to wait until the conflict is (potentially) resolved by the owner incrementing the
        // conflict id when releasing the lock.
        let current_token = lock_state.load(Ordering::Acquire);
        if current_token == observed_token {
            transaction.extra().set(conflict_id);
            return true;
        }

        false
    }

    #[inline]
    fn release_and_notify<F: FnOnce()>(
        _transaction: &ChainLockTransaction,
        contention_state: &Self::ContentionState,
        do_release: F,
    ) {
        // Sample the contention state before releasing the chainlock. The caller will ensure that
        // contention state will not be modified until after do_release is called.
        let mut wake_mask = contention_state.swap(0, Ordering::AcqRel);

        // Perform the actual chainlock release operation, allowing contenders to attempt to
        // acquire the lock. The contention state MUST NOT be accessed after the release to avoid
        // use-after-free hazards!!
        do_release();

        // Use the sampled contention state to poke any backed-off contenders.
        while wake_mask != 0 {
            let cpu = wake_mask.trailing_zeros();
            PerCpu::get(cpu).chain_lock_conflict_id.fetch_add(1, Ordering::Release);
            wake_mask &= wake_mask - 1;
        }
    }
}

/// Waits for the observed contention to be resolved on the given `percpu` that was latched before
/// IRQ/preemption state was restored during relax.
#[inline]
fn wait_for_conflict_resolution(transaction: &ChainLockTransaction, percpu: &PerCpu) {
    let conflict_id = transaction.extra().get();
    if conflict_id != 0 {
        // Wait for the contention to be resolved on the CPU where the contention by this
        // transaction was originally observed. That might be a different CPU than the current
        // thread is on, since the thread might be able to migrate during the relax phase of
        // transaction backoff.
        loop {
            core::hint::spin_loop();
            if percpu.chain_lock_conflict_id.load(Ordering::Acquire) != conflict_id {
                break;
            }
        }
        transaction.extra().set(0);
    } else {
        core::hint::spin_loop();
    }
}

/// State saver for [`NoIrqSave`].
#[repr(C)]
#[derive(Default)]
pub struct NoIrqSaveStateSaver {
    // Matches the 1-byte size of the empty C++ struct `StateSaver<StateOptions::NoIrqSave>`.
    _byte: u8,
    _not_send: PhantomData<*mut ()>,
}

zr::static_assert!(size_of::<NoIrqSaveStateSaver>() == 1);
zr::static_assert!(align_of::<NoIrqSaveStateSaver>() == 1);

impl StateSaver<ChainLockTransaction> for NoIrqSaveStateSaver {
    #[inline]
    fn relax(&mut self, transaction: &ChainLockTransaction) {
        wait_for_conflict_resolution(transaction, PerCpu::get_current());
    }
}

impl StateOption<ChainLockTransaction> for NoIrqSave {
    type Saver = NoIrqSaveStateSaver;

    #[inline]
    fn save_state(&self) -> Self::Saver {
        assert!(ints_disabled());
        NoIrqSaveStateSaver { _byte: 0, _not_send: PhantomData }
    }
}

/// State saver for [`IrqSave`].
#[repr(C)]
pub struct IrqSaveStateSaver {
    // Safety invariant: Holds the interrupt state saved when interrupts were disabled by this
    // saver (or most recently re-saved after `relax`).
    interrupt_state: InterruptSavedState,
    _not_send: PhantomData<*mut ()>,
}

zr::static_assert!(size_of::<IrqSaveStateSaver>() == size_of::<InterruptSavedState>());
zr::static_assert!(align_of::<IrqSaveStateSaver>() == align_of::<InterruptSavedState>());

impl IrqSaveStateSaver {
    /// Saves current interrupt state and disables interrupts.
    #[inline]
    fn new() -> Self {
        Self { interrupt_state: arch_interrupt_save(), _not_send: PhantomData }
    }
}

impl Drop for IrqSaveStateSaver {
    #[inline]
    fn drop(&mut self) {
        arch_interrupt_restore(self.interrupt_state);
    }
}

impl StateSaver<ChainLockTransaction> for IrqSaveStateSaver {
    #[inline]
    fn relax(&mut self, transaction: &ChainLockTransaction) {
        assert_eq!(ChainLockTransaction::active(), Some(NonNull::from(transaction)));
        // Must be latched before enabling interrupts so that the correct contention state is
        // monitored, even if the thread migrates during relax.
        let percpu = PerCpu::get_current();

        // SAFETY: `assert_eq!` above verified that `transaction` is currently the active
        // transaction on this CPU, and no `ChainLockTransactionToken` is used while `transaction`
        // is unregistered between here and `register()` below.
        unsafe {
            transaction.unregister();
        }
        arch_interrupt_restore(self.interrupt_state);

        wait_for_conflict_resolution(transaction, percpu);

        self.interrupt_state = arch_interrupt_save();
        // SAFETY: `transaction` was verified above to have been registered as the active
        // transaction prior to `unregister()` (so its caller upholds the stack-pinning invariant
        // for the duration of the transaction), interrupts are disabled by `arch_interrupt_save()`
        // above, and no transaction is currently registered on this CPU.
        let _ = unsafe { transaction.register() };
    }
}

impl StateOption<ChainLockTransaction> for IrqSave {
    type Saver = IrqSaveStateSaver;

    #[inline]
    fn save_state(&self) -> Self::Saver {
        IrqSaveStateSaver::new()
    }
}

/// State saver for [`PreemptDisable`].
#[repr(C)]
pub struct PreemptDisableStateSaver {
    // Safety invariant: Holds the interrupt state saved when interrupts were disabled by this
    // saver (or most recently re-saved after `relax`), while preemption is also disabled.
    interrupt_state: InterruptSavedState,
    _not_send: PhantomData<*mut ()>,
}

zr::static_assert!(size_of::<PreemptDisableStateSaver>() == size_of::<InterruptSavedState>());
zr::static_assert!(align_of::<PreemptDisableStateSaver>() == align_of::<InterruptSavedState>());

impl PreemptDisableStateSaver {
    /// Disables preemption and saves/disables interrupts.
    #[inline]
    fn new() -> Self {
        preempt_disable();
        let interrupt_state = arch_interrupt_save();
        Self { interrupt_state, _not_send: PhantomData }
    }
}

impl Drop for PreemptDisableStateSaver {
    #[inline]
    fn drop(&mut self) {
        arch_interrupt_restore(self.interrupt_state);
        preempt_reenable();
    }
}

impl StateSaver<ChainLockTransaction> for PreemptDisableStateSaver {
    #[inline]
    fn relax(&mut self, transaction: &ChainLockTransaction) {
        assert_eq!(ChainLockTransaction::active(), Some(NonNull::from(transaction)));
        // Must be latched before enabling interrupts/preemption so that the correct contention
        // state is monitored, even if the thread migrates during relax.
        let percpu = PerCpu::get_current();

        // SAFETY: `assert_eq!` above verified that `transaction` is currently the active
        // transaction on this CPU, and no `ChainLockTransactionToken` is used while `transaction`
        // is unregistered between here and `register()` below.
        unsafe {
            transaction.unregister();
        }
        arch_interrupt_restore(self.interrupt_state);
        preempt_reenable();

        wait_for_conflict_resolution(transaction, percpu);

        preempt_disable();
        self.interrupt_state = arch_interrupt_save();
        // SAFETY: `transaction` was verified above to have been registered as the active
        // transaction prior to `unregister()` (so its caller upholds the stack-pinning invariant
        // for the duration of the transaction), interrupts are disabled by `arch_interrupt_save()`
        // above, and no transaction is currently registered on this CPU.
        let _ = unsafe { transaction.register() };
    }
}

impl StateOption<ChainLockTransaction> for PreemptDisable {
    type Saver = PreemptDisableStateSaver;

    #[inline]
    fn save_state(&self) -> Self::Saver {
        PreemptDisableStateSaver::new()
    }
}

/// State saver for [`EagerReschedDisable`].
#[repr(C)]
pub struct EagerReschedDisableStateSaver {
    // Safety invariant: Holds the interrupt state saved when interrupts were disabled by this
    // saver (or most recently re-saved after `relax`), while eager rescheduling is also disabled.
    interrupt_state: InterruptSavedState,
    _not_send: PhantomData<*mut ()>,
}

zr::static_assert!(size_of::<EagerReschedDisableStateSaver>() == size_of::<InterruptSavedState>());
zr::static_assert!(
    align_of::<EagerReschedDisableStateSaver>() == align_of::<InterruptSavedState>()
);

impl EagerReschedDisableStateSaver {
    /// Disables eager rescheduling and saves/disables interrupts.
    #[inline]
    fn new() -> Self {
        eager_resched_disable();
        let interrupt_state = arch_interrupt_save();
        Self { interrupt_state, _not_send: PhantomData }
    }
}

impl Drop for EagerReschedDisableStateSaver {
    #[inline]
    fn drop(&mut self) {
        arch_interrupt_restore(self.interrupt_state);
        eager_resched_reenable();
    }
}

impl StateSaver<ChainLockTransaction> for EagerReschedDisableStateSaver {
    #[inline]
    fn relax(&mut self, transaction: &ChainLockTransaction) {
        assert_eq!(ChainLockTransaction::active(), Some(NonNull::from(transaction)));
        // Must be latched before enabling interrupts/eager resched so that the correct contention
        // state is monitored, even if the thread migrates during relax.
        let percpu = PerCpu::get_current();

        // SAFETY: `assert_eq!` above verified that `transaction` is currently the active
        // transaction on this CPU, and no `ChainLockTransactionToken` is used while `transaction`
        // is unregistered between here and `register()` below.
        unsafe {
            transaction.unregister();
        }
        arch_interrupt_restore(self.interrupt_state);
        eager_resched_reenable();

        wait_for_conflict_resolution(transaction, percpu);

        eager_resched_disable();
        self.interrupt_state = arch_interrupt_save();
        // SAFETY: `transaction` was verified above to have been registered as the active
        // transaction prior to `unregister()` (so its caller upholds the stack-pinning invariant
        // for the duration of the transaction), interrupts are disabled by `arch_interrupt_save()`
        // above, and no transaction is currently registered on this CPU.
        let _ = unsafe { transaction.register() };
    }
}

impl StateOption<ChainLockTransaction> for EagerReschedDisable {
    type Saver = EagerReschedDisableStateSaver;

    #[inline]
    fn save_state(&self) -> Self::Saver {
        EagerReschedDisableStateSaver::new()
    }
}

/// Scoped guard that saves state, starts and finalizes a [`ChainLockTransaction`], and acquires a
/// single [`ChainLock`].
#[repr(C)]
pub struct SingleChainLockGuard<'a, O: StateOption<ChainLockTransaction>> {
    // The order of construction/destruction of these members is important. State must be saved
    // before starting the transaction and the transaction must be started before acquiring the
    // lock with the guard. Likewise, the guard needs to be released before completing the
    // transaction and the transaction needs to be completed before restoring state.
    //
    // We use `ManuallyDrop` so that the struct field layout matches C++ (`state_saver_`,
    // `transaction_`, `guard_`) while `SingleChainLockScope::drop` explicitly destroys them in
    // reverse order (`guard`, `transaction`, `state_saver`).
    //
    // Safety invariant: Once initialized via `new_in_place`, all fields remain initialized and the
    // struct remains pinned in place until `SingleChainLockScope::drop` drops `guard`,
    // `transaction`, and `state_saver` in reverse order.
    state_saver: core::mem::ManuallyDrop<O::Saver>,
    transaction: core::mem::ManuallyDrop<ChainLockTransaction>,
    guard: core::mem::ManuallyDrop<ChainLockGuard<'a>>,
    token: ChainLockTransactionToken,
}

zr::static_assert!(offset_of!(SingleChainLockGuard<'static, IrqSave>, state_saver) == 0);
zr::static_assert!(offset_of!(SingleChainLockGuard<'static, IrqSave>, transaction) == 8);

impl<'a, O: StateOption<ChainLockTransaction>> SingleChainLockGuard<'a, O> {
    /// Initializes a [`SingleChainLockGuard`] in-place in `slot` and returns a
    /// [`SingleChainLockScope`] that drops it when leaving scope.
    ///
    /// Prefer using the [`single_chain_lock_guard!`] macro or [`Self::scope`].
    ///
    /// # Safety
    ///
    /// The caller must ensure that the returned [`SingleChainLockScope`] is dropped (and not
    /// passed to `core::mem::forget`) before `slot` is overwritten, moved, or deallocated.
    #[inline]
    pub unsafe fn new_in_place<'slot>(
        slot: &'slot mut MaybeUninit<Self>,
        option: O,
        lock: &'a ChainLock,
        callsite_info: CallsiteInfo,
    ) -> SingleChainLockScope<'slot, 'a, O> {
        let state_saver = option.save_state();
        let transaction = ChainLockTransaction::new_unregistered(callsite_info, 0, false);

        let base_ptr = slot.as_mut_ptr();
        // SAFETY:
        // - `base_ptr` is derived from `slot: &'slot mut MaybeUninit<Self>`, so it is non-null,
        //   properly aligned, and valid for writes to all fields of `Self`.
        // - `state_saver` and `transaction` are written into their pinned locations in `slot`
        //   before calling `register()`.
        // - By this function's `# Safety` contract, the caller guarantees that the returned
        //   `SingleChainLockScope` is dropped before `slot` is overwritten, moved, or deallocated,
        //   so `(*base_ptr).transaction` remains alive and pinned at its address in `slot` until
        //   `SingleChainLockScope::drop` drops it, discharging `register()`'s pinning
        //   precondition.
        // - `token` is written into `(*base_ptr).token` before borrowing `&(*base_ptr).token` for
        //   `ChainLockGuard::new` and `ChainLockTransaction::finalize`.
        // - After `register()`, only disjoint sibling fields (`token`, `guard`) are accessed via
        //   raw field projections and no `&mut Self` covering `transaction` is created while it is
        //   registered in `PerCpu`.
        // - `ChainLockGuard::new` and `ChainLockTransaction::finalize` are infallible and do not
        //   unwind once `transaction` is registered.
        unsafe {
            (&raw mut (*base_ptr).state_saver).write(core::mem::ManuallyDrop::new(state_saver));
            (&raw mut (*base_ptr).transaction).write(core::mem::ManuallyDrop::new(transaction));
            let token = (*base_ptr).transaction.register();
            (&raw mut (*base_ptr).token).write(token);

            let guard = ChainLockGuard::new(lock, &(*base_ptr).token);
            (&raw mut (*base_ptr).guard).write(core::mem::ManuallyDrop::new(guard));
            ChainLockTransaction::finalize(&(*base_ptr).token);

            SingleChainLockScope { ptr: NonNull::new_unchecked(base_ptr), _marker: PhantomData }
        }
    }

    /// Executes `f` while holding `lock` inside a finalized single-lock [`ChainLockTransaction`]
    /// configured with `option`.
    #[inline]
    pub fn scope<R, F: FnOnce(&Self) -> R>(
        option: O,
        lock: &'a ChainLock,
        callsite_info: CallsiteInfo,
        f: F,
    ) -> R {
        let mut slot = MaybeUninit::uninit();
        // SAFETY: `guard` is dropped at the end of this function before `slot` goes out of scope.
        let guard = unsafe { Self::new_in_place(&mut slot, option, lock, callsite_info) };
        f(&guard)
    }

    /// Returns a reference to the [`ChainLockTransactionToken`] for this active transaction.
    #[inline]
    pub const fn token(&self) -> &ChainLockTransactionToken {
        &self.token
    }

    /// Returns a reference to the active [`ChainLockTransaction`].
    #[inline]
    pub fn transaction(&self) -> &ChainLockTransaction {
        &self.transaction
    }

    /// Returns a reference to the underlying [`ChainLockGuard`].
    #[inline]
    pub fn guard(&self) -> &ChainLockGuard<'a> {
        &self.guard
    }
}

/// RAII scope handle returned by [`SingleChainLockGuard::new_in_place`].
#[must_use = "if unused, the SingleChainLockGuard will immediately unlock"]
pub struct SingleChainLockScope<'slot, 'a, O: StateOption<ChainLockTransaction>> {
    // Safety invariant: Points to a `SingleChainLockGuard` initialized in `'slot` by
    // `SingleChainLockGuard::new_in_place` whose fields remain valid and free of mutable aliases
    // until `drop`.
    ptr: NonNull<SingleChainLockGuard<'a, O>>,
    _marker: PhantomData<&'slot mut MaybeUninit<SingleChainLockGuard<'a, O>>>,
}

impl<'slot, 'a, O: StateOption<ChainLockTransaction>> Deref for SingleChainLockScope<'slot, 'a, O> {
    type Target = SingleChainLockGuard<'a, O>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        // SAFETY: By `SingleChainLockScope` and `SingleChainLockGuard`'s safety invariants,
        // `self.ptr` is properly aligned and points to an initialized `SingleChainLockGuard` valid
        // for `'slot`, no mutable references to it exist while `self` is alive, and any mutation
        // of `transaction` via `PerCpu::active_cl_transaction` occurs exclusively through
        // `Cell`/`UnsafeCell` interior mutability.
        unsafe { self.ptr.as_ref() }
    }
}

impl<'slot, 'a, O: StateOption<ChainLockTransaction>> Drop for SingleChainLockScope<'slot, 'a, O> {
    #[inline]
    fn drop(&mut self) {
        let base_ptr = self.ptr.as_ptr();
        // SAFETY:
        // - By `SingleChainLockScope` and `SingleChainLockGuard`'s safety invariants, each
        //   `ManuallyDrop` field at `base_ptr` was initialized in `new_in_place` and has not yet
        //   been dropped.
        // - We drop the fields in reverse construction order (`guard`, then `transaction`, then
        //   `state_saver`) by projecting directly from `base_ptr`:
        //   1. Dropping `(*base_ptr).guard` projects only to the disjoint `guard` field, avoiding
        //      any `&mut` reference covering `(*base_ptr).transaction` while `ChainLockGuard::drop`
        //      accesses `transaction` via `PerCpu::active_cl_transaction`.
        //   2. Dropping `(*base_ptr).transaction` runs after `guard` is dropped while interrupts
        //      remain disabled via `state_saver`; `ChainLockTransaction::drop` clears
        //      `PerCpu::active_cl_transaction` without dereferencing the old pointer.
        //   3. Dropping `(*base_ptr).state_saver` runs after `transaction` has been unregistered
        //      and dropped, safely restoring interrupt and preemption state.
        unsafe {
            core::mem::ManuallyDrop::drop(&mut (*base_ptr).guard);
            core::mem::ManuallyDrop::drop(&mut (*base_ptr).transaction);
            core::mem::ManuallyDrop::drop(&mut (*base_ptr).state_saver);
        }
    }
}

/// Acquires a single [`ChainLock`] inside a finalized [`ChainLockTransaction`] pinned on the
/// current stack frame for the enclosing lexical scope.
///
/// Because `$guard` is bound as a shared reference (`&SingleChainLockGuard`) rather than a
/// by-value RAII guard (to prevent safe callers from passing the stack-pinned scope guard to
/// [`core::mem::forget`]), the lock is always held for the entire enclosing lexical scope and
/// cannot be released early via `drop(guard)`. Use an inner block or
/// [`SingleChainLockGuard::scope`] if a shorter lock lifetime is needed.
///
/// # Example
/// ```ignore
/// single_chain_lock_guard!(guard, IRQ_SAVE_OPTION, &lock, clt_tag!("example"));
/// ```
#[macro_export]
macro_rules! single_chain_lock_guard {
    ($guard:ident, $option:expr, $lock:expr, $callsite_info:expr $(,)?) => {
        let __single_cl_guard_option = $option;
        let __single_cl_guard_lock = $lock;
        let __single_cl_guard_callsite_info = $callsite_info;
        let mut __single_cl_guard_slot = ::core::mem::MaybeUninit::uninit();
        // SAFETY: `__single_cl_guard_scope` is bound in the same lexical scope immediately after
        // `__single_cl_guard_slot` and cannot be named or passed to `core::mem::forget` by the
        // caller, ensuring it is dropped before `__single_cl_guard_slot` goes out of scope.
        let __single_cl_guard_scope = unsafe {
            $crate::kconcurrent::SingleChainLockGuard::new_in_place(
                &mut __single_cl_guard_slot,
                __single_cl_guard_option,
                __single_cl_guard_lock,
                __single_cl_guard_callsite_info,
            )
        };
        #[allow(unused_variables)]
        let $guard: &$crate::kconcurrent::SingleChainLockGuard<'_, _> = &__single_cl_guard_scope;
    };
}
pub use single_chain_lock_guard;

/// Tests for kernel [`ChainLock`] and [`ChainLockTransaction`] primitives.
#[cfg(ktest)]
#[unittest::suite(name = "kconcurrent_rust")]
mod tests {
    use core::sync::atomic::AtomicUsize;

    use concurrent::{Action, DONE, TransactionResult};
    use unittest::{assert_false, assert_true, expect_eq, expect_true};

    use crate::arch_rs::InterruptDisableGuard;
    use crate::kconcurrent::{
        EAGER_RESCHED_DISABLE_AND_IRQ_SAVE_OPTION, IRQ_SAVE_OPTION, NO_IRQ_SAVE_OPTION,
        PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION, SavedState, acquire_both_or_backoff, clt_tag,
    };
    use crate::kernel::mp::{MpIpiTarget, get_online_mask, sync_exec};
    use crate::kernel::thread::{eager_resched_disable_count, preempt_disable_count};

    /// Verifies type layouts and monotonic token generation.
    #[test]
    fn layout_and_token_generator() {
        expect_eq!(size_of::<ChainLock>(), 16);
        expect_eq!(align_of::<ChainLock>(), 8);
        expect_eq!(size_of::<ChainLockGuard<'_>>(), 16);
        expect_eq!(align_of::<ChainLockGuard<'_>>(), 8);
        expect_eq!(size_of::<SavedState>(), 16);
        expect_eq!(align_of::<SavedState>(), 8);
        expect_eq!(size_of::<NoIrqSaveStateSaver>(), 1);
        expect_eq!(size_of::<IrqSaveStateSaver>(), size_of::<InterruptSavedState>());
        expect_eq!(size_of::<PreemptDisableStateSaver>(), size_of::<InterruptSavedState>());
        expect_eq!(size_of::<EagerReschedDisableStateSaver>(), size_of::<InterruptSavedState>());

        let t1 = ChainLockTransaction::next_token();
        let t2 = ChainLockTransaction::next_token();
        expect_true!(t1.is_valid());
        expect_true!(!t1.is_reserved());
        expect_true!(t2 > t1);
    }

    /// Verifies IRQ and preemption state options across transactions.
    #[test]
    fn state_options_irq_and_preempt() {
        assert_false!(ints_disabled());
        let initial_preempt = preempt_disable_count();
        let initial_eager = eager_resched_disable_count();

        // 1. IrqSave
        let mut irq_disabled_in_tx = false;
        let mut active_in_tx = false;
        let mut preempt_in_tx = 0;
        let mut eager_in_tx = 0;
        let val = ChainLockTransaction::until_done(
            IRQ_SAVE_OPTION,
            clt_tag!("test_irq_save"),
            |_token| -> TransactionResult<u32> {
                irq_disabled_in_tx = ints_disabled();
                active_in_tx = ChainLockTransaction::active().is_some();
                preempt_in_tx = preempt_disable_count();
                eager_in_tx = eager_resched_disable_count();
                TransactionResult::Value(11)
            },
        );
        expect_true!(irq_disabled_in_tx);
        expect_true!(active_in_tx);
        expect_eq!(preempt_in_tx, initial_preempt);
        expect_eq!(eager_in_tx, initial_eager);
        expect_eq!(val, 11);
        assert_false!(ints_disabled());

        // 2. NoIrqSave (must manually disable interrupts first)
        {
            let _irq_guard = InterruptDisableGuard::new();
            assert_true!(ints_disabled());
            expect_true!(ChainLockTransaction::active().is_none());

            let mut no_irq_ok = false;
            ChainLockTransaction::until_done(
                NO_IRQ_SAVE_OPTION,
                clt_tag!("test_no_irq_save"),
                |_token| {
                    no_irq_ok = ints_disabled() && ChainLockTransaction::active().is_some();
                    DONE
                },
            );
            expect_true!(no_irq_ok);
            expect_true!(ChainLockTransaction::active().is_none());
            assert_true!(ints_disabled());
        }
        assert_false!(ints_disabled());

        // 3. PreemptDisable
        ChainLockTransaction::until_done(
            PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION,
            clt_tag!("test_preempt_disable"),
            |_token| {
                irq_disabled_in_tx = ints_disabled();
                preempt_in_tx = preempt_disable_count();
                eager_in_tx = eager_resched_disable_count();
                DONE
            },
        );
        expect_true!(irq_disabled_in_tx);
        expect_eq!(preempt_in_tx, initial_preempt + 1);
        expect_eq!(eager_in_tx, initial_eager);
        assert_false!(ints_disabled());
        expect_eq!(preempt_disable_count(), initial_preempt);

        // 4. EagerReschedDisable
        ChainLockTransaction::until_done(
            EAGER_RESCHED_DISABLE_AND_IRQ_SAVE_OPTION,
            clt_tag!("test_eager_resched_disable"),
            |_token| {
                irq_disabled_in_tx = ints_disabled();
                preempt_in_tx = preempt_disable_count();
                eager_in_tx = eager_resched_disable_count();
                DONE
            },
        );
        expect_true!(irq_disabled_in_tx);
        expect_eq!(preempt_in_tx, initial_preempt);
        expect_eq!(eager_in_tx, initial_eager + 1);
        assert_false!(ints_disabled());
        expect_eq!(eager_resched_disable_count(), initial_eager);
    }

    /// Verifies `SingleChainLockGuard` macro and scope helpers across state options.
    #[test]
    fn single_chain_lock_guard_variants() {
        let lock = ChainLock::new();
        assert_false!(ints_disabled());
        let initial_preempt = preempt_disable_count();
        let initial_eager = eager_resched_disable_count();

        {
            single_chain_lock_guard!(guard, IRQ_SAVE_OPTION, &lock, clt_tag!("single_irq_save"));
            expect_true!(ints_disabled());
            expect_true!(lock.is_held(guard.token()));
            if ChainLockTransaction::DEBUG_ACCOUNTING_ENABLED {
                expect_eq!(guard.transaction().locks_held(), 1);
                expect_true!(guard.transaction().is_finalized());
            }
        }
        assert_false!(ints_disabled());
        expect_true!(lock.is_unlocked());

        {
            let _irq = InterruptDisableGuard::new();
            single_chain_lock_guard!(
                guard,
                NO_IRQ_SAVE_OPTION,
                &lock,
                clt_tag!("single_no_irq_save")
            );
            expect_true!(ints_disabled());
            expect_true!(lock.is_held(guard.token()));
        }
        assert_false!(ints_disabled());
        expect_true!(lock.is_unlocked());

        let mut scope_ok = false;
        let res = SingleChainLockGuard::scope(
            PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION,
            &lock,
            clt_tag!("single_preempt"),
            |guard| {
                scope_ok = ints_disabled()
                    && preempt_disable_count() == initial_preempt + 1
                    && lock.is_held(guard.token());
                42u32
            },
        );
        expect_true!(scope_ok);
        expect_eq!(res, 42);
        assert_false!(ints_disabled());
        expect_eq!(preempt_disable_count(), initial_preempt);
        expect_true!(lock.is_unlocked());

        let mut eager_scope_ok = false;
        SingleChainLockGuard::scope(
            EAGER_RESCHED_DISABLE_AND_IRQ_SAVE_OPTION,
            &lock,
            clt_tag!("single_eager"),
            |guard| {
                eager_scope_ok = ints_disabled()
                    && eager_resched_disable_count() == initial_eager + 1
                    && lock.is_held(guard.token());
            },
        );
        expect_true!(eager_scope_ok);
        assert_false!(ints_disabled());
        expect_eq!(eager_resched_disable_count(), initial_eager);
        expect_true!(lock.is_unlocked());

        {
            let lock_ptr: *const ChainLock = &lock;
            // SAFETY: `lock_ptr` points to the live stack `lock`. Passing an `unsafe`
            // expression as `$lock` verifies `single_chain_lock_guard!` evaluates its
            // arguments outside its internal `unsafe` block without `unused_unsafe`.
            single_chain_lock_guard!(
                guard,
                IRQ_SAVE_OPTION,
                unsafe { &*lock_ptr },
                clt_tag!("single_unsafe_expr")
            );
            expect_true!(ints_disabled());
            expect_true!(lock.is_held(guard.token()));
        }
        assert_false!(ints_disabled());
        expect_true!(lock.is_unlocked());
    }

    /// Verifies chaining multiple locks and restarting a transaction.
    #[test]
    fn transaction_chain_and_restart() {
        let lock1 = ChainLock::new();
        let lock2 = ChainLock::new();
        let mut attempts = 0u32;
        let mut held_on_restart = false;

        let result = ChainLockTransaction::until_done(
            IRQ_SAVE_OPTION,
            clt_tag!("chain_and_restart"),
            |token| -> TransactionResult<u32> {
                attempts += 1;
                if attempts == 1 {
                    lock1.acquire_first_in_chain(token);
                    assert!(lock2.acquire_or_backoff(token));
                    ChainLockTransaction::finalize(token);
                    lock2.release(token);
                    return Action::Restart.into();
                }
                // On restart, lock1 is still held and transaction is unfinalized.
                held_on_restart = lock1.is_held(token);
                assert!(lock2.acquire_or_backoff(token));
                ChainLockTransaction::finalize(token);
                lock2.release(token);
                lock1.release(token);
                TransactionResult::Value(attempts)
            },
        );
        expect_true!(held_on_restart);
        expect_eq!(result, 2);
        expect_true!(lock1.is_unlocked());
        expect_true!(lock2.is_unlocked());
    }

    /// Verifies saving transaction state, switching to a per-CPU reserved token, and restoring.
    #[test]
    fn save_and_restore_state() {
        let lock = ChainLock::new();
        let mut orig_not_reserved = false;
        let mut saved_matches_orig = false;
        let mut reserved_is_reserved = false;
        let mut reserved_matches_cpu = false;
        let mut synced_to_reserved = false;
        let mut restored_matches_orig = false;
        let mut synced_to_orig = false;

        ChainLockTransaction::until_done(IRQ_SAVE_OPTION, clt_tag!("save_and_restore"), |token| {
            lock.acquire_first_in_chain(token);
            ChainLockTransaction::finalize(token);

            let orig_token = ChainLockTransaction::active_ref(token).token();
            orig_not_reserved = !orig_token.is_reserved();

            let saved = ChainLockTransaction::save_state_and_use_reserved_token(token);
            saved_matches_orig = saved.token() == orig_token;

            let reserved_token = ChainLockTransaction::active_ref(token).token();
            reserved_is_reserved = reserved_token.is_reserved();
            reserved_matches_cpu = reserved_token.value() == (curr_cpu_num() as u64) + 1;

            lock.sync_token(token);
            synced_to_reserved = lock.state_for_testing() == reserved_token;

            // Simulate context switch clearing active transaction and restoring it.
            // SAFETY: Interrupts are disabled via `IRQ_SAVE_OPTION`, `token` is not used before
            // `restore_state(saved)` on the next line, and passing `None` clears the active
            // transaction pointer.
            unsafe { KernelTransactionHooks::set_active(None) };
            // SAFETY: `saved` was produced by `save_state_and_use_reserved_token` for the
            // enclosing `until_done` transaction (which remains alive and stack-pinned for the
            // duration of `until_done`), interrupts are disabled, and the active transaction slot
            // was cleared above.
            unsafe { ChainLockTransaction::restore_state(saved) };

            restored_matches_orig = ChainLockTransaction::active_ref(token).token() == orig_token;
            lock.sync_token(token);
            synced_to_orig = lock.state_for_testing() == orig_token;

            lock.release(token);
            DONE
        });

        expect_true!(orig_not_reserved);
        expect_true!(saved_matches_orig);
        expect_true!(reserved_is_reserved);
        expect_true!(reserved_matches_cpu);
        expect_true!(synced_to_reserved);
        expect_true!(restored_matches_orig);
        expect_true!(synced_to_orig);
    }

    /// Verifies backoff recording, `chain_lock_conflict_id` latching, and conflict resolution.
    #[test]
    fn backoff_and_conflict_resolution() {
        // Custom StateOption that simulates the lock owner releasing the lock and incrementing
        // `chain_lock_conflict_id` during `relax()`.
        struct SimulatedOwnerOption<'a> {
            lock: &'a ChainLock,
            owner_tx: &'a ChainLockTransaction,
            relaxed: &'a Cell<bool>,
        }

        struct SimulatedOwnerSaver<'a> {
            inner: IrqSaveStateSaver,
            lock: &'a ChainLock,
            owner_tx: &'a ChainLockTransaction,
            relaxed: &'a Cell<bool>,
        }

        impl<'a> StateSaver<ChainLockTransaction> for SimulatedOwnerSaver<'a> {
            fn relax(&mut self, transaction: &ChainLockTransaction) {
                // Verify that `record_backoff_or_retry` latched the current CPU's non-zero
                // `chain_lock_conflict_id`.
                let latched_conflict_id = transaction.extra().get();
                assert_ne!(latched_conflict_id, 0);
                assert_eq!(self.lock.state_for_testing(), self.owner_tx.token());

                // Temporarily switch the active transaction to `owner_tx` while interrupts remain
                // disabled so `owner_tx` can release `self.lock` and notify contenders via
                // `release_and_notify`.
                // SAFETY: `transaction` is currently the active transaction on this CPU and no
                // token for `transaction` is used until `transaction.register()` below.
                unsafe { transaction.unregister() };
                // SAFETY: `self.owner_tx` is pinned on the enclosing test stack frame and
                // interrupts are disabled on this CPU.
                let owner_token = unsafe { self.owner_tx.register() };
                self.lock.release(&owner_token);
                // SAFETY: `self.owner_tx` is currently the active transaction and `owner_token` is
                // not used after this point.
                unsafe { self.owner_tx.unregister() };
                // SAFETY: `transaction` is pinned on `until_done`'s stack frame and interrupts are
                // disabled on this CPU.
                let _ = unsafe { transaction.register() };

                assert_ne!(
                    PerCpu::get_current().chain_lock_conflict_id.load(Ordering::Relaxed),
                    latched_conflict_id
                );
                self.relaxed.set(true);

                // Now delegate to `IrqSaveStateSaver::relax`, which will see that
                // `chain_lock_conflict_id` has changed and return immediately.
                self.inner.relax(transaction);
                assert_eq!(transaction.extra().get(), 0);
            }
        }

        impl<'a> StateOption<ChainLockTransaction> for SimulatedOwnerOption<'a> {
            type Saver = SimulatedOwnerSaver<'a>;

            fn save_state(&self) -> Self::Saver {
                SimulatedOwnerSaver {
                    inner: IrqSaveStateSaver::new(),
                    lock: self.lock,
                    owner_tx: self.owner_tx,
                    relaxed: self.relaxed,
                }
            }
        }

        let lock = ChainLock::new();
        // Allocate and acquire `lock` with a higher-priority transaction BEFORE the contender
        // transaction starts so `owner_tx.token() < contender_tx.token()`.
        let mut owner_tx = core::mem::ManuallyDrop::new(ChainLockTransaction::new_unregistered(
            clt_tag!("owner_tx"),
            0,
            false,
        ));
        {
            let _irq = InterruptDisableGuard::new();
            // SAFETY: `owner_tx` remains alive and pinned at its stack location for the duration
            // of this test, and interrupts are disabled.
            let owner_token = unsafe { owner_tx.register() };
            lock.acquire_first_in_chain(&owner_token);
            ChainLockTransaction::finalize(&owner_token);
            // SAFETY: `owner_tx` is currently the active transaction and `owner_token` is not used
            // after this point.
            unsafe { owner_tx.unregister() };
        }
        let relaxed = Cell::new(false);
        let mut observed_backoffs = 0u64;

        let before_backoffs = CHAINLOCK_BACKOFFS.sum_across_all_cpus();

        ChainLockTransaction::until_done(
            SimulatedOwnerOption { lock: &lock, owner_tx: &owner_tx, relaxed: &relaxed },
            clt_tag!("backoff_conflict_resolution"),
            |token| -> TransactionResult<()> {
                if !lock.acquire_or_backoff(token) {
                    return Action::Backoff.into();
                }
                observed_backoffs = ChainLockTransaction::active_ref(token).backoff_count();
                ChainLockTransaction::finalize(token);
                lock.release(token);
                TransactionResult::DONE
            },
        );

        {
            let _irq = InterruptDisableGuard::new();
            // SAFETY: `owner_tx` is still alive at its original stack location and interrupts are
            // disabled; we re-register it before dropping it in-place so
            // `ChainLockTransaction::drop` unregisters it cleanly.
            unsafe {
                let _ = owner_tx.register();
                core::mem::ManuallyDrop::drop(&mut owner_tx);
            }
        }

        expect_eq!(observed_backoffs, 1);
        expect_true!(relaxed.get());
        expect_true!(lock.is_unlocked());
        expect_true!(CHAINLOCK_BACKOFFS.sum_across_all_cpus() > before_backoffs);
    }

    /// Verifies multi-CPU contention and wound-wait deadlock avoidance across online CPUs.
    #[test]
    fn multi_cpu_contention() {
        let online_mask = get_online_mask();
        if online_mask.count_ones() < 2 {
            return true;
        }

        let lock_a = ChainLock::new();
        let lock_b = ChainLock::new();
        let counter_a = AtomicUsize::new(0);
        let counter_b = AtomicUsize::new(0);

        sync_exec(MpIpiTarget::Mask, online_mask, || {
            let cpu = curr_cpu_num();
            for _ in 0..50 {
                // Note: `sync_exec` runs with interrupts disabled in IPI context, so we use
                // `NO_IRQ_SAVE_OPTION`.
                ChainLockTransaction::until_done(
                    NO_IRQ_SAVE_OPTION,
                    clt_tag!("multi_cpu_contention"),
                    |token| -> TransactionResult<()> {
                        let acquired = if (cpu & 1) == 0 {
                            acquire_both_or_backoff(&lock_a, &lock_b, token)
                        } else {
                            acquire_both_or_backoff(&lock_b, &lock_a, token)
                        };
                        if !acquired {
                            return Action::Backoff.into();
                        }
                        ChainLockTransaction::finalize(token);

                        counter_a.fetch_add(1, Ordering::Relaxed);
                        counter_b.fetch_add(1, Ordering::Relaxed);

                        lock_b.release(token);
                        lock_a.release(token);
                        TransactionResult::DONE
                    },
                );
            }
        });

        let expected = (online_mask.count_ones() as usize) * 50;
        expect_eq!(counter_a.load(Ordering::Relaxed), expected);
        expect_eq!(counter_b.load(Ordering::Relaxed), expected);
        expect_true!(lock_a.is_unlocked());
        expect_true!(lock_b.is_unlocked());
    }
}
