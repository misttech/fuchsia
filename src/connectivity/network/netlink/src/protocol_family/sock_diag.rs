// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A module for handling the `NETLINK_SOCK_DIAG` API.

mod eventloop;
mod request;

pub(crate) use eventloop::SockDiagEventLoop;
pub(crate) use request::NetlinkSockDiagRequestHandler;

use std::convert::Infallible as Never;
use std::num::NonZeroU32;

use linux_uapi::{
    sknetlink_groups_SKNLGRP_INET_TCP_DESTROY, sknetlink_groups_SKNLGRP_INET_UDP_DESTROY,
    sknetlink_groups_SKNLGRP_INET6_TCP_DESTROY, sknetlink_groups_SKNLGRP_INET6_UDP_DESTROY,
    sknetlink_groups_SKNLGRP_NONE,
};
use netlink_packet_sock_diag::{SockDiagRequest, SockDiagResponse};

use crate::client::{AsyncWorkCompletionWaiter, ExternalClient};
use crate::messaging::{MessageWithPermission, Sender};
use crate::multicast_groups::{
    GroupSupport, InvalidLegacyGroupsError, InvalidModernGroupError, LegacyGroups, ModernGroup,
    MulticastCapableNetlinkFamily,
};
use crate::protocol_family::{NamedNetlinkFamily, NetlinkClient, ProtocolFamily};

/// An implementation of the `NETLINK_SOCK_DIAG` protocol family.
pub(crate) struct NetlinkSockDiag;

impl MulticastCapableNetlinkFamily for NetlinkSockDiag {
    #[allow(non_upper_case_globals)]
    fn check_support(
        ModernGroup(group): &ModernGroup,
    ) -> Result<GroupSupport, InvalidModernGroupError> {
        match *group {
            sknetlink_groups_SKNLGRP_INET_TCP_DESTROY
            | sknetlink_groups_SKNLGRP_INET_UDP_DESTROY
            | sknetlink_groups_SKNLGRP_INET6_TCP_DESTROY
            | sknetlink_groups_SKNLGRP_INET6_UDP_DESTROY
            | sknetlink_groups_SKNLGRP_NONE => Ok(GroupSupport::Unsupported),
            _ => Err(InvalidModernGroupError),
        }
    }
}

#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
// TODO(https://fxbug.dev/470079735): Support multicast socket closure
// notifications.
#[expect(dead_code)]
pub(crate) enum NetlinkSockDiagNotifiedGroup {
    TcpV4Destroy,
    TcpV6Destroy,
    UdpV4Destroy,
    UdpV6Destroy,
}

impl MessageWithPermission for SockDiagRequest {
    fn permission(&self) -> crate::messaging::Permission {
        match self {
            SockDiagRequest::InetRequest(_) | SockDiagRequest::UnixRequest(_) => {
                crate::messaging::Permission::NetlinkSockDiagRead
            }
            SockDiagRequest::InetSockDestroy(_) => {
                crate::messaging::Permission::NetlinkSockDiagDestroy
            }
        }
    }
}

/// A connection to the `NETLINK_SOCK_DIAG` protocol family.
pub struct NetlinkSockDiagClient(pub(crate) ExternalClient<NetlinkSockDiag>);

impl NetlinkClient for NetlinkSockDiagClient {
    type Request = SockDiagRequest;

    fn set_pid(&self, pid: NonZeroU32) {
        let NetlinkSockDiagClient(client) = self;
        client.set_port_number(pid)
    }

    fn add_membership(
        &self,
        group: ModernGroup,
    ) -> Result<AsyncWorkCompletionWaiter, InvalidModernGroupError> {
        let NetlinkSockDiagClient(client) = self;
        client.add_membership(group)
    }

    fn del_membership(&self, group: ModernGroup) -> Result<(), InvalidModernGroupError> {
        let NetlinkSockDiagClient(client) = self;
        client.del_membership(group)
    }

    fn set_legacy_memberships(
        &self,
        legacy_memberships: LegacyGroups,
    ) -> Result<AsyncWorkCompletionWaiter, InvalidLegacyGroupsError> {
        let NetlinkSockDiagClient(client) = self;
        client.set_legacy_memberships(legacy_memberships)
    }
}

impl NamedNetlinkFamily for NetlinkSockDiag {
    const NAME: &'static str = "NETLINK_SOCK_DIAG";
}

impl ProtocolFamily for NetlinkSockDiag {
    type Request = SockDiagRequest;
    type Response = SockDiagResponse;
    type RequestHandler<S: Sender<Self::Response>> = NetlinkSockDiagRequestHandler<S>;
    type NotifiedMulticastGroup = NetlinkSockDiagNotifiedGroup;
    type AsyncWorkItem = Never;

    fn should_notify_on_group_membership_change(
        _group: ModernGroup,
    ) -> Option<Self::NotifiedMulticastGroup> {
        // TODO(https://fxbug.dev/470079735): All membership changes need to
        // be notified so the system can avoid generating socket destruction
        // messages when nobody is listening.
        None
    }
}

#[cfg(test)]
mod testutil {
    use net_declare::{std_ip_v4, std_ip_v6};
    use net_types::ip::{Ip, Ipv4, Ipv4Addr, Ipv6, Ipv6Addr};

    pub(crate) trait TestIpExt: Ip {
        const SRC_ADDR: Self::Addr;
        const DST_ADDR: Self::Addr;
        const LINUX_FAMILY: u8;
    }

    impl TestIpExt for Ipv4 {
        const SRC_ADDR: Ipv4Addr = Ipv4Addr::new(std_ip_v4!("192.168.0.1").octets());
        const DST_ADDR: Ipv4Addr = Ipv4Addr::new(std_ip_v4!("192.168.0.2").octets());
        const LINUX_FAMILY: u8 = linux_uapi::AF_INET as u8;
    }

    impl TestIpExt for Ipv6 {
        const SRC_ADDR: Ipv6Addr = Ipv6Addr::new(std_ip_v6!("2001:db8::1").segments());
        const DST_ADDR: Ipv6Addr = Ipv6Addr::new(std_ip_v6!("2001:db8::2").segments());
        const LINUX_FAMILY: u8 = linux_uapi::AF_INET6 as u8;
    }
}
