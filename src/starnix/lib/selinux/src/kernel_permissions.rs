// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/// Declares an `enum` and implements an `all_variants()` API for it.
macro_rules! enumerable_enum {
    ($(#[$meta:meta])* $name:ident $(extends $common_name:ident)? {
        $($(#[$variant_meta:meta])* $variant:ident,)*
    }) => {
        $(#[$meta])*
        pub enum $name {
            $($(#[$variant_meta])* $variant,)*
            $(Common($common_name),)?
        }

        impl $name {
            pub fn all_variants() -> impl Iterator<Item=Self> {
                let iter = [$($name::$variant),*].iter().map(Clone::clone);
                $(let iter = iter.chain($common_name::all_variants().map($name::Common));)?
                iter
            }
        }
    }
}

enumerable_enum! {
    /// A well-known class in SELinux policy that has a particular meaning in policy enforcement
    /// hooks.
    #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
    KernelClass {
        // keep-sorted start
        /// The SELinux "anon_inode" object class.
        AnonFsNode,
        /// The SELinux "binder" object class.
        Binder,
        /// The SELinux "blk_file" object class.
        Block,
        /// The SELinux "bpf" object class.
        Bpf,
        /// The SELinux "capability" object class.
        Capability,
        /// The SELinux "capability2" object class.
        Capability2,
        /// The SELinux "chr_file" object class.
        Character,
        /// The SELinux "dir" object class.
        Dir,
        /// The SELinux "fd" object class.
        Fd,
        /// The SELinux "fifo_file" object class.
        Fifo,
        /// The SELinux "file" object class.
        File,
        /// The SELinux "filesystem" object class.
        FileSystem,
        /// "icmp_socket" class enabled via the "extended_socket_class" policy capability.
        IcmpSocket,
        /// The SELinux "key_socket" object class.
        KeySocket,
        /// The SELinux "lnk_file" object class.
        Link,
        /// The SELinux "memfd_file" object class.
        MemFdFile,
        /// The SELinux "netlink_audit_socket" object class.
        NetlinkAuditSocket,
        /// The SELinux "netlink_connector_socket" object class.
        NetlinkConnectorSocket,
        /// The SELinux "netlink_crypto_socket" object class.
        NetlinkCryptoSocket,
        /// The SELinux "netlink_dnrt_socket" object class.
        NetlinkDnrtSocket,
        /// The SELinux "netlink_fib_lookup_socket" object class.
        NetlinkFibLookupSocket,
        /// The SELinux "netlink_firewall_socket" object class.
        NetlinkFirewallSocket,
        /// The SELinux "netlink_generic_socket" object class.
        NetlinkGenericSocket,
        /// The SELinux "netlink_ip6fw_socket" object class.
        NetlinkIp6FwSocket,
        /// The SELinux "netlink_iscsi_socket" object class.
        NetlinkIscsiSocket,
        /// The SELinux "netlink_kobject_uevent_socket" object class.
        NetlinkKobjectUeventSocket,
        /// The SELinux "netlink_netfilter_socket" object class.
        NetlinkNetfilterSocket,
        /// The SELinux "netlink_nflog_socket" object class.
        NetlinkNflogSocket,
        /// The SELinux "netlink_rdma_socket" object class.
        NetlinkRdmaSocket,
        /// The SELinux "netlink_route_socket" object class.
        NetlinkRouteSocket,
        /// The SELinux "netlink_scsitransport_socket" object class.
        NetlinkScsitransportSocket,
        /// The SELinux "netlink_selinux_socket" object class.
        NetlinkSelinuxSocket,
        /// The SELinux "netlink_socket" object class.
        NetlinkSocket,
        /// The SELinux "netlink_tcpdiag_socket" object class.
        NetlinkTcpDiagSocket,
        /// The SELinux "netlink_xfrm_socket" object class.
        NetlinkXfrmSocket,
        /// The SELinux "packet_socket" object class.
        PacketSocket,
        /// The SELinux "perf_event" object class.
        PerfEvent,
        /// The SELinux "process" object class.
        Process,
        /// The SELinux "process2" object class.
        Process2,
        /// The SELinux "qipcrtr_socket" object class.
        QipcrtrSocket,
        /// The SELinux "rawip_socket" object class.
        RawIpSocket,
        /// "sctp_socket" class enabled via the "extended_socket_class" policy capability.
        SctpSocket,
        /// The SELinux "security" object class.
        Security,
        /// The SELinux "sock_file" object class.
        SockFile,
        /// The SELinux "socket" object class.
        Socket,
        /// The SELinux "system" object class.
        System,
        /// The SELinux "tcp_socket" object class.
        TcpSocket,
        /// The SELinux "tun_socket" object class.
        TunSocket,
        /// The SELinux "udp_socket" object class.
        UdpSocket,
        /// The SELinux "unix_dgram_socket" object class.
        UnixDgramSocket,
        /// The SELinux "unix_stream_socket" object class.
        UnixStreamSocket,
        /// "vsock_socket" class enabled via the "extended_socket_class" policy capability.
        VSockSocket,
        // keep-sorted end
    }
}

impl KernelClass {
    /// Returns the name used to refer to this object class in the SELinux binary policy.
    pub fn name(&self) -> &'static str {
        match self {
            // keep-sorted start
            Self::AnonFsNode => "anon_inode",
            Self::Binder => "binder",
            Self::Block => "blk_file",
            Self::Bpf => "bpf",
            Self::Capability => "capability",
            Self::Capability2 => "capability2",
            Self::Character => "chr_file",
            Self::Dir => "dir",
            Self::Fd => "fd",
            Self::Fifo => "fifo_file",
            Self::File => "file",
            Self::FileSystem => "filesystem",
            Self::IcmpSocket => "icmp_socket",
            Self::KeySocket => "key_socket",
            Self::Link => "lnk_file",
            Self::MemFdFile => "memfd_file",
            Self::NetlinkAuditSocket => "netlink_audit_socket",
            Self::NetlinkConnectorSocket => "netlink_connector_socket",
            Self::NetlinkCryptoSocket => "netlink_crypto_socket",
            Self::NetlinkDnrtSocket => "netlink_dnrt_socket",
            Self::NetlinkFibLookupSocket => "netlink_fib_lookup_socket",
            Self::NetlinkFirewallSocket => "netlink_firewall_socket",
            Self::NetlinkGenericSocket => "netlink_generic_socket",
            Self::NetlinkIp6FwSocket => "netlink_ip6fw_socket",
            Self::NetlinkIscsiSocket => "netlink_iscsi_socket",
            Self::NetlinkKobjectUeventSocket => "netlink_kobject_uevent_socket",
            Self::NetlinkNetfilterSocket => "netlink_netfilter_socket",
            Self::NetlinkNflogSocket => "netlink_nflog_socket",
            Self::NetlinkRdmaSocket => "netlink_rdma_socket",
            Self::NetlinkRouteSocket => "netlink_route_socket",
            Self::NetlinkScsitransportSocket => "netlink_scsitransport_socket",
            Self::NetlinkSelinuxSocket => "netlink_selinux_socket",
            Self::NetlinkSocket => "netlink_socket",
            Self::NetlinkTcpDiagSocket => "netlink_tcpdiag_socket",
            Self::NetlinkXfrmSocket => "netlink_xfrm_socket",
            Self::PacketSocket => "packet_socket",
            Self::PerfEvent => "perf_event",
            Self::Process => "process",
            Self::Process2 => "process2",
            Self::QipcrtrSocket => "qipcrtr_socket",
            Self::RawIpSocket => "rawip_socket",
            Self::SctpSocket => "sctp_socket",
            Self::Security => "security",
            Self::SockFile => "sock_file",
            Self::Socket => "socket",
            Self::System => "system",
            Self::TcpSocket => "tcp_socket",
            Self::TunSocket => "tun_socket",
            Self::UdpSocket => "udp_socket",
            Self::UnixDgramSocket => "unix_dgram_socket",
            Self::UnixStreamSocket => "unix_stream_socket",
            Self::VSockSocket => "vsock_socket",
            // keep-sorted end
        }
    }
}

impl<T: Into<KernelClass>> ForClass<T> for KernelPermission {
    fn for_class(&self, class: T) -> KernelPermission {
        assert_eq!(self.class(), class.into());
        self.clone()
    }
}

pub trait ForClass<T> {
    /// Returns the `class`-affine `KernelPermission` value corresponding to this common permission.
    /// This is used to allow hooks to resolve e.g. common "sys_nice" permission access based on the
    /// "allow" rules for the correct target object class.
    fn for_class(&self, class: T) -> KernelPermission;
}

enumerable_enum! {
    /// Covers the set of classes that inherit from the common "cap" symbol (e.g. "capability" for
    /// now and "cap_userns" after Starnix gains user namespacing support).
    #[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
    CapClass {
        // keep-sorted start
        /// The SELinux "capability" object class.
        Capability,
        // keep-sorted end
    }
}

impl From<CapClass> for KernelClass {
    fn from(cap_class: CapClass) -> Self {
        match cap_class {
            // keep-sorted start
            CapClass::Capability => Self::Capability,
            // keep-sorted end
        }
    }
}

enumerable_enum! {
    /// Covers the set of classes that inherit from the common "cap2" symbol (e.g. "capability2" for
    /// now and "cap2_userns" after Starnix gains user namespacing support).
    #[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
    Cap2Class {
        // keep-sorted start
        /// The SELinux "capability2" object class.
        Capability2,
        // keep-sorted end
    }
}

impl From<Cap2Class> for KernelClass {
    fn from(cap2_class: Cap2Class) -> Self {
        match cap2_class {
            // keep-sorted start
            Cap2Class::Capability2 => Self::Capability2,
            // keep-sorted end
        }
    }
}

enumerable_enum! {
    /// A well-known file-like class in SELinux policy that has a particular meaning in policy
    /// enforcement hooks.
    #[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
    FileClass {
        // keep-sorted start
        /// The SELinux "anon_inode" object class.
        AnonFsNode,
        /// The SELinux "blk_file" object class.
        Block,
        /// The SELinux "chr_file" object class.
        Character,
        /// The SELinux "dir" object class.
        Dir,
        /// The SELinux "fifo_file" object class.
        Fifo,
        /// The SELinux "file" object class.
        File,
        /// The SELinux "lnk_file" object class.
        Link,
        /// The SELinux "memfd_file" object class.
        MemFdFile,
        /// The SELinux "sock_file" object class.
        SockFile,
        // keep-sorted end
    }
}

impl From<FileClass> for KernelClass {
    fn from(file_class: FileClass) -> Self {
        match file_class {
            // keep-sorted start
            FileClass::AnonFsNode => Self::AnonFsNode,
            FileClass::Block => Self::Block,
            FileClass::Character => Self::Character,
            FileClass::Dir => Self::Dir,
            FileClass::Fifo => Self::Fifo,
            FileClass::File => Self::File,
            FileClass::Link => Self::Link,
            FileClass::MemFdFile => Self::MemFdFile,
            FileClass::SockFile => Self::SockFile,
            // keep-sorted end
        }
    }
}

enumerable_enum! {
    /// Distinguishes socket-like kernel object classes defined in SELinux policy.
    #[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
    SocketClass {
        // keep-sorted start
        Icmp,
        Key,
        Netlink,
        NetlinkAudit,
        NetlinkConnector,
        NetlinkCrypto,
        NetlinkDnrt,
        NetlinkFibLookup,
        NetlinkFirewall,
        NetlinkGeneric,
        NetlinkIp6Fw,
        NetlinkIscsi,
        NetlinkKobjectUevent,
        NetlinkNetfilter,
        NetlinkNflog,
        NetlinkRdma,
        NetlinkRoute,
        NetlinkScsitransport,
        NetlinkSelinux,
        NetlinkTcpDiag,
        NetlinkXfrm,
        Packet,
        Qipcrtr,
        RawIp,
        Sctp,
        /// Generic socket class applied to all socket-like objects for which no more specific
        /// class is defined.
        Socket,
        Tcp,
        Tun,
        Udp,
        UnixDgram,
        UnixStream,
        Vsock,
        // keep-sorted end
    }
}

impl From<SocketClass> for KernelClass {
    fn from(socket_class: SocketClass) -> Self {
        match socket_class {
            // keep-sorted start
            SocketClass::Icmp => Self::IcmpSocket,
            SocketClass::Key => Self::KeySocket,
            SocketClass::Netlink => Self::NetlinkSocket,
            SocketClass::NetlinkAudit => Self::NetlinkAuditSocket,
            SocketClass::NetlinkConnector => Self::NetlinkConnectorSocket,
            SocketClass::NetlinkCrypto => Self::NetlinkCryptoSocket,
            SocketClass::NetlinkDnrt => Self::NetlinkDnrtSocket,
            SocketClass::NetlinkFibLookup => Self::NetlinkFibLookupSocket,
            SocketClass::NetlinkFirewall => Self::NetlinkFirewallSocket,
            SocketClass::NetlinkGeneric => Self::NetlinkGenericSocket,
            SocketClass::NetlinkIp6Fw => Self::NetlinkIp6FwSocket,
            SocketClass::NetlinkIscsi => Self::NetlinkIscsiSocket,
            SocketClass::NetlinkKobjectUevent => Self::NetlinkKobjectUeventSocket,
            SocketClass::NetlinkNetfilter => Self::NetlinkNetfilterSocket,
            SocketClass::NetlinkNflog => Self::NetlinkNflogSocket,
            SocketClass::NetlinkRdma => Self::NetlinkRdmaSocket,
            SocketClass::NetlinkRoute => Self::NetlinkRouteSocket,
            SocketClass::NetlinkScsitransport => Self::NetlinkScsitransportSocket,
            SocketClass::NetlinkSelinux => Self::NetlinkSelinuxSocket,
            SocketClass::NetlinkTcpDiag => Self::NetlinkTcpDiagSocket,
            SocketClass::NetlinkXfrm => Self::NetlinkXfrmSocket,
            SocketClass::Packet => Self::PacketSocket,
            SocketClass::Qipcrtr => Self::QipcrtrSocket,
            SocketClass::RawIp => Self::RawIpSocket,
            SocketClass::Sctp => Self::SctpSocket,
            SocketClass::Socket => Self::Socket,
            SocketClass::Tcp => Self::TcpSocket,
            SocketClass::Tun => Self::TunSocket,
            SocketClass::Udp => Self::UdpSocket,
            SocketClass::UnixDgram => Self::UnixDgramSocket,
            SocketClass::UnixStream => Self::UnixStreamSocket,
            SocketClass::Vsock => Self::VSockSocket,
            // keep-sorted end
        }
    }
}

/// Container for a security class that could be associated with a [`crate::vfs::FsNode`], to allow
/// permissions common to both file-like and socket-like classes to be generated easily by hooks.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub enum FsNodeClass {
    File(FileClass),
    Socket(SocketClass),
}

impl From<FsNodeClass> for KernelClass {
    fn from(class: FsNodeClass) -> Self {
        match class {
            FsNodeClass::File(file_class) => file_class.into(),
            FsNodeClass::Socket(sock_class) => sock_class.into(),
        }
    }
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
}

macro_rules! permission_enum {
    ($(#[$meta:meta])* $name:ident {
        $($(#[$variant_meta:meta])* $variant:ident($inner:ident)),*,
    }) => {
        $(#[$meta])*
        pub enum $name {
            $($(#[$variant_meta])* $variant($inner)),*
        }

        $(impl From<$inner> for $name {
            fn from(v: $inner) -> Self {
                Self::$variant(v)
            }
        })*

        impl ClassPermission for $name {
            fn class(&self) -> KernelClass {
                match self {
                    $($name::$variant(_) => KernelClass::$variant),*
                }
            }
        }

        impl $name {
            pub fn name(&self) -> &'static str {
                match self {
                    $($name::$variant(v) => v.name()),*
                }
            }

            pub fn all_variants() -> impl Iterator<Item=Self> {
                let iter = [].iter().map(Clone::clone);
                $(let iter = iter.chain($inner::all_variants().map($name::from));)*
                iter
            }
        }
    }
}

permission_enum! {
    /// A well-known `(class, permission)` pair in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    KernelPermission {
        // keep-sorted start
        /// Permissions for the well-known SELinux "anon_inode" file-like object class.
        AnonFsNode(AnonFsNodePermission),
        /// Permissions for the well-known SELinux "binder" file-like object class.
        Binder(BinderPermission),
        /// Permissions for the well-known SELinux "blk_file" file-like object class.
        Block(BlockFilePermission),
        /// Permissions for the well-known SELinux "bpf" file-like object class.
        Bpf(BpfPermission),
        /// Permissions for the well-known SELinux "capability" object class.
        Capability(CapabilityPermission),
        /// Permissions for the well-known SELinux "capability2" object class.
        Capability2(Capability2Permission),
        /// Permissions for the well-known SELinux "chr_file" file-like object class.
        Character(CharacterFilePermission),
        /// Permissions for the well-known SELinux "dir" file-like object class.
        Dir(DirPermission),
        /// Permissions for the well-known SELinux "fd" object class.
        Fd(FdPermission),
        /// Permissions for the well-known SELinux "fifo_file" file-like object class.
        Fifo(FifoFilePermission),
        /// Permissions for the well-known SELinux "file" object class.
        File(FilePermission),
        /// Permissions for the well-known SELinux "filesystem" object class.
        FileSystem(FileSystemPermission),
        /// "icmp_socket" class permissions, enabled by "extended_socket_class" policy capability.
        IcmpSocket(IcmpSocketPermission),
        /// Permissions for the well-known SELinux "packet_socket" object class.
        KeySocket(KeySocketPermission),
        /// Permissions for the well-known SELinux "lnk_file" file-like object class.
        Link(LinkFilePermission),
        /// Permissions for the well-known SELinux "memfd_file" file-like object class.
        MemFdFile(MemFdFilePermission),
        /// Permissions for the well-known SELinux "netlink_audit_socket" file-like object class.
        NetlinkAuditSocket(NetlinkAuditSocketPermission),
        /// Permissions for the well-known SELinux "netlink_connector_socket" file-like object class.
        NetlinkConnectorSocket(NetlinkConnectorSocketPermission),
        /// Permissions for the well-known SELinux "netlink_crypto_socket" file-like object class.
        NetlinkCryptoSocket(NetlinkCryptoSocketPermission),
        /// Permissions for the well-known SELinux "netlink_dnrt_socket" file-like object class.
        NetlinkDnrtSocket(NetlinkDnrtSocketPermission),
        /// Permissions for the well-known SELinux "netlink_fib_lookup_socket" file-like object class.
        NetlinkFibLookupSocket(NetlinkFibLookupSocketPermission),
        /// Permissions for the well-known SELinux "netlink_firewall_socket" file-like object class.
        NetlinkFirewallSocket(NetlinkFirewallSocketPermission),
        /// Permissions for the well-known SELinux "netlink_generic_socket" file-like object class.
        NetlinkGenericSocket(NetlinkGenericSocketPermission),
        /// Permissions for the well-known SELinux "netlink_ip6fw_socket" file-like object class.
        NetlinkIp6FwSocket(NetlinkIp6FwSocketPermission),
        /// Permissions for the well-known SELinux "netlink_iscsi_socket" file-like object class.
        NetlinkIscsiSocket(NetlinkIscsiSocketPermission),
        /// Permissions for the well-known SELinux "netlink_kobject_uevent_socket" file-like object class.
        NetlinkKobjectUeventSocket(NetlinkKobjectUeventSocketPermission),
        /// Permissions for the well-known SELinux "netlink_netfilter_socket" file-like object class.
        NetlinkNetfilterSocket(NetlinkNetfilterSocketPermission),
        /// Permissions for the well-known SELinux "netlink_nflog_socket" file-like object class.
        NetlinkNflogSocket(NetlinkNflogSocketPermission),
        /// Permissions for the well-known SELinux "netlink_rdma_socket" file-like object class.
        NetlinkRdmaSocket(NetlinkRdmaSocketPermission),
        /// Permissions for the well-known SELinux "netlink_route_socket" file-like object class.
        NetlinkRouteSocket(NetlinkRouteSocketPermission),
        /// Permissions for the well-known SELinux "netlink_scsitransport_socket" file-like object class.
        NetlinkScsitransportSocket(NetlinkScsitransportSocketPermission),
        /// Permissions for the well-known SELinux "netlink_selinux_socket" file-like object class.
        NetlinkSelinuxSocket(NetlinkSelinuxSocketPermission),
        /// Permissions for the well-known SELinux "netlink_socket" file-like object class.
        NetlinkSocket(NetlinkSocketPermission),
        /// Permissions for the well-known SELinux "netlink_tcpdiag_socket" file-like object class.
        NetlinkTcpDiagSocket(NetlinkTcpDiagSocketPermission),
        /// Permissions for the well-known SELinux "netlink_xfrm_socket" file-like object class.
        NetlinkXfrmSocket(NetlinkXfrmSocketPermission),
        /// Permissions for the well-known SELinux "packet_socket" object class.
        PacketSocket(PacketSocketPermission),
        /// Permissions for the well-known SELinux "perf_event" object class.
        PerfEvent(PerfEventPermission),
        /// Permissions for the well-known SELinux "process" object class.
        Process(ProcessPermission),
        /// Permissions for the well-known SELinux "process2" object class.
        Process2(Process2Permission),
        /// Permissions for the well-known SELinux "qipcrtr_socket" object class.
        QipcrtrSocket(QipcrtrSocketPermission),
        /// Permissions for the well-known SELinux "rawip_socket" object class.
        RawIpSocket(RawIpSocketPermission),
        /// "sctp_socket" class permissions, enabled by "extended_socket_class" policy capability.
        SctpSocket(SctpSocketPermission),
        /// Permissions for access to parts of the "selinuxfs" used to administer and query SELinux.
        Security(SecurityPermission),
        /// Permissions for the well-known SELinux "sock_file" file-like object class.
        SockFile(SockFilePermission),
        /// Permissions for the well-known SELinux "socket" object class.
        Socket(SocketPermission),
        /// Permissions for the well-known SELinux "system" object class.
        System(SystemPermission),
        /// Permissions for the well-known SELinux "tcp_socket" object class.
        TcpSocket(TcpSocketPermission),
        /// Permissions for the well-known SELinux "tun_socket" object class.
        TunSocket(TunSocketPermission),
        /// Permissions for the well-known SELinux "udp_socket" object class.
        UdpSocket(UdpSocketPermission),
        /// Permissions for the well-known SELinux "unix_dgram_socket" object class.
        UnixDgramSocket(UnixDgramSocketPermission),
        /// Permissions for the well-known SELinux "unix_stream_socket" object class.
        UnixStreamSocket(UnixStreamSocketPermission),
        /// "vsock_socket" class permissions, enabled by "extended_socket_class" policy capability.
        VSockSocket(VsockSocketPermission),
        // keep-sorted end
    }
}

/// Helper used to define an enum of permission values, with specified names.
/// Uses of this macro should not rely on "extends", which is solely for use to express permission
/// inheritance in `class_permission_enum`.
macro_rules! common_permission_enum {
    ($(#[$meta:meta])* $name:ident $(extends $common_name:ident)? {
        $($(#[$variant_meta:meta])* $variant:ident ($variant_name:literal),)*
    }) => {
        enumerable_enum! {
            $(#[$meta])* $name $(extends $common_name)? {
                $($(#[$variant_meta])* $variant,)*
            }
        }

        impl $name {
            fn name(&self) -> &'static str {
                match self {
                    $($name::$variant => $variant_name,)*
                    $(Self::Common(v) => {let v:$common_name = v.clone(); v.name()},)?
                }
            }
        }
    }
}

/// Helper used to declare the set of named permissions associated with an SELinux class.
/// The `ClassType` trait is implemented on the declared `enum`, enabling values to be wrapped into
/// the generic `KernelPermission` container.
/// If an "extends" type is specified then a `Common` enum case is added, encapsulating the values
/// of that underlying permission type. This is used to represent e.g. SELinux "dir" class deriving
/// a basic set of permissions from the common "file" symbol.
macro_rules! class_permission_enum {
    ($(#[$meta:meta])* $name:ident $(extends $common_name:ident)? {
        $($(#[$variant_meta:meta])* $variant:ident ($variant_name:literal),)*
    }) => {
        common_permission_enum! {
            $(#[$meta])* $name $(extends $common_name)? {
                $($(#[$variant_meta])* $variant ($variant_name),)*
            }
        }

        impl ClassPermission for $name {
            fn class(&self) -> KernelClass {
                KernelPermission::from(self.clone()).class()
            }
        }
    }
}

common_permission_enum! {
    /// Permissions common to all cap-like object classes (e.g. "capability" for now and
    /// "cap_userns" after Starnix gains user namespacing support). These are combined with a
    /// specific `CapabilityClass` by policy enforcement hooks, to obtain class-affine permission
    /// values to check.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    CommonCapPermission {
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
    }
}

class_permission_enum! {
    /// A well-known "capability" class permission in SELinux policy that has a particular meaning
    /// in policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    CapabilityPermission extends CommonCapPermission {}
}

impl ForClass<CapClass> for CommonCapPermission {
    /// Returns the `class`-affine `KernelPermission` value corresponding to this common permission.
    /// This is used to allow hooks to resolve e.g. common "sys_nice" permission access based on the
    /// "allow" rules for the correct target object class.
    fn for_class(&self, class: CapClass) -> KernelPermission {
        match class {
            CapClass::Capability => CapabilityPermission::Common(self.clone()).into(),
        }
    }
}

common_permission_enum! {
    /// Permissions common to all cap2-like object classes (e.g. "capability2" for now and
    /// "cap2_userns" after Starnix gains user namespacing support). These are combined with a
    /// specific `Capability2Class` by policy enforcement hooks, to obtain class-affine permission
    /// values to check.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    CommonCap2Permission {
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
    }
}

class_permission_enum! {
    /// A well-known "capability2" class permission in SELinux policy that has a particular meaning
    /// in policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    Capability2Permission extends CommonCap2Permission {}
}

impl ForClass<Cap2Class> for CommonCap2Permission {
    /// Returns the `class`-affine `KernelPermission` value corresponding to this common permission.
    /// This is used to allow hooks to resolve e.g. common "mac_admin" permission access based on
    /// the "allow" rules for the correct target object class.
    fn for_class(&self, class: Cap2Class) -> KernelPermission {
        match class {
            Cap2Class::Capability2 => Capability2Permission::Common(self.clone()).into(),
        }
    }
}

common_permission_enum! {
    /// Permissions meaningful for all [`crate::vfs::FsNode`]s, whether file- or socket-like.
    ///
    /// This extra layer of common permissions is not reflected in the hierarchy defined by the
    /// SELinux Reference Policy. Because even common permissions are mapped per-class, by name, to
    /// the policy equivalents, the implementation and policy notions of common permissions need not
    /// be identical.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    CommonFsNodePermission {
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
    }
}

impl<T: Into<FsNodeClass>> ForClass<T> for CommonFsNodePermission {
    /// Returns the `class`-affine `KernelPermission` value corresponding to this common permission.
    /// This is used to allow hooks to resolve e.g. common "read" permission access based on the
    /// "allow" rules for the correct target object class.
    fn for_class(&self, class: T) -> KernelPermission {
        match class.into() {
            FsNodeClass::File(file_class) => {
                CommonFilePermission::Common(self.clone()).for_class(file_class)
            }
            FsNodeClass::Socket(sock_class) => {
                CommonSocketPermission::Common(self.clone()).for_class(sock_class)
            }
        }
    }
}
common_permission_enum! {
    /// Permissions common to all socket-like object classes. These are combined with a specific
    /// `SocketClass` by policy enforcement hooks, to obtain class-affine permission values.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    CommonSocketPermission extends CommonFsNodePermission {
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
    }
}

impl ForClass<SocketClass> for CommonSocketPermission {
    /// Returns the `class`-affine `KernelPermission` value corresponding to this common permission.
    /// This is used to allow hooks to resolve e.g. common "read" permission access based on the
    /// "allow" rules for the correct target object class.
    fn for_class(&self, class: SocketClass) -> KernelPermission {
        match class {
            SocketClass::Key => KeySocketPermission::Common(self.clone()).into(),
            SocketClass::Netlink => NetlinkSocketPermission::Common(self.clone()).into(),
            SocketClass::NetlinkAudit => NetlinkAuditSocketPermission::Common(self.clone()).into(),
            SocketClass::NetlinkConnector => {
                NetlinkConnectorSocketPermission::Common(self.clone()).into()
            }
            SocketClass::NetlinkCrypto => {
                NetlinkCryptoSocketPermission::Common(self.clone()).into()
            }
            SocketClass::NetlinkDnrt => NetlinkDnrtSocketPermission::Common(self.clone()).into(),
            SocketClass::NetlinkFibLookup => {
                NetlinkFibLookupSocketPermission::Common(self.clone()).into()
            }
            SocketClass::NetlinkFirewall => {
                NetlinkFirewallSocketPermission::Common(self.clone()).into()
            }
            SocketClass::NetlinkGeneric => {
                NetlinkGenericSocketPermission::Common(self.clone()).into()
            }
            SocketClass::NetlinkIp6Fw => NetlinkIp6FwSocketPermission::Common(self.clone()).into(),
            SocketClass::NetlinkIscsi => NetlinkIscsiSocketPermission::Common(self.clone()).into(),
            SocketClass::NetlinkKobjectUevent => {
                NetlinkKobjectUeventSocketPermission::Common(self.clone()).into()
            }
            SocketClass::NetlinkNetfilter => {
                NetlinkNetfilterSocketPermission::Common(self.clone()).into()
            }
            SocketClass::NetlinkNflog => NetlinkNflogSocketPermission::Common(self.clone()).into(),
            SocketClass::NetlinkRdma => NetlinkRdmaSocketPermission::Common(self.clone()).into(),
            SocketClass::NetlinkRoute => NetlinkRouteSocketPermission::Common(self.clone()).into(),
            SocketClass::NetlinkScsitransport => {
                NetlinkScsitransportSocketPermission::Common(self.clone()).into()
            }
            SocketClass::NetlinkSelinux => {
                NetlinkSelinuxSocketPermission::Common(self.clone()).into()
            }
            SocketClass::NetlinkTcpDiag => {
                NetlinkTcpDiagSocketPermission::Common(self.clone()).into()
            }
            SocketClass::NetlinkXfrm => NetlinkXfrmSocketPermission::Common(self.clone()).into(),
            SocketClass::Packet => PacketSocketPermission::Common(self.clone()).into(),
            SocketClass::Qipcrtr => QipcrtrSocketPermission::Common(self.clone()).into(),
            SocketClass::RawIp => RawIpSocketPermission::Common(self.clone()).into(),
            SocketClass::Sctp => SctpSocketPermission::Common(self.clone()).into(),
            SocketClass::Socket => SocketPermission::Common(self.clone()).into(),
            SocketClass::Tcp => TcpSocketPermission::Common(self.clone()).into(),
            SocketClass::Tun => TunSocketPermission::Common(self.clone()).into(),
            SocketClass::Udp => UdpSocketPermission::Common(self.clone()).into(),
            SocketClass::UnixDgram => UnixDgramSocketPermission::Common(self.clone()).into(),
            SocketClass::UnixStream => UnixStreamSocketPermission::Common(self.clone()).into(),
            SocketClass::Vsock => VsockSocketPermission::Common(self.clone()).into(),
            SocketClass::Icmp => IcmpSocketPermission::Common(self.clone()).into(),
        }
    }
}

class_permission_enum! {
    /// A well-known "key_socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    KeySocketPermission extends CommonSocketPermission {
    }
}
class_permission_enum! {
    /// A well-known "netlink_socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    NetlinkSocketPermission extends CommonSocketPermission {
    }
}

class_permission_enum! {
    /// A well-known "netlink_route_socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    NetlinkRouteSocketPermission extends CommonSocketPermission {
        // keep-sorted start
        /// Permission for nlmsg xperms.
        Nlmsg("nlmsg"),
        /// Permission to read the kernel routing table.
        NlmsgRead("nlmsg_read"),
        /// Permission to write to the kernel routing table.
        NlmsgWrite("nlmsg_write"),
        // keep-sorted end
    }
}

class_permission_enum! {
    /// A well-known "netlink_firewall_socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    NetlinkFirewallSocketPermission extends CommonSocketPermission {
    }
}

class_permission_enum! {
    /// A well-known "netlink_tcpdiag_socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    NetlinkTcpDiagSocketPermission extends CommonSocketPermission {
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

class_permission_enum! {
    /// A well-known "netlink_nflog_socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    NetlinkNflogSocketPermission extends CommonSocketPermission {
    }
}

class_permission_enum! {
    /// A well-known "netlink_xfrm_socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    NetlinkXfrmSocketPermission extends CommonSocketPermission {
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

class_permission_enum! {
    /// A well-known "netlink_selinux_socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    NetlinkSelinuxSocketPermission extends CommonSocketPermission {
    }
}

class_permission_enum! {
    /// A well-known "netlink_iscsi_socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    NetlinkIscsiSocketPermission extends CommonSocketPermission {
    }
}

class_permission_enum! {
    /// A well-known "netlink_audit_socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    NetlinkAuditSocketPermission extends CommonSocketPermission {
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

class_permission_enum! {
    /// A well-known "netlink_fib_lookup_socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    NetlinkFibLookupSocketPermission extends CommonSocketPermission {
    }
}

class_permission_enum! {
    /// A well-known "netlink_connector_socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    NetlinkConnectorSocketPermission extends CommonSocketPermission {
    }
}

class_permission_enum! {
    /// A well-known "netlink_netfilter_socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    NetlinkNetfilterSocketPermission extends CommonSocketPermission {
    }
}

class_permission_enum! {
    /// A well-known "netlink_ip6fw_socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    NetlinkIp6FwSocketPermission extends CommonSocketPermission {
    }
}

class_permission_enum! {
    /// A well-known "netlink_dnrt_socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    NetlinkDnrtSocketPermission extends CommonSocketPermission {
    }
}

class_permission_enum! {
    /// A well-known "netlink_kobject_uevent_socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    NetlinkKobjectUeventSocketPermission extends CommonSocketPermission {
    }
}

class_permission_enum! {
    /// A well-known "netlink_generic_socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    NetlinkGenericSocketPermission extends CommonSocketPermission {
    }
}

class_permission_enum! {
    /// A well-known "netlink_scsitransport_socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    NetlinkScsitransportSocketPermission extends CommonSocketPermission {
    }
}

class_permission_enum! {
    /// A well-known "netlink_rdma_socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    NetlinkRdmaSocketPermission extends CommonSocketPermission {
    }
}

class_permission_enum! {
    /// A well-known "netlink_crypto_socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    NetlinkCryptoSocketPermission extends CommonSocketPermission {
    }
}

class_permission_enum! {
    /// A well-known "packet_socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    PacketSocketPermission extends CommonSocketPermission {
    }
}

class_permission_enum! {
    /// A well-known "qipcrtr_socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    QipcrtrSocketPermission extends CommonSocketPermission {
    }
}

class_permission_enum! {
    /// A well-known "rawip_socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    RawIpSocketPermission extends CommonSocketPermission {
    }
}

class_permission_enum! {
    /// A well-known "sctp_socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    SctpSocketPermission extends CommonSocketPermission {
        // keep-sorted start
        /// Permission to create an SCTP association.
        Associate("associate"),
        /// Permission to `connect()` or `connectx()` an SCTP socket.
        NameConnect("name_connect"),
        /// Permission to `bind()` or `bindx()` an SCTP socket.
        NodeBind("node_bind"),
        // keep-sorted end
    }
}

class_permission_enum! {
    /// A well-known "socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    SocketPermission extends CommonSocketPermission {
    }
}

class_permission_enum! {
    /// A well-known "tcp_socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    TcpSocketPermission extends CommonSocketPermission {
    }
}

class_permission_enum! {
    /// A well-known "tun_socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    TunSocketPermission extends CommonSocketPermission {
    }
}

class_permission_enum! {
    /// A well-known "udp_socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    UdpSocketPermission extends CommonSocketPermission {
    }
}

class_permission_enum! {
    /// A well-known "unix_stream_socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    UnixStreamSocketPermission extends CommonSocketPermission {
        // keep-sorted start
        /// Permission to connect a streaming Unix-domain socket.
        ConnectTo("connectto"),
        // keep-sorted end
    }
}

class_permission_enum! {
    /// A well-known "unix_dgram_socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    UnixDgramSocketPermission extends CommonSocketPermission {
    }
}

class_permission_enum! {
    /// A well-known "vsock_socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    VsockSocketPermission extends CommonSocketPermission {
    }
}

class_permission_enum! {
    /// A well-known "icmp_socket" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    IcmpSocketPermission extends CommonSocketPermission {
        // keep-sorted start
        /// Permission to `bind()` an ICMP socket.
        NodeBind("node_bind"),
        // keep-sorted end
    }
}

common_permission_enum! {
    /// Permissions common to all file-like object classes (e.g. "lnk_file", "dir"). These are
    /// combined with a specific `FileClass` by policy enforcement hooks, to obtain class-affine
    /// permission values to check.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    CommonFilePermission extends CommonFsNodePermission {
        // keep-sorted start
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
        // keep-sorted end
    }
}

impl ForClass<FileClass> for CommonFilePermission {
    /// Returns the `class`-affine `KernelPermission` value corresponding to this common permission.
    /// This is used to allow hooks to resolve e.g. common "read" permission access based on the
    /// "allow" rules for the correct target object class.
    fn for_class(&self, class: FileClass) -> KernelPermission {
        match class {
            FileClass::AnonFsNode => AnonFsNodePermission::Common(self.clone()).into(),
            FileClass::Block => BlockFilePermission::Common(self.clone()).into(),
            FileClass::Character => CharacterFilePermission::Common(self.clone()).into(),
            FileClass::Dir => DirPermission::Common(self.clone()).into(),
            FileClass::Fifo => FifoFilePermission::Common(self.clone()).into(),
            FileClass::File => FilePermission::Common(self.clone()).into(),
            FileClass::Link => LinkFilePermission::Common(self.clone()).into(),
            FileClass::SockFile => SockFilePermission::Common(self.clone()).into(),
            FileClass::MemFdFile => MemFdFilePermission::Common(self.clone()).into(),
        }
    }
}

class_permission_enum! {
    /// A well-known "anon_file" class permission used to manage special file-like nodes not linked
    /// into any directory structures.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    AnonFsNodePermission extends CommonFilePermission {
    }
}

class_permission_enum! {
    /// A well-known "binder" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    BinderPermission {
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

class_permission_enum! {
    /// A well-known "blk_file" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    BlockFilePermission extends CommonFilePermission {
    }
}

class_permission_enum! {
    /// A well-known "chr_file" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    CharacterFilePermission extends CommonFilePermission {
    }
}

class_permission_enum! {
    /// A well-known "dir" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    DirPermission extends CommonFilePermission {
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
    /// A well-known "fd" class permission in SELinux policy that has a particular meaning in policy
    /// enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    FdPermission {
        // keep-sorted start
        /// Permission to use file descriptors copied/retained/inherited from another security
        /// context. This permission is generally used to control whether an `exec*()` call from a
        /// cloned process that retained a copy of the file descriptor table should succeed.
        Use("use"),
        // keep-sorted end
    }
}

class_permission_enum! {
    /// A well-known "bpf" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    BpfPermission {
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
    /// A well-known "perf_event" class permission in SELinux policy that has a particular meaning
    /// in policy hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    PerfEventPermission {
        // keep-sorted start
        /// Permission to monitor the cpu.
        Cpu("cpu"),
        /// Permission to monitor the kernel.
        Kernel("kernel"),
        /// Permission to open a perf event.
        Open("open"),
        /// Permission to read a perf event.
        Read("read"),
        /// Permission to set tracepoints.
        Tracepoint("tracepoint"),
        /// Permission to write a perf event.
        Write("write"),
        // keep-sorted end
    }
}

class_permission_enum! {
    /// A well-known "fifo_file" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    FifoFilePermission extends CommonFilePermission {
    }
}

class_permission_enum! {
    /// A well-known "file" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    FilePermission extends CommonFilePermission {
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
    /// A well-known "filesystem" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    FileSystemPermission {
        // keep-sorted start
        /// Permission to associate a file to the filesystem.
        Associate("associate"),
        /// Permission to get filesystem attributes.
        GetAttr("getattr"),
        /// Permission mount a filesystem.
        Mount("mount"),
        /// Permission to remount a filesystem with different flags.
        Remount("remount"),
        /// Permission to unmount a filesystem.
        Unmount("unmount"),
        // keep-sorted end
    }
}

class_permission_enum! {
    /// A well-known "lnk_file" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    LinkFilePermission extends CommonFilePermission {
    }
}

class_permission_enum! {
    /// A well-known "mem_file" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    MemFdFilePermission extends CommonFilePermission {
    }
}

class_permission_enum! {
    /// A well-known "sock_file" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    SockFilePermission extends CommonFilePermission {
    }
}

class_permission_enum! {
    /// A well-known "process" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    ProcessPermission {
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
    /// A well-known "process2" class permission in SELinux policy that has a particular meaning in
    /// policy enforcement hooks.
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    Process2Permission {
        // keep-sorted start
        /// Permission to transition to an unbounded domain when no-new-privileges is set.
        NnpTransition("nnp_transition"),
        /// Permission to transition domain when executing from a no-SUID mounted filesystem.
        NosuidTransition("nosuid_transition"),
        // keep-sorted end
    }
}

class_permission_enum! {
    /// A well-known "security" class permission in SELinux policy, used to control access to
    /// sensitive administrative and query API surfaces in the "selinuxfs".
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    SecurityPermission {
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
    /// A well-known "system" class permission in SELinux policy, used to control access to
    /// sensitive administrative and query API surfaces in the "selinuxfs".
    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    SystemPermission {
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
