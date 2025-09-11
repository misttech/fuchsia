// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::rcu_ptr::{RcuPtr, RcuReadGuard};
use crate::state_machine::rcu_drop;
use std::sync::Arc;

/// An RCU (Read-Copy-Update) version of `Arc`.
///
/// This Arc can be read from multiple threads concurrently without blocking.
/// When the Arc is written, reads may continue to see the old value of the Arc
/// for some period of time.
pub struct RcuArc<T: Send + Sync + 'static> {
    ptr: RcuPtr<T>,
}

impl<T: Send + Sync + 'static> RcuArc<T> {
    /// Create a new RCU Arc from an `Arc`.
    pub fn new(data: Arc<T>) -> Self {
        Self { ptr: RcuPtr::new(Self::into_ptr(data)) }
    }

    /// Read the value of the RCU Arc.
    ///
    /// The object referenced by the RCU Arc will remain valid until the `RcuReadGuard` is dropped.
    /// However, another thread running concurrently might see a different value for the object.
    pub fn read(&self) -> RcuReadGuard<T> {
        self.ptr.read()
    }

    /// Write the value of the RCU Arc.
    ///
    /// Concurrent readers may continue to see the old value of the Arc until the RCU state machine
    /// has made sufficient progress. To wait until all concurrent readers have dropped their read
    /// guards, call `rcu_synchronize()`.
    pub fn set(&self, data: Arc<T>) {
        let old_ptr = self.ptr.replace(Self::into_ptr(data));
        // SAFETY: The old pointer is no longer referenced from this object. We can drop our strong
        // reference to the object once all the in-flight readers have finished.
        unsafe { Self::drop_ptr(old_ptr) };
    }

    /// Create a new `Arc` to the object referenced by the RCU Arc.
    ///
    /// This function returns a new `Arc` to the object referenced by the RCU Arc, increasing the
    /// reference count of the object by one.
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

    /// Decrement the reference count of the object referenced by the given pointer.
    ///
    /// This function defers the drop of the object until the RCU state machine has made sufficient
    /// progress to ensure that no concurrent readers are holding read guards.
    unsafe fn drop_ptr(data: *const T) {
        rcu_drop(Arc::from_raw(data));
    }
}

impl<T: Send + Sync + 'static> Drop for RcuArc<T> {
    fn drop(&mut self) {
        let ptr = self.ptr.replace(std::ptr::null_mut());
        // SAFETY: The old pointer is no longer referenced from this object. We can drop our strong
        // reference to the object once all the in-flight readers have finished.
        unsafe { Self::drop_ptr(ptr) };
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
    fn test_rcu_arc() {
        {
            let object = DropCounter::new(42);
            let drops = object.drops.clone();

            let arc = RcuArc::from(object);
            assert_eq!(arc.read().value, 42);
            assert_eq!(drops.load(Ordering::Relaxed), 0);
            arc.set(DropCounter::new(43));
            assert_eq!(arc.read().value, 43);
            assert_eq!(drops.load(Ordering::Relaxed), 0);

            rcu_synchronize();
            assert_eq!(drops.load(Ordering::Relaxed), 1);
        }
        rcu_synchronize();
    }
}
