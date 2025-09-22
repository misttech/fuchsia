// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::state_machine::{rcu_drop, rcu_synchronize};

/// A scope that ensures that the client calls `rcu_synchronize` eventually.
#[derive(Default)]
pub struct RcuWriteScope {}

impl RcuWriteScope {
    /// Synchronize the RCU state machine.
    ///
    /// This function blocks until the RCU state machine has made sufficient progress to ensure
    /// that no concurrent readers are holding read guards.
    ///
    /// Equivalent to dropping the scope.
    pub fn sync(self) {
        std::mem::drop(self);
    }

    /// Schedule the object to be dropped after all in-flight read operations have completed.
    pub fn drop<T: Send + Sync + 'static>(&self, value: T) {
        rcu_drop(value);
    }

    /// Schedule a pointer to a Box to be dropped after all in-flight read operations have completed.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the pointer was originally created by `Box::into_raw`.
    pub unsafe fn drop_box<T: Send + Sync + 'static>(&self, ptr: *mut T) {
        rcu_drop(Box::from_raw(ptr));
    }
}

impl Drop for RcuWriteScope {
    fn drop(&mut self) {
        rcu_synchronize();
    }
}
