// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Kernel classes and permissions are added here when the relevant hook and enforcement is added.
use crate::policy::AccessVector;
use fuchsia_rcu::RcuDroppable;
use paste::paste;
use strum_macros::VariantArray;

/// Declares an `enum` with a `name()` method that returns the name for the given variant.
macro_rules! named_enum {
    ($(#[$meta:meta])* $name:ident {
        $($(#[$variant_meta:meta])* $variant:ident ($variant_name:literal),)*
    }) => {
        $(#[$meta])*
        pub enum $name  {
            $($(#[$variant_meta])* $variant,)*
        }

        impl $name {
            pub const fn name(&self) -> &'static str {
                match self {
                    $($name::$variant => $variant_name,)*
                }
            }
        }
    }
}

/// Declares an `enum` with the specified subset of values from an existing enum, and implements
/// [`ForClass`] for its common permission type.
macro_rules! subset_enum {
    ($(#[$meta:meta])* $name:ident (common $common_permission:ident) from $existing_enum:ident {
        $($(#[$variant_meta:meta])* $variant:ident,)*
    }) => {
        $(#[$meta])*
        pub enum $name {
            $($(#[$variant_meta])* $variant = $existing_enum::$variant as isize,)*
        }

        impl From<$name> for $existing_enum {
            #[inline]
            fn from(other: $name) -> Self {
                match other {
                    $($name::$variant => Self::$variant,)*
                }
            }
        }

        paste! {
            impl ForClass<$name> for $common_permission {
                fn for_class(&self, class: $name) -> KernelPermission {
                    match class {
                        $($name::$variant => [<$variant Permission>]::from(*self).into(),)*
                    }
                }
            }
        }
    }
}

macro_rules! declare_kernel_classes {
    ($(#[$meta:meta])* {
        $($(#[$variant_meta:meta])* $variant:ident ($variant_name:literal),)*
    }) => {
        named_enum! {
            #[derive(VariantArray, zerocopy::IntoBytes, zerocopy::Immutable, RcuDroppable)]
            $(#[$meta])* KernelClass {
                $($(#[$variant_meta])* $variant ($variant_name),)*
            }
        }

        paste! {
            $(#[$meta])*
            pub enum KernelPermission {
                $($(#[$variant_meta])* $variant([<$variant Permission>]),)*
            }

            $(impl From<[<$variant Permission>]> for KernelPermission {
                #[inline]
                fn from(v: [<$variant Permission>]) -> Self {
                    Self::$variant(v)
                }
            }
            )*

            impl ClassPermission for KernelPermission {
                #[inline]
                fn class(&self) -> KernelClass {
                    match self {
                        $(KernelPermission::$variant(_) => KernelClass::$variant),*
                    }
                }
                #[inline]
                fn id(&self) -> u8 {
                    match self {
                        $(KernelPermission::$variant(v) => v.id()),*
                    }
                }
            }

            impl KernelPermission {
                pub const fn name(&self) -> &'static str {
                    match self {
                        $(KernelPermission::$variant(v) => v.name()),*
                    }
                }
            }

            impl KernelClass {
                /// Returns the [`KernelClass`] corresponding to the supplied policy class `name`,
                /// if any.
                pub fn from_name(name: &str) -> Option<Self> {
                    match name {
                        $($variant_name => Some(Self::$variant),)*
                        _ => None,
                    }
                }

                pub(super) const fn permissions(&self) -> &'static [KernelPermission] {
                    match *self {
                        $(KernelClass::$variant => [<$variant Permission>]::PERMISSIONS,)*
                    }
                }
            }
        }
    }
}

declare_kernel_classes! {
    /// Well-known class in SELinux policy that has a particular meaning in policy enforcement
    /// hooks.
    #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
    #[repr(u32)]
    {
        // keep-sorted start
        /// SELinux "anon_inode" object class.
        AnonFsNode("anon_inode"),
        /// SELinux "binder" object class.
        Binder("binder"),
        /// SELinux "blk_file" object class.
        BlkFile("blk_file"),
        /// SELinux "bpf" object class.
        Bpf("bpf"),
        /// SELinux "capability" object class.
        Capability("capability"),
        /// SELinux "capability2" object class.
        Capability2("capability2"),
        /// SELinux "chr_file" object class.
        ChrFile("chr_file"),
        /// SELinux "dir" object class.
        Dir("dir"),
        /// SELinux "fd" object class.
        Fd("fd"),
        /// SELinux "fifo_file" object class.
        FifoFile("fifo_file"),
        /// SELinux "file" object class.
        File("file"),
        /// SELinux "filesystem" object class.
        FileSystem("filesystem"),
        /// SELinux "icmp_socket" class enabled via the "extended_socket_class" policy capability.
        IcmpSocket("icmp_socket"),
        /// SELinux "key" object class.
        Key("key"),
        /// SELinux "key_socket" object class.
        KeySocket("key_socket"),
        /// SELinux "lnk_file" object class.
        LnkFile("lnk_file"),
        /// SELinux "memfd_file" object class.
        MemFdFile("memfd_file"),
        /// SELinux "netlink_audit_socket" object class.
        NetlinkAuditSocket("netlink_audit_socket"),
        /// SELinux "netlink_connector_socket" object class.
        NetlinkConnectorSocket("netlink_connector_socket"),
        /// SELinux "netlink_crypto_socket" object class.
        NetlinkCryptoSocket("netlink_crypto_socket"),
        /// SELinux "netlink_dnrt_socket" object class.
        NetlinkDnrtSocket("netlink_dnrt_socket"),
        /// SELinux "netlink_fib_lookup_socket" object class.
        NetlinkFibLookupSocket("netlink_fib_lookup_socket"),
        /// SELinux "netlink_firewall_socket" object class.
        NetlinkFirewallSocket("netlink_firewall_socket"),
        /// SELinux "netlink_generic_socket" object class.
        NetlinkGenericSocket("netlink_generic_socket"),
        /// SELinux "netlink_ip6fw_socket" object class.
        NetlinkIp6FwSocket("netlink_ip6fw_socket"),
        /// SELinux "netlink_iscsi_socket" object class.
        NetlinkIscsiSocket("netlink_iscsi_socket"),
        /// SELinux "netlink_kobject_uevent_socket" object class.
        NetlinkKobjectUeventSocket("netlink_kobject_uevent_socket"),
        /// SELinux "netlink_netfilter_socket" object class.
        NetlinkNetfilterSocket("netlink_netfilter_socket"),
        /// SELinux "netlink_nflog_socket" object class.
        NetlinkNflogSocket("netlink_nflog_socket"),
        /// SELinux "netlink_rdma_socket" object class.
        NetlinkRdmaSocket("netlink_rdma_socket"),
        /// SELinux "netlink_route_socket" object class.
        NetlinkRouteSocket("netlink_route_socket"),
        /// SELinux "netlink_scsitransport_socket" object class.
        NetlinkScsitransportSocket("netlink_scsitransport_socket"),
        /// SELinux "netlink_selinux_socket" object class.
        NetlinkSelinuxSocket("netlink_selinux_socket"),
        /// SELinux "netlink_socket" object class.
        NetlinkSocket("netlink_socket"),
        /// SELinux "netlink_tcpdiag_socket" object class.
        NetlinkTcpDiagSocket("netlink_tcpdiag_socket"),
        /// SELinux "netlink_xfrm_socket" object class.
        NetlinkXfrmSocket("netlink_xfrm_socket"),
        /// SELinux "packet_socket" object class.
        PacketSocket("packet_socket"),
        /// SELinux "perf_event" object class.
        PerfEvent("perf_event"),
        /// SELinux "process" object class.
        Process("process"),
        /// SELinux "process2" object class.
        Process2("process2"),
        /// SELinux "qipcrtr_socket" object class.
        QipcrtrSocket("qipcrtr_socket"),
        /// SELinux "rawip_socket" object class.
        RawIpSocket("rawip_socket"),
        /// SELinux "sctp_socket" class enabled via the "extended_socket_class" policy capability.
        SctpSocket("sctp_socket"),
        /// SELinux "security" object class.
        Security("security"),
        /// SELinux "sock_file" object class.
        SockFile("sock_file"),
        /// SELinux "socket" object class.
        Socket("socket"),
        /// SELinux "system" object class.
        System("system"),
        /// SELinux "tcp_socket" object class.
        TcpSocket("tcp_socket"),
        /// SELinux "tun_socket" object class.
        TunSocket("tun_socket"),
        /// SELinux "udp_socket" object class.
        UdpSocket("udp_socket"),
        /// SELinux "unix_dgram_socket" object class.
        UnixDgramSocket("unix_dgram_socket"),
        /// SELinux "unix_stream_socket" object class.
        UnixStreamSocket("unix_stream_socket"),
        /// SELinux "vsock_socket" class enabled via the "extended_socket_class" policy capability.
        VsockSocket("vsock_socket"),
        // keep-sorted end
    }
}

impl From<FsNodeClass> for KernelClass {
    fn from(class: FsNodeClass) -> Self {
        match class {
            FsNodeClass::File(file_class) => file_class.into(),
            FsNodeClass::Socket(sock_class) => sock_class.into(),
        }
    }
}
pub trait ForClass<T> {
    /// Returns the `class`-affine [`KernelPermission`] value corresponding to this common
    /// permission. This is used to allow hooks to resolve e.g. common "sys_nice" permission access
    /// based on the "allow" rules for the correct target object class.
    fn for_class(&self, class: T) -> KernelPermission;
}

subset_enum! {
    /// Covers the set of classes that inherit from the common "cap" symbol (e.g. "capability" for
    /// now and "cap_userns" after Starnix gains user namespacing support).
    #[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
    CapClass (common CommonCapPermission) from KernelClass {
        // keep-sorted start
        /// SELinux "capability" object class.
        Capability,
        // keep-sorted end
    }
}

subset_enum! {
    /// Covers the set of classes that inherit from the common "cap2" symbol (e.g. "capability2" for
    /// now and "cap2_userns" after Starnix gains user namespacing support).
    #[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
    Cap2Class (common CommonCap2Permission) from KernelClass {
        // keep-sorted start
        /// SELinux "capability2" object class.
        Capability2,
        // keep-sorted end
    }
}

subset_enum! {
    /// Well-known file-like class in SELinux policy that has a particular meaning in policy
    /// enforcement hooks.
    #[derive(Copy, Clone, Debug, Eq, Hash, PartialEq, RcuDroppable)]
    FileClass (common CommonFilePermission) from KernelClass {
        // keep-sorted start
        /// SELinux "anon_inode" object class.
        AnonFsNode,
        /// SELinux "blk_file" object class.
        BlkFile,
        /// SELinux "chr_file" object class.
        ChrFile,
        /// SELinux "dir" object class.
        Dir,
        /// SELinux "fifo_file" object class.
        FifoFile,
        /// SELinux "file" object class.
        File,
        /// SELinux "lnk_file" object class.
        LnkFile,
        /// SELinux "memfd_file" object class.
        MemFdFile,
        /// SELinux "sock_file" object class.
        SockFile,
        // keep-sorted end
    }
}

subset_enum! {
    /// Distinguishes socket-like kernel object classes defined in SELinux policy.
    #[derive(Copy, Clone, Debug, Eq, Hash, PartialEq, RcuDroppable)]
    SocketClass (common CommonSocketPermission) from KernelClass {
        // keep-sorted start
        IcmpSocket,
        KeySocket,
        NetlinkAuditSocket,
        NetlinkConnectorSocket,
        NetlinkCryptoSocket,
        NetlinkDnrtSocket,
        NetlinkFibLookupSocket,
        NetlinkFirewallSocket,
        NetlinkGenericSocket,
        NetlinkIp6FwSocket,
        NetlinkIscsiSocket,
        NetlinkKobjectUeventSocket,
        NetlinkNetfilterSocket,
        NetlinkNflogSocket,
        NetlinkRdmaSocket,
        NetlinkRouteSocket,
        NetlinkScsitransportSocket,
        NetlinkSelinuxSocket,
        NetlinkSocket,
        NetlinkTcpDiagSocket,
        NetlinkXfrmSocket,
        PacketSocket,
        QipcrtrSocket,
        RawIpSocket,
        SctpSocket,
        /// Generic socket class applied to all socket-like objects for which no more specific
        /// class is defined.
        Socket,
        TcpSocket,
        TunSocket,
        UdpSocket,
        UnixDgramSocket,
        UnixStreamSocket,
        VsockSocket,
        // keep-sorted end
    }
}

/// Container for a security class that could be associated with a [`crate::vfs::FsNode`], to allow
/// permissions common to both file-like and socket-like classes to be generated easily by hooks.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq, RcuDroppable)]
pub enum FsNodeClass {
    File(FileClass),
    Socket(SocketClass),
}

impl From<FileClass> for FsNodeClass {
    fn from(file_class: FileClass) -> Self {
        FsNodeClass::File(file_class)
    }
}

impl From<SocketClass> for FsNodeClass {
    fn from(sock_class: SocketClass) -> Self {
        FsNodeClass::Socket(sock_class)
    }
}

pub trait ClassPermission {
    fn class(&self) -> KernelClass;
    fn id(&self) -> u8;
    #[inline]
    fn as_access_vector(&self) -> AccessVector {
        AccessVector::from(1u32 << self.id())
    }
}

impl<T: Into<KernelClass>> ForClass<T> for KernelPermission {
    fn for_class(&self, class: T) -> KernelPermission {
        assert_eq!(self.class(), class.into());
        *self
    }
}

/// Declares the set of named permissions associated with an SELinux class.
///
/// When `for $kernel_class` is specified, [`ClassPermission`] is implemented on the declared
/// `enum`, enabling values to be wrapped into the generic [`KernelPermission`] container.
/// Otherwise the `enum` represents a common permission subset, and an `impl_from_<snake_name>!`
/// macro is generated to implement [`From`] on derived permission enums.
macro_rules! class_permission_enum {
    ($(#[$meta:meta])* $name:ident for $kernel_class:ident {
        $($(#[$variant_meta:meta])* $variant:ident ($variant_name:literal),)*
    }) => {
        named_enum! {
            #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
            #[repr(u8)]
            $(#[$meta])* $name {
                $($(#[$variant_meta])* $variant ($variant_name),)*
            }
        }


        impl ClassPermission for $name {
            #[inline]
            fn class(&self) -> KernelClass {
                KernelClass::$kernel_class
            }
            #[inline]
            fn id(&self) -> u8 {
                *self as u8
            }
        }

        impl $name {
            const PERMISSIONS: &[KernelPermission] = &[$(KernelPermission::$kernel_class(Self::$variant)),*];
        }
    };
    ($(#[$meta:meta])* $name:ident {
        $($(#[$variant_meta:meta])* $variant:ident ($variant_name:literal),)*
    }) => {
        named_enum! {
            #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
            #[repr(u8)]
            $(#[$meta])* $name {
                $($(#[$variant_meta])* $variant ($variant_name),)*
            }
        }

        class_permission_enum!(@declare_from_macro ($) $name { $($variant),* });
    };
    (@declare_from_macro ($d:tt) $name:ident { $($variant:ident),* }) => {
        paste! {
            macro_rules! [<impl_from_ $name:snake>] {
                ($d target:ident $d (for $d _kernel_class:ident)?) => {
                    impl From<$name> for $d target {
                        fn from(other: $name) -> Self {
                            match other {
                                $($name::$variant => Self::$variant,)*
                            }
                        }
                    }
                };
            }
        }
    };
}

/// Permissions common to all cap-like object classes (e.g. "capability" for now and
/// "cap_userns" after Starnix gains user namespacing support). These are combined with a
/// specific [`CapClass`] by policy enforcement hooks, to obtain class-affine permission
/// values to check.
macro_rules! cap_class_permission_enum {
    ($(#[$meta:meta])* $name:ident $(for $kernel_class:ident)? {
        $($(#[$variant_meta:meta])* $variant:ident ($variant_name:literal),)*
    }) => {
        class_permission_enum! {
            $(#[$meta])* $name $(for $kernel_class)? {
                // keep-sorted start

                AuditControl("audit_control"),
                AuditWrite("audit_write"),
                Chown("chown"),
                DacOverride("dac_override"),
                DacReadSearch("dac_read_search"),
                Fowner("fowner"),
                Fsetid("fsetid"),
                IpcLock("ipc_lock"),
                IpcOwner("ipc_owner"),
                Kill("kill"),
                Lease("lease"),
                LinuxImmutable("linux_immutable"),
                Mknod("mknod"),
                NetAdmin("net_admin"),
                NetBindService("net_bind_service"),
                NetBroadcast("net_broadcast"),
                NetRaw("net_raw"),
                Setfcap("setfcap"),
                Setgid("setgid"),
                Setpcap("setpcap"),
                Setuid("setuid"),
                SysAdmin("sys_admin"),
                SysBoot("sys_boot"),
                SysChroot("sys_chroot"),
                SysModule("sys_module"),
                SysNice("sys_nice"),
                SysPacct("sys_pacct"),
                SysPtrace("sys_ptrace"),
                SysRawio("sys_rawio"),
                SysResource("sys_resource"),
                SysTime("sys_time"),
                SysTtyConfig("sys_tty_config"),

                // keep-sorted end

                // Additional permissions specific to the derived class.
                $($(#[$variant_meta])* $variant ($variant_name),)*
            }
        }

        $(impl_from_common_cap_permission!($name for $kernel_class);)?
    }
}

cap_class_permission_enum! {
    CommonCapPermission {}
}

cap_class_permission_enum! {
    CapabilityPermission for Capability {}
}

/// Permissions common to all cap2-like object classes (e.g. "capability2" for now and
/// "cap2_userns" after Starnix gains user namespacing support). These are combined with a
/// specific [`Cap2Class`] by policy enforcement hooks, to obtain class-affine permission
/// values to check.
macro_rules! cap2_class_permission_enum {
    ($(#[$meta:meta])* $name:ident $(for $kernel_class:ident)? {
        $($(#[$variant_meta:meta])* $variant:ident ($variant_name:literal),)*
    }) => {
        class_permission_enum! {
            $(#[$meta])* $name $(for $kernel_class)? {
                // keep-sorted start

                AuditRead("audit_read"),
                BlockSuspend("block_suspend"),
                Bpf("bpf"),
                MacAdmin("mac_admin"),
                MacOverride("mac_override"),
                Perfmon("perfmon"),
                Syslog("syslog"),
                WakeAlarm("wake_alarm"),

                // keep-sorted end

                // Additional permissions specific to the derived class.
                $($(#[$variant_meta])* $variant ($variant_name),)*
            }
        }

        $(impl_from_common_cap2_permission!($name for $kernel_class);)?
    }
}

cap2_class_permission_enum! {
    /// Common symbol inherited by "capability2" and "cap2_userns" classes.
    CommonCap2Permission {}
}

cap2_class_permission_enum! {
    /// Permissions for the kernel "capability2" class.
    Capability2Permission for Capability2 {}
}

/// Permissions meaningful for all [`crate::vfs::FsNode`]s, whether file- or socket-like.
///
/// This extra layer of common permissions is not reflected in the hierarchy defined by the
/// SELinux Reference Policy. Because even common permissions are mapped per-class, by name, to
/// the policy equivalents, the implementation and policy notions of common permissions need not
/// be identical.
macro_rules! fs_node_class_permission_enum {
    ($(#[$meta:meta])* $name:ident $(for $kernel_class:ident)? {
        $($(#[$variant_meta:meta])* $variant:ident ($variant_name:literal),)*
    }) => {
        class_permission_enum! {
            $(#[$meta])* $name $(for $kernel_class)? {
                // keep-sorted start
                /// Permission to append to a file or socket.
                Append("append"),
                /// Pseudo-permission used in `dontaudit` access-rules to allow access checks to be made
                /// between specific sources & targets without generating audit logs.
                AuditAccess("audit_access"),
                /// Permission to create a file or socket.
                Create("create"),
                /// Permission to query attributes, including uid, gid and extended attributes.
                GetAttr("getattr"),
                /// Permission to execute ioctls on the file or socket.
                Ioctl("ioctl"),
                /// Permission to set and unset file or socket locks.
                Lock("lock"),
                /// Permission to map a file.
                Map("map"),
                /// Permission to read content from a file or socket, as well as reading or following links.
                Read("read"),
                /// Permission checked against the existing label when updating a node's security label.
                RelabelFrom("relabelfrom"),
                /// Permission checked against the new label when updating a node's security label.
                RelabelTo("relabelto"),
                /// Permission to modify attributes, including uid, gid and extended attributes.
                SetAttr("setattr"),
                /// Permission to write contents to the file or socket.
                Write("write"),
                // keep-sorted end

                // Additional permissions specific to the derived class.
                $($(#[$variant_meta])* $variant ($variant_name),)*
            }
        }
    }
}

fs_node_class_permission_enum! {
    CommonFsNodePermission {}
}

impl<T: Into<FsNodeClass>> ForClass<T> for CommonFsNodePermission {
    fn for_class(&self, class: T) -> KernelPermission {
        match class.into() {
            FsNodeClass::File(file_class) => {
                CommonFilePermission::from(*self).for_class(file_class)
            }
            FsNodeClass::Socket(sock_class) => {
                CommonSocketPermission::from(*self).for_class(sock_class)
            }
        }
    }
}

impl_from_common_fs_node_permission!(CommonFilePermission);
impl_from_common_fs_node_permission!(CommonSocketPermission);

/// Permissions common to all socket-like object classes. These are combined with a specific
/// [`SocketClass`] by policy enforcement hooks, to obtain class-affine permission values.
macro_rules! socket_class_permission_enum {
    ($(#[$meta:meta])* $name:ident $(for $kernel_class:ident)? {
        $($(#[$variant_meta:meta])* $variant:ident ($variant_name:literal),)*
    }) => {
        fs_node_class_permission_enum! {
            $(#[$meta])* $name $(for $kernel_class)? {
                // keep-sorted start
                /// Permission to accept a connection.
                Accept("accept"),
                /// Permission to bind to a name.
                Bind("bind"),
                /// Permission to initiate a connection.
                Connect("connect"),
                /// Permission to get socket options.
                GetOpt("getopt"),
                /// Permission to listen for connections.
                Listen("listen"),
                /// Permission to send datagrams to the socket.
                SendTo("sendto"),
                /// Permission to set socket options.
                SetOpt("setopt"),
                /// Permission to terminate connection.
                Shutdown("shutdown"),
                // keep-sorted end

                // Additional permissions specific to the derived class.
                $($(#[$variant_meta])* $variant ($variant_name),)*
            }
        }

        $(impl_from_common_socket_permission!($name for $kernel_class);)?
    }
}

socket_class_permission_enum! {
    CommonSocketPermission {}
}

socket_class_permission_enum! {
    KeySocketPermission for KeySocket {
    }
}

socket_class_permission_enum! {
    NetlinkSocketPermission for NetlinkSocket {}
}

socket_class_permission_enum! {
    NetlinkRouteSocketPermission for NetlinkRouteSocket {
        // keep-sorted start
        /// Permission for nlmsg xperms.
        Nlmsg("nlmsg"),
        /// Permission to read the kernel neighbor table.
        NlmsgGetNeigh("nlmsg_getneigh"),
        /// Permission to read the kernel routing table.
        NlmsgRead("nlmsg_read"),
        /// Permission to read privileged netlink messages.
        NlmsgReadPriv("nlmsg_readpriv"),
        /// Permission to write to the kernel routing table.
        NlmsgWrite("nlmsg_write"),
        // keep-sorted end
    }
}

socket_class_permission_enum! {
    NetlinkFirewallSocketPermission for NetlinkFirewallSocket {
    }
}

socket_class_permission_enum! {
    NetlinkTcpDiagSocketPermission for NetlinkTcpDiagSocket {
        // keep-sorted start
        /// Permission for nlmsg xperms.
        Nlmsg("nlmsg"),
        /// Permission to request information about a protocol.
        NlmsgRead("nlmsg_read"),
        /// Permission to write netlink message.
        NlmsgWrite("nlmsg_write"),
        // keep-sorted end
    }
}

socket_class_permission_enum! {
    NetlinkNflogSocketPermission for NetlinkNflogSocket {
    }
}

socket_class_permission_enum! {
    NetlinkXfrmSocketPermission  for NetlinkXfrmSocket {
        // keep-sorted start
        /// Permission for nlmsg xperms.
        Nlmsg("nlmsg"),
        /// Permission to get IPSec configuration information.
        NlmsgRead("nlmsg_read"),
        /// Permission to set IPSec configuration information.
        NlmsgWrite("nlmsg_write"),
        // keep-sorted end
    }
}

socket_class_permission_enum! {
    NetlinkSelinuxSocketPermission for NetlinkSelinuxSocket {
    }
}

socket_class_permission_enum! {
    NetlinkIscsiSocketPermission for NetlinkIscsiSocket {
    }
}

socket_class_permission_enum! {
    NetlinkAuditSocketPermission for NetlinkAuditSocket {
        // keep-sorted start
        /// Permission for nlmsg xperms.
        Nlmsg("nlmsg"),
        /// Permission to query status of audit service.
        NlmsgRead("nlmsg_read"),
        /// Permission to list auditing configuration rules.
        NlmsgReadPriv("nlmsg_readpriv"),
        /// Permission to send userspace audit messages to the audit service.
        NlmsgRelay("nlmsg_relay"),
        /// Permission to control TTY auditing.
        NlmsgTtyAudit("nlmsg_tty_audit"),
        /// Permission to update the audit service configuration.
        NlmsgWrite("nlmsg_write"),
        // keep-sorted end
    }
}

socket_class_permission_enum! {
    NetlinkFibLookupSocketPermission for NetlinkFibLookupSocket {
    }
}

socket_class_permission_enum! {
    NetlinkConnectorSocketPermission for NetlinkConnectorSocket {
    }
}

socket_class_permission_enum! {
    NetlinkNetfilterSocketPermission for NetlinkNetfilterSocket {
    }
}

socket_class_permission_enum! {
    NetlinkIp6FwSocketPermission for NetlinkIp6FwSocket {
    }
}

socket_class_permission_enum! {
    NetlinkDnrtSocketPermission for NetlinkDnrtSocket {
    }
}

socket_class_permission_enum! {
    NetlinkKobjectUeventSocketPermission for NetlinkKobjectUeventSocket {
    }
}

socket_class_permission_enum! {
    NetlinkGenericSocketPermission for NetlinkGenericSocket {
    }
}

socket_class_permission_enum! {
    NetlinkScsitransportSocketPermission for NetlinkScsitransportSocket {
    }
}

socket_class_permission_enum! {
    NetlinkRdmaSocketPermission for NetlinkRdmaSocket {
    }
}

socket_class_permission_enum! {
    NetlinkCryptoSocketPermission for NetlinkCryptoSocket {
    }
}

socket_class_permission_enum! {
    PacketSocketPermission for PacketSocket {
    }
}

socket_class_permission_enum! {
    QipcrtrSocketPermission for QipcrtrSocket {
    }
}

socket_class_permission_enum! {
    RawIpSocketPermission for RawIpSocket {
    }
}

socket_class_permission_enum! {
    SctpSocketPermission for SctpSocket {

    }
}

socket_class_permission_enum! {
    SocketPermission for Socket {
    }
}

socket_class_permission_enum! {
    TcpSocketPermission for TcpSocket {
    }
}

socket_class_permission_enum! {
    TunSocketPermission for TunSocket {
    }
}

socket_class_permission_enum! {
    UdpSocketPermission for UdpSocket {
    }
}

socket_class_permission_enum! {
    UnixStreamSocketPermission for UnixStreamSocket {
        // keep-sorted start
        /// Permission to connect a streaming Unix-domain socket.
        ConnectTo("connectto"),
        // keep-sorted end
    }
}

socket_class_permission_enum! {
    UnixDgramSocketPermission for UnixDgramSocket {
    }
}

socket_class_permission_enum! {
    VsockSocketPermission for VsockSocket {
    }
}

socket_class_permission_enum! {
    IcmpSocketPermission for IcmpSocket {

    }
}

/// Permissions common to all file-like object classes (e.g. "lnk_file", "dir"). These are
/// combined with a specific [`FileClass`] by policy enforcement hooks, to obtain class-affine
/// permission values to check.
macro_rules! file_class_permission_enum {
    ($(#[$meta:meta])* $name:ident $(for $kernel_class:ident)? {
        $($(#[$variant_meta:meta])* $variant:ident ($variant_name:literal),)*
    }) => {
        fs_node_class_permission_enum! {
        $(#[$meta])* $name $(for $kernel_class)? {
            // keep-sorted start

            /// Permission to make executable a file mapping that has been modified by copy-on-write.
            ExecMod("execmod"),
            /// Permission to execute a file with domain transition.
            Execute("execute"),
            /// Permissions to create hard link.
            Link("link"),
            /// Permission to use as mount point; only useful for directories and files.
            MountOn("mounton"),
            /// Permission to open a file.
            Open("open"),
            /// Permission to rename a file.
            Rename("rename"),
            /// Permission to delete a file or remove a hard link.
            Unlink("unlink"),
            /// Permission to set a watch for changes to a file (e.g. via inotify).
            Watch("watch"),
            /// Permission to set a watch that will receive notifications of read-like events.
            WatchReads("watch_reads"),
            // keep-sorted end

            // Additional permissions specific to the derived class.
            $($(#[$variant_meta])* $variant ($variant_name),)*
        }}

        $(impl_from_common_file_permission!($name for $kernel_class);)?
    }
}

file_class_permission_enum! {
    CommonFilePermission {}
}

file_class_permission_enum! {
    AnonFsNodePermission for AnonFsNode {
    }
}

class_permission_enum! {
    BinderPermission for Binder {
        // keep-sorted start
        /// Permission to perform a binder IPC to a given target process.
        Call("call"),
        /// Permission to use a Binder connection created with a different security context.
        Impersonate("impersonate"),
        /// Permission to set oneself as a context manager.
        SetContextMgr("set_context_mgr"),
        /// Permission to transfer Binder objects as part of a Binder transaction.
        Transfer("transfer"),
        // keep-sorted end
    }
}

file_class_permission_enum! {
    BlkFilePermission for BlkFile {
    }
}

file_class_permission_enum! {
    ChrFilePermission for ChrFile {
    }
}

file_class_permission_enum! {
    DirPermission for Dir {
        // keep-sorted start
        /// Permission to add a file to the directory.
        AddName("add_name"),
        /// Permission to remove a directory.
        RemoveDir("rmdir"),
        /// Permission to remove an entry from a directory.
        RemoveName("remove_name"),
        /// Permission to change parent directory.
        Reparent("reparent"),
        /// Search access to the directory.
        Search("search"),
        // keep-sorted end
    }
}

class_permission_enum! {
    FdPermission for Fd {
        // keep-sorted start
        /// Permission to use file descriptors copied/retained/inherited from another security
        /// context. This permission is generally used to control whether an `exec*()` call from a
        /// cloned process that retained a copy of the file descriptor table should succeed.
        Use("use"),
        // keep-sorted end
    }
}

class_permission_enum! {
    BpfPermission for Bpf {
        // keep-sorted start
        /// Permission to create a map.
        MapCreate("map_create"),
        /// Permission to read from a map.
        MapRead("map_read"),
        /// Permission to write on a map.
        MapWrite("map_write"),
        /// Permission to load a program.
        ProgLoad("prog_load"),
        /// Permission to run a program.
        ProgRun("prog_run"),
        // keep-sorted end
    }
}

class_permission_enum! {
    PerfEventPermission for PerfEvent {
        // keep-sorted start

        /// Permission to monitor the cpu.
        Cpu("cpu"),
        /// Permission to monitor the kernel.
        Kernel("kernel"),
        /// Permission to open a perf event.
        Open("open"),
        /// Permission to read a perf event.
        Read("read"),
        /// Permission to monitor tracepoints.
        Tracepoint("tracepoint"),
        /// Permission to write a perf event.
        Write("write"),
        // keep-sorted end
    }
}

file_class_permission_enum! {
    FifoFilePermission for FifoFile {
    }
}

file_class_permission_enum! {
    FilePermission for File {
        // keep-sorted start
        /// Permission to use a file as an entry point into the new domain on transition.
        Entrypoint("entrypoint"),
        /// Permission to use a file as an entry point to the calling domain without performing a
        /// transition.
        ExecuteNoTrans("execute_no_trans"),
        // keep-sorted end
    }
}

class_permission_enum! {
    FileSystemPermission for FileSystem {
        // keep-sorted start
        /// Permission to associate a file to the filesystem.
        Associate("associate"),
        /// Permission to get filesystem attributes.
        GetAttr("getattr"),
        /// Permission mount a filesystem.
        Mount("mount"),
        /// Permission to relabel from this filesystem SID.
        RelabelFrom("relabelfrom"),
        /// Permission to relabel to this filesystem SID.
        RelabelTo("relabelto"),
        /// Permission to remount a filesystem with different flags.
        Remount("remount"),
        /// Permission to unmount a filesystem.
        Unmount("unmount"),
        // keep-sorted end
    }
}

class_permission_enum! {
    KeyPermission for Key {
        // keep-sorted start
        /// Permission to create a key.
        Create("create"),
        // keep-sorted end
    }
}

file_class_permission_enum! {
    LnkFilePermission for LnkFile {
    }
}

file_class_permission_enum! {
    MemFdFilePermission for MemFdFile {
    }
}

file_class_permission_enum! {
    SockFilePermission for SockFile {
    }
}

class_permission_enum! {
    ProcessPermission for Process {
        // keep-sorted start
        /// Permission to dynamically transition a process to a different security domain.
        DynTransition("dyntransition"),
        /// Permission to execute arbitrary code from the heap.
        ExecHeap("execheap"),
        /// Permission to execute arbitrary code from memory.
        ExecMem("execmem"),
        /// Permission to execute arbitrary code from the stack.
        ExecStack("execstack"),
        /// Permission to fork the current running process.
        Fork("fork"),
        /// Permission to get Linux capabilities of a process.
        GetCap("getcap"),
        /// Permission to get the process group ID.
        GetPgid("getpgid"),
        /// Permission to get the resource limits on a process.
        GetRlimit("getrlimit"),
        /// Permission to get scheduling policy currently applied to a process.
        GetSched("getsched"),
        /// Permission to get the session ID.
        GetSession("getsession"),
        /// Permission to exec into a new security domain without setting the AT_SECURE entry in the
        /// executable's auxiliary vector.
        NoAtSecure("noatsecure"),
        /// Permission to trace a process.
        Ptrace("ptrace"),
        /// Permission to inherit the parent process's resource limits on exec.
        RlimitInh("rlimitinh"),
        /// Permission to set Linux capabilities of a process.
        SetCap("setcap"),
        /// Permission to set the calling task's current Security Context.
        /// The "dyntransition" permission separately limits which Contexts "setcurrent" may be used to transition to.
        SetCurrent("setcurrent"),
        /// Permission to set the Security Context used by `exec()`.
        SetExec("setexec"),
        /// Permission to set the Security Context used when creating filesystem objects.
        SetFsCreate("setfscreate"),
        /// Permission to set the Security Context used when creating kernel keyrings.
        SetKeyCreate("setkeycreate"),
        /// Permission to set the process group ID.
        SetPgid("setpgid"),
        /// Permission to set the resource limits on a process.
        SetRlimit("setrlimit"),
        /// Permission to set scheduling policy for a process.
        SetSched("setsched"),
        /// Permission to set the Security Context used when creating new labeled sockets.
        SetSockCreate("setsockcreate"),
        /// Permission to share resources (e.g. FD table, address-space, etc) with a process.
        Share("share"),
        /// Permission to send SIGCHLD to a process.
        SigChld("sigchld"),
        /// Permission to inherit the parent process's signal state.
        SigInh("siginh"),
        /// Permission to send SIGKILL to a process.
        SigKill("sigkill"),
        /// Permission to send SIGSTOP to a process.
        SigStop("sigstop"),
        /// Permission to send a signal other than SIGKILL, SIGSTOP, or SIGCHLD to a process.
        Signal("signal"),
        /// Permission to transition to a different security domain.
        Transition("transition"),
        // keep-sorted end
    }
}

class_permission_enum! {
    Process2Permission for Process2 {
        // keep-sorted start
        /// Permission to transition to an unbounded domain when no-new-privileges is set.
        NnpTransition("nnp_transition"),
        /// Permission to transition domain when executing from a no-SUID mounted filesystem.
        NosuidTransition("nosuid_transition"),
        // keep-sorted end
    }
}

class_permission_enum! {
    SecurityPermission for Security {
        // keep-sorted start
        /// Permission to validate Security Context using the "context" API.
        CheckContext("check_context"),
        /// Permission to compute access vectors via the "access" API.
        ComputeAv("compute_av"),
        /// Permission to compute security contexts based on `type_transition` rules via "create".
        ComputeCreate("compute_create"),
        /// Permission to compute security contexts based on `type_member` rules via "member".
        ComputeMember("compute_member"),
        /// Permission to compute security contexts based on `type_change` rules via "relabel".
        ComputeRelabel("compute_relabel"),
        /// Permission to compute user decisions via "user".
        ComputeUser("compute_user"),
        /// Permission to load a new binary policy into the kernel via the "load" API.
        LoadPolicy("load_policy"),
        /// Permission to read the loaded binary policy via the "policy" file.
        ReadPolicy("read_policy"),
        /// Permission to commit booleans to control conditional elements of the policy.
        SetBool("setbool"),
        /// Permission to change the way permissions are validated for `mmap()` operations.
        SetCheckReqProt("setcheckreqprot"),
        /// Permission to switch the system between permissive and enforcing modes, via "enforce".
        SetEnforce("setenforce"),
        // keep-sorted end
     }
}

class_permission_enum! {
    SystemPermission for System {
        // keep-sorted start
        /// Permission to use the syslog(2) CONSOLE action types.
        SyslogConsole("syslog_console"),
        /// Permission to use other syslog(2) action types.
        SyslogMod("syslog_mod"),
        /// Permission to use the syslog(2) READ_ALL related action types.
        SyslogRead("syslog_read"),
        // keep-sorted end
     }
}
