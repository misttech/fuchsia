// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use addr::TargetAddr;
use anyhow::{anyhow, Error, Result};
use ffx_list_args::{AddressTypes, Format};
use fidl_fuchsia_developer_ffx as ffx;
use fidl_fuchsia_net::IpAddress;
use netext::IsLocalAddr;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::cmp::max;
use std::fmt::{self, Display, Write};

const NAME: &'static str = "NAME";
const SERIAL: &'static str = "SERIAL";
const TYPE: &'static str = "TYPE";
const STATE: &'static str = "STATE";
const ADDRS: &'static str = "ADDRS/IP";
const RCS: &'static str = "RCS";
const MANUAL: &'static str = "MANUAL";

const UNKNOWN: &'static str = "<unknown>";

const PADDING_SPACES: usize = 4;
/// A trait for returning a consistent SSH address.
///
/// Based on the structure from which the SSH address is coming, this will
/// return in order of priority:
/// -- The first local IPv6 address with a scope id.
/// -- The last local IPv4 address.
/// -- Any other address.
///
/// DEPRECATED: Migrate to using the ssh address target data.
pub trait SshAddrFetcher {
    fn to_ssh_addr(self) -> Option<TargetAddr>;
}

impl<'a, T: Copy + IntoIterator<Item = &'a TargetAddr>> SshAddrFetcher for &'a T {
    fn to_ssh_addr(self) -> Option<TargetAddr> {
        let mut res: Option<TargetAddr> = None;
        for addr in self.into_iter() {
            let Some(ip) = addr.ip() else {
                continue;
            };
            let is_valid_local_addr = ip.is_local_addr()
                && (ip.is_ipv4() || !(ip.is_link_local_addr() && addr.scope_id() == 0));

            if res.is_none() || is_valid_local_addr {
                res.replace(addr.clone());
            }
            if ip.is_ipv6() && is_valid_local_addr {
                res.replace(addr.clone());
                break;
            }
        }
        res
    }
}

const DEFAULT_SSH_PORT: u16 = 22;
pub fn port_str(ta: TargetAddr) -> String {
    match ta {
        TargetAddr::Net(mut addr) => {
            let mut port = addr.port();
            if port == 0 {
                port = DEFAULT_SSH_PORT;
            }
            addr.set_port(port);
            addr.to_string()
        }
        TargetAddr::VSockCtx(_) | TargetAddr::UsbCtx(_) => format!("{ta}"),
    }
}

fn nodename_to_string(index: Option<usize>, nodename: Option<String>) -> String {
    match nodename {
        Some(name) => name,
        None => match index {
            Some(index) => format!("<unknown-{}>", index),
            None => target_errors::UNKNOWN_TARGET_NAME.to_owned(),
        },
    }
}

fn has_multiple_unknown_targets(targets: &Vec<ffx::TargetInfo>) -> bool {
    let mut unknown_count = 0;
    for target in targets.iter() {
        if target.nodename.is_none() {
            unknown_count += 1;
        }
        if unknown_count > 1 {
            return true;
        }
    }
    false
}

fn is_ipv4(info: &ffx::TargetAddrInfo) -> bool {
    let ip = match info {
        ffx::TargetAddrInfo::Ip(ip) => ip.ip,
        ffx::TargetAddrInfo::IpPort(ip) => ip.ip,
        ffx::TargetAddrInfo::Vsock(_) => return false,
    };
    match ip {
        IpAddress::Ipv4(_) => true,
        IpAddress::Ipv6(_) => false,
    }
}

pub fn filter_targets_by_address_types(
    targets: Vec<ffx::TargetInfo>,
    address_types: AddressTypes,
) -> Vec<ffx::TargetInfo> {
    targets
        .into_iter()
        .filter_map(|mut target| match address_types {
            AddressTypes::All => Some(target),
            AddressTypes::None => None,
            AddressTypes::Ipv4Only => {
                target.addresses.as_mut().map(|addresses| addresses.retain(|addr| is_ipv4(addr)));
                Some(target)
            }
            AddressTypes::Ipv6Only => {
                target.addresses.as_mut().map(|addresses| addresses.retain(|addr| !is_ipv4(addr)));
                Some(target)
            }
        })
        .collect()
}

/// Simple trait for a target formatter.
pub trait TargetFormatter {
    fn lines(&self, default_nodename: Option<&str>) -> Vec<String>;
}

impl TryFrom<(Format, AddressTypes, Vec<ffx::TargetInfo>)> for Box<dyn TargetFormatter> {
    type Error = Error;

    fn try_from(tup: (Format, AddressTypes, Vec<ffx::TargetInfo>)) -> Result<Self> {
        let (format, address_types, targets) = tup;
        let targets = filter_targets_by_address_types(targets, address_types);
        Ok(match format {
            Format::Tabular => Box::new(TabularTargetFormatter::try_from(targets)?),
            Format::Simple => Box::new(SimpleTargetFormatter::try_from(targets)?),
            Format::Addresses => Box::new(AddressesTargetFormatter::try_from(targets)?),
            Format::NameOnly => Box::new(NameOnlyTargetFormatter::try_from(targets)?),
            Format::Json => Box::new(JsonTargetFormatter::try_from(targets)?),
        })
    }
}

pub struct AddressesTarget(TargetAddr);

impl TryFrom<ffx::TargetInfo> for AddressesTarget {
    type Error = Error;

    fn try_from(t: ffx::TargetInfo) -> Result<Self> {
        let addrs = t.addresses.ok_or_else(|| anyhow!("must contain an address"))?;
        let addrs = addrs.iter().map(TargetAddr::from).collect::<Vec<_>>();

        Ok(Self((&addrs).to_ssh_addr().ok_or_else(|| anyhow!("could not convert to ssh addr"))?))
    }
}

pub struct AddressesTargetFormatter {
    targets: Vec<AddressesTarget>,
}

impl TryFrom<Vec<ffx::TargetInfo>> for AddressesTargetFormatter {
    type Error = Error;

    fn try_from(targets: Vec<ffx::TargetInfo>) -> Result<Self> {
        let targets = targets.into_iter().flat_map(AddressesTarget::try_from).collect::<Vec<_>>();
        Ok(Self { targets })
    }
}

impl TargetFormatter for AddressesTargetFormatter {
    fn lines(&self, _default_nodename: Option<&str>) -> Vec<String> {
        self.targets.iter().map(|t| port_str(t.0)).collect()
    }
}

pub struct NameOnlyTarget(String);

impl TryFrom<(Option<usize>, ffx::TargetInfo)> for NameOnlyTarget {
    type Error = Error;

    fn try_from((index, target): (Option<usize>, ffx::TargetInfo)) -> Result<Self> {
        let name = nodename_to_string(index, target.nodename);
        Ok(Self(name))
    }
}

pub struct NameOnlyTargetFormatter {
    targets: Vec<NameOnlyTarget>,
}

impl TryFrom<Vec<ffx::TargetInfo>> for NameOnlyTargetFormatter {
    type Error = Error;

    fn try_from(targets: Vec<ffx::TargetInfo>) -> Result<Self> {
        let use_index = has_multiple_unknown_targets(&targets);
        let targets = targets
            .into_iter()
            .enumerate()
            .map(|(i, t)| (if use_index { Some(i) } else { None }, t))
            .flat_map(NameOnlyTarget::try_from)
            .collect::<Vec<_>>();
        Ok(Self { targets })
    }
}

impl TargetFormatter for NameOnlyTargetFormatter {
    fn lines(&self, _default_nodename: Option<&str>) -> Vec<String> {
        self.targets.iter().map(|t| t.0.clone()).collect()
    }
}

pub struct SimpleTarget(String, TargetAddr);

pub struct SimpleTargetFormatter {
    targets: Vec<SimpleTarget>,
}

impl TryFrom<Vec<ffx::TargetInfo>> for SimpleTargetFormatter {
    type Error = Error;

    fn try_from(targets: Vec<ffx::TargetInfo>) -> Result<Self> {
        let targets = targets.into_iter().flat_map(SimpleTarget::try_from).collect::<Vec<_>>();
        Ok(Self { targets })
    }
}

impl TargetFormatter for SimpleTargetFormatter {
    fn lines(&self, _default_nodename: Option<&str>) -> Vec<String> {
        self.targets.iter().map(|t| format!("{} {}", t.1, t.0)).collect()
    }
}

impl TryFrom<ffx::TargetInfo> for SimpleTarget {
    type Error = Error;

    fn try_from(t: ffx::TargetInfo) -> Result<Self> {
        let nodename = t.nodename.unwrap_or_else(|| "".to_string());
        let addrs = t.addresses.ok_or_else(|| anyhow!("must contain an address"))?;
        let addrs = addrs.iter().map(TargetAddr::from).collect::<Vec<_>>();

        Ok(Self(
            nodename,
            (&addrs).to_ssh_addr().ok_or_else(|| anyhow!("could not convert to ssh addr"))?,
        ))
    }
}

pub struct JsonTargetFormatter {
    pub targets: Vec<JsonTarget>,
}

impl TryFrom<Vec<ffx::TargetInfo>> for JsonTargetFormatter {
    type Error = Error;

    fn try_from(targets: Vec<ffx::TargetInfo>) -> Result<Self> {
        let use_index = has_multiple_unknown_targets(&targets);
        let targets = targets
            .into_iter()
            .enumerate()
            .map(|(i, t)| (if use_index { Some(i) } else { None }, t))
            .flat_map(JsonTarget::try_from)
            .collect::<Vec<_>>();
        Ok(Self { targets })
    }
}

impl TargetFormatter for JsonTargetFormatter {
    fn lines(&self, default_nodename: Option<&str>) -> Vec<String> {
        let mut t = self.targets.clone();
        JsonTargetFormatter::set_default_target(&mut t, default_nodename);
        vec![serde_json::to_string(&t).expect("should serialize")]
    }
}

impl JsonTargetFormatter {
    pub fn set_default_target(targets: &mut Vec<JsonTarget>, default_nodename: Option<&str>) {
        targets
            .iter_mut()
            .find(|t| default_nodename.map(|n| t.nodename == n).unwrap_or(false))
            .map(|s| s.is_default = true.into());
    }
}

#[derive(Debug, PartialEq, Eq)]
enum StringifiedField {
    String(String),
    Array(Vec<String>),
}

impl Default for StringifiedField {
    fn default() -> Self {
        StringifiedField::String(String::new())
    }
}

impl StringifiedField {
    fn len(&self) -> usize {
        match self {
            StringifiedField::String(_s) => 1,
            StringifiedField::Array(a) => a.len(),
        }
    }

    fn string_len(&self) -> usize {
        match self {
            StringifiedField::String(s) => s.len(),
            StringifiedField::Array(a) => a.iter().map(|s| s.len()).max().unwrap_or(0),
        }
    }

    fn at_index(&self, index: usize) -> Option<String> {
        match self {
            StringifiedField::String(s) => {
                if index == 0 {
                    Some(s.clone())
                } else {
                    None
                }
            }
            StringifiedField::Array(a) => a.get(index).cloned(),
        }
    }
}

// Convenience macro to make potential addition/removal of fields less likely
// to affect internal logic. Other functions that construct these targets will
// fail to compile if more fields are added.
macro_rules! make_structs_and_support_functions {
    ($( $field:ident ),+ $(,)?) => {
        #[derive(Default)]
        struct Limits {
            $(
                $field: usize,
            )*
        }

        impl Limits {
            fn update(&mut self, target: &mut StringifiedTarget) {
                $(
                    self.$field = max(self.$field, target.$field.string_len());
                    target.__longest_array = max(target.__longest_array, target.$field.len());
                )*
            }

            fn capacity(&self) -> usize {
                let mut result = 0;
                $(
                    result += self.$field + PADDING_SPACES;
                )*
                result
            }
        }

        #[derive(Debug, PartialEq, Eq)]
        struct StringifiedTarget {
            __longest_array: usize,
            $(
                $field: StringifiedField,
            )*
        }

        impl Default for StringifiedTarget {
            fn default() -> Self {
                Self {
                    __longest_array: 0,
                    $(
                        $field: StringifiedField::default(),
                    )*
                }
            }
        }

        make_structs_and_support_functions!(@print_func $($field,)*);
    };

    (@print_func $nodename:ident, $last_field:ident, $($field:ident),* $(,)?) => {
        #[inline]
        fn format_fields(target: &StringifiedTarget, limits: &Limits, default_nodename: &str) -> String {
            fn format_fields_(target: &StringifiedTarget, limits: &Limits, default_nodename: &str, index: usize) -> String {
                let mut s = String::with_capacity(limits.capacity());
                let nodename = match target.$nodename.at_index(index) {
                    Some(nodename) if nodename == default_nodename => format!("{}*", nodename),
                    Some(nodename) => nodename,
                    None => String::new(),
                };
                write!(s, "{:width$}", nodename, width = limits.$nodename + PADDING_SPACES).unwrap();
                $(
                    write!(s, "{:width$}", target.$field.at_index(index).unwrap_or_else(String::new), width = limits.$field + PADDING_SPACES).unwrap();
                )*
                // Skips spaces on the end.
                write!(s, "{}", target.$last_field.at_index(index).unwrap_or_else(String::new)).unwrap();
                s
            }

            (0..target.__longest_array).map(|i| format_fields_(target, limits, default_nodename, i)).collect::<Vec<_>>().join("\n")
        }
    };
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq, JsonSchema)]
#[serde(tag = "type")]
pub enum JsonTargetAddress {
    Ip { ip: String, ssh_port: u16 },
    VSock { cid: u32 },
    Usb { cid: u32 },
}

impl From<ffx::TargetAddrInfo> for JsonTargetAddress {
    fn from(info: ffx::TargetAddrInfo) -> Self {
        let tai: TargetAddr = info.into();

        match &tai {
            TargetAddr::Net(_) => {
                JsonTargetAddress::Ip { ip: tai.to_string(), ssh_port: tai.port().unwrap() }
            }
            TargetAddr::VSockCtx(cid) => JsonTargetAddress::VSock { cid: *cid },
            TargetAddr::UsbCtx(cid) => JsonTargetAddress::Usb { cid: *cid },
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq, JsonSchema)]
pub struct JsonTarget {
    nodename: String,
    rcs_state: String,
    serial: String,
    target_type: String,
    target_state: String,
    addresses: Vec<JsonTargetAddress>,
    is_default: bool,
    is_manual: bool,
}
// Second field is printed last in this implementation, everything else is printed in order.
make_structs_and_support_functions!(
    nodename,
    rcs_state,
    serial,
    target_type,
    target_state,
    addresses,
    is_manual,
);

#[derive(Debug, PartialEq, Eq)]
pub enum StringifyError {
    MissingAddresses,
    MissingRcsState,
    MissingTargetState,
}

impl Display for StringifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "stringification error: {:?}", self)
    }
}

impl std::error::Error for StringifyError {}

impl StringifiedTarget {
    fn from_target_addr_info(a: ffx::TargetAddrInfo) -> String {
        TargetAddr::from(a).optional_port_str()
    }

    fn from_addresses(mut v: Vec<ffx::TargetAddrInfo>) -> String {
        format!(
            "[{}]",
            v.drain(..)
                .map(|a| StringifiedTarget::from_target_addr_info(a))
                .collect::<Vec<_>>()
                .join(",; ")
        )
    }

    fn field_from_addresses(v: Vec<ffx::TargetAddrInfo>) -> StringifiedField {
        let all_addresses = StringifiedTarget::from_addresses(v);
        StringifiedField::Array(all_addresses.split(';').map(String::from).collect::<Vec<_>>())
    }

    fn from_rcs_state(r: ffx::RemoteControlState) -> String {
        match r {
            ffx::RemoteControlState::Down | ffx::RemoteControlState::Unknown => "N".to_string(),
            ffx::RemoteControlState::Up => "Y".to_string(),
        }
    }

    fn from_target_type(board_config: Option<&str>, product_config: Option<&str>) -> String {
        match (board_config, product_config) {
            (None, None) => String::from("Unknown"),
            (board, product) => {
                format!("{}.{}", product.unwrap_or(UNKNOWN), board.unwrap_or(UNKNOWN))
            }
        }
    }

    fn from_target_state(t: ffx::TargetState) -> String {
        match t {
            ffx::TargetState::Unknown => "Unknown".to_string(),
            ffx::TargetState::Disconnected => "Disconnected".to_string(),
            ffx::TargetState::Product => "Product".to_string(),
            ffx::TargetState::Fastboot => "Fastboot".to_string(),
            ffx::TargetState::Zedboot => "Zedboot (R)".to_string(),
        }
    }

    fn from_is_manual(is_manual: Option<bool>) -> String {
        String::from(if is_manual.unwrap_or(false) { "Y" } else { "N" })
    }
}

impl TryFrom<(Option<usize>, ffx::TargetInfo)> for StringifiedTarget {
    type Error = StringifyError;

    fn try_from((index, target): (Option<usize>, ffx::TargetInfo)) -> Result<Self, Self::Error> {
        let target_type = StringifiedTarget::from_target_type(
            target.board_config.as_deref(),
            target.product_config.as_deref(),
        );
        Ok(Self {
            nodename: StringifiedField::String(nodename_to_string(index, target.nodename)),
            serial: StringifiedField::String(
                target.serial_number.unwrap_or_else(|| UNKNOWN.to_string()),
            ),
            addresses: StringifiedTarget::field_from_addresses(
                target.addresses.ok_or(StringifyError::MissingAddresses)?,
            ),
            rcs_state: StringifiedField::String(StringifiedTarget::from_rcs_state(
                target.rcs_state.ok_or(StringifyError::MissingRcsState)?,
            )),
            target_type: StringifiedField::String(target_type),
            target_state: StringifiedField::String(StringifiedTarget::from_target_state(
                target.target_state.ok_or(StringifyError::MissingTargetState)?,
            )),
            is_manual: StringifiedField::String(StringifiedTarget::from_is_manual(
                target.is_manual,
            )),
            ..Default::default()
        })
    }
}

impl TryFrom<(Option<usize>, ffx::TargetInfo)> for JsonTarget {
    type Error = StringifyError;

    fn try_from((index, target): (Option<usize>, ffx::TargetInfo)) -> Result<Self, Self::Error> {
        Ok(Self {
            nodename: nodename_to_string(index, target.nodename),
            serial: target.serial_number.unwrap_or_else(|| UNKNOWN.to_string()),
            addresses: target
                .addresses
                .unwrap_or(vec![])
                .drain(..)
                .map(JsonTargetAddress::from)
                .collect::<Vec<_>>(),
            rcs_state: StringifiedTarget::from_rcs_state(
                target.rcs_state.ok_or(StringifyError::MissingRcsState)?,
            ),
            target_type: StringifiedTarget::from_target_type(
                target.board_config.as_deref(),
                target.product_config.as_deref(),
            ),
            target_state: StringifiedTarget::from_target_state(
                target.target_state.ok_or(StringifyError::MissingTargetState)?,
            ),
            is_default: false.into(),
            is_manual: target.is_manual.unwrap_or(false),
        })
    }
}

pub struct TabularTargetFormatter {
    targets: Vec<StringifiedTarget>,
    limits: Limits,
}

impl TargetFormatter for TabularTargetFormatter {
    fn lines(&self, default_nodename: Option<&str>) -> Vec<String> {
        self.targets
            .iter()
            .map(|t| format_fields(t, &self.limits, default_nodename.unwrap_or("")))
            .collect()
    }
}

impl TryFrom<Vec<ffx::TargetInfo>> for TabularTargetFormatter {
    type Error = StringifyError;

    fn try_from(mut targets: Vec<ffx::TargetInfo>) -> Result<Self, Self::Error> {
        // First target is the table header in this case, since the formatting
        // for the table header is (for now) identical to the rest of the
        // targets
        let mut initial = vec![StringifiedTarget {
            nodename: StringifiedField::String(NAME.to_string()),
            serial: StringifiedField::String(SERIAL.to_string()),
            addresses: StringifiedField::String(ADDRS.to_string()),
            rcs_state: StringifiedField::String(RCS.to_string()),
            is_manual: StringifiedField::String(MANUAL.to_string()),
            target_type: StringifiedField::String(TYPE.to_string()),
            target_state: StringifiedField::String(STATE.to_string()),
            ..Default::default()
        }];
        let mut limits = Limits::default();
        limits.update(&mut initial[0]);

        let acc = Self { targets: initial, limits };
        let use_index = has_multiple_unknown_targets(&targets);
        Ok(targets.drain(..).enumerate().try_fold(acc, |mut a, (index, t)| {
            let index = if use_index { Some(index) } else { None };
            let mut s = StringifiedTarget::try_from((index, t))?;
            a.limits.update(&mut s);
            a.targets.push(s);
            Ok(a)
        })?)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use fidl_fuchsia_net::{Ipv4Address, Ipv6Address};
    use std::collections::HashMap;
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
    use std::sync::LazyLock;

    fn make_target(
        addr: fidl_fuchsia_developer_ffx::TargetAddrInfo,
    ) -> fidl_fuchsia_developer_ffx::TargetInfo {
        ffx::TargetInfo {
            nodename: Some("lorberding".to_string()),
            addresses: Some(vec![addr]),
            rcs_state: Some(ffx::RemoteControlState::Unknown),
            target_state: Some(ffx::TargetState::Unknown),
            ..Default::default()
        }
    }

    fn make_ip_v4_port_info(port: u16) -> fidl_fuchsia_developer_ffx::TargetAddrInfo {
        ffx::TargetAddrInfo::IpPort(ffx::TargetIpPort {
            ip: IpAddress::Ipv4(Ipv4Address { addr: [127, 0, 0, 1] }),
            scope_id: 0,
            port,
        })
    }

    fn make_ip_v6_port_info(
        scope_id: u32,
        port: u16,
    ) -> fidl_fuchsia_developer_ffx::TargetAddrInfo {
        ffx::TargetAddrInfo::IpPort(ffx::TargetIpPort {
            ip: IpAddress::Ipv6(Ipv6Address {
                addr: [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1],
            }),
            scope_id: scope_id,
            port: port,
        })
    }

    static EMPTY_FORMATTER_GOLDEN: LazyLock<String> = LazyLock::new(|| {
        include_str!("../test_data/target_formatter_empty_formatter_golden").trim().to_owned()
    });
    static ONE_TARGET_WITH_DEFAULT_GOLDEN: LazyLock<String> = LazyLock::new(|| {
        include_str!("../test_data/target_formatter_one_target_with_default_golden")
            .trim()
            .to_owned()
    });
    static ONE_TARGET_NO_DEFAULT_GOLDEN: LazyLock<String> = LazyLock::new(|| {
        include_str!("../test_data/target_formatter_one_target_no_default_golden").trim().to_owned()
    });
    static EMPTY_NODENAME_WITH_DEFAULT_GOLDEN: LazyLock<String> = LazyLock::new(|| {
        include_str!("../test_data/target_formatter_empty_nodename_with_default_golden")
            .trim()
            .to_owned()
    });
    static EMPTY_NODENAME_WITH_DEFAULT_MULTIPLE_UNKNOWN_GOLDEN: LazyLock<String> =
        LazyLock::new(|| {
            include_str!(
                "../test_data/target_formatter_empty_nodename_with_default_multiple_unknown_golden"
            )
            .trim()
            .to_owned()
        });
    static EMPTY_NODENAME_NO_DEFAULT_GOLDEN: LazyLock<String> = LazyLock::new(|| {
        include_str!("../test_data/target_formatter_empty_nodename_no_default_golden")
            .trim()
            .to_owned()
    });
    static SIMPLE_FORMATTER_WITH_DEFAULT_GOLDEN: LazyLock<String> = LazyLock::new(|| {
        include_str!("../test_data/target_formatter_simple_formatter_with_default_golden")
            .trim()
            .to_owned()
    });
    static NAME_ONLY_FORMATTER_WITH_DEFAULT_GOLDEN: LazyLock<String> = LazyLock::new(|| {
        include_str!("../test_data/target_formatter_name_only_formatter_with_default_golden")
            .trim()
            .to_owned()
    });
    static NAME_ONLY_FORMATTER_MULTIPLE_UNKNOWN_WITH_DEFAULT_GOLDEN: LazyLock<String> =
        LazyLock::new(|| {
            include_str!("../test_data/target_formatter_name_only_multiple_unknown_formatter_with_default_golden").trim().to_owned()
        });
    static DEVICE_FINDER_FORMAT_GOLDEN: LazyLock<String> = LazyLock::new(|| {
        include_str!("../test_data/target_formatter_device_finder_format_golden").trim().to_owned()
    });
    static DEVICE_FINDER_FORMAT_IPV4_ONLY_GOLDEN: LazyLock<String> = LazyLock::new(|| {
        include_str!("../test_data/target_formatter_device_finder_format_ipv4_only_golden")
            .trim()
            .to_owned()
    });
    static DEVICE_FINDER_FORMAT_IPV6_ONLY_GOLDEN: LazyLock<String> = LazyLock::new(|| {
        include_str!("../test_data/target_formatter_device_finder_format_ipv6_only_golden")
            .trim()
            .to_owned()
    });
    static ADDRESSES_FORMAT_GOLDEN: LazyLock<String> = LazyLock::new(|| {
        include_str!("../test_data/target_formatter_addresses_format_golden").trim().to_owned()
    });
    static BUILD_CONFIG_FULL_GOLDEN: LazyLock<String> = LazyLock::new(|| {
        include_str!("../test_data/target_formatter_build_config_full_golden").trim().to_owned()
    });
    static BUILD_CONFIG_PRODUCT_MISSING_GOLDEN: LazyLock<String> = LazyLock::new(|| {
        include_str!("../test_data/target_formatter_build_config_product_missing_golden")
            .trim()
            .to_owned()
    });
    static BUILD_CONFIG_BOARD_MISSING_GOLDEN: LazyLock<String> = LazyLock::new(|| {
        include_str!("../test_data/target_formatter_build_config_board_missing_golden")
            .trim()
            .to_owned()
    });
    static JSON_BUILD_CONFIG_FULL_GOLDEN: LazyLock<String> = LazyLock::new(|| {
        include_str!("../test_data/target_formatter_json_build_config_full_golden")
            .trim()
            .to_owned()
    });
    static JSON_BUILD_CONFIG_FULL_DEFAULT_TARGET_GOLDEN: LazyLock<String> = LazyLock::new(|| {
        include_str!("../test_data/target_formatter_json_build_config_full_default_target_golden")
            .trim()
            .to_owned()
    });
    static JSON_BUILD_CONFIG_PRODUCT_MISSING_GOLDEN: LazyLock<String> = LazyLock::new(|| {
        include_str!("../test_data/target_formatter_json_build_config_product_missing_golden")
            .trim()
            .to_owned()
    });
    static JSON_BUILD_CONFIG_BOARD_MISSING_GOLDEN: LazyLock<String> = LazyLock::new(|| {
        include_str!("../test_data/target_formatter_json_build_config_board_missing_golden")
            .trim()
            .to_owned()
    });
    static JSON_BUILD_CONFIG_BOTH_MISSING_GOLDEN: LazyLock<String> = LazyLock::new(|| {
        include_str!("../test_data/target_formatter_json_build_config_both_missing_golden")
            .trim()
            .to_owned()
    });

    fn make_valid_target() -> ffx::TargetInfo {
        ffx::TargetInfo {
            nodename: Some("fooberdoober".to_string()),
            addresses: Some(vec![
                ffx::TargetAddrInfo::Ip(ffx::TargetIp {
                    ip: IpAddress::Ipv6(Ipv6Address {
                        addr: [1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1],
                    }),
                    scope_id: 198,
                }),
                ffx::TargetAddrInfo::Ip(ffx::TargetIp {
                    ip: IpAddress::Ipv4(Ipv4Address { addr: [122, 24, 25, 25] }),
                    scope_id: 186,
                }),
            ]),
            rcs_state: Some(ffx::RemoteControlState::Unknown),
            target_state: Some(ffx::TargetState::Unknown),
            ..Default::default()
        }
    }

    fn make_valid_ipv4_only_target() -> ffx::TargetInfo {
        ffx::TargetInfo {
            nodename: Some("fooberdoober4".to_string()),
            addresses: Some(vec![ffx::TargetAddrInfo::Ip(ffx::TargetIp {
                ip: IpAddress::Ipv4(Ipv4Address { addr: [122, 24, 25, 25] }),
                scope_id: 186,
            })]),
            rcs_state: Some(ffx::RemoteControlState::Unknown),
            target_state: Some(ffx::TargetState::Unknown),
            ..Default::default()
        }
    }

    #[test]
    fn test_empty_formatter() {
        let formatter = TabularTargetFormatter::try_from(Vec::<ffx::TargetInfo>::new()).unwrap();
        let lines = formatter.lines(None);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].len(), 60); // Just some manual math.
        assert_eq!(lines.join("\n"), EMPTY_FORMATTER_GOLDEN.to_string());
    }

    #[fuchsia::test]
    async fn test_formatter_one_target() {
        let formatter = TabularTargetFormatter::try_from(vec![
            make_valid_target(),
            ffx::TargetInfo {
                nodename: Some("lorberding".to_string()),
                addresses: Some(vec![ffx::TargetAddrInfo::Ip(ffx::TargetIp {
                    ip: IpAddress::Ipv6(Ipv6Address {
                        addr: [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1],
                    }),
                    scope_id: 137,
                })]),
                rcs_state: Some(ffx::RemoteControlState::Unknown),
                target_state: Some(ffx::TargetState::Unknown),
                ..Default::default()
            },
        ])
        .unwrap();
        let lines = formatter.lines(Some("fooberdoober"));
        assert_eq!(lines.len(), 3);
        assert_eq!(lines.join("\n"), ONE_TARGET_WITH_DEFAULT_GOLDEN.to_string());

        let lines = formatter.lines(None);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines.join("\n"), ONE_TARGET_NO_DEFAULT_GOLDEN.to_string());
    }

    #[fuchsia::test]
    async fn test_formatter_empty_nodename() {
        let formatter = TabularTargetFormatter::try_from(vec![
            make_valid_target(),
            ffx::TargetInfo {
                nodename: None,
                addresses: Some(vec![ffx::TargetAddrInfo::Ip(ffx::TargetIp {
                    ip: IpAddress::Ipv6(Ipv6Address {
                        addr: [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1],
                    }),
                    scope_id: 137,
                })]),
                rcs_state: Some(ffx::RemoteControlState::Unknown),
                target_state: Some(ffx::TargetState::Unknown),
                serial_number: Some("cereal".to_owned()),
                ..Default::default()
            },
        ])
        .unwrap();
        let lines = formatter.lines(Some("fooberdoober"));
        assert_eq!(lines.len(), 3);
        assert_eq!(lines.join("\n"), EMPTY_NODENAME_WITH_DEFAULT_GOLDEN.to_string());

        let lines = formatter.lines(None);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines.join("\n"), EMPTY_NODENAME_NO_DEFAULT_GOLDEN.to_string());
    }

    #[fuchsia::test]
    async fn test_formatter_multiple_empty_nodename() {
        let formatter = TabularTargetFormatter::try_from(vec![
            make_valid_target(),
            ffx::TargetInfo {
                nodename: None,
                addresses: Some(vec![ffx::TargetAddrInfo::Ip(ffx::TargetIp {
                    ip: IpAddress::Ipv6(Ipv6Address {
                        addr: [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1],
                    }),
                    scope_id: 137,
                })]),
                rcs_state: Some(ffx::RemoteControlState::Unknown),
                target_state: Some(ffx::TargetState::Unknown),
                serial_number: Some("cereal".to_owned()),
                ..Default::default()
            },
            ffx::TargetInfo {
                nodename: None,
                addresses: Some(vec![ffx::TargetAddrInfo::Ip(ffx::TargetIp {
                    ip: IpAddress::Ipv6(Ipv6Address {
                        addr: [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 0],
                    }),
                    scope_id: 42,
                })]),
                rcs_state: Some(ffx::RemoteControlState::Unknown),
                target_state: Some(ffx::TargetState::Unknown),
                ..Default::default()
            },
        ])
        .unwrap();
        let lines = formatter.lines(Some("fooberdoober"));
        assert_eq!(lines.len(), 4);
        assert_eq!(
            lines.join("\n"),
            EMPTY_NODENAME_WITH_DEFAULT_MULTIPLE_UNKNOWN_GOLDEN.to_string()
        );
    }

    #[fuchsia::test]
    async fn test_simple_formatter() {
        let formatter = SimpleTargetFormatter::try_from(vec![
            make_valid_target(),
            ffx::TargetInfo {
                nodename: None,
                addresses: Some(vec![ffx::TargetAddrInfo::Ip(ffx::TargetIp {
                    ip: IpAddress::Ipv6(Ipv6Address {
                        addr: [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1],
                    }),
                    scope_id: 137,
                })]),
                rcs_state: Some(ffx::RemoteControlState::Unknown),
                target_state: Some(ffx::TargetState::Unknown),
                ..Default::default()
            },
        ])
        .unwrap();
        let lines = formatter.lines(Some("fooberdoober"));
        assert_eq!(lines.len(), 2);
        assert_eq!(lines.join("\n").trim(), SIMPLE_FORMATTER_WITH_DEFAULT_GOLDEN.to_string());

        let lines = formatter.lines(None);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines.join("\n").trim(), SIMPLE_FORMATTER_WITH_DEFAULT_GOLDEN.to_string());
    }

    #[test]
    fn test_simple_formatter_with_invalid() {
        let names =
            vec!["nodename0", "nodename1", "nodename2", "nodename3", "nodename4", "nodename5"];
        let mut targets = names
            .into_iter()
            .map(|name| {
                let mut t = make_valid_target();
                t.nodename = Some(name.to_string());
                t
            })
            .collect::<Vec<_>>();

        targets[1].addresses = None;
        targets[3].rcs_state = None;

        let formatter = SimpleTargetFormatter::try_from(targets).unwrap();
        assert_eq!(formatter.targets.len(), 5);
    }

    #[fuchsia::test]
    async fn test_name_only_formatter() {
        let formatter = NameOnlyTargetFormatter::try_from(vec![
            make_valid_target(),
            ffx::TargetInfo {
                nodename: None,
                addresses: Some(vec![ffx::TargetAddrInfo::Ip(ffx::TargetIp {
                    ip: IpAddress::Ipv6(Ipv6Address {
                        addr: [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1],
                    }),
                    scope_id: 137,
                })]),
                rcs_state: Some(ffx::RemoteControlState::Unknown),
                target_state: Some(ffx::TargetState::Unknown),
                ..Default::default()
            },
        ])
        .unwrap();
        let lines = formatter.lines(Some("fooberdoober"));
        assert_eq!(lines.len(), 2);
        assert_eq!(lines.join("\n"), NAME_ONLY_FORMATTER_WITH_DEFAULT_GOLDEN.to_string());

        let lines = formatter.lines(None);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines.join("\n"), NAME_ONLY_FORMATTER_WITH_DEFAULT_GOLDEN.to_string());
    }

    #[fuchsia::test]
    async fn test_name_only_multiple_unknown_formatter() {
        let formatter = NameOnlyTargetFormatter::try_from(vec![
            make_valid_target(),
            ffx::TargetInfo {
                nodename: None,
                addresses: Some(vec![ffx::TargetAddrInfo::Ip(ffx::TargetIp {
                    ip: IpAddress::Ipv6(Ipv6Address {
                        addr: [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1],
                    }),
                    scope_id: 137,
                })]),
                rcs_state: Some(ffx::RemoteControlState::Unknown),
                target_state: Some(ffx::TargetState::Unknown),
                ..Default::default()
            },
            ffx::TargetInfo {
                nodename: None,
                addresses: Some(vec![ffx::TargetAddrInfo::Ip(ffx::TargetIp {
                    ip: IpAddress::Ipv6(Ipv6Address {
                        addr: [0xfe, 0x80, 0, 0, 0, 0, 0, 1, 0, 1, 1, 1, 1, 1, 1, 1],
                    }),
                    scope_id: 42,
                })]),
                rcs_state: Some(ffx::RemoteControlState::Unknown),
                target_state: Some(ffx::TargetState::Unknown),
                ..Default::default()
            },
        ])
        .unwrap();
        let lines = formatter.lines(Some("fooberdoober"));
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines.join("\n"),
            NAME_ONLY_FORMATTER_MULTIPLE_UNKNOWN_WITH_DEFAULT_GOLDEN.to_string()
        );

        let lines = formatter.lines(None);
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines.join("\n"),
            NAME_ONLY_FORMATTER_MULTIPLE_UNKNOWN_WITH_DEFAULT_GOLDEN.to_string()
        );
    }

    #[test]
    fn test_name_only_formatter_with_invalid() {
        let names =
            vec!["nodename0", "nodename1", "nodename2", "nodename3", "nodename4", "nodename5"];
        let mut targets = names
            .into_iter()
            .map(|name| {
                let mut t = make_valid_target();
                t.nodename = Some(name.to_string());
                t
            })
            .collect::<Vec<_>>();

        targets[1].addresses = None;
        targets[3].rcs_state = None;

        let formatter = NameOnlyTargetFormatter::try_from(targets).unwrap();
        // NameOnlyTargetFormatter is infalliable
        assert_eq!(formatter.targets.len(), 6);
    }

    #[test]
    fn test_stringified_target_missing_state() {
        let mut t = make_valid_target();
        t.target_state = None;
        assert_eq!(StringifiedTarget::try_from((None, t)), Err(StringifyError::MissingTargetState));
    }

    #[test]
    fn test_stringified_target_missing_rcs_state() {
        let mut t = make_valid_target();
        t.rcs_state = None;
        assert_eq!(StringifiedTarget::try_from((None, t)), Err(StringifyError::MissingRcsState));
    }

    #[test]
    fn test_stringified_target_missing_addresses() {
        let mut t = make_valid_target();
        t.addresses = None;
        assert_eq!(StringifiedTarget::try_from((None, t)), Err(StringifyError::MissingAddresses));
    }

    #[test]
    fn test_stringified_target_missing_nodename() {
        let mut t = make_valid_target();
        t.nodename = None;
        assert!(StringifiedTarget::try_from((None, t)).is_ok());
    }

    #[test]
    fn test_device_finder_format() {
        let formatter = Box::<dyn TargetFormatter>::try_from((
            Format::Simple,
            AddressTypes::All,
            vec![make_valid_target(), make_valid_target()],
        ))
        .unwrap();
        let lines = formatter.lines(None);
        assert_eq!(lines.join("\n"), DEVICE_FINDER_FORMAT_GOLDEN.to_string());
    }

    #[test]
    fn test_device_finder_format_ipv4_only() {
        let formatter = Box::<dyn TargetFormatter>::try_from((
            Format::Simple,
            AddressTypes::Ipv4Only,
            vec![make_valid_ipv4_only_target(), make_valid_target()],
        ))
        .unwrap();
        let lines = formatter.lines(None);
        assert_eq!(lines.join("\n"), DEVICE_FINDER_FORMAT_IPV4_ONLY_GOLDEN.to_string());
    }

    #[test]
    fn test_device_finder_format_ipv6_only() {
        let formatter = Box::<dyn TargetFormatter>::try_from((
            Format::Simple,
            AddressTypes::Ipv6Only,
            vec![make_valid_ipv4_only_target(), make_valid_target()],
        ))
        .unwrap();
        let lines = formatter.lines(None);
        assert_eq!(lines.join("\n"), DEVICE_FINDER_FORMAT_IPV6_ONLY_GOLDEN.to_string());
    }

    #[test]
    fn test_addresses_format() {
        let formatter = Box::<dyn TargetFormatter>::try_from((
            Format::Addresses,
            AddressTypes::All,
            vec![make_valid_target(), make_valid_target()],
        ))
        .unwrap();
        let lines = formatter.lines(None);
        assert_eq!(lines.join("\n"), ADDRESSES_FORMAT_GOLDEN.to_string());
    }

    #[test]
    fn test_build_config_full() {
        let b = String::from("board");
        let p = String::from("default");
        let mut t = make_valid_target();
        t.board_config = Some(b);
        t.product_config = Some(p);
        let formatter = TabularTargetFormatter::try_from(vec![t]).unwrap();
        let lines = formatter.lines(None);
        assert_eq!(lines.join("\n").trim(), BUILD_CONFIG_FULL_GOLDEN.to_string());
    }

    #[test]
    fn test_build_config_product_missing() {
        let b = String::from("x64");
        let mut t = make_valid_target();
        t.board_config = Some(b);
        t.product_config = None;
        let formatter = TabularTargetFormatter::try_from(vec![t]).unwrap();
        let lines = formatter.lines(None);
        assert_eq!(lines.join("\n").trim(), BUILD_CONFIG_PRODUCT_MISSING_GOLDEN.to_string());
    }

    #[test]
    fn test_build_config_board_missing() {
        let p = String::from("foo");
        let mut t = make_valid_target();
        t.board_config = None;
        t.product_config = Some(p);
        let formatter = TabularTargetFormatter::try_from(vec![t]).unwrap();
        let lines = formatter.lines(None);
        assert_eq!(lines.join("\n").trim(), BUILD_CONFIG_BOARD_MISSING_GOLDEN.to_string());
    }

    #[test]
    fn test_to_ssh_addr() {
        let sockets = vec![
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 0)),
            SocketAddr::V6(SocketAddrV6::new("f111::3".parse().unwrap(), 0, 0, 0)),
            SocketAddr::V6(SocketAddrV6::new("fe80::1".parse().unwrap(), 0, 0, 0)),
            SocketAddr::V6(SocketAddrV6::new("fe80::2".parse().unwrap(), 0, 0, 1)),
            SocketAddr::V6(SocketAddrV6::new("fe80::3".parse().unwrap(), 0, 0, 0)),
        ];
        let addrs = sockets.iter().map(|s| TargetAddr::from(*s)).collect::<Vec<_>>();
        assert_eq!((&addrs).to_ssh_addr(), Some(addrs[3]));

        let sockets = vec![
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 0)),
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(129, 0, 0, 1), 0)),
        ];
        let addrs = sockets.iter().map(|s| TargetAddr::from(*s)).collect::<Vec<_>>();
        assert_eq!((&addrs).to_ssh_addr(), Some(addrs[0]));

        let addrs = Vec::<TargetAddr>::new();
        assert_eq!((&addrs).to_ssh_addr(), None);
    }

    #[test]
    fn test_stringified_product_state() {
        let mut t = make_valid_target();
        t.target_state = Some(ffx::TargetState::Product);
        assert!(StringifiedTarget::try_from((None, t)).is_ok());
    }

    #[test]
    fn test_stringified_fastboot_state() {
        let mut t = make_valid_target();
        t.target_state = Some(ffx::TargetState::Fastboot);
        assert!(StringifiedTarget::try_from((None, t)).is_ok());
    }

    #[test]
    fn test_stringified_unknown_state() {
        let mut t = make_valid_target();
        t.target_state = Some(ffx::TargetState::Unknown);
        assert!(StringifiedTarget::try_from((None, t)).is_ok());
    }

    #[test]
    fn test_stringified_disconnected_state() {
        let mut t = make_valid_target();
        t.target_state = Some(ffx::TargetState::Disconnected);
        assert!(StringifiedTarget::try_from((None, t)).is_ok());
    }

    #[test]
    fn test_addresses_target_formatter_some_invalid() {
        let names =
            vec!["nodename0", "nodename1", "nodename2", "nodename3", "nodename4", "nodename5"];
        let mut targets = names
            .into_iter()
            .map(|name| {
                let mut t = make_valid_target();
                t.nodename = Some(name.to_string());
                t
            })
            .collect::<Vec<_>>();

        targets[1].addresses = None;
        targets[3].rcs_state = None;
        targets[4].addresses = None;

        let formatter = AddressesTargetFormatter::try_from(targets).unwrap();
        assert_eq!(formatter.targets.len(), 4);
    }
    #[test]
    fn test_json_target_formatter_valid() {
        let names =
            vec!["nodename0", "nodename1", "nodename2", "nodename3", "nodename4", "nodename5"];
        let targets = names
            .into_iter()
            .map(|name| {
                let mut t = make_valid_target();
                t.nodename = Some(name.to_string());
                t
            })
            .collect::<Vec<_>>();

        let formatter = JsonTargetFormatter::try_from(targets).unwrap();
        assert_eq!(formatter.targets.len(), 6);
    }

    #[test]
    fn test_json_target_formatter_some_invalid() {
        let names =
            vec!["nodename0", "nodename1", "nodename2", "nodename3", "nodename4", "nodename5"];
        let mut targets = names
            .into_iter()
            .map(|name| {
                let mut t = make_valid_target();
                t.nodename = Some(name.to_string());
                t
            })
            .collect::<Vec<_>>();

        targets[1].target_state = None;
        targets[3].rcs_state = None;

        let formatter = JsonTargetFormatter::try_from(targets).unwrap();
        assert_eq!(formatter.targets.len(), 4);
    }

    #[test]
    fn test_json_formatter_build_config_full() {
        let b = String::from("board");
        let p = String::from("default");
        let mut t = make_valid_target();
        t.board_config = Some(b);
        t.product_config = Some(p);
        let formatter = JsonTargetFormatter::try_from(vec![t]).unwrap();
        let lines = formatter.lines(None);
        assert_eq!(lines.join("\n"), JSON_BUILD_CONFIG_FULL_GOLDEN.to_string());
    }

    #[test]
    fn test_json_formatter_build_config_full_default_target() {
        let b = String::from("board");
        let p = String::from("default");
        let mut t = make_valid_target();
        t.board_config = Some(b);
        t.product_config = Some(p);
        let formatter = JsonTargetFormatter::try_from(vec![t]).unwrap();
        let lines = formatter.lines(Some("fooberdoober"));
        assert_eq!(lines.join("\n"), JSON_BUILD_CONFIG_FULL_DEFAULT_TARGET_GOLDEN.to_string());
    }

    #[test]
    fn test_json_formatter_build_config_full_default_target_set_default_target() {
        let b = String::from("board");
        let p = String::from("default");
        let mut t = make_valid_target();
        t.board_config = Some(b);
        t.product_config = Some(p);
        let mut formatter = JsonTargetFormatter::try_from(vec![t]).unwrap();
        JsonTargetFormatter::set_default_target(&mut formatter.targets, Some("fooberdoober"));
        let lines = vec![serde_json::to_string(&formatter.targets).expect("should serialize")];
        assert_eq!(lines.join("\n"), JSON_BUILD_CONFIG_FULL_DEFAULT_TARGET_GOLDEN.to_string());
    }

    #[test]
    fn test_json_formatter_build_config_product_missing() {
        let b = String::from("x64");
        let mut t = make_valid_target();
        t.board_config = Some(b);
        t.product_config = None;
        let formatter = JsonTargetFormatter::try_from(vec![t]).unwrap();
        let lines = formatter.lines(None);
        assert_eq!(lines.join("\n"), JSON_BUILD_CONFIG_PRODUCT_MISSING_GOLDEN.to_string());
    }

    #[test]
    fn test_json_formatter_build_config_board_missing() {
        let p = String::from("foo");
        let mut t = make_valid_target();
        t.board_config = None;
        t.product_config = Some(p);
        let formatter = JsonTargetFormatter::try_from(vec![t]).unwrap();
        let lines = formatter.lines(None);
        assert_eq!(lines.join("\n"), JSON_BUILD_CONFIG_BOARD_MISSING_GOLDEN.to_string());
    }

    #[test]
    fn test_json_formatter_build_config_both_missing() {
        let mut t = make_valid_target();
        t.board_config = None;
        t.product_config = None;
        let formatter = JsonTargetFormatter::try_from(vec![t]).unwrap();
        let lines = formatter.lines(None);
        assert_eq!(lines.join("\n"), JSON_BUILD_CONFIG_BOTH_MISSING_GOLDEN.to_string());
    }

    fn get_first_address(json: &str) -> (String, u16) {
        let parsed_json: Vec<HashMap<String, serde_json::Value>> =
            serde_json::from_str(&json).unwrap();
        let addresses: Vec<serde_json::Value> =
            serde_json::from_value(parsed_json[0]["addresses"].clone()).unwrap();
        let first_address: HashMap<String, serde_json::Value> =
            serde_json::from_value(addresses[0].clone()).unwrap();
        let ip = serde_json::from_value(first_address["ip"].clone()).unwrap();
        let port = serde_json::from_value(first_address["ssh_port"].clone()).unwrap();
        (ip, port)
    }

    #[fuchsia::test]
    async fn test_nonstandard_port_ipv4() {
        let target = make_target(make_ip_v4_port_info(1234));
        let formatter = JsonTargetFormatter::try_from(vec![target.clone()]).unwrap();
        let json = formatter.lines(None)[0].clone();
        let (first_ip, first_port) = get_first_address(&json);
        assert_eq!(first_ip, "127.0.0.1".to_string());
        assert_eq!(first_port, 1234);

        let formatter = TabularTargetFormatter::try_from(vec![target.clone()]).unwrap();
        let out = formatter.lines(None)[1].clone();
        assert!(out.contains("127.0.0.1:1234"));

        let formatter = AddressesTargetFormatter::try_from(vec![target.clone()]).unwrap();
        let out = formatter.lines(None)[0].clone();
        assert_eq!(out, "127.0.0.1:1234".to_string());
    }

    #[fuchsia::test]
    async fn test_nonstandard_port_ipv6() {
        let addr = make_ip_v6_port_info(42, 1234);
        let target = make_target(addr);
        let formatter = JsonTargetFormatter::try_from(vec![target.clone()]).unwrap();
        let json = formatter.lines(None)[0].clone();
        let (first_ip, first_port) = get_first_address(&json);
        assert_eq!(first_ip, "fe80::1:101:101:101%42".to_string());
        assert_eq!(first_port, 1234);

        let formatter = TabularTargetFormatter::try_from(vec![target.clone()]).unwrap();
        let out = formatter.lines(None)[1].clone();
        assert!(out.contains("[fe80::1:101:101:101%42]:1234"));

        let formatter = AddressesTargetFormatter::try_from(vec![target.clone()]).unwrap();
        let out = formatter.lines(None)[0].clone();
        assert_eq!(out, "[fe80::1:101:101:101%42]:1234".to_string());
    }

    #[fuchsia::test]
    async fn test_addresses_std_ports() {
        let target = make_target(make_ip_v4_port_info(0));

        let formatter = AddressesTargetFormatter::try_from(vec![target.clone()]).unwrap();
        let out = formatter.lines(None)[0].clone();
        assert_eq!(out, "127.0.0.1:22".to_string());

        let target = make_target(make_ip_v6_port_info(0, 22));
        let formatter = AddressesTargetFormatter::try_from(vec![target.clone()]).unwrap();
        let out = formatter.lines(None)[0].clone();
        assert_eq!(out, "[fe80::1:101:101:101]:22".to_string());
    }
}
