// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! FIDL Worker for the `fuchsia.net.sockets` API.

use std::convert::Infallible as Never;

use fidl::endpoints::ProtocolMarker;
use fidl_fuchsia_net_sockets::{self as fnet_sockets};
use fidl_fuchsia_net_sockets_ext::IpSocketMatcherError;
use futures::TryStreamExt;
use net_types::ip::{Ip, Ipv4, Ipv6};
use netstack3_core::MatcherBindingsTypes;
use netstack3_core::socket::IpSocketMatcher;
use netstack3_core::tcp::TcpSocketDiagnostics;
use netstack3_core::udp::UdpSocketDiagnostics;
use {fidl_fuchsia_net_sockets_ext as fnet_sockets_ext, fuchsia_async as fasync};

use crate::bindings::util::{
    IntoCore as _, IntoFidlExtender, ScopeExt as _, TryFromFidl, TryIntoFidl,
};
use crate::bindings::{BindingsCtx, Ctx};

pub(crate) async fn serve_diagnostics(
    mut stream: fnet_sockets::DiagnosticsRequestStream,
    mut ctx: Ctx,
) -> Result<(), fidl::Error> {
    log::debug!("serving {}", fnet_sockets::DiagnosticsMarker::DEBUG_NAME);
    while let Some(req) = stream.try_next().await? {
        match req {
            fidl_fuchsia_net_sockets::DiagnosticsRequest::IterateIp {
                s,
                // TODO(https://fxbug.dev/449158649): Add support for the
                // TCP_INFO extension.
                extensions: _,
                matchers,
                responder,
            } => match iterate_ip(&mut ctx, matchers) {
                Ok(results) => {
                    fasync::Scope::current()
                        .spawn_request_stream_handler(s.into_stream(), |requests| {
                            serve_ipiterator(requests, results)
                        });
                    responder.send(&fnet_sockets::IterateIpResult::Ok(fnet_sockets::Empty))?
                }
                Err(err) => responder.send(&fnet_sockets::IterateIpResult::MatcherError(err))?,
            },
        }
    }

    Ok(())
}

fn iterate_ip(
    ctx: &mut Ctx,
    matchers: Vec<fnet_sockets::IpSocketMatcher>,
) -> Result<Vec<fnet_sockets::IpSocketState>, fnet_sockets::IterateIpMatcherError> {
    let matchers = matchers
        .into_iter()
        .enumerate()
        .map(|(i, matcher)| match fnet_sockets_ext::IpSocketMatcher::try_from(matcher) {
            Ok(matcher) => Ok(matcher.into_core()),
            Err(err) => Err((err, i)),
        })
        .collect::<Result<Vec<_>, (IpSocketMatcherError, usize)>>();

    let matchers = match matchers {
        Ok(matchers) => matchers,
        Err((err, index)) => {
            log::debug!("encountered matcher error in IterateIp request: {err}");
            return Err(fnet_sockets::IterateIpMatcherError {
                // Unwrap is safe because the target type is a u32, and the
                // index will never be more than MAX_IP_SOCKET_MATCHERS, which
                // is a u32.
                index: Some(index.try_into().unwrap()),
                __source_breaking: fidl::marker::SourceBreaking,
            });
        }
    };

    // TODO(https://fxbug.dev/452064956): Track which transport
    // protocols and IP versions could be matched and scope the API
    // calls to just those.
    let mut results = IntoFidlExtender::new(Vec::new());
    ctx.api().tcp::<Ipv4>().bound_sockets_diagnostics(&matchers[..], &mut results);
    ctx.api().udp::<Ipv4>().bound_sockets_diagnostics(&matchers[..], &mut results);
    ctx.api().tcp::<Ipv6>().bound_sockets_diagnostics(&matchers[..], &mut results);
    ctx.api().udp::<Ipv6>().bound_sockets_diagnostics(&matchers[..], &mut results);

    Ok(results.into_inner())
}

async fn serve_ipiterator(
    mut stream: fnet_sockets::IpIteratorRequestStream,
    results: Vec<fnet_sockets::IpSocketState>,
) -> Result<(), fidl::Error> {
    let mut iter = results.chunks(fnet_sockets::MAX_IP_SOCKET_BATCH_SIZE as usize).peekable();

    // TODO(https://fxbug.dev/452354359): Close the connection if the reader
    // hasn't asked for anything recently.
    while let Some(req) = stream.try_next().await? {
        match req {
            fidl_fuchsia_net_sockets::IpIteratorRequest::Next { responder } => {
                let to_send = iter.next().unwrap_or_default();
                let has_more = iter.peek().is_some();
                responder.send(to_send, has_more)?;

                if !has_more {
                    break;
                }
            }

            fidl_fuchsia_net_sockets::IpIteratorRequest::_UnknownMethod { ordinal, .. } => {
                log::warn!("Received unknown ordinal {ordinal} on IpIterator");
            }
        }
    }

    Ok(())
}

impl TryFromFidl<fnet_sockets_ext::IpSocketMatcher>
    for IpSocketMatcher<<BindingsCtx as MatcherBindingsTypes>::DeviceClass>
{
    type Error = Never;

    fn try_from_fidl(fidl: fnet_sockets_ext::IpSocketMatcher) -> Result<Self, Self::Error> {
        match fidl {
            fnet_sockets_ext::IpSocketMatcher::Family(ip_version) => {
                Ok(IpSocketMatcher::Family(ip_version))
            }
            fnet_sockets_ext::IpSocketMatcher::SrcAddr(address) => {
                Ok(IpSocketMatcher::SrcAddr(address.into_core()))
            }
            fnet_sockets_ext::IpSocketMatcher::DstAddr(address) => {
                Ok(IpSocketMatcher::DstAddr(address.into_core()))
            }
            fnet_sockets_ext::IpSocketMatcher::Proto(socket_transport_protocol) => {
                Ok(IpSocketMatcher::Proto(socket_transport_protocol.into_core()))
            }
            fnet_sockets_ext::IpSocketMatcher::BoundInterface(bound_interface) => {
                Ok(IpSocketMatcher::BoundInterface(bound_interface.into_core()))
            }
            fnet_sockets_ext::IpSocketMatcher::Cookie(socket_cookie) => {
                Ok(IpSocketMatcher::Cookie(socket_cookie.into_core()))
            }
            fnet_sockets_ext::IpSocketMatcher::Mark(mark) => {
                Ok(IpSocketMatcher::Mark(mark.into_core()))
            }
        }
    }
}

impl<I: Ip> TryIntoFidl<fnet_sockets::IpSocketState> for UdpSocketDiagnostics<I> {
    type Error = Never;

    fn try_into_fidl(self) -> Result<fnet_sockets::IpSocketState, Self::Error> {
        todo!("TODO(https://fxbug.dev/449158183): Implement socket diagnostics for UDP.")
    }
}

impl<I: Ip> TryIntoFidl<fnet_sockets::IpSocketState> for TcpSocketDiagnostics<I> {
    type Error = Never;

    fn try_into_fidl(self) -> Result<fnet_sockets::IpSocketState, Self::Error> {
        todo!("TODO(https://fxbug.dev/449157844): Implement socket diagnostics for TCP.");
    }
}
