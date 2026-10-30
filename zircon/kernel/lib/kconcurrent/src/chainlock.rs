// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel [`ChainLock`] declarations, callsite tags, state option tags, and type aliases.

use core::mem::offset_of;

use concurrent::TransactionOption as Option;

use crate::ktrace_rs::InternedString;

use super::chainlock_proof::ClassedChainLock;
use super::chainlock_transaction::KernelTransactionHooks;

/// Produce trace records documenting the CLT contention overhead if we have lock contention
/// tracing enabled.
pub const CLT_TRACING_ENABLED: bool = cfg!(scheduler_lock_spin_tracing_enabled);

/// Enable debug lock accounting and state assertions when `LK_DEBUGLEVEL >= 2`.
pub const CLT_DEBUG_CHECKS_ENABLED: bool = cfg!(lk_debuglevel = "2");

/// Stores callsite info for the current transaction used in debugging and tracing.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct CallsiteInfo {
    /// Optional reference to the FXT interned string literal identifying the callsite.
    pub label: core::option::Option<&'static InternedString>,
    /// Source line number of the callsite.
    pub line_number: u32,
}

impl core::fmt::Debug for CallsiteInfo {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CallsiteInfo")
            .field("label", &self.label.map(core::ptr::from_ref))
            .field("line_number", &self.line_number)
            .finish()
    }
}

zr::static_assert!(size_of::<CallsiteInfo>() == 16);
zr::static_assert!(align_of::<CallsiteInfo>() == 8);
zr::static_assert!(offset_of!(CallsiteInfo, label) == 0);
zr::static_assert!(offset_of!(CallsiteInfo, line_number) == 8);

/// Returns a [`CallsiteInfo`] instance for the given string literal label at the current source
/// location.
#[macro_export]
macro_rules! clt_tag {
    ($label:literal) => {{
        $crate::kconcurrent::CallsiteInfo {
            label: ::core::option::Option::Some($crate::ktrace_rs::resolve_string!($label)),
            line_number: line!(),
        }
    }};
}
pub use clt_tag;

/// Concrete kernel `ChainLockTransaction` type.
pub type ChainLockTransaction = concurrent::ChainLockTransaction<KernelTransactionHooks>;

/// Concrete kernel `ChainLockTransactionToken` type.
pub type ChainLockTransactionToken = concurrent::ChainLockTransactionToken<ChainLockTransaction>;

/// Concrete kernel `SavedState` type.
pub type SavedState = concurrent::SavedState<KernelTransactionHooks>;

/// Concrete kernel `ChainLock` type.
pub type ChainLock = concurrent::ChainLock<ChainLockTransaction>;

/// Concrete kernel `ChainLockGuard` type.
pub type ChainLockGuard<'a> = concurrent::ChainLockGuard<'a, ChainLockTransaction>;

zr::static_assert!(size_of::<SavedState>() == 16);
zr::static_assert!(align_of::<SavedState>() == 8);
zr::static_assert!(size_of::<ChainLock>() == 16);
zr::static_assert!(align_of::<ChainLock>() == 8);
zr::static_assert!(size_of::<ChainLockGuard<'static>>() == 16);
zr::static_assert!(align_of::<ChainLockGuard<'static>>() == 8);

#[cold]
#[inline(never)]
#[track_caller]
fn panic_lock_already_held() -> ! {
    panic!("ChainLock is already held by the active transaction on this CPU");
}

#[cold]
#[inline(never)]
#[track_caller]
fn panic_transaction_already_active() -> ! {
    panic!("ChainLockTransaction is already active on this CPU");
}

/// Asserts that `lock` is not currently held by the active transaction on the current CPU.
///
/// # Safety-usable invariant
///
/// If this function returns normally, `lock` is not currently held by an active
/// [`ChainLockTransaction`] on the current CPU.
#[inline(always)]
#[track_caller]
pub fn assert_lock_not_held<C>(lock: &ClassedChainLock<C>) {
    if crate::arch_rs::ints_disabled() {
        let active = crate::kernel::percpu::PerCpu::get_current().active_cl_transaction();
        if !active.is_null() {
            // SAFETY: `ints_disabled()` is true and `active` is non-null, so by
            // `PerCpu::active_cl_transaction`'s safety invariant `active` points to the live,
            // stack-pinned `ChainLockTransaction` on this CPU with no mutable aliases.
            let tx_token = unsafe { (*active).token() };
            if lock.is_held_by_token(tx_token) {
                panic_lock_already_held();
            }
        }
    }
}

/// Asserts in debug builds that `lock` is not currently held by the active transaction on the
/// current CPU.
///
/// Unlike [`assert_lock_not_held`], this provides no guarantee in release builds, so it is only
/// suitable where the condition is already an obligation of the caller (for example, a
/// precondition stated in an `unsafe fn`'s `# Safety` section).
#[inline(always)]
#[track_caller]
pub fn debug_assert_lock_not_held<C>(lock: &ClassedChainLock<C>) {
    if cfg!(debug_assertions) {
        assert_lock_not_held(lock);
    }
}

/// Asserts in debug builds that a [`ChainLockTransaction`] is currently active on the current CPU.
#[inline(always)]
#[track_caller]
pub fn assert_transaction_active(_clt_token: &ChainLockTransactionToken) {
    if cfg!(debug_assertions) {
        let ints_off = crate::arch_rs::ints_disabled();
        if !ints_off
            || crate::kernel::percpu::PerCpu::get_current().active_cl_transaction().is_null()
        {
            super::chainlock_transaction::panic_no_active_transaction(ints_off);
        }
    }
}

/// Asserts that no [`ChainLockTransaction`] is currently active on the current CPU.
///
/// # Safety-usable invariant
///
/// If this function returns normally, no [`ChainLockTransaction`] is currently active on the
/// current CPU.
#[inline(always)]
#[track_caller]
pub fn assert_no_transaction_active() {
    if crate::arch_rs::ints_disabled()
        && !crate::kernel::percpu::PerCpu::get_current().active_cl_transaction().is_null()
    {
        panic_transaction_already_active();
    }
}

/// Asserts in debug builds that no [`ChainLockTransaction`] is active on the current CPU.
///
/// Unlike [`assert_no_transaction_active`], this provides no guarantee in release builds, so it is
/// only suitable where the condition is already an obligation of the caller (for example, a
/// precondition stated in an `unsafe fn`'s `# Safety` section).
#[inline(always)]
#[track_caller]
pub fn debug_assert_no_transaction_active() {
    if cfg!(debug_assertions) {
        assert_no_transaction_active();
    }
}

/// Acquires both `lock1` and `lock2`, returning `true` if both were acquired or `false` if neither
/// stays held and the caller should back off.
#[inline(always)]
#[must_use]
pub fn acquire_both_or_backoff(
    lock1: &ChainLock,
    lock2: &ChainLock,
    token: &ChainLockTransactionToken,
) -> bool {
    concurrent::acquire_both_or_backoff(lock1, lock2, token)
}

/// State option tag specifying that interrupts are already disabled and should not be saved or
/// restored by the transaction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NoIrqSave;

/// State option tag specifying that interrupts should be saved and disabled for the transaction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IrqSave;

/// State option tag specifying that preemption and interrupts should be disabled for the
/// transaction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PreemptDisable;

/// State option tag specifying that eager rescheduling and interrupts should be disabled for the
/// transaction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EagerReschedDisable;

/// Module grouping the transaction state option types.
pub mod state_options {
    pub use super::{EagerReschedDisable, IrqSave, NoIrqSave, PreemptDisable};
}

/// Constant for [`NoIrqSave`] passed to [`ChainLockTransaction::until_done`].
pub const NO_IRQ_SAVE_OPTION: Option<NoIrqSave> = Option::new(NoIrqSave);

/// Constant for [`IrqSave`] passed to [`ChainLockTransaction::until_done`].
pub const IRQ_SAVE_OPTION: Option<IrqSave> = Option::new(IrqSave);

/// Constant for [`PreemptDisable`] passed to [`ChainLockTransaction::until_done`].
pub const PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION: Option<PreemptDisable> = Option::new(PreemptDisable);

/// Constant for [`EagerReschedDisable`] passed to [`ChainLockTransaction::until_done`].
pub const EAGER_RESCHED_DISABLE_AND_IRQ_SAVE_OPTION: Option<EagerReschedDisable> =
    Option::new(EagerReschedDisable);
