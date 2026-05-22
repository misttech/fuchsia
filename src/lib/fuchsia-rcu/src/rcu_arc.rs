// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::rcu_ptr::{RcuPtr, RcuReadGuard};
use crate::rcu_read_scope::RcuReadScope;
use crate::state_machine::rcu_drop;
use std::sync::Arc;

/// An RCU (Read-Copy-Update) wrapper around an `Arc`.
///
/// The Arc can be dereferenced from multiple threads concurrently without blocking.
/// When the Arc is replaced, reads may continue to see the old Arc pointer for some period of time.
#[derive(Debug)]
pub struct RcuArc<T: Send + Sync + 'static> {
    ptr: RcuPtr<T>,
}

impl<T: Send + Sync + 'static> RcuArc<T> {
    /// Create a new RCU wrapper around an `Arc`.
    pub fn new(data: Arc<T>) -> Self {
        Self { ptr: RcuPtr::new(Self::into_ptr(data)) }
    }

    /// Read the value of the wrapped Arc.
    ///
    /// The object referenced by the RCU Arc will remain valid until the `RcuReadGuard` is dropped.
    /// However, another thread running concurrently might see a different value for the object.
    pub fn read(&self) -> RcuReadGuard<T> {
        self.ptr.get()
    }

    /// Returns a reference to the value of the wrapped Arc.
    ///
    /// The object referenced by the RCU Arc will remain valid until the `RcuReadScope` is dropped.
    /// However, another thread running concurrently might see a different value for the object.
    pub fn as_ref<'a>(&self, scope: &'a RcuReadScope) -> &'a T {
        self.ptr.read(scope).as_ref().unwrap()
    }

    /// Write a new Arc to the RCU wrapper.
    ///
    /// Concurrent readers may continue to see the old Arc pointer until the RCU state machine has
    /// made sufficient progress to ensure that no concurrent readers are holding read guards.
    pub fn update(&self, data: Arc<T>) {
        let ptr = Self::into_ptr(data);
        // SAFETY: We can pass `Self::into_ptr` to `Self::replace`.
        unsafe { self.replace(ptr) };
    }

    /// Write a new Arc to the RCU wrapper and return a reference to the old value.
    ///
    /// Concurrent readers may continue to see the old Arc pointer until the RCU state machine has
    /// made sufficient progress to ensure that no concurrent readers are holding read guards.
    pub fn update_swap<'a>(&self, scope: &'a RcuReadScope, data: Arc<T>) -> &'a T {
        let ptr = Self::into_ptr(data);
        // SAFETY: We can pass `Self::into_ptr` to `Self::replace_swap`.
        unsafe { self.replace_swap(scope, ptr) }
    }

    /// Create a new `Arc` to the object referenced by the wrapped Arc.
    ///
    /// This function returns a new `Arc` to the object referenced by the wrapped Arc,
    /// increasing the reference count of the object by one.
    pub fn to_arc(&self) -> Arc<T> {
        let guard = self.read();
        let ptr = guard.as_ptr();
        // SAFETY: We can make a new Arc to the object by incrementing the strong count and then
        // converting the pointer to an Arc.
        unsafe {
            Arc::increment_strong_count(ptr);
            Arc::from_raw(ptr)
        }
    }

    /// Extract the raw pointer from an `Arc`.
    ///
    /// The caller is responsible for ensuring that the pointer returned by this function is
    /// eventually converted back into an `Arc` to balance its reference count.
    fn into_ptr(data: Arc<T>) -> *mut T {
        Arc::into_raw(data) as *mut T
    }

    /// Replace the Arc pointer in the RCU wrapper with a new pointer.
    ///
    /// # Safety
    ///
    /// The caller must have obtained the pointer from `Self::into_ptr` or from `std::ptr::null_mut`.
    unsafe fn replace(&self, ptr: *mut T) {
        let old_ptr = self.ptr.replace(ptr);
        let arc = unsafe { Arc::from_raw(old_ptr) };
        rcu_drop(arc);
    }

    /// Replace the Arc pointer in the RCU wrapper with a new pointer and return a reference to the
    /// old value.
    ///
    /// # Safety
    ///
    /// The caller must have obtained the pointer from `Self::into_ptr` or from `std::ptr::null_mut`.
    unsafe fn replace_swap<'a>(&self, scope: &'a RcuReadScope, ptr: *mut T) -> &'a T {
        let old_ptr_ref = self.ptr.swap(scope, ptr);
        // SAFETY: `old_ptr_ref` points to an existing `Arc<T>` with a strong reference.
        let arc = unsafe { Arc::from_raw(old_ptr_ref.as_ptr()) };
        rcu_drop(arc);
        old_ptr_ref.as_ref().unwrap()
    }
}

impl<T: Send + Sync + 'static> Drop for RcuArc<T> {
    fn drop(&mut self) {
        // SAFETY: We can pass `std::ptr::null_mut`.
        unsafe { self.replace(std::ptr::null_mut()) };
    }
}

impl<T: Send + Sync + 'static> Clone for RcuArc<T> {
    fn clone(&self) -> Self {
        Self::new(self.to_arc())
    }
}

impl<T: Send + Sync + 'static> From<Arc<T>> for RcuArc<T> {
    fn from(data: Arc<T>) -> Self {
        Self::new(data)
    }
}

impl<T: Default + Send + Sync + 'static> Default for RcuArc<T> {
    fn default() -> Self {
        Self::new(Arc::new(T::default()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_machine::rcu_synchronize;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct DropCounter {
        value: usize,
        drops: Arc<AtomicUsize>,
    }

    impl DropCounter {
        pub fn new(value: usize) -> Arc<Self> {
            Arc::new(Self { value, drops: Arc::new(AtomicUsize::new(0)) })
        }
    }

    impl Drop for DropCounter {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn test_rcu_arc_update() {
        let object = DropCounter::new(42);
        let drops = object.drops.clone();

        let arc = RcuArc::from(object);
        assert_eq!(arc.read().value, 42);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        arc.update(DropCounter::new(43));
        assert_eq!(arc.read().value, 43);
        assert_eq!(drops.load(Ordering::Relaxed), 0);

        rcu_synchronize();
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_rcu_arc_update_swap() {
        let object = DropCounter::new(42);
        let drops = object.drops.clone();

        let arc = RcuArc::from(object);
        {
            let scope = RcuReadScope::new();
            let old_object = arc.update_swap(&scope, DropCounter::new(43));
            assert_eq!(old_object.value, 42);
            assert_eq!(arc.read().value, 43);
            assert_eq!(drops.load(Ordering::Relaxed), 0);
        }

        rcu_synchronize();
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }
}
