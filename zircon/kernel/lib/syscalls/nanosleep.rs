// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::counters::define_kcounter;
use crate::object::ProcessDispatcher;
use crate::platform_rs::timer::current_mono_time;
use debug::ltracef;
use syscalls_macro::syscall;
use zx_status::Status;
use zx_types::{zx_duration_mono_t, zx_instant_mono_t};

const LOCAL_TRACE: u32 = 0;

define_kcounter!(SYSCALLS_ZX_NANOSLEEP, "syscalls.zx_nanosleep", Sum);
define_kcounter!(SYSCALLS_ZX_NANOSLEEP_ZERO_DURATION, "syscalls.zx_nanosleep_zero_duration", Sum);

unsafe extern "C" {
    fn cpp_thread_current_sleep_nanosleep(
        deadline: zx_instant_mono_t,
        now: zx_instant_mono_t,
        slack_amount: zx_duration_mono_t,
    ) -> zx_types::zx_status_t;
}

#[syscall]
pub fn sys_nanosleep(deadline: zx_instant_mono_t) -> Result<(), Status> {
    ltracef!("nseconds {}\n", deadline);
    SYSCALLS_ZX_NANOSLEEP.add(1);

    if deadline <= 0 {
        SYSCALLS_ZX_NANOSLEEP_ZERO_DURATION.add(1);
        return Ok(());
    }

    let now = current_mono_time();
    let slack_policy = ProcessDispatcher::with_current(|up| up.get_timer_slack_policy_amount());

    // This syscall is declared as "blocking", so a higher layer will automatically
    // retry if we return ZX_ERR_INTERNAL_INTR_RETRY.
    //
    // SAFETY: Calling C++ thread sleep helper from kernel thread context is safe.
    let status = unsafe { cpp_thread_current_sleep_nanosleep(deadline, now.0, slack_policy) };
    Status::ok(status)?;
    Ok(())
}
