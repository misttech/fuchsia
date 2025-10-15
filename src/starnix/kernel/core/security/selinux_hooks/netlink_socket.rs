// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::security::selinux_hooks::{
    FsNodeSidAndClass, KernelPermission, check_permission_and_xperms, current_task_state,
    fs_node_effective_sid_and_class, socket,
};
use crate::task::CurrentTask;
use crate::vfs::socket::{NetlinkFamily, Socket, SocketDomain};
use linux_uapi::{
    AUDIT_ADD_RULE, AUDIT_DEL_RULE, AUDIT_FIRST_USER_MSG, AUDIT_FIRST_USER_MSG2, AUDIT_GET,
    AUDIT_GET_FEATURE, AUDIT_LAST_USER_MSG, AUDIT_LAST_USER_MSG2, AUDIT_LIST_RULES, AUDIT_SET,
    AUDIT_SET_FEATURE, AUDIT_TTY_SET, AUDIT_USER, RTM_DELADDR, RTM_DELCHAIN, RTM_DELLINK,
    RTM_DELLINKPROP, RTM_DELNEIGH, RTM_DELNSID, RTM_DELQDISC, RTM_DELROUTE, RTM_DELRULE,
    RTM_DELTCLASS, RTM_DELTFILTER, RTM_GETADDR, RTM_GETCHAIN, RTM_GETLINK, RTM_GETNEIGH,
    RTM_GETNEIGHTBL, RTM_GETNSID, RTM_GETQDISC, RTM_GETROUTE, RTM_GETRULE, RTM_GETTCLASS,
    RTM_GETTFILTER, RTM_NEWADDR, RTM_NEWCHAIN, RTM_NEWLINK, RTM_NEWLINKPROP, RTM_NEWNDUSEROPT,
    RTM_NEWNEIGH, RTM_NEWNEIGHTBL, RTM_NEWNSID, RTM_NEWPREFIX, RTM_NEWQDISC, RTM_NEWROUTE,
    RTM_NEWRULE, RTM_NEWTCLASS, RTM_NEWTFILTER, RTM_SETLINK, RTM_SETNEIGHTBL, SOCK_DESTROY,
    SOCK_DIAG_BY_FAMILY, XFRM_MSG_DELPOLICY, XFRM_MSG_DELSA, XFRM_MSG_GETPOLICY, XFRM_MSG_GETSA,
    XFRM_MSG_NEWPOLICY, XFRM_MSG_NEWSA,
};
use selinux::policy::XpermsKind;
use selinux::{
    NetlinkAuditSocketPermission, NetlinkRouteSocketPermission, NetlinkTcpDiagSocketPermission,
    NetlinkXfrmSocketPermission, SecurityServer,
};
use starnix_logging::track_stub;
use starnix_uapi::errors::Errno;

pub struct NlmsgPermissions {
    permission: Option<KernelPermission>,
    xperm: NlmsgExtendedPermission,
}

pub struct NlmsgExtendedPermission {
    permission: KernelPermission,
    value: u16,
}

/// Computes the required netlink message permission for `netlink_family` and `message_type`.
fn compute_netlink_message_permissions(
    netlink_family: &NetlinkFamily,
    message_type: u16,
) -> Option<NlmsgPermissions> {
    match netlink_family {
        NetlinkFamily::Route => Some(NlmsgPermissions {
            permission: match message_type as u32 {
                RTM_GETROUTE | RTM_GETLINK | RTM_GETADDR | RTM_GETNEIGH | RTM_GETNEIGHTBL
                | RTM_GETQDISC | RTM_GETCHAIN | RTM_GETNSID | RTM_GETRULE | RTM_GETTCLASS
                | RTM_GETTFILTER => Some(NetlinkRouteSocketPermission::NlmsgRead.into()),
                RTM_NEWROUTE | RTM_DELROUTE | RTM_NEWLINK | RTM_DELLINK | RTM_NEWADDR
                | RTM_DELADDR | RTM_DELCHAIN | RTM_DELLINKPROP | RTM_DELNEIGH | RTM_DELNSID
                | RTM_DELQDISC | RTM_DELRULE | RTM_DELTCLASS | RTM_DELTFILTER | RTM_NEWCHAIN
                | RTM_NEWLINKPROP | RTM_NEWNDUSEROPT | RTM_NEWNEIGH | RTM_NEWNEIGHTBL
                | RTM_NEWNSID | RTM_NEWPREFIX | RTM_NEWQDISC | RTM_NEWRULE | RTM_NEWTCLASS
                | RTM_NEWTFILTER | RTM_SETLINK | RTM_SETNEIGHTBL => {
                    Some(NetlinkRouteSocketPermission::NlmsgWrite.into())
                }
                _ => None,
            },
            xperm: NlmsgExtendedPermission {
                permission: NetlinkRouteSocketPermission::Nlmsg.into(),
                value: message_type,
            },
        }),
        NetlinkFamily::Audit => Some(NlmsgPermissions {
            permission: match message_type as u32 {
                AUDIT_GET | AUDIT_GET_FEATURE => {
                    Some(NetlinkAuditSocketPermission::NlmsgRead.into())
                }
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
            xperm: NlmsgExtendedPermission {
                permission: NetlinkAuditSocketPermission::Nlmsg.into(),
                value: message_type,
            },
        }),
        NetlinkFamily::SockDiag => Some(NlmsgPermissions {
            permission: match message_type as u32 {
                SOCK_DIAG_BY_FAMILY => Some(NetlinkTcpDiagSocketPermission::NlmsgRead.into()),
                SOCK_DESTROY => Some(NetlinkTcpDiagSocketPermission::NlmsgWrite.into()),
                _ => None,
            },
            xperm: NlmsgExtendedPermission {
                permission: NetlinkTcpDiagSocketPermission::Nlmsg.into(),
                value: message_type,
            },
        }),
        NetlinkFamily::Xfrm => Some(NlmsgPermissions {
            permission: match message_type as u32 {
                XFRM_MSG_GETSA | XFRM_MSG_GETPOLICY => {
                    Some(NetlinkXfrmSocketPermission::NlmsgRead.into())
                }
                XFRM_MSG_NEWSA | XFRM_MSG_DELSA | XFRM_MSG_NEWPOLICY | XFRM_MSG_DELPOLICY => {
                    Some(NetlinkXfrmSocketPermission::NlmsgWrite.into())
                }
                _ => None,
            },
            xperm: NlmsgExtendedPermission {
                permission: NetlinkXfrmSocketPermission::Nlmsg.into(),
                value: message_type,
            },
        }),

        // Other Netlink families don't have message permissions besides the common socket
        // permissions.
        _ => None,
    }
}

/// Checks if the Netlink `socket` is allowed to send a message of `message_type`.
pub(in crate::security) fn check_netlink_send_access(
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
    let Some(netlink_permissions) =
        compute_netlink_message_permissions(&netlink_family, message_type)
    else {
        // No message permissions are required for this netlink family.
        return Ok(());
    };
    let Some(permission) = netlink_permissions.permission else {
        // No message permissions are required for this message type.
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
    socket::has_socket_permission(
        &security_server.as_permission_check(),
        current_task,
        current_sid,
        &socket_node,
        permission,
        current_task.into(),
    )
    .or_else(|_| {
        let FsNodeSidAndClass { sid: socket_sid, class: _ } =
            fs_node_effective_sid_and_class(&socket_node);
        let audit_context = &[current_task.into(), socket_node.as_ref().as_ref().into()];
        check_permission_and_xperms(
            &security_server.as_permission_check(),
            current_task,
            current_sid,
            socket_sid,
            netlink_permissions.xperm.permission,
            XpermsKind::Nlmsg,
            netlink_permissions.xperm.value,
            audit_context.into(),
        )
    })
}
