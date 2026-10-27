// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::{VaList, c_char, c_void};

unsafe extern "C" {
    /// Returns the return address of the specified call frame (`level == 0` for the current
    /// function).
    ///
    /// # Safety
    ///
    /// `level` must be `0`.
    #[link_name = "llvm.returnaddress"]
    pub(super) fn return_address(level: i32) -> *mut c_void;

    /// Returns the frame pointer of the specified call frame (`level == 0` for the current
    /// function).
    ///
    /// # Safety
    ///
    /// `level` must be `0`.
    #[link_name = "llvm.frameaddress"]
    pub(super) fn frame_address(level: i32) -> *mut c_void;
}

/// Inline functions in libc++ headers call this via `std::__libcpp_verbose_abort`.
///
/// # Safety
///
/// `fmt` must be non-null and point to a valid NUL-terminated C format string whose format
/// specifiers match the arguments in `ap`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vpanic(
    pc: *mut c_void,
    frame: *mut c_void,
    fmt: *const c_char,
    ap: VaList<'_>,
) -> ! {
    // SAFETY: Upheld by the caller per this function's `# Safety` contract.
    unsafe { super::debug::vpanic(pc, frame, fmt, ap) }
}
