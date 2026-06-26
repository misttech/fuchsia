// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]

use zx_status::Status;

#[unsafe(no_mangle)]
pub extern "C" fn sys_syscall_test_rust_0() -> i32 {
    Status::OK.into_raw()
}

#[unsafe(no_mangle)]
pub extern "C" fn sys_syscall_test_rust_1(a: i32) -> i32 {
    a
}

#[unsafe(no_mangle)]
pub extern "C" fn sys_syscall_test_rust_2(a: i32, b: i32) -> i32 {
    a.wrapping_add(b)
}

#[unsafe(no_mangle)]
pub extern "C" fn sys_syscall_test_rust_3(a: i32, b: i32, c: i32) -> i32 {
    a.wrapping_add(b).wrapping_add(c)
}

#[unsafe(no_mangle)]
pub extern "C" fn sys_syscall_test_rust_4(a: i32, b: i32, c: i32, d: i32) -> i32 {
    a.wrapping_add(b).wrapping_add(c).wrapping_add(d)
}

#[unsafe(no_mangle)]
pub extern "C" fn sys_syscall_test_rust_5(a: i32, b: i32, c: i32, d: i32, e: i32) -> i32 {
    a.wrapping_add(b).wrapping_add(c).wrapping_add(d).wrapping_add(e)
}

#[unsafe(no_mangle)]
pub extern "C" fn sys_syscall_test_rust_6(a: i32, b: i32, c: i32, d: i32, e: i32, f: i32) -> i32 {
    a.wrapping_add(b).wrapping_add(c).wrapping_add(d).wrapping_add(e).wrapping_add(f)
}

#[unsafe(no_mangle)]
pub extern "C" fn sys_syscall_test_rust_7(
    a: i32,
    b: i32,
    c: i32,
    d: i32,
    e: i32,
    f: i32,
    g: i32,
) -> i32 {
    a.wrapping_add(b)
        .wrapping_add(c)
        .wrapping_add(d)
        .wrapping_add(e)
        .wrapping_add(f)
        .wrapping_add(g)
}

#[unsafe(no_mangle)]
pub extern "C" fn sys_syscall_test_rust_8(
    a: i32,
    b: i32,
    c: i32,
    d: i32,
    e: i32,
    f: i32,
    g: i32,
    h: i32,
) -> i32 {
    a.wrapping_add(b)
        .wrapping_add(c)
        .wrapping_add(d)
        .wrapping_add(e)
        .wrapping_add(f)
        .wrapping_add(g)
        .wrapping_add(h)
}

#[unsafe(no_mangle)]
pub extern "C" fn sys_syscall_test_rust_wrapper(a: i32, b: i32, c: i32) -> i32 {
    if a < 0 || b < 0 || c < 0 {
        return Status::INVALID_ARGS.into_raw();
    }
    let ret = a.wrapping_add(b).wrapping_add(c);
    if ret > 50 { Status::OUT_OF_RANGE.into_raw() } else { ret }
}
