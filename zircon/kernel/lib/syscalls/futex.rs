// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::deadline::Deadline;
use crate::object::{HandleValue, OwnerAction, ProcessDispatcher};
use crate::platform_rs::timer::InstantUnknown;
use crate::user_copy::{UserInPtr, UserOutPtr};
use debug::ltracef;
use syscalls_macro::syscall;
use zx_status::Status;
use zx_types::{zx_futex_t, zx_instant_mono_t, zx_koid_t};

const LOCAL_TRACE: u32 = 0;

// zx_status_t zx_futex_wait
#[syscall]
pub fn sys_futex_wait(
    value_ptr: UserInPtr<zx_futex_t>,
    current_value: i32,
    new_futex_owner: HandleValue,
    deadline: zx_instant_mono_t,
) -> Result<(), Status> {
    ltracef!("futex {:p} current {}\n", value_ptr.as_ptr(), current_value);

    ProcessDispatcher::with_current(|dispatcher| {
        let slack = dispatcher.get_timer_slack_policy();
        let slack_deadline = Deadline::new(InstantUnknown(deadline), slack);

        dispatcher.futex_context().futex_wait(
            value_ptr,
            current_value,
            new_futex_owner,
            &slack_deadline,
        )
    })
}

// zx_status_t zx_futex_wake
#[syscall]
pub fn sys_futex_wake(value_ptr: UserInPtr<zx_futex_t>, count: u32) -> Result<(), Status> {
    ltracef!("futex {:p} count {}\n", value_ptr.as_ptr(), count);

    ProcessDispatcher::with_current(|up| {
        up.futex_context().futex_wake(value_ptr, count, OwnerAction::Release)
    })
}

// zx_status_t zx_futex_requeue
#[syscall]
pub fn sys_futex_requeue(
    wake_ptr: UserInPtr<zx_futex_t>,
    wake_count: u32,
    current_value: i32,
    requeue_ptr: UserInPtr<zx_futex_t>,
    requeue_count: u32,
    requeue_owner: HandleValue,
) -> Result<(), Status> {
    ltracef!(
        "futex {:p} wake_count {}current_value {} requeue_futex {:p} requeue_count {}\n",
        wake_ptr.as_ptr(),
        wake_count,
        current_value,
        requeue_ptr.as_ptr(),
        requeue_count
    );

    ProcessDispatcher::with_current(|up| {
        up.futex_context().futex_requeue(
            wake_ptr,
            wake_count,
            current_value,
            OwnerAction::Release,
            requeue_ptr,
            requeue_count,
            requeue_owner,
        )
    })
}

// zx_status_t zx_futex_wake_single_owner
#[syscall]
pub fn sys_futex_wake_single_owner(value_ptr: UserInPtr<zx_futex_t>) -> Result<(), Status> {
    ltracef!("futex {:p}\n", value_ptr.as_ptr());

    ProcessDispatcher::with_current(|up| {
        up.futex_context().futex_wake(value_ptr, 1, OwnerAction::AssignWoken)
    })
}

// zx_status_t zx_futex_requeue_single_owner
#[syscall]
pub fn sys_futex_requeue_single_owner(
    wake_ptr: UserInPtr<zx_futex_t>,
    current_value: i32,
    requeue_ptr: UserInPtr<zx_futex_t>,
    requeue_count: u32,
    requeue_owner: HandleValue,
) -> Result<(), Status> {
    ltracef!(
        "futex {:p} current_value {} requeue_futex {:p} requeue_count {}\n",
        wake_ptr.as_ptr(),
        current_value,
        requeue_ptr.as_ptr(),
        requeue_count
    );

    ProcessDispatcher::with_current(|up| {
        up.futex_context().futex_requeue(
            wake_ptr,
            1,
            current_value,
            OwnerAction::AssignWoken,
            requeue_ptr,
            requeue_count,
            requeue_owner,
        )
    })
}

#[syscall]
pub fn sys_futex_get_owner(
    value_ptr: UserInPtr<zx_futex_t>,
    koid: UserOutPtr<zx_koid_t>,
) -> Result<(), Status> {
    ltracef!("futex {:p}\n", value_ptr.as_ptr());
    ProcessDispatcher::with_current(|up| up.futex_context().futex_get_owner(value_ptr, koid))
}
