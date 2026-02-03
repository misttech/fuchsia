// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::NetlinkSockDiag;

use std::convert::Infallible as Never;

use derivative::Derivative;
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use {fidl_fuchsia_net_sockets as fnet_sockets, fidl_fuchsia_net_sockets_ext as fnet_sockets_ext};

use crate::client::{AsyncWorkItem, InternalClient};
use crate::messaging::Sender;
use crate::netlink_packet::errno::Errno;
use crate::protocol_family::ProtocolFamily;

/// The argument(s) for a [`Request`].
#[derive(Clone, Debug, PartialEq)]
// TODO(https://fxbug.dev/323590076): Remove allowance once used.
#[allow(dead_code)]
pub(crate) enum RequestArgs {
    Get(Vec<fnet_sockets_ext::IpSocketMatcher>, fnet_sockets::Extensions, bool),
    Destroy(Vec<fnet_sockets_ext::IpSocketMatcher>),
}

/// An error encountered while handling a [`Request`].
#[derive(Clone, Debug, PartialEq, Eq)]
// TODO(https://fxbug.dev/323590076): Remove allowance once used.
#[allow(dead_code)]
pub(crate) enum RequestError {
    NotFound,
    InvalidRequest,
    Unsupported,
}

impl RequestError {
    // TODO(https://fxbug.dev/323590076): Remove allowance once used.
    #[allow(dead_code)]
    pub(crate) fn into_errno(self) -> Errno {
        match self {
            RequestError::NotFound => Errno::ENOENT,
            RequestError::InvalidRequest => Errno::EINVAL,
            RequestError::Unsupported => Errno::ENOTSUP,
        }
    }
}

/// A `NETLINK_SOCK_DIAG` request.
#[derive(Derivative)]
#[derivative(Debug(bound = ""))]
pub(crate) struct Request<S: Sender<<NetlinkSockDiag as ProtocolFamily>::Response>> {
    /// The operation-specific arguments for this request.
    pub args: RequestArgs,
    /// The request's sequence number.
    ///
    /// This value will be copied verbatim into any message sent as a result of
    /// this request.
    pub sequence_number: u32,
    /// The client that made the request.
    pub client: InternalClient<NetlinkSockDiag, S>,
    /// A completer that will have the result of the request sent over.
    pub completer: oneshot::Sender<Result<(), RequestError>>,
}

pub(crate) struct SockDiagEventLoop<
    S: crate::messaging::Sender<<NetlinkSockDiag as ProtocolFamily>::Response>,
> {
    // TODO(https://fxbug.dev/323590076): Remove allowance once used.
    #[allow(dead_code)]
    pub(crate) socket_diagnostics: fnet_sockets::DiagnosticsProxy,
    // TODO(https://fxbug.dev/323590076): Remove allowance once used.
    #[allow(dead_code)]
    pub(crate) socket_control: fnet_sockets::ControlProxy,
    pub(crate) request_stream: mpsc::Receiver<Request<S>>,
    // TODO(https://fxbug.dev/470079735): Support multicast socket destruction
    // notifications.
    #[allow(dead_code)]
    pub(crate) async_work_receiver: mpsc::UnboundedReceiver<AsyncWorkItem<NetlinkSockDiag>>,
}

impl<S: crate::messaging::Sender<<NetlinkSockDiag as ProtocolFamily>::Response>>
    SockDiagEventLoop<S>
{
    pub(crate) async fn run(mut self) -> Never {
        loop {
            self.run_one_step().await;
        }
    }

    async fn run_one_step(&mut self) {
        let Self {
            socket_diagnostics: _,
            socket_control: _,
            request_stream,
            async_work_receiver: _,
        } = self;

        let request = request_stream.next().await.expect("request stream cannot end");

        // TODO(https://fxbug.dev/433948037): Support `Get` requests.
        // TODO(https://fxbug.dev/433947762): Support `Destroy` requests.
        request
            .completer
            .send(Err(RequestError::Unsupported))
            .expect("receiving end of completer should not be dropped");
    }
}
