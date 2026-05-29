// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::lock_token::LockToken;
use core::cell::UnsafeCell;
use core::marker::PhantomData;

/// A cell that guards a value of type `T` behind a lock of type `Class`.
///
/// To access the value, a `LockToken` for the correct `Class` must be presented.
#[repr(transparent)]
pub struct KCell<T, Class> {
    value: UnsafeCell<T>,
    _marker: PhantomData<Class>,
}

// SAFETY: KCell is Sync if T is Send, because access is exclusive via the lock token.
unsafe impl<T: Send, Class> Sync for KCell<T, Class> {}

// SAFETY: KCell is Send if T is Send.
unsafe impl<T: Send, Class> Send for KCell<T, Class> {}

impl<T, Class> KCell<T, Class> {
    /// Create a new `KCell`.
    #[inline]
    pub const fn new(value: T) -> Self {
        Self { value: UnsafeCell::new(value), _marker: PhantomData }
    }

    /// Access the value immutably using a shared lock token.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that the presented `token` was obtained from the specific `KMutex`
    /// instance that logically protects this specific `KCell` instance (usually the mutex field in
    /// the same parent structure).
    #[inline]
    pub unsafe fn get<'b>(&self, _token: &'b LockToken<'_, Class>) -> &'b T {
        // SAFETY: The safety invariant of this function guarantees that the token is from the
        // correct mutex instance for this cell, meaning the associated lock is held.
        // The lifetime of the returned reference is tied to the token borrow, preventing
        // concurrent mutable access on this thread.
        unsafe { &*self.value.get() }
    }

    /// Access the value mutably using a mutable lock token.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that the presented `token` was obtained from the specific `KMutex`
    /// instance that logically protects this specific `KCell` instance (usually the mutex field in
    /// the same parent structure).
    #[inline]
    pub unsafe fn get_mut<'b>(&self, _token: &'b mut LockToken<'_, Class>) -> &'b mut T {
        // SAFETY: The safety invariant of this function guarantees that the token is from the
        // correct mutex instance for this cell, meaning the associated lock is held exclusively.
        unsafe { &mut *self.value.get() }
    }

    /// Get a mutable raw pointer to the value.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that the presented `token` was obtained from the specific `KMutex`
    /// instance that logically protects this specific `KCell` instance (usually the mutex field in
    /// the same parent structure).
    ///
    /// This is also unsafe because it returns a raw pointer without lifetime guarantees.
    #[inline]
    pub unsafe fn as_mut_ptr(&self, _token: &mut LockToken<'_, Class>) -> *mut T {
        self.value.get()
    }

    /// Access the value mutably without a token if you have exclusive access to the cell.
    #[inline]
    pub fn get_inner_mut(&mut self) -> &mut T {
        self.value.get_mut()
    }

    /// Consume the cell and return the inner value safely.
    #[inline]
    pub fn into_inner(self) -> T {
        self.value.into_inner()
    }
}

impl<T: Default, Class> Default for KCell<T, Class> {
    #[inline]
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T, Class> From<T> for KCell<T, Class> {
    #[inline]
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

impl<T, Class> AsMut<T> for KCell<T, Class> {
    #[inline]
    fn as_mut(&mut self) -> &mut T {
        self.get_inner_mut()
    }
}

impl<T, Class> core::fmt::Debug for KCell<T, Class> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("KCell")
            .field("value", &"<locked>")
            .field("class", &core::any::type_name::<Class>())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MyClass;

    #[test]
    fn test_kcell_default() {
        let cell: KCell<u32, MyClass> = KCell::default();
        unsafe {
            let token = LockToken::new();
            assert_eq!(*cell.get(&token), 0);
        }
    }

    #[test]
    fn test_kcell_exclusive_access() {
        let mut cell: KCell<u32, MyClass> = KCell::new(10);
        *cell.get_inner_mut() = 20;
        let reference: &mut u32 = cell.as_mut();
        *reference = 30;
        let value = cell.into_inner();
        assert_eq!(value, 30);
    }

    #[test]
    fn test_kcell_as_mut_ptr() {
        let cell: KCell<u32, MyClass> = KCell::new(100u32);
        unsafe {
            let mut token = LockToken::new();
            let ptr = cell.as_mut_ptr(&mut token);
            assert_eq!(*ptr, 100);
        }
    }

    #[test]
    fn test_kcell_debug() {
        extern crate std;
        let cell: KCell<u32, MyClass> = KCell::new(5);
        let debug_str = std::format!("{:?}", cell);
        assert!(debug_str.contains("KCell"));
        assert!(debug_str.contains("<locked>"));
        assert!(debug_str.contains("MyClass"));
    }
}
