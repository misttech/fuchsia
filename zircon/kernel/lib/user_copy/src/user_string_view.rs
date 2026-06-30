// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::user_ptr::UserInPtr;
use zx_status::Status;

/// A wrapper around `zx_string_view_t` passed from userspace into kernel syscalls.
///
/// This struct has exact memory layout parity with `zx_string_view_t` (`const char*`, `size_t`),
/// ensuring ABI compatibility when passed by value across FFI boundaries.
#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
pub struct UserStringView {
    /// Pointer to userspace UTF-8 bytes.
    pub data: UserInPtr<u8>,
    /// Length of the string in bytes.
    pub length: usize,
}

impl UserStringView {
    /// Returns true if the string view is empty.
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// Returns the length of the string view in bytes.
    pub fn len(&self) -> usize {
        self.length
    }

    /// Copies the string bytes from userspace into a destination slice.
    ///
    /// Returns `Status::INVALID_ARGS` if the slice is smaller than `length`.
    pub fn copy_slice_from_user<'a>(
        &self,
        dst: &'a mut [core::mem::MaybeUninit<u8>],
    ) -> Result<&'a mut [u8], Status> {
        if dst.len() < self.length {
            return Err(Status::INVALID_ARGS);
        }
        self.data.copy_slice_from_user(&mut dst[..self.length])
    }
}
