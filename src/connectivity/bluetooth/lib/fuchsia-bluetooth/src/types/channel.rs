// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl::endpoints::{ClientEnd, Proxy};
use fidl_fuchsia_bluetooth as fidl_bt;
use fidl_fuchsia_bluetooth_bredr as bredr;
use fuchsia_sync::Mutex;
use futures::sink::Sink;
use futures::stream::{FusedStream, Stream};
use futures::{Future, StreamExt};
use log::warn;
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use crate::error::Error;

pub mod socket;

// TODO(b/414410187): Add mod for FIDL client/server.

use socket::SocketConnection;

/// The Channel mode in use for a L2CAP channel.
#[derive(PartialEq, Debug, Clone)]
pub enum ChannelMode {
    Basic,
    EnhancedRetransmissionMode,
    LeCreditBasedFlowControl,
    EnhancedCreditBasedFlowControl,
}

impl fmt::Display for ChannelMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChannelMode::Basic => write!(f, "Basic"),
            ChannelMode::EnhancedRetransmissionMode => write!(f, "ERTM"),
            ChannelMode::LeCreditBasedFlowControl => write!(f, "LE_Credit"),
            ChannelMode::EnhancedCreditBasedFlowControl => write!(f, "Credit"),
        }
    }
}

pub enum A2dpDirection {
    Normal,
    Source,
    Sink,
}

impl From<A2dpDirection> for bredr::A2dpDirectionPriority {
    fn from(pri: A2dpDirection) -> Self {
        match pri {
            A2dpDirection::Normal => bredr::A2dpDirectionPriority::Normal,
            A2dpDirection::Source => bredr::A2dpDirectionPriority::Source,
            A2dpDirection::Sink => bredr::A2dpDirectionPriority::Sink,
        }
    }
}

impl TryFrom<fidl_bt::ChannelMode> for ChannelMode {
    type Error = Error;
    fn try_from(fidl: fidl_bt::ChannelMode) -> Result<Self, Error> {
        match fidl {
            fidl_bt::ChannelMode::Basic => Ok(ChannelMode::Basic),
            fidl_bt::ChannelMode::EnhancedRetransmission => {
                Ok(ChannelMode::EnhancedRetransmissionMode)
            }
            fidl_bt::ChannelMode::LeCreditBasedFlowControl => {
                Ok(ChannelMode::LeCreditBasedFlowControl)
            }
            fidl_bt::ChannelMode::EnhancedCreditBasedFlowControl => {
                Ok(ChannelMode::EnhancedCreditBasedFlowControl)
            }
            x => Err(Error::FailedConversion(format!("Unsupported channel mode type: {x:?}"))),
        }
    }
}

impl From<ChannelMode> for fidl_bt::ChannelMode {
    fn from(x: ChannelMode) -> Self {
        match x {
            ChannelMode::Basic => fidl_bt::ChannelMode::Basic,
            ChannelMode::EnhancedRetransmissionMode => fidl_bt::ChannelMode::EnhancedRetransmission,
            ChannelMode::LeCreditBasedFlowControl => fidl_bt::ChannelMode::LeCreditBasedFlowControl,
            ChannelMode::EnhancedCreditBasedFlowControl => {
                fidl_bt::ChannelMode::EnhancedCreditBasedFlowControl
            }
        }
    }
}

pub enum ConnectionBackendType {
    Socket,
    FidlClient,
    FidlServer,
}

/// A trait representing a Bluetooth data connection.
/// Concrete implementations handle the specific transport mechanism (e.g., socket or FIDL protocol)
/// while fulfilling the `Sink` and `Stream` contracts for data transfer.
pub trait Connection:
    Stream<Item = Result<Vec<u8>, zx::Status>>
    + Sink<Vec<u8>, Error = zx::Status>
    + Send
    + Sync
    + std::fmt::Debug
    + Unpin
{
    /// Returns a future that resolves when the connection is closed.
    fn closed<'a>(&'a self) -> Pin<Box<dyn Future<Output = Result<(), zx::Status>> + 'a>>;

    /// Returns the type of the connection backend.
    fn connection_type(&self) -> ConnectionBackendType;

    /// Writes data to the connection. This is a non-blocking fast path.
    /// Returns `SHOULD_WAIT` if the buffer is full.
    fn write(&self, bytes: &[u8]) -> Result<usize, zx::Status>;

    /// Returns true if the connection is currently closed.
    fn is_closed(&self) -> bool;

    /// Consumes the connection and returns a partially filled FIDL channel
    /// containing the transport (e.g., socket handle) if applicable.
    fn into_fidl_channel(self: Box<Self>) -> Result<bredr::Channel, zx::Status>;
}

/// A wrapper for Bluetooth channel. Profiles interact with this struct.
#[derive(Debug)]
pub struct Channel {
    pub(crate) connection: Box<dyn Connection>,
    mode: ChannelMode,
    max_tx_size: usize,
    flush_timeout: Arc<Mutex<Option<zx::MonotonicDuration>>>,
    audio_direction_ext: Option<bredr::AudioDirectionExtProxy>,
    l2cap_parameters_ext: Option<bredr::L2capParametersExtProxy>,
    audio_offload_ext: Option<bredr::AudioOffloadExtProxy>,
    terminated: bool,
}

impl Channel {
    pub const DEFAULT_MAX_TX: usize = 672;

    pub fn from_socket(socket: zx::Socket, max_tx_size: usize) -> Result<Self, zx::Status> {
        let connection = Box::new(SocketConnection::new(socket));
        Ok(Channel {
            connection,
            mode: ChannelMode::Basic,
            max_tx_size,
            flush_timeout: Arc::new(Mutex::new(None)),
            audio_direction_ext: None,
            l2cap_parameters_ext: None,
            audio_offload_ext: None,
            terminated: false,
        })
    }

    pub fn from_socket_infallible(socket: zx::Socket, max_tx_size: usize) -> Self {
        Self::from_socket(socket, max_tx_size).unwrap()
    }

    pub fn create() -> (Self, Self) {
        Self::create_with_max_tx(Self::DEFAULT_MAX_TX)
    }

    pub fn create_with_max_tx(max_tx_size: usize) -> (Self, Self) {
        let (remote, local) = zx::Socket::create_datagram();
        (
            Channel::from_socket(remote, max_tx_size).unwrap(),
            Channel::from_socket(local, max_tx_size).unwrap(),
        )
    }

    pub fn max_tx_size(&self) -> usize {
        self.max_tx_size
    }

    pub fn channel_mode(&self) -> &ChannelMode {
        &self.mode
    }

    pub fn flush_timeout(&self) -> Option<zx::MonotonicDuration> {
        self.flush_timeout.lock().clone()
    }

    pub fn closed<'a>(&'a self) -> impl Future<Output = Result<(), zx::Status>> + 'a {
        self.connection.closed()
    }

    pub fn is_closed(&self) -> bool {
        self.connection.is_closed()
    }

    pub fn write(&self, bytes: &[u8]) -> Result<usize, zx::Status> {
        self.connection.write(bytes)
    }

    pub fn set_audio_priority(
        &self,
        dir: A2dpDirection,
    ) -> impl Future<Output = Result<(), Error>> + use<> {
        let proxy = self.audio_direction_ext.clone();
        async move {
            match proxy {
                None => return Err(Error::profile("audio priority not supported")),
                Some(proxy) => proxy
                    .set_priority(dir.into())
                    .await?
                    .map_err(|e| Error::profile(format!("setting priority failed: {e:?}"))),
            }
        }
    }

    pub fn set_flush_timeout(
        &self,
        duration: Option<zx::MonotonicDuration>,
    ) -> impl Future<Output = Result<Option<zx::MonotonicDuration>, Error>> + use<> {
        let flush_timeout = self.flush_timeout.clone();
        let current = self.flush_timeout.lock().clone();
        let proxy = self.l2cap_parameters_ext.clone();
        async move {
            match (current, duration) {
                (None, None) => return Ok(None),
                (Some(old), Some(new)) if (old - new).into_millis().abs() < 2 => {
                    return Ok(current);
                }
                _ => {}
            };
            let proxy =
                proxy.ok_or_else(|| Error::profile("l2cap parameter changing not supported"))?;
            let parameters = fidl_bt::ChannelParameters {
                flush_timeout: duration.clone().map(zx::MonotonicDuration::into_nanos),
                ..Default::default()
            };
            let new_params = proxy.request_parameters(&parameters).await?;
            let new_timeout = new_params.flush_timeout.map(zx::MonotonicDuration::from_nanos);
            *(flush_timeout.lock()) = new_timeout.clone();
            Ok(new_timeout)
        }
    }

    pub fn audio_offload(&self) -> Option<bredr::AudioOffloadExtProxy> {
        self.audio_offload_ext.clone()
    }
}

impl Stream for Channel {
    type Item = Result<Vec<u8>, zx::Status>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.terminated {
            warn!("Stream was polled after termination");
            return Poll::Ready(None);
        }
        let res = this.connection.poll_next_unpin(cx);
        if let Poll::Ready(None) = res {
            this.terminated = true;
        }
        res
    }
}

impl FusedStream for Channel {
    fn is_terminated(&self) -> bool {
        self.terminated
    }
}

impl Sink<Vec<u8>> for Channel {
    type Error = zx::Status;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut *self.get_mut().connection).poll_ready(cx)
    }

    fn start_send(self: Pin<&mut Self>, item: Vec<u8>) -> Result<(), Self::Error> {
        Pin::new(&mut *self.get_mut().connection).start_send(item)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut *self.get_mut().connection).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut *self.get_mut().connection).poll_close(cx)
    }
}

impl TryFrom<Channel> for bredr::Channel {
    type Error = Error;

    fn try_from(channel: Channel) -> Result<Self, Self::Error> {
        let mut fidl_channel = channel
            .connection
            .into_fidl_channel()
            .map_err(|e| Error::profile(format!("Failed to convert to FIDL channel: {e:?}")))?;

        fidl_channel.channel_mode = Some(channel.mode.into());
        fidl_channel.max_tx_sdu_size = Some(channel.max_tx_size as u16);

        let flush_timeout = channel.flush_timeout.lock().clone();
        fidl_channel.flush_timeout = flush_timeout.map(zx::MonotonicDuration::into_nanos);

        fidl_channel.ext_direction = channel
            .audio_direction_ext
            .map(|proxy| {
                let chan = proxy.into_channel()?;
                Ok(ClientEnd::new(chan.into()))
            })
            .transpose()
            .map_err(|_: bredr::AudioDirectionExtProxy| {
                Error::profile("AudioDirection proxy in use")
            })?;

        fidl_channel.ext_l2cap = channel
            .l2cap_parameters_ext
            .map(|proxy| {
                let chan = proxy.into_channel()?;
                Ok(ClientEnd::new(chan.into()))
            })
            .transpose()
            .map_err(|_: bredr::L2capParametersExtProxy| {
                Error::profile("l2cap parameters proxy in use")
            })?;

        fidl_channel.ext_audio_offload = channel
            .audio_offload_ext
            .map(|proxy| {
                let chan = proxy.into_channel()?;
                Ok(ClientEnd::new(chan.into()))
            })
            .transpose()
            .map_err(|_: bredr::AudioOffloadExtProxy| {
                Error::profile("audio offload proxy in use")
            })?;

        Ok(fidl_channel)
    }
}

impl TryFrom<fidl_fuchsia_bluetooth_bredr::Channel> for Channel {
    type Error = zx::Status;

    fn try_from(fidl: bredr::Channel) -> Result<Self, Self::Error> {
        let mode = match fidl.channel_mode.unwrap_or(fidl_bt::ChannelMode::Basic).try_into() {
            Err(e) => {
                warn!("Unsupported channel mode type: {e:?}");
                return Err(zx::Status::INTERNAL);
            }
            Ok(c) => c,
        };

        let socket = fidl.socket.ok_or(zx::Status::INVALID_ARGS)?;
        let connection = Box::new(SocketConnection::new(socket));

        Ok(Self {
            connection,
            mode,
            max_tx_size: fidl.max_tx_sdu_size.ok_or(zx::Status::INVALID_ARGS)? as usize,
            flush_timeout: Arc::new(Mutex::new(
                fidl.flush_timeout.map(zx::MonotonicDuration::from_nanos),
            )),
            audio_direction_ext: fidl.ext_direction.map(|e| e.into_proxy()),
            l2cap_parameters_ext: fidl.ext_l2cap.map(|e| e.into_proxy()),
            audio_offload_ext: fidl.ext_audio_offload.map(|c| c.into_proxy()),
            terminated: false,
        })
    }
}
