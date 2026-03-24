// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fdomain_client::fidl::{DiscoverableProtocolMarker, ProtocolMarker, Proxy};
use fdomain_client::{AsHandleRef, Socket as AsyncSocket, Socket};
use fdomain_fuchsia_sys2::OpenDirType;
use futures::{AsyncRead, AsyncWrite, Stream};
use std::fmt;
use std::fmt::Debug;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use {
    fdomain_fuchsia_developer_remotecontrol as frcs, fdomain_fuchsia_net as fnet,
    fdomain_fuchsia_posix as fposix, fdomain_fuchsia_posix_socket as fsock,
    fuchsia_async as fasync,
};

use crate::{Error, Result};

/// A connected TCP socket opened on the target that can be controlled from the
/// host.
pub struct TargetTcpStream {
    socket: AsyncSocket,
    addr: SocketAddr,
    peer: SocketAddr,
    fidl: fsock::StreamSocketProxy,
}

impl Debug for TargetTcpStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TargetTcpStream")
            .field("addr", &self.addr)
            .field("peer", &self.peer)
            .finish_non_exhaustive()
    }
}

impl TargetTcpStream {
    /// Closes this connected socket.
    ///
    /// Dropping the stream has the same effect, but closing happens
    /// asynchronously.
    pub async fn close(self) -> Result<()> {
        // Map FDomain error to Status::PEER_CLOSED as a reasonable approximation for "IPC failed".
        self.fidl
            .close()
            .await
            .map_err(|_| Error::Close(fidl::Status::PEER_CLOSED))?
            .map_err(|s| Error::Close(fidl::Status::from_raw(s)))
    }

    /// Returns the local address of the connected TCP socket (from the target's
    /// perspective).
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Returns the peer address this TCP socket is connected to.
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer
    }
}

impl AsyncWrite for TargetTcpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.socket).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.socket).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.socket).poll_close(cx)
    }
}

impl AsyncRead for TargetTcpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.socket).poll_read(cx, buf)
    }
}

/// A listening TCP socket on the target that can be controlled from the host.
pub struct TargetTcpListener {
    socket: Socket,
    fidl: fsock::StreamSocketProxy,
    addr: SocketAddr,
}

impl TargetTcpListener {
    /// Returns the local address of this listener, on the target side.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Closes this listener.
    ///
    /// Dropping the listener has the same effect, but closing happens
    /// asynchronously.
    pub async fn close(self) -> Result<()> {
        self.fidl
            .close()
            .await
            .map_err(|_| Error::Close(fidl::Status::PEER_CLOSED))?
            .map_err(|s| Error::Close(fidl::Status::from_raw(s)))
    }

    /// Blocks until a new incoming connection is available on this listening
    /// socket, returning the connected socket.
    pub async fn accept(&self) -> Result<TargetTcpStream> {
        loop {
            let Self { fidl, socket, addr: listen_addr } = self;

            match fidl.accept(true).await.map_err(|_e| Error::Accept(fposix::Errno::Eio))? {
                Ok((addr, got_socket)) => {
                    let addr = addr.ok_or_else(|| Error::MissingField("accept address"))?;
                    let addr = to_std_addr(*addr);
                    let fidl = got_socket.into_proxy();
                    let socket =
                        fidl.describe().await.map_err(|_| Error::MissingField("describe"))?;
                    let socket = socket.socket.ok_or_else(|| Error::MissingField("describe"))?;
                    return Ok(TargetTcpStream {
                        // socket is fdomain_client::Socket.
                        // AsyncSocket::from_socket not needed if it IS AsyncSocket (or Socket).
                        // Original code used fasync::Socket::from_socket.
                        // FDomain socket IS the socket.
                        socket,
                        addr: *listen_addr,
                        peer: addr,
                        fidl,
                    });
                }
                // Fallback into waiting.
                Err(fposix::Errno::Eagain) => (),
                Err(error) => return Err(Error::Accept(error)),
            }

            let incoming_signal = fidl::Signals::from_bits(fsock::SIGNAL_STREAM_INCOMING).unwrap();
            let signals = fdomain_client::OnFDomainSignals::new(
                &socket.as_handle_ref(),
                incoming_signal | fidl::Signals::OBJECT_PEER_CLOSED,
            )
            .await
            .map_err(Error::WaitingSignal)?;
            if !signals.contains(incoming_signal) {
                return Err(Error::Hangup);
            }
            socket
                .as_handle_ref()
                .signal(incoming_signal, fidl::Signals::empty())
                .await
                .map_err(Error::ClearingSignal)?;
        }
    }

    /// Transforms this listener into a stream of incoming connections.
    pub fn into_stream(self) -> impl Stream<Item = Result<TargetTcpStream>> {
        futures::stream::try_unfold(self, |listener| async move {
            let incoming = listener.accept().await?;
            Ok(Some((incoming, listener)))
        })
    }
}

impl Debug for TargetTcpListener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TargetTcpListener").field("addr", &self.addr).finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct SocketProvider {
    socket_provider: fsock::ProviderProxy,
}

impl SocketProvider {
    /// The default backlog used when one is not provided.
    pub const DEFAULT_BACKLOG: u16 = 128;

    /// Creates a new [`SocketProvider`] with the given FIDL proxy.
    pub fn new(socket_provider: fsock::ProviderProxy) -> Self {
        Self { socket_provider }
    }

    /// Creates a new [`SocketProvider`] from a [`RemoteControlProxy`].
    /// Note: This uses FDomain-specific `RemoteControlProxy`.
    pub async fn new_with_rcs(
        connect_timeout: Duration,
        rcs_proxy: &frcs::RemoteControlProxy,
    ) -> Result<Self> {
        let socket_provider = connect_with_timeout::<fsock::ProviderMarker>(
            rcs_proxy,
            Some("core/network/netstack"),
            connect_timeout,
        )
        .await
        .map_err(Error::OpenProtocol)?;
        Ok(Self { socket_provider })
    }

    /// Creates a connected [`TargetTcpStream`] to `peer` on the target.
    pub async fn connect(&self, peer: SocketAddr) -> Result<TargetTcpStream> {
        let domain = match &peer {
            SocketAddr::V4(_) => fsock::Domain::Ipv4,
            SocketAddr::V6(_) => fsock::Domain::Ipv6,
        };

        let socket_fidl = self
            .socket_provider
            .stream_socket(domain, fsock::StreamSocketProtocol::Tcp)
            .await
            .map_err(|_e| Error::CreateSocket(fposix::Errno::Eio))?
            .map_err(Error::CreateSocket)?;
        let socket_fidl = socket_fidl.into_proxy();
        let socket = socket_fidl
            .describe()
            .await
            .map_err(|_| Error::MissingField("socket describe"))?
            .socket
            .ok_or_else(|| Error::MissingField("socket describe"))?;

        loop {
            match socket_fidl
                .connect(&to_fidl_sockaddr(peer))
                .await
                .map_err(|_| Error::Connect(fposix::Errno::Eio))?
            {
                Ok(()) => break,
                Err(fposix::Errno::Einprogress) => {}
                Err(e) => return Err(Error::Connect(e)),
            }

            let connected_signal =
                fidl::Signals::from_bits(fsock::SIGNAL_STREAM_CONNECTED).unwrap();
            let signals = fdomain_client::OnFDomainSignals::new(
                &socket.as_handle_ref(),
                connected_signal | fidl::Signals::OBJECT_PEER_CLOSED,
            )
            .await
            .map_err(Error::WaitingSignal)?;
            if !signals.contains(connected_signal) {
                return Err(Error::Hangup);
            }
            socket
                .as_handle_ref()
                .signal(connected_signal, fidl::Signals::empty())
                .await
                .map_err(Error::ClearingSignal)?;
        }

        let addr = socket_fidl
            .get_sock_name()
            .await
            .map_err(|_| Error::GetSockName(fposix::Errno::Eio))?
            .map_err(Error::GetSockName)?;
        let addr = to_std_addr(addr);

        Ok(TargetTcpStream { socket, addr, peer, fidl: socket_fidl })
    }

    /// Creates a [`TargetTcpListener`] on `listen_addr` on the target.
    ///
    /// If `conn_backlog` is `None`, [`PortForwarder::DEFAULT_BACKLOG`] is used.
    pub async fn listen(
        &self,
        listen_addr: SocketAddr,
        conn_backlog: Option<u16>,
    ) -> Result<TargetTcpListener> {
        let domain = match &listen_addr {
            SocketAddr::V4(_) => fsock::Domain::Ipv4,
            SocketAddr::V6(_) => fsock::Domain::Ipv6,
        };

        let listen_socket = self
            .socket_provider
            .stream_socket(domain, fsock::StreamSocketProtocol::Tcp)
            .await
            .map_err(|_| Error::CreateSocket(fposix::Errno::Eio))?
            .map_err(Error::CreateSocket)?;
        let listen_socket = listen_socket.into_proxy();
        listen_socket
            .bind(&to_fidl_sockaddr(listen_addr))
            .await
            .map_err(|_| Error::Bind(fposix::Errno::Eio))?
            .map_err(Error::Bind)?;

        listen_socket
            .listen(conn_backlog.unwrap_or(Self::DEFAULT_BACKLOG).try_into().unwrap_or(i16::MAX))
            .await
            .map_err(|_| Error::Listen(fposix::Errno::Eio))?
            .map_err(Error::Listen)?;

        let sockaddr = listen_socket
            .get_sock_name()
            .await
            .map_err(|_| Error::GetSockName(fposix::Errno::Eio))?
            .map_err(Error::GetSockName)?;
        let sockaddr = to_std_addr(sockaddr);

        let listen_socket_fidl_socket =
            listen_socket.describe().await.map_err(|_| Error::MissingField("socket describe"))?;
        let listen_socket_fidl_socket = listen_socket_fidl_socket
            .socket
            .ok_or_else(|| Error::MissingField("socket describe"))?;

        Ok(TargetTcpListener {
            socket: listen_socket_fidl_socket,
            fidl: listen_socket,
            addr: sockaddr,
        })
    }
}

pub const TOOLBOX_MONIKER: &str = "toolbox";
pub const LEGACY_TOOLBOX_MONIKER: &str = "core/toolbox";

/// Connects to a protocol available in the namespace of the `toolbox` component.
pub async fn connect_with_timeout<P>(
    rcs_proxy: &frcs::RemoteControlProxy,
    backup_moniker: Option<impl AsRef<str>>,
    dur: Duration,
) -> anyhow::Result<P::Proxy>
where
    P: DiscoverableProtocolMarker,
{
    let protocol_name = P::PROTOCOL_NAME;
    let start_time = std::time::Instant::now();
    let toolbox_res = open_with_timeout_at::<P>(
        dur,
        TOOLBOX_MONIKER,
        OpenDirType::NamespaceDir,
        &format!("svc/{protocol_name}"),
        rcs_proxy,
    )
    .await;

    // Fallback to legacy toolbox moniker if toolbox is not available.
    let toolbox_res = match toolbox_res {
        Ok(toolbox) => Ok(toolbox),
        Err(_) => {
            let toolbox_took = start_time.elapsed();
            let timeout = dur.saturating_sub(toolbox_took);
            open_with_timeout_at::<P>(
                timeout,
                LEGACY_TOOLBOX_MONIKER,
                OpenDirType::NamespaceDir,
                &format!("svc/{protocol_name}"),
                rcs_proxy,
            )
            .await
        }
    };

    let toolbox_took = start_time.elapsed();

    if let Ok(toolbox) = toolbox_res {
        return Ok(toolbox);
    }

    let Some(backup) = backup_moniker.as_ref().map(|s| s.as_ref()) else {
        return toolbox_res.map_err(|e| anyhow::anyhow!(e));
    };

    // try to connect to the moniker given instead, but don't double
    // up the timeout.
    let timeout = dur.saturating_sub(toolbox_took);
    let moniker_res =
        open_with_timeout::<P>(timeout, &backup, OpenDirType::ExposedDir, &rcs_proxy).await;

    moniker_res.map_err(|e| anyhow::anyhow!(e))
}

pub async fn open_with_timeout_at<T: ProtocolMarker>(
    dur: Duration,
    moniker: &str,
    capability_set: OpenDirType,
    capability_name: &str,
    rcs_proxy: &frcs::RemoteControlProxy,
) -> anyhow::Result<T::Proxy> {
    let connect_capability_fut = async move {
        // Try to connect via fuchsia.developer.remotecontrol/RemoteControl.ConnectCapability.
        let (proxy, server) = rcs_proxy.domain().create_proxy::<T>();
        rcs_proxy
            .connect_capability(moniker, capability_set, capability_name, server.into_channel())
            .await
            .map(|result| result.map(|_| proxy))
    };

    use futures::FutureExt;

    let fut = connect_capability_fut.fuse();
    let timer = fasync::Timer::new(dur).fuse();
    futures::pin_mut!(fut, timer);

    futures::select! {
        res = fut => res.map_err(|e| anyhow::anyhow!(e))?.map_err(|e| anyhow::anyhow!("{:?}", e)),
        _ = timer => Err(anyhow::anyhow!("Timed out connecting to capability: '{}' with moniker: '{}'", capability_name, moniker)),
    }
}

pub async fn open_with_timeout<P: DiscoverableProtocolMarker>(
    dur: Duration,
    moniker: &str,
    capability_set: OpenDirType,
    rcs_proxy: &frcs::RemoteControlProxy,
) -> anyhow::Result<P::Proxy> {
    open_with_timeout_at::<P>(dur, moniker, capability_set, P::PROTOCOL_NAME, rcs_proxy).await
}

fn to_fidl_sockaddr(addr: SocketAddr) -> fnet::SocketAddress {
    match addr {
        SocketAddr::V4(v4) => fnet::SocketAddress::Ipv4(fnet::Ipv4SocketAddress {
            address: fnet::Ipv4Address { addr: v4.ip().octets() },
            port: v4.port(),
        }),
        SocketAddr::V6(v6) => fnet::SocketAddress::Ipv6(fnet::Ipv6SocketAddress {
            address: fnet::Ipv6Address { addr: v6.ip().octets() },
            port: v6.port(),
            zone_index: v6.scope_id() as u64,
        }),
    }
}

fn to_std_addr(addr: fnet::SocketAddress) -> SocketAddr {
    match addr {
        fnet::SocketAddress::Ipv4(v4) => SocketAddr::V4(std::net::SocketAddrV4::new(
            std::net::Ipv4Addr::from(v4.address.addr),
            v4.port,
        )),
        fnet::SocketAddress::Ipv6(v6) => SocketAddr::V6(std::net::SocketAddrV6::new(
            std::net::Ipv6Addr::from(v6.address.addr),
            v6.port,
            0,
            v6.zone_index as u32,
        )),
    }
}
