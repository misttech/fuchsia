// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use object::{CounterDispatcher, Dispatcher, HandleValue};
use syscalls_macro::syscall;
use user_copy::UserOutPtr;
use zx_status::{ErrorStatus, Status};
use zx_types::{ZX_RIGHT_READ, ZX_RIGHT_WRITE};

#[syscall]
pub fn sys_counter_create(options: u32, out: &mut HandleValue) -> Result<(), ErrorStatus> {
    if options != 0 {
        return Err(Status::INVALID_ARGS.into());
    }

    // TODO(https://fxbug.dev/387324141): Add/enforce ZX_POL_NEW_COUNTER policy.
    let (kernel_handle, rights) = CounterDispatcher::create()?;
    let user_handle = kernel_handle.make_and_add_handle(rights)?;
    *out = user_handle;
    Ok(())
}

#[syscall]
pub fn sys_counter_add(handle: HandleValue, value: i64) -> Result<(), ErrorStatus> {
    // Both read and write rights are required for add because the resulting signal state and error
    // code can be used to determine the counter's value.
    let counter =
        Dispatcher::get_with_rights::<CounterDispatcher>(handle, ZX_RIGHT_READ | ZX_RIGHT_WRITE)?;

    counter.add(value)?;
    Ok(())
}

#[syscall]
pub fn sys_counter_read(
    handle: HandleValue,
    value_out: UserOutPtr<i64>,
) -> Result<(), ErrorStatus> {
    if value_out.is_null() {
        return Err(Status::INVALID_ARGS.into());
    }

    let counter = Dispatcher::get_with_rights::<CounterDispatcher>(handle, ZX_RIGHT_READ)?;
    let value = counter.value();
    value_out.write(value)?;
    Ok(())
}

#[syscall]
pub fn sys_counter_write(handle: HandleValue, value: i64) -> Result<(), ErrorStatus> {
    let counter = Dispatcher::get_with_rights::<CounterDispatcher>(handle, ZX_RIGHT_WRITE)?;
    counter.set_value(value);
    Ok(())
}
