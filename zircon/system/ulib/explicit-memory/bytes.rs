// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Safe wrapper and C bindings for explicit memory clearing.

#![no_std]

use core::ops::{Deref, DerefMut};
use core::ptr;
use zerocopy::FromZeros;

unsafe extern "C" {
    /// Invocations of this implementation are guaranteed to not be optimized away.
    ///
    /// Writes `c` (converted to an `unsigned char`) into each of the first `n` bytes of the
    /// object pointed to by `dst`, even if `dst` will not be read from again for the rest of
    /// its lifetime.
    pub fn mandatory_memset(dst: *mut u8, c: core::ffi::c_int, n: usize) -> *mut u8;
}

/// Guarantees that the wrapped value will be zeroed using [`mandatory_memset`] when dropped.
///
/// This prevents sensitive data held in `self` from leaking after going out of scope,
/// ensuring that the zeroing cannot be elided by the compiler.
pub struct ZeroOnDrop<T: FromZeros>(T);

impl<T: FromZeros> ZeroOnDrop<T> {
    /// Creates a new [`ZeroOnDrop`] wrapping `val`.
    pub const fn new(val: T) -> Self {
        Self(val)
    }
}

impl<T: FromZeros> Deref for ZeroOnDrop<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T: FromZeros> DerefMut for ZeroOnDrop<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T: FromZeros> Drop for ZeroOnDrop<T> {
    fn drop(&mut self) {
        // SAFETY:
        // 1. `ptr::drop_in_place` drops `self.0` before zeroing to release any resources
        //    the underlying `T` instance might hold.
        // 2. `&mut self.0` is an exclusive reference to a live `T` within `self`, valid
        //    for writes of `size_of::<T>()` bytes. Because `T: FromZeros`, all zeroes is a valid
        //    bit pattern for `T`.
        unsafe {
            ptr::drop_in_place(ptr::from_mut(&mut self.0));
            mandatory_memset(ptr::from_mut(&mut self.0).cast::<u8>(), 0, size_of::<T>());
        }
    }
}
