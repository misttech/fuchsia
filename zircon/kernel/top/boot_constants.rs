// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Accessors for the strings in `kBootConstants`.

use zr::slice_from_raw_parts;

unsafe extern "C" {
    /// Returns a pointer to `kBootConstants.kernel_version_ident` and writes its byte length to
    /// `size`.
    fn cpp_boot_constants_kernel_version_ident(size: &mut usize) -> *const u8;

    /// Returns a pointer to `kBootConstants.kernel_debug_ident` and writes its byte length to
    /// `size`.
    fn cpp_boot_constants_kernel_debug_ident(size: &mut usize) -> *const u8;
}

/// Returns `kBootConstants.kernel_version_ident`: the human-readable prefix of the debug ident.
pub(crate) fn kernel_version_ident() -> &'static [u8] {
    let mut size = 0;
    // SAFETY: The shim only reads `kBootConstants` and writes `size`. It returns a pointer to
    // `size` bytes of permanent handoff memory, which physboot fills in and the kernel never
    // modifies or frees, so the bytes are valid for `'static`.
    unsafe {
        let ptr = cpp_boot_constants_kernel_version_ident(&mut size);
        slice_from_raw_parts(ptr, size)
    }
}

/// Returns `kBootConstants.kernel_debug_ident`: the version ident followed by the kernel's full
/// symbolizer markup context.
pub(crate) fn kernel_debug_ident() -> &'static [u8] {
    let mut size = 0;
    // SAFETY: The shim only reads `kBootConstants` and writes `size`. It returns a pointer to
    // `size` bytes of permanent handoff memory, which physboot fills in and the kernel never
    // modifies or frees, so the bytes are valid for `'static`.
    unsafe {
        let ptr = cpp_boot_constants_kernel_debug_ident(&mut size);
        slice_from_raw_parts(ptr, size)
    }
}
