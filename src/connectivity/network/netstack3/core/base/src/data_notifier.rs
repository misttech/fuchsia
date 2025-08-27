// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Traits and types for a "data availability" notification mechanism.

use core::fmt::Debug;

/// Trait defining the `Notifier` type provided by Bindings to Core that allows
/// Core to notify Bindings when data is available for some networking resource
/// (e.g., a socket).
pub trait DataNotifierTypes {
    /// The type of a notifier that can be used to signal to a receiver that data is
    /// available.
    type Notifier: DataNotifier;
}

/// A handle to a notifier that can be used to signal to a receiver that data is
/// available.
///
/// Notifiers can be cloned to allow for multiple current notifiers.
pub trait DataNotifier: Debug + Clone + Send + Sync {
    /// Notify the receiver that data is available.
    fn notify(&self);
}

impl<D: DataNotifier> DataNotifier for Option<D> {
    fn notify(&self) {
        if let Some(this) = self {
            this.notify();
        }
    }
}

#[cfg(any(test, feature = "testutils"))]
pub mod testutil {
    use super::*;

    impl DataNotifier for () {
        fn notify(&self) {}
    }
}
