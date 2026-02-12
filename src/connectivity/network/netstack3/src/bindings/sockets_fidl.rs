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
use netstack3_core::socket::IpSocketMatcher;
use netstack3_core::tcp::{TcpSocketDiagnostics, TcpSocketState};
use netstack3_core::udp::{UdpSocketDiagnosticTuple, UdpSocketDiagnostics};
use netstack3_core::{Instant as _, MatcherBindingsTypes};
use {
    fidl_fuchsia_net_sockets_ext as fnet_sockets_ext, fidl_fuchsia_net_tcp as fnet_tcp,
    fidl_fuchsia_net_udp as fnet_udp, fuchsia_async as fasync,
};

use crate::bindings::time::StackTime;
use crate::bindings::util::{
    IntoCore as _, IntoFidl as _, IntoFidlExtender, ScopeExt as _, TryFromFidl, TryIntoFidl,
};
use crate::bindings::{BindingsCtx, Ctx};
use netstack3_core::tcp::{CongestionControlState, TcpSocketInfo};

pub(crate) async fn serve_diagnostics(
    mut stream: fnet_sockets::DiagnosticsRequestStream,
    mut ctx: Ctx,
) -> Result<(), fidl::Error> {
    log::debug!("serving {}", fnet_sockets::DiagnosticsMarker::DEBUG_NAME);
    while let Some(req) = stream.try_next().await? {
        match req {
            fidl_fuchsia_net_sockets::DiagnosticsRequest::IterateIp {
                s,
                extensions,
                matchers,
                responder,
            } => match iterate_ip(&mut ctx, extensions, matchers) {
                Ok(results) => {
                    fasync::Scope::current()
                        .spawn_request_stream_handler(s.into_stream(), |requests| {
                            serve_ipiterator(requests, results)
                        });
                    responder.send(&fnet_sockets::IterateIpResult::Ok(fnet_sockets::Empty))?
                }
                Err(err) => responder.send(&fnet_sockets::IterateIpResult::InvalidMatcher(err))?,
            },
        }
    }

    Ok(())
}

fn iterate_ip(
    ctx: &mut Ctx,
    extensions: fnet_sockets::Extensions,
    matchers: Vec<fnet_sockets::IpSocketMatcher>,
) -> Result<Vec<fnet_sockets_ext::IpSocketState>, fnet_sockets::InvalidMatcher> {
    let matchers = match convert_matchers(matchers) {
        Ok(matchers) => matchers,
        Err((err, index)) => {
            log::debug!("encountered matcher error in IterateIp request: {err}");
            return Err(fnet_sockets::InvalidMatcher {
                // Unwrap is safe because the target type is a u32, and the
                // index will never be more than MAX_IP_SOCKET_MATCHERS, which
                // is a u32.
                index: index.try_into().unwrap(),
            });
        }
    };

    let tcp_info = extensions.contains(fnet_sockets::Extensions::TCP_INFO);

    // TODO(https://fxbug.dev/452064956): Track which transport
    // protocols and IP versions could be matched and scope the API
    // calls to just those.
    let mut results = IntoFidlExtender::new(Vec::new());
    ctx.api().tcp::<Ipv4>().bound_sockets_diagnostics(&matchers[..], &mut results, tcp_info);
    ctx.api().udp::<Ipv4>().bound_sockets_diagnostics(&matchers[..], &mut results);
    ctx.api().tcp::<Ipv6>().bound_sockets_diagnostics(&matchers[..], &mut results, tcp_info);
    ctx.api().udp::<Ipv6>().bound_sockets_diagnostics(&matchers[..], &mut results);

    Ok(results.into_inner())
}

async fn serve_ipiterator(
    mut stream: fnet_sockets::IpIteratorRequestStream,
    results: Vec<fnet_sockets_ext::IpSocketState>,
) -> Result<(), fidl::Error> {
    let results: Vec<_> = results.into_iter().map(|s| s.into()).collect();
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

pub(crate) async fn serve_control(
    mut stream: fnet_sockets::ControlRequestStream,
    mut ctx: Ctx,
) -> Result<(), fidl::Error> {
    log::debug!("serving {}", fnet_sockets::ControlMarker::DEBUG_NAME);
    while let Some(req) = stream.try_next().await? {
        match req {
            fnet_sockets::ControlRequest::DisconnectIp { payload, responder } => {
                let res = disconnect_ip(&mut ctx, payload);
                responder.send(&res)?;
            }
        }
    }
    Ok(())
}

fn disconnect_ip(
    ctx: &mut Ctx,
    payload: fnet_sockets::ControlDisconnectIpRequest,
) -> fnet_sockets::DisconnectIpResult {
    let matchers = payload.matchers.unwrap_or_default();
    if matchers.is_empty() {
        return fnet_sockets::DisconnectIpResult::UnconstrainedMatchers(fnet_sockets::Empty);
    }

    let matchers = match convert_matchers(matchers) {
        Ok(matchers) => matchers,
        Err((err, index)) => {
            log::debug!("encountered matcher error in DisconnectIp request: {err}");
            return fnet_sockets::DisconnectIpResult::InvalidMatcher(
                fnet_sockets::InvalidMatcher {
                    // Unwrap is safe because the target type is a u32, and the
                    // index will never be more than MAX_IP_SOCKET_MATCHERS, which
                    // is a u32.
                    index: index.try_into().unwrap(),
                },
            );
        }
    };

    // TODO(https://fxbug.dev/452064956): Track which transport
    // protocols and IP versions could be matched and scope the API
    // calls to just those.
    let mut count: usize = 0;
    count += ctx.api().tcp::<Ipv4>().disconnect_bound(&matchers[..]);
    count += ctx.api().udp::<Ipv4>().disconnect_bound(&matchers[..]);
    count += ctx.api().tcp::<Ipv6>().disconnect_bound(&matchers[..]);
    count += ctx.api().udp::<Ipv6>().disconnect_bound(&matchers[..]);

    fnet_sockets::DisconnectIpResult::Ok(fnet_sockets::DisconnectIpResponse {
        disconnected: count.try_into().unwrap_or(u32::MAX),
    })
}

fn convert_matchers(
    matchers: Vec<fnet_sockets::IpSocketMatcher>,
) -> Result<
    Vec<IpSocketMatcher<<BindingsCtx as MatcherBindingsTypes>::DeviceClass>>,
    (IpSocketMatcherError, usize),
> {
    matchers
        .into_iter()
        .enumerate()
        .map(|(i, matcher)| match fnet_sockets_ext::IpSocketMatcher::try_from(matcher) {
            Ok(matcher) => Ok(matcher.into_core()),
            Err(err) => Err((err, i)),
        })
        .collect()
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

impl<I: Ip> TryIntoFidl<fnet_sockets_ext::IpSocketState> for UdpSocketDiagnostics<I> {
    type Error = Never;

    fn try_into_fidl(self) -> Result<fnet_sockets_ext::IpSocketState, Self::Error> {
        let UdpSocketDiagnostics { state, cookie, marks } = self;

        let state_specific = fnet_sockets_ext::IpSocketStateSpecific {
            src_addr: state.src_addr(),
            dst_addr: state.dst_addr(),
            cookie: cookie.export_value(),
            marks: marks.into_fidl(),
            transport: fnet_sockets_ext::IpSocketTransportState::Udp(
                fnet_sockets_ext::IpSocketUdpState {
                    src_port: state.src_port().map(|p| p.get()),
                    dst_port: state.dst_port(),
                    state: match state {
                        UdpSocketDiagnosticTuple::Bound { .. } => fnet_udp::State::Bound,
                        UdpSocketDiagnosticTuple::Connected { .. } => fnet_udp::State::Connected,
                    },
                },
            ),
        };

        Ok(I::map_ip_in(
            state_specific,
            |state| fnet_sockets_ext::IpSocketState::V4(state),
            |state| fnet_sockets_ext::IpSocketState::V6(state),
        ))
    }
}

impl TryIntoFidl<fnet_tcp::State> for TcpSocketState {
    type Error = Never;

    fn try_into_fidl(self) -> Result<fnet_tcp::State, Self::Error> {
        Ok(match self {
            TcpSocketState::Established => fnet_tcp::State::Established,
            TcpSocketState::SynSent => fnet_tcp::State::SynSent,
            TcpSocketState::SynRecv => fnet_tcp::State::SynRecv,
            TcpSocketState::FinWait1 => fnet_tcp::State::FinWait1,
            TcpSocketState::FinWait2 => fnet_tcp::State::FinWait2,
            TcpSocketState::TimeWait => fnet_tcp::State::TimeWait,
            TcpSocketState::CloseWait => fnet_tcp::State::CloseWait,
            TcpSocketState::LastAck => fnet_tcp::State::LastAck,
            TcpSocketState::Closing => fnet_tcp::State::Closing,
            TcpSocketState::Listen => fnet_tcp::State::Listen,
            TcpSocketState::Close => fnet_tcp::State::Close,
        })
    }
}

impl TryIntoFidl<fnet_tcp::CongestionControlState> for CongestionControlState {
    type Error = Never;

    fn try_into_fidl(self) -> Result<fnet_tcp::CongestionControlState, Self::Error> {
        Ok(match self {
            CongestionControlState::Open => fnet_tcp::CongestionControlState::Open,
            CongestionControlState::Disorder => fnet_tcp::CongestionControlState::Disorder,
            CongestionControlState::CongestionWindowReduced => {
                fnet_tcp::CongestionControlState::CongestionWindowReduced
            }
            CongestionControlState::Recovery => fnet_tcp::CongestionControlState::Recovery,
            CongestionControlState::Loss => fnet_tcp::CongestionControlState::Loss,
        })
    }
}

impl TryIntoFidl<fnet_sockets_ext::TcpInfo> for TcpSocketInfo<StackTime> {
    type Error = Never;
    fn try_into_fidl(self) -> Result<fnet_sockets_ext::TcpInfo, Self::Error> {
        let TcpSocketInfo {
            state,
            ca_state,
            rto,
            rtt,
            rtt_var,
            snd_ssthresh,
            snd_cwnd,
            retransmits,
            last_ack_recv,
            segs_out,
            segs_in,
            last_data_sent,
        } = self;

        let now = StackTime::now();

        Ok(fnet_sockets_ext::TcpInfo {
            state: state.into_fidl(),
            ca_state: ca_state.into_fidl(),
            rto_usec: rto.map(|d| d.as_micros().try_into().unwrap_or(u32::MAX)),
            rtt_usec: rtt.map(|d| d.as_micros().try_into().unwrap_or(u32::MAX)),
            rtt_var_usec: rtt_var.map(|d| d.as_micros().try_into().unwrap_or(u32::MAX)),
            snd_ssthresh: snd_ssthresh,
            snd_cwnd: snd_cwnd,
            tcpi_total_retrans: retransmits.try_into().unwrap_or(u32::MAX),
            tcpi_last_ack_recv_msec: last_ack_recv.and_then(|i| {
                now.checked_duration_since(i).map(|d| d.as_millis().try_into().unwrap_or(u32::MAX))
            }),
            tcpi_segs_out: segs_out,
            tcpi_segs_in: segs_in,
            // TODO(https://fxbug.dev/404910001): Netstack2 only reports
            // reordering when using RACK, which Netstack3 doesn't support.
            reorder_seen: false,
            tcpi_last_data_sent_msec: last_data_sent.and_then(|i| {
                now.checked_duration_since(i).map(|d| d.as_millis().try_into().unwrap_or(u32::MAX))
            }),
        })
    }
}

impl<I: Ip> TryIntoFidl<fnet_sockets_ext::IpSocketState> for TcpSocketDiagnostics<I, StackTime> {
    type Error = Never;

    fn try_into_fidl(self) -> Result<fnet_sockets_ext::IpSocketState, Self::Error> {
        let TcpSocketDiagnostics { tuple, state_machine, cookie, marks, tcp_info } = self;

        let state_specific = fnet_sockets_ext::IpSocketStateSpecific {
            src_addr: tuple.src_addr(),
            dst_addr: tuple.dst_addr(),
            cookie: cookie.export_value(),
            marks: marks.into_fidl(),
            transport: fnet_sockets_ext::IpSocketTransportState::Tcp(
                fnet_sockets_ext::IpSocketTcpState {
                    src_port: tuple.src_port().map(|p| p.get()),
                    dst_port: tuple.dst_port().map(|p| p.get()),
                    state: state_machine.into_fidl(),
                    tcp_info: tcp_info.map(|i| i.into_fidl()),
                },
            ),
        };

        Ok(I::map_ip_in(
            state_specific,
            |state| fnet_sockets_ext::IpSocketState::V4(state),
            |state| fnet_sockets_ext::IpSocketState::V6(state),
        ))
    }
}
