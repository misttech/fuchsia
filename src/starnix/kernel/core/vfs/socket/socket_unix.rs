// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::bpf::context::EbpfRunContextImpl;
use crate::bpf::fs::get_bpf_object;
use crate::mm::MemoryAccessorExt;
use crate::security;
use crate::task::{CurrentTask, EventHandler, WaitCanceler, WaitQueue, Waiter};
use crate::vfs::buffers::{
    AncillaryData, InputBuffer, MessageQueue, MessageReadInfo, OutputBuffer, PasscredMode,
    UnixControlData,
};
use crate::vfs::socket::{
    AcceptQueue, DEFAULT_LISTEN_BACKLOG, SockOptValue, Socket, SocketAddress, SocketBpfState,
    SocketDomain, SocketFile, SocketHandle, SocketMessageFlags, SocketOps, SocketPeer,
    SocketProtocol, SocketShutdownFlags, SocketType,
};
use crate::vfs::{
    AccessCheck, FdNumber, FileHandle, FileObject, FsNodeHandle, FsStr, LookupContext, Message,
    UcredPtr,
};
use ebpf::{
    BpfProgramContext, BpfValue, CbpfConfig, DataWidth, EbpfProgram, Packet, ProgramArgument, Type,
};
use ebpf_api::{
    LoadBytesBase, PacketWithLoadBytes, PinnedMap, ProgramType, SOCKET_FILTER_CBPF_CONFIG,
    SOCKET_FILTER_SK_BUF_TYPE, SocketFilterProgramContext, SocketRef,
};
use starnix_logging::track_stub;
use starnix_sync::{LockDepGuard, LockDepMutex, UnixSocketInnerLock, allow_subclass};
use starnix_syscalls::{SUCCESS, SyscallArg, SyscallResult};
use starnix_uapi::errors::{EACCES, ECONNREFUSED, EINTR, EPERM, Errno};
use starnix_uapi::file_mode::Access;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::user_address::{UserAddress, UserRef};
use starnix_uapi::vfs::FdEvents;
use starnix_uapi::{
    __sk_buff, FIONREAD, SO_ACCEPTCONN, SO_ATTACH_BPF, SO_BROADCAST, SO_ERROR, SO_KEEPALIVE,
    SO_LINGER, SO_NO_CHECK, SO_PASSCRED, SO_PASSSEC, SO_PEERCRED, SO_PEERSEC, SO_RCVBUF,
    SO_REUSEADDR, SO_REUSEPORT, SO_SNDBUF, SOL_SOCKET, errno, error, gid_t, socklen_t, uapi, ucred,
    uid_t,
};
use std::sync::Arc;
use zerocopy::IntoBytes;

// From unix.go in gVisor.
const SOCKET_MIN_SIZE: usize = 4 << 10;
const SOCKET_DEFAULT_SIZE: usize = 208 << 10;
const SOCKET_MAX_SIZE: usize = 4 << 20;

/// The data of a socket is stored in the "Inner" struct. Because both ends have separate locks,
/// care must be taken to avoid taking both locks since there is no way to tell what order to
/// take them in.
///
/// When writing, data is buffered in the "other" end of the socket's Inner.MessageQueue:
///
///            UnixSocket end #1          UnixSocket end #2
///            +---------------+          +---------------+
///            |               |          |   +-------+   |
///   Writes -------------------------------->| Inner |------> Reads
///            |               |          |   +-------+   |
///            |   +-------+   |          |               |
///   Reads <------| Inner |<-------------------------------- Writes
///            |   +-------+   |          |               |
///            +---------------+          +---------------+
///
pub struct UnixSocket {
    inner: LockDepMutex<UnixSocketInner, UnixSocketInnerLock>,
    waiters: WaitQueue,
}

fn downcast_socket_to_unix(socket: &Socket) -> &UnixSocket {
    // It is a programing error if we are downcasting
    // a different type of socket as sockets from different families
    // should not communicate, so unwrapping here
    // will let us know that.
    socket.downcast_socket::<UnixSocket>().unwrap()
}

enum UnixSocketState {
    /// The socket has not been connected.
    Disconnected,

    /// The socket has had `listen` called and can accept incoming connections.
    Listening(AcceptQueue),

    /// The socket is connected to a peer.
    Connected(SocketHandle),

    /// The socket is closed.
    Closed,
}

struct UnixSocketInner {
    /// The `MessageQueue` that contains messages sent to this socket.
    messages: MessageQueue,

    /// The address that this socket has been bound to, if it has been bound.
    address: Option<SocketAddress>,

    /// The `FsNode` that this socket has been bound to in the filesystem, if any.
    ///
    /// Retaining a strong reference while the socket is open prevents the `FsNode` (and its
    /// `bound_socket` association) from being evicted from `FsNodeCache` when its `DirEntry` is
    /// purged from a bounded `DirEntryCache::Lru` (e.g. on `RemoteFs`). This forms a `Socket` <->
    /// `FsNode` cycle that only `close()` breaks, so callers must close bound sockets.
    fs_node: Option<FsNodeHandle>,

    /// Whether this end of the socket has been shut down and can no longer receive message. It is
    /// still possible to send messages to the peer, if it exists and hasn't also been shut down.
    is_read_shutdown: bool,

    /// Whether this end of the socket has been shut down and can no longer send messages.
    is_write_shutdown: bool,

    /// Whether the peer had unread data when it was closed. In this case, reads should return
    /// ECONNRESET instead of 0 (eof).
    peer_closed_with_unread_data: bool,

    /// See SO_LINGER.
    linger: uapi::linger,

    /// See SO_PASSCRED.
    passcred: PasscredMode,

    /// See SO_PASSSEC.
    passsec: bool,

    /// See SO_BROADCAST.
    broadcast: bool,

    /// See SO_NO_CHECK.
    no_check: bool,

    /// See SO_REUSEPORT.
    reuseport: bool,

    /// See SO_REUSEADDR.
    reuseaddr: bool,

    /// See SO_KEEPALIVE.
    keepalive: bool,

    /// See SO_ATTACH_BPF.
    bpf_program: Option<UnixSocketFilter>,

    /// Unix credentials of the peer of this socket, or of the listening socket itself, for
    /// SO_PEERCRED.
    credentials: Option<ucred>,

    /// Socket state: a queue if this is a listening socket, or a peer if this is a connected
    /// socket.
    state: UnixSocketState,
}

impl UnixSocket {
    pub fn new(_socket_type: SocketType) -> UnixSocket {
        UnixSocket {
            inner: UnixSocketInner {
                messages: MessageQueue::new(SOCKET_DEFAULT_SIZE),
                address: None,
                fs_node: None,
                is_read_shutdown: false,
                is_write_shutdown: false,
                peer_closed_with_unread_data: false,
                linger: uapi::linger::default(),
                passcred: PasscredMode::Disabled,
                passsec: false,
                broadcast: false,
                no_check: false,
                reuseaddr: false,
                reuseport: false,
                keepalive: false,
                bpf_program: None,
                credentials: None,
                state: UnixSocketState::Disconnected,
            }
            .into(),
            waiters: WaitQueue::default(),
        }
    }

    /// Creates a pair of connected sockets.
    ///
    /// # Parameters
    /// - `domain`: The domain of the socket (e.g., `AF_UNIX`).
    /// - `socket_type`: The type of the socket (e.g., `SOCK_STREAM`).
    pub fn new_pair(
        current_task: &CurrentTask,
        domain: SocketDomain,
        socket_type: SocketType,
        open_flags: OpenFlags,
    ) -> Result<(FileHandle, FileHandle), Errno> {
        let credentials = current_task.current_ucred();
        let left = Socket::new(
            current_task,
            domain,
            socket_type,
            SocketProtocol::default(),
            /* kernel_private = */ false,
        )?;
        let right = Socket::new(
            current_task,
            domain,
            socket_type,
            SocketProtocol::default(),
            /* kernel_private = */ false,
        )?;
        downcast_socket_to_unix(&left).lock().state = UnixSocketState::Connected(right.clone());
        downcast_socket_to_unix(&left).lock().credentials = Some(credentials.clone());
        downcast_socket_to_unix(&right).lock().state = UnixSocketState::Connected(left.clone());
        downcast_socket_to_unix(&right).lock().credentials = Some(credentials);
        left.set_bpf_state(SocketBpfState::Established);
        right.set_bpf_state(SocketBpfState::Established);
        let left = SocketFile::from_socket(
            current_task,
            left,
            open_flags,
            /* kernel_private= */ false,
        )?;
        let right = SocketFile::from_socket(
            current_task,
            right,
            open_flags,
            /* kernel_private= */ false,
        )?;
        let left_socket = SocketFile::get_from_file(&left)?;
        let right_socket = SocketFile::get_from_file(&right)?;

        security::socket_socketpair(current_task, left_socket, right_socket)?;
        Ok((left, right))
    }

    fn connect_stream(
        &self,
        socket: &SocketHandle,
        current_task: &CurrentTask,
        peer: &SocketHandle,
    ) -> Result<(), Errno> {
        // Only hold one lock at a time until we make sure the lock ordering
        // is right: client before listener
        match downcast_socket_to_unix(peer).lock().state {
            UnixSocketState::Listening(_) => {}
            _ => return error!(ECONNREFUSED),
        }

        let mut client = downcast_socket_to_unix(socket).lock();
        match client.state {
            UnixSocketState::Disconnected => {}
            UnixSocketState::Connected(_) => return error!(EISCONN),
            _ => return error!(EINVAL),
        };

        let unix_socket_peer = downcast_socket_to_unix(peer);
        {
            // Lock ordering is client before listener.
            let _token = allow_subclass();
            let mut listener = unix_socket_peer.lock();

            // Must check this again because we released the listener lock for a moment
            let queue = match &listener.state {
                UnixSocketState::Listening(queue) => queue,
                _ => return error!(ECONNREFUSED),
            };

            self.check_type_for_connect(socket, peer, &listener.address)?;

            if queue.sockets.len() > queue.backlog {
                return error!(EAGAIN);
            }

            let server = Socket::new(
                current_task,
                peer.domain,
                peer.socket_type,
                SocketProtocol::default(),
                /* kernel_private = */ true,
            )?;
            security::unix_stream_connect(current_task, socket, peer, &server)?;
            client.state = UnixSocketState::Connected(server.clone());
            client.credentials = listener.credentials.clone();
            {
                // This allow_subclass is safe because `server` is a newly created socket
                // that hasn't been added to any public table or returned to the user yet.
                // It is unreachable by other threads, making lock ordering cycles impossible.
                let _token = allow_subclass();
                let mut server = downcast_socket_to_unix(&server).lock();
                server.state = UnixSocketState::Connected(socket.clone());
                server.address = listener.address.clone();
                server.messages.set_capacity(listener.messages.capacity())?;
                server.credentials = Some(current_task.current_ucred());
                server.passcred = listener.passcred;
                server.passsec = listener.passsec;
            }

            // We already checked that the socket is in Listening state...but the borrow checker cannot
            // be convinced that it's ok to combine these checks
            let queue = match listener.state {
                UnixSocketState::Listening(ref mut queue) => queue,
                _ => panic!("something changed the server socket state while I held a lock on it"),
            };
            queue.sockets.push_back(server);
        }
        unix_socket_peer.waiters.notify_fd_events(FdEvents::POLLIN);
        Ok(())
    }

    fn connect_datagram(
        &self,
        socket: &SocketHandle,
        current_task: &CurrentTask,
        peer: &SocketHandle,
    ) -> Result<(), Errno> {
        {
            let unix_socket = socket.downcast_socket::<UnixSocket>().unwrap();
            let peer_inner = unix_socket.lock();
            self.check_type_for_connect(socket, peer, &peer_inner.address)?;
        }
        security::unix_may_send(current_task, socket, peer)?;
        let unix_socket = socket.downcast_socket::<UnixSocket>().unwrap();
        unix_socket.lock().state = UnixSocketState::Connected(peer.clone());
        Ok(())
    }

    pub fn check_type_for_connect(
        &self,
        socket: &Socket,
        peer: &Socket,
        peer_address: &Option<SocketAddress>,
    ) -> Result<(), Errno> {
        if socket.domain != peer.domain || socket.socket_type != peer.socket_type {
            // According to ConnectWithWrongType in accept_bind_test, abstract
            // UNIX domain sockets return ECONNREFUSED rather than EPROTOTYPE.
            if let Some(address) = peer_address {
                if address.is_abstract_unix() {
                    return error!(ECONNREFUSED);
                }
            }
            return error!(EPROTOTYPE);
        }
        Ok(())
    }

    /// Locks and returns the inner state of the Socket.
    fn lock(&self) -> LockDepGuard<'_, UnixSocketInner> {
        self.inner.lock()
    }

    fn is_listening(&self, _socket: &Socket) -> bool {
        matches!(self.lock().state, UnixSocketState::Listening(_))
    }

    fn get_receive_capacity(&self) -> usize {
        self.lock().messages.capacity()
    }

    fn set_receive_capacity(&self, requested_capacity: usize) {
        self.lock().set_capacity(requested_capacity);
    }

    fn get_send_capacity(&self) -> usize {
        let peer = {
            if let Some(peer) = self.lock().peer() {
                peer.clone()
            } else {
                return 0;
            }
        };
        let unix_socket = downcast_socket_to_unix(&peer);
        let capacity = unix_socket.lock().messages.capacity();
        capacity
    }

    fn set_send_capacity(&self, requested_capacity: usize) {
        let peer = {
            if let Some(peer) = self.lock().peer() {
                peer.clone()
            } else {
                return;
            }
        };
        let unix_socket = downcast_socket_to_unix(&peer);
        unix_socket.lock().set_capacity(requested_capacity);
    }

    fn get_linger(&self) -> uapi::linger {
        let inner = self.lock();
        inner.linger
    }

    fn set_linger(&self, linger: uapi::linger) {
        let mut inner = self.lock();
        inner.linger = linger;
    }

    fn get_passcred(&self) -> PasscredMode {
        let inner = self.lock();
        inner.passcred
    }

    fn set_passcred(&self, passcred: PasscredMode) {
        let mut inner = self.lock();
        inner.passcred = passcred;
    }

    fn get_passsec(&self) -> bool {
        let inner = self.lock();
        inner.passsec
    }

    fn set_passsec(&self, passsec: bool) {
        let mut inner = self.lock();
        inner.passsec = passsec;
    }

    fn get_broadcast(&self) -> bool {
        let inner = self.lock();
        inner.broadcast
    }

    fn set_broadcast(&self, broadcast: bool) {
        let mut inner = self.lock();
        inner.broadcast = broadcast;
    }

    fn get_no_check(&self) -> bool {
        let inner = self.lock();
        inner.no_check
    }

    fn set_no_check(&self, no_check: bool) {
        let mut inner = self.lock();
        inner.no_check = no_check;
    }

    fn get_reuseaddr(&self) -> bool {
        let inner = self.lock();
        inner.reuseaddr
    }

    fn set_reuseaddr(&self, reuseaddr: bool) {
        let mut inner = self.lock();
        inner.reuseaddr = reuseaddr;
    }

    fn get_reuseport(&self) -> bool {
        let inner = self.lock();
        inner.reuseport
    }

    fn set_reuseport(&self, reuseport: bool) {
        let mut inner = self.lock();
        inner.reuseport = reuseport;
    }

    fn get_keepalive(&self) -> bool {
        let inner = self.lock();
        inner.keepalive
    }

    fn set_keepalive(&self, keepalive: bool) {
        let mut inner = self.lock();
        inner.keepalive = keepalive;
    }

    fn set_bpf_program(&self, program: Option<UnixSocketFilter>) {
        let mut inner = self.lock();
        inner.bpf_program = program;
    }

    fn peer_cred(&self) -> Option<ucred> {
        self.lock().credentials.clone()
    }

    pub fn bind_socket_to_node(
        &self,
        socket: &SocketHandle,
        address: SocketAddress,
        node: &FsNodeHandle,
    ) -> Result<(), Errno> {
        let unix_socket = downcast_socket_to_unix(socket);
        let mut inner = unix_socket.lock();
        inner.bind(address)?;
        inner.fs_node = Some(node.clone());
        node.set_bound_socket(socket.clone());
        Ok(())
    }

    fn notify_shutdown(&self) {
        self.waiters.notify_fd_events(FdEvents::POLLIN | FdEvents::POLLOUT | FdEvents::POLLHUP);
    }
}

impl SocketOps for UnixSocket {
    fn connect(
        &self,
        socket: &SocketHandle,
        current_task: &CurrentTask,
        peer: SocketPeer,
    ) -> Result<(), Errno> {
        let peer = match peer {
            SocketPeer::Handle(handle) => handle,
            SocketPeer::Address(SocketAddress::Unspecified) => {
                if socket.socket_type == SocketType::Datagram {
                    let unix_socket = socket.downcast_socket::<UnixSocket>().unwrap();
                    unix_socket.lock().state = UnixSocketState::Disconnected;
                    return Ok(());
                }
                return error!(EINVAL);
            }
            SocketPeer::Address(_) => return error!(EINVAL),
        };
        match socket.socket_type {
            SocketType::Stream | SocketType::SeqPacket => {
                self.connect_stream(socket, current_task, &peer)
            }
            SocketType::Datagram | SocketType::Raw => {
                self.connect_datagram(socket, current_task, &peer)
            }
            _ => error!(EINVAL),
        }
    }

    fn listen(&self, socket: &Socket, backlog: i32, credentials: ucred) -> Result<(), Errno> {
        match socket.socket_type {
            SocketType::Stream | SocketType::SeqPacket => {}
            _ => return error!(EOPNOTSUPP),
        }
        let mut inner = self.lock();
        let is_bound = inner.address.is_some();
        let backlog = if backlog < 0 { DEFAULT_LISTEN_BACKLOG } else { backlog as usize };
        match &mut inner.state {
            UnixSocketState::Disconnected if is_bound => {
                inner.state = UnixSocketState::Listening(AcceptQueue::new(backlog));
            }
            UnixSocketState::Listening(queue) => {
                queue.set_backlog(backlog)?;
            }
            _ => return error!(EINVAL),
        }
        inner.credentials = Some(credentials);
        Ok(())
    }

    fn accept(&self, socket: &Socket, _current_task: &CurrentTask) -> Result<SocketHandle, Errno> {
        match socket.socket_type {
            SocketType::Stream | SocketType::SeqPacket => {}
            _ => return error!(EOPNOTSUPP),
        }
        let mut inner = self.lock();
        let queue = match &mut inner.state {
            UnixSocketState::Listening(queue) => queue,
            _ => return error!(EINVAL),
        };
        queue.sockets.pop_front().ok_or_else(|| errno!(EAGAIN))
    }

    fn bind(
        &self,
        _socket: &Socket,
        _current_task: &CurrentTask,
        socket_address: SocketAddress,
    ) -> Result<(), Errno> {
        match socket_address {
            SocketAddress::Unix(_) => {}
            _ => return error!(EINVAL),
        }
        self.lock().bind(socket_address)
    }

    fn read(
        &self,
        socket: &Socket,
        current_task: &CurrentTask,
        data: &mut dyn OutputBuffer,
        flags: SocketMessageFlags,
    ) -> Result<MessageReadInfo, Errno> {
        let mut info = MessageReadInfo::default();
        self.read_and_append(socket, current_task, &mut info, data, flags)?;
        Ok(info)
    }

    fn read_and_append(
        &self,
        socket: &Socket,
        _current_task: &CurrentTask,
        read_info: &mut MessageReadInfo,
        data: &mut dyn OutputBuffer,
        flags: SocketMessageFlags,
    ) -> Result<bool, Errno> {
        let bytes_read_before = read_info.bytes_read;
        let (can_coalesce, peer) = {
            let mut inner = self.lock();
            let can_coalesce = inner.read_and_append(read_info, data, socket.socket_type, flags)?;
            let peer =
                (read_info.bytes_read > bytes_read_before).then(|| inner.peer().cloned()).flatten();
            (can_coalesce, peer)
        };
        if let Some(socket) = peer {
            if let Some(socket) = socket.downcast_socket::<UnixSocket>() {
                socket.waiters.notify_fd_events(FdEvents::POLLOUT);
            }
        }
        Ok(can_coalesce)
    }

    fn write(
        &self,
        socket: &Socket,
        current_task: &CurrentTask,
        data: &mut dyn InputBuffer,
        dest_address: &mut Option<SocketAddress>,
        ancillary_data: &mut Vec<AncillaryData>,
    ) -> Result<usize, Errno> {
        let (connected_peer, local_address, local_passcred, is_write_shutdown) = {
            let inner = self.lock();
            (
                inner.peer().map(|p| p.clone()),
                inner.address.clone(),
                inner.passcred,
                inner.is_write_shutdown,
            )
        };

        if is_write_shutdown {
            return error!(EPIPE);
        }

        let peer = match (connected_peer, dest_address, socket.socket_type) {
            (Some(peer), None, _) => peer,
            (Some(peer), Some(_), SocketType::SeqPacket) => peer,
            (None, Some(_), SocketType::Stream) => return error!(EOPNOTSUPP),
            (None, Some(_), SocketType::SeqPacket) => return error!(ENOTCONN),
            (Some(_), Some(_), SocketType::Stream) => return error!(EISCONN),
            (_, Some(SocketAddress::Unix(name)), _) => {
                if name.is_empty() {
                    return error!(EINVAL);
                }
                resolve_unix_socket_address(current_task, name.as_ref())?
            }
            (_, Some(_), _) => return error!(EINVAL),
            (None, None, _) => return error!(ENOTCONN),
        };

        let dgram_security_context = if socket.socket_type == SocketType::Datagram {
            security::unix_may_send(current_task, socket, &peer)?;
            // TODO: https://fxbug.dev/364568855 - Store the opaque LSM property value, and expand
            // it to a string upon readmsg.
            Some(security::socket_getpeersec_dgram(current_task, socket))
        } else {
            None
        };

        let peer_unaccepted = peer.fs_node().is_none();
        let unix_socket = downcast_socket_to_unix(&peer);
        let write_result = {
            let mut peer = unix_socket.lock();
            let default_creds =
                (local_passcred.is_enabled() || peer.passcred.is_enabled() || peer_unaccepted)
                    .then(|| current_task.current_ucred());
            peer.write(
                current_task,
                data,
                local_address,
                default_creds,
                dgram_security_context,
                ancillary_data,
                socket,
            )
        };

        if let Err(ref err) = write_result {
            if err.code == ECONNREFUSED && socket.socket_type == SocketType::Datagram {
                let mut inner = self.lock();
                if let UnixSocketState::Connected(ref connected_peer) = inner.state {
                    if Arc::ptr_eq(connected_peer, &peer) {
                        inner.state = UnixSocketState::Disconnected;
                        self.waiters.notify_fd_events(FdEvents::POLLOUT | FdEvents::POLLHUP);
                    }
                }
            }
        }

        let bytes_written = write_result?;
        if bytes_written > 0 {
            unix_socket.waiters.notify_fd_events(FdEvents::POLLIN);
        }
        Ok(bytes_written)
    }

    fn wait_async(
        &self,
        _socket: &Socket,
        _current_task: &CurrentTask,
        waiter: &Waiter,
        events: FdEvents,
        handler: EventHandler,
    ) -> WaitCanceler {
        self.waiters.wait_async_fd_events(waiter, events, handler)
    }

    fn query_events(
        &self,
        socket: &Socket,
        _current_task: &CurrentTask,
    ) -> Result<FdEvents, Errno> {
        // Note that self.lock() must be dropped before acquiring peer.inner.lock() to avoid
        // potential deadlocks.
        let (
            mut events,
            peer,
            local_is_read_shutdown,
            local_is_write_shutdown,
            is_closed,
            is_disconnected,
        ) = {
            let inner = self.lock();

            let mut events = FdEvents::empty();
            let local_events = inner.messages.query_events();
            if local_events.contains(FdEvents::POLLIN) {
                events |= FdEvents::POLLIN;
            }

            // Listening socket gets POLLIN when there are pending connections.
            if let UnixSocketState::Listening(queue) = &inner.state {
                if !queue.sockets.is_empty() {
                    events |= FdEvents::POLLIN;
                }
            }

            (
                events,
                inner.peer().cloned(),
                inner.is_read_shutdown,
                inner.is_write_shutdown,
                matches!(inner.state, UnixSocketState::Closed),
                matches!(inner.state, UnixSocketState::Disconnected),
            )
        };

        let connection_oriented = socket.socket_type.is_connection_oriented();

        let mut read_dead = local_is_read_shutdown || is_closed;
        let mut write_dead = local_is_write_shutdown || is_closed;
        let mut force_pollout = is_closed || is_disconnected;
        let mut force_pollhup = false;

        if connection_oriented && is_disconnected {
            force_pollhup = true;
            write_dead = true;
        }

        if let Some(peer) = peer {
            let unix_socket = downcast_socket_to_unix(&peer);
            let peer_inner = unix_socket.lock();

            if peer_inner.is_read_shutdown {
                write_dead = true;
            }
            if matches!(peer_inner.state, UnixSocketState::Closed) {
                read_dead = true;
                write_dead = true;
                force_pollout = true;
            }

            let peer_events = peer_inner.messages.query_events();
            if peer_events.contains(FdEvents::POLLOUT) {
                events |= FdEvents::POLLOUT;
            }
        }

        if force_pollout {
            events |= FdEvents::POLLOUT;
        }

        if read_dead {
            events |= FdEvents::POLLIN | FdEvents::POLLRDHUP;
        }
        if (read_dead && write_dead) || force_pollhup {
            events |= FdEvents::POLLHUP;
        }

        Ok(events)
    }

    /// Shuts down reception and/or transmission on this socket according to `how`
    /// ([`SocketShutdownFlags`]), notifying waiters with the resulting [`FdEvents`].
    ///
    /// Used by the `shutdown(2)` syscall.
    fn shutdown(&self, socket: &Socket, how: SocketShutdownFlags) -> Result<(), Errno> {
        // Acquire and release `self.lock()` before locking `peer` below to avoid ABBA deadlocks
        // when both endpoints call `shutdown(2)` concurrently.
        let (peer, self_notify_events) = {
            let mut inner = self.lock();
            let events = inner.shutdown(how);
            (inner.peer().cloned(), events)
        };
        let mut peer_notify_events = FdEvents::empty();
        if let Some(peer) = &peer {
            if socket.socket_type.is_connection_oriented() {
                // For a connected `AF_UNIX` pair `(self, peer)`, the two unidirectional
                // channels are:
                //   - `self -> peer`: controlled by `(self.is_write_shutdown, peer.is_read_shutdown)`
                //   - `peer -> self`: controlled by `(peer.is_write_shutdown, self.is_read_shutdown)`
                //
                // Transposing `READ` <-> `WRITE` on `peer` ensures that `SHUT_WR` on `self` shuts
                // down reads on `peer` (`peer_inner.is_read_shutdown`) and `SHUT_RD` on `self`
                // shuts down writes on `peer` (`peer_inner.is_write_shutdown`).
                let mut peer_how = SocketShutdownFlags::empty();
                if how.contains(SocketShutdownFlags::WRITE) {
                    peer_how |= SocketShutdownFlags::READ;
                }
                if how.contains(SocketShutdownFlags::READ) {
                    peer_how |= SocketShutdownFlags::WRITE;
                }
                let peer_socket = downcast_socket_to_unix(peer);
                peer_notify_events = peer_socket.lock().shutdown(peer_how);
            }
        }
        if !self_notify_events.is_empty() {
            self.waiters.notify_fd_events(self_notify_events);
        }
        if !peer_notify_events.is_empty() {
            if let Some(peer) = &peer {
                let peer_socket = downcast_socket_to_unix(peer);
                peer_socket.waiters.notify_fd_events(peer_notify_events);
            }
        }
        Ok(())
    }

    /// Close this socket.
    ///
    /// Called by SocketFile when the file descriptor that is holding this
    /// socket is closed.
    ///
    /// Close differs from shutdown in two ways. First, close will call
    /// mark_peer_closed_with_unread_data if this socket has unread data,
    /// which changes how read() behaves on that socket. Second, close
    /// transitions the internal state of this socket to Closed and releases
    /// any bound filesystem node, which breaks the reference cycles that exist
    /// in the connected and bound states.
    fn close(&self, _current_task: &CurrentTask, socket: &Socket) {
        // Keep `_maybe_fs_node` in the tuple so that it is dropped outside of `self.lock()`.
        let (maybe_peer, has_unread, _maybe_fs_node) = {
            let mut inner = self.lock();
            let maybe_peer = inner.peer().map(Arc::clone);
            inner.is_read_shutdown = true;
            inner.state = UnixSocketState::Closed;
            (maybe_peer, !inner.messages.is_empty(), inner.fs_node.take())
        };
        self.notify_shutdown();
        // If this is a connected socket type, also shut down the connected peer.
        if socket.socket_type.is_connection_oriented() {
            if let Some(peer) = maybe_peer {
                let unix_socket = downcast_socket_to_unix(&peer);

                {
                    let mut peer_inner = unix_socket.lock();
                    if has_unread {
                        peer_inner.peer_closed_with_unread_data = true;
                    }
                    peer_inner.is_read_shutdown = true;
                }
                unix_socket.notify_shutdown();
            }
        }
    }

    /// Returns the name of this socket.
    ///
    /// The name is derived from the address and domain. A socket
    /// will always have a name, even if it is not bound to an address.
    fn getsockname(&self, socket: &Socket) -> Result<SocketAddress, Errno> {
        let inner = self.lock();
        if let Some(address) = &inner.address {
            Ok(address.clone())
        } else {
            Ok(SocketAddress::default_for_domain(socket.domain))
        }
    }

    /// Returns the name of the peer of this socket, if such a peer exists.
    ///
    /// Returns an error if the socket is not connected.
    fn getpeername(&self, _socket: &Socket) -> Result<SocketAddress, Errno> {
        let peer = self.lock().peer().ok_or_else(|| errno!(ENOTCONN))?.clone();
        peer.getsockname()
    }

    fn setsockopt(
        &self,
        _socket: &Socket,
        current_task: &CurrentTask,
        level: u32,
        optname: u32,
        optval: SockOptValue,
    ) -> Result<(), Errno> {
        match level {
            SOL_SOCKET => match optname {
                SO_SNDBUF => {
                    let requested_capacity: socklen_t = optval.read(current_task)?;
                    // See StreamUnixSocketPairTest.SetSocketSendBuf for why we multiply by 2 here.
                    self.set_send_capacity(requested_capacity as usize * 2);
                }
                SO_RCVBUF => {
                    let requested_capacity: socklen_t = optval.read(current_task)?;
                    self.set_receive_capacity(requested_capacity as usize);
                }
                SO_LINGER => {
                    let mut linger: uapi::linger = optval.read(current_task)?;
                    if linger.l_onoff != 0 {
                        linger.l_onoff = 1;
                    }
                    self.set_linger(linger);
                }
                SO_PASSCRED => {
                    let passcred: u32 = optval.read(current_task)?;
                    self.set_passcred(PasscredMode::from(passcred != 0));
                }
                SO_PASSSEC => {
                    let passsec: u32 = optval.read(current_task)?;
                    self.set_passsec(passsec != 0);
                }
                SO_BROADCAST => {
                    let broadcast: u32 = optval.read(current_task)?;
                    self.set_broadcast(broadcast != 0);
                }
                SO_NO_CHECK => {
                    let no_check: u32 = optval.read(current_task)?;
                    self.set_no_check(no_check != 0);
                }
                SO_REUSEADDR => {
                    let reuseaddr: u32 = optval.read(current_task)?;
                    self.set_reuseaddr(reuseaddr != 0);
                }
                SO_REUSEPORT => {
                    let reuseport: u32 = optval.read(current_task)?;
                    self.set_reuseport(reuseport != 0);
                }
                SO_KEEPALIVE => {
                    let keepalive: u32 = optval.read(current_task)?;
                    self.set_keepalive(keepalive != 0);
                }
                SO_ATTACH_BPF => {
                    let fd: FdNumber = optval.read(current_task)?;
                    let object = get_bpf_object(current_task, fd)?;
                    let program = object.as_program()?;

                    let linked_program = program.link(ProgramType::SocketFilter)?;

                    self.set_bpf_program(Some(linked_program));
                }
                _ => return error!(ENOPROTOOPT),
            },
            _ => return error!(ENOPROTOOPT),
        }
        Ok(())
    }

    fn getsockopt(
        &self,
        socket: &Socket,
        current_task: &CurrentTask,
        level: u32,
        optname: u32,
        _optlen: u32,
    ) -> Result<Vec<u8>, Errno> {
        match level {
            SOL_SOCKET => match optname {
                SO_PEERCRED => Ok(UcredPtr::into_bytes(
                    current_task,
                    self.peer_cred().unwrap_or(ucred { pid: 0, uid: uid_t::MAX, gid: gid_t::MAX }),
                )
                .map_err(|_| errno!(EINVAL))?),
                SO_PEERSEC => match socket.socket_type {
                    SocketType::Stream => security::socket_getpeersec_stream(current_task, socket),
                    _ => error!(ENOPROTOOPT),
                },
                SO_ACCEPTCONN =>
                {
                    #[allow(clippy::bool_to_int_with_if)]
                    Ok(if self.is_listening(socket) { 1u32 } else { 0u32 }.to_ne_bytes().to_vec())
                }
                SO_SNDBUF => Ok((self.get_send_capacity() as socklen_t).to_ne_bytes().to_vec()),
                SO_RCVBUF => Ok((self.get_receive_capacity() as socklen_t).to_ne_bytes().to_vec()),
                SO_LINGER => Ok(self.get_linger().as_bytes().to_vec()),
                SO_PASSCRED => Ok((self.get_passcred().is_enabled() as u32).as_bytes().to_vec()),
                SO_PASSSEC => Ok((self.get_passsec() as u32).as_bytes().to_vec()),
                SO_BROADCAST => Ok((self.get_broadcast() as u32).as_bytes().to_vec()),
                SO_NO_CHECK => Ok((self.get_no_check() as u32).as_bytes().to_vec()),
                SO_REUSEADDR => Ok((self.get_reuseaddr() as u32).as_bytes().to_vec()),
                SO_REUSEPORT => Ok((self.get_reuseport() as u32).as_bytes().to_vec()),
                SO_KEEPALIVE => Ok((self.get_keepalive() as u32).as_bytes().to_vec()),
                SO_ERROR => Ok((0u32).as_bytes().to_vec()),
                _ => error!(ENOPROTOOPT),
            },
            _ => error!(ENOPROTOOPT),
        }
    }

    fn ioctl(
        &self,
        socket: &Socket,
        _file: &FileObject,
        current_task: &CurrentTask,
        request: u32,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        let user_addr = UserAddress::from(arg);
        match request {
            FIONREAD if socket.socket_type == SocketType::Stream => {
                let length: i32 =
                    self.lock().messages.len().try_into().map_err(|_| errno!(EINVAL))?;
                current_task.write_object(UserRef::<i32>::new(user_addr), &length)?;
                Ok(SUCCESS)
            }
            _ => error!(ENOTTY),
        }
    }
}

impl UnixSocketInner {
    fn bind(&mut self, socket_address: SocketAddress) -> Result<(), Errno> {
        if self.address.is_some() {
            return error!(EINVAL);
        }
        self.address = Some(socket_address);
        Ok(())
    }

    /// Shuts down the local endpoint directions specified by `how` ([`SocketShutdownFlags`])
    /// and returns the [`FdEvents`] to notify on this endpoint's wait queue.
    ///
    /// Per `shutdown(2)`:
    /// > "The `shutdown()` call causes all or part of a full-duplex connection on
    /// > the socket associated with `sockfd` to be shut down. If `how` is `SHUT_RD`,
    /// > further receptions will be disallowed. If `how` is `SHUT_WR`, further
    /// > transmissions will be disallowed. If `how` is `SHUT_RDWR`, further
    /// > receptions and transmissions will be disallowed."
    ///
    /// Per `poll(2)`:
    /// > "`POLLHUP`: Hang up (only returned in `revents`; ignored in `events`).
    /// > Note that when reading from a channel such as a pipe or a stream socket,
    /// > this event merely indicates that the peer closed its end of the channel."
    /// > "`POLLRDHUP` (since Linux 2.6.17): Stream socket peer closed connection,
    /// > or shut down writing half of connection."
    fn shutdown(&mut self, how: SocketShutdownFlags) -> FdEvents {
        let mut notify_events = FdEvents::empty();
        if how.contains(SocketShutdownFlags::READ) {
            self.is_read_shutdown = true;
            notify_events |= FdEvents::POLLIN | FdEvents::POLLRDHUP;
        }
        if how.contains(SocketShutdownFlags::WRITE) {
            self.is_write_shutdown = true;
            notify_events |= FdEvents::POLLOUT;
        }
        if self.is_read_shutdown && self.is_write_shutdown {
            notify_events |= FdEvents::POLLHUP;
        }
        notify_events
    }

    fn set_capacity(&mut self, requested_capacity: usize) {
        let capacity = requested_capacity.clamp(SOCKET_MIN_SIZE, SOCKET_MAX_SIZE);
        let capacity = std::cmp::max(capacity, self.messages.len());
        // We have validated capacity sufficiently that set_capacity should always succeed.
        self.messages.set_capacity(capacity).unwrap();
    }

    /// Returns the socket that is connected to this socket, if such a peer exists. Returns
    /// ENOTCONN otherwise.
    fn peer(&self) -> Option<&SocketHandle> {
        match &self.state {
            UnixSocketState::Connected(peer) => Some(peer),
            _ => None,
        }
    }

    /// Reads the the contents of this socket into `InputBuffer`.
    ///
    /// Will stop reading if a message with ancillary data is encountered (after the message with
    /// ancillary data has been read).
    ///
    /// # Parameters
    /// - `data`: The `OutputBuffer` to write the data to.
    ///
    /// Returns the number of bytes that were read into the buffer, and any ancillary data that was
    /// read from the socket.
    fn read(
        &mut self,
        data: &mut dyn OutputBuffer,
        socket_type: SocketType,
        flags: SocketMessageFlags,
    ) -> Result<MessageReadInfo, Errno> {
        let (mut info, has_message) = if socket_type == SocketType::Stream {
            if flags.contains(SocketMessageFlags::PEEK) {
                self.messages.peek_stream(data, self.passcred)?
            } else {
                self.messages.read_stream_with_passcred(data, self.passcred)?
            }
        } else if flags.contains(SocketMessageFlags::PEEK) {
            self.messages.peek_datagram(data)?
        } else {
            self.messages.read_datagram(data)?
        };
        if !has_message {
            if self.peer_closed_with_unread_data {
                // Reset the flag
                self.peer_closed_with_unread_data = false;
                return error!(ECONNRESET);
            }
            if !self.is_read_shutdown {
                return error!(EAGAIN);
            }
        }

        if self.passcred.is_enabled() {
            let creds = info.credentials.unwrap_or_else(UnixControlData::unknown_ucred);
            info.credentials = Some(creds);
            // Allow credentials to take priority if they are enabled, so insert at 0.
            info.ancillary_data.insert(0, AncillaryData::Unix(UnixControlData::Credentials(creds)));
        } else {
            info.credentials = None;
        }

        // Security labels are only delivered if passsec is currently enabled on this socket.
        if !self.passsec {
            info.ancillary_data
                .retain(|m| !matches!(m, AncillaryData::Unix(UnixControlData::Security(..))));
        }

        Ok(info)
    }

    fn read_and_append(
        &mut self,
        read_info: &mut MessageReadInfo,
        data: &mut dyn OutputBuffer,
        socket_type: SocketType,
        flags: SocketMessageFlags,
    ) -> Result<bool, Errno> {
        if socket_type == SocketType::Stream
            && !self.messages.can_coalesce_with(read_info, self.passcred)
        {
            return Ok(false);
        }
        let mut info = self.read(data, socket_type, flags)?;
        let bytes_read = info.bytes_read;
        read_info.append(&mut info);
        if socket_type != SocketType::Stream || bytes_read == 0 {
            return Ok(false);
        }
        Ok(self.messages.can_coalesce_with(read_info, self.passcred))
    }

    /// Writes the the contents of `InputBuffer` into this socket.
    ///
    /// # Parameters
    /// - `data`: The `InputBuffer` to read the data from.
    /// - `ancillary_data`: Any ancillary data to write to the socket. Note that the ancillary data
    ///                     will only be written if the entirety of the requested write completes.
    ///
    /// Returns the number of bytes that were written to the socket.
    fn write(
        &mut self,
        current_task: &CurrentTask,
        data: &mut dyn InputBuffer,
        address: Option<SocketAddress>,
        default_credentials: Option<ucred>,
        dgram_security_context: Option<Vec<u8>>,
        ancillary_data: &mut Vec<AncillaryData>,
        socket: &Socket,
    ) -> Result<usize, Errno> {
        if matches!(self.state, UnixSocketState::Closed) {
            if !socket.socket_type.is_connection_oriented() {
                return error!(ECONNREFUSED);
            } else {
                return error!(EPIPE);
            }
        }
        if self.is_read_shutdown {
            return error!(EPIPE);
        }
        let filter = |mut message: Message| {
            if let Some(context) = dgram_security_context {
                message
                    .ancillary_data
                    .push(AncillaryData::Unix(UnixControlData::Security(context.into())));
            }
            let Some(bpf_program) = self.bpf_program.as_ref() else {
                return Some(message);
            };

            // TODO(https://fxbug.dev/385015056): Fill in SkBuf.
            let mut sk_buf = SkBuf::default();

            let mut context = EbpfRunContextImpl::<'_>::new(current_task);
            let s = bpf_program.run(&mut context, &mut sk_buf);
            if s == 0 {
                None
            } else {
                message.truncate(s as usize);
                Some(message)
            }
        };
        let bytes_written = if socket.socket_type == SocketType::Stream {
            self.messages.write_stream_with_filter(
                data,
                address,
                default_credentials,
                ancillary_data,
                filter,
            )?
        } else {
            self.messages.write_datagram_with_filter(
                data,
                address,
                default_credentials,
                ancillary_data,
                filter,
            )?
        };
        Ok(bytes_written)
    }
}

pub fn resolve_unix_socket_address(
    current_task: &CurrentTask,
    name: &FsStr,
) -> Result<SocketHandle, Errno> {
    if name[0] == b'\0' {
        current_task.running_state().abstract_socket_namespace.lookup(name)
    } else {
        let mut context = LookupContext::default();
        let (parent, basename) =
            current_task.lookup_parent_at(&mut context, FdNumber::AT_FDCWD, name)?;
        let name = parent.lookup_child(current_task, &mut context, basename).map_err(|errno| {
            if matches!(errno.code, EACCES | EPERM | EINTR) { errno } else { errno!(ECONNREFUSED) }
        })?;
        name.check_access(current_task, AccessCheck::for_internal(Access::WRITE))?;
        name.entry.node.bound_socket().map(|s| s.clone()).ok_or_else(|| errno!(ECONNREFUSED))
    }
}

// Packet buffer representation used for eBPF filters.
#[repr(C)]
#[derive(Default)]
struct SkBuf {
    sk_buff: __sk_buff,
}

impl Packet for &mut SkBuf {
    fn load(&self, _offset: i32, _width: DataWidth) -> Option<BpfValue> {
        // TODO(https://fxbug.dev/385015056): Implement packet access.
        None
    }
}

impl<'a> PacketWithLoadBytes for &'a mut SkBuf {
    fn load_bytes_relative(
        &self,
        _base: LoadBytesBase,
        _offset: usize,
        _buf: ebpf::EbpfBufferPtr<'_>,
    ) -> i64 {
        track_stub!(TODO("https://fxbug.dev/385015056"), "bpf_load_bytes_relative");
        -1
    }
}

impl ProgramArgument for &'_ mut SkBuf {
    fn get_type() -> &'static Type {
        &*SOCKET_FILTER_SK_BUF_TYPE
    }
}

impl SocketRef for &'_ mut SkBuf {
    fn get_socket_cookie(&self) -> Option<u64> {
        track_stub!(TODO("https://fxbug.dev/385015056"), "bpf_get_socket_cookie");
        None
    }

    fn get_socket_uid(&self) -> Option<uid_t> {
        track_stub!(TODO("https://fxbug.dev/385015056"), "bpf_get_socket_uid");
        None
    }
}

struct UnixSocketEbpfContext {}
impl BpfProgramContext for UnixSocketEbpfContext {
    type RunContext<'a> = EbpfRunContextImpl<'a>;
    type Packet<'a> = &'a mut SkBuf;
    type Map = PinnedMap;
    const CBPF_CONFIG: &'static CbpfConfig = &SOCKET_FILTER_CBPF_CONFIG;
}

ebpf_api::ebpf_program_context_type!(UnixSocketEbpfContext, SocketFilterProgramContext);

type UnixSocketFilter = EbpfProgram<UnixSocketEbpfContext>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::MemoryAccessor;
    use crate::testing::{map_memory, spawn_kernel_and_run};
    use crate::vfs::{DirectoryMode, UnlinkKind};
    use starnix_types::user_buffer::UserBuffer;
    use starnix_uapi::file_mode::mode;

    #[::fuchsia::test]
    async fn test_socket_send_capacity() {
        spawn_kernel_and_run(async |current_task| {
            let _kernel = current_task.kernel();
            let socket = Socket::new(
                &current_task,
                SocketDomain::Unix,
                SocketType::Stream,
                SocketProtocol::default(),
                /* kernel_private = */ false,
            )
            .expect("Failed to create socket.");
            socket
                .bind(&current_task, SocketAddress::Unix(b"\0".into()))
                .expect("Failed to bind socket.");
            socket.listen(&current_task, 10).expect("Failed to listen.");
            let connecting_socket = Socket::new(
                &current_task,
                SocketDomain::Unix,
                SocketType::Stream,
                SocketProtocol::default(),
                /* kernel_private = */ false,
            )
            .expect("Failed to connect socket.");
            connecting_socket
                .ops
                .connect(&connecting_socket, &current_task, SocketPeer::Handle(socket.clone()))
                .expect("Failed to connect socket.");
            assert_eq!(Ok(FdEvents::POLLIN), socket.query_events(&current_task));
            let server_socket = socket.accept(&current_task).unwrap();

            let opt_size = std::mem::size_of::<socklen_t>();
            let user_address = map_memory(&current_task, UserAddress::default(), opt_size as u64);
            let send_capacity: socklen_t = 4 * 4096;
            current_task.write_memory(user_address, &send_capacity.to_ne_bytes()).unwrap();
            let user_buffer = UserBuffer { address: user_address, length: opt_size };
            server_socket
                .setsockopt(&current_task, SOL_SOCKET, SO_SNDBUF, user_buffer.into())
                .unwrap();

            let opt_bytes =
                server_socket.getsockopt(&current_task, SOL_SOCKET, SO_SNDBUF, 0).unwrap();
            let retrieved_capacity = socklen_t::from_ne_bytes(opt_bytes.try_into().unwrap());
            // Setting SO_SNDBUF actually sets it to double the size
            assert_eq!(2 * send_capacity, retrieved_capacity);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_datagram_socket_disconnect_af_unspec() {
        spawn_kernel_and_run(async |current_task| {
            let socket1 = Socket::new(
                &current_task,
                SocketDomain::Unix,
                SocketType::Datagram,
                SocketProtocol::default(),
                /* kernel_private = */ false,
            )
            .expect("Failed to create socket 1.");
            socket1
                .bind(&current_task, SocketAddress::Unix(b"\0sock1".into()))
                .expect("Failed to bind socket 1.");

            let socket2 = Socket::new(
                &current_task,
                SocketDomain::Unix,
                SocketType::Datagram,
                SocketProtocol::default(),
                /* kernel_private = */ false,
            )
            .expect("Failed to create socket 2.");
            socket2
                .bind(&current_task, SocketAddress::Unix(b"\0sock2".into()))
                .expect("Failed to bind socket 2.");

            // Connect socket1 to socket2.
            socket1
                .ops
                .connect(&socket1, &current_task, SocketPeer::Handle(socket2.clone()))
                .expect("Failed to connect socket1 to socket2.");

            // Disconnect socket1 using AF_UNSPEC.
            socket1
                .ops
                .connect(&socket1, &current_task, SocketPeer::Address(SocketAddress::Unspecified))
                .expect("Failed to disconnect socket1 with AF_UNSPEC.");
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_bind_socket_retains_fs_node_until_close() {
        spawn_kernel_and_run(async |current_task| {
            let socket = Socket::new(
                &current_task,
                SocketDomain::Unix,
                SocketType::Stream,
                SocketProtocol::default(),
                /* kernel_private = */ false,
            )
            .expect("Failed to create socket.");

            let root = current_task.fs().root();
            let bound_node = root
                .bind_socket(
                    &current_task,
                    "test_sock".into(),
                    socket.clone(),
                    SocketAddress::Unix("test_sock".into()),
                    mode!(IFSOCK, 0o777),
                )
                .expect("Failed to bind socket to node.");
            let weak_fs_node = Arc::downgrade(&bound_node.entry.node);
            std::mem::drop(bound_node);

            // Unlink the directory entry so that `TmpFs`'s permanent `DirEntryCache` drops its
            // reference to the `DirEntry` and `FsNode`, simulating `DirEntryCache::Lru` eviction on
            // cached filesystems (such as `RemoteFs`).
            root.unlink(
                &current_task,
                "test_sock".into(),
                UnlinkKind::NonDirectory,
                DirectoryMode::AllowAny,
            )
            .expect("Failed to unlink socket.");
            current_task.trigger_delayed_releaser();

            // While the socket remains open, it must retain the bound `FsNode` and its
            // `bound_socket` association.
            let fs_node =
                weak_fs_node.upgrade().expect("FsNode should remain alive while socket is open");
            assert!(fs_node.bound_socket().is_some());
            std::mem::drop(fs_node);

            // Closing the socket must release its reference to the `FsNode`, breaking the
            // `Socket` <-> `FsNode` reference cycle so both can be freed.
            let weak_socket = Arc::downgrade(&socket);
            socket.close(&current_task);
            std::mem::drop(socket);
            current_task.trigger_delayed_releaser();

            assert!(weak_fs_node.upgrade().is_none());
            assert!(weak_socket.upgrade().is_none());
        })
        .await;
    }
}
