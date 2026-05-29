// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;

/// A wrapper for types that are opaque to Rust.
///
/// This is used to wrap C++ objects that Rust should not access directly.
/// It provides a raw pointer to the inner data for use in FFI.
#[repr(transparent)]
pub struct Opaque<T>(MaybeUninit<UnsafeCell<T>>);

impl<T> Opaque<T> {
    /// Creates a new `Opaque` value.
    pub const fn new(value: T) -> Self {
        Self(MaybeUninit::new(UnsafeCell::new(value)))
    }

    /// Creates an uninitialized `Opaque` value.
    pub const fn uninit() -> Self {
        Self(MaybeUninit::uninit())
    }

    /// Returns a raw pointer to the opaque data.
    pub const fn get(&self) -> *mut T {
        let ptr = self.0.as_ptr();
        UnsafeCell::raw_get(ptr)
    }
}
