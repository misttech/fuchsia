// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::security::selinux_hooks::PerfEventState;
use crate::security::{PerfEventType, TargetTaskType};
use crate::task::CurrentTask;
use linux_uapi::perf_event_attr;
use selinux::{
    Cap2Class, CapClass, CommonCap2Permission, CommonCapPermission, ForClass, PerfEventPermission,
    SecurityServer,
};
use starnix_uapi::errors::Errno;

use super::{check_self_permission, current_task_state};

/// Checks whether `current_task` has the necessary permissions to open a perf_event for the given
/// target task.
pub(in crate::security) fn check_perf_event_open_access(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    target_task_type: TargetTaskType<'_>,
    attr: &perf_event_attr,
    event_type: PerfEventType,
) -> Result<(), Errno> {
    let audit_context = current_task.into();
    let subject_sid = current_task_state(current_task).lock().current_sid;
    // Always check `perf_event { open }` permission on the current task.
    check_self_permission(
        &security_server.as_permission_check(),
        current_task,
        subject_sid,
        PerfEventPermission::Open,
        audit_context,
    )?;
    // Check capability `capability2 { perfmon }` first, and if it fails check
    // `capability { sys_admin }` instead.
    if check_self_permission(
        &security_server.as_permission_check(),
        current_task,
        subject_sid,
        CommonCap2Permission::Perfmon.for_class(Cap2Class::Capability2),
        audit_context,
    )
    .is_err()
    {
        check_self_permission(
            &security_server.as_permission_check(),
            current_task,
            subject_sid,
            CommonCapPermission::SysAdmin.for_class(CapClass::Capability),
            audit_context,
        )?;
    }

    // Check `perf_event { kernel }` permission when `exclude_kernel` is 0.
    if attr.exclude_kernel() == 0 {
        check_self_permission(
            &security_server.as_permission_check(),
            current_task,
            subject_sid,
            PerfEventPermission::Kernel,
            audit_context,
        )?;
    }

    // Check `perf_event { cpu }` permission when
    // - type is PERF_TYPE_SOFTWARE or
    // - type is PERF_TYPE_HARDWARE or
    // - type is in [CACHE, TRACEPOINT, BREAKPOINT, RAW) and pid == -1
    let check_cpu = match event_type {
        PerfEventType::Software | PerfEventType::Hardware => true,
        PerfEventType::HwCache
        | PerfEventType::Tracepoint
        | PerfEventType::Breakpoint
        | PerfEventType::Raw => matches!(target_task_type, TargetTaskType::AllTasks),
    };
    if check_cpu {
        check_self_permission(
            &security_server.as_permission_check(),
            current_task,
            subject_sid,
            PerfEventPermission::Cpu,
            audit_context,
        )?;
    }

    // Check `perf_event { tracepoint }` permission when type is PERF_TYPE_TRACEPOINT
    if event_type == PerfEventType::Tracepoint {
        check_self_permission(
            &security_server.as_permission_check(),
            current_task,
            subject_sid,
            PerfEventPermission::Tracepoint,
            audit_context,
        )?;
    }

    Ok(())
}

/// Returns the SID to be used for a PerfEventFileState object upon creation.
pub(in crate::security) fn perf_event_alloc(current_task: &CurrentTask) -> PerfEventState {
    PerfEventState { sid: current_task_state(current_task).lock().current_sid }
}
