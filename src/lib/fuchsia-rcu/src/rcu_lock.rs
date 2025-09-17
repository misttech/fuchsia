// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::rcu_cell::RcuCell;
use crate::rcu_ptr::RcuReadGuard;
use crate::rcu_write_scope::RcuWriteScope;
use crate::state_machine::rcu_synchronize;
use fuchsia_sync::RawSyncMutex;
use lock_api::RawMutex;

/// A guard that provides write access to an `RcuLock`.
///
/// The contents of the `RcuLock` cannot be modified by another thread while the guard is held.
/// However, the contents of the `RcuLock` can be read by another thread concurrently.
///
/// See `RcuLock` for more details.
pub struct RcuLockGuard<'a, T: Send + Sync + 'static> {
    /// The `RcuLock` that is protected by this guard.
    lock: &'a RcuLock<T>,
}

impl<'a, T: Send + Sync + 'static> RcuLockGuard<'a, T> {
    /// Read the value stored in the RCU lock.
    ///
    /// The value will not change until the `RcuLockGuard` is dropped. The `RcuReadGuard` and the
    /// `RcuLockGuard` can be dropped in any order.
    ///
    /// To make a copy of the value, use the `copy` method.
    pub fn read(&self) -> RcuReadGuard<T> {
        self.lock.cell.read()
    }

    /// Update the value stored in the RCU lock.
    ///
    /// Blocks until all concurrent readers have dropped their read guards.
    ///
    /// Cannot be called while this thread holds an RCU read guard.
    pub fn update_sync(self, data: T) {
        // SAFETY: We call `rcu_synchronize` before dropping the old value to ensure that no
        // concurrent readers are holding read guards.
        let old_data = unsafe { self.lock.cell.update(data) };
        // We drop the guard before synchronizing so that we are not holding the lock while
        // waiting for the RCU state machine to advance.
        std::mem::drop(self);
        rcu_synchronize();
        std::mem::drop(old_data);
    }

    // Update the value stored in the RCU lock.
    //
    // Concurrent readers may continue to see the old value of the lock until the RCU state machine
    // has made sufficient progress to ensure that no concurrent readers are holding read guards.
    pub fn update_deferred(self, scope: &RcuWriteScope, data: T) {
        self.lock.cell.update_deferred(scope, data);
        std::mem::drop(self);
    }
}

impl<'a, T: Clone + Send + Sync + 'static> RcuLockGuard<'a, T> {
    // Make a copy of the value stored in the RCU lock.
    //
    // The value stored inside the RCU lock will not change until the guard is dropped.
    pub fn copy(&self) -> T {
        self.lock.cell.read().clone()
    }
}

impl<'a, T: Send + Sync + 'static> Drop for RcuLockGuard<'a, T> {
    fn drop(&mut self) {
        unsafe {
            self.lock.mutex.unlock();
        }
    }
}

/// Provides concurrent, non-blocking reads and serialized writes.
///
/// An `RcuLock` provides the full Read-Copy-Update (RCU) lifecycle for a single value. Reads happen
/// concurrently and do not block. To change the stored value:
///
/// ```rust
/// // Call `write` to wait for any other writers to complete.
/// let guard = self.field.write();
///
/// // Call `copy` on the returned `RcuLockGuard` to make a local copy of the value stored in the
/// // `RcuLock`.
/// let mut data = guard.copy();
///
/// // Modify the local copy as desired.
/// data.a += 1;
///
/// // Call `update_sync` or `update_deferred` on the `RcuLockGuard` to update the stored value.
/// // These operations drop the `RcuLockGuard` and let other writers proceed.
/// guard.update_sync(data);
/// ```
///
/// If you do not need to serialize writers, consider using an [`RcuCell`] instead. If you are simply
/// storing an `Arc`, consider using [`ArcRcu`] instead.
///
/// [`RcuCell`]: crate::rcu_cell::RcuCell
/// [`ArcRcu`]: crate::arc_rcu::ArcRcu
pub struct RcuLock<T: Send + Sync + 'static> {
    /// The cell that stores the value.
    cell: RcuCell<T>,

    /// The mutex that serializes writers.
    mutex: RawSyncMutex,
}

impl<T: Send + Sync + 'static> RcuLock<T> {
    /// Creates a new `RcuLock` containing the given data.
    pub fn new(data: T) -> Self {
        Self { cell: RcuCell::new(data), mutex: RawSyncMutex::INIT }
    }

    /// Read the value stored in the cell.
    ///
    /// Another thread might update the value stored in the cell concurrently, but the value will
    /// not change until the guard is dropped.
    pub fn read(&self) -> RcuReadGuard<T> {
        self.cell.read()
    }

    /// Acquire a write lock on the cell.
    ///
    /// This method blocks until any other writers have dropped their write locks. Once the lock is
    /// acquired, the value stored in the cell cannot be modified concurrently by other threads.
    ///
    /// The lock is released when the returned `RcuLockGuard` is dropped.
    pub fn write(&self) -> RcuLockGuard<'_, T> {
        self.mutex.lock();
        RcuLockGuard { lock: self }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone)]
    struct Payload {
        a: i32,
        b: i32,
    }

    #[test]
    fn test_rcu_lock_read() {
        let value = RcuLock::new(Payload { a: 1, b: 2 });
        let guard = value.read();
        assert_eq!(guard.a, 1);
        assert_eq!(guard.b, 2);
    }

    #[test]
    fn test_rcu_lock_read_copy_update() {
        let value = RcuLock::new(Payload { a: 1, b: 2 });
        let guard = value.write();
        let read_guard = guard.read();
        assert_eq!(read_guard.a, 1);
        assert_eq!(read_guard.b, 2);
        std::mem::drop(read_guard);
        let mut copy = guard.copy();
        assert_eq!(copy.a, 1);
        assert_eq!(copy.b, 2);
        copy.a = 3;
        copy.b = 4;
        guard.update_sync(copy);
        let read_guard = value.read();
        assert_eq!(read_guard.a, 3);
        assert_eq!(read_guard.b, 4);
    }

    #[test]
    fn test_rcu_lock_read_copy_update_deferred() {
        let value = RcuLock::new(Payload { a: 1, b: 2 });
        let guard = value.write();
        let mut copy = guard.copy();
        assert_eq!(copy.a, 1);
        assert_eq!(copy.b, 2);
        copy.a = 3;
        copy.b = 4;
        let scope = RcuWriteScope::default();
        guard.update_deferred(&scope, copy);
        let read_guard = value.read();
        assert_eq!(read_guard.a, 3);
        assert_eq!(read_guard.b, 4);
        std::mem::drop(read_guard);
        std::mem::drop(scope);
    }
}
