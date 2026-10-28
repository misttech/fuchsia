// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::object::HandleValue;
use crate::user_copy::{UserInOutPtr, UserInPtr, UserOutPtr};
use syscalls_macro::syscall;
use zx_status::Status;
use zx_types::zx_status_t;

// zx_status_t zx_syscall_test_0
#[syscall]
pub fn sys_syscall_test_0() -> Result<(), Status> {
    Ok(())
}

// zx_status_t zx_syscall_test_1
#[syscall]
pub fn sys_syscall_test_1(a: i32) -> Result<(), Status> {
    Status::ok(a)
}

// zx_status_t zx_syscall_test_2
#[syscall]
pub fn sys_syscall_test_2(a: i32, b: i32) -> Result<(), Status> {
    Status::ok(a.wrapping_add(b))
}

// zx_status_t zx_syscall_test_3
#[syscall]
pub fn sys_syscall_test_3(a: i32, b: i32, c: i32) -> Result<(), Status> {
    Status::ok(a.wrapping_add(b).wrapping_add(c))
}

// zx_status_t zx_syscall_test_4
#[syscall]
pub fn sys_syscall_test_4(a: i32, b: i32, c: i32, d: i32) -> Result<(), Status> {
    Status::ok(a.wrapping_add(b).wrapping_add(c).wrapping_add(d))
}

// zx_status_t zx_syscall_test_5
#[syscall]
pub fn sys_syscall_test_5(a: i32, b: i32, c: i32, d: i32, e: i32) -> Result<(), Status> {
    Status::ok(a.wrapping_add(b).wrapping_add(c).wrapping_add(d).wrapping_add(e))
}

// zx_status_t zx_syscall_test_6
#[syscall]
pub fn sys_syscall_test_6(a: i32, b: i32, c: i32, d: i32, e: i32, f: i32) -> Result<(), Status> {
    Status::ok(a.wrapping_add(b).wrapping_add(c).wrapping_add(d).wrapping_add(e).wrapping_add(f))
}

// zx_status_t zx_syscall_test_7
#[syscall]
pub fn sys_syscall_test_7(
    a: i32,
    b: i32,
    c: i32,
    d: i32,
    e: i32,
    f: i32,
    g: i32,
) -> Result<(), Status> {
    Status::ok(
        a.wrapping_add(b)
            .wrapping_add(c)
            .wrapping_add(d)
            .wrapping_add(e)
            .wrapping_add(f)
            .wrapping_add(g),
    )
}

// zx_status_t zx_syscall_test_8
#[syscall]
pub fn sys_syscall_test_8(
    a: i32,
    b: i32,
    c: i32,
    d: i32,
    e: i32,
    f: i32,
    g: i32,
    h: i32,
) -> Result<(), Status> {
    Status::ok(
        a.wrapping_add(b)
            .wrapping_add(c)
            .wrapping_add(d)
            .wrapping_add(e)
            .wrapping_add(f)
            .wrapping_add(g)
            .wrapping_add(h),
    )
}

// zx_syscall_next_1
#[syscall]
pub fn sys_syscall_next_1(arg: i32) -> Result<(), Status> {
    if arg > 50 { Err(Status::INVALID_ARGS) } else { Ok(()) }
}

// zx_status_t zx_syscall_test_wrapper
#[syscall]
pub fn sys_syscall_test_wrapper(a: i32, b: i32, c: i32) -> Result<(), Status> {
    if a < 0 || b < 0 || c < 0 {
        return Err(Status::INVALID_ARGS);
    }
    let ret = a.wrapping_add(b).wrapping_add(c);
    if ret > 50 { Err(Status::OUT_OF_RANGE) } else { Status::ok(ret) }
}

/// `zx_status_t zx_syscall_test_handle_create`
///
/// Unconditionally creates a valid handle. If we return a non-OK status, the
/// syscall wrappers should not copy the handle back to userspace and the
/// syscall should generate a `ZX_EXCP_POLICY_CODE_HANDLE_LEAK` exception.
#[syscall]
pub fn sys_syscall_test_handle_create(
    return_value: zx_status_t,
    handle_out: &mut HandleValue,
) -> Result<(), Status> {
    if super::event::sys_event_create(0, handle_out).is_err() {
        return Err(Status::INTERNAL);
    }
    Status::ok(return_value)
}

/// If the compiler assumes that incoming high bits in argument registers for
/// narrower-typed arguments are zero or sign-extended, then it won't narrow the
/// arguments being passed from the `syscall_test_*` function to the `test_*`
/// function and the high bits will show in the totals.
#[inline(never)]
fn test_narrow(a64: u64, a32: u32, a16: u16, a8: u8) -> u64 {
    a64.wrapping_add(u64::from(a32)).wrapping_add(u64::from(a16)).wrapping_add(u64::from(a8))
}

#[inline(never)]
fn test_signed_narrow(a64: i64, a32: i32, a16: i16, a8: i8) -> i64 {
    a64.wrapping_add(i64::from(a32)).wrapping_add(i64::from(a16)).wrapping_add(i64::from(a8))
}

#[inline(never)]
fn test_wide(a64: u64, a32: u64, a16: u64, a8: u64) -> u64 {
    a64.wrapping_add(a32).wrapping_add(a16).wrapping_add(a8)
}

#[inline(never)]
fn test_signed_wide(a64: i64, a32: i64, a16: i64, a8: i64) -> i64 {
    a64.wrapping_add(a32).wrapping_add(a16).wrapping_add(a8)
}

#[syscall]
pub fn sys_syscall_test_widening_unsigned_narrow(a64: u64, a32: u32, a16: u16, a8: u8) -> u64 {
    test_narrow(a64, a32, a16, a8)
}

#[syscall]
pub fn sys_syscall_test_widening_unsigned_wide(a64: u64, a32: u32, a16: u16, a8: u8) -> u64 {
    test_wide(a64, u64::from(a32), u64::from(a16), u64::from(a8))
}

#[syscall]
pub fn sys_syscall_test_widening_signed_narrow(a64: i64, a32: i32, a16: i16, a8: i8) -> i64 {
    test_signed_narrow(a64, a32, a16, a8)
}

#[syscall]
pub fn sys_syscall_test_widening_signed_wide(a64: i64, a32: i32, a16: i16, a8: i8) -> i64 {
    test_signed_wide(a64, i64::from(a32), i64::from(a16), i64::from(a8))
}

#[syscall]
pub fn sys_syscall_test_rust_0() -> Result<(), Status> {
    Ok(())
}

#[syscall]
pub fn sys_syscall_test_rust_1(a: i32) -> Result<(), Status> {
    Status::ok(a)
}

#[syscall]
pub fn sys_syscall_test_rust_2(a: i32, b: i32) -> Result<(), Status> {
    Status::ok(a.wrapping_add(b))
}

#[syscall]
pub fn sys_syscall_test_rust_3(a: i32, b: i32, c: i32) -> Result<(), Status> {
    Status::ok(a.wrapping_add(b).wrapping_add(c))
}

#[syscall]
pub fn sys_syscall_test_rust_4(a: i32, b: i32, c: i32, d: i32) -> Result<(), Status> {
    Status::ok(a.wrapping_add(b).wrapping_add(c).wrapping_add(d))
}

#[syscall]
pub fn sys_syscall_test_rust_5(a: i32, b: i32, c: i32, d: i32, e: i32) -> Result<(), Status> {
    Status::ok(a.wrapping_add(b).wrapping_add(c).wrapping_add(d).wrapping_add(e))
}

#[syscall]
pub fn sys_syscall_test_rust_6(
    a: i32,
    b: i32,
    c: i32,
    d: i32,
    e: i32,
    f: i32,
) -> Result<(), Status> {
    Status::ok(a.wrapping_add(b).wrapping_add(c).wrapping_add(d).wrapping_add(e).wrapping_add(f))
}

#[syscall]
pub fn sys_syscall_test_rust_7(
    a: i32,
    b: i32,
    c: i32,
    d: i32,
    e: i32,
    f: i32,
    g: i32,
) -> Result<(), Status> {
    Status::ok(
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
) -> Result<(), Status> {
    Status::ok(
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
pub fn sys_syscall_test_rust_wrapper(a: i32, b: i32, c: i32) -> Result<(), Status> {
    if a < 0 || b < 0 || c < 0 {
        return Err(Status::INVALID_ARGS);
    }
    let ret = a.wrapping_add(b).wrapping_add(c);
    if ret > 50 { Err(Status::OUT_OF_RANGE) } else { Status::ok(ret) }
}

#[syscall]
pub fn sys_syscall_test_rust_inptr(
    ptr: UserInPtr<i32>,
    value: UserOutPtr<i32>,
) -> Result<(), Status> {
    if ptr.is_null() || value.is_null() {
        return Err(Status::INVALID_ARGS);
    }
    let val = ptr.read()?;
    value.write(val)?;
    Ok(())
}

#[syscall]
pub fn sys_syscall_test_rust_outptr(value: i32, ptr: UserOutPtr<i32>) -> Result<(), Status> {
    if ptr.is_null() {
        return Err(Status::INVALID_ARGS);
    }
    ptr.write(value)?;
    Ok(())
}

#[syscall]
pub fn sys_syscall_test_rust_inoutptr(ptr: UserInOutPtr<i32>) -> Result<(), Status> {
    if ptr.is_null() {
        return Err(Status::INVALID_ARGS);
    }
    let val = ptr.read()?;
    ptr.write(val.wrapping_add(val))?;
    Ok(())
}

#[syscall]
pub fn sys_syscall_test_rust_handle(
    handle: HandleValue,
    value: UserOutPtr<u32>,
) -> Result<(), Status> {
    if value.is_null() {
        return Err(Status::INVALID_ARGS);
    }
    value.write(handle.raw_value())?;
    Ok(())
}
