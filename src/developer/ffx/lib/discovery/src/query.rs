// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use crate::desc::Description;
use crate::{DiscoverySources, TargetHandle};
use addr::{TargetAddr, TargetAddrParseError, TargetIpAddr};
use std::net::SocketAddr;

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum TargetInfoQuery {
    /// Attempts to match the nodename, falling back to ID (in that order).
    NodenameOrId(String),
    Id(String),
    Addr(SocketAddr),
    /// Match a target which has a VSock address with the given CID.
    VSock(u32),
    /// Match a target which has a USB emulated VSock address with the given CID.
    Usb(u32),
    /// Match a target which has a UART serial connection endpoint matching the given endpoint.
    Uart(String),
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
        matches!(self, TargetInfoQuery::NodenameOrId(..) | TargetInfoQuery::First)
    }

    pub fn is_query_on_address(&self) -> bool {
        matches!(self, TargetInfoQuery::Addr(..))
    }

    /// If the query already resolves to an address, return that TargetAddr
    pub fn get_target_addr(&self) -> Option<TargetAddr> {
        match self {
            Self::NodenameOrId(_) | Self::Id(_) | Self::First => None,
            Self::Addr(socket_addr) => Some(TargetAddr::Net(*socket_addr)),
            Self::VSock(id) => Some(TargetAddr::VSockCtx(*id)),
            Self::Usb(id) => Some(TargetAddr::UsbCtx(*id)),
            Self::Uart(endpoint) => Some(TargetAddr::Uart(endpoint.clone())),
        }
    }

    pub fn match_description(&self, t: &Description) -> bool {
        log::debug!("Matching description {t:?} against query {self:?}");
        match self {
            Self::NodenameOrId(arg) => {
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
            Self::Id(arg) => {
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
            Self::Uart(endpoint) => {
                t.addresses.iter().any(|addr| matches!(addr, TargetAddr::Uart(e) if e == endpoint))
            }
            Self::First => true,
        }
    }

    pub fn match_handle(&self, h: &TargetHandle) -> bool {
        let desc = Description::from(h);
        self.match_description(&desc)
    }

    /// Return the invoke discovery on to resolve this query
    pub fn discovery_sources(&self) -> DiscoverySources {
        match self {
            TargetInfoQuery::Addr(_) => {
                DiscoverySources::MDNS
                    | DiscoverySources::MANUAL
                    | DiscoverySources::EMULATOR
                    | DiscoverySources::GCE
            }
            TargetInfoQuery::Id(_) => DiscoverySources::USB_FASTBOOT,
            TargetInfoQuery::VSock(_) => DiscoverySources::EMULATOR,
            TargetInfoQuery::Usb(_) => DiscoverySources::USB_VSOCK,
            TargetInfoQuery::Uart(_) => DiscoverySources::UART,
            _ => {
                DiscoverySources::MDNS
                    | DiscoverySources::MANUAL
                    | DiscoverySources::EMULATOR
                    | DiscoverySources::USB_FASTBOOT
                    | DiscoverySources::USB_VSOCK
                    | DiscoverySources::GCE
                    | DiscoverySources::UART
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

impl TryFrom<&str> for TargetInfoQuery {
    type Error = crate::error::Error;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        String::from(s).try_into()
    }
}

impl TryFrom<String> for TargetInfoQuery {
    type Error = crate::error::Error;

    /// If the string can be parsed as some kind of IP address, will attempt to
    /// match based on that, else fall back to the nodename or ID matches.
    fn try_from(s: String) -> Result<Self, Self::Error> {
        if s.is_empty() {
            return Ok(Self::First);
        }
        if let Some(rest) = s.strip_prefix("id:") {
            return Ok(Self::Id(rest.to_string()));
        }
        if let Some(rest) = s.strip_prefix("serial:") {
            return Ok(Self::Id(rest.to_string()));
        }
        match s.parse::<TargetAddr>() {
            Ok(addr) => Ok(Self::from(addr)),
            Err(TargetAddrParseError::Net(_)) => {
                log::trace!("Failed to parse address from '{s}'. Interpreting as nodename");
                Ok(Self::NodenameOrId(s))
            }
            Err(e) => Err(crate::error::Error::ParseError(e.to_string())),
        }
    }
}

impl TryFrom<Option<String>> for TargetInfoQuery {
    type Error = crate::error::Error;

    fn try_from(o: Option<String>) -> Result<Self, Self::Error> {
        match o {
            Some(s) => TargetInfoQuery::try_from(s),
            None => Ok(TargetInfoQuery::First),
        }
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
            TargetInfoQuery::Id(s) => {
                format!("id:{}", s)
            }
            TargetInfoQuery::Usb(cid) => {
                format!("usb:cid:{}", cid)
            }
            TargetInfoQuery::VSock(cid) => {
                format!("vsock:cid:{}", cid)
            }
            TargetInfoQuery::Uart(endpoint) => {
                format!("uart:{}", endpoint)
            }
            TargetInfoQuery::NodenameOrId(nnos) => {
                format!("{}", nnos)
            }
            TargetInfoQuery::Addr(addr) => {
                format!("{}", addr)
            }
        }
    }
}

impl std::fmt::Display for TargetInfoQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", String::from(self))
    }
}

impl From<TargetAddr> for TargetInfoQuery {
    fn from(t: TargetAddr) -> Self {
        match t {
            TargetAddr::Net(socket_addr) => Self::Addr(socket_addr),
            TargetAddr::VSockCtx(cid) => Self::VSock(cid),
            TargetAddr::UsbCtx(cid) => Self::Usb(cid),
            TargetAddr::Uart(endpoint) => Self::Uart(endpoint),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use net_declare::std_socket_addr;
    use test_case::test_case;

    #[test]
    fn test_discovery_sources() {
        let query = TargetInfoQuery::try_from("name").unwrap();
        let sources = query.discovery_sources();
        assert_eq!(
            sources,
            DiscoverySources::MDNS
                | DiscoverySources::MANUAL
                | DiscoverySources::EMULATOR
                | DiscoverySources::USB_FASTBOOT
                | DiscoverySources::USB_VSOCK
                | DiscoverySources::GCE
                | DiscoverySources::UART
        );

        // Uart query should only use UART source
        let query = TargetInfoQuery::try_from("uart:/path/to/socket").unwrap();
        let sources = query.discovery_sources();
        assert_eq!(sources, DiscoverySources::UART);

        // IP Address shouldn't use USB source
        let query = TargetInfoQuery::try_from("1.2.3.4").unwrap();
        let sources = query.discovery_sources();
        assert_eq!(
            sources,
            DiscoverySources::MDNS
                | DiscoverySources::MANUAL
                | DiscoverySources::EMULATOR
                | DiscoverySources::GCE
        );

        // ID should only use USB source
        let query = TargetInfoQuery::try_from("id:abcdef").unwrap();
        let sources = query.discovery_sources();
        assert_eq!(sources, DiscoverySources::USB_FASTBOOT);
    }

    #[test]
    fn test_id_query() {
        let serial = "abcdef";
        let q = TargetInfoQuery::try_from(format!("id:{serial}")).unwrap();
        match q {
            TargetInfoQuery::Id(s) if s == serial => {}
            _ => panic!("parsing of ID query failed"),
        }
    }

    #[test]
    fn test_serial_query() {
        let serial = "abcdef";
        let q = TargetInfoQuery::try_from(format!("serial:{serial}")).unwrap();
        match q {
            TargetInfoQuery::Id(s) if s == serial => {}
            _ => panic!("parsing of serial query failed"),
        }
    }

    #[test]
    fn test_vsock_query() {
        const CID: u32 = 3;
        let q = TargetInfoQuery::try_from(format!("vsock:cid:{CID}")).unwrap();
        match q {
            TargetInfoQuery::VSock(cid) if cid == CID => {}
            _ => panic!("parsing of vsock query failed"),
        }

        assert!(q.match_description(&Description {
            addresses: vec![TargetAddr::VSockCtx(CID)],
            ..Default::default()
        }));
    }

    #[test]
    fn test_usb_query() {
        const CID: u32 = 3;
        let q = TargetInfoQuery::try_from(format!("usb:cid:{CID}")).unwrap();
        match q {
            TargetInfoQuery::Usb(cid) if cid == CID => {}
            _ => panic!("parsing of serial query failed"),
        }

        assert!(q.match_description(&Description {
            addresses: vec![TargetAddr::UsbCtx(CID)],
            ..Default::default()
        }));
    }

    #[test_case(
        TargetInfoQuery::Addr("127.0.0.1:8022".parse().unwrap()),
        Some(TargetAddr::Net("127.0.0.1:8022".parse().unwrap()));
        "Test Addr"
    )]
    #[test_case(
        TargetInfoQuery::VSock(123),
        Some(TargetAddr::VSockCtx(123));
        "Test VSock"
    )]
    #[test_case(
        TargetInfoQuery::Usb(456),
        Some(TargetAddr::UsbCtx(456));
        "Test Usb"
    )]
    #[test_case(
        TargetInfoQuery::Uart("/dev/tty".to_string()),
        Some(TargetAddr::Uart("/dev/tty".to_string()));
        "Test Uart"
    )]
    #[test_case(
        TargetInfoQuery::First,
        None;
        "Test First"
    )]
    #[test_case(
        TargetInfoQuery::NodenameOrId("foo".to_string()),
        None;
        "Test Nodename"
    )]
    fn test_is_target_addr(query: TargetInfoQuery, want: Option<TargetAddr>) {
        assert_eq!(query.get_target_addr(), want);
    }

    #[test_case(
        "id:123456";
        "Test ID"
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
        "uart:/dev/ttyUSB0";
        "Test Uart"
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
        let tiq = TargetInfoQuery::try_from(str_input).unwrap();
        let tiq_string = String::from(tiq);
        assert_eq!(tiq_string, str_input);
    }

    #[test_case(
        TargetInfoQuery::First,
        None;
        "Test First"
    )]
    #[test_case(
        TargetInfoQuery::NodenameOrId("tressoftheemeraldsea".to_string()),
        Some("tressoftheemeraldsea".to_string());
        "Test Nodename or ID"
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
        TargetInfoQuery::Uart("/dev/tty".to_string()),
        Some("uart:/dev/tty".to_string());
        "Test Uart"
    )]
    #[test_case(
        TargetInfoQuery::Id("totallynothoid".to_string()),
        Some("id:totallynothoid".to_string());
        "Test ID"
    )]
    #[test_case(
        TargetInfoQuery::Addr(std_socket_addr!("192.168.1.1:8082")),
        Some("192.168.1.1:8082".to_string());
        "Test Addr"
    )]
    fn test_into_option(query: TargetInfoQuery, want: Option<String>) {
        let got: Option<String> = query.into();
        assert_eq!(got, want);
    }

    #[test]
    fn test_try_from_option() {
        let q = TargetInfoQuery::try_from(Some("name".to_string())).unwrap();
        assert_eq!(q, TargetInfoQuery::NodenameOrId("name".to_string()));

        let q = TargetInfoQuery::try_from(None as Option<String>).unwrap();
        assert_eq!(q, TargetInfoQuery::First);

        let q = TargetInfoQuery::try_from(Some("".to_string())).unwrap();
        assert_eq!(q, TargetInfoQuery::First);
    }

    #[test]
    fn test_from_string_invalid_scope() {
        let str_input = "[fe80::1%invalidscope]:8022";
        let res = TargetInfoQuery::try_from(str_input);
        assert!(res.is_err());
    }

    #[test]
    fn test_serial_to_string_becomes_id() {
        let q = TargetInfoQuery::try_from("serial:123456").unwrap();
        assert_eq!(String::from(q), "id:123456");
    }
    #[test]
    fn test_query_match_description_ipv4_and_ipv6_scope_edge_cases() {
        use std::net::{Ipv4Addr, Ipv6Addr};

        let ipv4_desc = Description {
            nodename: Some("node-v4".to_string()),
            addresses: vec![TargetAddr::Net(SocketAddr::new(
                std::net::IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)),
                8022,
            ))],
            ssh_port: Some(8022),
            ..Default::default()
        };

        // Query with port 0 matches target with port 8022
        let q_v4_wildcard = TargetInfoQuery::Addr(SocketAddr::new(
            std::net::IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)),
            0,
        ));
        assert!(q_v4_wildcard.match_description(&ipv4_desc));

        // Query with exact port 8022 matches
        let q_v4_exact = TargetInfoQuery::Addr(SocketAddr::new(
            std::net::IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)),
            8022,
        ));
        assert!(q_v4_exact.match_description(&ipv4_desc));

        // Query with mismatched port does NOT match
        let q_v4_mismatch = TargetInfoQuery::Addr(SocketAddr::new(
            std::net::IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)),
            2222,
        ));
        assert!(!q_v4_mismatch.match_description(&ipv4_desc));

        let ipv6_desc = Description {
            nodename: Some("node-v6".to_string()),
            addresses: vec![TargetAddr::Net(SocketAddr::V6(std::net::SocketAddrV6::new(
                Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1),
                22,
                0,
                3, // scope_id = 3
            )))],
            ssh_port: Some(22),
            ..Default::default()
        };

        // Query with scope_id 0 (wildcard) matches target with scope_id 3
        let q_v6_wildcard_scope = TargetInfoQuery::Addr(SocketAddr::V6(
            std::net::SocketAddrV6::new(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1), 22, 0, 0),
        ));
        assert!(q_v6_wildcard_scope.match_description(&ipv6_desc));

        // Query with matching scope_id 3 matches
        let q_v6_matching_scope = TargetInfoQuery::Addr(SocketAddr::V6(
            std::net::SocketAddrV6::new(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1), 22, 0, 3),
        ));
        assert!(q_v6_matching_scope.match_description(&ipv6_desc));

        // Query with mismatched scope_id 4 does NOT match
        let q_v6_mismatched_scope = TargetInfoQuery::Addr(SocketAddr::V6(
            std::net::SocketAddrV6::new(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1), 22, 0, 4),
        ));
        assert!(!q_v6_mismatched_scope.match_description(&ipv6_desc));
    }

    #[test]
    fn test_query_match_description_vsock_and_usb_cid() {
        let vsock_desc = Description {
            nodename: Some("node-vsock".to_string()),
            addresses: vec![TargetAddr::VSockCtx(42)],
            ..Default::default()
        };
        assert!(TargetInfoQuery::VSock(42).match_description(&vsock_desc));
        assert!(!TargetInfoQuery::VSock(99).match_description(&vsock_desc));
        assert!(!TargetInfoQuery::Usb(42).match_description(&vsock_desc));

        let usb_desc = Description {
            nodename: Some("node-usb".to_string()),
            addresses: vec![TargetAddr::UsbCtx(100)],
            ..Default::default()
        };
        assert!(TargetInfoQuery::Usb(100).match_description(&usb_desc));
        assert!(!TargetInfoQuery::Usb(101).match_description(&usb_desc));
        assert!(!TargetInfoQuery::VSock(100).match_description(&usb_desc));
    }

    #[test]
    fn test_empty_uart_query_is_err() {
        assert!(TargetInfoQuery::try_from("uart:").is_err());
    }
}
