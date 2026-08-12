// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::thread::restricted_enter;
use crate::object::{Dispatcher, HandleValue, ThreadDispatcher};
use crate::user_copy::UserOutPtr;
use debug::ltracef;
use syscalls_macro::syscall;
use zx_status::{ErrorStatus, Status};
use zx_types::{ZX_RIGHT_MANAGE_THREAD, zx_exception_report_t, zx_status_t};

// Disable local tracing by default for this file.
const LOCAL_TRACE: u32 = 0;

unsafe extern "C" {
    fn cpp_restricted_bind_state(
        out_exception_ptr: *mut zx_exception_report_t,
        out_handle: *mut HandleValue,
    ) -> zx_status_t;
    fn cpp_restricted_unbind_state() -> zx_status_t;
}

/// Enters restricted mode using the given vector table pointer and context.
///
/// # Errors
/// Returns `Status::INVALID_ARGS` if options is non-zero.
#[syscall]
pub fn sys_restricted_enter(
    options: u32,
    vector_table_ptr: usize,
    context: usize,
) -> Result<(), ErrorStatus> {
    ltracef!("options {:#x} vector {:#x} context {:#x}\n", options, vector_table_ptr, context);

    // Reject invalid option bits.
    if options != 0 {
        return Err(Status::INVALID_ARGS.into());
    }
    restricted_enter(vector_table_ptr, context)?;
    Ok(())
}

/// Binds restricted state to the current thread and returns a handle to the state VMO.
///
/// # Errors
/// Returns `Status::INVALID_ARGS` if options is non-zero.
#[syscall]
pub fn sys_restricted_bind_state(
    options: u32,
    out: &mut HandleValue,
    out_exception: UserOutPtr<zx_exception_report_t>,
) -> Result<(), ErrorStatus> {
    ltracef!("options {:#x}\n", options);

    // No options allowed.
    if options != 0 {
        return Err(Status::INVALID_ARGS.into());
    }

    // Check if user provided an exception report pointer
    let exception_ptr =
        if !out_exception.is_null() { out_exception.as_ptr() } else { core::ptr::null_mut() };

    // SAFETY: cpp_restricted_bind_state handles creating RestrictedState and the VMO dispatcher,
    // and registers it with the process's handle table and the current thread.
    let status = unsafe { cpp_restricted_bind_state(exception_ptr, out) };
    Status::ok(status)?;
    Ok(())
}

/// Unbinds the restricted state from the current thread.
///
/// # Errors
/// Returns `Status::INVALID_ARGS` if options is non-zero.
#[syscall]
pub fn sys_restricted_unbind_state(options: u32) -> Result<(), ErrorStatus> {
    ltracef!("options {:#x}\n", options);

    // No options allowed.
    if options != 0 {
        return Err(Status::INVALID_ARGS.into());
    }
    // SAFETY: cpp_restricted_unbind_state clears the restricted state from the current thread.
    let status = unsafe { cpp_restricted_unbind_state() };
    Status::ok(status)?;
    Ok(())
}

/// Kicks a thread currently running in restricted mode.
///
/// # Errors
/// Returns `Status::INVALID_ARGS` if options is non-zero.
#[syscall]
pub fn sys_restricted_kick(handle: HandleValue, options: u32) -> Result<(), ErrorStatus> {
    ltracef!("options {:#x}\n", options);

    // No options allowed.
    if options != 0 {
        return Err(Status::INVALID_ARGS.into());
    }

    let thread = Dispatcher::get_with_rights::<ThreadDispatcher>(handle, ZX_RIGHT_MANAGE_THREAD)?;
    thread.restricted_kick()?;
    Ok(())
}
