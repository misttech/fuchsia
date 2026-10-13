// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel-specific implementation of [`ChainLock`], [`ChainLockTransaction`], and related types
//! adapting the environment-independent `concurrent` crate to the Zircon kernel environment.

mod chainlock;
mod chainlock_transaction;

pub use chainlock::*;
pub use chainlock_transaction::*;
pub use concurrent::{
    Action, AllowFinalized, AtomicToken, ChainLockResult, DONE, DoneType, IntoTransactionResult,
    RecordBackoff, SelectAccounting, SelectTracing, StateOption, StateSaver, Token, Transaction,
    TransactionHooks, TransactionOption as Option, TransactionResult, TransactionState as State,
};
