// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::mem::{MaybeUninit, forget};
use core::ptr::copy_nonoverlapping;

use crate::CopyOptimization;

/// A type which is convertible from a wire type.
pub trait FromWire<W>: Sized {
    /// Whether the conversion from `W` to `Self` is equivalent to copying the raw bytes of `W`.
    ///
    /// Copy optimization is disabled by default.
    const COPY_OPTIMIZATION: CopyOptimization<W, Self> = CopyOptimization::disable();

    /// Converts the given wire value to this type.
    fn from_wire(wire: W) -> Self;
}

/// An optional type which is convertible from a wire type.
pub trait FromWireOption<W>: Sized {
    /// Converts the given wire value to an option of this type.
    fn from_wire_option(wire: W) -> Option<Self>;
}

impl<T: FromWire<W>, W, const N: usize> FromWire<[W; N]> for [T; N] {
    fn from_wire(wire: [W; N]) -> Self {
        let mut result = MaybeUninit::<[T; N]>::uninit();
        if T::COPY_OPTIMIZATION.is_enabled() {
            // SAFETY: `T` has copy optimization enabled and so is safe to copy bytewise.
            unsafe {
                copy_nonoverlapping(wire.as_ptr().cast(), result.as_mut_ptr(), 1);
            }
            forget(wire);
        } else {
            for (i, item) in wire.into_iter().enumerate() {
                // SAFETY: `i` is in bounds for the array of size `N`, and the pointer is valid to
                // write.
                unsafe {
                    result.as_mut_ptr().cast::<T>().add(i).write(T::from_wire(item));
                }
            }
        }
        // SAFETY: All `N` elements of `result` have been initialized.
        unsafe { result.assume_init() }
    }
}

impl<'a, T: FromWire<&'a W>, W, const N: usize> FromWire<&'a [W; N]> for [T; N] {
    fn from_wire(wire: &'a [W; N]) -> Self {
        let mut result = MaybeUninit::<[T; N]>::uninit();
        if T::COPY_OPTIMIZATION.is_enabled() {
            // SAFETY: `T` has copy optimization enabled and so is safe to copy bytewise.
            unsafe {
                copy_nonoverlapping(wire.as_ptr().cast(), result.as_mut_ptr(), 1);
            }
        } else {
            for (i, item) in wire.iter().enumerate() {
                // SAFETY: `i` is in bounds for the array of size `N`, and the pointer is valid to
                // write.
                unsafe {
                    result.as_mut_ptr().cast::<T>().add(i).write(T::from_wire(item));
                }
            }
        }
        // SAFETY: All `N` elements of `result` have been initialized.
        unsafe { result.assume_init() }
    }
}

impl<T: FromWire<W>, W> FromWire<W> for Box<T> {
    fn from_wire(wire: W) -> Self {
        Box::new(T::from_wire(wire))
    }
}

impl<T: FromWireOption<W>, W> FromWire<W> for Option<T> {
    fn from_wire(wire: W) -> Self {
        T::from_wire_option(wire)
    }
}
