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

pub mod once;
pub mod rc;

/// Concrete synchronization primitive implementations for bindings and tests.
///
/// This module provides default raw lock and `Once` types so bindings and fake
/// bindings contexts can implement `BindingsSyncTypes` without duplicating
/// platform-specific lock selection. Core crates should obtain synchronization
/// types through `BindingsSyncTypes` rather than using `lock_impl` directly.
///
/// TODO(https://fxbug.dev/553027583): Remove this module, this exists only for
/// ease transition to lock API.
pub mod lock_impl {
    pub use fuchsia_sync::{RawMutex, RawRwLock};
    /// Default [`crate::once::Once`] implementation backed by
    /// [`std::sync::Once`].
    pub type Once = std::sync::Once;
}
#[cfg(not(loom))]
pub use inner::*;
#[cfg(loom)]
pub use inner_loom::*;

#[cfg(not(loom))]
mod inner {
    use core::fmt;

    /// A mutex assuming lock poisoning will never occur.
    pub struct Mutex<T, R>(lock_api::Mutex<R, T>);

    /// Lock guard for access to a [`Mutex`].
    pub type LockGuard<'a, T, R> = lock_api::MutexGuard<'a, R, T>;

    impl<T: Default, R: lock_api::RawMutex> Default for Mutex<T, R> {
        #[inline]
        fn default() -> Self {
            Self(lock_api::Mutex::default())
        }
    }

    impl<T: fmt::Debug, R: lock_api::RawMutex> fmt::Debug for Mutex<T, R> {
        #[inline]
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let Self(m) = self;
            m.fmt(f)
        }
    }

    impl<T, R: lock_api::RawMutex> Mutex<T, R> {
        /// Creates a new mutex in an unlocked state ready for use.
        #[inline]
        pub const fn new(t: T) -> Self {
            Self(lock_api::Mutex::new(t))
        }

        /// Acquires a mutex, blocking the current thread until it is able to do
        /// so.
        ///
        /// # Panics
        ///
        /// This method may panic if the calling thread is already holding the
        /// lock.
        #[inline]
        pub fn lock(&self) -> LockGuard<'_, T, R> {
            let Self(m) = self;
            m.lock()
        }

        /// Consumes this mutex, returning the underlying data.
        #[inline]
        pub fn into_inner(self) -> T {
            let Self(mutex) = self;
            mutex.into_inner()
        }

        /// Returns a mutable reference to the underlying data.
        ///
        /// Since this call borrows the [`Mutex`] mutably, no actual locking
        /// needs to take place.
        #[inline]
        pub fn get_mut(&mut self) -> &mut T {
            let Self(mutex) = self;
            mutex.get_mut()
        }
    }
    /// A reader-writer lock assuming lock poisoning will never occur.
    pub struct RwLock<T, R>(lock_api::RwLock<R, T>);

    /// Lock guard for read access to a [`RwLock`].
    pub type RwLockReadGuard<'a, T, R> = lock_api::RwLockReadGuard<'a, R, T>;

    /// Lock guard for write access to a [`RwLock`].
    pub type RwLockWriteGuard<'a, T, R> = lock_api::RwLockWriteGuard<'a, R, T>;

    impl<T: Default, R: lock_api::RawRwLock> Default for RwLock<T, R> {
        #[inline]
        fn default() -> Self {
            Self(lock_api::RwLock::default())
        }
    }

    impl<T: fmt::Debug, R: lock_api::RawRwLock> fmt::Debug for RwLock<T, R> {
        #[inline]
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let Self(rw) = self;
            rw.fmt(f)
        }
    }

    impl<T, R: lock_api::RawRwLock> RwLock<T, R> {
        /// Creates a new instance of an `RwLock<T>` which is unlocked.
        #[inline]
        pub const fn new(t: T) -> Self {
            Self(lock_api::RwLock::new(t))
        }

        /// Locks this rwlock with shared read access, blocking the current
        /// thread until it can be acquired.
        ///
        /// # Panics
        ///
        /// This method may panic if the calling thread already holds the read
        /// or write lock.
        #[inline]
        pub fn read(&self) -> RwLockReadGuard<'_, T, R> {
            let Self(rw) = self;
            rw.read()
        }

        /// Locks this rwlock with exclusive write access, blocking the current
        /// thread until it can be acquired.
        ///
        /// # Panics
        ///
        /// This method may panic if the calling thread already holds the read
        /// or write lock.
        #[inline]
        pub fn write(&self) -> RwLockWriteGuard<'_, T, R> {
            let Self(rw) = self;
            rw.write()
        }

        /// Consumes this rwlock, returning the underlying data.
        #[inline]
        pub fn into_inner(self) -> T {
            let Self(rwlock) = self;
            rwlock.into_inner()
        }

        /// Returns a mutable reference to the underlying data.
        ///
        /// Since this call borrows the [`RwLock`] mutably, no actual locking
        /// needs to take place.
        #[inline]
        pub fn get_mut(&mut self) -> &mut T {
            let Self(rwlock) = self;
            rwlock.get_mut()
        }
    }
}

/// An alternative implementation that provides the exact same API for `Mutex`
/// and `RwLock` as the `inner` module does, but alway`loom::sync` locks.
#[cfg(loom)]
mod inner_loom {
    use core::fmt;
    use core::marker::PhantomData;
    use core::ops::{Deref, DerefMut};

    use lock_api as _;

    use crate::sync;

    /// A mutex assuming lock poisoning will never occur.
    pub struct Mutex<T, R>(sync::Mutex<T>, PhantomData<R>);

    /// Lock guard for access to a [`Mutex`].
    pub struct LockGuard<'a, T, R>(sync::MutexGuard<'a, T>, PhantomData<R>);

    impl<T: fmt::Debug, R> fmt::Debug for LockGuard<'_, T, R> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let Self(g, _marker) = self;
            g.fmt(f)
        }
    }

    impl<T: fmt::Display, R> fmt::Display for LockGuard<'_, T, R> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let Self(g, _marker) = self;
            g.fmt(f)
        }
    }

    impl<T, R> Deref for LockGuard<'_, T, R> {
        type Target = T;

        fn deref(&self) -> &Self::Target {
            let Self(g, _marker) = self;
            g.deref()
        }
    }

    impl<T, R> DerefMut for LockGuard<'_, T, R> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            let Self(g, _marker) = self;
            g.deref_mut()
        }
    }

    impl<T: Default, R> Default for Mutex<T, R> {
        #[inline]
        fn default() -> Self {
            Self::new(T::default())
        }
    }

    impl<T: fmt::Debug, R> fmt::Debug for Mutex<T, R> {
        #[inline]
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let Self(m, _marker) = self;
            m.fmt(f)
        }
    }

    impl<T, R> Mutex<T, R> {
        /// Creates a new mutex in an unlocked state ready for use.
        #[inline]
        pub fn new(t: T) -> Self {
            Self(sync::Mutex::new(t), PhantomData)
        }

        /// Acquires a mutex, blocking the current thread until it is able to do
        /// so.
        ///
        /// # Panics
        ///
        /// This method may panic if the calling thread is already holding the
        /// lock.
        #[inline]
        pub fn lock(&self) -> LockGuard<'_, T, R> {
            let Self(m, _marker) = self;
            LockGuard(m.lock().expect("unexpectedly poisoned"), PhantomData)
        }

        /// Consumes this mutex, returning the underlying data.
        #[inline]
        pub fn into_inner(self) -> T {
            let Self(mutex, _marker) = self;
            mutex.into_inner().expect("unexpectedly poisoned")
        }
    }

    /// A reader-writer lock assuming lock poisoning will never occur.
    pub struct RwLock<T, R>(sync::RwLock<T>, PhantomData<R>);

    /// Lock guard for read access to a [`RwLock`].
    pub struct RwLockReadGuard<'a, T, R>(sync::RwLockReadGuard<'a, T>, PhantomData<R>);

    impl<T: fmt::Debug, R> fmt::Debug for RwLockReadGuard<'_, T, R> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let Self(g, _marker) = self;
            g.fmt(f)
        }
    }

    impl<T: fmt::Display, R> fmt::Display for RwLockReadGuard<'_, T, R> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let Self(g, _marker) = self;
            g.fmt(f)
        }
    }

    impl<T, R> Deref for RwLockReadGuard<'_, T, R> {
        type Target = T;

        fn deref(&self) -> &Self::Target {
            let Self(g, _marker) = self;
            g.deref()
        }
    }

    /// Lock guard for write access to a [`RwLock`].
    pub struct RwLockWriteGuard<'a, T, R>(sync::RwLockWriteGuard<'a, T>, PhantomData<R>);

    impl<T: fmt::Debug, R> fmt::Debug for RwLockWriteGuard<'_, T, R> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let Self(g, _marker) = self;
            g.fmt(f)
        }
    }

    impl<T: fmt::Display, R> fmt::Display for RwLockWriteGuard<'_, T, R> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let Self(g, _marker) = self;
            g.fmt(f)
        }
    }

    impl<T, R> Deref for RwLockWriteGuard<'_, T, R> {
        type Target = T;

        fn deref(&self) -> &Self::Target {
            let Self(g, _marker) = self;
            g.deref()
        }
    }

    impl<T, R> DerefMut for RwLockWriteGuard<'_, T, R> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            let Self(g, _marker) = self;
            g.deref_mut()
        }
    }

    impl<T: Default, R> Default for RwLock<T, R> {
        #[inline]
        fn default() -> Self {
            Self::new(T::default())
        }
    }

    impl<T: fmt::Debug, R> fmt::Debug for RwLock<T, R> {
        #[inline]
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let Self(rw, _marker) = self;
            rw.fmt(f)
        }
    }

    impl<T, R> RwLock<T, R> {
        /// Creates a new instance of an `RwLock<T>` which is unlocked.
        #[inline]
        pub fn new(t: T) -> Self {
            Self(sync::RwLock::new(t), PhantomData)
        }

        /// Locks this rwlock with shared read access, blocking the current
        /// thread until it can be acquired.
        ///
        /// # Panics
        ///
        /// This method may panic if the calling thread already holds the read
        /// or write lock.
        #[inline]
        pub fn read(&self) -> RwLockReadGuard<'_, T, R> {
            let Self(rw, _marker) = self;
            RwLockReadGuard(rw.read().expect("unexpectedly poisoned"), PhantomData)
        }

        /// Locks this rwlock with exclusive write access, blocking the current
        /// thread until it can be acquired.
        ///
        /// # Panics
        ///
        /// This method may panic if the calling thread already holds the read
        /// or write lock.
        #[inline]
        pub fn write(&self) -> RwLockWriteGuard<'_, T, R> {
            let Self(rw, _marker) = self;
            RwLockWriteGuard(rw.write().expect("unexpectedly poisoned"), PhantomData)
        }

        /// Consumes this rwlock, returning the underlying data.
        #[inline]
        pub fn into_inner(self) -> T {
            let Self(rwlock, _marker) = self;
            rwlock.into_inner().expect("unexpectedly poisoned")
        }
    }
}

impl<T: 'static, R: lock_api::RawMutex + 'static> lock_order::lock::ExclusiveLock<T>
    for Mutex<T, R>
{
    type Guard<'l> = LockGuard<'l, T, R>;

    fn lock(&self) -> Self::Guard<'_> {
        self.lock()
    }
}

impl<T, I: Ip, R> GenericOverIp<I> for Mutex<T, R>
where
    T: GenericOverIp<I>,
{
    type Type = Mutex<T::Type, R>;
}

impl<T: 'static, R: lock_api::RawRwLock + 'static> lock_order::lock::ReadWriteLock<T>
    for RwLock<T, R>
{
    type ReadGuard<'l> = RwLockReadGuard<'l, T, R>;

    type WriteGuard<'l> = RwLockWriteGuard<'l, T, R>;

    fn read_lock(&self) -> Self::ReadGuard<'_> {
        self.read()
    }

    fn write_lock(&self) -> Self::WriteGuard<'_> {
        self.write()
    }
}

impl<T, I: Ip, R> GenericOverIp<I> for RwLock<T, R>
where
    T: GenericOverIp<I>,
{
    type Type = RwLock<T::Type, R>;
}
