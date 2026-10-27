// Copyright 2021 The Fuchsia Authors
// Copyright (c) 2008 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::{c_char, c_int, c_void};
use core::fmt;
use core::marker::PhantomData;

use super::FILE;

/// `FILE*` wrapper over a string.
///
/// The last byte of the buffer is always reserved for a null character, which is only written by
/// [`StringFile::take`].
#[derive(Debug)]
pub struct StringFile<'a> {
    buffer: &'a mut [u8],
    offset: usize,
}

impl<'a> StringFile<'a> {
    /// Creates a `StringFile` that writes into `buffer`.
    pub fn new(buffer: &'a mut [u8]) -> Self {
        Self { buffer, offset: 0 }
    }

    /// Adds a null character at the end of the current buffer and returns a view into the written
    /// section.
    pub fn take(self) -> &'a mut [u8] {
        if self.buffer.is_empty() {
            return &mut [];
        }
        self.buffer[self.offset] = 0;
        &mut self.buffer[..self.offset + 1]
    }

    /// Returns `bytes.len()` and writes as much of `bytes` as it would fit in the buffer
    /// `[offset, size - 1)` while reserving the last byte for null character.
    // Kept out of line so that callers don't each get a copy of the bounds checks.
    #[inline(never)]
    pub fn write(&mut self, bytes: &[u8]) -> usize {
        if self.buffer.is_empty() {
            return bytes.len();
        }
        // Last element is reserved for '\0' character.
        let count = bytes.len().min(self.buffer.len() - self.offset - 1);
        self.buffer[self.offset..self.offset + count].copy_from_slice(&bytes[..count]);
        self.offset += count;
        bytes.len()
    }

    /// Returns a region representing the currently used portion of the buffer.
    pub fn used_region(&self) -> &[u8] {
        &self.buffer[..self.offset]
    }

    /// Returns a region representing the remaining unused space in the buffer, _not_ including the
    /// space reserved for the final null character.
    pub fn available_region(&mut self) -> &mut [u8] {
        let available = (self.buffer.len() - self.offset).saturating_sub(1);
        &mut self.buffer[self.offset..self.offset + available]
    }

    /// Skip up to `amount` bytes in the buffer, advancing the write pointer as if we had written
    /// data, but without actually changing the buffer.
    pub fn skip(&mut self, amount: usize) {
        let amount = amount.min(self.available_region().len());
        self.offset += amount;
    }

    /// Returns the currently used portion of the buffer, like C++ `as_string_view()`.
    pub fn as_bytes(&self) -> &[u8] {
        self.used_region()
    }

    /// Calls `f` with a C++-compatible `FILE` whose output is written into this `StringFile`.
    ///
    /// This lets C++ routines that render into a `FILE*` write into a Rust `StringFile`.
    pub fn with_file<R>(&mut self, f: impl for<'f> FnOnce(&mut ScopedFile<'f>) -> R) -> R {
        // SAFETY: `file_write` is sound to call with a pointer to this `StringFile` and any
        // readable `(data, len)`. The `FILE` is only reachable through the `ScopedFile` lent to
        // `f`, which safe code can neither move out of nor swap with another `ScopedFile`, so the
        // `FILE` cannot outlive this call, during which `self` is mutably borrowed and the pointer
        // stays valid and unaliased.
        let file =
            unsafe { FILE::new(Some(Self::file_write), (self as *mut Self).cast::<c_void>()) };
        f(&mut ScopedFile { file, _brand: PhantomData })
    }

    /// The `FILE` callback installed by [`StringFile::with_file`].
    ///
    /// # Safety
    ///
    /// `ptr` must point to a live `StringFile` that is not otherwise accessed during the call, and
    /// `data` must point to `len` readable bytes when `len > 0`.
    unsafe extern "C" fn file_write(ptr: *mut c_void, data: *const c_char, len: usize) -> c_int {
        // SAFETY: `with_file` passes a pointer to the `StringFile` it mutably borrows for the
        // lifetime of the `FILE`, so no other access can happen while the callback runs.
        let this = unsafe { &mut *ptr.cast::<StringFile<'_>>() };
        let bytes = if len == 0 {
            &[]
        } else {
            // SAFETY: `len > 0` and the caller guarantees `data` points to `len` readable bytes.
            unsafe { core::slice::from_raw_parts(data.cast::<u8>(), len) }
        };
        this.write(bytes) as c_int
    }
}

impl AsRef<[u8]> for StringFile<'_> {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl fmt::Write for StringFile<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write(s.as_bytes());
        Ok(())
    }
}

/// The `FILE` lent to the closure passed to [`StringFile::with_file`].
///
/// The `FILE` points back at its `StringFile`, so it must not outlive the `with_file` call. Safe
/// code only ever gets a `&mut ScopedFile<'f>` whose `'f` is unique to one `with_file` call, and
/// can never get a `&mut FILE`, so the `FILE` can be neither moved out nor swapped with the `FILE`
/// of another (possibly shorter-lived) `StringFile`.
pub struct ScopedFile<'f> {
    file: FILE,
    // Makes `ScopedFile` invariant in `'f`, so `ScopedFile`s from different `with_file` calls have
    // distinct types and cannot be swapped.
    _brand: PhantomData<fn(&'f ()) -> &'f ()>,
}

impl ScopedFile<'_> {
    /// Writes `bytes` to the `StringFile`, like [`FILE::write`].
    pub fn write(&self, bytes: &[u8]) -> c_int {
        self.file.write(bytes)
    }

    /// Returns a pointer to the `FILE`, for passing to C++ routines that take a `FILE*`.
    ///
    /// The pointer must not be used after the `with_file` closure returns.
    pub fn as_mut_ptr(&mut self) -> *mut FILE {
        &raw mut self.file
    }
}

impl fmt::Write for ScopedFile<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        fmt::Write::write_str(&mut self.file, s)
    }
}
