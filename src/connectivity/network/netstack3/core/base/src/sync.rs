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
