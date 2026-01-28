// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Extensions for the fuchsia.sockets FIDL library.

#![warn(
    missing_docs,
    unreachable_patterns,
    clippy::useless_conversion,
    clippy::redundant_clone,
    clippy::precedence
)]

use fidl_fuchsia_net_ext::IntoExt;
use futures::{Stream, TryStreamExt as _};
use net_types::ip;
use thiserror::Error;
use {
    fidl_fuchsia_net_matchers as fnet_matchers, fidl_fuchsia_net_matchers_ext as fnet_matchers_ext,
    fidl_fuchsia_net_sockets as fnet_sockets,
};

/// An extension type for [`fnet_sockets::IpSocketMatcher`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpSocketMatcher {
    /// Matches against the IP version of the socket.
    Family(ip::IpVersion),
    /// Matches against the source address of the socket.
    SrcAddr(fnet_matchers_ext::BoundAddress),
    /// Matches against the destination address of the socket.
    DstAddr(fnet_matchers_ext::BoundAddress),
    /// Matches against transport protocol fields of the socket.
    Proto(fnet_matchers_ext::SocketTransportProtocol),
    /// Matches against the (bound, i.e. SO_BINDTODEVICE) interface of the
    /// socket.
    BoundInterface(fnet_matchers_ext::BoundInterface),
    /// Matches against the cookie of the socket (i.e. SO_COOKIE)
    Cookie(fnet_matchers::SocketCookie),
    /// Matches against one mark of the socket.
    Mark(fnet_matchers_ext::MarkInDomain),
}

/// Errors returned by the conversion from [`fnet_sockets::IpSocketMatcher`]
/// to [`IpSocketMatcher`].
#[derive(Debug, PartialEq, Error)]
pub enum IpSocketMatcherError {
    /// A union type was unknown.
    #[error("got unexpected union variant: {0}")]
    UnknownUnionVariant(u64),
    /// An error was encountered when converting one of the address matchers.
    #[error("address matcher conversion failure: {0}")]
    Address(fnet_matchers_ext::BoundAddressError),
    /// An error was encountered when converting the transport protocol
    /// matcher.
    #[error("protocol matcher conversion failure: {0}")]
    TransportProtocol(fnet_matchers_ext::SocketTransportProtocolError),
    /// An error was encountered while converting the interface matcher.
    #[error("bound interface matcher conversion failure: {0}")]
    BoundInterface(fnet_matchers_ext::BoundInterfaceError),
    /// An error was encountered when converting one of the mark matchers.
    #[error("mark matcher conversion failure: {0}")]
    Mark(fnet_matchers_ext::MarkInDomainError),
}

impl TryFrom<fnet_sockets::IpSocketMatcher> for IpSocketMatcher {
    type Error = IpSocketMatcherError;

    fn try_from(matcher: fnet_sockets::IpSocketMatcher) -> Result<Self, Self::Error> {
        match matcher {
            fnet_sockets::IpSocketMatcher::Family(ip_version) => {
                Ok(Self::Family(ip_version.into_ext()))
            }
            fnet_sockets::IpSocketMatcher::SrcAddr(addr) => {
                Ok(Self::SrcAddr(addr.try_into().map_err(|e| IpSocketMatcherError::Address(e))?))
            }
            fnet_sockets::IpSocketMatcher::DstAddr(addr) => {
                Ok(Self::DstAddr(addr.try_into().map_err(|e| IpSocketMatcherError::Address(e))?))
            }
            fnet_sockets::IpSocketMatcher::Proto(proto) => Ok(Self::Proto(
                proto.try_into().map_err(|e| IpSocketMatcherError::TransportProtocol(e))?,
            )),
            fnet_sockets::IpSocketMatcher::BoundInterface(bound_interface) => {
                Ok(Self::BoundInterface(
                    bound_interface
                        .try_into()
                        .map_err(|e| IpSocketMatcherError::BoundInterface(e))?,
                ))
            }
            fnet_sockets::IpSocketMatcher::Cookie(cookie) => Ok(Self::Cookie(cookie)),
            fnet_sockets::IpSocketMatcher::Mark(mark) => {
                Ok(Self::Mark(mark.try_into().map_err(|e| IpSocketMatcherError::Mark(e))?))
            }
            fnet_sockets::IpSocketMatcher::__SourceBreaking { unknown_ordinal } => {
                Err(IpSocketMatcherError::UnknownUnionVariant(unknown_ordinal))
            }
        }
    }
}

impl From<IpSocketMatcher> for fnet_sockets::IpSocketMatcher {
    fn from(value: IpSocketMatcher) -> Self {
        match value {
            IpSocketMatcher::Family(ip_version) => {
                fnet_sockets::IpSocketMatcher::Family(ip_version.into_ext())
            }
            IpSocketMatcher::SrcAddr(address) => {
                fnet_sockets::IpSocketMatcher::SrcAddr(address.into())
            }
            IpSocketMatcher::DstAddr(address) => {
                fnet_sockets::IpSocketMatcher::DstAddr(address.into())
            }
            IpSocketMatcher::Proto(socket_transport_protocol) => {
                fnet_sockets::IpSocketMatcher::Proto(socket_transport_protocol.into())
            }
            IpSocketMatcher::BoundInterface(mark) => {
                fnet_sockets::IpSocketMatcher::BoundInterface(mark.into())
            }
            IpSocketMatcher::Cookie(socket_cookie) => {
                fnet_sockets::IpSocketMatcher::Cookie(socket_cookie)
            }
            IpSocketMatcher::Mark(mark) => fnet_sockets::IpSocketMatcher::Mark(mark.into()),
        }
    }
}

/// Errors returned by [`iterate_ip`]
#[derive(Debug, Error)]
pub enum IterateIpError {
    /// The specified matcher was the first invalid one.
    #[error("invalid matcher at position {0}")]
    InvalidMatcher(usize),
    /// An unknown response was received on the call to `Diagnostics.IterateIp`
    #[error("unknown ordinal on Diagnostics.IterateIp call: {0}")]
    UnknownOrdinal(u64),
    /// A low-level FIDL error was encountered on the call to
    /// `Diagnostics.IterateIp`.
    #[error("fidl error during Diagnostics.IterateIp call: {0}")]
    Fidl(fidl::Error),
}

impl From<fidl::Error> for IterateIpError {
    fn from(e: fidl::Error) -> Self {
        IterateIpError::Fidl(e)
    }
}

/// Errors returned by the stream returned from [`iterate_ip`].
#[derive(Debug, Error)]
pub enum IpIteratorError {
    /// The netstack returned an empty batch of sockets
    #[error("received empty batch of sockets")]
    EmptyBatch,
    /// A low-level FIDL error was encountered on the call to
    /// `Diagnostics.IterateIp`.
    #[error("fidl error during Diagnostics.IterateIp call: {0}")]
    Fidl(fidl::Error),
}

impl From<fidl::Error> for IpIteratorError {
    fn from(e: fidl::Error) -> Self {
        IpIteratorError::Fidl(e)
    }
}

/// Send a request to `Diagnostics.IterateIp` and drive the resulting
/// `IpIterator`.
///
/// `IpIterator` returns a series of batches of sockets matching the query, the
/// returned stream flattens those batches into individual sockets. If an error
/// is encuontered during iteration, it is returned and iteration halts.
pub async fn iterate_ip<M, I>(
    diagnostics: &fnet_sockets::DiagnosticsProxy,
    extensions: fnet_sockets::Extensions,
    matchers: M,
) -> Result<impl Stream<Item = Result<fnet_sockets::IpSocketState, IpIteratorError>>, IterateIpError>
where
    M: IntoIterator<Item = I>,
    I: Into<fnet_sockets::IpSocketMatcher>,
{
    let (proxy, server_end) = fidl::endpoints::create_proxy::<fnet_sockets::IpIteratorMarker>();
    match diagnostics
        .iterate_ip(
            server_end,
            extensions,
            &matchers.into_iter().map(Into::into).collect::<Vec<_>>()[..],
        )
        .await?
    {
        fnet_sockets::IterateIpResult::Ok(_empty) => Ok(()),
        fnet_sockets::IterateIpResult::InvalidMatcher(fnet_sockets::InvalidMatcher { index }) => {
            Err(IterateIpError::InvalidMatcher(index as usize))
        }
        fnet_sockets::IterateIpResult::__SourceBreaking { unknown_ordinal } => {
            Err(IterateIpError::UnknownOrdinal(unknown_ordinal))
        }
    }?;

    Ok(futures::stream::try_unfold((proxy, true), |(proxy, has_more)| async move {
        if !has_more {
            return Ok(None);
        }

        let (batch, has_more) = proxy.next().await?;
        if batch.is_empty() && has_more {
            Err(IpIteratorError::EmptyBatch)
        } else {
            Ok(Some((futures::stream::iter(batch.into_iter().map(Ok)), (proxy, has_more))))
        }
    })
    .try_flatten())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::num::NonZeroU64;

    use assert_matches::assert_matches;
    use futures::{FutureExt as _, StreamExt as _, future, pin_mut};
    use net_declare::{fidl_ip, fidl_subnet};
    use test_case::test_case;
    use {fidl_fuchsia_net as fnet, fidl_fuchsia_net_tcp as fnet_tcp};

    #[test_case(
        fnet_sockets::IpSocketMatcher::Family(fnet::IpVersion::V4),
        IpSocketMatcher::Family(ip::IpVersion::V4);
        "FamilyIpv4"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::Family(fnet::IpVersion::V6),
        IpSocketMatcher::Family(ip::IpVersion::V6);
        "FamilyIpv6"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::SrcAddr(fnet_matchers::BoundAddress::Bound(
            fnet_matchers::Address {
                matcher: fnet_matchers::AddressMatcherType::Subnet(fidl_subnet!("192.0.2.0/24")),
                invert: true,
            }
        )),
        IpSocketMatcher::SrcAddr(fnet_matchers_ext::BoundAddress::Bound(
            fnet_matchers_ext::Address {
                matcher: fnet_matchers_ext::AddressMatcherType::Subnet(
                    fnet_matchers_ext::Subnet::try_from(fidl_subnet!("192.0.2.0/24")).unwrap()
                ),
                invert: true,
            }
        ));
        "SrcAddr"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::DstAddr(fnet_matchers::BoundAddress::Bound(
            fnet_matchers::Address {
                matcher: fnet_matchers::AddressMatcherType::Subnet(fidl_subnet!("2001:db8::/32")),
                invert: false,
            }
        )),
        IpSocketMatcher::DstAddr(fnet_matchers_ext::BoundAddress::Bound(
            fnet_matchers_ext::Address {
                matcher: fnet_matchers_ext::AddressMatcherType::Subnet(
                    fnet_matchers_ext::Subnet::try_from(fidl_subnet!("2001:db8::/32")).unwrap()
                ),
                invert: false,
            }
        ));
        "DstAddr"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::Proto(fnet_matchers::SocketTransportProtocol::Tcp(
            fnet_matchers::TcpSocket::Empty(fnet_matchers::Empty)
        )),
        IpSocketMatcher::Proto(fnet_matchers_ext::SocketTransportProtocol::Tcp(
            fnet_matchers_ext::TcpSocket::Empty
        ));
        "ProtoTcp"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::Proto(fnet_matchers::SocketTransportProtocol::Udp(
            fnet_matchers::UdpSocket::Empty(fnet_matchers::Empty)
        )),
        IpSocketMatcher::Proto(fnet_matchers_ext::SocketTransportProtocol::Udp(
            fnet_matchers_ext::UdpSocket::Empty
        ));
        "ProtoUdp"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::BoundInterface(fnet_matchers::BoundInterface::Unbound(
            fnet_matchers::Empty
        )),
        IpSocketMatcher::BoundInterface(fnet_matchers_ext::BoundInterface::Unbound);
        "BoundInterfaceUnbound"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::BoundInterface(fnet_matchers::BoundInterface::Bound(
            fnet_matchers::Interface::Id(1)
        )),
        IpSocketMatcher::BoundInterface(fnet_matchers_ext::BoundInterface::Bound(
            fnet_matchers_ext::Interface::Id(NonZeroU64::new(1).unwrap())
        ));
        "BoundInterfaceBound"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::Cookie(fnet_matchers::SocketCookie {
            cookie: 12345,
            invert: false,
        }),
        IpSocketMatcher::Cookie(fnet_matchers::SocketCookie {
            cookie: 12345,
            invert: false,
        });
        "Cookie"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::Mark(fnet_matchers::MarkInDomain {
            domain: fnet::MarkDomain::Mark1,
            mark: fnet_matchers::Mark::Unmarked(fnet_matchers::Unmarked),
        }),
        IpSocketMatcher::Mark(fnet_matchers_ext::MarkInDomain {
            domain: fnet::MarkDomain::Mark1,
            mark: fnet_matchers_ext::Mark::Unmarked,
        });
        "Mark"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::SrcAddr(fnet_matchers::BoundAddress::Unbound(fnet_matchers::Empty)),
        IpSocketMatcher::SrcAddr(fnet_matchers_ext::BoundAddress::Unbound);
        "SrcAddrUnbound"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::DstAddr(fnet_matchers::BoundAddress::Unbound(fnet_matchers::Empty)),
        IpSocketMatcher::DstAddr(fnet_matchers_ext::BoundAddress::Unbound);
        "DstAddrUnbound"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::Proto(fnet_matchers::SocketTransportProtocol::Tcp(
            fnet_matchers::TcpSocket::SrcPort(fnet_matchers::BoundPort::Unbound(fnet_matchers::Empty))
        )),
        IpSocketMatcher::Proto(fnet_matchers_ext::SocketTransportProtocol::Tcp(
            fnet_matchers_ext::TcpSocket::SrcPort(fnet_matchers_ext::BoundPort::Unbound)
        ));
        "ProtoTcpSrcPortUnbound"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::Proto(fnet_matchers::SocketTransportProtocol::Tcp(
            fnet_matchers::TcpSocket::DstPort(fnet_matchers::BoundPort::Unbound(fnet_matchers::Empty))
        )),
        IpSocketMatcher::Proto(fnet_matchers_ext::SocketTransportProtocol::Tcp(
            fnet_matchers_ext::TcpSocket::DstPort(fnet_matchers_ext::BoundPort::Unbound)
        ));
        "ProtoTcpDstPortUnbound"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::Proto(fnet_matchers::SocketTransportProtocol::Udp(
            fnet_matchers::UdpSocket::SrcPort(fnet_matchers::BoundPort::Unbound(fnet_matchers::Empty))
        )),
        IpSocketMatcher::Proto(fnet_matchers_ext::SocketTransportProtocol::Udp(
            fnet_matchers_ext::UdpSocket::SrcPort(fnet_matchers_ext::BoundPort::Unbound)
        ));
        "ProtoUdpSrcPortUnbound"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::Proto(fnet_matchers::SocketTransportProtocol::Udp(
            fnet_matchers::UdpSocket::DstPort(fnet_matchers::BoundPort::Unbound(fnet_matchers::Empty))
        )),
        IpSocketMatcher::Proto(fnet_matchers_ext::SocketTransportProtocol::Udp(
            fnet_matchers_ext::UdpSocket::DstPort(fnet_matchers_ext::BoundPort::Unbound)
        ));
        "ProtoUdpDstPortUnbound"
    )]
    fn convert_from_fidl_and_back<F, E>(fidl_type: F, local_type: E)
    where
        E: TryFrom<F> + Clone + std::fmt::Debug + PartialEq,
        <E as TryFrom<F>>::Error: std::fmt::Debug + PartialEq,
        F: From<E> + Clone + std::fmt::Debug + PartialEq,
    {
        assert_eq!(fidl_type.clone().try_into(), Ok(local_type.clone()));
        assert_eq!(<_ as Into<F>>::into(local_type), fidl_type);
    }

    #[test_case(
        fnet_sockets::IpSocketMatcher::__SourceBreaking { unknown_ordinal: 100 } =>
            Err(IpSocketMatcherError::UnknownUnionVariant(100));
        "UnknownUnionVariant"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::SrcAddr(fnet_matchers::BoundAddress::Bound(
            fnet_matchers::Address {
                matcher: fnet_matchers::AddressMatcherType::__SourceBreaking { unknown_ordinal: 100 },
                invert: false,
            }
        )) => Err(IpSocketMatcherError::Address(fnet_matchers_ext::BoundAddressError::Address(
            fnet_matchers_ext::AddressError::AddressMatcherType(
                fnet_matchers_ext::AddressMatcherTypeError::UnknownUnionVariant
            )
        )));
        "AddressError"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::Proto(
            fnet_matchers::SocketTransportProtocol::__SourceBreaking { unknown_ordinal: 100 }
        ) => Err(IpSocketMatcherError::TransportProtocol(
            fnet_matchers_ext::SocketTransportProtocolError::UnknownUnionVariant(100)
        ));
        "TransportProtocolError"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::BoundInterface(
            fnet_matchers::BoundInterface::__SourceBreaking { unknown_ordinal: 100 }
        ) => Err(IpSocketMatcherError::BoundInterface(
            fnet_matchers_ext::BoundInterfaceError::UnknownUnionVariant(100)
        ));
        "BoundInterfaceError"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::Mark(fnet_matchers::MarkInDomain {
            domain: fnet::MarkDomain::Mark1,
            mark: fnet_matchers::Mark::__SourceBreaking { unknown_ordinal: 100 },
        }) => Err(IpSocketMatcherError::Mark(
            fnet_matchers_ext::MarkInDomainError::Mark(
                fnet_matchers_ext::MarkError::UnknownUnionVariant(100)
            )
        ));
        "MarkError"
    )]
    fn ip_socket_matcher_try_from_error(
        fidl: fnet_sockets::IpSocketMatcher,
    ) -> Result<IpSocketMatcher, IpSocketMatcherError> {
        IpSocketMatcher::try_from(fidl)
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn iterate_ip_diagnostics_iterate_ip_error() {
        async fn serve_matcher_error(req: fnet_sockets::DiagnosticsRequest) {
            match req {
                fnet_sockets::DiagnosticsRequest::IterateIp {
                    s: _,
                    extensions: _,
                    matchers: _,
                    responder,
                } => responder
                    .send(&fnet_sockets::IterateIpResult::InvalidMatcher(
                        fnet_sockets::InvalidMatcher { index: 0 },
                    ))
                    .unwrap(),
            };
        }

        let (diagnostics, diagnostics_server_end) =
            fidl::endpoints::create_proxy::<fnet_sockets::DiagnosticsMarker>();

        let (mut diagnostics_request_stream, _control_handle) =
            diagnostics_server_end.into_stream_and_control_handle();
        let server_fut = diagnostics_request_stream
            .next()
            .then(|req| {
                serve_matcher_error(req.expect("Request stream ended unexpectedly").unwrap())
            })
            .fuse();
        let client_fut = iterate_ip::<[IpSocketMatcher; 0], _>(
            &diagnostics,
            fnet_sockets::Extensions::empty(),
            [],
        );

        pin_mut!(server_fut);
        pin_mut!(client_fut);

        let ((), resp) = future::join(server_fut, client_fut).await;

        assert_matches!(
            // Discard the stream because it can't be formatted.
            resp.map(|_| ()),
            Err(IterateIpError::InvalidMatcher(0))
        );
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn iterate_ip_next_error() {
        async fn serve_matcher(req: fnet_sockets::DiagnosticsRequest) {
            match req {
                fnet_sockets::DiagnosticsRequest::IterateIp {
                    s,
                    extensions: _,
                    matchers: _,
                    responder,
                } => {
                    s.close_with_epitaph(zx_status::Status::PEER_CLOSED).unwrap();
                    responder.send(&fnet_sockets::IterateIpResult::Ok(fnet_sockets::Empty)).unwrap()
                }
            }
        }

        let (diagnostics, diagnostics_server_end) =
            fidl::endpoints::create_proxy::<fnet_sockets::DiagnosticsMarker>();

        let (mut diagnostics_request_stream, _control_handle) =
            diagnostics_server_end.into_stream_and_control_handle();
        let server_fut = diagnostics_request_stream
            .next()
            .then(|req| serve_matcher(req.expect("Request stream ended unexpectedly").unwrap()))
            .fuse();

        let client_fut = iterate_ip::<[IpSocketMatcher; 0], _>(
            &diagnostics,
            fnet_sockets::Extensions::empty(),
            [],
        );

        let ((), resp) = future::join(server_fut, client_fut).await;
        let stream = resp.unwrap();
        pin_mut!(stream);

        assert_matches!(
            stream.try_next().await,
            Err(IpIteratorError::Fidl(fidl::Error::ClientChannelClosed { .. }))
        );
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn iterate_ip_empty_batch() {
        async fn serve_matcher(req: fnet_sockets::DiagnosticsRequest) {
            match req {
                fnet_sockets::DiagnosticsRequest::IterateIp {
                    s,
                    extensions: _,
                    matchers: _,
                    responder,
                } => {
                    responder
                        .send(&fnet_sockets::IterateIpResult::Ok(fnet_sockets::Empty))
                        .unwrap();

                    let (mut stream, _control) = s.into_stream_and_control_handle();
                    match stream.next().await.unwrap().unwrap() {
                        fidl_fuchsia_net_sockets::IpIteratorRequest::Next { responder } => {
                            // Send an empty batch but indicate there's more to come.
                            responder.send(&[], true).unwrap();
                        }
                        fidl_fuchsia_net_sockets::IpIteratorRequest::_UnknownMethod { .. } => {
                            unreachable!()
                        }
                    }
                }
            }
        }

        let (diagnostics, diagnostics_server_end) =
            fidl::endpoints::create_proxy::<fnet_sockets::DiagnosticsMarker>();

        let (mut diagnostics_request_stream, _control_handle) =
            diagnostics_server_end.into_stream_and_control_handle();
        let server_fut = diagnostics_request_stream
            .next()
            .then(|req| serve_matcher(req.expect("Request stream ended unexpectedly").unwrap()))
            .fuse();

        let client_fut = async {
            let stream = iterate_ip::<[IpSocketMatcher; 0], _>(
                &diagnostics,
                fnet_sockets::Extensions::empty(),
                [],
            )
            .await
            .unwrap();
            pin_mut!(stream);
            stream.try_next().await
        };

        let ((), resp) = future::join(server_fut, client_fut).await;
        assert_matches!(resp, Err(IpIteratorError::EmptyBatch));
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn iterate_ip_success() {
        let socket_1 = fnet_sockets::IpSocketState {
            family: Some(fnet::IpVersion::V4),
            src_addr: Some(fidl_ip!("192.168.1.1")),
            dst_addr: Some(fidl_ip!("192.168.1.2")),
            cookie: Some(1234),
            marks: None,
            transport: Some(fnet_sockets::IpSocketTransportState::Tcp(
                fnet_sockets::IpSocketTcpState {
                    src_port: Some(1111),
                    dst_port: Some(2222),
                    state: Some(fnet_tcp::State::Established),
                    tcp_info: None,
                    __source_breaking: fidl::marker::SourceBreaking,
                },
            )),
            __source_breaking: fidl::marker::SourceBreaking,
        };

        let socket_2 = fnet_sockets::IpSocketState {
            family: Some(fnet::IpVersion::V4),
            src_addr: Some(fidl_ip!("192.168.8.1")),
            dst_addr: Some(fidl_ip!("192.168.8.2")),
            cookie: Some(9876),
            marks: None,
            transport: Some(fnet_sockets::IpSocketTransportState::Tcp(
                fnet_sockets::IpSocketTcpState {
                    src_port: Some(3333),
                    dst_port: Some(4444),
                    state: Some(fnet_tcp::State::TimeWait),
                    tcp_info: None,
                    __source_breaking: fidl::marker::SourceBreaking,
                },
            )),
            __source_breaking: fidl::marker::SourceBreaking,
        };

        let socket_3 = fnet_sockets::IpSocketState {
            family: Some(fnet::IpVersion::V6),
            src_addr: Some(fidl_ip!("2001:db8::1")),
            dst_addr: Some(fidl_ip!("2001:db8::2")),
            cookie: Some(5678),
            marks: None,
            transport: Some(fnet_sockets::IpSocketTransportState::Tcp(
                fnet_sockets::IpSocketTcpState {
                    src_port: Some(5555),
                    dst_port: Some(6666),
                    state: Some(fnet_tcp::State::TimeWait),
                    tcp_info: None,
                    __source_breaking: fidl::marker::SourceBreaking,
                },
            )),
            __source_breaking: fidl::marker::SourceBreaking,
        };

        let (diagnostics, diagnostics_server_end) =
            fidl::endpoints::create_proxy::<fnet_sockets::DiagnosticsMarker>();

        let serve_matcher = async |req: fnet_sockets::DiagnosticsRequest| {
            let responses = &[vec![socket_1.clone()], vec![socket_2.clone(), socket_3.clone()]];

            match req {
                fnet_sockets::DiagnosticsRequest::IterateIp {
                    s,
                    extensions: _,
                    matchers: _,
                    responder,
                } => {
                    responder
                        .send(&fnet_sockets::IterateIpResult::Ok(fnet_sockets::Empty))
                        .unwrap();

                    let (mut stream, _control) = s.into_stream_and_control_handle();
                    for (i, resp) in responses.iter().enumerate() {
                        match stream.next().await.unwrap().unwrap() {
                            fidl_fuchsia_net_sockets::IpIteratorRequest::Next { responder } => {
                                let has_more = i < responses.len() - 1;
                                responder.send(&resp, has_more).unwrap();
                            }
                            fidl_fuchsia_net_sockets::IpIteratorRequest::_UnknownMethod {
                                ..
                            } => {
                                unreachable!()
                            }
                        }
                    }
                }
            };
        };

        let (mut diagnostics_request_stream, _control_handle) =
            diagnostics_server_end.into_stream_and_control_handle();

        let server_fut = diagnostics_request_stream
            .next()
            .then(|req| serve_matcher(req.expect("Request stream ended unexpectedly").unwrap()))
            .fuse();

        let client_fut = async {
            iterate_ip::<[IpSocketMatcher; 0], _>(
                &diagnostics,
                fnet_sockets::Extensions::empty(),
                [],
            )
            .await
            .unwrap()
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
        };

        let ((), sockets) = future::join(server_fut, client_fut).await;
        assert_eq!(sockets, vec![socket_1.clone(), socket_2.clone(), socket_3.clone()]);
    }
}
