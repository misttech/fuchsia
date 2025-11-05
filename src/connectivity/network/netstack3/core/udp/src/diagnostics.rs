// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::convert::Infallible as Never;
use core::num::NonZeroU16;

use net_types::ip::Ip;
use netstack3_base::socket::SocketCookie;
use netstack3_base::{
    Marks, Matcher, MaybeSocketTransportProperties, SocketTransportProtocolMatcher,
    UdpSocketProperties, UdpSocketState as UdpSocketMatcherState, WeakDeviceIdentifier,
};
use netstack3_datagram::{
    DatagramSocketDiagnosticsSpec, IpExt, SocketInfo, SocketState as DatagramSocketState,
};

use crate::internal::base::{Udp, UdpBindingsTypes, UdpSocketId, UdpSocketState};

/// Publicly-accessible diagnostic information about UDP sockets.
//
// The reason this isn't on the datagram API is that we don't have plans to
// support other datagram socket types at this time.
#[derive(Debug)]
#[cfg_attr(any(test, feature = "testutils"), derive(PartialEq, Eq))]
#[allow(missing_docs)]
pub struct UdpSocketDiagnostics<I: Ip> {
    pub state: UdpSocketDiagnosticTuple<I>,
    pub cookie: SocketCookie,
    pub marks: Marks,
}

/// UDP socket tuple information for diagnostics.
#[derive(Debug)]
#[cfg_attr(any(test, feature = "testutils"), derive(PartialEq, Eq))]
#[allow(missing_docs)]
pub enum UdpSocketDiagnosticTuple<I: Ip> {
    Bound { src_addr: Option<I::Addr>, src_port: NonZeroU16 },
    Connected { src_addr: I::Addr, src_port: NonZeroU16, dst_addr: I::Addr, dst_port: u16 },
}

impl<I: Ip> UdpSocketDiagnosticTuple<I> {
    /// Returns the source address of the socket.
    pub fn src_addr(&self) -> Option<I::Addr> {
        match self {
            Self::Bound { src_addr, src_port: _ } => *src_addr,
            Self::Connected { src_addr, src_port: _, dst_addr: _, dst_port: _ } => Some(*src_addr),
        }
    }

    /// Returns the source port of the socket.
    pub fn src_port(&self) -> Option<NonZeroU16> {
        match self {
            Self::Bound { src_addr: _, src_port }
            | Self::Connected { src_addr: _, src_port, dst_addr: _, dst_port: _ } => {
                Some(*src_port)
            }
        }
    }

    /// Returns the destination address of the socket.
    pub fn dst_addr(&self) -> Option<I::Addr> {
        match self {
            Self::Bound { src_addr: _, src_port: _ } => None,
            Self::Connected { src_addr: _, src_port: _, dst_addr, dst_port: _ } => Some(*dst_addr),
        }
    }

    /// Returns the destination port of the socket.
    pub fn dst_port(&self) -> Option<u16> {
        match self {
            Self::Bound { src_addr: _, src_port: _ } => None,
            Self::Connected { src_addr: _, src_port: _, dst_addr: _, dst_port } => Some(*dst_port),
        }
    }
}

/// A wrapper around [`UdpSocketState`], which is defined in
/// `netstack3_datagram`, to allow implementing traits on it.
pub struct UdpTransportProtocolDiagnosticsProperties<'a, I, D, BT>(&'a UdpSocketState<I, D, BT>)
where
    I: IpExt,
    D: WeakDeviceIdentifier,
    BT: UdpBindingsTypes;

impl<I, D, BT> MaybeSocketTransportProperties
    for UdpTransportProtocolDiagnosticsProperties<'_, I, D, BT>
where
    I: IpExt,
    D: WeakDeviceIdentifier,
    BT: UdpBindingsTypes,
{
    type TcpProps<'a>
        = Never
    where
        Self: 'a;

    type UdpProps<'a>
        = Self
    where
        Self: 'a;

    fn tcp_socket_properties(&self) -> Option<&Self::TcpProps<'_>> {
        None
    }

    fn udp_socket_properties(&self) -> Option<&Self::UdpProps<'_>> {
        Some(self)
    }
}

impl<I, D, BT> UdpSocketProperties for UdpTransportProtocolDiagnosticsProperties<'_, I, D, BT>
where
    I: IpExt,
    D: WeakDeviceIdentifier,
    BT: UdpBindingsTypes,
{
    fn src_port_matches(&self, matcher: &netstack3_base::PortMatcher) -> bool {
        let Self(udp_state) = self;
        matcher.required_matches(udp_state.local_identifier().map(|p| p.get()).as_ref())
    }

    fn dst_port_matches(&self, matcher: &netstack3_base::PortMatcher) -> bool {
        let Self(udp_state) = self;
        matcher.required_matches(udp_state.remote_identifier().as_ref())
    }

    fn state_matches(&self, matcher: &netstack3_base::UdpStateMatcher) -> bool {
        let Self(udp_state) = self;
        matcher.matches(match udp_state.to_socket_info() {
            SocketInfo::Unbound => return false,
            SocketInfo::Listener(_) => &UdpSocketMatcherState::Bound,
            SocketInfo::Connected(_) => &UdpSocketMatcherState::Connected,
        })
    }
}

impl<BT: UdpBindingsTypes> DatagramSocketDiagnosticsSpec for Udp<BT> {
    type DeviceClass = BT::DeviceClass;

    fn transport_protocol_matches<I: IpExt, D: WeakDeviceIdentifier>(
        state: &DatagramSocketState<I, D, Udp<BT>>,
        matcher: &SocketTransportProtocolMatcher,
    ) -> bool {
        matcher.matches(&UdpTransportProtocolDiagnosticsProperties(state))
    }

    fn cookie_matches<I: IpExt, D: WeakDeviceIdentifier>(
        id: &UdpSocketId<I, D, BT>,
        matcher: &netstack3_base::SocketCookieMatcher,
    ) -> bool {
        matcher.matches(&id.socket_cookie().export_value())
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::num::NonZeroU16;

    use ip_test_macro::ip_test;
    use net_types::ip::Subnet;
    use net_types::{Witness as _, ZonedAddr};
    use netstack3_base::testutil::FakeDeviceId;
    use netstack3_base::{
        AddressMatcher, AddressMatcherEither, AddressMatcherType, BoundInterfaceMatcher,
        InterfaceMatcher, IpSocketMatcher, Mark, MarkDomain, MarkMatcher, PortMatcher,
        SocketCookieMatcher, SocketTransportProtocolMatcher, SubnetMatcher, TcpSocketMatcher,
        UdpSocketMatcher, UdpStateMatcher,
    };

    use crate::internal::base::testutils::{FakeUdpCoreCtx, TestIpExt, UdpFakeDeviceCtx};
    use crate::internal::base::{UdpApi, UdpRemotePort};

    use super::*;

    const LOCAL_PORT_1: NonZeroU16 = NonZeroU16::new(1234).unwrap();
    const LOCAL_PORT_2: NonZeroU16 = NonZeroU16::new(5678).unwrap();
    const LOCAL_PORT_3: NonZeroU16 = NonZeroU16::new(4321).unwrap();

    const REMOTE_PORT_1: NonZeroU16 = NonZeroU16::new(100).unwrap();
    const REMOTE_PORT_2: NonZeroU16 = NonZeroU16::new(200).unwrap();

    const MARK: u32 = 0x10;
    const MARK_MASK: u32 = !0;

    #[ip_test(I)]
    fn diagnostics_match_ip_version<I: TestIpExt>() {
        let mut ctx = UdpFakeDeviceCtx::with_core_ctx(FakeUdpCoreCtx::new_fake_device::<I>());
        let mut api = UdpApi::<I, _>::new(ctx.as_mut());

        let socket = api.create();
        api.listen(&socket, None, Some(LOCAL_PORT_1)).expect("listen should succeed");

        let mut results = Vec::new();
        api.bound_sockets_diagnostics(&IpSocketMatcher::Family(I::VERSION), &mut results);
        assert_eq!(
            results,
            vec![UdpSocketDiagnostics {
                state: UdpSocketDiagnosticTuple::Bound { src_addr: None, src_port: LOCAL_PORT_1 },
                cookie: socket.socket_cookie(),
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
    fn diagnostics_match_src_addr<I: TestIpExt>() {
        let mut ctx = UdpFakeDeviceCtx::with_core_ctx(FakeUdpCoreCtx::new_fake_device::<I>());
        let mut api = UdpApi::<I, _>::new(ctx.as_mut());

        let socket = api.create();
        api.listen(&socket, Some(ZonedAddr::Unzoned(I::TEST_ADDRS.local_ip)), Some(LOCAL_PORT_1))
            .expect("listen should succeed");

        let mut results = Vec::new();
        let matcher = I::map_ip_in(
            I::TEST_ADDRS.subnet,
            |subnet| {
                AddressMatcherEither::V4(AddressMatcher {
                    matcher: AddressMatcherType::Subnet(SubnetMatcher(subnet)),
                    invert: false,
                })
            },
            |subnet| {
                AddressMatcherEither::V6(AddressMatcher {
                    matcher: AddressMatcherType::Subnet(SubnetMatcher(subnet)),
                    invert: false,
                })
            },
        );
        api.bound_sockets_diagnostics(&IpSocketMatcher::SrcAddr(matcher), &mut results);
        assert_eq!(
            results,
            vec![UdpSocketDiagnostics {
                state: UdpSocketDiagnosticTuple::Bound {
                    src_addr: Some(I::TEST_ADDRS.local_ip.get()),
                    src_port: LOCAL_PORT_1
                },
                cookie: socket.socket_cookie(),
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
    fn diagnostics_match_dst_addr<I: TestIpExt>() {
        let mut ctx = UdpFakeDeviceCtx::with_core_ctx(FakeUdpCoreCtx::new_fake_device::<I>());
        let mut api = UdpApi::<I, _>::new(ctx.as_mut());

        let socket = api.create();
        api.listen(&socket, Some(ZonedAddr::Unzoned(I::TEST_ADDRS.local_ip)), Some(LOCAL_PORT_2))
            .expect("listen should succeed");
        api.connect(
            &socket,
            Some(ZonedAddr::Unzoned(I::TEST_ADDRS.remote_ip)),
            UdpRemotePort::Set(LOCAL_PORT_1),
        )
        .expect("connect should succeed");

        let mut results = Vec::new();
        let matcher = I::map_ip_in(
            I::TEST_ADDRS.subnet,
            |subnet| {
                AddressMatcherEither::V4(AddressMatcher {
                    matcher: AddressMatcherType::Subnet(SubnetMatcher(subnet)),
                    invert: false,
                })
            },
            |subnet| {
                AddressMatcherEither::V6(AddressMatcher {
                    matcher: AddressMatcherType::Subnet(SubnetMatcher(subnet)),
                    invert: false,
                })
            },
        );
        api.bound_sockets_diagnostics(&IpSocketMatcher::DstAddr(matcher), &mut results);
        assert_eq!(
            results,
            vec![UdpSocketDiagnostics {
                state: UdpSocketDiagnosticTuple::Connected {
                    src_addr: I::TEST_ADDRS.local_ip.get(),
                    src_port: LOCAL_PORT_2,
                    dst_addr: I::TEST_ADDRS.remote_ip.get(),
                    dst_port: LOCAL_PORT_1.get(),
                },
                cookie: socket.socket_cookie(),
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
    fn diagnostics_match_proto<I: TestIpExt>() {
        let mut ctx = UdpFakeDeviceCtx::with_core_ctx(FakeUdpCoreCtx::new_fake_device::<I>());
        let mut api = UdpApi::<I, _>::new(ctx.as_mut());

        let socket = api.create();
        api.listen(&socket, None, Some(LOCAL_PORT_1)).expect("listen should succeed");

        let mut results = Vec::new();
        api.bound_sockets_diagnostics(
            &IpSocketMatcher::Proto(SocketTransportProtocolMatcher::Udp(UdpSocketMatcher::Empty)),
            &mut results,
        );
        assert_eq!(
            results,
            vec![UdpSocketDiagnostics {
                state: UdpSocketDiagnosticTuple::Bound { src_addr: None, src_port: LOCAL_PORT_1 },
                cookie: socket.socket_cookie(),
                marks: Marks::default(),
            }]
        );

        results.clear();
        api.bound_sockets_diagnostics(
            &IpSocketMatcher::Proto(SocketTransportProtocolMatcher::Tcp(TcpSocketMatcher::Empty)),
            &mut results,
        );
        assert_eq!(results, Vec::new());
    }

    #[ip_test(I)]
    fn diagnostics_match_src_port<I: TestIpExt>() {
        let mut ctx = UdpFakeDeviceCtx::with_core_ctx(FakeUdpCoreCtx::new_fake_device::<I>());
        let mut api = UdpApi::<I, _>::new(ctx.as_mut());

        let socket = api.create();
        api.listen(&socket, Some(ZonedAddr::Unzoned(I::TEST_ADDRS.local_ip)), Some(LOCAL_PORT_1))
            .expect("listen should succeed");

        let mut results = Vec::new();
        api.bound_sockets_diagnostics(
            &IpSocketMatcher::Proto(SocketTransportProtocolMatcher::Udp(
                UdpSocketMatcher::SrcPort(PortMatcher {
                    range: LOCAL_PORT_1.get()..=LOCAL_PORT_1.get(),
                    invert: false,
                }),
            )),
            &mut results,
        );
        assert_eq!(
            results,
            vec![UdpSocketDiagnostics {
                state: UdpSocketDiagnosticTuple::Bound {
                    src_addr: Some(I::TEST_ADDRS.local_ip.get()),
                    src_port: LOCAL_PORT_1
                },
                cookie: socket.socket_cookie(),
                marks: Marks::default(),
            }]
        );

        results.clear();
        api.bound_sockets_diagnostics(
            &IpSocketMatcher::Proto(SocketTransportProtocolMatcher::Udp(
                UdpSocketMatcher::SrcPort(PortMatcher {
                    range: (LOCAL_PORT_1.get() + 1)..=(LOCAL_PORT_1.get() + 1),
                    invert: false,
                }),
            )),
            &mut results,
        );
        assert_eq!(results, Vec::new());
    }

    #[ip_test(I)]
    fn diagnostics_match_dst_port<I: TestIpExt>() {
        let mut ctx = UdpFakeDeviceCtx::with_core_ctx(FakeUdpCoreCtx::new_fake_device::<I>());
        let mut api = UdpApi::<I, _>::new(ctx.as_mut());

        let socket = api.create();
        api.listen(&socket, Some(ZonedAddr::Unzoned(I::TEST_ADDRS.local_ip)), Some(LOCAL_PORT_2))
            .expect("listen should succeed");
        api.connect(
            &socket,
            Some(ZonedAddr::Unzoned(I::TEST_ADDRS.remote_ip)),
            UdpRemotePort::Set(LOCAL_PORT_1),
        )
        .expect("connect should succeed");

        let mut results = Vec::new();
        api.bound_sockets_diagnostics(
            &IpSocketMatcher::Proto(SocketTransportProtocolMatcher::Udp(
                UdpSocketMatcher::DstPort(PortMatcher {
                    range: LOCAL_PORT_1.get()..=LOCAL_PORT_1.get(),
                    invert: false,
                }),
            )),
            &mut results,
        );
        assert_eq!(
            results,
            vec![UdpSocketDiagnostics {
                state: UdpSocketDiagnosticTuple::Connected {
                    src_addr: I::TEST_ADDRS.local_ip.get(),
                    src_port: LOCAL_PORT_2,
                    dst_addr: I::TEST_ADDRS.remote_ip.get(),
                    dst_port: LOCAL_PORT_1.get(),
                },
                cookie: socket.socket_cookie(),
                marks: Marks::default(),
            }]
        );

        results.clear();
        api.bound_sockets_diagnostics(
            &IpSocketMatcher::Proto(SocketTransportProtocolMatcher::Udp(
                UdpSocketMatcher::DstPort(PortMatcher {
                    range: (LOCAL_PORT_1.get() + 1)..=(LOCAL_PORT_1.get() + 1),
                    invert: false,
                }),
            )),
            &mut results,
        );
        assert_eq!(results, Vec::new());
    }

    #[ip_test(I)]
    fn diagnostics_match_state<I: TestIpExt>() {
        let mut ctx = UdpFakeDeviceCtx::with_core_ctx(FakeUdpCoreCtx::new_fake_device::<I>());
        let mut api = UdpApi::<I, _>::new(ctx.as_mut());

        let socket_1 = api.create();
        api.listen(&socket_1, Some(ZonedAddr::Unzoned(I::TEST_ADDRS.local_ip)), Some(LOCAL_PORT_1))
            .expect("listen should succeed");

        let socket_2 = api.create();
        api.listen(&socket_2, Some(ZonedAddr::Unzoned(I::TEST_ADDRS.local_ip)), Some(LOCAL_PORT_2))
            .expect("listen should succeed");
        api.connect(
            &socket_2,
            Some(ZonedAddr::Unzoned(I::TEST_ADDRS.remote_ip)),
            UdpRemotePort::Set(LOCAL_PORT_3),
        )
        .expect("connect should succeed");

        let mut results = Vec::new();
        api.bound_sockets_diagnostics(
            &IpSocketMatcher::Proto(SocketTransportProtocolMatcher::Udp(UdpSocketMatcher::State(
                UdpStateMatcher::BOUND,
            ))),
            &mut results,
        );
        assert_eq!(
            results,
            vec![UdpSocketDiagnostics {
                state: UdpSocketDiagnosticTuple::Bound {
                    src_addr: Some(I::TEST_ADDRS.local_ip.get()),
                    src_port: LOCAL_PORT_1
                },
                cookie: socket_1.socket_cookie(),
                marks: Marks::default(),
            }]
        );

        results.clear();
        api.bound_sockets_diagnostics(
            &IpSocketMatcher::Proto(SocketTransportProtocolMatcher::Udp(UdpSocketMatcher::State(
                UdpStateMatcher::CONNECTED,
            ))),
            &mut results,
        );
        assert_eq!(
            results,
            vec![UdpSocketDiagnostics {
                state: UdpSocketDiagnosticTuple::Connected {
                    src_addr: I::TEST_ADDRS.local_ip.get(),
                    src_port: LOCAL_PORT_2,
                    dst_addr: I::TEST_ADDRS.remote_ip.get(),
                    dst_port: LOCAL_PORT_3.get(),
                },
                cookie: socket_2.socket_cookie(),
                marks: Marks::default(),
            }]
        );
    }

    #[ip_test(I)]
    fn diagnostics_match_device<I: TestIpExt>() {
        let mut ctx = UdpFakeDeviceCtx::with_core_ctx(FakeUdpCoreCtx::new_fake_device::<I>());
        let mut api = UdpApi::<I, _>::new(ctx.as_mut());

        let socket = api.create();
        api.set_device(&socket, Some(&FakeDeviceId)).expect("set device should succeed");
        api.listen(&socket, None, Some(LOCAL_PORT_1)).expect("listen should succeed");

        let mut results = Vec::new();
        api.bound_sockets_diagnostics(
            &IpSocketMatcher::BoundInterface(BoundInterfaceMatcher::Bound(InterfaceMatcher::Name(
                FakeDeviceId::FAKE_NAME.to_string(),
            ))),
            &mut results,
        );
        assert_eq!(
            results,
            vec![UdpSocketDiagnostics {
                state: UdpSocketDiagnosticTuple::Bound { src_addr: None, src_port: LOCAL_PORT_1 },
                cookie: socket.socket_cookie(),
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
    fn diagnostics_match_cookie<I: TestIpExt>() {
        let mut ctx = UdpFakeDeviceCtx::with_core_ctx(FakeUdpCoreCtx::new_fake_device::<I>());
        let mut api = UdpApi::<I, _>::new(ctx.as_mut());

        let socket = api.create();
        api.listen(&socket, None, Some(LOCAL_PORT_1)).expect("listen should succeed");

        let mut results = Vec::new();
        api.bound_sockets_diagnostics(
            &IpSocketMatcher::Cookie(SocketCookieMatcher {
                cookie: socket.socket_cookie().export_value(),
                invert: false,
            }),
            &mut results,
        );
        assert_eq!(
            results,
            vec![UdpSocketDiagnostics {
                state: UdpSocketDiagnosticTuple::Bound { src_addr: None, src_port: LOCAL_PORT_1 },
                cookie: socket.socket_cookie(),
                marks: Marks::default(),
            }]
        );

        results.clear();
        api.bound_sockets_diagnostics(
            &IpSocketMatcher::Cookie(SocketCookieMatcher {
                cookie: socket.socket_cookie().export_value() + 1,
                invert: false,
            }),
            &mut results,
        );
        assert_eq!(results, Vec::new());
    }

    #[ip_test(I)]
    #[test_case::test_case(MarkDomain::Mark1; "mark_1")]
    #[test_case::test_case(MarkDomain::Mark2; "mark_2")]
    fn diagnostics_match_mark<I: TestIpExt>(domain: MarkDomain) {
        let mut ctx = UdpFakeDeviceCtx::with_core_ctx(FakeUdpCoreCtx::new_fake_device::<I>());
        let mut api = UdpApi::<I, _>::new(ctx.as_mut());

        let socket = api.create();
        api.listen(&socket, None, Some(LOCAL_PORT_1)).expect("listen should succeed");

        api.set_mark(&socket, domain, Mark(Some(MARK)));

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
            vec![UdpSocketDiagnostics {
                state: UdpSocketDiagnosticTuple::Bound { src_addr: None, src_port: LOCAL_PORT_1 },
                cookie: socket.socket_cookie(),
                marks: netstack3_base::MarkStorage::new([(domain, MARK)]),
            }]
        );

        results.clear();
        api.bound_sockets_diagnostics(&matcher(MARK + 1), &mut results);
        assert_eq!(results, Vec::new());
    }

    /// Create three sockets, two of which target the same remote port, and make
    /// sure that multiple matching sockets are returned.
    #[ip_test(I)]
    fn diagnostics_match_multiple<I: TestIpExt>() {
        let mut ctx = UdpFakeDeviceCtx::with_core_ctx(FakeUdpCoreCtx::new_fake_device::<I>());
        let mut api = UdpApi::<I, _>::new(ctx.as_mut());

        let socket_1 = api.create();
        api.listen(&socket_1, Some(ZonedAddr::Unzoned(I::TEST_ADDRS.local_ip)), Some(LOCAL_PORT_1))
            .expect("listen should succeed");
        api.connect(
            &socket_1,
            Some(ZonedAddr::Unzoned(I::TEST_ADDRS.remote_ip)),
            UdpRemotePort::Set(REMOTE_PORT_1),
        )
        .expect("connect should succeed");

        let socket_2 = api.create();
        api.listen(&socket_2, Some(ZonedAddr::Unzoned(I::TEST_ADDRS.local_ip)), Some(LOCAL_PORT_2))
            .expect("listen should succeed");
        api.connect(
            &socket_2,
            Some(ZonedAddr::Unzoned(I::TEST_ADDRS.remote_ip)),
            UdpRemotePort::Set(REMOTE_PORT_1),
        )
        .expect("connect should succeed");

        let socket_3 = api.create();
        api.listen(&socket_3, Some(ZonedAddr::Unzoned(I::TEST_ADDRS.local_ip)), Some(LOCAL_PORT_3))
            .expect("listen should succeed");
        api.connect(
            &socket_3,
            Some(ZonedAddr::Unzoned(I::TEST_ADDRS.remote_ip)),
            UdpRemotePort::Set(REMOTE_PORT_2),
        )
        .expect("connect should succeed");

        let mut results = Vec::new();
        api.bound_sockets_diagnostics(
            &IpSocketMatcher::Proto(SocketTransportProtocolMatcher::Udp(
                UdpSocketMatcher::DstPort(PortMatcher {
                    range: REMOTE_PORT_1.get()..=REMOTE_PORT_1.get(),
                    invert: false,
                }),
            )),
            &mut results,
        );

        results.sort_by(|a, b| a.cookie.cmp(&b.cookie));
        let mut expected = vec![
            UdpSocketDiagnostics {
                state: UdpSocketDiagnosticTuple::Connected {
                    src_addr: I::TEST_ADDRS.local_ip.get(),
                    src_port: LOCAL_PORT_1,
                    dst_addr: I::TEST_ADDRS.remote_ip.get(),
                    dst_port: REMOTE_PORT_1.get(),
                },
                cookie: socket_1.socket_cookie(),
                marks: Marks::default(),
            },
            UdpSocketDiagnostics {
                state: UdpSocketDiagnosticTuple::Connected {
                    src_addr: I::TEST_ADDRS.local_ip.get(),
                    src_port: LOCAL_PORT_2,
                    dst_addr: I::TEST_ADDRS.remote_ip.get(),
                    dst_port: REMOTE_PORT_1.get(),
                },
                cookie: socket_2.socket_cookie(),
                marks: Marks::default(),
            },
        ];
        expected.sort_by(|a, b| a.cookie.cmp(&b.cookie));
        assert_eq!(results, expected);

        results.clear();
        api.bound_sockets_diagnostics(
            &IpSocketMatcher::Proto(SocketTransportProtocolMatcher::Udp(
                UdpSocketMatcher::DstPort(PortMatcher {
                    range: REMOTE_PORT_2.get()..=REMOTE_PORT_2.get(),
                    invert: false,
                }),
            )),
            &mut results,
        );
        assert_eq!(
            results,
            vec![UdpSocketDiagnostics {
                state: UdpSocketDiagnosticTuple::Connected {
                    src_addr: I::TEST_ADDRS.local_ip.get(),
                    src_port: LOCAL_PORT_3,
                    dst_addr: I::TEST_ADDRS.remote_ip.get(),
                    dst_port: REMOTE_PORT_2.get(),
                },
                cookie: socket_3.socket_cookie(),
                marks: Marks::default(),
            }]
        );
    }
}
