// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use starnix_sync::Mutex;
use starnix_types::atomic::{AsAtomic, AtomicOperations};
use std::ops::{Deref, DerefMut};
use std::sync::atomic::Ordering;

#[derive(Debug)]
pub struct RcuAtomic<T: AsAtomic> {
    mutex: Mutex<T>,
    atomic: T::Atomic,
}

impl<T: AsAtomic> RcuAtomic<T> {
    pub fn new(value: T) -> Self {
        Self { mutex: Mutex::new(value), atomic: T::Atomic::new(value) }
    }

    /// Read the value from the atomic without taking the lock.
    pub fn read(&self) -> T {
        self.atomic.load(Ordering::Relaxed)
    }

    /// Basic write operation: takes the mutex and updates both the mutex value and the atomic.
    pub fn write(&self, value: T) {
        let mut guard = self.mutex.lock();
        *guard = value;
        self.atomic.store(value, Ordering::Relaxed);
    }

    /// Takes the mutex and returns a guard that can be used to read and modify the value.
    /// Updates are only committed to the atomic when `update` is called on the guard.
    pub fn copy(&self) -> RcuAtomicGuard<'_, T> {
        let guard = self.mutex.lock();
        RcuAtomicGuard { parent: self, guard }
    }
}

pub struct RcuAtomicGuard<'a, T: AsAtomic> {
    parent: &'a RcuAtomic<T>,
    guard: starnix_sync::MutexGuard<'a, T>,
}

impl<'a, T: AsAtomic> Deref for RcuAtomicGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &*self.guard
    }
}

impl<'a, T: AsAtomic> DerefMut for RcuAtomicGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut *self.guard
    }
}

impl<'a, T: AsAtomic> RcuAtomicGuard<'a, T> {
    /// Consumes the guard, updates the atomic in `RcuAtomic` with the current value,
    /// and drops the mutex guard.
    pub fn update(self) {
        let value = *self.guard;
        self.parent.atomic.store(value, Ordering::Relaxed);
        // guard dropped here
    }
}
