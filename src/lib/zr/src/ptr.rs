// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/// Extension trait for references to obtain a mutable raw pointer.
pub trait ToMutPtr {
    /// The target type of the pointer.
    type Target: ?Sized;

    /// Casts the reference to a mutable raw pointer.
    fn to_mut_ptr(&self) -> *mut Self::Target;
}

impl<T: ?Sized> ToMutPtr for T {
    type Target = T;

    #[inline(always)]
    fn to_mut_ptr(&self) -> *mut T {
        self as *const T as *mut T
    }
}
