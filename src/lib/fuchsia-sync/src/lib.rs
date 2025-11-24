// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Fuchsia-native synchronization primitives.

#[cfg(target_os = "fuchsia")]
mod condvar;
#[cfg(target_os = "fuchsia")]
mod mutex;
#[cfg(target_os = "fuchsia")]
mod rwlock;

#[cfg(target_os = "fuchsia")]
pub use condvar::*;
#[cfg(target_os = "fuchsia")]
pub use mutex::*;
#[cfg(target_os = "fuchsia")]
pub use rwlock::*;

#[cfg(not(target_os = "fuchsia"))]
pub use parking_lot::{
    Condvar, MappedMutexGuard, MappedRwLockReadGuard, MappedRwLockWriteGuard, Mutex, MutexGuard,
    RawMutex as RawSyncMutex, RawRwLock as RawSyncRwLock, RwLock, RwLockReadGuard,
    RwLockWriteGuard,
};

/// Prevent potential deadlocks from panicking when lock cycle detection is enabled. This will
/// cause them to print instead of exiting the process.
pub fn suppress_lock_cycle_panics() {
    #[cfg(detect_lock_cycles)]
    tracing_mutex::suppress_panics();
}
