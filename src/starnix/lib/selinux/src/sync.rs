// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/// `LockDepMutex`, `LockDepRwLock`, and lock level types exercised in the `selinux` crate when
/// built for integration with starnix.
#[cfg(feature = "selinux_starnix")]
pub(super) use starnix_sync::{
    LockDepMutex, LockDepRwLock, SeLinuxQueryCacheResetLock, SeLinuxSecurityServerStateLock,
    SeLinuxStatusLock,
};

/// Lock level placeholder types and lock aliases exercised in the `selinux` crate when
/// built for non-fuchsia platforms.
#[cfg(not(feature = "selinux_starnix"))]
pub(super) enum SeLinuxQueryCacheResetLock {}

#[cfg(not(feature = "selinux_starnix"))]
pub(super) enum SeLinuxSecurityServerStateLock {}

#[cfg(not(feature = "selinux_starnix"))]
pub(super) enum SeLinuxStatusLock {}

#[cfg(not(feature = "selinux_starnix"))]
pub(super) type LockDepMutex<T, _L> = parking_lot::Mutex<T>;

#[cfg(not(feature = "selinux_starnix"))]
pub(super) type LockDepRwLock<T, _L> = parking_lot::RwLock<T>;
