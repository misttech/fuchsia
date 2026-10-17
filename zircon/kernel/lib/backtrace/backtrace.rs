// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use object_constants_rs as object_constants;
use zr::OpaqueBytes;

#[allow(improper_ctypes)]
unsafe extern "C" {
    fn cpp_backtrace_print(bt: *const Backtrace);
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
}

impl Default for Backtrace {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}
