// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Provides implementation for handling Netlink requests and transforming them
//! into requests for [`eventloop::SockDiagEventLoop`].

use async_trait::async_trait;
use futures::channel::mpsc;
use netlink_packet_core::NetlinkMessage;
use netlink_packet_sock_diag::{SockDiagRequest, SockDiagResponse};

use crate::client::InternalClient;
use crate::messaging::Sender;

use crate::netlink_packet;
use crate::netlink_packet::errno::Errno;
use crate::protocol_family::NetlinkFamilyRequestHandler;
use crate::protocol_family::sock_diag::{NetlinkSockDiag, eventloop};

#[derive(Clone)]
pub(crate) struct NetlinkSockDiagRequestHandler<S: Sender<SockDiagResponse>> {
    // TODO(https://fxbug.dev/323590076): Remove allowance once used.
    #[allow(dead_code)]
    pub(crate) sock_diag_request_sink: mpsc::Sender<eventloop::Request<S>>,
}

#[async_trait]
impl<S: Sender<SockDiagResponse>> NetlinkFamilyRequestHandler<NetlinkSockDiag, S>
    for NetlinkSockDiagRequestHandler<S>
{
    async fn handle_request(
        &mut self,
        req: NetlinkMessage<SockDiagRequest>,
        client: &mut InternalClient<NetlinkSockDiag, S>,
    ) {
        let (req_header, _payload) = req.into_parts();
        client.send_unicast(netlink_packet::new_error(Err(Errno::ENOTSUP), req_header));
    }
}
