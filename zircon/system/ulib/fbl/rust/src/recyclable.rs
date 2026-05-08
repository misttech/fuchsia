// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_void;
use core::ptr::NonNull;

pub trait Recyclable: Sized {
    /// Recycles the object.
    ///
    /// # Safety
    ///
    /// - The caller must ensure that `ptr` points to a valid, fully-initialized
    ///   instance of `Self` that has no other references.
    /// - The caller must not use the pointer or any references derived from it
    ///   after this call.
    unsafe fn recycle(ptr: NonNull<Self>);

    /// Helper for FFI functions to call `recycle`.
    ///
    /// # Safety
    ///
    /// - The caller must ensure that `ptr` is valid and points to a valid object
    ///   of type `Self` that has no other references.
    /// - The caller must not use the pointer or any references derived from it
    ///   after this call.
    unsafe fn recycle_ffi(ptr: *mut c_void) {
        // SAFETY: The caller of `recycle_ffi` must ensure that `ptr` is non-null
        // and points to a valid `Self` that can be safely recycled.
        unsafe {
            Self::recycle(NonNull::new_unchecked(ptr as *mut Self));
        }
    }
}
