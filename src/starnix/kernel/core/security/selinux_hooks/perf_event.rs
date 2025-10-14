// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::security::{PerfEventType, TargetTaskType};
use crate::task::CurrentTask;
use linux_uapi::perf_event_attr;
use selinux::{InitialSid, PerfEventPermission, SecurityServer};
use starnix_uapi::errors::Errno;

use super::{check_permission, current_task_state};

/// Checks whether `current_task` has the necessary permissions to open a perf_event for the given
/// target task.
pub fn check_perf_event_open_access(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    target_task_type: TargetTaskType<'_>,
    _attr: &perf_event_attr,
    event_type: PerfEventType,
) -> Result<(), Errno> {
    let audit_context = current_task.into();
    let subject_sid = current_task_state(current_task).lock().current_sid;
    let target_sid = match target_task_type {
        TargetTaskType::CurrentTask => subject_sid,
        TargetTaskType::AllTasks => InitialSid::Kernel.into(),
        TargetTaskType::Task(target_task) => target_task.security_state.lock().current_sid,
    };
    check_permission(
        &security_server.as_permission_check(),
        current_task,
        subject_sid,
        target_sid,
        PerfEventPermission::Open,
        audit_context,
    )?;

    match event_type {
        PerfEventType::Hardware | PerfEventType::HwCache | PerfEventType::Software => {
            check_permission(
                &security_server.as_permission_check(),
                current_task,
                subject_sid,
                target_sid,
                PerfEventPermission::Cpu,
                audit_context,
            )?
        }
        PerfEventType::Tracepoint => {
            check_permission(
                &security_server.as_permission_check(),
                current_task,
                subject_sid,
                target_sid,
                PerfEventPermission::Kernel,
                audit_context,
            )?;
            check_permission(
                &security_server.as_permission_check(),
                current_task,
                subject_sid,
                target_sid,
                PerfEventPermission::Tracepoint,
                audit_context,
            )?
        }
        _ => {}
    }
    Ok(())
}
