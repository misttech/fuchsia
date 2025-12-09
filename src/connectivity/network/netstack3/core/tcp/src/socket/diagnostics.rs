// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::convert::Infallible as Never;
use core::num::NonZeroU16;

use net_types::Witness as _;
use net_types::ip::{Ip, IpAddress as _};
use netstack3_base::{IpSocketProperties, Marks, Matcher, WeakDeviceIdentifier};

use crate::internal::socket::{
    BoundSocketState, DualStackIpExt, MaybeListener, SocketCookie, TcpBindingsTypes, TcpSocketId,
    TcpSocketState, TcpSocketStateInner,
};
use crate::internal::state::State;

/// Required state gathered into one struct for matching a socket, so it's
/// possible to implement traits against the collection.
pub(crate) struct TcpSocketStateForMatching<
    'a,
    I: DualStackIpExt,
    D: netstack3_base::WeakDeviceIdentifier,
    BT: TcpBindingsTypes,
> {
    pub(crate) state: &'a TcpSocketState<I, D, BT>,
    pub(crate) id: &'a TcpSocketId<I, D, BT>,
}

impl<'a, I: DualStackIpExt, D: netstack3_base::WeakDeviceIdentifier, BT: TcpBindingsTypes>
    netstack3_base::MaybeSocketTransportProperties for TcpSocketStateForMatching<'a, I, D, BT>
{
    type TcpProps<'b>
        = Self
    where
        Self: 'b;

    type UdpProps<'b>
        = Never
    where
        Self: 'b;

    fn tcp_socket_properties(&self) -> Option<&Self::TcpProps<'_>> {
        Some(self)
    }

    fn udp_socket_properties(&self) -> Option<&Self::UdpProps<'_>> {
        None
    }
}

impl<'a, I: DualStackIpExt, D: netstack3_base::WeakDeviceIdentifier, BT: TcpBindingsTypes>
    netstack3_base::TcpSocketProperties for TcpSocketStateForMatching<'a, I, D, BT>
{
    fn src_port_matches(&self, matcher: &netstack3_base::PortMatcher) -> bool {
        let src_port = match &self.state.socket_state {
            TcpSocketStateInner::Unbound(_) => return false,
            TcpSocketStateInner::Bound(BoundSocketState::Listener((_, _, addr))) => {
                I::get_bound_info(addr).port.get()
            }
            TcpSocketStateInner::Bound(BoundSocketState::Connected { conn, .. }) => {
                I::get_conn_info(conn).local_addr.port.get()
            }
        };

        matcher.matches(&src_port)
    }

    fn dst_port_matches(&self, matcher: &netstack3_base::PortMatcher) -> bool {
        let dst_port = match &self.state.socket_state {
            TcpSocketStateInner::Unbound(_)
            | TcpSocketStateInner::Bound(BoundSocketState::Listener(_)) => return false,
            TcpSocketStateInner::Bound(BoundSocketState::Connected { conn, .. }) => {
                I::get_conn_info(conn).remote_addr.port.get()
            }
        };

        matcher.matches(&dst_port)
    }

    fn state_matches(&self, matcher: &netstack3_base::TcpStateMatcher) -> bool {
        matcher.matches(&self.state.base_state())
    }
}

impl<'a, I: DualStackIpExt, D: netstack3_base::WeakDeviceIdentifier, BT: TcpBindingsTypes>
    IpSocketProperties<BT::DeviceClass> for TcpSocketStateForMatching<'a, I, D, BT>
where
    D::Strong: netstack3_base::InterfaceProperties<BT::DeviceClass>,
{
    fn family_matches(&self, family: &net_types::ip::IpVersion) -> bool {
        I::VERSION == *family
    }

    fn src_addr_matches(&self, addr: &netstack3_base::AddressMatcherEither) -> bool {
        let src_addr = match &self.state.socket_state {
            TcpSocketStateInner::Unbound(_) => None,
            TcpSocketStateInner::Bound(BoundSocketState::Listener((_, _, addr))) => {
                I::get_bound_info(addr).addr.map(|a| a.addr().get())
            }
            TcpSocketStateInner::Bound(BoundSocketState::Connected { conn, .. }) => {
                Some(I::get_conn_info(conn).local_addr.ip.addr().get())
            }
        };

        let Some(src_addr) = src_addr else { return false };
        addr.matches(&src_addr.to_ip_addr())
    }

    fn dst_addr_matches(&self, addr: &netstack3_base::AddressMatcherEither) -> bool {
        let dst_addr = match &self.state.socket_state {
            TcpSocketStateInner::Unbound(_)
            | TcpSocketStateInner::Bound(BoundSocketState::Listener(_)) => return false,
            TcpSocketStateInner::Bound(BoundSocketState::Connected { conn, .. }) => {
                I::get_conn_info(conn).remote_addr.ip.addr().get()
            }
        };

        addr.matches(&dst_addr.to_ip_addr())
    }

    fn transport_protocol_matches(
        &self,
        proto: &netstack3_base::SocketTransportProtocolMatcher,
    ) -> bool {
        proto.matches(self)
    }

    fn bound_interface_matches(
        &self,
        iface: &netstack3_base::BoundInterfaceMatcher<BT::DeviceClass>,
    ) -> bool {
        let device = match &self.state.socket_state {
            TcpSocketStateInner::Unbound(_) => None,
            TcpSocketStateInner::Bound(BoundSocketState::Listener((_, _, addr))) => {
                I::get_bound_info(addr).device
            }
            TcpSocketStateInner::Bound(BoundSocketState::Connected { conn, .. }) => {
                I::get_conn_info(conn).device
            }
        };
        iface.matches(&device.and_then(|d| d.upgrade()).as_ref())
    }

    fn cookie_matches(&self, cookie: &netstack3_base::SocketCookieMatcher) -> bool {
        cookie.matches(&self.id.socket_cookie().export_value())
    }

    fn mark_matches(&self, matcher: &netstack3_base::MarkInDomainMatcher) -> bool {
        matcher.matcher.matches(self.state.socket_options.ip_options.marks.get(matcher.domain))
    }
}

/// Publicly-accessible diagnostic information about TCP sockets.
#[cfg_attr(any(test, feature = "testutils"), derive(Debug, PartialEq, Eq))]
pub struct TcpSocketDiagnostics<I: Ip> {
    /// The socket's TCP state machine.
    pub state_machine: netstack3_base::TcpSocketState,
    /// The socket's tuple.
    pub tuple: TcpSocketDiagnosticTuple<I>,
    /// The socket's cookie.
    pub cookie: SocketCookie,
    /// The socket's marks.
    pub marks: Marks,
}

/// The tuple of a TCP socket.
///
/// This is separate from the state machine state because it's possible for
/// some states to be entered while having just the 2-tuple or the full
/// 4-tuple (CLOSED and LISTENING).
#[derive(Debug, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum TcpSocketDiagnosticTuple<I: Ip> {
    /// The socket is bound, but not connected. Only the 2-tuple is available,
    /// although the source address might be None if the socket is bound to the
    /// catch-all address.
    Bound { src_addr: Option<I::Addr>, src_port: NonZeroU16 },
    /// The socket is connected, so the full 4-tuple is available.
    Connected { src_addr: I::Addr, src_port: NonZeroU16, dst_addr: I::Addr, dst_port: NonZeroU16 },
}

impl<I: Ip> TcpSocketDiagnosticTuple<I> {
    /// Returns the socket's source address.
    pub fn src_addr(&self) -> Option<I::Addr> {
        match self {
            Self::Bound { src_addr, src_port: _ } => *src_addr,
            Self::Connected { src_addr, src_port: _, dst_addr: _, dst_port: _ } => Some(*src_addr),
        }
    }

    /// Returns the socket's source port.
    pub fn src_port(&self) -> Option<NonZeroU16> {
        match self {
            Self::Bound { src_addr: _, src_port }
            | Self::Connected { src_addr: _, src_port, dst_addr: _, dst_port: _ } => {
                Some(*src_port)
            }
        }
    }

    /// Returns the socket's destination address.
    pub fn dst_addr(&self) -> Option<I::Addr> {
        match self {
            Self::Bound { src_addr: _, src_port: _ } => None,
            Self::Connected { src_addr: _, src_port: _, dst_addr, dst_port: _ } => Some(*dst_addr),
        }
    }

    /// Returns the socket's destination port.
    pub fn dst_port(&self) -> Option<NonZeroU16> {
        match self {
            Self::Bound { src_addr: _, src_port: _ } => None,
            Self::Connected { src_addr: _, src_port: _, dst_addr: _, dst_port } => Some(*dst_port),
        }
    }
}

impl<I, D, BT> TcpSocketState<I, D, BT>
where
    I: DualStackIpExt,
    D: WeakDeviceIdentifier,
    BT: TcpBindingsTypes,
{
    pub(crate) fn get_diagnostics(
        &self,
    ) -> Option<(TcpSocketDiagnosticTuple<I>, netstack3_base::TcpSocketState, Marks)> {
        let tuple = match &self.socket_state {
            TcpSocketStateInner::Unbound(_) => None,
            TcpSocketStateInner::Bound(BoundSocketState::Listener((_, _, addr))) => {
                let addr_info = I::get_bound_info(addr);
                Some(TcpSocketDiagnosticTuple::Bound {
                    src_addr: addr_info.addr.map(|ip| ip.addr().get()),
                    src_port: addr_info.port,
                })
            }
            TcpSocketStateInner::Bound(BoundSocketState::Connected { conn, .. }) => {
                let info = I::get_conn_info(conn);
                Some(TcpSocketDiagnosticTuple::Connected {
                    src_addr: info.local_addr.ip.addr().get(),
                    dst_addr: info.remote_addr.ip.addr().get(),
                    src_port: info.local_addr.port,
                    dst_port: info.remote_addr.port,
                })
            }
        }?;

        Some((tuple, self.base_state(), self.socket_options.ip_options.marks))
    }

    pub(crate) fn base_state(&self) -> netstack3_base::TcpSocketState {
        match &self.socket_state {
            TcpSocketStateInner::Unbound(_) => netstack3_base::TcpSocketState::Close,
            TcpSocketStateInner::Bound(BoundSocketState::Listener((maybe_listener, _, _))) => {
                match maybe_listener {
                    MaybeListener::Listener(_) => netstack3_base::TcpSocketState::Listen,
                    // Despite being in the BoundSocketState::Listener variant,
                    // this is used for a socket that is bound, but not yet
                    // either connected or listening.
                    MaybeListener::Bound(_) => netstack3_base::TcpSocketState::Close,
                }
            }
            TcpSocketStateInner::Bound(BoundSocketState::Connected { conn, .. }) => {
                match I::get_state(conn) {
                    State::Closed(_) => netstack3_base::TcpSocketState::Close,
                    State::Listen(_) => netstack3_base::TcpSocketState::Listen,
                    State::SynRcvd(_) => netstack3_base::TcpSocketState::SynRecv,
                    State::SynSent(_) => netstack3_base::TcpSocketState::SynSent,
                    State::Established(_) => netstack3_base::TcpSocketState::Established,
                    State::CloseWait(_) => netstack3_base::TcpSocketState::CloseWait,
                    State::LastAck(_) => netstack3_base::TcpSocketState::LastAck,
                    State::FinWait1(_) => netstack3_base::TcpSocketState::FinWait1,
                    State::FinWait2(_) => netstack3_base::TcpSocketState::FinWait2,
                    State::Closing(_) => netstack3_base::TcpSocketState::Closing,
                    State::TimeWait(_) => netstack3_base::TcpSocketState::TimeWait,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::num::NonZeroUsize;

    use ip_test_macro::ip_test;
    use net_types::ZonedAddr;
    use net_types::ip::{Ip, Subnet};
    use netstack3_base::testutil::{FakeDeviceId, set_logger_for_test};
    use netstack3_base::{
        AddressMatcher, AddressMatcherEither, AddressMatcherType, BoundInterfaceMatcher,
        InterfaceMatcher, IpSocketMatcher, Mark, MarkDomain, MarkMatcher, PortMatcher,
        SocketCookieMatcher, SocketTransportProtocolMatcher, SubnetMatcher, TcpSocketMatcher,
        TcpStateMatcher, UdpSocketMatcher,
    };

    use super::*;
    use crate::TcpContext;
    use crate::internal::socket::tests::{
        TcpApiExt, TcpBindingsCtx, TcpCoreCtx, TcpCtx, TcpTestIpExt,
    };

    const LOCAL_PORT_1: NonZeroU16 = NonZeroU16::new(1234).unwrap();
    const LOCAL_PORT_2: NonZeroU16 = NonZeroU16::new(5678).unwrap();
    const LOCAL_PORT_3: NonZeroU16 = NonZeroU16::new(4321).unwrap();

    const REMOTE_PORT_1: NonZeroU16 = NonZeroU16::new(100).unwrap();
    const REMOTE_PORT_2: NonZeroU16 = NonZeroU16::new(200).unwrap();

    const MARK: u32 = 0x10;
    const MARK_MASK: u32 = !0;

    #[ip_test(I)]
    fn diagnostics_match_ip_version<I: TcpTestIpExt>()
    where
        TcpCoreCtx<FakeDeviceId, TcpBindingsCtx<FakeDeviceId>>:
            TcpContext<I, TcpBindingsCtx<FakeDeviceId>>,
    {
        set_logger_for_test();

        let mut ctx = TcpCtx::with_core_ctx(TcpCoreCtx::new::<I>(
            I::TEST_ADDRS.local_ip,
            I::TEST_ADDRS.remote_ip,
        ));
        let mut api = ctx.tcp_api::<I>();
        let s = api.create(Default::default());
        api.bind(&s, None, Some(LOCAL_PORT_1)).expect("bind should succeed");
        api.listen(&s, NonZeroUsize::new(1).unwrap()).expect("listen should succeed");

        let mut results = Vec::new();
        api.bound_sockets_diagnostics(&IpSocketMatcher::Family(I::VERSION), &mut results);
        assert_eq!(
            results,
            vec![TcpSocketDiagnostics {
                state_machine: netstack3_base::TcpSocketState::Listen,
                tuple: TcpSocketDiagnosticTuple::Bound { src_addr: None, src_port: LOCAL_PORT_1 },
                cookie: s.socket_cookie(),
                marks: Marks::default(),
            }]
        );

        results.clear();
        api.bound_sockets_diagnostics(
            &IpSocketMatcher::Family(
                <<I as netstack3_base::socket::DualStackIpExt>::OtherVersion as Ip>::VERSION,
            ),
            &mut results,
        );
        assert_eq!(results, Vec::new());
    }

    #[ip_test(I)]
    fn diagnostics_match_src_addr<I: TcpTestIpExt>()
    where
        TcpCoreCtx<FakeDeviceId, TcpBindingsCtx<FakeDeviceId>>:
            TcpContext<I, TcpBindingsCtx<FakeDeviceId>>,
    {
        set_logger_for_test();

        let mut ctx = TcpCtx::with_core_ctx(TcpCoreCtx::new::<I>(
            I::TEST_ADDRS.local_ip,
            I::TEST_ADDRS.remote_ip,
        ));
        let mut api = ctx.tcp_api::<I>();
        let s = api.create(Default::default());
        api.bind(&s, Some(ZonedAddr::Unzoned(I::TEST_ADDRS.local_ip)), Some(LOCAL_PORT_1))
            .expect("bind should succeed");
        api.listen(&s, NonZeroUsize::new(1).unwrap()).expect("listen should succeed");

        let mut results = Vec::new();
        let matcher = I::map_ip_in(
            I::TEST_ADDRS.local_ip.get(),
            |addr| {
                AddressMatcherEither::V4(AddressMatcher {
                    matcher: AddressMatcherType::Subnet(SubnetMatcher(
                        Subnet::new(addr, 32).unwrap(),
                    )),
                    invert: false,
                })
            },
            |addr| {
                AddressMatcherEither::V6(AddressMatcher {
                    matcher: AddressMatcherType::Subnet(SubnetMatcher(
                        Subnet::new(addr, 128).unwrap(),
                    )),
                    invert: false,
                })
            },
        );
        api.bound_sockets_diagnostics(&IpSocketMatcher::SrcAddr(matcher), &mut results);
        assert_eq!(
            results,
            vec![TcpSocketDiagnostics {
                state_machine: netstack3_base::TcpSocketState::Listen,
                tuple: TcpSocketDiagnosticTuple::Bound {
                    src_addr: Some(I::TEST_ADDRS.local_ip.get()),
                    src_port: LOCAL_PORT_1,
                },
                cookie: s.socket_cookie(),
                marks: Marks::default(),
            }]
        );

        results.clear();
        let matcher = I::map_ip_in(
            I::TEST_ADDRS.remote_ip.get(),
            |addr| {
                AddressMatcherEither::V4(AddressMatcher {
                    matcher: AddressMatcherType::Subnet(SubnetMatcher(
                        Subnet::new(addr, 32).unwrap(),
                    )),
                    invert: false,
                })
            },
            |addr| {
                AddressMatcherEither::V6(AddressMatcher {
                    matcher: AddressMatcherType::Subnet(SubnetMatcher(
                        Subnet::new(addr, 128).unwrap(),
                    )),
                    invert: false,
                })
            },
        );
        api.bound_sockets_diagnostics(&IpSocketMatcher::SrcAddr(matcher), &mut results);
        assert_eq!(results, Vec::new());
    }

    #[ip_test(I)]
    fn diagnostics_match_dst_addr<I: TcpTestIpExt>()
    where
        TcpCoreCtx<FakeDeviceId, TcpBindingsCtx<FakeDeviceId>>:
            TcpContext<I, TcpBindingsCtx<FakeDeviceId>>,
    {
        set_logger_for_test();

        let mut ctx = TcpCtx::with_core_ctx(TcpCoreCtx::new::<I>(
            I::TEST_ADDRS.local_ip,
            I::TEST_ADDRS.remote_ip,
        ));
        let mut api = ctx.tcp_api::<I>();
        let s = api.create(Default::default());
        api.bind(&s, Some(ZonedAddr::Unzoned(I::TEST_ADDRS.local_ip)), Some(LOCAL_PORT_1))
            .expect("bind should succeed");
        api.connect(&s, Some(ZonedAddr::Unzoned(I::TEST_ADDRS.remote_ip)), LOCAL_PORT_2)
            .expect("connect should succeed");

        let mut results = Vec::new();
        let matcher = I::map_ip_in(
            I::TEST_ADDRS.remote_ip.get(),
            |addr| {
                AddressMatcherEither::V4(AddressMatcher {
                    matcher: AddressMatcherType::Subnet(SubnetMatcher(
                        Subnet::new(addr, 32).unwrap(),
                    )),
                    invert: false,
                })
            },
            |addr| {
                AddressMatcherEither::V6(AddressMatcher {
                    matcher: AddressMatcherType::Subnet(SubnetMatcher(
                        Subnet::new(addr, 128).unwrap(),
                    )),
                    invert: false,
                })
            },
        );
        api.bound_sockets_diagnostics(&IpSocketMatcher::DstAddr(matcher), &mut results);
        assert_eq!(
            results,
            vec![TcpSocketDiagnostics {
                state_machine: netstack3_base::TcpSocketState::SynSent,
                tuple: TcpSocketDiagnosticTuple::Connected {
                    src_addr: I::TEST_ADDRS.local_ip.get(),
                    src_port: LOCAL_PORT_1,
                    dst_addr: I::TEST_ADDRS.remote_ip.get(),
                    dst_port: LOCAL_PORT_2,
                },
                cookie: s.socket_cookie(),
                marks: Marks::default(),
            }]
        );

        results.clear();
        let matcher = I::map_ip_in(
            I::TEST_ADDRS.local_ip.get(),
            |addr| {
                AddressMatcherEither::V4(AddressMatcher {
                    matcher: AddressMatcherType::Subnet(SubnetMatcher(
                        Subnet::new(addr, 32).unwrap(),
                    )),
                    invert: false,
                })
            },
            |addr| {
                AddressMatcherEither::V6(AddressMatcher {
                    matcher: AddressMatcherType::Subnet(SubnetMatcher(
                        Subnet::new(addr, 128).unwrap(),
                    )),
                    invert: false,
                })
            },
        );
        api.bound_sockets_diagnostics(&IpSocketMatcher::DstAddr(matcher), &mut results);
        assert_eq!(results, Vec::new());
    }

    #[ip_test(I)]
    fn diagnostics_match_proto<I: TcpTestIpExt>()
    where
        TcpCoreCtx<FakeDeviceId, TcpBindingsCtx<FakeDeviceId>>:
            TcpContext<I, TcpBindingsCtx<FakeDeviceId>>,
    {
        set_logger_for_test();

        let mut ctx = TcpCtx::with_core_ctx(TcpCoreCtx::new::<I>(
            I::TEST_ADDRS.local_ip,
            I::TEST_ADDRS.remote_ip,
        ));
        let mut api = ctx.tcp_api::<I>();
        let s = api.create(Default::default());
        api.bind(&s, None, Some(LOCAL_PORT_1)).expect("bind should succeed");
        api.listen(&s, NonZeroUsize::new(1).unwrap()).expect("listen should succeed");

        let mut results = Vec::new();
        api.bound_sockets_diagnostics(
            &IpSocketMatcher::Proto(SocketTransportProtocolMatcher::Tcp(TcpSocketMatcher::Empty)),
            &mut results,
        );
        assert_eq!(
            results,
            vec![TcpSocketDiagnostics {
                state_machine: netstack3_base::TcpSocketState::Listen,
                tuple: TcpSocketDiagnosticTuple::Bound { src_addr: None, src_port: LOCAL_PORT_1 },
                cookie: s.socket_cookie(),
                marks: Marks::default(),
            }]
        );

        results.clear();
        api.bound_sockets_diagnostics(
            &IpSocketMatcher::Proto(SocketTransportProtocolMatcher::Udp(UdpSocketMatcher::Empty)),
            &mut results,
        );
        assert_eq!(results, Vec::new());
    }

    #[ip_test(I)]
    fn diagnostics_match_device<I: TcpTestIpExt>()
    where
        TcpCoreCtx<FakeDeviceId, TcpBindingsCtx<FakeDeviceId>>:
            TcpContext<I, TcpBindingsCtx<FakeDeviceId>>,
    {
        set_logger_for_test();

        let mut ctx = TcpCtx::with_core_ctx(TcpCoreCtx::new::<I>(
            I::TEST_ADDRS.local_ip,
            I::TEST_ADDRS.remote_ip,
        ));
        let mut api = ctx.tcp_api::<I>();
        let s = api.create(Default::default());
        api.set_device(&s, Some(FakeDeviceId)).expect("set device should succeed");
        api.bind(&s, None, Some(LOCAL_PORT_1)).expect("bind should succeed");
        api.listen(&s, NonZeroUsize::new(1).unwrap()).expect("listen should succeed");

        let mut results = Vec::new();
        api.bound_sockets_diagnostics(
            &IpSocketMatcher::BoundInterface(BoundInterfaceMatcher::Bound(InterfaceMatcher::Name(
                FakeDeviceId::FAKE_NAME.to_string(),
            ))),
            &mut results,
        );
        assert_eq!(
            results,
            vec![TcpSocketDiagnostics {
                state_machine: netstack3_base::TcpSocketState::Listen,
                tuple: TcpSocketDiagnosticTuple::Bound { src_addr: None, src_port: LOCAL_PORT_1 },
                cookie: s.socket_cookie(),
                marks: Marks::default(),
            }]
        );

        results.clear();
        api.bound_sockets_diagnostics(
            &IpSocketMatcher::BoundInterface(BoundInterfaceMatcher::Unbound),
            &mut results,
        );
        assert_eq!(results, Vec::new());
    }

    #[ip_test(I)]
    fn diagnostics_match_cookie<I: TcpTestIpExt>()
    where
        TcpCoreCtx<FakeDeviceId, TcpBindingsCtx<FakeDeviceId>>:
            TcpContext<I, TcpBindingsCtx<FakeDeviceId>>,
    {
        set_logger_for_test();

        let mut ctx = TcpCtx::with_core_ctx(TcpCoreCtx::new::<I>(
            I::TEST_ADDRS.local_ip,
            I::TEST_ADDRS.remote_ip,
        ));
        let mut api = ctx.tcp_api::<I>();

        let socket_1 = api.create(Default::default());
        api.bind(&socket_1, None, Some(LOCAL_PORT_1)).expect("bind should succeed");
        api.listen(&socket_1, NonZeroUsize::new(1).unwrap()).expect("listen should succeed");
        let socket_2 = api.create(Default::default());
        api.bind(&socket_2, None, Some(LOCAL_PORT_2)).expect("bind should succeed");
        api.listen(&socket_2, NonZeroUsize::new(1).unwrap()).expect("listen should succeed");

        let mut results = Vec::new();
        api.bound_sockets_diagnostics(
            &IpSocketMatcher::Cookie(SocketCookieMatcher {
                cookie: socket_1.socket_cookie().export_value(),
                invert: false,
            }),
            &mut results,
        );
        assert_eq!(
            results,
            vec![TcpSocketDiagnostics {
                state_machine: netstack3_base::TcpSocketState::Listen,
                tuple: TcpSocketDiagnosticTuple::Bound { src_addr: None, src_port: LOCAL_PORT_1 },
                cookie: socket_1.socket_cookie(),
                marks: Marks::default(),
            }]
        );

        results.clear();
        api.bound_sockets_diagnostics(
            &IpSocketMatcher::Cookie(SocketCookieMatcher {
                cookie: socket_2.socket_cookie().export_value(),
                invert: false,
            }),
            &mut results,
        );
        assert_eq!(
            results,
            vec![TcpSocketDiagnostics {
                state_machine: netstack3_base::TcpSocketState::Listen,
                tuple: TcpSocketDiagnosticTuple::Bound { src_addr: None, src_port: LOCAL_PORT_2 },
                cookie: socket_2.socket_cookie(),
                marks: Marks::default(),
            }]
        );
    }

    #[ip_test(I)]
    #[test_case::test_case(MarkDomain::Mark1; "mark_1")]
    #[test_case::test_case(MarkDomain::Mark2; "mark_2")]
    fn diagnostics_match_mark<I: TcpTestIpExt>(domain: MarkDomain)
    where
        TcpCoreCtx<FakeDeviceId, TcpBindingsCtx<FakeDeviceId>>:
            TcpContext<I, TcpBindingsCtx<FakeDeviceId>>,
    {
        set_logger_for_test();

        let mut ctx = TcpCtx::with_core_ctx(TcpCoreCtx::new::<I>(
            I::TEST_ADDRS.local_ip,
            I::TEST_ADDRS.remote_ip,
        ));
        let mut api = ctx.tcp_api::<I>();

        let s = api.create(Default::default());
        api.bind(&s, None, Some(LOCAL_PORT_1)).expect("bind should succeed");
        api.listen(&s, NonZeroUsize::new(1).unwrap()).expect("listen should succeed");
        api.set_mark(&s, domain, Mark(Some(MARK)));

        let mut results = Vec::new();
        let matcher = |query_mark| {
            IpSocketMatcher::Mark(netstack3_base::MarkInDomainMatcher {
                domain,
                matcher: MarkMatcher::Marked {
                    mask: MARK_MASK,
                    start: query_mark,
                    end: query_mark,
                    invert: false,
                },
            })
        };
        api.bound_sockets_diagnostics(&matcher(MARK), &mut results);
        assert_eq!(
            results,
            vec![TcpSocketDiagnostics {
                state_machine: netstack3_base::TcpSocketState::Listen,
                tuple: TcpSocketDiagnosticTuple::Bound { src_addr: None, src_port: LOCAL_PORT_1 },
                cookie: s.socket_cookie(),
                marks: netstack3_base::MarkStorage::new([(domain, MARK)]),
            }]
        );

        results.clear();
        api.bound_sockets_diagnostics(&matcher(MARK + 1), &mut results);
        assert_eq!(results, Vec::new());
    }

    #[ip_test(I)]
    fn diagnostics_match_multiple<I: TcpTestIpExt>()
    where
        TcpCoreCtx<FakeDeviceId, TcpBindingsCtx<FakeDeviceId>>:
            TcpContext<I, TcpBindingsCtx<FakeDeviceId>>,
    {
        set_logger_for_test();

        let mut ctx = TcpCtx::with_core_ctx(TcpCoreCtx::new::<I>(
            I::TEST_ADDRS.local_ip,
            I::TEST_ADDRS.remote_ip,
        ));
        let mut api = ctx.tcp_api::<I>();

        let socket_1 = api.create(Default::default());
        api.bind(&socket_1, Some(ZonedAddr::Unzoned(I::TEST_ADDRS.local_ip)), Some(LOCAL_PORT_1))
            .expect("bind should succeed");
        api.connect(&socket_1, Some(ZonedAddr::Unzoned(I::TEST_ADDRS.remote_ip)), REMOTE_PORT_1)
            .expect("connect socket 1");
        let socket_2 = api.create(Default::default());
        api.bind(&socket_2, Some(ZonedAddr::Unzoned(I::TEST_ADDRS.local_ip)), Some(LOCAL_PORT_2))
            .expect("bind should succeed");
        api.connect(&socket_2, Some(ZonedAddr::Unzoned(I::TEST_ADDRS.remote_ip)), REMOTE_PORT_1)
            .expect("connect socket 2");
        let socket_3 = api.create(Default::default());
        api.bind(&socket_3, Some(ZonedAddr::Unzoned(I::TEST_ADDRS.local_ip)), Some(LOCAL_PORT_3))
            .expect("bind should succeed");
        api.connect(&socket_3, Some(ZonedAddr::Unzoned(I::TEST_ADDRS.remote_ip)), REMOTE_PORT_2)
            .expect("connect socket 3");

        let mut results = Vec::new();

        api.bound_sockets_diagnostics(
            &IpSocketMatcher::Proto(SocketTransportProtocolMatcher::Tcp(
                TcpSocketMatcher::DstPort(PortMatcher {
                    range: REMOTE_PORT_1.get()..=REMOTE_PORT_1.get(),
                    invert: false,
                }),
            )),
            &mut results,
        );

        results.sort_by(|a, b| a.cookie.cmp(&b.cookie));
        let mut expected = vec![
            TcpSocketDiagnostics {
                state_machine: netstack3_base::TcpSocketState::SynSent,
                tuple: TcpSocketDiagnosticTuple::Connected {
                    src_addr: I::TEST_ADDRS.local_ip.get(),
                    src_port: LOCAL_PORT_1,
                    dst_addr: I::TEST_ADDRS.remote_ip.get(),
                    dst_port: REMOTE_PORT_1,
                },
                cookie: socket_1.socket_cookie(),
                marks: Marks::default(),
            },
            TcpSocketDiagnostics {
                state_machine: netstack3_base::TcpSocketState::SynSent,
                tuple: TcpSocketDiagnosticTuple::Connected {
                    src_addr: I::TEST_ADDRS.local_ip.get(),
                    src_port: LOCAL_PORT_2,
                    dst_addr: I::TEST_ADDRS.remote_ip.get(),
                    dst_port: REMOTE_PORT_1,
                },
                cookie: socket_2.socket_cookie(),
                marks: Marks::default(),
            },
        ];
        expected.sort_by(|a, b| a.cookie.cmp(&b.cookie));
        assert_eq!(results, expected);

        results.clear();
        api.bound_sockets_diagnostics(
            &IpSocketMatcher::Proto(SocketTransportProtocolMatcher::Tcp(
                TcpSocketMatcher::DstPort(PortMatcher {
                    range: REMOTE_PORT_2.get()..=REMOTE_PORT_2.get(),
                    invert: false,
                }),
            )),
            &mut results,
        );
        assert_eq!(
            results,
            vec![TcpSocketDiagnostics {
                state_machine: netstack3_base::TcpSocketState::SynSent,
                tuple: TcpSocketDiagnosticTuple::Connected {
                    src_addr: I::TEST_ADDRS.local_ip.get(),
                    src_port: LOCAL_PORT_3,
                    dst_addr: I::TEST_ADDRS.remote_ip.get(),
                    dst_port: REMOTE_PORT_2,
                },
                cookie: socket_3.socket_cookie(),
                marks: Marks::default(),
            }]
        );
    }

    #[ip_test(I)]
    fn diagnostics_match_src_port<I: TcpTestIpExt>()
    where
        TcpCoreCtx<FakeDeviceId, TcpBindingsCtx<FakeDeviceId>>: TcpContext<
                I,
                TcpBindingsCtx<FakeDeviceId>,
                SingleStackConverter = I::SingleStackConverter,
                DualStackConverter = I::DualStackConverter,
            >,
    {
        set_logger_for_test();

        let mut ctx = TcpCtx::with_core_ctx(TcpCoreCtx::new::<I>(
            I::TEST_ADDRS.local_ip,
            I::TEST_ADDRS.remote_ip,
        ));
        let mut api = ctx.tcp_api::<I>();
        let s = api.create(Default::default());
        api.bind(&s, Some(ZonedAddr::Unzoned(I::TEST_ADDRS.local_ip)), Some(LOCAL_PORT_1))
            .expect("bind should succeed");
        api.listen(&s, NonZeroUsize::new(1).unwrap()).expect("listen should succeed");

        let mut results = Vec::new();
        api.bound_sockets_diagnostics(
            &IpSocketMatcher::Proto(SocketTransportProtocolMatcher::Tcp(
                TcpSocketMatcher::SrcPort(PortMatcher {
                    range: LOCAL_PORT_1.get()..=LOCAL_PORT_1.get(),
                    invert: false,
                }),
            )),
            &mut results,
        );
        assert_eq!(
            results,
            vec![TcpSocketDiagnostics {
                state_machine: netstack3_base::TcpSocketState::Listen,
                tuple: TcpSocketDiagnosticTuple::Bound {
                    src_addr: Some(I::TEST_ADDRS.local_ip.get()),
                    src_port: LOCAL_PORT_1,
                },
                cookie: s.socket_cookie(),
                marks: Marks::default(),
            }]
        );

        results.clear();
        api.bound_sockets_diagnostics(
            &IpSocketMatcher::Proto(SocketTransportProtocolMatcher::Tcp(
                TcpSocketMatcher::SrcPort(PortMatcher {
                    range: (LOCAL_PORT_1.get() + 1)..=(LOCAL_PORT_1.get() + 1),
                    invert: false,
                }),
            )),
            &mut results,
        );
        assert_eq!(results, Vec::new());
    }

    #[ip_test(I)]
    fn diagnostics_match_dst_port<I: TcpTestIpExt>()
    where
        TcpCoreCtx<FakeDeviceId, TcpBindingsCtx<FakeDeviceId>>: TcpContext<
                I,
                TcpBindingsCtx<FakeDeviceId>,
                SingleStackConverter = I::SingleStackConverter,
                DualStackConverter = I::DualStackConverter,
            >,
    {
        set_logger_for_test();

        let mut ctx = TcpCtx::with_core_ctx(TcpCoreCtx::new::<I>(
            I::TEST_ADDRS.local_ip,
            I::TEST_ADDRS.remote_ip,
        ));
        let mut api = ctx.tcp_api::<I>();
        let s = api.create(Default::default());
        api.bind(&s, Some(ZonedAddr::Unzoned(I::TEST_ADDRS.local_ip)), Some(LOCAL_PORT_1))
            .expect("bind should succeed");
        api.connect(&s, Some(ZonedAddr::Unzoned(I::TEST_ADDRS.remote_ip)), LOCAL_PORT_2)
            .expect("connect should succeed");

        let mut results = Vec::new();
        api.bound_sockets_diagnostics(
            &IpSocketMatcher::Proto(SocketTransportProtocolMatcher::Tcp(
                TcpSocketMatcher::DstPort(PortMatcher {
                    range: LOCAL_PORT_2.get()..=LOCAL_PORT_2.get(),
                    invert: false,
                }),
            )),
            &mut results,
        );
        assert_eq!(
            results,
            vec![TcpSocketDiagnostics {
                state_machine: netstack3_base::TcpSocketState::SynSent,
                tuple: TcpSocketDiagnosticTuple::Connected {
                    src_addr: I::TEST_ADDRS.local_ip.get(),
                    src_port: LOCAL_PORT_1,
                    dst_addr: I::TEST_ADDRS.remote_ip.get(),
                    dst_port: LOCAL_PORT_2,
                },
                cookie: s.socket_cookie(),
                marks: Marks::default(),
            }]
        );

        results.clear();
        api.bound_sockets_diagnostics(
            &IpSocketMatcher::Proto(SocketTransportProtocolMatcher::Tcp(
                TcpSocketMatcher::DstPort(PortMatcher {
                    range: (LOCAL_PORT_2.get() + 1)..=(LOCAL_PORT_2.get() + 1),
                    invert: false,
                }),
            )),
            &mut results,
        );
        assert_eq!(results, Vec::new());
    }

    #[ip_test(I)]
    fn diagnostics_match_state<I: TcpTestIpExt>()
    where
        TcpCoreCtx<FakeDeviceId, TcpBindingsCtx<FakeDeviceId>>: TcpContext<
                I,
                TcpBindingsCtx<FakeDeviceId>,
                SingleStackConverter = I::SingleStackConverter,
                DualStackConverter = I::DualStackConverter,
            >,
    {
        set_logger_for_test();

        let mut ctx = TcpCtx::with_core_ctx(TcpCoreCtx::new::<I>(
            I::TEST_ADDRS.local_ip,
            I::TEST_ADDRS.remote_ip,
        ));
        let mut api = ctx.tcp_api::<I>();

        // Socket 1: LISTEN
        let listen_socket = api.create(Default::default());
        api.bind(
            &listen_socket,
            Some(ZonedAddr::Unzoned(I::TEST_ADDRS.local_ip)),
            Some(LOCAL_PORT_1),
        )
        .expect("bind");
        api.listen(&listen_socket, NonZeroUsize::new(1).unwrap()).expect("listen");

        // Socket 2: SYN_SENT
        let syn_sent_socket = api.create(Default::default());
        api.bind(
            &syn_sent_socket,
            Some(ZonedAddr::Unzoned(I::TEST_ADDRS.local_ip)),
            Some(LOCAL_PORT_2),
        )
        .expect("bind");
        // Connect to a remote address that won't respond immediately (since we don't step the
        // network).
        api.connect(
            &syn_sent_socket,
            Some(ZonedAddr::Unzoned(I::TEST_ADDRS.remote_ip)),
            LOCAL_PORT_3,
        )
        .expect("connect");

        let mut results = Vec::new();

        api.bound_sockets_diagnostics(
            &IpSocketMatcher::Proto(SocketTransportProtocolMatcher::Tcp(TcpSocketMatcher::State(
                TcpStateMatcher::LISTEN,
            ))),
            &mut results,
        );
        assert_eq!(
            results,
            vec![TcpSocketDiagnostics {
                state_machine: netstack3_base::TcpSocketState::Listen,
                tuple: TcpSocketDiagnosticTuple::Bound {
                    src_addr: Some(I::TEST_ADDRS.local_ip.get()),
                    src_port: LOCAL_PORT_1,
                },
                cookie: listen_socket.socket_cookie(),
                marks: Marks::default(),
            }]
        );

        results.clear();
        api.bound_sockets_diagnostics(
            &IpSocketMatcher::Proto(SocketTransportProtocolMatcher::Tcp(TcpSocketMatcher::State(
                TcpStateMatcher::SYN_SENT,
            ))),
            &mut results,
        );
        assert_eq!(
            results,
            vec![TcpSocketDiagnostics {
                state_machine: netstack3_base::TcpSocketState::SynSent,
                tuple: TcpSocketDiagnosticTuple::Connected {
                    src_addr: I::TEST_ADDRS.local_ip.get(),
                    src_port: LOCAL_PORT_2,
                    dst_addr: I::TEST_ADDRS.remote_ip.get(),
                    dst_port: LOCAL_PORT_3,
                },
                cookie: syn_sent_socket.socket_cookie(),
                marks: Marks::default(),
            }]
        );

        results.clear();
        api.bound_sockets_diagnostics(
            &IpSocketMatcher::Proto(SocketTransportProtocolMatcher::Tcp(TcpSocketMatcher::State(
                TcpStateMatcher::ESTABLISHED,
            ))),
            &mut results,
        );
        assert_eq!(results, Vec::new());
    }
}
