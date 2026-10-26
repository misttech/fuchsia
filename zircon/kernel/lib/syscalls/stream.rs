// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::object::{HandleValue, ProcessDispatcher, StreamDispatcher, VmObjectDispatcher};
use crate::user_copy::{UserInPtr, UserOutPtr, make_user_in_iovec, make_user_out_iovec};
use debug::ltracef;
use syscalls_macro::syscall;
use zx_status::Status;
use zx_types::{
    ZX_RIGHT_READ, ZX_RIGHT_WRITE, ZX_STREAM_APPEND, ZX_STREAM_CREATE_MASK, zx_iovec_t, zx_off_t,
    zx_stream_seek_origin_t,
};

const LOCAL_TRACE: u32 = 0;

#[syscall]
pub fn sys_stream_create(
    options: u32,
    vmo_handle: HandleValue,
    seek: zx_off_t,
    out_stream: &mut HandleValue,
) -> Result<(), Status> {
    if (options & !ZX_STREAM_CREATE_MASK) != 0 {
        return Err(Status::INVALID_ARGS);
    }

    let (stream_options, desired_vmo_rights) =
        StreamDispatcher::parse_create_syscall_flags(options)?;

    let vmo_disp = ProcessDispatcher::with_current(|up| {
        up.handle_table().get_dispatcher_with_rights::<VmObjectDispatcher>(
            up,
            vmo_handle,
            desired_vmo_rights,
        )
    })?;

    // Cannot create a stream from a physical or contiguous VMO.
    if !vmo_disp.vmo().is_paged() || vmo_disp.vmo().is_contiguous() {
        return Err(Status::WRONG_TYPE);
    }

    let (kernel_handle, rights) = StreamDispatcher::create(stream_options, &vmo_disp, seek)?;
    let user_handle = kernel_handle.make_and_add_handle(rights)?;
    *out_stream = user_handle;
    Ok(())
}

#[syscall]
pub fn sys_stream_writev(
    handle: HandleValue,
    options: u32,
    vector: UserInPtr<zx_iovec_t>,
    vector_count: usize,
    out_actual: UserOutPtr<usize>,
) -> Result<(), Status> {
    ltracef!("handle {:#x}\n", handle.raw_value());

    if (options & !ZX_STREAM_APPEND) != 0 {
        return Err(Status::INVALID_ARGS);
    }
    if vector.is_null() {
        return Err(Status::INVALID_ARGS);
    }

    let stream = ProcessDispatcher::with_current(|up| {
        up.handle_table().get_dispatcher_with_rights::<StreamDispatcher>(up, handle, ZX_RIGHT_WRITE)
    })?;
    let user_data = make_user_in_iovec(vector, vector_count);
    let (status, actual) = if (options & ZX_STREAM_APPEND) != 0 {
        stream.append_vector(user_data)
    } else {
        stream.write_vector(user_data)
    };
    status?;

    if !out_actual.is_null() {
        out_actual.write(actual)?;
    }
    Ok(())
}

#[syscall]
pub fn sys_stream_writev_at(
    handle: HandleValue,
    options: u32,
    offset: zx_off_t,
    vector: UserInPtr<zx_iovec_t>,
    vector_count: usize,
    out_actual: UserOutPtr<usize>,
) -> Result<(), Status> {
    ltracef!("handle {:#x}\n", handle.raw_value());

    if options != 0 {
        return Err(Status::INVALID_ARGS);
    }
    if vector.is_null() {
        return Err(Status::INVALID_ARGS);
    }

    let stream = ProcessDispatcher::with_current(|up| {
        up.handle_table().get_dispatcher_with_rights::<StreamDispatcher>(up, handle, ZX_RIGHT_WRITE)
    })?;
    let (status, actual) = stream.write_vector_at(make_user_in_iovec(vector, vector_count), offset);
    status?;

    if !out_actual.is_null() {
        out_actual.write(actual)?;
    }
    Ok(())
}

#[syscall]
pub fn sys_stream_readv(
    handle: HandleValue,
    options: u32,
    vector: UserOutPtr<zx_iovec_t>,
    vector_count: usize,
    out_actual: UserOutPtr<usize>,
) -> Result<(), Status> {
    ltracef!("handle {:#x}\n", handle.raw_value());

    if options != 0 {
        return Err(Status::INVALID_ARGS);
    }
    if vector.is_null() {
        return Err(Status::INVALID_ARGS);
    }

    let stream = ProcessDispatcher::with_current(|up| {
        up.handle_table().get_dispatcher_with_rights::<StreamDispatcher>(up, handle, ZX_RIGHT_READ)
    })?;
    let (status, actual) = stream.read_vector(make_user_out_iovec(vector, vector_count));
    status?;

    if !out_actual.is_null() {
        out_actual.write(actual)?;
    }
    Ok(())
}

#[syscall]
pub fn sys_stream_readv_at(
    handle: HandleValue,
    options: u32,
    offset: zx_off_t,
    vector: UserOutPtr<zx_iovec_t>,
    vector_count: usize,
    out_actual: UserOutPtr<usize>,
) -> Result<(), Status> {
    ltracef!("handle {:#x}\n", handle.raw_value());

    if options != 0 {
        return Err(Status::INVALID_ARGS);
    }
    if vector.is_null() {
        return Err(Status::INVALID_ARGS);
    }

    let stream = ProcessDispatcher::with_current(|up| {
        up.handle_table().get_dispatcher_with_rights::<StreamDispatcher>(up, handle, ZX_RIGHT_READ)
    })?;
    let (status, actual) = stream.read_vector_at(make_user_out_iovec(vector, vector_count), offset);
    status?;

    if !out_actual.is_null() {
        out_actual.write(actual)?;
    }
    Ok(())
}

#[syscall]
pub fn sys_stream_seek(
    handle: HandleValue,
    whence: zx_stream_seek_origin_t,
    offset: i64,
    out_seek: UserOutPtr<zx_off_t>,
) -> Result<(), Status> {
    ltracef!("handle {:#x}\n", handle.raw_value());

    let (stream, rights) = ProcessDispatcher::with_current(|up| {
        up.handle_table().get_dispatcher_and_rights::<StreamDispatcher>(up, handle)
    })?;
    if (rights & (ZX_RIGHT_READ | ZX_RIGHT_WRITE)) == 0 {
        return Err(Status::ACCESS_DENIED);
    }

    let seek = stream.seek(whence, offset)?;
    if !out_seek.is_null() {
        out_seek.write(seek)?;
    }
    Ok(())
}
