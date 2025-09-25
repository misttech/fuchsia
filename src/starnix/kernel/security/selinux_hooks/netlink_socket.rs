// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::TODO_DENY;
use crate::security::selinux_hooks::{KernelPermission, current_task_state, socket};
use crate::task::CurrentTask;
use crate::vfs::socket::{NetlinkFamily, Socket, SocketDomain};
use linux_uapi::{
    AUDIT_ADD_RULE, AUDIT_DEL_RULE, AUDIT_FIRST_USER_MSG, AUDIT_FIRST_USER_MSG2, AUDIT_GET,
    AUDIT_GET_FEATURE, AUDIT_LAST_USER_MSG, AUDIT_LAST_USER_MSG2, AUDIT_LIST_RULES, AUDIT_SET,
    AUDIT_SET_FEATURE, AUDIT_TTY_SET, AUDIT_USER, RTM_DELADDR, RTM_DELLINK, RTM_DELROUTE,
    RTM_GETADDR, RTM_GETLINK, RTM_GETNEIGH, RTM_GETROUTE, RTM_NEWADDR, RTM_NEWLINK, RTM_NEWROUTE,
    SOCK_DESTROY, SOCK_DIAG_BY_FAMILY, XFRM_MSG_DELPOLICY, XFRM_MSG_DELSA, XFRM_MSG_GETPOLICY,
    XFRM_MSG_GETSA, XFRM_MSG_NEWPOLICY, XFRM_MSG_NEWSA,
};
use selinux::{
    NetlinkAuditSocketPermission, NetlinkRouteSocketPermission, NetlinkTcpDiagSocketPermission,
    NetlinkXfrmSocketPermission, SecurityServer,
};
use starnix_logging::track_stub;
use starnix_uapi::errors::Errno;

/// Computes the required netlink message permission for `netlink_family` and `message_type`.
fn compute_netlink_message_permission(
    netlink_family: &NetlinkFamily,
    message_type: u16,
) -> Option<KernelPermission> {
    match netlink_family {
        NetlinkFamily::Route => match message_type as u32 {
            RTM_GETROUTE | RTM_GETLINK | RTM_GETADDR | RTM_GETNEIGH => {
                Some(NetlinkRouteSocketPermission::NlmsgRead.into())
            }
            RTM_NEWROUTE | RTM_DELROUTE | RTM_NEWLINK | RTM_DELLINK | RTM_NEWADDR | RTM_DELADDR => {
                Some(NetlinkRouteSocketPermission::NlmsgWrite.into())
            }
            _ => None,
        },
        NetlinkFamily::Audit => match message_type as u32 {
            AUDIT_GET | AUDIT_GET_FEATURE => Some(NetlinkAuditSocketPermission::NlmsgRead.into()),
            AUDIT_LIST_RULES => Some(NetlinkAuditSocketPermission::NlmsgReadPriv.into()),
            AUDIT_USER
            | AUDIT_FIRST_USER_MSG..=AUDIT_LAST_USER_MSG
            | AUDIT_FIRST_USER_MSG2..=AUDIT_LAST_USER_MSG2 => {
                Some(NetlinkAuditSocketPermission::NlmsgRelay.into())
            }
            AUDIT_TTY_SET => Some(NetlinkAuditSocketPermission::NlmsgTtyAudit.into()),
            AUDIT_SET | AUDIT_SET_FEATURE | AUDIT_ADD_RULE | AUDIT_DEL_RULE => {
                Some(NetlinkAuditSocketPermission::NlmsgWrite.into())
            }
            _ => None,
        },
        NetlinkFamily::SockDiag => match message_type as u32 {
            SOCK_DIAG_BY_FAMILY => Some(NetlinkTcpDiagSocketPermission::NlmsgRead.into()),
            SOCK_DESTROY => Some(NetlinkTcpDiagSocketPermission::NlmsgWrite.into()),
            _ => None,
        },
        NetlinkFamily::Xfrm => match message_type as u32 {
            XFRM_MSG_GETSA | XFRM_MSG_GETPOLICY => {
                Some(NetlinkXfrmSocketPermission::NlmsgRead.into())
            }
            XFRM_MSG_NEWSA | XFRM_MSG_DELSA | XFRM_MSG_NEWPOLICY | XFRM_MSG_DELPOLICY => {
                Some(NetlinkXfrmSocketPermission::NlmsgWrite.into())
            }
            _ => None,
        },
        // Other Netlink families don't have message permissions besides the common socket
        // permissions.
        _ => None,
    }
}

/// Checks if the Netlink `socket` is allowed to send a message of `message_type`.
pub fn check_netlink_send_access(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    socket: &Socket,
    message_type: u16,
) -> Result<(), Errno> {
    assert_eq!(
        socket.domain,
        SocketDomain::Netlink,
        "check_netlink_send_access called for non-Netlink socket"
    );
    let netlink_family = NetlinkFamily::from_raw(socket.protocol.as_raw());
    let Some(permission) = compute_netlink_message_permission(&netlink_family, message_type) else {
        // No message permissions are required for this netlink family and message type.
        return Ok(());
    };

    let current_sid = current_task_state(current_task).lock().current_sid;
    let Some(socket_node) = socket.fs_node() else {
        track_stub!(
            TODO("https://fxbug.dev/414583985"),
            "check_netlink_send_access called without FsNode"
        );
        return Ok(());
    };
    socket::todo_has_socket_permission(
        TODO_DENY!("https://fxbug.dev/364569156", "Enforce netlink_send"),
        &security_server.as_permission_check(),
        current_task,
        current_sid,
        &socket_node,
        permission,
        current_task.into(),
    )
}
