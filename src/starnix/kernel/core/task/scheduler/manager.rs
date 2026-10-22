// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::{RoleOverrides, Task};
use fidl_fuchsia_scheduler::{
    RoleManagerGetProfileForRoleRequest, RoleManagerMarker, RoleManagerSetRoleRequest,
    RoleManagerSynchronousProxy, RoleName, RoleTarget, RoleType,
};
use fuchsia_component::client::connect_to_protocol_sync;
use starnix_logging::{impossible_error, log_debug, log_warn, track_stub};
use starnix_sync::{LockDepMutex, ProfileHandleCacheLock};
use starnix_task_command::TaskCommand;
use starnix_uapi::errors::Errno;
use starnix_uapi::{
    SCHED_BATCH, SCHED_DEADLINE, SCHED_FIFO, SCHED_IDLE, SCHED_NORMAL, SCHED_RESET_ON_FORK,
    SCHED_RR, errno, error, sched_param,
};
use std::collections::HashMap;

pub struct SchedulerManager {
    role_manager: Option<RoleManagerSynchronousProxy>,
    role_overrides: RoleOverrides,
    profile_handle_cache: LockDepMutex<HashMap<String, zx::Profile>, ProfileHandleCacheLock>,
}

impl SchedulerManager {
    /// Create a new SchedulerManager which will apply any provided `role_overrides` before
    /// computing a role name based on a Task's scheduler state.
    pub fn new(role_overrides: RoleOverrides) -> SchedulerManager {
        let profile_handle_cache = Default::default();
        let role_manager = fuchsia_runtime::with_thread_self(|thread| {
            let role_manager = connect_to_protocol_sync::<RoleManagerMarker>().unwrap();
            if let Err(e) = Self::set_zx_thread_role(
                &role_manager,
                thread,
                SchedulerState::default().role_name(),
                &profile_handle_cache,
            ) {
                log_debug!("Setting thread role failed ({e:?}), will not set thread priority.");
                None
            } else {
                log_debug!("Thread role set successfully, scheduler manager initialized.");
                Some(role_manager)
            }
        });

        SchedulerManager { role_manager, role_overrides, profile_handle_cache }
    }

    /// Create a new empty SchedulerManager for testing.
    pub(crate) fn empty_for_tests() -> Self {
        Self {
            role_manager: None,
            role_overrides: RoleOverrides::new().build().unwrap(),
            profile_handle_cache: Default::default(),
        }
    }

    /// Create a new SchedulerManager with a custom role manager and overrides for testing.
    #[cfg(test)]
    pub(crate) fn new_for_tests(
        role_manager: Option<RoleManagerSynchronousProxy>,
        role_overrides: RoleOverrides,
    ) -> Self {
        Self { role_manager, role_overrides, profile_handle_cache: Default::default() }
    }

    /// Return the currently active role name for this task. Requires read access to `task`'s state,
    /// should only be called by code which is not already modifying the provided `task` or holding
    /// `task.scheduler_state`.
    pub(crate) fn role_name(
        &self,
        task: &Task,
        scheduler_state: &SchedulerState,
    ) -> Result<&str, Errno> {
        let thread_group = task.thread_group();
        let thread_group_state = thread_group.read();
        // Only inherit custom roles if the process has executed `execve`. Child processes created
        // via `fork` only inherit standard scheduling policies (SCHED_NORMAL/SCHED_FIFO) with
        // default profiles to maintain basic parity with Linux and prevent demand amplification.
        if thread_group_state.did_exec {
            let thread_name = task.command();
            let process_name = if task.tid == task.pid {
                thread_name.clone()
            } else {
                task.pid.get_task().map_err(|_| errno!(EINVAL))?.command()
            };

            return Ok(self.resolve_role_name(
                &process_name,
                &thread_name,
                &task.read().cpuset_path,
                task.is_leader(),
                scheduler_state,
            ));
        }
        Ok(scheduler_state.role_name())
    }

    fn resolve_role_name(
        &self,
        process_name: &TaskCommand,
        thread_name: &TaskCommand,
        cgroup_path: &str,
        is_leader: bool,
        scheduler_state: &SchedulerState,
    ) -> &str {
        if let Some(name) = self.role_overrides.get_role_name_with_filters(
            process_name,
            thread_name,
            cgroup_path,
            is_leader,
            scheduler_state.normal_priority().as_nice(),
        ) {
            return name;
        }
        scheduler_state.role_name()
    }

    /// Resolves the target scheduler role for `task` and commits the corresponding profile to its
    /// Zircon thread if it differs from the previously committed role.
    pub(crate) fn commit_thread_role(
        &self,
        task: &Task,
        scheduler_state: &mut SchedulerState,
    ) -> Result<(), Errno> {
        let Some(role_manager) = self.role_manager.as_ref() else {
            log_debug!("no role manager for setting role");
            return Ok(());
        };

        let thread_name = task.command();
        let process_name = if task.tid == task.pid {
            thread_name.clone()
        } else {
            task.pid
                .get_task()
                .map(|l| l.command())
                .unwrap_or_else(|_| TaskCommand::new(b"<leader exited>"))
        };
        let role_name = self.resolve_role_name(
            &process_name,
            &thread_name,
            &task.read().cpuset_path,
            task.is_leader(),
            scheduler_state,
        );
        if scheduler_state.committed_role_name.as_deref() == Some(role_name) {
            return Ok(());
        }

        let zircon_thread = {
            let running_state = match task.running_state() {
                Ok(live) => live,
                Err(_) => {
                    log_debug!(
                        "thread role update requested for task without live state, skipping"
                    );
                    return Ok(());
                }
            };
            let Some(zircon_thread) = running_state.thread.get() else {
                log_debug!("thread role update requested for task without thread, skipping");
                return Ok(());
            };
            zircon_thread.clone()
        };

        Self::set_zx_thread_role(
            role_manager,
            &zircon_thread.thread,
            role_name,
            &self.profile_handle_cache,
        )?;
        scheduler_state.committed_role_name = Some(role_name.to_string());

        Ok(())
    }

    fn set_zx_thread_role(
        role_manager: &RoleManagerSynchronousProxy,
        thread: &zx::Thread,
        role_name: &str,
        cache: &LockDepMutex<HashMap<String, zx::Profile>, ProfileHandleCacheLock>,
    ) -> Result<(), Errno> {
        Self::set_zx_thread_role_with_profile_setter(
            role_manager,
            thread,
            role_name,
            cache,
            |profile| thread.set_profile(profile, 0),
        )
    }

    fn set_zx_thread_role_with_profile_setter(
        role_manager: &RoleManagerSynchronousProxy,
        thread: &zx::Thread,
        role_name: &str,
        cache: &LockDepMutex<HashMap<String, zx::Profile>, ProfileHandleCacheLock>,
        set_profile: impl FnOnce(&zx::Profile) -> Result<(), zx::Status>,
    ) -> Result<(), Errno> {
        log_debug!(role_name; "setting thread role");

        {
            let params = cache.lock();
            if let Some(profile) = params.get(role_name) {
                return Self::set_zx_thread_profile(role_name, profile, set_profile);
            }
        }

        let request = RoleManagerGetProfileForRoleRequest {
            role: Some(RoleName { role: role_name.to_string() }),
            target: Some(RoleType::Task),
            ..Default::default()
        };
        match role_manager.get_profile_for_role(request, zx::MonotonicInstant::INFINITE) {
            Ok(Ok(response)) => {
                let Some(profile) = response.profile else {
                    log_warn!("GetRole returned success but no profile handle");
                    return error!(EINVAL);
                };

                let mut params = cache.lock();
                let profile = params.entry(role_name.to_string()).or_insert(profile);
                Self::set_zx_thread_profile(role_name, profile, set_profile)
            }
            Ok(Err(e)) => {
                log_warn!(e:%; "GetRole returned error");
                Self::set_zx_thread_role_legacy(role_manager, thread, role_name)
            }
            Err(e) => {
                log_warn!(e:%; "GetRole FIDL call failed");
                Self::set_zx_thread_role_legacy(role_manager, thread, role_name)
            }
        }
    }

    /// Applies `profile` to the target thread via `set_profile`.
    ///
    /// Returns:
    /// - `Ok(())` if the profile was applied to the thread, or if the thread was in
    ///   `zx::Status::BAD_STATE` (`INITIAL`, `DYING`, or `DEAD`). For a `zx::Thread`, Zircon only
    ///   returns `BAD_STATE` due to the thread's lifecycle state; profile parameters are validated
    ///   when the `zx::Profile` is created.
    /// - `Err(EINVAL)` if `profile` is invalid (`BAD_HANDLE`, `WRONG_TYPE`, `ACCESS_DENIED`) or
    ///   another error occurred.
    fn set_zx_thread_profile(
        role_name: &str,
        profile: &zx::Profile,
        set_profile: impl FnOnce(&zx::Profile) -> Result<(), zx::Status>,
    ) -> Result<(), Errno> {
        match set_profile(profile) {
            Ok(()) => Ok(()),
            // A task's underlying Zircon thread can transition to DYING or DEAD concurrently
            // after we snapshot its handle from `task.running_state()`. Ignore `BAD_STATE` just
            // as `commit_thread_role` ignores tasks whose live state has already been cleared.
            Err(zx::Status::BAD_STATE) => {
                log_debug!(
                    role_name;
                    "thread role update requested for dead or dying thread, skipping"
                );
                Ok(())
            }
            Err(e) => {
                log_warn!(role_name, e:%; "Failed to set thread profile from handle");
                error!(EINVAL)
            }
        }
    }

    fn set_zx_thread_role_legacy(
        role_manager: &RoleManagerSynchronousProxy,
        thread: &zx::Thread,
        role_name: &str,
    ) -> Result<(), Errno> {
        let thread = thread.duplicate_handle(zx::Rights::SAME_RIGHTS).map_err(impossible_error)?;
        let request = RoleManagerSetRoleRequest {
            target: Some(RoleTarget::Thread(thread)),
            role: Some(RoleName { role: role_name.to_string() }),
            ..Default::default()
        };
        let _ = role_manager.set_role(request, zx::MonotonicInstant::INFINITE).map_err(|err| {
            log_warn!(err:%; "Unable to set thread role.");
            errno!(EINVAL)
        })?;
        Ok(())
    }
}

/// The task normal priority, used for favoring or disfavoring a task running
/// with some non-real-time scheduling policies. Ranges from -20 to +19 in
/// "user-space" representation and +1 to +40 in "kernel-internal"
/// representation. See "The nice value" at sched(7) for full specification.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct NormalPriority {
    /// 1 (weakest) to 40 (strongest) (in "kernel-internal" representation),
    /// from setpriority()
    value: u8,
}

impl NormalPriority {
    const MIN_VALUE: u8 = 1;
    const DEFAULT_VALUE: u8 = 20;
    const MAX_VALUE: u8 = 40;

    /// Creates a normal priority from a value to be interpreted according
    /// to the "user-space nice" (-20..=19) scale, clamping values outside
    /// that scale.
    ///
    /// It would be strange for this to be called from anywhere outside of
    /// the setpriority system call.
    pub(crate) fn from_setpriority_syscall(user_nice: i32) -> Self {
        Self {
            value: (Self::DEFAULT_VALUE as i32)
                .saturating_sub(user_nice)
                .clamp(Self::MIN_VALUE as i32, Self::MAX_VALUE as i32) as u8,
        }
    }

    /// Returns this normal priority's integer representation according
    /// to the "user-space nice" (-20..=19) scale.
    pub fn as_nice(&self) -> i8 {
        (Self::DEFAULT_VALUE as i8) - (self.value as i8)
    }

    /// Returns this normal priority's integer representation according
    /// to the "kernel space nice" (1..=40) scale.
    pub(crate) fn raw_priority(&self) -> u8 {
        self.value
    }

    /// Returns whether this normal priority exceeds the given limit.
    pub(crate) fn exceeds(&self, limit: u64) -> bool {
        limit < (self.value as u64)
    }
}

impl std::default::Default for NormalPriority {
    fn default() -> Self {
        Self { value: Self::DEFAULT_VALUE }
    }
}

/// The task real-time priority, used for favoring or disfavoring a task
/// running with real-time scheduling policies. See "Scheduling policies"
/// at sched(7) for full specification.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct RealtimePriority {
    /// 1 (weakest) to 99 (strongest), from sched_setscheduler() and
    /// sched_setparam(). Only meaningfully used for Fifo and
    /// Round-Robin; set to 0 for other policies.
    value: u8,
}

impl RealtimePriority {
    const NON_REAL_TIME_VALUE: u8 = 0;
    const MIN_VALUE: u8 = 1;
    const MAX_VALUE: u8 = 99;

    const NON_REAL_TIME: RealtimePriority = RealtimePriority { value: Self::NON_REAL_TIME_VALUE };

    pub(crate) fn exceeds(&self, limit: u64) -> bool {
        limit < (self.value as u64)
    }
}

/// The scheduling policies described in "Scheduling policies" at sched(7).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SchedulingPolicy {
    Normal,
    Batch,
    Idle,
    Fifo,
    RoundRobin,
}

impl SchedulingPolicy {
    fn realtime_priority_min(&self) -> u8 {
        match self {
            Self::Normal | Self::Batch | Self::Idle => RealtimePriority::NON_REAL_TIME_VALUE,
            Self::Fifo | Self::RoundRobin => RealtimePriority::MIN_VALUE,
        }
    }

    fn realtime_priority_max(&self) -> u8 {
        match self {
            Self::Normal | Self::Batch | Self::Idle => RealtimePriority::NON_REAL_TIME_VALUE,
            Self::Fifo | Self::RoundRobin => RealtimePriority::MAX_VALUE,
        }
    }

    pub(crate) fn is_valid_priority(&self, priority: RealtimePriority) -> bool {
        (self.realtime_priority_min()..=self.realtime_priority_max()).contains(&priority.value)
    }

    pub(crate) fn realtime_priority_from(&self, priority: i32) -> Result<RealtimePriority, Errno> {
        let priority =
            RealtimePriority { value: u8::try_from(priority).map_err(|_| errno!(EINVAL))? };
        if !self.is_valid_priority(priority) {
            return error!(EINVAL);
        }
        Ok(priority)
    }
}

impl TryFrom<u32> for SchedulingPolicy {
    type Error = Errno;

    fn try_from(value: u32) -> Result<Self, Errno> {
        Ok(match value {
            SCHED_NORMAL => Self::Normal,
            SCHED_BATCH => Self::Batch,
            SCHED_IDLE => Self::Idle,
            SCHED_FIFO => Self::Fifo,
            SCHED_RR => Self::RoundRobin,
            _ => {
                return error!(EINVAL);
            }
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchedulerState {
    pub(crate) policy: SchedulingPolicy,
    /// Although nice is only used for Normal and Batch, normal priority
    /// ("nice") is still maintained, observable, and alterable when a
    /// task is using Idle, Fifo, and RoundRobin.
    pub(crate) normal_priority: NormalPriority,
    /// 1 (weakest) to 99 (strongest), from sched_setscheduler() and
    /// sched_setparam(). Only used for Fifo and Round-Robin.
    pub(crate) realtime_priority: RealtimePriority,
    pub(crate) reset_on_fork: bool,
    /// The last committed scheduler role name.
    committed_role_name: Option<String>,
}

impl SchedulerState {
    pub(crate) fn is_default(&self) -> bool {
        let default = Self::default();
        self.policy == default.policy
            && self.normal_priority == default.normal_priority
            && self.realtime_priority == default.realtime_priority
            && self.reset_on_fork == default.reset_on_fork
            && self.committed_role_name.as_deref().is_none_or(|role| role == default.role_name())
    }

    pub(crate) fn fork(&self) -> Self {
        if self.reset_on_fork {
            let (policy, normal_priority, realtime_priority) = if self.is_realtime() {
                // If the calling task has a real-time scheduling policy, the
                // policy given to child processes is SCHED_OTHER and the nice is
                // NormalPriority::default() (in all such cases and without caring
                // about whether the caller's nice had been stronger or weaker than
                // NormalPriority::default()).
                (
                    SchedulingPolicy::Normal,
                    NormalPriority::default(),
                    RealtimePriority::NON_REAL_TIME,
                )
            } else {
                // If the calling task has a non-real-time scheduling policy, the
                // state given to child processes is the same as that of the
                // caller except with the caller's nice clamped to
                // NormalPriority::default() at the strongest.
                (
                    self.policy,
                    std::cmp::min(self.normal_priority, NormalPriority::default()),
                    RealtimePriority::NON_REAL_TIME,
                )
            };
            Self {
                policy,
                normal_priority,
                realtime_priority,
                // This flag is disabled in child processes created by fork(2).
                reset_on_fork: false,
                committed_role_name: None,
            }
        } else {
            Self { committed_role_name: None, ..*self }
        }
    }

    /// Return the policy as an integer (SCHED_NORMAL, SCHED_BATCH, &c) bitwise-ored
    /// with the current reset-on-fork status (SCHED_RESET_ON_FORK or 0, depending).
    ///
    /// It would be strange for this to need to be called anywhere outside the
    /// implementation of the sched_getscheduler system call.
    pub(crate) fn policy_for_sched_getscheduler(&self) -> u32 {
        let mut base = match self.policy {
            SchedulingPolicy::Normal => SCHED_NORMAL,
            SchedulingPolicy::Batch => SCHED_BATCH,
            SchedulingPolicy::Idle => SCHED_IDLE,
            SchedulingPolicy::Fifo => SCHED_FIFO,
            SchedulingPolicy::RoundRobin => SCHED_RR,
        };
        if self.reset_on_fork {
            base |= SCHED_RESET_ON_FORK;
        }
        base
    }

    /// Return the priority as a field in a sched_param struct.
    ///
    /// It would be strange for this to need to be called anywhere outside the
    /// implementation of the sched_getparam system call.
    pub(crate) fn get_sched_param(&self) -> sched_param {
        sched_param {
            sched_priority: (if self.is_realtime() {
                self.realtime_priority.value
            } else {
                RealtimePriority::NON_REAL_TIME_VALUE
            }) as i32,
        }
    }

    pub fn normal_priority(&self) -> NormalPriority {
        self.normal_priority
    }

    /// Returns a monotonic priority key (`0..=139`) for ordering `FUTEX_WAIT` waiters:
    /// - `0`: `SCHED_IDLE`
    /// - `1..=40`: `SCHED_NORMAL` / `SCHED_BATCH` (`20` is default nice `0`)
    /// - `41..=139`: `SCHED_FIFO` / `SCHED_RR` (`40 + realtime_priority`)
    pub(crate) fn futex_priority(&self) -> u8 {
        match self.policy {
            SchedulingPolicy::Idle => 0,
            SchedulingPolicy::Normal | SchedulingPolicy::Batch => {
                self.normal_priority.raw_priority()
            }
            SchedulingPolicy::Fifo | SchedulingPolicy::RoundRobin => {
                NormalPriority::MAX_VALUE.saturating_add(self.realtime_priority.value)
            }
        }
    }

    /// Returns the task priority value as reported in `/proc/<pid>/stat` (see `task_prio` in
    /// Linux `kernel/sched/core.c` and `proc_pid_stat(5)`).
    ///
    /// For non-real-time scheduling policies (`SCHED_NORMAL`, `SCHED_BATCH`, `SCHED_IDLE`),
    /// this is `20 + nice` in the range `0..=39` (corresponding to nice `-20..=19`).
    /// For real-time scheduling policies (`SCHED_FIFO`, `SCHED_RR`), this is
    /// `-1 - rt_priority` in the range `-100..=-2` (corresponding to real-time priorities
    /// `1..=99`).
    pub fn task_priority(&self) -> i64 {
        match self.policy {
            SchedulingPolicy::Normal | SchedulingPolicy::Batch | SchedulingPolicy::Idle => {
                (NormalPriority::DEFAULT_VALUE as i64) + (self.normal_priority.as_nice() as i64)
            }
            SchedulingPolicy::Fifo | SchedulingPolicy::RoundRobin => {
                -1 - (self.realtime_priority.value as i64)
            }
        }
    }

    fn is_realtime(&self) -> bool {
        match self.policy {
            SchedulingPolicy::Normal | SchedulingPolicy::Batch | SchedulingPolicy::Idle => false,
            SchedulingPolicy::Fifo | SchedulingPolicy::RoundRobin => true,
        }
    }

    /// Returns a number 0-31 (inclusive) mapping Linux scheduler priority to a Zircon priority
    /// level for the fair scheduler.
    ///
    /// The range of 32 Zircon priorities is divided into a region for each flavor of Linux
    /// scheduling:
    ///
    /// 1. 0 is used for SCHED_IDLE, the lowest priority Linux tasks.
    /// 2. 6-15 (inclusive) is used for lower-than-default-priority SCHED_OTHER/SCHED_BATCH tasks.
    /// 3. 16 is used for the default priority SCHED_OTHER/SCHED_BATCH, the same as Zircon's
    ///    default for Fuchsia processes.
    /// 4. 17-26 (inclusive) is used for higher-than-default-priority SCHED_OTHER/SCHED_BATCH tasks.
    /// 5. Realtime tasks receive their own profile name.
    fn role_name(&self) -> &'static str {
        match self.policy {
            // Mapped to 0; see "the [...] nice value has no influence for [the SCHED_IDLE] policy"
            // at sched(7).
            SchedulingPolicy::Idle => FAIR_PRIORITY_ROLE_NAMES[0],

            // Configured with nice 0-40 and mapped to 6-26. 20 is the default nice which we want to
            // map to 16.
            SchedulingPolicy::Normal => {
                FAIR_PRIORITY_ROLE_NAMES[(self.normal_priority.value as usize / 2) + 6]
            }
            SchedulingPolicy::Batch => {
                track_stub!(TODO("https://fxbug.dev/308055542"), "SCHED_BATCH hinting");
                FAIR_PRIORITY_ROLE_NAMES[(self.normal_priority.value as usize / 2) + 6]
            }

            // Configured with priority 1-99, mapped to a constant bandwidth profile. Priority
            // between realtime tasks is ignored because we don't currently have a way to tell the
            // scheduler that a given realtime task is more important than another without
            // specifying an earlier deadline for the higher priority task. We can't specify
            // deadlines at runtime, so we'll treat their priorities all the same.
            SchedulingPolicy::Fifo | SchedulingPolicy::RoundRobin => REALTIME_ROLE_NAME,
        }
    }
}

impl std::default::Default for SchedulerState {
    fn default() -> Self {
        Self {
            policy: SchedulingPolicy::Normal,
            normal_priority: NormalPriority::default(),
            realtime_priority: RealtimePriority::NON_REAL_TIME,
            reset_on_fork: false,
            committed_role_name: None,
        }
    }
}

pub(crate) fn min_priority_for_sched_policy(policy: u32) -> Result<u8, Errno> {
    Ok(match policy {
        SCHED_DEADLINE => RealtimePriority::NON_REAL_TIME_VALUE,
        _ => SchedulingPolicy::try_from(policy)?.realtime_priority_min(),
    })
}

pub(crate) fn max_priority_for_sched_policy(policy: u32) -> Result<u8, Errno> {
    Ok(match policy {
        SCHED_DEADLINE => RealtimePriority::NON_REAL_TIME_VALUE,
        _ => SchedulingPolicy::try_from(policy)?.realtime_priority_max(),
    })
}

/// Names of RoleManager roles for each static Zircon priority in the fair scheduler.
/// The index in the array is equal to the static priority.
// LINT.IfChange
const FAIR_PRIORITY_ROLE_NAMES: [&str; 32] = [
    "fuchsia.starnix.fair.0",
    "fuchsia.starnix.fair.1",
    "fuchsia.starnix.fair.2",
    "fuchsia.starnix.fair.3",
    "fuchsia.starnix.fair.4",
    "fuchsia.starnix.fair.5",
    "fuchsia.starnix.fair.6",
    "fuchsia.starnix.fair.7",
    "fuchsia.starnix.fair.8",
    "fuchsia.starnix.fair.9",
    "fuchsia.starnix.fair.10",
    "fuchsia.starnix.fair.11",
    "fuchsia.starnix.fair.12",
    "fuchsia.starnix.fair.13",
    "fuchsia.starnix.fair.14",
    "fuchsia.starnix.fair.15",
    "fuchsia.starnix.fair.16",
    "fuchsia.starnix.fair.17",
    "fuchsia.starnix.fair.18",
    "fuchsia.starnix.fair.19",
    "fuchsia.starnix.fair.20",
    "fuchsia.starnix.fair.21",
    "fuchsia.starnix.fair.22",
    "fuchsia.starnix.fair.23",
    "fuchsia.starnix.fair.24",
    "fuchsia.starnix.fair.25",
    "fuchsia.starnix.fair.26",
    "fuchsia.starnix.fair.27",
    "fuchsia.starnix.fair.28",
    "fuchsia.starnix.fair.29",
    "fuchsia.starnix.fair.30",
    "fuchsia.starnix.fair.31",
];
const REALTIME_ROLE_NAME: &str = "fuchsia.starnix.realtime";
// LINT.ThenChange(src/starnix/config/starnix.profiles)

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn default_role_name() {
        assert_eq!(SchedulerState::default().role_name(), "fuchsia.starnix.fair.16");
    }

    #[fuchsia::test]
    fn scheduler_state_is_default() {
        assert!(SchedulerState::default().is_default());
        assert!(
            SchedulerState {
                committed_role_name: Some("fuchsia.starnix.fair.16".to_string()),
                ..SchedulerState::default()
            }
            .is_default()
        );
        assert!(
            !SchedulerState {
                committed_role_name: Some("overridden_role".to_string()),
                ..SchedulerState::default()
            }
            .is_default()
        );
        assert!(
            !SchedulerState { policy: SchedulingPolicy::Idle, ..SchedulerState::default() }
                .is_default()
        );
        assert!(
            !SchedulerState {
                normal_priority: NormalPriority { value: 10 },
                ..SchedulerState::default()
            }
            .is_default()
        );
        assert!(!SchedulerState { reset_on_fork: true, ..SchedulerState::default() }.is_default());
    }

    #[fuchsia::test]
    fn scheduler_state_fork_clears_committed_role_name() {
        let state = SchedulerState {
            policy: SchedulingPolicy::Fifo,
            normal_priority: NormalPriority { value: 30 },
            realtime_priority: RealtimePriority { value: 50 },
            reset_on_fork: false,
            committed_role_name: Some("fuchsia.starnix.realtime".to_string()),
        };
        assert_eq!(
            state.fork(),
            SchedulerState {
                policy: SchedulingPolicy::Fifo,
                normal_priority: NormalPriority { value: 30 },
                realtime_priority: RealtimePriority { value: 50 },
                reset_on_fork: false,
                committed_role_name: None,
            }
        );

        let reset_realtime_state = SchedulerState { reset_on_fork: true, ..state };
        assert_eq!(
            reset_realtime_state.fork(),
            SchedulerState {
                policy: SchedulingPolicy::Normal,
                normal_priority: NormalPriority::default(),
                realtime_priority: RealtimePriority::NON_REAL_TIME,
                reset_on_fork: false,
                committed_role_name: None,
            }
        );

        let reset_normal_state = SchedulerState {
            policy: SchedulingPolicy::Normal,
            normal_priority: NormalPriority { value: 30 },
            realtime_priority: RealtimePriority::NON_REAL_TIME,
            reset_on_fork: true,
            committed_role_name: Some("fuchsia.starnix.fair.21".to_string()),
        };
        assert_eq!(
            reset_normal_state.fork(),
            SchedulerState {
                policy: SchedulingPolicy::Normal,
                normal_priority: NormalPriority::default(),
                realtime_priority: RealtimePriority::NON_REAL_TIME,
                reset_on_fork: false,
                committed_role_name: None,
            }
        );
    }

    #[fuchsia::test]
    fn normal_with_non_default_nice_role_name() {
        assert_eq!(
            SchedulerState {
                policy: SchedulingPolicy::Normal,
                normal_priority: NormalPriority { value: 10 },
                realtime_priority: RealtimePriority::NON_REAL_TIME,
                reset_on_fork: false,
                committed_role_name: None,
            }
            .role_name(),
            "fuchsia.starnix.fair.11"
        );
        assert_eq!(
            SchedulerState {
                policy: SchedulingPolicy::Normal,
                normal_priority: NormalPriority { value: 27 },
                realtime_priority: RealtimePriority::NON_REAL_TIME,
                reset_on_fork: false,
                committed_role_name: None,
            }
            .role_name(),
            "fuchsia.starnix.fair.19"
        );
    }

    #[fuchsia::test]
    fn fifo_role_name() {
        assert_eq!(
            SchedulerState {
                policy: SchedulingPolicy::Fifo,
                normal_priority: NormalPriority::default(),
                realtime_priority: RealtimePriority { value: 1 },
                reset_on_fork: false,
                committed_role_name: None,
            }
            .role_name(),
            "fuchsia.starnix.realtime",
        );
        assert_eq!(
            SchedulerState {
                policy: SchedulingPolicy::Fifo,
                normal_priority: NormalPriority::default(),
                realtime_priority: RealtimePriority { value: 2 },
                reset_on_fork: false,
                committed_role_name: None,
            }
            .role_name(),
            "fuchsia.starnix.realtime",
        );
        assert_eq!(
            SchedulerState {
                policy: SchedulingPolicy::Fifo,
                normal_priority: NormalPriority::default(),
                realtime_priority: RealtimePriority { value: 99 },
                reset_on_fork: false,
                committed_role_name: None,
            }
            .role_name(),
            "fuchsia.starnix.realtime",
        );
    }

    #[fuchsia::test]
    fn idle_role_name() {
        assert_eq!(
            SchedulerState {
                policy: SchedulingPolicy::Idle,
                normal_priority: NormalPriority { value: 1 },
                realtime_priority: RealtimePriority::NON_REAL_TIME,
                reset_on_fork: false,
                committed_role_name: None,
            }
            .role_name(),
            "fuchsia.starnix.fair.0"
        );
        assert_eq!(
            SchedulerState {
                policy: SchedulingPolicy::Idle,
                normal_priority: NormalPriority::default(),
                realtime_priority: RealtimePriority::NON_REAL_TIME,
                reset_on_fork: false,
                committed_role_name: None,
            }
            .role_name(),
            "fuchsia.starnix.fair.0"
        );
        assert_eq!(
            SchedulerState {
                policy: SchedulingPolicy::Idle,
                normal_priority: NormalPriority { value: 40 },
                realtime_priority: RealtimePriority::NON_REAL_TIME,
                reset_on_fork: false,
                committed_role_name: None,
            }
            .role_name(),
            "fuchsia.starnix.fair.0"
        );
    }

    #[fuchsia::test]
    async fn role_overrides_non_realtime() {
        crate::testing::spawn_kernel_and_run_sync(|current_task| {
            let mut builder = RoleOverrides::new();
            builder.add("my_task", "my_task", None, "overridden_role");
            let overrides = builder.build().unwrap();
            let manager = SchedulerManager {
                role_manager: None,
                role_overrides: overrides,
                profile_handle_cache: Default::default(),
            };

            // Set did_exec = true so custom role overrides are applied.
            current_task.thread_group().write().did_exec = true;
            current_task.set_command_name(starnix_task_command::TaskCommand::new(b"my_task"));

            let mut state = SchedulerState::default();
            state.policy = SchedulingPolicy::Normal;

            let role = manager.role_name(current_task, &state).expect("role_name");
            assert_eq!(role, "overridden_role");
        })
        .await;
    }

    /// Creates a `zx::Profile` wrapping a `zx::Socket` handle.
    ///
    /// Creating a real `zx::Profile` requires the root `ProfileResource`, which is not available in
    /// unit tests. When passed to the real `zx::Thread::set_profile` syscall, Zircon rejects this
    /// handle with `zx::Status::WRONG_TYPE`, exercising the invalid-profile error path; tests for
    /// `Ok(())` and `zx::Status::BAD_STATE` use `set_zx_thread_role_with_profile_setter`.
    fn invalid_profile_for_test() -> zx::Profile {
        let (client, _server) = zx::Socket::create_stream();
        zx::Profile::from(zx::NullableHandle::from(client))
    }

    #[fuchsia::test]
    async fn commit_thread_role_failure_does_not_update_committed_role_name() {
        crate::testing::spawn_kernel_and_run_sync(|current_task| {
            let (role_manager, server_end) =
                fidl::endpoints::create_sync_proxy::<RoleManagerMarker>();
            drop(server_end);

            let manager = SchedulerManager {
                role_manager: Some(role_manager),
                role_overrides: RoleOverrides::new().build().unwrap(),
                profile_handle_cache: LockDepMutex::new(HashMap::from([(
                    "fuchsia.starnix.fair.16".to_string(),
                    invalid_profile_for_test(),
                )])),
            };

            assert_eq!(current_task.scheduler_state.lock().committed_role_name, None);
            assert_eq!(
                manager.commit_thread_role(current_task, &mut current_task.scheduler_state.lock()),
                error!(EINVAL)
            );
            assert_eq!(current_task.scheduler_state.lock().committed_role_name, None);

            current_task.scheduler_state.lock().committed_role_name =
                Some("previous_role".to_string());
            assert_eq!(
                manager.commit_thread_role(current_task, &mut current_task.scheduler_state.lock()),
                error!(EINVAL)
            );
            assert_eq!(
                current_task.scheduler_state.lock().committed_role_name.as_deref(),
                Some("previous_role")
            );
        })
        .await;
    }

    #[fuchsia::test]
    fn set_zx_thread_role_cached_profile_ok() {
        let (role_manager, server_end) = fidl::endpoints::create_sync_proxy::<RoleManagerMarker>();
        // Drop the server endpoint so any fallback FIDL call would fail with PEER_CLOSED.
        drop(server_end);

        let cache = LockDepMutex::new(HashMap::from([(
            "fuchsia.starnix.fair.16".to_string(),
            invalid_profile_for_test(),
        )]));

        fuchsia_runtime::with_thread_self(|thread| {
            let mut called = false;
            let result = SchedulerManager::set_zx_thread_role_with_profile_setter(
                &role_manager,
                thread,
                "fuchsia.starnix.fair.16",
                &cache,
                |_| {
                    called = true;
                    Ok(())
                },
            );
            assert_eq!(result, Ok(()));
            assert!(called);
        });
    }

    #[fuchsia::test]
    fn set_zx_thread_role_cached_profile_bad_state_is_ignored() {
        let (role_manager, server_end) = fidl::endpoints::create_sync_proxy::<RoleManagerMarker>();
        // Drop the server endpoint so any fallback FIDL call to RoleManager would fail.
        drop(server_end);

        let cache = LockDepMutex::new(HashMap::from([(
            "fuchsia.starnix.fair.16".to_string(),
            invalid_profile_for_test(),
        )]));

        fuchsia_runtime::with_thread_self(|thread| {
            let result = SchedulerManager::set_zx_thread_role_with_profile_setter(
                &role_manager,
                thread,
                "fuchsia.starnix.fair.16",
                &cache,
                |_| Err(zx::Status::BAD_STATE),
            );
            assert_eq!(result, Ok(()));
        });
    }

    #[fuchsia::test]
    fn set_zx_thread_role_cached_invalid_profile_does_not_call_role_manager() {
        let (role_manager, server_end) = fidl::endpoints::create_sync_proxy::<RoleManagerMarker>();
        let server_channel = server_end.into_channel();
        let watcher = std::thread::spawn(move || {
            let signals = server_channel
                .wait_one(
                    zx::Signals::CHANNEL_READABLE | zx::Signals::CHANNEL_PEER_CLOSED,
                    zx::MonotonicInstant::INFINITE,
                )
                .to_result()
                .expect("wait_one");
            signals.contains(zx::Signals::CHANNEL_READABLE)
        });

        let cache = LockDepMutex::new(HashMap::from([(
            "fuchsia.starnix.fair.16".to_string(),
            invalid_profile_for_test(),
        )]));

        fuchsia_runtime::with_thread_self(|thread| {
            let result = SchedulerManager::set_zx_thread_role(
                &role_manager,
                thread,
                "fuchsia.starnix.fair.16",
                &cache,
            );
            assert_eq!(result, error!(EINVAL));
        });

        drop(role_manager);
        let received_fidl_request = watcher.join().expect("watcher thread");
        assert!(!received_fidl_request);
    }

    #[fuchsia::test]
    fn set_zx_thread_profile_handles_outcomes() {
        let profile = invalid_profile_for_test();
        assert_eq!(
            SchedulerManager::set_zx_thread_profile(
                "fuchsia.starnix.fair.16",
                &profile,
                |_| Ok(())
            ),
            Ok(())
        );
        assert_eq!(
            SchedulerManager::set_zx_thread_profile("fuchsia.starnix.fair.16", &profile, |_| Err(
                zx::Status::BAD_STATE
            )),
            Ok(())
        );
        assert_eq!(
            SchedulerManager::set_zx_thread_profile("fuchsia.starnix.fair.16", &profile, |_| Err(
                zx::Status::WRONG_TYPE
            )),
            error!(EINVAL)
        );
    }

    #[fuchsia::test]
    fn futex_priority_orders_policies_and_priorities() {
        let idle = SchedulerState {
            policy: SchedulingPolicy::Idle,
            normal_priority: NormalPriority::from_setpriority_syscall(-20),
            ..SchedulerState::default()
        };
        let normal_low = SchedulerState {
            policy: SchedulingPolicy::Normal,
            normal_priority: NormalPriority::from_setpriority_syscall(19),
            ..SchedulerState::default()
        };
        let normal_default = SchedulerState::default();
        let batch_high = SchedulerState {
            policy: SchedulingPolicy::Batch,
            normal_priority: NormalPriority::from_setpriority_syscall(-20),
            ..SchedulerState::default()
        };
        let fifo_min = SchedulerState {
            policy: SchedulingPolicy::Fifo,
            realtime_priority: RealtimePriority { value: 1 },
            ..SchedulerState::default()
        };
        let rr_max = SchedulerState {
            policy: SchedulingPolicy::RoundRobin,
            realtime_priority: RealtimePriority { value: 99 },
            ..SchedulerState::default()
        };

        assert_eq!(idle.futex_priority(), 0);
        assert_eq!(normal_low.futex_priority(), 1);
        assert_eq!(normal_default.futex_priority(), 20);
        assert_eq!(batch_high.futex_priority(), 40);
        assert_eq!(fifo_min.futex_priority(), 41);
        assert_eq!(rr_max.futex_priority(), 139);
    }

    #[fuchsia::test]
    fn task_priority_values() {
        assert_eq!(SchedulerState::default().task_priority(), 20);
        assert_eq!(
            SchedulerState {
                normal_priority: NormalPriority::from_setpriority_syscall(1),
                ..SchedulerState::default()
            }
            .task_priority(),
            21
        );
        assert_eq!(
            SchedulerState {
                normal_priority: NormalPriority::from_setpriority_syscall(-20),
                ..SchedulerState::default()
            }
            .task_priority(),
            0
        );
        assert_eq!(
            SchedulerState {
                normal_priority: NormalPriority::from_setpriority_syscall(19),
                ..SchedulerState::default()
            }
            .task_priority(),
            39
        );
        assert_eq!(
            SchedulerState {
                policy: SchedulingPolicy::Batch,
                normal_priority: NormalPriority::from_setpriority_syscall(-20),
                ..SchedulerState::default()
            }
            .task_priority(),
            0
        );
        assert_eq!(
            SchedulerState {
                policy: SchedulingPolicy::Idle,
                normal_priority: NormalPriority::from_setpriority_syscall(-20),
                ..SchedulerState::default()
            }
            .task_priority(),
            0
        );
        assert_eq!(
            SchedulerState {
                policy: SchedulingPolicy::Idle,
                normal_priority: NormalPriority::from_setpriority_syscall(5),
                ..SchedulerState::default()
            }
            .task_priority(),
            25
        );
        assert_eq!(
            SchedulerState {
                policy: SchedulingPolicy::Fifo,
                realtime_priority: RealtimePriority { value: 1 },
                ..SchedulerState::default()
            }
            .task_priority(),
            -2
        );
        assert_eq!(
            SchedulerState {
                policy: SchedulingPolicy::RoundRobin,
                realtime_priority: RealtimePriority { value: 99 },
                ..SchedulerState::default()
            }
            .task_priority(),
            -100
        );
    }
}
