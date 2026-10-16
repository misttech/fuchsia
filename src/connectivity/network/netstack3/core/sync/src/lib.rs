// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Synchronization primitives for Netstack3.

#![warn(missing_docs, unreachable_patterns, unused)]

extern crate alloc;

#[cfg(loom)]
pub(crate) use loom::sync;
#[cfg(not(loom))]
pub(crate) use std::sync;

use net_types::ip::{GenericOverIp, Ip};

// Re-export atomics module for loom compatibility in appropriate targets.
pub use sync::atomic;

pub mod rc;

/// A mutex assuming lock poisoning will never occur.
#[derive(Debug, Default)]
pub struct Mutex<T>(
    #[cfg(all(target_os = "fuchsia", not(loom)))] fuchsia_sync::Mutex<T>,
    #[cfg(not(all(target_os = "fuchsia", not(loom))))] sync::Mutex<T>,
);

/// Lock guard for access to a [`Mutex`].
#[cfg(all(target_os = "fuchsia", not(loom)))]
pub type LockGuard<'a, T> = fuchsia_sync::MutexGuard<'a, T>;

/// Lock guard for access to a [`Mutex`].
#[cfg(not(all(target_os = "fuchsia", not(loom))))]
pub type LockGuard<'a, T> = sync::MutexGuard<'a, T>;

impl<T> Mutex<T> {
    /// Creates a new mutex in an unlocked state ready for use.
    #[inline]
    pub fn new(t: T) -> Mutex<T> {
        #[cfg(all(target_os = "fuchsia", not(loom)))]
        let m = fuchsia_sync::Mutex::new(t);
        #[cfg(not(all(target_os = "fuchsia", not(loom))))]
        let m = sync::Mutex::new(t);
        Mutex(m)
    }

    /// Acquires a mutex, blocking the current thread until it is able to do so.
    ///
    /// # Panics
    ///
    /// This method may panic if the calling thread is already holding the
    /// lock.
    #[inline]
    pub fn lock(&self) -> LockGuard<'_, T> {
        let Self(m) = self;
        #[cfg(all(target_os = "fuchsia", not(loom)))]
        return m.lock();
        #[cfg(not(all(target_os = "fuchsia", not(loom))))]
        return m.lock().expect("unexpectedly poisoned");
    }

    /// Consumes this mutex, returning the underlying data.
    #[inline]
    pub fn into_inner(self) -> T {
        let Self(mutex) = self;
        #[cfg(all(target_os = "fuchsia", not(loom)))]
        return mutex.into_inner();
        #[cfg(not(all(target_os = "fuchsia", not(loom))))]
        return mutex.into_inner().expect("unexpectedly poisoned");
    }

    /// Returns a mutable reference to the underlying data.
    ///
    /// Since this call borrows the [`Mutex`] mutably, no actual locking needs
    /// to take place.
    #[inline]
    // TODO(https://github.com/tokio-rs/loom/pull/322): remove the disable for
    // loom once loom's lock type supports the method.
    #[cfg(not(loom))]
    pub fn get_mut(&mut self) -> &mut T {
        let Self(mutex) = self;
        #[cfg(target_os = "fuchsia")]
        return mutex.get_mut();
        #[cfg(not(target_os = "fuchsia"))]
        return mutex.get_mut().expect("unexpectedly poisoned");
    }
}

impl<T: 'static> lock_order::lock::ExclusiveLock<T> for Mutex<T> {
    type Guard<'l> = LockGuard<'l, T>;

    fn lock(&self) -> Self::Guard<'_> {
        self.lock()
    }
}

impl<T, I: Ip> GenericOverIp<I> for Mutex<T>
where
    T: GenericOverIp<I>,
{
    type Type = Mutex<T::Type>;
}

/// A reader-writer lock assuming lock poisoning will never occur.
#[derive(Debug, Default)]
pub struct RwLock<T>(
    #[cfg(all(target_os = "fuchsia", not(loom)))] fuchsia_sync::RwLock<T>,
    #[cfg(not(all(target_os = "fuchsia", not(loom))))] sync::RwLock<T>,
);

/// Lock guard for read access to a [`RwLock`].
#[cfg(all(target_os = "fuchsia", not(loom)))]
pub type RwLockReadGuard<'a, T> = fuchsia_sync::RwLockReadGuard<'a, T>;

/// Lock guard for read access to a [`RwLock`].
#[cfg(not(all(target_os = "fuchsia", not(loom))))]
pub type RwLockReadGuard<'a, T> = sync::RwLockReadGuard<'a, T>;

/// Lock guard for write access to a [`RwLock`].
#[cfg(all(target_os = "fuchsia", not(loom)))]
pub type RwLockWriteGuard<'a, T> = fuchsia_sync::RwLockWriteGuard<'a, T>;

/// Lock guard for write access to a [`RwLock`].
#[cfg(not(all(target_os = "fuchsia", not(loom))))]
pub type RwLockWriteGuard<'a, T> = sync::RwLockWriteGuard<'a, T>;

impl<T> RwLock<T> {
    /// Creates a new instance of an `RwLock<T>` which is unlocked.
    #[inline]
    pub fn new(t: T) -> RwLock<T> {
        #[cfg(all(target_os = "fuchsia", not(loom)))]
        let rw = fuchsia_sync::RwLock::new(t);
        #[cfg(not(all(target_os = "fuchsia", not(loom))))]
        let rw = sync::RwLock::new(t);
        RwLock(rw)
    }

    /// Locks this rwlock with shared read access, blocking the current thread
    /// until it can be acquired.
    ///
    /// # Panics
    ///
    /// This method may panic if the calling thread already holds the read or
    /// write lock.
    #[inline]
    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        let Self(rw) = self;
        #[cfg(all(target_os = "fuchsia", not(loom)))]
        return rw.read();
        #[cfg(not(all(target_os = "fuchsia", not(loom))))]
        return rw.read().expect("unexpectedly poisoned");
    }

    /// Locks this rwlock with exclusive write access, blocking the current
    /// thread until it can be acquired.
    ///
    /// # Panics
    ///
    /// This method may panic if the calling thread already holds the read or
    /// write lock.
    #[inline]
    pub fn write(&self) -> RwLockWriteGuard<'_, T> {
        let Self(rw) = self;
        #[cfg(all(target_os = "fuchsia", not(loom)))]
        return rw.write();
        #[cfg(not(all(target_os = "fuchsia", not(loom))))]
        return rw.write().expect("unexpectedly poisoned");
    }

    /// Consumes this rwlock, returning the underlying data.
    #[inline]
    pub fn into_inner(self) -> T {
        let Self(rwlock) = self;
        #[cfg(all(target_os = "fuchsia", not(loom)))]
        return rwlock.into_inner();
        #[cfg(not(all(target_os = "fuchsia", not(loom))))]
        return rwlock.into_inner().expect("unexpectedly poisoned");
    }

    /// Returns a mutable reference to the underlying data.
    ///
    /// Since this call borrows the [`RwLock`] mutably, no actual locking needs
    /// to take place.
    #[inline]
    // TODO(https://github.com/tokio-rs/loom/pull/322): remove the disable for
    // loom once loom's lock type supports the method.
    #[cfg(not(loom))]
    pub fn get_mut(&mut self) -> &mut T {
        let Self(rwlock) = self;
        #[cfg(target_os = "fuchsia")]
        return rwlock.get_mut();
        #[cfg(not(target_os = "fuchsia"))]
        return rwlock.get_mut().expect("unexpectedly poisoned");
    }
}

impl<T: 'static> lock_order::lock::ReadWriteLock<T> for RwLock<T> {
    type ReadGuard<'l> = RwLockReadGuard<'l, T>;

    type WriteGuard<'l> = RwLockWriteGuard<'l, T>;

    fn read_lock(&self) -> Self::ReadGuard<'_> {
        self.read()
    }

    fn write_lock(&self) -> Self::WriteGuard<'_> {
        self.write()
    }
}

impl<T, I: Ip> GenericOverIp<I> for RwLock<T>
where
    T: GenericOverIp<I>,
{
    type Type = RwLock<T::Type>;
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::thread;

    #[test]
    fn mutex_lock_and_write() {
        let m = Mutex::<u32>::new(0);
        {
            let mut guard = m.lock();
            assert_eq!(*guard, 0);
            *guard = 5;
        }

        {
            let guard = m.lock();
            assert_eq!(*guard, 5);
        }
    }

    #[test]
    fn mutex_lock_from_different_threads() {
        const NUM_THREADS: u32 = 4;

        let m = Mutex::<u32>::new(u32::MAX);
        let m = &m;

        thread::scope(|s| {
            for i in 0..NUM_THREADS {
                let _: thread::ScopedJoinHandle<'_, _> = s.spawn(move || {
                    let prev = {
                        let mut guard = m.lock();
                        let prev = *guard;
                        *guard = i;
                        prev
                    };

                    assert!(prev == u32::MAX || prev < NUM_THREADS);
                });
            }
        });

        let guard = m.lock();
        assert!(*guard < NUM_THREADS);
    }

    #[test]
    fn rwlock_read_lock() {
        let rw = RwLock::<u32>::new(0);

        {
            let guard = rw.read();
            assert_eq!(*guard, 0);
        }

        {
            let guard = rw.read();
            assert_eq!(*guard, 0);
        }
    }

    #[test]
    fn rwlock_write_lock() {
        let rw = RwLock::<u32>::new(0);
        {
            let mut guard = rw.write();
            assert_eq!(*guard, 0);
            *guard = 5;
        }

        {
            let guard = rw.write();
            assert_eq!(*guard, 5);
        }
    }

    #[test]
    fn rwlock_read_and_write_from_different_threads() {
        const NUM_THREADS: u32 = 4;

        let rw = RwLock::<u32>::new(u32::MAX);
        let rw = &rw;

        thread::scope(|s| {
            for i in 0..NUM_THREADS {
                let _: thread::ScopedJoinHandle<'_, _> = s.spawn(move || {
                    let prev = if i % 2 == 0 {
                        // Only threads with even numbered `i` performs a write.
                        let mut guard = rw.write();
                        let prev = *guard;
                        *guard = i;
                        prev
                    } else {
                        let guard = rw.read();
                        *guard
                    };

                    assert!(prev == u32::MAX || (prev < NUM_THREADS && prev % 2 == 0));
                });
            }
        });

        let val = *rw.read();
        assert!(val < NUM_THREADS && val % 2 == 0);
    }
}
