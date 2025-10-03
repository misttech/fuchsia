// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::TODO_DENY;
use crate::security::selinux_hooks::{
    BinderConnectionState, check_permission, check_self_permission, current_task_state,
    todo_check_permission,
};
use crate::task::CurrentTask;
use selinux::{BinderPermission, SecurityServer};
use starnix_core::task::Task;
use starnix_uapi::errors::Errno;

/// Returns the security state to be assigned to a Binder connection. This is defined as the
/// security context of the creating task.
pub(in crate::security) fn binder_connection_alloc(
    current_task: &CurrentTask,
) -> BinderConnectionState {
    BinderConnectionState { sid: current_task_state(current_task).lock().current_sid }
}

/// Returns the serialized Security Context associated with the given state.
pub(in crate::security) fn binder_get_context(
    security_server: &SecurityServer,
    state: &BinderConnectionState,
) -> Option<Vec<u8>> {
    security_server.sid_to_security_context(state.sid)
}

/// Checks whether the given `current_task` can become the binder context manager.
pub(in crate::security) fn binder_set_context_mgr(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
) -> Result<(), Errno> {
    let audit_context = current_task.into();
    let sid = current_task_state(current_task).lock().current_sid;
    check_self_permission(
        &security_server.as_permission_check(),
        current_task,
        sid,
        BinderPermission::SetContextMgr,
        audit_context,
    )
}

/// Checks whether the given `current_task` has permission to send a binder transaction
/// to the `target_task`.
pub(in crate::security) fn binder_transaction(
    security_server: &SecurityServer,
    connection_security_state: &BinderConnectionState,
    current_task: &CurrentTask,
    target_task: &Task,
) -> Result<(), Errno> {
    let audit_context = current_task.into();
    let source_sid = current_task_state(current_task).lock().current_sid;
    let target_sid = target_task.security_state.lock().current_sid;
    if source_sid != connection_security_state.sid {
        check_permission(
            &security_server.as_permission_check(),
            current_task,
            source_sid,
            connection_security_state.sid,
            BinderPermission::Impersonate,
            audit_context,
        )?;
    }
    check_permission(
        &security_server.as_permission_check(),
        current_task,
        connection_security_state.sid,
        target_sid,
        BinderPermission::Call,
        audit_context,
    )?;
    Ok(())
}

/// Checks whether the given `current_task` has permission to transfer Binder objects
/// to the `target_task`.
pub(in crate::security) fn binder_transfer_binder(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    target_task: &Task,
) -> Result<(), Errno> {
    let audit_context = current_task.into();
    let source_sid = current_task_state(current_task).lock().current_sid;
    let target_sid = target_task.security_state.lock().current_sid;
    todo_check_permission(
        TODO_DENY!("https://fxbug.dev/364569748", "Enforce all the time in all contexts."),
        &security_server.as_permission_check(),
        current_task,
        source_sid,
        target_sid,
        BinderPermission::Transfer,
        audit_context,
    )
}
