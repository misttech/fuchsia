// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Synchronization primitive trait definitions for netstack3.

use netstack3_sync::once;

/// Synchronization primitive types provided by bindings.
pub trait BindingsSyncTypes {
    /// The raw mutex implementation used by [`netstack3_sync::Mutex`].
    type Mutex: lock_api::RawMutex + Send + Sync + 'static;
    /// The raw reader-writer lock implementation used by [`netstack3_sync::RwLock`].
    type RwLock: lock_api::RawRwLock + Send + Sync + 'static;
    /// The one-time initialization primitive used by [`netstack3_sync::OnceLock`].
    type Once: once::Once + Send + Sync + 'static;
}

/// Concrete synchronization primitive implementations for tests.
///
/// This module provides default raw lock and `Once` types so fake bindings
/// contexts and tests can implement [`BindingsSyncTypes`] without duplicating
/// platform-specific lock selection.
#[cfg(any(test, feature = "testutils"))]
pub mod lock_impl {
    extern crate std;

    pub use fuchsia_sync::{RawMutex, RawRwLock};

    /// Default [`netstack3_sync::once::Once`] implementation backed by
    /// [`std::sync::Once`].
    #[derive(Default)]
    pub struct Once(std::sync::Once);

    // SAFETY: `std::sync::Once` upholds all the guarantees required by
    // `netstack3_sync::once::Once`.
    unsafe impl super::once::Once for Once {
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
}
