// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#![no_std]
#![allow(clippy::missing_safety_doc)]

use user_copy::{UserInIovec, UserInOutPtr, UserInPtr, UserOutPtr, UserStringView};
use zx_status::Status;
use zx_types::{zx_iovec_t, zx_status_t};

fn status_raw<T>(res: Result<T, Status>) -> zx_status_t {
    match res {
        Ok(_) => Status::OK.into_raw(),
        Err(err) => err.into_raw(),
    }
}

/// # Safety
/// Caller must ensure `ptr` points to valid memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_user_copy_user_out_ptr_write(
    ptr: UserOutPtr<u32>,
    val: u32,
) -> zx_status_t {
    status_raw(ptr.write(val))
}

/// # Safety
/// Caller must ensure `ptr` and `out_val` point to valid memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_user_copy_user_in_ptr_read(
    ptr: UserInPtr<u32>,
    out_val: *mut u32,
) -> zx_status_t {
    match ptr.read() {
        // SAFETY: Caller guarantees `out_val` points to valid memory.
        Ok(val) => unsafe {
            *out_val = val;
            Status::OK.into_raw()
        },
        Err(err) => err.into_raw(),
    }
}

/// # Safety
/// Caller must ensure `ptr` and `out_val` point to valid memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_user_copy_user_in_ptr_copy_from_user(
    ptr: UserInPtr<u32>,
    out_val: *mut u32,
) -> zx_status_t {
    let mut uninit = core::mem::MaybeUninit::uninit();
    match ptr.copy_from_user(&mut uninit) {
        // SAFETY: Caller guarantees `out_val` points to valid memory.
        Ok(val_ref) => unsafe {
            *out_val = *val_ref;
            Status::OK.into_raw()
        },
        Err(err) => err.into_raw(),
    }
}

/// # Safety
/// Caller must ensure `ptr` and `dst` point to valid memory buffer of size `dst_len`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_user_copy_user_in_ptr_copy_slice_from_user(
    ptr: UserInPtr<u32>,
    dst: *mut u32,
    dst_len: usize,
) -> zx_status_t {
    // SAFETY: Caller guarantees `dst` points to a buffer of at least `dst_len` elements.
    let slice = unsafe {
        core::slice::from_raw_parts_mut(dst.cast::<core::mem::MaybeUninit<u32>>(), dst_len)
    };
    status_raw(ptr.copy_slice_from_user(slice))
}

/// # Safety
/// Caller must ensure `ptr` and `out_capacity` point to valid memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_user_copy_user_in_iovec_get_total_capacity(
    ptr: UserInPtr<zx_iovec_t>,
    count: usize,
    out_capacity: *mut usize,
) -> zx_status_t {
    let iovec = UserInIovec::new(ptr, count);
    match iovec.get_total_capacity() {
        // SAFETY: Caller guarantees `out_capacity` points to valid memory.
        Ok(cap) => unsafe {
            *out_capacity = cap;
            Status::OK.into_raw()
        },
        Err(err) => err.into_raw(),
    }
}

/// # Safety
/// Caller must ensure `ptr` and `out_product` point to valid memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_user_copy_user_in_iovec_for_each(
    ptr: UserInPtr<zx_iovec_t>,
    count: usize,
    out_product: *mut usize,
) -> zx_status_t {
    let iovec = UserInIovec::new(ptr, count);
    let mut product = 2usize;
    let status = iovec.for_each(|_buf, cap| {
        product = product.wrapping_mul(cap);
        Ok(())
    });
    match status {
        // SAFETY: Caller guarantees `out_product` points to valid memory.
        Ok(()) => unsafe {
            *out_product = product;
            Status::OK.into_raw()
        },
        Err(err) => err.into_raw(),
    }
}

/// # Safety
/// Caller must ensure `ptr` and `dst` point to valid memory buffer of size `dst_len`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_user_copy_user_string_view_copy_slice_from_user(
    ptr: UserInPtr<u8>,
    length: usize,
    dst: *mut u8,
    dst_len: usize,
) -> zx_status_t {
    let sv = UserStringView { data: ptr, length };
    // SAFETY: Caller guarantees `dst` points to a buffer of at least `dst_len` bytes.
    let slice = unsafe {
        core::slice::from_raw_parts_mut(dst.cast::<core::mem::MaybeUninit<u8>>(), dst_len)
    };
    status_raw(sv.copy_slice_from_user(slice))
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_user_copy_test_offsets() -> zx_status_t {
    let base = 0x1000 as *mut u32;

    // UserInPtr offset tests
    let in_ptr = UserInPtr::new(base as *const u32);
    if in_ptr.byte_offset(8).as_ptr() != (0x1008 as *const u32) {
        return Status::INTERNAL.into_raw();
    }
    if in_ptr.byte_offset(-4).as_ptr() != (0x0ffc as *const u32) {
        return Status::INTERNAL.into_raw();
    }
    if in_ptr.element_offset(3).as_ptr() != (0x100c as *const u32) {
        return Status::INTERNAL.into_raw();
    }
    let null_in = UserInPtr::<u32>::new(core::ptr::null());
    if !null_in.byte_offset(8).is_null() || !null_in.element_offset(3).is_null() {
        return Status::INTERNAL.into_raw();
    }
    let def_in = UserInPtr::<u32>::default();
    if !def_in.is_null() {
        return Status::INTERNAL.into_raw();
    }

    // UserOutPtr offset tests
    let out_ptr = UserOutPtr::new(base);
    if out_ptr.byte_offset(8).as_ptr() != (0x1008 as *mut u32) {
        return Status::INTERNAL.into_raw();
    }
    if out_ptr.byte_offset(-4).as_ptr() != (0x0ffc as *mut u32) {
        return Status::INTERNAL.into_raw();
    }
    if out_ptr.element_offset(3).as_ptr() != (0x100c as *mut u32) {
        return Status::INTERNAL.into_raw();
    }
    let null_out = UserOutPtr::<u32>::new(core::ptr::null_mut());
    if !null_out.byte_offset(8).is_null() || !null_out.element_offset(3).is_null() {
        return Status::INTERNAL.into_raw();
    }
    let def_out = UserOutPtr::<u32>::default();
    if !def_out.is_null() {
        return Status::INTERNAL.into_raw();
    }

    // UserInOutPtr offset tests
    let inout_ptr = UserInOutPtr::new(base);
    if inout_ptr.byte_offset(8).as_ptr() != (0x1008 as *mut u32) {
        return Status::INTERNAL.into_raw();
    }
    if inout_ptr.byte_offset(-4).as_ptr() != (0x0ffc as *mut u32) {
        return Status::INTERNAL.into_raw();
    }
    if inout_ptr.element_offset(3).as_ptr() != (0x100c as *mut u32) {
        return Status::INTERNAL.into_raw();
    }
    let null_inout = UserInOutPtr::<u32>::new(core::ptr::null_mut());
    if !null_inout.byte_offset(8).is_null() || !null_inout.element_offset(3).is_null() {
        return Status::INTERNAL.into_raw();
    }
    let def_inout = UserInOutPtr::<u32>::default();
    if !def_inout.is_null() {
        return Status::INTERNAL.into_raw();
    }

    // UserStringView Default test
    let def_sv = UserStringView::default();
    if !def_sv.data.is_null() || !def_sv.is_empty() {
        return Status::INTERNAL.into_raw();
    }

    Status::OK.into_raw()
}
