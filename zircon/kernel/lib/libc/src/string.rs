// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_char;

unsafe extern "C" {
    /// Returns the length in bytes of the NUL-terminated string `s`.
    ///
    /// # Safety
    ///
    /// `s` must be non-null and point to a valid NUL-terminated C string.
    pub fn strlen(s: *const c_char) -> usize;
}
