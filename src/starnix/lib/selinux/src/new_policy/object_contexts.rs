// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! System object context labeling rules parsed from SELinux binary policy.

use super::context::Context;
use super::error::{ParseError, SerializeError};
use super::metadata::PolicyVersion;
use super::parser::{Array, ByteArray, PolicyCursor, PolicyWriter};
use super::traits::{Parse, Serialize};
use selinux_policy_derive::{Parse, Serialize, Validate};

/// Named context pair mapping a filesystem type string to mount and root security [`Context`]s.
///
/// Corresponding SELinux text policy syntax: `fs_con <fs_type> <fs_context> <root_context>`.
#[derive(Debug, Clone, Parse, Serialize, Validate)]
pub struct FilesystemContext {
    name: ByteArray,
    fs_context: Context,
    root_context: Context,
}

impl FilesystemContext {
    /// Returns the filesystem type name bytes (`<fs_type>`).
    #[cfg(test)]
    pub fn name(&self) -> &[u8] {
        &self.name
    }

    /// Returns the security [`Context`] applied to the filesystem mount (`<fs_context>`).
    #[cfg(test)]
    pub fn fs_context(&self) -> &Context {
        &self.fs_context
    }

    /// Returns the security [`Context`] applied to the filesystem root directory (`<root_context>`).
    #[cfg(test)]
    pub fn root_context(&self) -> &Context {
        &self.root_context
    }
}

/// Named context pair mapping a network interface string to device and packet security [`Context`]s.
///
/// Corresponding SELinux text policy syntax: `netifcon <interface_name> <if_context> <packet_context>`.
#[derive(Debug, Clone, Parse, Serialize, Validate)]
pub struct NetworkInterfaceContext {
    name: ByteArray,
    if_context: Context,
    msg_context: Context,
}

impl NetworkInterfaceContext {
    /// Returns the network interface name bytes (`<interface_name>`).
    #[cfg(test)]
    pub fn name(&self) -> &[u8] {
        &self.name
    }
}

/// Port specification mapping a protocol and port range to a security [`Context`].
///
/// Corresponding SELinux text policy syntax: `portcon <protocol> <port_low>-<port_high> <context>`.
#[derive(Debug, Clone, Parse, Serialize, Validate)]
pub struct PortContext {
    protocol: u32,
    low_port: u32,
    high_port: u32,
    context: Context,
}

impl PortContext {
    /// Returns the security [`Context`].
    #[cfg(test)]
    pub fn context(&self) -> &Context {
        &self.context
    }
}

/// IPv4 node specification mapping an address and mask to a security [`Context`].
///
/// Corresponding SELinux text policy syntax: `nodecon <ipv4_addr> <netmask> <context>`.
#[derive(Debug, Clone, Parse, Serialize, Validate)]
pub struct IPv4NodeContext {
    address: u32,
    mask: u32,
    context: Context,
}

impl IPv4NodeContext {
    /// Returns the IPv4 address.
    #[cfg(test)]
    pub fn address(&self) -> u32 {
        self.address
    }

    /// Returns the subnet mask.
    #[cfg(test)]
    pub fn mask(&self) -> u32 {
        self.mask
    }

    /// Returns the security [`Context`].
    #[cfg(test)]
    pub fn context(&self) -> &Context {
        &self.context
    }
}

/// Discriminates among the different kinds of `fs_use_*` labeling statements in policy.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq, Parse, Serialize, Validate)]
#[policy(wire_type = u32)]
pub enum FsUseType {
    Xattr = 1,
    Trans = 2,
    Task = 3,
}

/// Filesystem labeling behavior rule (`fs_use_xattr`, `fs_use_trans`, `fs_use_task`).
///
/// Corresponding SELinux text policy syntax: `fs_use_xattr <fs_type> <context>`,
/// `fs_use_trans <fs_type> <context>`, or `fs_use_task <fs_type> <context>`.
#[derive(Debug, Clone, Parse, Serialize, Validate)]
pub struct FsUse {
    behavior: FsUseType,
    name: ByteArray,
    context: Context,
}

impl FsUse {
    /// Returns the filesystem use statement behavior type.
    pub fn behavior(&self) -> FsUseType {
        self.behavior
    }

    /// Returns the filesystem type name bytes (`<fs_type>`).
    pub fn fs_type(&self) -> &[u8] {
        &self.name
    }

    /// Returns the security [`Context`].
    pub fn context(&self) -> &Context {
        &self.context
    }
}

/// IPv6 node specification mapping a 128-bit address and mask to a security [`Context`].
///
/// Corresponding SELinux text policy syntax: `nodecon <ipv6_addr> <netmask> <context>`.
#[derive(Debug, Clone, Parse, Serialize, Validate)]
pub struct IPv6NodeContext {
    address: [u32; 4],
    mask: [u32; 4],
    context: Context,
}

impl IPv6NodeContext {
    /// Returns the IPv6 address 32-bit words by value.
    #[cfg(test)]
    pub fn address(&self) -> [u32; 4] {
        self.address
    }

    /// Returns the IPv6 mask 32-bit words by value.
    #[cfg(test)]
    pub fn mask(&self) -> [u32; 4] {
        self.mask
    }

    /// Returns the security [`Context`].
    #[cfg(test)]
    pub fn context(&self) -> &Context {
        &self.context
    }
}

/// InfiniBand partition key specification (for policy versions >= [`PolicyVersion::MIN_INFINIBAND`]).
///
/// Corresponding SELinux text policy syntax: `ibpkeycon <subnet_prefix> <pkey_low>-<pkey_high> <context>`.
#[derive(Debug, Clone, Parse, Serialize, Validate)]
pub struct InfiniBandPartitionKey {
    low: u32,
    high: u32,
    context: Context,
}

impl InfiniBandPartitionKey {
    /// Returns the low partition key bound.
    #[cfg(test)]
    pub fn low(&self) -> u32 {
        self.low
    }

    /// Returns the high partition key bound.
    #[cfg(test)]
    pub fn high(&self) -> u32 {
        self.high
    }

    /// Returns the security [`Context`].
    #[cfg(test)]
    pub fn context(&self) -> &Context {
        &self.context
    }
}

/// InfiniBand end port specification (for policy versions >= [`PolicyVersion::MIN_INFINIBAND`]).
///
/// Corresponding SELinux text policy syntax: `ibendportcon <device_name> <port> <context>`.
#[derive(Debug, Clone, Parse, Serialize, Validate)]
pub struct InfiniBandEndPort {
    name: ByteArray,
    port: u32,
    context: Context,
}

impl InfiniBandEndPort {
    /// Returns the InfiniBand device name string bytes (`<device_name>`).
    #[cfg(test)]
    pub fn name(&self) -> &[u8] {
        &self.name
    }

    /// Returns the security [`Context`].
    #[cfg(test)]
    pub fn context(&self) -> &Context {
        &self.context
    }
}

/// Container for system object context labeling statements (`fs_con`, `portcon`, `netifcon`,
/// `nodecon`, `fs_use_*`, `ibpkeycon`, `ibendportcon`).
#[derive(Debug, Clone, Validate)]
pub struct ObjectContexts {
    filesystems: Array<FilesystemContext>,
    ports: Array<PortContext>,
    network_interfaces: Array<NetworkInterfaceContext>,
    ipv4_nodes: Array<IPv4NodeContext>,
    fs_uses: Array<FsUse>,
    ipv6_nodes: Array<IPv6NodeContext>,
    infiniband_partition_keys: Array<InfiniBandPartitionKey>,
    infiniband_end_ports: Array<InfiniBandEndPort>,
}

impl ObjectContexts {
    /// Returns the filesystem labeling statements table.
    #[cfg(test)]
    pub fn filesystems(&self) -> &[FilesystemContext] {
        &self.filesystems
    }

    /// Returns the network port labeling statements table.
    #[cfg(test)]
    pub fn ports(&self) -> &[PortContext] {
        &self.ports
    }

    /// Returns the network interface labeling statements table.
    #[cfg(test)]
    pub fn network_interfaces(&self) -> &[NetworkInterfaceContext] {
        &self.network_interfaces
    }

    /// Returns the IPv4 node labeling statements table.
    #[cfg(test)]
    pub fn ipv4_nodes(&self) -> &[IPv4NodeContext] {
        &self.ipv4_nodes
    }

    /// Returns the filesystem behavior labeling statements table.
    pub fn fs_uses(&self) -> &[FsUse] {
        &self.fs_uses
    }

    /// Returns the IPv6 node labeling statements table.
    #[cfg(test)]
    pub fn ipv6_nodes(&self) -> &[IPv6NodeContext] {
        &self.ipv6_nodes
    }

    /// Returns the InfiniBand partition key labeling statements table.
    #[cfg(test)]
    pub fn infiniband_partition_keys(&self) -> &[InfiniBandPartitionKey] {
        &self.infiniband_partition_keys
    }

    /// Returns the InfiniBand end port labeling statements table.
    #[cfg(test)]
    pub fn infiniband_end_ports(&self) -> &[InfiniBandEndPort] {
        &self.infiniband_end_ports
    }
}

impl Parse for ObjectContexts {
    fn parse(cursor: &mut PolicyCursor<'_>) -> Result<Self, ParseError> {
        let filesystems = Array::<FilesystemContext>::parse(cursor)?;
        let ports = Array::<PortContext>::parse(cursor)?;
        let network_interfaces = Array::<NetworkInterfaceContext>::parse(cursor)?;
        let ipv4_nodes = Array::<IPv4NodeContext>::parse(cursor)?;
        let fs_uses = Array::<FsUse>::parse(cursor)?;
        let ipv6_nodes = Array::<IPv6NodeContext>::parse(cursor)?;
        let (infiniband_partition_keys, infiniband_end_ports) =
            if cursor.policy_version() >= PolicyVersion::MIN_INFINIBAND {
                (
                    Array::<InfiniBandPartitionKey>::parse(cursor)?,
                    Array::<InfiniBandEndPort>::parse(cursor)?,
                )
            } else {
                (Array::default(), Array::default())
            };

        Ok(Self {
            filesystems,
            ports,
            network_interfaces,
            ipv4_nodes,
            fs_uses,
            ipv6_nodes,
            infiniband_partition_keys,
            infiniband_end_ports,
        })
    }
}

impl Serialize for ObjectContexts {
    fn serialize(&self, writer: &mut PolicyWriter<'_>) -> Result<(), SerializeError> {
        self.filesystems.serialize(writer)?;
        self.ports.serialize(writer)?;
        self.network_interfaces.serialize(writer)?;
        self.ipv4_nodes.serialize(writer)?;
        self.fs_uses.serialize(writer)?;
        self.ipv6_nodes.serialize(writer)?;
        if writer.version() >= PolicyVersion::MIN_INFINIBAND {
            self.infiniband_partition_keys.serialize(writer)?;
            self.infiniband_end_ports.serialize(writer)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::new_policy::context::{MlsLevel, MlsRange};
    use crate::new_policy::traits::PolicyId;
    use crate::new_policy::{CategorySet, NewPolicy, RoleId, SensitivityId, TypeId, UserId};

    #[test]
    fn test_object_contexts_minimal_policy() {
        let policy_bytes =
            include_bytes!("../../testdata/composite_policies/compiled/minimal_policy");
        let policy = NewPolicy::parse(policy_bytes).expect("parse minimal_policy");
        policy.validate().expect("validate minimal_policy");

        let object_contexts = policy.object_contexts();
        assert!(object_contexts.filesystems().is_empty());
        assert!(object_contexts.ports().is_empty());
        assert!(object_contexts.network_interfaces().is_empty());
        assert!(object_contexts.ipv4_nodes().is_empty());
        assert!(object_contexts.ipv6_nodes().is_empty());
        assert!(object_contexts.infiniband_partition_keys().is_empty());
        assert!(object_contexts.infiniband_end_ports().is_empty());
    }

    #[test]
    fn test_ipv6_node_context_getters_by_value() {
        let node = IPv6NodeContext {
            address: [1, 2, 3, 4],
            mask: [0xff, 0xff, 0, 0],
            context: Context::new(
                UserId::from_u32(1).unwrap(),
                RoleId::from_u32(1).unwrap(),
                TypeId::from_u32(1).unwrap(),
                MlsRange::new(
                    MlsLevel::new(SensitivityId::from_u32(1).unwrap(), CategorySet::empty()),
                    None,
                ),
            ),
        };
        let addr: [u32; 4] = node.address();
        let mask: [u32; 4] = node.mask();
        assert_eq!(addr, [1, 2, 3, 4]);
        assert_eq!(mask, [0xff, 0xff, 0, 0]);
    }
}
