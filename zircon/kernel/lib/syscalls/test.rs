// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]

use syscalls_macro::syscall;
use user_copy::{UserInOutPtr, UserInPtr, UserOutPtr};
use zx_status::Status;

#[syscall]
pub fn sys_syscall_test_rust_0() -> Status {
    Status::OK
}

#[syscall]
pub fn sys_syscall_test_rust_1(a: i32) -> Status {
    Status::from_raw(a)
}

#[syscall]
pub fn sys_syscall_test_rust_2(a: i32, b: i32) -> Status {
    Status::from_raw(a.wrapping_add(b))
}

#[syscall]
pub fn sys_syscall_test_rust_3(a: i32, b: i32, c: i32) -> Status {
    Status::from_raw(a.wrapping_add(b).wrapping_add(c))
}

#[syscall]
pub fn sys_syscall_test_rust_4(a: i32, b: i32, c: i32, d: i32) -> Status {
    Status::from_raw(a.wrapping_add(b).wrapping_add(c).wrapping_add(d))
}

#[syscall]
pub fn sys_syscall_test_rust_5(a: i32, b: i32, c: i32, d: i32, e: i32) -> Status {
    Status::from_raw(a.wrapping_add(b).wrapping_add(c).wrapping_add(d).wrapping_add(e))
}

#[syscall]
pub fn sys_syscall_test_rust_6(a: i32, b: i32, c: i32, d: i32, e: i32, f: i32) -> Status {
    Status::from_raw(
        a.wrapping_add(b).wrapping_add(c).wrapping_add(d).wrapping_add(e).wrapping_add(f),
    )
}

#[syscall]
pub fn sys_syscall_test_rust_7(a: i32, b: i32, c: i32, d: i32, e: i32, f: i32, g: i32) -> Status {
    Status::from_raw(
        a.wrapping_add(b)
            .wrapping_add(c)
            .wrapping_add(d)
            .wrapping_add(e)
            .wrapping_add(f)
            .wrapping_add(g),
    )
}

#[syscall]
pub fn sys_syscall_test_rust_8(
    a: i32,
    b: i32,
    c: i32,
    d: i32,
    e: i32,
    f: i32,
    g: i32,
    h: i32,
) -> Status {
    Status::from_raw(
        a.wrapping_add(b)
            .wrapping_add(c)
            .wrapping_add(d)
            .wrapping_add(e)
            .wrapping_add(f)
            .wrapping_add(g)
            .wrapping_add(h),
    )
}

#[syscall]
pub fn sys_syscall_test_rust_wrapper(a: i32, b: i32, c: i32) -> Status {
    if a < 0 || b < 0 || c < 0 {
        return Status::INVALID_ARGS;
    }
    let ret = a.wrapping_add(b).wrapping_add(c);
    if ret > 50 { Status::OUT_OF_RANGE } else { Status::from_raw(ret) }
}

#[syscall]
pub fn sys_syscall_test_rust_inptr(ptr: UserInPtr<i32>, value: UserOutPtr<i32>) -> Status {
    if ptr.is_null() || value.is_null() {
        return Status::INVALID_ARGS;
    }
    match ptr.read() {
        Ok(val) => match value.write(val) {
            Ok(()) => Status::OK,
            Err(err) => err,
        },
        Err(err) => err,
    }
}

#[syscall]
pub fn sys_syscall_test_rust_outptr(value: i32, ptr: UserOutPtr<i32>) -> Status {
    if ptr.is_null() {
        return Status::INVALID_ARGS;
    }
    match ptr.write(value) {
        Ok(()) => Status::OK,
        Err(err) => err,
    }
}

#[syscall]
pub fn sys_syscall_test_rust_inoutptr(ptr: UserInOutPtr<i32>) -> Status {
    if ptr.is_null() {
        return Status::INVALID_ARGS;
    }
    match ptr.read() {
        Ok(val) => match ptr.write(val.wrapping_add(val)) {
            Ok(()) => Status::OK,
            Err(err) => err,
        },
        Err(err) => err,
    }
}
