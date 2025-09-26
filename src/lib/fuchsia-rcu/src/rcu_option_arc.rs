// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::rcu_ptr::{RcuPtr, RcuReadGuard};
use crate::rcu_write_scope::RcuWriteScope;
use crate::state_machine::rcu_synchronize;
use std::sync::Arc;

/// An RCU (Read-Copy-Update) version of `Option<Arc<...>>`.
///
/// This Arc can be read from multiple threads concurrently without blocking.
/// When the Arc is written, reads may continue to see the old value of the Arc
/// for some period of time.
#[derive(Debug)]
pub struct RcuOptionArc<T: Send + Sync + 'static> {
    ptr: RcuPtr<T>,
}

impl<T: Send + Sync + 'static> RcuOptionArc<T> {
    /// Create a new `RcuOptionArc` from an `Option<Arc<T>>`.
    pub fn new(data: Option<Arc<T>>) -> Self {
        Self { ptr: RcuPtr::new(Self::into_ptr(data)) }
    }

    /// Read the value of the `RcuOptionArc`.
    ///
    /// The object referenced by the RCU Arc will remain valid until the `RcuReadGuard` is dropped.
    /// However, another thread running concurrently might see a different value for the object.
    pub fn read(&self) -> Option<RcuReadGuard<T>> {
        self.ptr.maybe_get()
    }

    /// Write the value of the `RcuOptionArc`.
    ///
    /// Blocks until all concurrent readers have dropped their read guards.
    ///
    /// Cannot be called while this thread holds an RCU read guard.
    pub fn update_sync(&self, data: Option<Arc<T>>) {
        self.replace_sync(Self::into_ptr(data));
    }

    /// Write the value of the `RcuOptionArc`.
    ///
    /// Concurrent readers may continue to see the old value of the Arc until the RCU state machine
    /// has made sufficient progress to ensure that no concurrent readers are holding read guards.
    pub fn update(&self, scope: &RcuWriteScope, data: Option<Arc<T>>) {
        let ptr = Self::into_ptr(data);
        // SAFETY: `scope.drop` defers the drop of the object until the RCU state machine has made
        // sufficient progress to ensure that no concurrent readers are holding read guards.
        let arc = unsafe { self.replace(ptr) };
        scope.drop(arc);
    }

    /// Create a new `Option<Arc<T>>` to the object referenced by the `RcuOptionArc`.
    ///
    /// This function returns a new `Option<Arc<T>>` to the object referenced by the `RcuOptionArc`,
    /// potentially increasing the reference count of the object by one.
    pub fn to_option_arc(&self) -> Option<Arc<T>> {
        let guard = self.read()?;
        let ptr = guard.as_ptr();
        // SAFETY: We can make a new Arc to the object by incrementing the strong count and then
        // converting the pointer to an Arc.
        unsafe {
            Arc::increment_strong_count(ptr);
            Some(Arc::from_raw(ptr))
        }
    }

    /// Extract the raw pointer from an `Option<Arc<T>>`.
    ///
    /// The caller is responsible for ensuring that the pointer returned by this function is
    /// eventually converted back into an `Option<Arc<T>>` to balance its reference count.
    fn into_ptr(data: Option<Arc<T>>) -> *mut T {
        match data {
            Some(arc) => Arc::into_raw(arc) as *mut T,
            None => std::ptr::null_mut(),
        }
    }

    /// Replace the pointer in the `RcuOptionArc` with a new pointer.
    ///
    /// # Safety
    ///
    /// The caller must defer the drop of the object until the RCU state machine has made
    /// sufficient progress to ensure that no concurrent readers are holding read guards.
    #[must_use]
    unsafe fn replace(&self, ptr: *mut T) -> Option<Arc<T>> {
        let old_ptr = self.ptr.replace(ptr);
        if old_ptr.is_null() { None } else { Some(Arc::from_raw(old_ptr)) }
    }

    /// Replace the pointer in the `RcuOptionArc` with a new pointer.
    ///
    /// This function blocks until the RCU state machine has made sufficient progress to ensure
    /// that no concurrent readers are holding read guards.
    fn replace_sync(&self, ptr: *mut T) {
        // SAFETY: `rcu_synchronize` blocks until the RCU state machine has made sufficient
        // progress to ensure that no concurrent readers are holding read guards.
        let maybe_arc = unsafe { self.replace(ptr) };
        if let Some(arc) = maybe_arc {
            rcu_synchronize();
            std::mem::drop(arc);
        }
    }
}

impl<T: Send + Sync + 'static> Drop for RcuOptionArc<T> {
    fn drop(&mut self) {
        self.replace_sync(std::ptr::null_mut());
    }
}

impl<T: Send + Sync + 'static> Clone for RcuOptionArc<T> {
    fn clone(&self) -> Self {
        Self::new(self.to_option_arc())
    }
}

impl<T: Send + Sync + 'static> From<Option<Arc<T>>> for RcuOptionArc<T> {
    fn from(data: Option<Arc<T>>) -> Self {
        Self::new(data)
    }
}

impl<T: Send + Sync + 'static> Default for RcuOptionArc<T> {
    fn default() -> Self {
        Self::new(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn test_rcu_option_arc_deferred() {
        let object = DropCounter::new(42);
        let drops = object.drops.clone();

        let arc = RcuOptionArc::from(Some(object));
        assert_eq!(arc.read().unwrap().value, 42);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        let scope = RcuWriteScope::default();
        arc.update(&scope, Some(DropCounter::new(43)));
        assert_eq!(arc.read().unwrap().value, 43);
        assert_eq!(drops.load(Ordering::Relaxed), 0);

        scope.sync();
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_rcu_option_arc_sync() {
        let object = DropCounter::new(42);
        let drops = object.drops.clone();

        let arc = RcuOptionArc::from(Some(object));
        assert_eq!(arc.read().unwrap().value, 42);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        arc.update_sync(Some(DropCounter::new(43)));
        assert_eq!(arc.read().unwrap().value, 43);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_rcu_option_arc_deferred_none() {
        let object = DropCounter::new(42);
        let drops = object.drops.clone();

        let arc = RcuOptionArc::from(Some(object));
        assert_eq!(arc.read().unwrap().value, 42);
        assert_eq!(drops.load(Ordering::Relaxed), 0);

        let scope = RcuWriteScope::default();
        arc.update(&scope, None);
        assert!(arc.read().is_none());
        assert_eq!(drops.load(Ordering::Relaxed), 0);

        scope.sync();
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_rcu_option_arc_sync_none() {
        let object = DropCounter::new(42);
        let drops = object.drops.clone();

        let arc = RcuOptionArc::from(Some(object));
        assert_eq!(arc.read().unwrap().value, 42);
        assert_eq!(drops.load(Ordering::Relaxed), 0);

        arc.update_sync(None);
        assert!(arc.read().is_none());
        assert_eq!(drops.load(Ordering::Relaxed), 1);

        arc.update_sync(Some(DropCounter::new(43)));
        assert_eq!(arc.read().unwrap().value, 43);
    }

    #[test]
    fn test_rcu_option_arc_default() {
        let arc: RcuOptionArc<DropCounter> = RcuOptionArc::default();
        assert!(arc.read().is_none());
    }

    #[test]
    fn test_rcu_option_arc_to_option_arc_none() {
        let arc: RcuOptionArc<DropCounter> = RcuOptionArc::default();
        assert!(arc.to_option_arc().is_none());
    }
}
