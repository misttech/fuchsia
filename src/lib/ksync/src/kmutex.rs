// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::lock_token::LockToken;
use core::marker::PhantomData;
use lock_api::RawMutex as _;

/// A token-based Mutex.
///
/// This mutex does not contain the data it protects. Instead, it protects fields wrapped in `KCell`
/// that are associated with the same `Class`.
#[repr(transparent)]
pub struct KMutex<Class = ()> {
    inner: fuchsia_sync::RawMutex,
    _marker: PhantomData<Class>,
}

impl<Class> KMutex<Class> {
    /// Create a new `KMutex`.
    #[inline]
    pub const fn new() -> Self {
        Self { inner: fuchsia_sync::RawMutex::INIT, _marker: PhantomData }
    }

    /// Lock the mutex and return a guard containing the lock token.
    #[inline]
    pub fn lock(&self) -> KMutexGuard<'_, Class> {
        self.inner.lock();
        // SAFETY: We have just successfully acquired the mutual exclusion lock,
        // so it is safe to create a token proving the lock is held.
        let token = unsafe { LockToken::new() };
        KMutexGuard { mutex: self, token }
    }
}

impl<Class> Default for KMutex<Class> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<Class> core::fmt::Debug for KMutex<Class> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("KMutex").field("class", &core::any::type_name::<Class>()).finish()
    }
}

/// A guard that keeps the underlying mutex locked and provides access to the lock token.
#[repr(transparent)]
pub struct KMutexGuard<'a, Class> {
    mutex: &'a KMutex<Class>,
    token: LockToken<'a, Class>,
}

impl<'a, Class> KMutexGuard<'a, Class> {
    /// Get a reference to the lock token.
    #[inline]
    pub fn token(&self) -> &LockToken<'a, Class> {
        &self.token
    }

    /// Get a mutable reference to the lock token.
    #[inline]
    pub fn token_mut(&mut self) -> &mut LockToken<'a, Class> {
        &mut self.token
    }
}

impl<'a, Class> Drop for KMutexGuard<'a, Class> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: We hold the guard, so we locked it, and we are now unlocking it.
        unsafe {
            self.mutex.inner.unlock();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{KCell, guarded};

    struct MyClass;

    struct MyStruct {
        mu: KMutex<MyClass>,
        data1: KCell<u32, MyClass>,
        data2: KCell<i32, MyClass>,
    }

    #[test]
    fn test_basic_token_access() {
        let s = MyStruct { mu: KMutex::new(), data1: KCell::new(10), data2: KCell::new(-5) };

        let mut guard = s.mu.lock();

        // Immutable access
        // SAFETY: The token is obtained from the same struct instance `s` that contains the cells,
        // satisfying the safe instance-bound invariant.
        unsafe {
            assert_eq!(*s.data1.get(guard.token()), 10);
            assert_eq!(*s.data2.get(guard.token()), -5);
        }

        // Mutable access (one at a time)
        // SAFETY: The token is obtained from the same struct instance `s` that contains the cells,
        // satisfying the safe instance-bound invariant.
        unsafe {
            *s.data1.get_mut(guard.token_mut()) = 20;
            assert_eq!(*s.data1.get(guard.token()), 20);
        }

        // Disjoint mutable access using raw pointers (simulating what the macro will do)
        // SAFETY: This is safe because:
        // 1. We have exclusive access to the LockToken (via &mut LockToken).
        // 2. The fields data1 and data2 are disjoint in MyStruct.
        // 3. The raw pointers are only dereferenced while the lock is held.
        // 4. The token is obtained from the same struct instance `s` that contains the cells.
        unsafe {
            let token_mut = guard.token_mut();
            let p1 = s.data1.as_mut_ptr(token_mut);
            let p2 = s.data2.as_mut_ptr(token_mut);
            *p1 = 30;
            *p2 = -10;
        }

        // SAFETY: The token is from the same instance `s` as the cells.
        unsafe {
            assert_eq!(*s.data1.get(guard.token()), 30);
            assert_eq!(*s.data2.get(guard.token()), -10);
        }
    }

    #[guarded]
    struct MyGuardedStruct {
        #[mutex]
        mu: KMutex,

        #[guarded_by(mu)]
        data1: u32,

        #[guarded_by(mu)]
        data2: i32,
    }

    #[test]
    fn test_macro_guarded() {
        let s = MyGuardedStruct { mu: Default::default(), data1: 100.into(), data2: (-50).into() };

        let mut guard = s.lock_mu();

        // Individual accessors
        assert_eq!(*guard.data1(), 100);
        assert_eq!(*guard.data2(), -50);

        *guard.data1_mut() = 200;
        assert_eq!(*guard.data1(), 200);

        // Split accessors (shared)
        {
            let fields = guard.fields();
            assert_eq!(*fields.data1, 200);
            assert_eq!(*fields.data2, -50);
        }

        // Split accessors (mut)
        {
            let fields = guard.fields_mut();
            *fields.data1 = 300;
            *fields.data2 = -100;
        }

        assert_eq!(*guard.data1(), 300);
        assert_eq!(*guard.data2(), -100);
    }

    #[guarded]
    struct MyMultiGuardedStruct {
        #[mutex]
        mu1: KMutex,
        #[mutex]
        mu2: KMutex,

        #[guarded_by(mu1)]
        data1: u32,

        #[guarded_by(mu2)]
        data2: i32,
    }

    #[test]
    fn test_macro_multi_guarded() {
        let s = MyMultiGuardedStruct {
            mu1: Default::default(),
            mu2: Default::default(),
            data1: 10.into(),
            data2: 20.into(),
        };

        let mut guard1 = s.lock_mu1();
        let mut guard2 = s.lock_mu2();

        assert_eq!(*guard1.data1(), 10);
        assert_eq!(*guard2.data2(), 20);

        *guard1.data1_mut() = 15;
        *guard2.data2_mut() = 25;

        assert_eq!(*guard1.data1(), 15);
        assert_eq!(*guard2.data2(), 25);
    }

    #[test]
    fn test_kmutex_default() {
        let mu: KMutex<MyClass> = KMutex::default();
        let guard = mu.lock();
        let _token = guard.token();
    }

    #[derive(Default)]
    #[guarded]
    struct MyDefaultGuardedStruct {
        #[mutex]
        mu: KMutex,
        #[guarded_by(mu)]
        data: u32,
    }

    #[test]
    fn test_derive_default_guarded() {
        let s: MyDefaultGuardedStruct = Default::default();
        let guard = s.lock_mu();
        assert_eq!(*guard.data(), 0);
    }

    #[derive(Default)]
    #[guarded]
    struct MyGenericGuardedStruct<T> {
        #[mutex]
        mu: KMutex,
        #[guarded_by(mu)]
        data: T,
    }

    #[test]
    fn test_macro_generic_guarded() {
        let s: MyGenericGuardedStruct<u32> = Default::default();
        let mut guard = s.lock_mu();

        // Test safe target accessor (read)
        assert_eq!(*guard.data(), 0);

        // Test safe target accessor (write)
        *guard.data_mut() = 42;
        assert_eq!(*guard.data(), 42);

        // Test split accessors (mut)
        let fields = guard.fields_mut();
        *fields.data = 100;

        // Test split accessors (shared)
        let fields_shared = guard.fields();
        assert_eq!(*fields_shared.data, 100);
    }

    #[test]
    fn test_kmutex_debug() {
        extern crate std;
        let mu: KMutex<MyClass> = KMutex::default();
        let debug_str = std::format!("{:?}", mu);

        assert!(debug_str.contains("KMutex"));
        assert!(debug_str.contains("MyClass"));
    }

    #[derive(Default)]
    #[guarded]
    struct MyExplicitParentGuardedStruct {
        #[mutex]
        mu: KMutex,
        #[guarded_by(mu)]
        data: u32,

        // Un-guarded field
        pub label: &'static str,
    }

    impl MyExplicitParentGuardedStruct {
        // Lock-free parent method
        pub fn has_label(&self) -> bool {
            !self.label.is_empty()
        }
    }

    impl<'a> MyExplicitParentGuardedStructMuGuard<'a> {
        pub fn process_with_context(&mut self) {
            // Explicitly read un-guarded field and call parent method via self.parent.
            let has_label = self.parent.has_label();
            let label = self.parent.label;

            if has_label && label == "apply_update" {
                let fields = self.fields_mut();
                *fields.data = 100;
            }
        }
    }

    #[test]
    fn test_macro_guard_explicit_parent_access() {
        let s = MyExplicitParentGuardedStruct {
            mu: Default::default(),
            data: 0.into(),
            label: "apply_update",
        };

        let mut guard = s.lock_mu();
        guard.process_with_context();

        // Drop guard to unlock
        drop(guard);

        // Verify updates
        let guard = s.lock_mu();
        assert_eq!(*guard.data(), 100);
    }
}
