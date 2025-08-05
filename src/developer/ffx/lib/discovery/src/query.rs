// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use crate::desc::Description;
use crate::DiscoverySources;
use addr::{TargetAddr, TargetIpAddr};
use fidl_fuchsia_developer_ffx::{
    TargetAddrInfo, TargetInfo, TargetIpAddrInfo, TargetIpPort, TargetVSockNamespace,
};
use std::net::SocketAddr;

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum TargetInfoQuery {
    /// Attempts to match the nodename, falling back to serial (in that order).
    NodenameOrSerial(String),
    Serial(String),
    Addr(SocketAddr),
    /// Match a target which has a VSock address with the given CID.
    VSock(u32),
    /// Match a target which has a USB emulated VSock address with the given CID.
    Usb(u32),
    First,
}

fn address_matcher(ours: &SocketAddr, theirs: &mut SocketAddr, ssh_port: u16) -> bool {
    // Use the SSH port if the target address' port is 0
    if theirs.port() == 0 {
        theirs.set_port(ssh_port)
    }

    // Clear the target address' port if the query has no port
    if ours.port() == 0 {
        theirs.set_port(0)
    }

    // Clear the target address' scope if the query has no scope
    if let (SocketAddr::V6(ours), SocketAddr::V6(theirs)) = (ours, &mut *theirs) {
        if ours.scope_id() == 0 {
            theirs.set_scope_id(0)
        }
    }

    theirs == ours
}

impl TargetInfoQuery {
    pub fn is_query_on_identity(&self) -> bool {
        matches!(self, TargetInfoQuery::NodenameOrSerial(..) | TargetInfoQuery::First)
    }

    pub fn is_query_on_address(&self) -> bool {
        matches!(self, TargetInfoQuery::Addr(..))
    }

    pub fn match_description(&self, t: &Description) -> bool {
        log::debug!("Matching description {t:?} against query {self:?}");
        match self {
            Self::NodenameOrSerial(arg) => {
                if let Some(ref nodename) = t.nodename {
                    if nodename == arg {
                        return true;
                    }
                }
                if let Some(ref serial) = t.serial {
                    if serial == arg {
                        return true;
                    }
                }
                false
            }
            Self::Serial(arg) => {
                if let Some(ref serial) = t.serial {
                    if serial == arg {
                        return true;
                    }
                }
                false
            }
            Self::Addr(addr) => t
                .addresses
                .iter()
                .filter_map(|x| TargetIpAddr::try_from(x).ok())
                .any(|a| address_matcher(addr, &mut a.into(), t.ssh_port.unwrap_or(22))),
            Self::VSock(cid) => t.addresses.iter().filter_map(|x| x.cid_vsock()).any(|x| x == *cid),
            Self::Usb(cid) => t.addresses.iter().filter_map(|x| x.cid_usb()).any(|x| x == *cid),
            Self::First => true,
        }
    }

    pub fn match_target_info(&self, t: &TargetInfo) -> bool {
        match self {
            Self::NodenameOrSerial(arg) => {
                if let Some(ref nodename) = t.nodename {
                    if nodename == arg {
                        return true;
                    }
                }
                if let Some(ref serial) = t.serial_number {
                    if serial == arg {
                        return true;
                    }
                }
                false
            }
            Self::Serial(arg) => {
                if let Some(ref serial) = t.serial_number {
                    if serial == arg {
                        return true;
                    }
                }
                false
            }
            Self::Addr(addr) => t
                .addresses
                .as_ref()
                .map(|addresses| {
                    addresses.iter().any(|a| {
                        let Ok(a) = TargetIpAddr::try_from(TargetAddr::from(a)) else {
                            return false;
                        };
                        let ssh_port = if let Some(TargetIpAddrInfo::IpPort(TargetIpPort {
                            port: tp,
                            ..
                        })) = t.ssh_address
                        {
                            tp
                        } else {
                            22
                        };
                        address_matcher(addr, &mut a.into(), ssh_port)
                    })
                })
                .unwrap_or(false),
            Self::VSock(cid) => t
                .addresses
                .as_ref()
                .map(|addresses| {
                    addresses.iter().any(|a| {
                        if let TargetAddrInfo::Vsock(a) = a {
                            a.cid == *cid && a.namespace == TargetVSockNamespace::Vsock
                        } else {
                            false
                        }
                    })
                })
                .unwrap_or(false),
            Self::Usb(cid) => t
                .addresses
                .as_ref()
                .map(|addresses| {
                    addresses.iter().any(|a| {
                        if let TargetAddrInfo::Vsock(a) = a {
                            a.cid == *cid && a.namespace == TargetVSockNamespace::Usb
                        } else {
                            false
                        }
                    })
                })
                .unwrap_or(false),
            Self::First => true,
        }
    }

    /// Return the invoke discovery on to resolve this query
    pub fn discovery_sources(&self) -> DiscoverySources {
        match self {
            TargetInfoQuery::Addr(_) => {
                DiscoverySources::MDNS | DiscoverySources::MANUAL | DiscoverySources::EMULATOR
            }
            TargetInfoQuery::Serial(_) => DiscoverySources::USB,
            _ => {
                DiscoverySources::MDNS
                    | DiscoverySources::MANUAL
                    | DiscoverySources::EMULATOR
                    | DiscoverySources::USB
            }
        }
    }
}

impl<T> From<Option<T>> for TargetInfoQuery
where
    T: Into<TargetInfoQuery>,
{
    fn from(o: Option<T>) -> Self {
        o.map(Into::into).unwrap_or(Self::First)
    }
}

impl From<TargetInfoQuery> for Option<String> {
    fn from(t: TargetInfoQuery) -> Self {
        match t {
            TargetInfoQuery::First => None,
            e @ _ => Some(e.into()),
        }
    }
}

impl From<&str> for TargetInfoQuery {
    fn from(s: &str) -> Self {
        String::from(s).into()
    }
}

impl From<String> for TargetInfoQuery {
    /// If the string can be parsed as some kind of IP address, will attempt to
    /// match based on that, else fall back to the nodename or serial matches.
    fn from(s: String) -> Self {
        if s == "" {
            return Self::First;
        }
        if s.starts_with("serial:") {
            // "serial:" is used when we _know_ something is a serial number,
            // and want to to preserve that across the client/daemon boundary
            return Self::Serial(String::from(&s[7..]));
        }
        if s.starts_with("usb:cid:") {
            if let Ok(cid) = s["usb:cid:".len()..].parse() {
                return Self::Usb(cid);
            }
        }
        if s.starts_with("vsock:cid:") {
            if let Ok(cid) = s["vsock:cid:".len()..].parse() {
                return Self::VSock(cid);
            }
        }

        let (addr, scope, port) = match netext::parse_address_parts(s.as_str()) {
            Ok(r) => r,
            Err(e) => {
                log::trace!(
                    "Failed to parse address from '{s}'. Interpreting as nodename: {:?}",
                    e
                );
                return Self::NodenameOrSerial(s);
            }
        };
        // If no such interface exists, just return 0 for a best effort search.
        // This does mean it might be possible to include arbitrary inaccurate scope names for
        // looking up a target, however (like `fe80::1%nonsense`).
        let scope = scope.map(|s| netext::get_verified_scope_id(s).unwrap_or(0)).unwrap_or(0);
        let addr = TargetIpAddr::new(addr, scope, port.unwrap_or(0)).into();
        Self::Addr(addr)
    }
}

impl From<TargetInfoQuery> for String {
    fn from(t: TargetInfoQuery) -> Self {
        String::from(&t)
    }
}

impl From<&TargetInfoQuery> for String {
    fn from(t: &TargetInfoQuery) -> Self {
        match t {
            TargetInfoQuery::First => {
                format!("")
            }
            TargetInfoQuery::Serial(s) => {
                format!("serial:{}", s)
            }
            TargetInfoQuery::Usb(cid) => {
                format!("usb:cid:{}", cid)
            }
            TargetInfoQuery::VSock(cid) => {
                format!("vsock:cid:{}", cid)
            }
            TargetInfoQuery::NodenameOrSerial(nnos) => {
                format!("{}", nnos)
            }
            TargetInfoQuery::Addr(addr) => {
                format!("{}", addr)
            }
        }
    }
}

impl From<TargetAddr> for TargetInfoQuery {
    fn from(t: TargetAddr) -> Self {
        match t {
            TargetAddr::Net(socket_addr) => Self::Addr(socket_addr),
            TargetAddr::VSockCtx(cid) => Self::VSock(cid),
            TargetAddr::UsbCtx(cid) => Self::Usb(cid),
        }
    }
}

/// Convert a TargetAddrInfo to a SocketAddr preserving the port number if
/// provided, otherwise the returned SocketAddr will have port number 0.
pub fn target_addr_info_to_socketaddr(tai: TargetIpAddrInfo) -> SocketAddr {
    let mut sa = SocketAddr::from(TargetIpAddr::from(&tai));
    // TODO(raggi): the port special case needed here indicates a general problem in our
    // addressing strategy that is worth reviewing.
    if let TargetIpAddrInfo::IpPort(ref ipp) = tai {
        sa.set_port(ipp.port)
    }
    sa
}

#[cfg(test)]
mod test {
    use super::*;
    use fidl_fuchsia_developer_ffx::{TargetIp, TargetVSockCtx};
    use fidl_fuchsia_net as net;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use test_case::test_case;

    #[test]
    fn test_discovery_sources() {
        let query = TargetInfoQuery::from("name");
        let sources = query.discovery_sources();
        assert_eq!(
            sources,
            DiscoverySources::MDNS
                | DiscoverySources::MANUAL
                | DiscoverySources::EMULATOR
                | DiscoverySources::USB
        );

        // IP Address shouldn't use USB source
        let query = TargetInfoQuery::from("1.2.3.4");
        let sources = query.discovery_sources();
        assert_eq!(
            sources,
            DiscoverySources::MDNS | DiscoverySources::MANUAL | DiscoverySources::EMULATOR
        );

        // Serial # should only use USB source
        let query = TargetInfoQuery::from("serial:abcdef");
        let sources = query.discovery_sources();
        assert_eq!(sources, DiscoverySources::USB);
    }

    #[test]
    fn test_serial_query() {
        let serial = "abcdef";
        let q = TargetInfoQuery::from(format!("serial:{serial}"));
        match q {
            TargetInfoQuery::Serial(s) if s == serial => {}
            _ => panic!("parsing of serial query failed"),
        }
    }

    #[test]
    fn test_vsock_query() {
        const CID: u32 = 3;
        let q = TargetInfoQuery::from(format!("vsock:cid:{CID}"));
        match q {
            TargetInfoQuery::VSock(cid) if cid == CID => {}
            _ => panic!("parsing of vsock query failed"),
        }

        assert!(q.match_description(&Description {
            addresses: vec![TargetAddr::VSockCtx(CID)],
            ..Default::default()
        }));
        assert!(q.match_target_info(&TargetInfo {
            addresses: Some(vec![TargetAddrInfo::Vsock(TargetVSockCtx {
                cid: CID,
                namespace: TargetVSockNamespace::Vsock
            })]),
            ..Default::default()
        }));
    }

    #[test]
    fn test_usb_query() {
        const CID: u32 = 3;
        let q = TargetInfoQuery::from(format!("usb:cid:{CID}"));
        match q {
            TargetInfoQuery::Usb(cid) if cid == CID => {}
            _ => panic!("parsing of serial query failed"),
        }

        assert!(q.match_description(&Description {
            addresses: vec![TargetAddr::UsbCtx(CID)],
            ..Default::default()
        }));
        assert!(q.match_target_info(&TargetInfo {
            addresses: Some(vec![TargetAddrInfo::Vsock(TargetVSockCtx {
                cid: CID,
                namespace: TargetVSockNamespace::Usb
            })]),
            ..Default::default()
        }));
    }

    #[test]
    fn test_target_addr_info_to_socketaddr() {
        let tai = TargetIpAddrInfo::IpPort(TargetIpPort {
            ip: net::IpAddress::Ipv4(net::Ipv4Address { addr: [127, 0, 0, 1] }),
            port: 8022,
            scope_id: 0,
        });

        let sa = "127.0.0.1:8022".parse::<SocketAddr>().unwrap();

        assert_eq!(target_addr_info_to_socketaddr(tai), sa);

        let tai = TargetIpAddrInfo::Ip(TargetIp {
            ip: net::IpAddress::Ipv4(net::Ipv4Address { addr: [127, 0, 0, 1] }),
            scope_id: 0,
        });

        let sa = "127.0.0.1:0".parse::<SocketAddr>().unwrap();

        assert_eq!(target_addr_info_to_socketaddr(tai), sa);

        let tai = TargetIpAddrInfo::IpPort(TargetIpPort {
            ip: net::IpAddress::Ipv6(net::Ipv6Address {
                addr: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
            }),
            port: 8022,
            scope_id: 0,
        });

        let sa = "[::1]:8022".parse::<SocketAddr>().unwrap();

        assert_eq!(target_addr_info_to_socketaddr(tai), sa);

        let tai = TargetIpAddrInfo::Ip(TargetIp {
            ip: net::IpAddress::Ipv6(net::Ipv6Address {
                addr: [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
            }),
            scope_id: 1,
        });

        let sa = "[fe80::1%1]:0".parse::<SocketAddr>().unwrap();

        assert_eq!(target_addr_info_to_socketaddr(tai), sa);

        let tai = TargetIpAddrInfo::IpPort(TargetIpPort {
            ip: net::IpAddress::Ipv6(net::Ipv6Address {
                addr: [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
            }),
            port: 8022,
            scope_id: 1,
        });

        let sa = "[fe80::1%1]:8022".parse::<SocketAddr>().unwrap();

        assert_eq!(target_addr_info_to_socketaddr(tai), sa);
    }

    #[test_case(
        "serial:123456";
        "Test Serial Number"
    )]
    #[test_case(
        "";
        "Test First"
    )]
    #[test_case(
        "usb:cid:16";
        "Test Usb Cid"
    )]
    #[test_case(
        "vsock:cid:12";
        "Test Vsock Cid"
    )]
    #[test_case(
        "tressoftheemeraldsea";
        "Test Nodename or serial"
    )]
    #[test_case(
        "192.168.1.1:8082";
        "Test Address"
    )]
    fn test_from_to_string_isomorphic(str_input: &str) {
        let tiq = TargetInfoQuery::from(str_input);
        let tiq_string = String::from(tiq);
        assert_eq!(tiq_string, str_input);
    }

    #[test_case(
        TargetInfoQuery::First,
        None;
        "Test First"
    )]
    #[test_case(
        TargetInfoQuery::NodenameOrSerial("tressoftheemeraldsea".to_string()),
        Some("tressoftheemeraldsea".to_string());
        "Test Nodename or serial"
    )]
    #[test_case(
        TargetInfoQuery::VSock(16),
        Some("vsock:cid:16".to_string());
        "Test Vsock Cid"
    )]
    #[test_case(
        TargetInfoQuery::Usb(12),
        Some("usb:cid:12".to_string());
        "Test Usb Cid"
    )]
    #[test_case(
        TargetInfoQuery::Serial("totallynothoid".to_string()),
        Some("serial:totallynothoid".to_string());
        "Test Serial"
    )]
    #[test_case(
        TargetInfoQuery::Addr(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),8082)),
        Some("192.168.1.1:8082".to_string());
        "Test Addr"
    )]
    fn test_into_option(query: TargetInfoQuery, want: Option<String>) {
        let got: Option<String> = query.into();
        assert_eq!(got, want);
    }
}
