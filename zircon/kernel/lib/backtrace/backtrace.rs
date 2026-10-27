// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use libc::stdio::FILE;
use object_constants_rs as object_constants;
use zr::OpaqueBytes;

#[allow(improper_ctypes)]
unsafe extern "C" {
    fn cpp_backtrace_print(bt: *const Backtrace);
    fn cpp_backtrace_print_without_version(bt: *const Backtrace, file: *mut FILE);
}

/// Contains an array of return address values representing a thread's backtrace.
///
/// This type is not thread-safe.
#[repr(C, align(8))]
pub struct Backtrace {
    _opaque: OpaqueBytes<{ object_constants::kBacktraceSize }>,
}

zr::static_assert_size_and_align!(
    Backtrace,
    object_constants::kBacktraceSize,
    object_constants::kBacktraceAlign,
);

impl Backtrace {
    /// Creates an empty `Backtrace`.
    #[inline]
    pub const fn new() -> Self {
        Self { _opaque: OpaqueBytes::new([0; object_constants::kBacktraceSize]) }
    }

    /// Pretty-prints this backtrace to `stdout`.
    #[inline]
    pub fn print(&self) {
        // SAFETY: `self` is a valid initialized `Backtrace`.
        unsafe { cpp_backtrace_print(self) }
    }

    /// Pretty-prints this backtrace to `file` without the kernel version preamble.
    ///
    /// # Safety
    ///
    /// `file` must point to a valid `FILE` for the duration of the call.
    #[inline]
    pub unsafe fn print_without_version(&self, file: *mut FILE) {
        // SAFETY: `self` is a valid initialized `Backtrace`, and the caller guarantees `file`
        // points to a valid `FILE`.
        unsafe { cpp_backtrace_print_without_version(self, file) }
    }
}

impl Default for Backtrace {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}
