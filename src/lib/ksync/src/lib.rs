// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]

#[cfg(test)]
extern crate self as ksync;

pub use kstring::declare_interned_string;
pub use ksync_macro::guarded;
pub use pin_init;

/// Locks a mutex.
///
/// Usage:
///   `ksync::lock!(let mut guard = self.lock_mu());`
///   Locks the mutex and binds a mutable pin to `guard`. Useful when you need to mutate
///   guarded fields via `guard.as_mut().fields_mut()`.
///
///   `ksync::lock!(let guard = self.lock_mu());`
///   Locks the mutex and binds an immutable pin to `guard`. Useful for read-only access
///   to guarded fields via `guard.fields()`.
///
///   `ksync::lock!(self.lock_mu());`
///   Locks the mutex and keeps it locked until the end of the scope, without binding the guard.
#[macro_export]
macro_rules! lock {
    (let mut $guard:ident = $lock_init:expr) => {
        $crate::pin_init::stack_pin_init!(let $guard = $lock_init);
        let mut $guard = $guard;
    };
    (let $guard:ident = $lock_init:expr) => {
        $crate::pin_init::stack_pin_init!(let $guard = $lock_init);
    };
    ($lock_init:expr) => {
        $crate::pin_init::stack_pin_init!(let _guard = $lock_init);
    };
}

mod kcell;
mod kmutex;
mod lock_token;
mod raw_lock;

#[cfg(not(feature = "kernel"))]
mod raw_userspace_mutex;

#[cfg(feature = "kernel")]
mod raw_kernel_event;
#[cfg(feature = "kernel")]
mod raw_kernel_mutex;
#[cfg(feature = "kernel")]
mod raw_spin_lock;

pub use kcell::{KCell, KCellInit, kcell_init};
pub use kmutex::{KMutex, KMutexGuard};
pub use lock_token::LockToken;
pub use lockdep::{LockClass, LockClassRegistration};
pub use raw_lock::RawLock;

#[cfg(not(feature = "kernel"))]
pub use raw_userspace_mutex::RawMutex;

#[cfg(feature = "kernel")]
pub use raw_kernel_event::{KEvent, RawEvent};
#[cfg(feature = "kernel")]
pub use raw_kernel_mutex::{RawCriticalMutex, RawMutex};
#[cfg(feature = "kernel")]
pub use raw_spin_lock::{InterruptSavedState, RawSpinlock};

#[cfg(feature = "kernel")]
pub type KSpinlock<Class> = KMutex<Class, RawSpinlock>;
