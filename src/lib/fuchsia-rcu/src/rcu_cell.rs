// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::rcu_ptr::{RcuPtr, RcuReadGuard};
use crate::rcu_write_scope::RcuWriteScope;
use crate::state_machine::rcu_synchronize;

/// An RCU (Read-Copy-Update) version of `Cell`.
///
/// This Cell can be read from multiple threads concurrently without blocking.
/// When the Cell is written, reads may continue to see the old value of the Cell
/// for some period of time.
pub struct RcuCell<T: Send + Sync + 'static> {
    ptr: RcuPtr<T>,
}

impl<T: Send + Sync + 'static> RcuCell<T> {
    /// Create a new RCU Cell from a value.
    pub fn new(data: T) -> Self {
        Self::from(Box::new(data))
    }

    /// Read the value of the RCU Cell.
    ///
    /// The object referenced by the RCU Cell will remain valid until the `RcuReadGuard` is dropped.
    /// However, another thread running concurrently might see a different value for the object.
    pub fn read(&self) -> RcuReadGuard<T> {
        self.ptr.read()
    }

    /// Write the value of the RCU Cell.
    ///
    /// Blocks until all concurrent readers have dropped their read guards.
    pub fn set_sync(&self, data: T) {
        self.replace_sync(Box::into_raw(Box::new(data)));
    }

    /// Write the value of the RCU Cell.
    ///
    /// Concurrent readers may continue to see the old value of the Cell until the RCU state machine
    /// has made sufficient progress to ensure that no concurrent readers are holding read guards.
    pub fn set_deferred(&self, scope: &RcuWriteScope, data: T) {
        let new_ptr = Box::into_raw(Box::new(data));
        // SAFETY: `scope.drop` defers the drop of the object until the RCU state machine has made
        // sufficient progress to ensure that no concurrent readers are holding read guards.
        let value = unsafe { self.replace(new_ptr) };
        scope.drop(value);
    }

    #[must_use]
    /// Replace the pointer in the RCU Cell with a new pointer.
    ///
    /// SAFETY: The caller must defer the drop of the object until the RCU state machine has made
    /// sufficient progress to ensure that no concurrent readers are holding read guards.
    unsafe fn replace(&self, ptr: *mut T) -> Box<T> {
        let old_ptr = self.ptr.replace(ptr);
        Box::from_raw(old_ptr)
    }

    fn replace_sync(&self, ptr: *mut T) {
        let value = unsafe { self.replace(ptr) };
        rcu_synchronize();
        std::mem::drop(value);
    }
}

impl<T: Send + Sync + 'static> Drop for RcuCell<T> {
    fn drop(&mut self) {
        self.replace_sync(std::ptr::null_mut());
    }
}

impl<T: Send + Sync + 'static> From<Box<T>> for RcuCell<T> {
    fn from(value: Box<T>) -> Self {
        Self { ptr: RcuPtr::new(Box::into_raw(value)) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ops::Deref;

    #[test]
    fn test_rcu_cell() {
        let value = RcuCell::new(42);
        assert_eq!(value.read().deref(), &42);
    }

    #[test]
    fn test_rcu_cell_set_deferred() {
        let value = RcuCell::new(42);
        let scope = RcuWriteScope::default();
        value.set_deferred(&scope, 43);
        assert_eq!(value.read().deref(), &43);
    }

    #[test]
    fn test_rcu_cell_set_sync() {
        let value = RcuCell::new(42);
        value.set_sync(43);
        assert_eq!(value.read().deref(), &43);
    }

    #[test]
    fn test_rcu_cell_drop() {
        let value = RcuCell::new(42);
        drop(value);
    }
}
