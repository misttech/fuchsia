// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_developer_ffx_common::{
    TargetAddrInfo, TargetIp, TargetIpAddrInfo, TargetIpPort, TargetVSockCtx, TargetVSockNamespace,
};
use fidl_fuchsia_net_common::{IpAddress, Ipv4Address, Ipv6Address};
use netext::{IsLocalAddr, scope_id_to_name};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::net::{IpAddr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::str::FromStr;
use thiserror::Error;

/// Error returned when we try to turn a non-network address into a `SocketAddr`
#[derive(Copy, Clone, Debug)]
pub struct NotANetworkAddress;

/// Error returned when parsing a target address fails.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum TargetAddrParseError {
    #[error("Invalid network address: {0}")]
    Net(#[from] std::net::AddrParseError),
    #[error("Invalid network port: {0}")]
    Port(#[source] std::num::ParseIntError),
    #[error("Invalid network interface scope '{scope}': {err}")]
    Scope { scope: String, err: String },
    #[error("Invalid UART address: {0}")]
    Uart(String),
    #[error("Invalid USB CID: {0}")]
    Usb(#[source] std::num::ParseIntError),
    #[error("Invalid VSOCK CID: {0}")]
    VSock(#[source] std::num::ParseIntError),
}

impl From<netext::NetExtError> for TargetAddrParseError {
    fn from(err: netext::NetExtError) -> Self {
        match err {
            netext::NetExtError::ParseAddress(_, e) => Self::Net(e),
            netext::NetExtError::ParsePort(_, e) => Self::Port(e),
            _ => Self::Net("".parse::<std::net::IpAddr>().unwrap_err()),
        }
    }
}

/// Represents an address associated with a target, like [`TargetAddr`], but is
/// restricted only to network addresses, i.e. addresses which might be suitable
/// for SSH. This saves us some type conversions when passing these addresses
/// around in parts of the code that are specifically supporting network
/// operations or opening SSH connections.
#[derive(Clone, Debug, Copy, Serialize, Deserialize)]
pub struct TargetIpAddr(SocketAddr);

impl TargetIpAddr {
    pub fn new(ip: IpAddr, scope_id: u32, port: u16) -> Self {
        match ip {
            IpAddr::V6(addr) => Self(SocketAddr::V6(SocketAddrV6::new(addr, port, 0, scope_id))),
            IpAddr::V4(addr) => Self(SocketAddr::V4(SocketAddrV4::new(addr, port))),
        }
    }

    pub fn scope_id(&self) -> u32 {
        match self.0 {
            SocketAddr::V6(v6) => v6.scope_id(),
            _ => 0,
        }
    }

    pub fn ip(&self) -> IpAddr {
        self.0.ip()
    }

    pub fn port(&self) -> u16 {
        self.0.port()
    }

    pub fn resolved_str(&self) -> String {
        if let IpAddr::V6(ip) = self.0.ip() {
            let scope_id = self.scope_id();
            if ip.is_link_local_addr() && scope_id > 0 {
                return format!("{ip}%{scope_id}");
            }
        }
        format!("{self}")
    }
}

/// Construct a new TargetIpAddr from a string representation, supporting both
/// `SocketAddr` syntax (e.g. `127.0.0.1:22`, `[fe80::1%lo]:0`) and portless IP
/// syntax (e.g. `127.0.0.1`, `fe80::1`, `fe80::1%lo`).
impl FromStr for TargetIpAddr {
    type Err = TargetAddrParseError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let (addr, scope, port) = netext::parse_address_parts(s)?;
        let scope = match scope {
            Some(scope) => netext::get_verified_scope_id(scope).map_err(|e| {
                TargetAddrParseError::Scope { scope: scope.to_string(), err: e.to_string() }
            })?,
            None => 0,
        };
        Ok(TargetIpAddr::new(addr, scope, port.unwrap_or(0)))
    }
}
// Compare `TargetIpAddr` by ip, port, and scope_id.
impl std::hash::Hash for TargetIpAddr {
    fn hash<H>(&self, state: &mut H)
    where
        H: std::hash::Hasher,
    {
        (self.0.ip(), self.0.port(), self.scope_id()).hash(state)
    }
}

impl PartialEq for TargetIpAddr {
    fn eq(&self, other: &Self) -> bool {
        self.0.ip() == other.0.ip()
            && self.0.port() == other.0.port()
            && self.scope_id() == other.scope_id()
    }
}

impl Eq for TargetIpAddr {}

impl Ord for TargetIpAddr {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .ip()
            .cmp(&other.0.ip())
            .then(self.0.port().cmp(&other.0.port()))
            .then(self.scope_id().cmp(&other.scope_id()))
    }
}

impl PartialOrd for TargetIpAddr {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl From<SocketAddr> for TargetIpAddr {
    fn from(s: SocketAddr) -> Self {
        TargetIpAddr(s)
    }
}

impl From<TargetIpAddr> for SocketAddr {
    fn from(t: TargetIpAddr) -> Self {
        Self::from(&t)
    }
}

impl From<&TargetIpAddr> for SocketAddr {
    fn from(t: &TargetIpAddr) -> Self {
        t.0
    }
}

impl Into<TargetIpAddrInfo> for &TargetIpAddr {
    fn into(self) -> TargetIpAddrInfo {
        let scope_id = self.scope_id();
        let ip = match self.0.ip() {
            IpAddr::V6(i) => IpAddress::Ipv6(Ipv6Address { addr: i.octets().into() }),
            IpAddr::V4(i) => IpAddress::Ipv4(Ipv4Address { addr: i.octets().into() }),
        };
        if self.0.port() == 0 {
            TargetIpAddrInfo::Ip(TargetIp { ip, scope_id })
        } else {
            TargetIpAddrInfo::IpPort(TargetIpPort { ip, scope_id, port: self.0.port() })
        }
    }
}

impl Into<TargetIpAddrInfo> for TargetIpAddr {
    fn into(self) -> TargetIpAddrInfo {
        (&self).into()
    }
}

impl Into<TargetAddrInfo> for &TargetIpAddr {
    fn into(self) -> TargetAddrInfo {
        let s: TargetIpAddrInfo = self.into();
        match s {
            TargetIpAddrInfo::IpPort(i) => TargetAddrInfo::IpPort(i),
            TargetIpAddrInfo::Ip(i) => TargetAddrInfo::Ip(i),
        }
    }
}

impl Into<TargetAddrInfo> for TargetIpAddr {
    fn into(self) -> TargetAddrInfo {
        (&self).into()
    }
}

impl From<TargetIpAddrInfo> for TargetIpAddr {
    fn from(t: TargetIpAddrInfo) -> Self {
        (&t).into()
    }
}

impl From<TargetIp> for TargetIpAddr {
    fn from(t: TargetIp) -> Self {
        let (addr, scope): (IpAddr, u32) = match t.ip {
            IpAddress::Ipv6(Ipv6Address { addr }) => (addr.into(), t.scope_id),
            IpAddress::Ipv4(Ipv4Address { addr }) => (addr.into(), t.scope_id),
        };
        TargetIpAddr::new(addr, scope, 0)
    }
}

impl From<&TargetIpAddrInfo> for TargetIpAddr {
    fn from(t: &TargetIpAddrInfo) -> Self {
        let (addr, scope, port): (IpAddr, u32, u16) = match t {
            TargetIpAddrInfo::Ip(ip) => match ip.ip {
                IpAddress::Ipv6(Ipv6Address { addr }) => (addr.into(), ip.scope_id, 0),
                IpAddress::Ipv4(Ipv4Address { addr }) => (addr.into(), ip.scope_id, 0),
            },
            TargetIpAddrInfo::IpPort(ip) => match ip.ip {
                IpAddress::Ipv6(Ipv6Address { addr }) => (addr.into(), ip.scope_id, ip.port),
                IpAddress::Ipv4(Ipv4Address { addr }) => (addr.into(), ip.scope_id, ip.port),
            },
        };

        TargetIpAddr::new(addr, scope, port)
    }
}

impl TryFrom<&TargetAddr> for TargetIpAddr {
    type Error = NotANetworkAddress;

    fn try_from(value: &TargetAddr) -> std::result::Result<Self, Self::Error> {
        match value {
            TargetAddr::Net(socket_addr) => Ok(TargetIpAddr(*socket_addr)),
            TargetAddr::VSockCtx(_) | TargetAddr::UsbCtx(_) | TargetAddr::Uart(_) => {
                Err(NotANetworkAddress)
            }
        }
    }
}

impl TryFrom<TargetAddr> for TargetIpAddr {
    type Error = NotANetworkAddress;

    fn try_from(value: TargetAddr) -> std::result::Result<Self, Self::Error> {
        Self::try_from(&value)
    }
}

impl TryFrom<&TargetAddrInfo> for TargetIpAddr {
    type Error = NotANetworkAddress;

    fn try_from(value: &TargetAddrInfo) -> std::result::Result<Self, Self::Error> {
        Self::try_from(TargetAddr::from(value))
    }
}

impl TryFrom<TargetAddrInfo> for TargetIpAddr {
    type Error = NotANetworkAddress;

    fn try_from(value: TargetAddrInfo) -> std::result::Result<Self, Self::Error> {
        Self::try_from(TargetAddr::from(value))
    }
}

impl std::fmt::Display for TargetIpAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0.ip() {
            IpAddr::V4(ip) => {
                write!(f, "{}", ip)?;
            }
            IpAddr::V6(ip) => {
                write!(f, "{}", ip)?;
                if ip.is_link_local_addr() && self.scope_id() > 0 {
                    write!(f, "%{}", scope_id_to_name(self.scope_id()))?;
                }
            }
        }
        Ok(())
    }
}

/// Represents an address associated with a target, network or otherwise.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TargetAddr {
    /// IPv6 connection, for example CDC Ethernet.
    Net(SocketAddr),

    /// Direct connection to a VM guest using virtio-vsock.
    VSockCtx(u32),

    /// VSOCK bridged over USB.
    UsbCtx(u32),

    /// Direct serial connection over UART, represented by an endpoint string:
    /// a character device (e.g. `"/dev/ttyUSB0"`), UNIX domain socket, or TCP address.
    ///
    /// Formatted as `"uart:<endpoint>"`. During target address resolution, UART addresses
    /// are evaluated after network and USB/VSOCK addresses in multi-transport priority order.
    Uart(String),
}

// Compare `TargetAddr` by ip, port, and scope_id (if network address) or cid (if VSOCK/USB address).
impl std::hash::Hash for TargetAddr {
    fn hash<H>(&self, state: &mut H)
    where
        H: std::hash::Hasher,
    {
        match self {
            TargetAddr::Net(addr) => (addr.ip(), addr.port(), self.scope_id()).hash(state),
            TargetAddr::VSockCtx(cid) => cid.hash(state),
            TargetAddr::UsbCtx(cid) => {
                cid.hash(state);
                "usb".hash(state)
            }
            TargetAddr::Uart(endpoint) => {
                endpoint.hash(state);
                "uart".hash(state)
            }
        }
    }
}

impl PartialEq for TargetAddr {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (TargetAddr::Net(addr), TargetAddr::Net(other_addr)) => {
                addr.ip() == other_addr.ip()
                    && addr.port() == other_addr.port()
                    && self.scope_id() == other.scope_id()
            }
            (TargetAddr::Net(_), _) | (_, TargetAddr::Net(_)) => false,
            (TargetAddr::VSockCtx(cid), TargetAddr::VSockCtx(other)) => cid == other,
            (TargetAddr::VSockCtx(_), _) | (_, TargetAddr::VSockCtx(_)) => false,
            (TargetAddr::UsbCtx(cid), TargetAddr::UsbCtx(other)) => cid == other,
            (TargetAddr::UsbCtx(_), _) | (_, TargetAddr::UsbCtx(_)) => false,
            (TargetAddr::Uart(endpoint), TargetAddr::Uart(other)) => endpoint == other,
        }
    }
}

impl Eq for TargetAddr {}

impl Ord for TargetAddr {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (TargetAddr::Net(addr), TargetAddr::Net(other_addr)) => addr
                .ip()
                .cmp(&other_addr.ip())
                .then(addr.port().cmp(&other_addr.port()))
                .then(self.scope_id().cmp(&other.scope_id())),
            (TargetAddr::VSockCtx(cid), TargetAddr::VSockCtx(other)) => cid.cmp(other),
            (TargetAddr::UsbCtx(cid), TargetAddr::UsbCtx(other)) => cid.cmp(other),
            (TargetAddr::Uart(endpoint), TargetAddr::Uart(other)) => endpoint.cmp(other),

            // VSockCtx is highest priority (smallest)
            (TargetAddr::VSockCtx(_), _) => Ordering::Less,
            (_, TargetAddr::VSockCtx(_)) => Ordering::Greater,

            // UsbCtx is second
            (TargetAddr::UsbCtx(_), _) => Ordering::Less,
            (_, TargetAddr::UsbCtx(_)) => Ordering::Greater,

            // Net is preferred over Uart
            (TargetAddr::Net(_), TargetAddr::Uart(_)) => Ordering::Less,
            (TargetAddr::Uart(_), TargetAddr::Net(_)) => Ordering::Greater,
        }
    }
}

impl PartialOrd for TargetAddr {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Into<TargetAddrInfo> for &TargetAddr {
    fn into(self) -> TargetAddrInfo {
        match self {
            TargetAddr::Net(addr) => TargetIpAddr::from(*addr).into(),
            TargetAddr::VSockCtx(cid) => TargetAddrInfo::Vsock(TargetVSockCtx {
                cid: *cid,
                namespace: TargetVSockNamespace::Vsock,
            }),
            TargetAddr::UsbCtx(cid) => TargetAddrInfo::Vsock(TargetVSockCtx {
                cid: *cid,
                namespace: TargetVSockNamespace::Usb,
            }),
            TargetAddr::Uart(endpoint) => TargetAddrInfo::Uart(endpoint.clone()),
        }
    }
}

impl Into<TargetAddrInfo> for TargetAddr {
    fn into(self) -> TargetAddrInfo {
        (&self).into()
    }
}

impl TryInto<TargetIpAddrInfo> for TargetAddr {
    type Error = NotANetworkAddress;

    fn try_into(self) -> std::result::Result<TargetIpAddrInfo, Self::Error> {
        Ok(TargetIpAddr::try_from(self)?.into())
    }
}

impl From<TargetAddrInfo> for TargetAddr {
    fn from(t: TargetAddrInfo) -> Self {
        (&t).into()
    }
}

impl From<TargetIpAddr> for TargetAddr {
    fn from(t: TargetIpAddr) -> Self {
        TargetAddr::Net(t.0)
    }
}

impl From<&TargetIpAddr> for TargetAddr {
    fn from(t: &TargetIpAddr) -> Self {
        Self::from(t.clone())
    }
}

impl From<TargetIp> for TargetAddr {
    fn from(t: TargetIp) -> Self {
        let (addr, scope): (IpAddr, u32) = match t.ip {
            IpAddress::Ipv6(Ipv6Address { addr }) => (addr.into(), t.scope_id),
            IpAddress::Ipv4(Ipv4Address { addr }) => (addr.into(), t.scope_id),
        };
        TargetAddr::new(addr, scope, 0)
    }
}

impl From<&TargetAddrInfo> for TargetAddr {
    fn from(t: &TargetAddrInfo) -> Self {
        let (addr, scope, port): (IpAddr, u32, u16) = match t {
            TargetAddrInfo::Ip(ip) => match ip.ip {
                IpAddress::Ipv6(Ipv6Address { addr }) => (addr.into(), ip.scope_id, 0),
                IpAddress::Ipv4(Ipv4Address { addr }) => (addr.into(), ip.scope_id, 0),
            },
            TargetAddrInfo::IpPort(ip) => match ip.ip {
                IpAddress::Ipv6(Ipv6Address { addr }) => (addr.into(), ip.scope_id, ip.port),
                IpAddress::Ipv4(Ipv4Address { addr }) => (addr.into(), ip.scope_id, ip.port),
            },
            TargetAddrInfo::Vsock(TargetVSockCtx {
                cid,
                namespace: TargetVSockNamespace::Vsock,
            }) => return TargetAddr::VSockCtx(*cid),
            TargetAddrInfo::Vsock(TargetVSockCtx { cid, namespace: TargetVSockNamespace::Usb }) => {
                return TargetAddr::UsbCtx(*cid);
            } // TODO(https://fxbug.dev/42130068): Add serial numbers.,
            TargetAddrInfo::Uart(endpoint) => return TargetAddr::Uart(endpoint.clone()),
        };

        TargetAddr::new(addr, scope, port)
    }
}

impl From<SocketAddr> for TargetAddr {
    fn from(s: SocketAddr) -> Self {
        Self::Net(s)
    }
}

/// Construct a new TargetAddr from a string representation of the form
/// accepted by std::net::SocketAddr, e.g. 127.0.0.1:22, or [fe80::1%1]:0.
impl FromStr for TargetAddr {
    type Err = TargetAddrParseError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        if let Some(endpoint) = s.strip_prefix("uart:") {
            if endpoint.is_empty() {
                return Err(TargetAddrParseError::Uart(
                    "requires a non-empty endpoint (e.g. uart:/dev/ttyUSB0)".to_string(),
                ));
            }
            return Ok(Self::Uart(endpoint.to_string()));
        }
        if let Some(cid_str) = s.strip_prefix("usb:cid:") {
            let cid = cid_str.parse().map_err(TargetAddrParseError::Usb)?;
            return Ok(Self::UsbCtx(cid));
        }
        if let Some(cid_str) = s.strip_prefix("vsock:cid:") {
            let cid = cid_str.parse().map_err(TargetAddrParseError::VSock)?;
            return Ok(Self::VSockCtx(cid));
        }
        let ip_addr = s.parse::<TargetIpAddr>()?;
        Ok(Self::from(ip_addr))
    }
}

impl TargetAddr {
    // TODO(https://fxbug.dev/570676953): clean up with wrapper types for `scope` and `port` to
    // avoid the "zero is default" legacy.
    pub fn new(ip: IpAddr, scope_id: u32, port: u16) -> Self {
        match ip {
            IpAddr::V6(addr) => {
                Self::Net(SocketAddr::V6(SocketAddrV6::new(addr, port, 0, scope_id)))
            }
            IpAddr::V4(addr) => Self::Net(SocketAddr::V4(SocketAddrV4::new(addr, port))),
        }
    }

    pub fn scope_id(&self) -> u32 {
        match self {
            TargetAddr::Net(SocketAddr::V6(v6)) => v6.scope_id(),
            _ => 0,
        }
    }

    pub fn set_scope_id(&mut self, scope_id: u32) {
        match self {
            TargetAddr::Net(SocketAddr::V6(v6)) => v6.set_scope_id(scope_id),
            _ => {}
        }
    }

    pub fn ip(&self) -> Option<IpAddr> {
        match self {
            TargetAddr::Net(addr) => Some(addr.ip()),
            TargetAddr::VSockCtx(_) | TargetAddr::UsbCtx(_) | TargetAddr::Uart(_) => None,
        }
    }

    pub fn port(&self) -> Option<u16> {
        match self {
            TargetAddr::Net(addr) => Some(addr.port()),
            TargetAddr::VSockCtx(_) | TargetAddr::UsbCtx(_) | TargetAddr::Uart(_) => None,
        }
    }

    pub fn cid_vsock(&self) -> Option<u32> {
        match self {
            TargetAddr::VSockCtx(cid) => Some(*cid),
            TargetAddr::Net(_) | TargetAddr::UsbCtx(_) | TargetAddr::Uart(_) => None,
        }
    }

    pub fn cid_usb(&self) -> Option<u32> {
        match self {
            TargetAddr::UsbCtx(cid) => Some(*cid),
            TargetAddr::Net(_) | TargetAddr::VSockCtx(_) | TargetAddr::Uart(_) => None,
        }
    }

    pub fn uart_endpoint(&self) -> Option<&str> {
        match self {
            TargetAddr::Uart(endpoint) => Some(endpoint.as_str()),
            TargetAddr::Net(_) | TargetAddr::VSockCtx(_) | TargetAddr::UsbCtx(_) => None,
        }
    }

    pub fn set_port(&mut self, new_port: u16) -> Result<(), NotANetworkAddress> {
        match self {
            TargetAddr::Net(addr) => {
                addr.set_port(new_port);
                Ok(())
            }
            TargetAddr::VSockCtx(_) | TargetAddr::UsbCtx(_) | TargetAddr::Uart(_) => {
                Err(NotANetworkAddress)
            }
        }
    }

    pub fn optional_port_str(&self) -> String {
        match self {
            TargetAddr::Net(addr) => match (addr.ip(), addr.port()) {
                (_, 0) | (_, 22) => format!("{self}"),
                (IpAddr::V6(_), p) => format!("[{self}]:{p}"),
                (_, p) => format!("{self}:{p}"),
            },
            TargetAddr::VSockCtx(_) | TargetAddr::UsbCtx(_) | TargetAddr::Uart(_) => {
                format!("{self}")
            }
        }
    }

    /// Compares two [`SocketAddr`] instances by discovery connection priority.
    ///
    /// Prioritizes link-local IPv6 addresses over global/routable IP addresses to prefer
    /// direct local connections (such as CDC Ethernet) during target resolution.
    ///
    /// # Arguments
    ///
    /// * `a1` - The first socket address to compare.
    /// * `a2` - The second socket address to compare.
    ///
    /// # Returns
    ///
    /// Returns [`Ordering::Less`] if `a1` has higher priority than `a2`,
    /// [`Ordering::Greater`] if `a2` has higher priority than `a1`, or
    /// [`Ordering::Equal`] if both addresses have the same link-local classification.
    pub fn compare_socket_addrs_by_priority(a1: &SocketAddr, a2: &SocketAddr) -> Ordering {
        match (a1.ip().is_link_local_addr(), a2.ip().is_link_local_addr()) {
            (true, true) | (false, false) => Ordering::Equal,
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
        }
    }

    /// Compares two [`TargetAddr`] instances by overall transport connection priority.
    ///
    /// Multi-transport resolution evaluates target addresses in the following order:
    /// 1. Virtual sockets ([`TargetAddr::VSockCtx`]) - highest priority
    /// 2. USB bridged virtual sockets ([`TargetAddr::UsbCtx`])
    /// 3. Network sockets ([`TargetAddr::Net`], with link-local prioritized over global)
    /// 4. Serial connections ([`TargetAddr::Uart`]) - lowest priority
    ///
    /// If both addresses belong to the same transport variant, ties are resolved using
    /// standard [`Ord::cmp`] ordering.
    ///
    /// # Arguments
    ///
    /// * `other` - The other target address to compare against.
    ///
    /// # Returns
    ///
    /// Returns [`Ordering::Less`] if `self` has higher priority than `other`,
    /// [`Ordering::Greater`] if `self` has lower priority, or [`Ordering::Equal`] if identical.
    pub fn compare_by_priority(&self, other: &Self) -> Ordering {
        match (self, other) {
            (TargetAddr::Net(a), TargetAddr::Net(b)) => {
                Self::compare_socket_addrs_by_priority(a, b).then_with(|| self.cmp(other))
            }
            _ => self.cmp(other),
        }
    }
}

impl std::fmt::Display for TargetAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TargetAddr::Net(addr) => write!(f, "{}", TargetIpAddr::from(*addr)),
            TargetAddr::VSockCtx(cid) => write!(f, "vsock:cid:{cid}"),
            TargetAddr::UsbCtx(cid) => write!(f, "usb:cid:{cid}"),
            TargetAddr::Uart(endpoint) => write!(f, "uart:{endpoint}"),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use net_declare::std_socket_addr;

    #[fuchsia::test]
    fn test_port_str_with_ipv6_bracket() {
        let v6addr = std_socket_addr!("[2001:db8::1%1]:8080");
        let addr: TargetAddr = v6addr.into();
        let s = addr.optional_port_str();
        assert_eq!(&s, "[2001:db8::1]:8080");
    }

    #[fuchsia::test]
    fn test_resolved_str() {
        // v4
        let v4addr = std_socket_addr!("192.168.1.1:8080");
        let addr = TargetIpAddr::from(v4addr);
        assert_eq!(&addr.resolved_str(), "192.168.1.1");

        // v6 without scope
        let v6addr = std_socket_addr!("[2001:db8::1]:8080");
        let addr = TargetIpAddr::from(v6addr);
        assert_eq!(&addr.resolved_str(), "2001:db8::1");

        // v6 link-local with scope
        let v6addr = std_socket_addr!("[fe80::1%1]:8080");
        let addr = TargetIpAddr::from(v6addr);
        assert_eq!(&addr.resolved_str(), "fe80::1%1");

        // v6 link-local without scope
        let v6addr = std_socket_addr!("[fe80::1]:8080");
        let addr = TargetIpAddr::from(v6addr);
        assert_eq!(&addr.resolved_str(), "fe80::1");
    }

    #[fuchsia::test]
    fn test_target_addr_eq_and_hash_includes_scope() {
        let addr1 = TargetAddr::new("fe80::1".parse().unwrap(), 1, 8080);
        let addr2 = TargetAddr::new("fe80::1".parse().unwrap(), 2, 8080);
        let addr1_dup = TargetAddr::new("fe80::1".parse().unwrap(), 1, 8080);

        assert_eq!(addr1, addr1_dup);
        assert_ne!(addr1, addr2);

        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut h1 = DefaultHasher::new();
        addr1.hash(&mut h1);

        let mut h2 = DefaultHasher::new();
        addr2.hash(&mut h2);

        let mut h1_dup = DefaultHasher::new();
        addr1_dup.hash(&mut h1_dup);

        assert_eq!(h1.finish(), h1_dup.finish());
        assert_ne!(h1.finish(), h2.finish());
    }

    #[fuchsia::test]
    fn test_target_ip_addr_eq_and_hash_includes_scope() {
        let addr1 = TargetIpAddr::new("fe80::1".parse().unwrap(), 1, 8080);
        let addr2 = TargetIpAddr::new("fe80::1".parse().unwrap(), 2, 8080);
        let addr1_dup = TargetIpAddr::new("fe80::1".parse().unwrap(), 1, 8080);

        assert_eq!(addr1, addr1_dup);
        assert_ne!(addr1, addr2);

        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut h1 = DefaultHasher::new();
        addr1.hash(&mut h1);

        let mut h2 = DefaultHasher::new();
        addr2.hash(&mut h2);

        let mut h1_dup = DefaultHasher::new();
        addr1_dup.hash(&mut h1_dup);

        assert_eq!(h1.finish(), h1_dup.finish());
        assert_ne!(h1.finish(), h2.finish());
    }

    #[fuchsia::test]
    fn test_target_addr_uart() {
        let parsed: TargetAddr = "uart:/dev/ttyUSB0".parse().expect("valid uart addr");
        assert_eq!(parsed, TargetAddr::Uart("/dev/ttyUSB0".to_string()));
        assert_eq!(format!("{parsed}"), "uart:/dev/ttyUSB0");
        assert_eq!(parsed.ip(), None);
        assert_eq!(parsed.port(), None);
        assert_eq!(parsed.cid_vsock(), None);
        assert_eq!(parsed.cid_usb(), None);
        assert_eq!(parsed.uart_endpoint(), Some("/dev/ttyUSB0"));

        // Empty path rejected
        assert!("uart:".parse::<TargetAddr>().is_err());

        // Conversion to/from FIDL TargetAddrInfo
        let info: TargetAddrInfo = (&parsed).into();
        let from_info: TargetAddr = (&info).into();
        assert_eq!(parsed, from_info);

        // Ordering: VSockCtx < UsbCtx < Net < Uart
        let vsock = TargetAddr::VSockCtx(42);
        let usb = TargetAddr::UsbCtx(42);
        let net = TargetAddr::Net("127.0.0.1:8022".parse().unwrap());
        let uart1 = TargetAddr::Uart("/dev/ttyUSB0".to_string());
        let uart2 = TargetAddr::Uart("/dev/ttyUSB1".to_string());

        assert!(vsock < usb);
        assert!(usb < net);
        assert!(net < uart1);
        assert!(uart1 < uart2);
    }

    #[fuchsia::test]
    fn test_target_addr_parse() {
        // Net
        let net: TargetAddr = "127.0.0.1:8022".parse().expect("valid net addr");
        assert_eq!(net, TargetAddr::Net("127.0.0.1:8022".parse().unwrap()));
        let net_no_port: TargetAddr = "127.0.0.1".parse().expect("valid portless v4 addr");
        assert_eq!(net_no_port, TargetAddr::Net("127.0.0.1:0".parse().unwrap()));
        let v6_no_port: TargetAddr = "fe80::1".parse().expect("valid portless v6 addr");
        assert_eq!(v6_no_port, TargetAddr::Net("[fe80::1]:0".parse().unwrap()));
        let v6_unspec_port: TargetAddr = "[::]:8022".parse().expect("valid v6 unspecified addr");
        assert_eq!(v6_unspec_port, TargetAddr::Net("[::]:8022".parse().unwrap()));
        assert!(matches!("not-an-ip".parse::<TargetAddr>(), Err(TargetAddrParseError::Net(_))));
        assert!(matches!(
            "127.0.0.1:65536".parse::<TargetAddr>(),
            Err(TargetAddrParseError::Port(_))
        ));
        assert!(matches!("[::]:65536".parse::<TargetAddr>(), Err(TargetAddrParseError::Port(_))));
        assert!(matches!(
            "127.0.0.1:abc".parse::<TargetAddr>(),
            Err(TargetAddrParseError::Port(_))
        ));

        // Uart
        let uart: TargetAddr = "uart:/dev/ttyUSB0".parse().expect("valid uart addr");
        assert_eq!(uart, TargetAddr::Uart("/dev/ttyUSB0".to_string()));
        assert!(matches!("uart:".parse::<TargetAddr>(), Err(TargetAddrParseError::Uart(_))));

        // USB
        let usb: TargetAddr = "usb:cid:42".parse().expect("valid usb addr");
        assert_eq!(usb, TargetAddr::UsbCtx(42));
        assert!(matches!("usb:cid:".parse::<TargetAddr>(), Err(TargetAddrParseError::Usb(_))));
        assert!(matches!("usb:cid:abc".parse::<TargetAddr>(), Err(TargetAddrParseError::Usb(_))));

        // VSock
        let vsock: TargetAddr = "vsock:cid:42".parse().expect("valid vsock addr");
        assert_eq!(vsock, TargetAddr::VSockCtx(42));
        assert!(matches!("vsock:cid:".parse::<TargetAddr>(), Err(TargetAddrParseError::VSock(_))));
        assert!(matches!(
            "vsock:cid:xyz".parse::<TargetAddr>(),
            Err(TargetAddrParseError::VSock(_))
        ));
    }
}
