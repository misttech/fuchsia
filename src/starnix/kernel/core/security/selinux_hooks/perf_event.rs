// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::perf::PerfEventFile;
use crate::security::PerfEventOpenType;
use crate::security::selinux_hooks::{PerfEventState, check_permission};
use crate::task::CurrentTask;
use selinux::{PerfEventPermission, SecurityServer};
use starnix_uapi::errors::Errno;

use super::{build_permission_check, check_self_permission, current_task_state};

/// Checks whether `current_task` has the `perf_event` permission corresponding to
/// `perf_event_open_type`.
pub(in crate::security) fn check_perf_event_open_access(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    perf_event_open_type: PerfEventOpenType,
) -> Result<(), Errno> {
    let permission = match perf_event_open_type {
        PerfEventOpenType::Open => PerfEventPermission::Open,
        PerfEventOpenType::Cpu => PerfEventPermission::Cpu,
        PerfEventOpenType::Kernel => PerfEventPermission::Kernel,
        // TODO(https://fxbug.dev/501038871): Check `PerfEventPermission::Tracepoint`. It is
        // currently not checked, to match the observed behaviour for tracepoint events, which
        // only require it when sampling raw tracepoint data or the ftrace function event.
        PerfEventOpenType::Tracepoint => return Ok(()),
    };
    let subject_sid = current_task_state(current_task).current_sid;
    check_self_permission(
        &build_permission_check(current_task, security_server),
        current_task,
        subject_sid,
        permission,
        current_task.into(),
    )
}

/// Returns the SID to be used for a PerfEventFileState object upon creation.
pub(in crate::security) fn perf_event_alloc(current_task: &CurrentTask) -> PerfEventState {
    PerfEventState { sid: current_task_state(current_task).current_sid }
}

/// Checks whether `current_task` has the necessary permissions to read the given `perf_event_file`.
pub(in crate::security) fn check_perf_event_read_access(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    perf_event_file: &PerfEventFile,
) -> Result<(), Errno> {
    let audit_context = current_task.into();
    let subject_sid = current_task_state(current_task).current_sid;
    let target_sid = perf_event_file.security_state.state.sid;
    check_permission(
        &build_permission_check(current_task, security_server),
        current_task,
        subject_sid,
        target_sid,
        PerfEventPermission::Read,
        audit_context,
    )
}

/// Checks whether `current_task` has the necessary permissions to write to the given `perf_event_file`.
pub(in crate::security) fn check_perf_event_write_access(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    perf_event_file: &PerfEventFile,
) -> Result<(), Errno> {
    let audit_context = current_task.into();
    let subject_sid = current_task_state(current_task).current_sid;
    let target_sid = perf_event_file.security_state.state.sid;
    check_permission(
        &build_permission_check(current_task, security_server),
        current_task,
        subject_sid,
        target_sid,
        PerfEventPermission::Write,
        audit_context,
    )
}
