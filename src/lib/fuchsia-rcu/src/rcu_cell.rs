// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::rcu_ptr::{RcuPtr, RcuReadGuard};
use crate::state_machine::rcu_drop;

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
    /// Concurrent readers may continue to see the old value of the Cell until the RCU state machine
    /// has made sufficient progress. To wait until all concurrent readers have dropped their read
    /// guards, call `rcu_synchronize()`.
    pub fn set(&self, data: T) {
        let new_ptr = Box::into_raw(Box::new(data));
        let old_ptr = self.ptr.replace(new_ptr);
        // SAFETY: The old pointer is no longer refernced from this cell. We can drop the object
        // once all the in-flight readers finish.
        unsafe { Self::drop_ptr(old_ptr) };
    }

    /// Drop the object referenced by the given pointer.
    ///
    /// This function defers the drop of the object until the RCU state machine has made sufficient
    /// progress to ensure that no concurrent readers are holding read guards.
    unsafe fn drop_ptr(data: *mut T) {
        rcu_drop(Box::from_raw(data));
    }
}

impl<T: Send + Sync + 'static> Drop for RcuCell<T> {
    fn drop(&mut self) {
        let ptr = self.ptr.replace(std::ptr::null_mut());
        // SAFETY: The old pointer is no longer refernced from this cell. We can drop the object
        // once all the in-flight readers finish.
        unsafe { Self::drop_ptr(ptr) };
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
    use crate::state_machine::rcu_synchronize;
    use std::ops::Deref;

    #[test]
    fn test_rcu_cell() {
        {
            let value = RcuCell::new(42);
            assert_eq!(value.read().deref(), &42);
        }
        rcu_synchronize();
    }

    #[test]
    fn test_rcu_cell_set() {
        {
            let value = RcuCell::new(42);
            value.set(43);
            assert_eq!(value.read().deref(), &43);
        }
        rcu_synchronize();
    }

    #[test]
    fn test_rcu_cell_drop() {
        {
            let value = RcuCell::new(42);
            drop(value);
        }
        rcu_synchronize();
    }
}
