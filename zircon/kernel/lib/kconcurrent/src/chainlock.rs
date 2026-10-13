// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel [`ChainLock`] declarations, callsite tags, state option tags, and type aliases.

use core::mem::offset_of;

use concurrent::TransactionOption as Option;

use crate::ktrace_rs::InternedString;

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

/// Acquires both `lock1` and `lock2`, returning `true` if both were acquired or `false` if neither
/// stays held and the caller should back off.
#[inline]
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
