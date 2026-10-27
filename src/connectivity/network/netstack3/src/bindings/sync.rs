// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Synchronization primitives for Netstack3 bindings.

use fuchsia_sync::{RawMutex, RawRwLock};
use netstack3_core::BindingsSyncTypes;

use crate::bindings::BindingsCtx;

impl BindingsSyncTypes for BindingsCtx {
    type Mutex = RawMutex;
    type RwLock = RawRwLock;
    type Once = StdOnce;
}

/// A [`netstack3_core::sync::once::Once`] implementation backed by
/// [`std::sync::Once`].
#[derive(Default)]
pub(crate) struct StdOnce(std::sync::Once);

// SAFETY: `std::sync::Once` upholds all the guarantees required by
// `netstack3_core::sync::once::Once`.
unsafe impl netstack3_core::sync::Once for StdOnce {
    #[inline]
    fn once(&self, f: impl FnOnce()) {
        let Self(once) = self;
        once.call_once(f);
    }

    #[inline]
    fn is_completed(&self) -> bool {
        let Self(once) = self;
        once.is_completed()
    }
}

/// A mutex backed by [`fuchsia_sync::RawMutex`].
pub(crate) type CoreMutex<T> = netstack3_core::sync::Mutex<T, RawMutex>;

/// Lock guard for access to a [`CoreMutex`].
pub(crate) type CoreMutexGuard<'a, T> = netstack3_core::sync::LockGuard<'a, T, RawMutex>;

/// A reader-writer lock backed by [`fuchsia_sync::RawRwLock`].
pub(crate) type CoreRwLock<T> = netstack3_core::sync::RwLock<T, RawRwLock>;
