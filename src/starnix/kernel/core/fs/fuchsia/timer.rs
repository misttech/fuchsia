// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::power::OnWakeOps;
use crate::task::{CurrentTask, Kernel};
use crate::time::TargetTime;
use crate::time::utc::estimate_boot_deadline_from_utc;
use crate::vfs::timer::{TimelineChangeObserver, TimerOps};
use starnix_uapi::errors::Errno;
use starnix_uapi::{error, from_status_like_fdio};
use std::sync::{Arc, Weak};
use zx::HandleRef;

pub struct MonotonicZxTimer {
    timer: zx::MonotonicTimer,
}

impl MonotonicZxTimer {
    pub fn new() -> Self {
        // Starnix passes the task's `timerslack_ns` to `timer.set()` below
        // (note: Linux timerfd uses a zero range in `hrtimer_start()`, whereas
        // `timerslack_ns` in Linux only applies to `nanosleep`/`poll`/`select`/
        // `epoll`/`futex`). Use `TimerSlackMode::Late` so coalescing never
        // causes the timer to fire before its deadline.
        Self { timer: zx::MonotonicTimer::create_with_slack_mode(zx::TimerSlackMode::Late) }
    }
}

impl TimerOps for MonotonicZxTimer {
    fn start(
        &self,
        current_task: &CurrentTask,
        _source: Option<Weak<dyn OnWakeOps>>,
        deadline: TargetTime,
    ) -> Result<(), Errno> {
        let timerslack = current_task.read().get_timerslack();
        match deadline {
            TargetTime::Monotonic(t) => {
                self.timer.set(t, timerslack).map_err(|status| from_status_like_fdio!(status))?
            }
            TargetTime::BootInstant(_) | TargetTime::RealTime(_) => return error!(EINVAL),
        };

        Ok(())
    }

    fn stop(&self, _kernel: &Arc<Kernel>) -> Result<(), Errno> {
        self.timer.cancel().map_err(|status| from_status_like_fdio!(status))
    }

    fn as_handle_ref(&self) -> HandleRef<'_> {
        self.timer.as_handle_ref()
    }

    // Not supported.
    fn get_timeline_change_observer(&self, _: &CurrentTask) -> Option<TimelineChangeObserver> {
        None
    }
}

pub struct BootZxTimer {
    timer: zx::BootTimer,
}

impl BootZxTimer {
    pub fn new() -> Self {
        // See comment in `MonotonicZxTimer::new`: use `TimerSlackMode::Late`
        // so coalescing never causes the timer to fire before its deadline.
        Self { timer: zx::BootTimer::create_with_slack_mode(zx::TimerSlackMode::Late) }
    }
}

impl TimerOps for BootZxTimer {
    fn start(
        &self,
        current_task: &CurrentTask,
        _source: Option<Weak<dyn OnWakeOps>>,
        deadline: TargetTime,
    ) -> Result<(), Errno> {
        let timerslack = current_task.read().get_timerslack();
        match deadline {
            TargetTime::BootInstant(t) => {
                self.timer.set(t, timerslack).map_err(|status| from_status_like_fdio!(status))?
            }
            TargetTime::RealTime(t) => {
                let (boot_instant, _) = estimate_boot_deadline_from_utc(t);
                self.timer
                    .set(boot_instant, timerslack)
                    .map_err(|status| from_status_like_fdio!(status))?
            }
            TargetTime::Monotonic(_) => return error!(EINVAL),
        }
        Ok(())
    }

    fn stop(&self, _kernel: &Arc<Kernel>) -> Result<(), Errno> {
        self.timer.cancel().map_err(|status| from_status_like_fdio!(status))
    }

    fn as_handle_ref(&self) -> HandleRef<'_> {
        self.timer.as_handle_ref()
    }

    // Not supported.
    fn get_timeline_change_observer(&self, _: &CurrentTask) -> Option<TimelineChangeObserver> {
        None
    }
}
