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
    InvalidLegacyGroupsError, InvalidModernGroupError, LegacyGroups, ModernGroup,
    MulticastCapableNetlinkFamily,
};
use crate::protocol_family::{NetlinkClient, ProtocolFamily};

/// An implementation of the `NETLINK_SOCK_DIAG` protocol family.
pub(crate) struct NetlinkSockDiag;

impl MulticastCapableNetlinkFamily for NetlinkSockDiag {
    #[allow(non_upper_case_globals)]
    fn is_valid_group(ModernGroup(group): &ModernGroup) -> bool {
        match *group {
            sknetlink_groups_SKNLGRP_INET_TCP_DESTROY
            | sknetlink_groups_SKNLGRP_INET_UDP_DESTROY
            | sknetlink_groups_SKNLGRP_INET6_TCP_DESTROY
            | sknetlink_groups_SKNLGRP_INET6_UDP_DESTROY
            | sknetlink_groups_SKNLGRP_NONE => true,
            _ => false,
        }
    }
}

#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
// TODO(https://fxbug.dev/470079735): Support multicast socket closure
// notifications.
#[allow(dead_code)]
pub(crate) enum NetlinkSockDiagNotifiedGroup {
    TcpV4Destroy,
    TcpV6Destroy,
    UdpV4Destroy,
    UdpV6Destroy,
}

impl MessageWithPermission for SockDiagRequest {
    fn permission(&self) -> crate::messaging::Permission {
        // TODO(323590076): Implement SOCK_DIAG requests.
        todo!()
    }
}

/// A connection to the `NETLINK_SOCK_DIAG` protocol family.
pub struct NetlinkSockDiagClient(pub(crate) ExternalClient<NetlinkSockDiag>);

impl NetlinkSockDiagClient {
    /// Sets the PID assigned to the client.
    pub fn set_pid(&self, pid: NonZeroU32) {
        let NetlinkSockDiagClient(client) = self;
        client.set_port_number(pid)
    }

    /// Adds the given multicast group membership.
    pub fn add_membership(
        &self,
        group: ModernGroup,
    ) -> Result<AsyncWorkCompletionWaiter, InvalidModernGroupError> {
        let NetlinkSockDiagClient(client) = self;
        client.add_membership(group)
    }

    /// Deletes the given multicast group membership.
    pub fn del_membership(&self, group: ModernGroup) -> Result<(), InvalidModernGroupError> {
        let NetlinkSockDiagClient(client) = self;
        client.del_membership(group)
    }

    /// Sets the legacy multicast group memberships.
    pub fn set_legacy_memberships(
        &self,
        legacy_memberships: LegacyGroups,
    ) -> Result<AsyncWorkCompletionWaiter, InvalidLegacyGroupsError> {
        let NetlinkSockDiagClient(client) = self;
        client.set_legacy_memberships(legacy_memberships)
    }
}

impl NetlinkClient for NetlinkSockDiagClient {
    fn set_pid(&self, pid: NonZeroU32) {
        self.set_pid(pid)
    }

    fn set_legacy_memberships(
        &self,
        legacy_memberships: LegacyGroups,
    ) -> Result<AsyncWorkCompletionWaiter, InvalidLegacyGroupsError> {
        self.set_legacy_memberships(legacy_memberships)
    }
}

impl ProtocolFamily for NetlinkSockDiag {
    type Request = SockDiagRequest;
    type Response = SockDiagResponse;
    type RequestHandler<S: Sender<Self::Response>> = NetlinkSockDiagRequestHandler<S>;
    const NAME: &'static str = "NETLINK_SOCK_DIAG";
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
