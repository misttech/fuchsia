// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Implementation of the `zx_object_get_property` and `zx_object_set_property` syscalls in Rust.

use crate::object::{
    Dispatcher, ExceptionDispatcher, HandleValue, JobDispatcher, ProcessDispatcher,
    SocketDispatcher, StreamDispatcher, VmObjectDispatcher,
};
use crate::user_copy::{UserInPtr, UserOutPtr};
use boot_options::BootOptions;
use core::mem::MaybeUninit;
use syscalls_macro::syscall;
use zerocopy::{FromBytes, Immutable, IntoBytes};
use zx_status::Status;
use zx_types::{
    ZX_EXCEPTION_CHANNEL_TYPE_DEBUGGER, ZX_EXCEPTION_STATE_HANDLED, ZX_EXCEPTION_STATE_THREAD_EXIT,
    ZX_EXCEPTION_STATE_TRY_NEXT, ZX_EXCEPTION_STRATEGY_FIRST_CHANCE,
    ZX_EXCEPTION_STRATEGY_SECOND_CHANCE, ZX_MAX_NAME_LEN, ZX_PROP_EXCEPTION_STATE,
    ZX_PROP_EXCEPTION_STRATEGY, ZX_PROP_JOB_KILL_ON_OOM, ZX_PROP_NAME,
    ZX_PROP_PROCESS_BREAK_ON_LOAD, ZX_PROP_PROCESS_DEBUG_ADDR, ZX_PROP_PROCESS_HW_TRACE_CONTEXT_ID,
    ZX_PROP_PROCESS_VDSO_BASE_ADDRESS, ZX_PROP_SOCKET_RX_THRESHOLD, ZX_PROP_SOCKET_TX_THRESHOLD,
    ZX_PROP_STREAM_MODE_APPEND, ZX_PROP_VMO_CONTENT_SIZE, ZX_RIGHT_GET_PROPERTY,
    ZX_RIGHT_SET_PROPERTY, ZX_RIGHT_WRITE,
};

#[cfg(target_arch = "x86_64")]
use crate::arch_rs::x86::registers::{X86_MSR_IA32_FS_BASE, X86_MSR_IA32_KERNEL_GS_BASE};
#[cfg(target_arch = "x86_64")]
use crate::arch_rs::x86::vm::is_vaddr_canonical;

#[cfg(target_arch = "x86_64")]
use zx_types::{ZX_PROP_REGISTER_FS, ZX_PROP_REGISTER_GS};

/// Copies a scalar value `val` of type `T` into the user buffer `value`.
///
/// # Errors
///
/// * [`Status::BUFFER_TOO_SMALL`]: If `size < core::mem::size_of::<T>()`.
fn copy_scalar_to_user<T: IntoBytes + Immutable>(
    value: UserOutPtr<u8>,
    size: usize,
    val: T,
) -> Result<(), Status> {
    if size < core::mem::size_of::<T>() {
        return Err(Status::BUFFER_TOO_SMALL);
    }
    value.reinterpret::<T>().write(val)
}

/// Copies a scalar value of type `T` from the user buffer `value`.
///
/// # Errors
///
/// * [`Status::BUFFER_TOO_SMALL`]: If `size < core::mem::size_of::<T>()`.
fn copy_scalar_from_user<T: FromBytes>(value: UserInPtr<u8>, size: usize) -> Result<T, Status> {
    if size < core::mem::size_of::<T>() {
        return Err(Status::BUFFER_TOO_SMALL);
    }
    value.reinterpret::<T>().read()
}

/// Validates that the given dispatcher refers to the currently executing thread.
///
/// On x86_64, reading or writing FS/GS base registers via `ZX_PROP_REGISTER_FS` and
/// `ZX_PROP_REGISTER_GS` is restricted to the caller's own current thread.
#[cfg(target_arch = "x86_64")]
fn require_current_thread(dispatcher: &Dispatcher) -> Result<(), Status> {
    let thread =
        dispatcher.downcast::<crate::object::ThreadDispatcher>().ok_or(Status::WRONG_TYPE)?;
    if !thread.is_current() {
        return Err(Status::ACCESS_DENIED);
    }
    Ok(())
}

#[syscall]
pub fn sys_object_get_property(
    handle_value: HandleValue,
    property: u32,
    value: UserOutPtr<u8>,
    size: usize,
) -> Result<(), Status> {
    if value.is_null() {
        return Err(Status::INVALID_ARGS);
    }

    let dispatcher = ProcessDispatcher::with_current(|up| {
        up.handle_table().get_dispatcher_with_rights::<Dispatcher>(
            up,
            handle_value,
            ZX_RIGHT_GET_PROPERTY,
        )
    })?;

    macro_rules! get_scalar_property {
        ($type:ty, $getter:ident) => {{
            let obj = dispatcher.downcast::<$type>().ok_or(Status::WRONG_TYPE)?;
            copy_scalar_to_user(value, size, obj.$getter())
        }};
    }

    match property {
        ZX_PROP_NAME => {
            if size < ZX_MAX_NAME_LEN {
                return Err(Status::BUFFER_TOO_SMALL);
            }
            let mut name = [0u8; ZX_MAX_NAME_LEN];
            dispatcher.get_name(&mut name)?;
            value.copy_slice_to_user(&name)
        }
        ZX_PROP_PROCESS_DEBUG_ADDR => get_scalar_property!(ProcessDispatcher, get_debug_addr),
        ZX_PROP_PROCESS_BREAK_ON_LOAD => {
            get_scalar_property!(ProcessDispatcher, get_dyn_break_on_load)
        }
        ZX_PROP_PROCESS_VDSO_BASE_ADDRESS => {
            get_scalar_property!(ProcessDispatcher, vdso_base_address)
        }
        ZX_PROP_PROCESS_HW_TRACE_CONTEXT_ID => {
            if !BootOptions::get().enable_debugging_syscalls {
                return Err(Status::NOT_SUPPORTED);
            }
            #[cfg(target_arch = "x86_64")]
            {
                get_scalar_property!(ProcessDispatcher, hw_trace_context_id)
            }
            #[cfg(not(target_arch = "x86_64"))]
            {
                Err(Status::NOT_SUPPORTED)
            }
        }
        ZX_PROP_SOCKET_RX_THRESHOLD => get_scalar_property!(SocketDispatcher, get_read_threshold),
        ZX_PROP_SOCKET_TX_THRESHOLD => get_scalar_property!(SocketDispatcher, get_write_threshold),
        ZX_PROP_VMO_CONTENT_SIZE => get_scalar_property!(VmObjectDispatcher, get_stream_size),
        ZX_PROP_EXCEPTION_STATE => get_scalar_property!(ExceptionDispatcher, get_disposition),
        ZX_PROP_EXCEPTION_STRATEGY => {
            let exception =
                dispatcher.downcast::<ExceptionDispatcher>().ok_or(Status::WRONG_TYPE)?;
            let strategy = if exception.is_second_chance() {
                ZX_EXCEPTION_STRATEGY_SECOND_CHANCE
            } else {
                ZX_EXCEPTION_STRATEGY_FIRST_CHANCE
            };
            copy_scalar_to_user(value, size, strategy)
        }
        #[cfg(target_arch = "x86_64")]
        ZX_PROP_REGISTER_FS | ZX_PROP_REGISTER_GS => {
            require_current_thread(&dispatcher)?;
            // SAFETY: Reading valid Model Specific Registers on x86_64 hardware.
            let val = if property == ZX_PROP_REGISTER_FS {
                unsafe { crate::arch_rs::x86::x86::read_msr(X86_MSR_IA32_FS_BASE) }
            } else {
                unsafe { crate::arch_rs::x86::x86::read_msr(X86_MSR_IA32_KERNEL_GS_BASE) }
            };
            copy_scalar_to_user(value, size, val as usize)
        }
        ZX_PROP_STREAM_MODE_APPEND => {
            if size < core::mem::size_of::<u8>() {
                return Err(Status::BUFFER_TOO_SMALL);
            }
            let stream = dispatcher.downcast::<StreamDispatcher>().ok_or(Status::WRONG_TYPE)?;
            copy_scalar_to_user(value, size, u8::from(stream.is_in_append_mode()))
        }
        _ => Err(Status::NOT_SUPPORTED),
    }
}

#[syscall]
pub fn sys_object_set_property(
    handle_value: HandleValue,
    property: u32,
    value: UserInPtr<u8>,
    size: usize,
) -> Result<(), Status> {
    if value.is_null() {
        return Err(Status::INVALID_ARGS);
    }

    let (dispatcher, rights) = ProcessDispatcher::with_current(|up| {
        up.handle_table().get_dispatcher_with_rights_and_actual::<Dispatcher>(
            up,
            handle_value,
            ZX_RIGHT_SET_PROPERTY,
        )
    })?;

    macro_rules! set_scalar_property {
        ($type:ty, $setter:ident) => {{
            let obj = dispatcher.downcast::<$type>().ok_or(Status::WRONG_TYPE)?;
            let val = copy_scalar_from_user(value, size)?;
            obj.$setter(val)
        }};
    }

    match property {
        ZX_PROP_NAME => {
            let mut name_buf = [MaybeUninit::uninit(); ZX_MAX_NAME_LEN];
            let name_slice = value.copy_user_string(size, &mut name_buf)?;
            dispatcher.set_name(name_slice)
        }
        #[cfg(target_arch = "x86_64")]
        ZX_PROP_REGISTER_FS | ZX_PROP_REGISTER_GS => {
            require_current_thread(&dispatcher)?;
            let addr = copy_scalar_from_user::<usize>(value, size)?;
            if !is_vaddr_canonical(addr) {
                return Err(Status::INVALID_ARGS);
            }
            // SAFETY: Writing canonical virtual address to valid MSR on current thread.
            if property == ZX_PROP_REGISTER_FS {
                unsafe { crate::arch_rs::x86::x86::write_msr(X86_MSR_IA32_FS_BASE, addr as u64) };
            } else {
                unsafe {
                    crate::arch_rs::x86::x86::write_msr(X86_MSR_IA32_KERNEL_GS_BASE, addr as u64)
                };
            }
            Ok(())
        }
        ZX_PROP_PROCESS_DEBUG_ADDR => {
            set_scalar_property!(ProcessDispatcher, set_debug_addr)
        }
        ZX_PROP_PROCESS_BREAK_ON_LOAD => {
            set_scalar_property!(ProcessDispatcher, set_dyn_break_on_load)
        }
        ZX_PROP_SOCKET_RX_THRESHOLD => {
            set_scalar_property!(SocketDispatcher, set_read_threshold)
        }
        ZX_PROP_SOCKET_TX_THRESHOLD => {
            set_scalar_property!(SocketDispatcher, set_write_threshold)
        }
        ZX_PROP_VMO_CONTENT_SIZE => {
            if (rights & ZX_RIGHT_WRITE) == 0 {
                return Err(Status::ACCESS_DENIED);
            }
            set_scalar_property!(VmObjectDispatcher, set_stream_size)
        }
        ZX_PROP_JOB_KILL_ON_OOM => {
            let job = dispatcher.downcast::<JobDispatcher>().ok_or(Status::WRONG_TYPE)?;
            let val = copy_scalar_from_user::<usize>(value, size)?;
            if val == 0 {
                job.set_kill_on_oom(false);
            } else if val == 1 {
                job.set_kill_on_oom(true);
            } else {
                return Err(Status::INVALID_ARGS);
            }
            Ok(())
        }
        ZX_PROP_EXCEPTION_STATE => {
            if size < core::mem::size_of::<u32>() {
                return Err(Status::BUFFER_TOO_SMALL);
            }
            let exception =
                dispatcher.downcast::<ExceptionDispatcher>().ok_or(Status::WRONG_TYPE)?;
            let val = copy_scalar_from_user::<u32>(value, size)?;
            match val {
                ZX_EXCEPTION_STATE_HANDLED
                | ZX_EXCEPTION_STATE_TRY_NEXT
                | ZX_EXCEPTION_STATE_THREAD_EXIT => {
                    exception.set_disposition(val);
                    Ok(())
                }
                _ => Err(Status::INVALID_ARGS),
            }
        }
        ZX_PROP_EXCEPTION_STRATEGY => {
            if size < core::mem::size_of::<u32>() {
                return Err(Status::BUFFER_TOO_SMALL);
            }
            let exception =
                dispatcher.downcast::<ExceptionDispatcher>().ok_or(Status::WRONG_TYPE)?;
            let info = exception.thread().get_info_for_userspace();
            // Invalid if the exception handle is not held by a debugger.
            if info.wait_exception_channel_type != ZX_EXCEPTION_CHANNEL_TYPE_DEBUGGER {
                return Err(Status::BAD_STATE);
            }
            let val = copy_scalar_from_user::<u32>(value, size)?;
            match val {
                ZX_EXCEPTION_STRATEGY_FIRST_CHANCE => {
                    exception.set_whether_second_chance(false);
                    Ok(())
                }
                ZX_EXCEPTION_STRATEGY_SECOND_CHANCE => {
                    exception.set_whether_second_chance(true);
                    Ok(())
                }
                _ => Err(Status::INVALID_ARGS),
            }
        }
        ZX_PROP_STREAM_MODE_APPEND => {
            if size < core::mem::size_of::<u8>() {
                return Err(Status::BUFFER_TOO_SMALL);
            }
            let stream = dispatcher.downcast::<StreamDispatcher>().ok_or(Status::WRONG_TYPE)?;
            let val = copy_scalar_from_user::<u8>(value, size)?;
            stream.set_append_mode(val != 0);
            Ok(())
        }
        _ => Err(Status::NOT_SUPPORTED),
    }
}
