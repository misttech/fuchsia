// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::{VaList, c_char, c_int, c_void};
use core::fmt;
use core::mem::{align_of, size_of};

mod string_file;

pub use string_file::{ScopedFile, StringFile};

/// The C++ `FILE::Callback` signature: `int(void* ptr, const char* str, size_t len)`.
pub type FileCallback = unsafe extern "C" fn(*mut c_void, *const c_char, usize) -> c_int;

/// This represents the kernel `<stdio.h>` `FILE` type.
#[repr(C)]
pub struct FILE {
    write: Option<FileCallback>,
    ptr: *mut c_void,
}

const _: () = {
    assert!(size_of::<FILE>() == 16);
    assert!(align_of::<FILE>() == 8);
};

impl FILE {
    /// Creates a `FILE` that sends everything written to it to `write`, passing `ptr` as the
    /// callback's first argument, like the C++ `FILE(Callback* write, void* ptr)` constructor.
    ///
    /// # Safety
    ///
    /// For the lifetime of the returned `FILE`, `write` (if present) must be sound to call with
    /// `ptr` and any `(data, len)` where `data` points to `len` readable bytes when `len > 0` (and
    /// may be null when `len == 0`).
    pub const unsafe fn new(write: Option<FileCallback>, ptr: *mut c_void) -> Self {
        Self { write, ptr }
    }

    /// Writes `bytes` via the callback, like the C++ `FILE::Write`. This is what `fprintf` calls
    /// to do output.
    ///
    /// Returns the callback's result, or -1 if this `FILE` has no callback.
    pub fn write(&self, bytes: &[u8]) -> c_int {
        let Some(write) = self.write else {
            return -1;
        };
        // SAFETY:
        // - `self.ptr` is the context uniquely paired with `write` by the C API.
        // - `bytes` points to a valid sequence of `bytes.len()` bytes that remains valid and
        //   immutable for the duration of the FFI call.
        unsafe { write(self.ptr, bytes.as_ptr().cast::<c_char>(), bytes.len()) }
    }
}

impl fmt::Write for FILE {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let Some(write) = self.write else {
            return Err(fmt::Error);
        };
        if s.is_empty() {
            return Ok(());
        }
        let c_chars = s.as_ptr().cast::<c_char>();
        // SAFETY:
        // - `self.ptr` is the context uniquely paired with `write` by the C API.
        // - `s` is not empty, so `c_chars` points to a valid sequence of `s.len()` bytes.
        // - The `s` memory region remains valid and immutable for the duration of the FFI call.
        let ret = unsafe { write(self.ptr, c_chars, s.len()) };
        if ret < 0 { Err(fmt::Error) } else { Ok(()) }
    }
}

unsafe extern "C" {
    /// Writes formatted output to `f`, matching `<stdio.h>` `fprintf`.
    ///
    /// # Safety
    ///
    /// `f` must point to a valid `FILE`. `fmt` must be non-null and point to a valid
    /// NUL-terminated C format string whose format specifiers match the variadic arguments.
    pub fn fprintf(f: *mut FILE, fmt: *const c_char, ...) -> c_int;

    /// Writes `VaList` formatted output to `f`, matching `<stdio.h>` `vfprintf`.
    ///
    /// # Safety
    ///
    /// `f` must point to a valid `FILE`. `fmt` must be non-null and point to a valid
    /// NUL-terminated C format string whose format specifiers match the arguments in `ap`.
    pub fn vfprintf(f: *mut FILE, fmt: *const c_char, ap: VaList<'_>) -> c_int;
}
