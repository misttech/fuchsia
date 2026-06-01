// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::lock_token::LockToken;
use core::marker::PhantomData;

/// A container cell that safe-guards data of type `T` using type-level lock class tracking.
///
/// `KCell` holds data that can only be safely accessed (read or written) by proving that the
/// corresponding mutual exclusion lock of class `Class` is currently held by the current thread
/// via a `LockToken`.
#[repr(transparent)]
pub struct KCell<T, Class> {
    value: core::cell::UnsafeCell<T>,
    _marker: PhantomData<Class>,
}

unsafe impl<T: Send, Class> Sync for KCell<T, Class> {}
unsafe impl<T: Send, Class> Send for KCell<T, Class> {}

impl<T, Class> KCell<T, Class> {
    /// Creates a new `KCell` containing the specified `value`.
    #[inline]
    pub const fn new(value: T) -> Self {
        Self { value: core::cell::UnsafeCell::new(value), _marker: PhantomData }
    }

    /// Access the guarded value immutably using a shared lock token.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that:
    /// 1. The provided `LockToken` belongs to the specific lock instance that guards this `KCell`
    ///    (rather than a different lock of the same lock class `Class`).
    /// 2. The lock is held continuously for the lifetime of the returned reference `'b`.
    #[inline]
    pub unsafe fn get<'b>(&self, _token: &'b LockToken<'_, Class>) -> &'b T {
        // SAFETY: The caller guarantees that the correct lock instance is held continuously
        // for the duration of the reference lifetime `'b`, ensuring safe immutable access to
        // the inner value without data races.
        unsafe { &*self.value.get() }
    }

    /// Access the guarded value mutably using a mutable lock token.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that:
    /// 1. The provided `LockToken` belongs to the specific lock instance that guards this `KCell`
    ///    (rather than a different lock of the same lock class `Class`).
    /// 2. The lock is held continuously for the lifetime of the returned reference `'b`.
    #[inline]
    pub unsafe fn get_mut<'b>(&self, _token: &'b mut LockToken<'_, Class>) -> &'b mut T {
        // SAFETY: The caller guarantees that the correct lock instance is held continuously for the
        // duration of the reference lifetime `'b`, and the exclusive mutable borrow of the
        // `LockToken` ensures that no other active borrows of the same cell can co-exist,
        // permitting safe mutable projection from the inner `UnsafeCell` without aliasing or data
        // races.
        unsafe { &mut *self.value.get() }
    }

    /// Returns a mutable raw pointer to the guarded value.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that the lock instance guarding this `KCell` is held continuously
    /// while dereferencing or accessing the returned raw pointer.
    #[inline]
    pub unsafe fn as_mut_ptr(&self, _token: &mut LockToken<'_, Class>) -> *mut T {
        self.value.get()
    }

    /// Accesses the inner value mutably by bypassing the locking requirements using unique borrow
    /// ownership.
    #[inline]
    pub fn get_inner_mut(&mut self) -> &mut T {
        self.value.get_mut()
    }

    /// Unwraps the cell, returning the inner value.
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

#[cfg(not(feature = "kernel"))]
#[cfg(test)]
mod tests {
    use super::*;
    use lockdep::LockClass;

    struct MyClass;
    impl LockClass for MyClass {
        const ID: *mut core::ffi::c_void = core::ptr::null_mut();
    }

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
