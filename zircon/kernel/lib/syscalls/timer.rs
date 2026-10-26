// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::object::{HandleValue, ProcessDispatcher, TimerDispatcher};
use debug::ltracef;
use syscalls_macro::syscall;
use zx_status::Status;
use zx_types::{
    ZX_CLOCK_BOOT, ZX_CLOCK_MONOTONIC, ZX_POL_NEW_TIMER, ZX_RIGHT_WRITE, zx_clock_t, zx_duration_t,
    zx_time_t,
};

const LOCAL_TRACE: u32 = 0;

#[syscall]
pub fn sys_timer_create(
    options: u32,
    clock_id: zx_clock_t,
    out: &mut HandleValue,
) -> Result<(), Status> {
    ltracef!("options {:#x} clock_id {}\n", options, clock_id);

    if clock_id != ZX_CLOCK_MONOTONIC && clock_id != ZX_CLOCK_BOOT {
        return Err(Status::INVALID_ARGS);
    }

    ProcessDispatcher::with_current(|up| up.enforce_basic_policy(ZX_POL_NEW_TIMER))?;

    let (kernel_handle, rights) = TimerDispatcher::create(options, clock_id)?;
    let user_handle = kernel_handle.make_and_add_handle(rights)?;
    *out = user_handle;
    Ok(())
}

#[syscall]
pub fn sys_timer_set(
    handle: HandleValue,
    deadline: zx_time_t,
    slack: zx_duration_t,
) -> Result<(), Status> {
    ltracef!("handle {:?} deadline {} slack {}\n", handle, deadline, slack);

    if slack < 0 {
        return Err(Status::OUT_OF_RANGE);
    }

    let (timer, policy_slack) = ProcessDispatcher::with_current(|up| -> Result<_, Status> {
        let timer = up.handle_table().get_dispatcher_with_rights::<TimerDispatcher>(
            up,
            handle,
            ZX_RIGHT_WRITE,
        )?;
        Ok((timer, up.get_timer_slack_policy().amount().0))
    })?;
    let effective_slack = core::cmp::max(slack, policy_slack);

    timer.set(deadline, effective_slack)?;
    Ok(())
}

#[syscall]
pub fn sys_timer_cancel(handle: HandleValue) -> Result<(), Status> {
    ltracef!("handle {:?}\n", handle);

    let timer = ProcessDispatcher::with_current(|up| {
        up.handle_table().get_dispatcher_with_rights::<TimerDispatcher>(up, handle, ZX_RIGHT_WRITE)
    })?;
    timer.cancel()?;
    Ok(())
}
