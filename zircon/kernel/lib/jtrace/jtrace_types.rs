// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Rust bindings and field/entry helper types for jump trace (`jtrace`).

#![no_std]

use core::ffi::CStr;
use core::mem::size_of;
use core::ptr::NonNull;

use fbl::{HasRefCount, Recyclable, RefPtr};
use kalloc::Box;

#[expect(non_camel_case_types)]
mod sys {
    include!(rustenv_path::envpath!("JTRACE_BINDINGS_RS"));
}

pub use sys::{
    jtrace_IsPersistent as IsPersistent, jtrace_LargeEntry as LargeEntry,
    jtrace_SmallEntry as SmallEntry, jtrace_TraceBufferType as TraceBufferType,
    jtrace_UseLargeEntries as UseLargeEntries, jtrace_UseMonoTimestamps as UseMonoTimestamps,
    jtrace_internal_FileFuncLineInfo as FileFuncLineInfo,
};

// All arguments provided to trace entries are either 32 or 64 bits, and will be
// rendered as just hex when the trace needs to be dumped. Define a few helper
// structs which will allow users to pass _any_ data type to a trace entry,
// provided that it will fit in the storage.

/// Helper converting up to 32-bit non-float values into a `u32` trace field
/// (`jtrace::internal::Field32`).
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field32 {
    /// Stored 32-bit field value.
    pub val: u32,
}

impl Field32 {
    /// Creates a `Field32` from a `u32`.
    #[inline]
    pub const fn new(val: u32) -> Self {
        Self { val }
    }
}

// Macro used to implement `From<T>` for primitive integer types up to 32 bits because Rust
// coherence rules forbid a blanket `impl<T> From<T> for Field32` (which conflicts with
// reflexive `impl<T> From<T> for T`).
macro_rules! impl_from_for_field32 {
    ($($t:ty),* $(,)?) => {
        $(
            impl From<$t> for Field32 {
                #[inline]
                fn from(v: $t) -> Self {
                    const { assert!(size_of::<$t>() <= 4) };
                    Self { val: v as u32 }
                }
            }
        )*
    };
}

impl_from_for_field32!(u8, u16, u32, i8, i16, i32, char);

/// Helper converting up to 64-bit non-float values and pointers into a `u64` trace field
/// (`jtrace::internal::Field64`).
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field64 {
    /// Stored 64-bit field value.
    pub val: u64,
}

impl Field64 {
    /// Creates a `Field64` from a `u64`.
    #[inline]
    pub const fn new(val: u64) -> Self {
        Self { val }
    }
}

// Macro used to implement `From<T>` for primitive integer types up to 64 bits because Rust
// coherence rules forbid a blanket `impl<T> From<T> for Field64` (which conflicts with
// reflexive `impl<T> From<T> for T`).
macro_rules! impl_from_for_field64 {
    ($($t:ty),* $(,)?) => {
        $(
            impl From<$t> for Field64 {
                #[inline]
                fn from(v: $t) -> Self {
                    const { assert!(size_of::<$t>() <= 8) };
                    Self { val: v as u64 }
                }
            }
        )*
    };
}

impl_from_for_field64!(u8, u16, u32, u64, usize, i8, i16, i32, i64, isize, char);

impl<T> From<*const T> for Field64 {
    #[inline]
    fn from(ptr: *const T) -> Self {
        Self { val: ptr.addr() as u64 }
    }
}

impl<T> From<*mut T> for Field64 {
    #[inline]
    fn from(ptr: *mut T) -> Self {
        Self { val: ptr.addr() as u64 }
    }
}

impl<T> From<NonNull<T>> for Field64 {
    #[inline]
    fn from(ptr: NonNull<T>) -> Self {
        Self { val: ptr.addr().get() as u64 }
    }
}

impl<T> From<&Box<T>> for Field64 {
    #[inline]
    fn from(b: &Box<T>) -> Self {
        Self { val: Box::as_ptr(b).addr() as u64 }
    }
}

impl<T: HasRefCount + Recyclable> From<&RefPtr<T>> for Field64 {
    #[inline]
    fn from(r: &RefPtr<T>) -> Self {
        Self { val: RefPtr::as_ptr(r).addr() as u64 }
    }
}

impl FileFuncLineInfo {
    /// Creates a new `FileFuncLineInfo` from static C string literals and a line number.
    #[inline]
    pub const fn new(file: &'static CStr, func: &'static CStr, line: i32) -> Self {
        Self { file: file.as_ptr(), func: func.as_ptr(), line, __bindgen_padding_0: [0; 4] }
    }
}

// SAFETY: `FileFuncLineInfo` has no interior mutability, and its `file` and `func` pointers
// only ever point to immutable `'static` C string literals.
unsafe impl Sync for FileFuncLineInfo {}

impl Default for SmallEntry {
    #[inline]
    fn default() -> Self {
        Self { ts_ticks: 0, tag: core::ptr::null(), cpu_id: 0, a: 0 }
    }
}

impl SmallEntry {
    /// Creates a new `SmallEntry` matching C++
    /// `Entry<UseLargeEntries::No>(tag, ffl_info, a)`.
    #[inline]
    pub fn new(
        tag: &'static CStr,
        _ffl_info: &'static FileFuncLineInfo,
        a: impl Into<Field32>,
    ) -> Self {
        Self { ts_ticks: 0, tag: tag.as_ptr(), cpu_id: 0, a: a.into().val }
    }
}

impl Default for LargeEntry {
    #[inline]
    fn default() -> Self {
        Self {
            ts_ticks: 0,
            tag: core::ptr::null(),
            ffl_info: core::ptr::null(),
            tid: 0,
            e: 0,
            f: 0,
            a: 0,
            b: 0,
            c: 0,
            d: 0,
            cpu_id: 0,
            padding: 0,
        }
    }
}

impl LargeEntry {
    /// Creates a new `LargeEntry` matching C++
    /// `Entry<UseLargeEntries::Yes>(tag, ffl_info, a, b, c, d, e, f)`.
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tag: &'static CStr,
        ffl_info: &'static FileFuncLineInfo,
        a: impl Into<Field32>,
        b: impl Into<Field32>,
        c: impl Into<Field32>,
        d: impl Into<Field32>,
        e: impl Into<Field64>,
        f: impl Into<Field64>,
    ) -> Self {
        Self {
            ts_ticks: 0,
            tag: tag.as_ptr(),
            ffl_info,
            tid: 0,
            e: e.into().val,
            f: f.into().val,
            a: a.into().val,
            b: b.into().val,
            c: c.into().val,
            d: d.into().val,
            cpu_id: 0,
            padding: 0,
        }
    }
}
