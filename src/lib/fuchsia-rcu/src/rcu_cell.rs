// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::rcu_ptr::{RcuPtr, RcuReadGuard};
use crate::rcu_read_scope::RcuReadScope;
use crate::state_machine::rcu_drop;

/// An RCU (Read-Copy-Update) version of `Cell`.
///
/// This Cell can be read from multiple threads concurrently without blocking.
/// When the Cell is written, reads may continue to see the old value of the Cell
/// for some period of time.
#[derive(Debug)]
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
        self.ptr.get()
    }

    /// Returns a reference to the value of the RCU Cell.
    ///
    /// The object referenced by the RCU Cell will remain valid until the `RcuReadGuard` is dropped.
    /// However, another thread running concurrently might see a different value for the object.
    pub fn as_ref<'a>(&self, scope: &'a RcuReadScope) -> &'a T {
        self.ptr.read(scope).as_ref().unwrap()
    }

    /// Write the value of the RCU Cell.
    ///
    /// Concurrent readers may continue to see the old value of the Cell until the RCU state machine
    /// has made sufficient progress to ensure that no concurrent readers are holding read guards.
    pub fn update(&self, data: T) {
        let ptr = Box::into_raw(Box::new(data));
        // SAFETY: We can pass `Box::into_raw` to `Self::replace`.
        unsafe { self.replace(ptr) };
    }

    /// Replace the pointer in the RCU Cell with a new pointer.
    ///
    /// # Safety
    ///
    /// The pointer must have been created by `Box::into_raw` or from `std::ptr::null_mut`.
    unsafe fn replace(&self, ptr: *mut T) {
        let old_ptr = self.ptr.replace(ptr);
        let object = unsafe { Box::from_raw(old_ptr) };
        rcu_drop(object);
    }
}

impl<T: Send + Sync + 'static> Drop for RcuCell<T> {
    fn drop(&mut self) {
        // SAFETY: We can pass `std::ptr::null_mut` to `Self::replace`.
        unsafe { self.replace(std::ptr::null_mut()) };
    }
}

impl<T: Default + Send + Sync + 'static> Default for RcuCell<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: Clone + Send + Sync + 'static> Clone for RcuCell<T> {
    fn clone(&self) -> Self {
        let value = self.read();
        Self::new(value.clone())
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
        let value = RcuCell::new(42);
        assert_eq!(value.read().deref(), &42);
    }

    #[test]
    fn test_rcu_cell_set_deferred() {
        let value = RcuCell::new(42);
        value.update(43);
        assert_eq!(value.read().deref(), &43);
        rcu_synchronize();
    }

    #[test]
    fn test_rcu_cell_drop() {
        let value = RcuCell::new(42);
        drop(value);
    }
}
