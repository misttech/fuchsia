// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::object::HandleValue;
use crate::user_copy::{UserInPtr, UserOutPtr};
use syscalls_macro::syscall;
use zx_status::Status;
use zx_types::{zx_handle_t, zx_status_t};

const LOCAL_TRACE: u32 = 0;

unsafe extern "C" {
    fn cpp_object_get_property(
        handle: zx_handle_t,
        property: u32,
        value: *mut core::ffi::c_void,
        size: usize,
    ) -> zx_status_t;

    fn cpp_object_set_property(
        handle: zx_handle_t,
        property: u32,
        value: *const core::ffi::c_void,
        size: usize,
    ) -> zx_status_t;
}

#[syscall]
pub fn sys_object_get_property(
    handle: HandleValue,
    property: u32,
    value: UserOutPtr<u8>,
    size: usize,
) -> Result<(), Status> {
    // SAFETY: Calling C++ implementation.
    let status = unsafe {
        cpp_object_get_property(
            handle.raw_value(),
            property,
            value.as_ptr() as *mut core::ffi::c_void,
            size,
        )
    };
    Status::ok(status)
}

#[syscall]
pub fn sys_object_set_property(
    handle: HandleValue,
    property: u32,
    value: UserInPtr<u8>,
    size: usize,
) -> Result<(), Status> {
    // SAFETY: Calling C++ implementation.
    let status = unsafe {
        cpp_object_set_property(
            handle.raw_value(),
            property,
            value.as_ptr() as *const core::ffi::c_void,
            size,
        )
    };
    Status::ok(status)
}
