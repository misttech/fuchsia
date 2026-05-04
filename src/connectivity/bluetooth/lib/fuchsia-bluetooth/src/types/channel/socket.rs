// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_bluetooth_bredr as bredr;
use fuchsia_async as fasync;
use futures::sink::Sink;
use futures::stream::Stream;
use futures::{Future, TryFutureExt, ready};
use log::error;
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};
use zx;

use super::{Connection, ConnectionBackendType};

/// A socket-based implementation of the Bluetooth channel transport.
#[derive(Debug)]
pub struct SocketConnection {
    socket: fasync::Socket,
    send_buffer: VecDeque<Vec<u8>>,
}

impl SocketConnection {
    const MAX_QUEUED_PACKETS: usize = 32;

    pub fn new(socket: zx::Socket) -> Self {
        Self {
            socket: fasync::Socket::from_socket(socket),
            send_buffer: VecDeque::with_capacity(Self::MAX_QUEUED_PACKETS),
        }
    }

    pub fn into_zx_socket(self) -> zx::Socket {
        self.socket.into_zx_socket()
    }
}

impl Connection for SocketConnection {
    fn closed<'a>(&'a self) -> Pin<Box<dyn Future<Output = Result<(), zx::Status>> + 'a>> {
        let close_signals = zx::Signals::SOCKET_PEER_CLOSED;
        let close_wait = fasync::OnSignals::new(&self.socket, close_signals);
        Box::pin(close_wait.map_ok(|_o| ()))
    }

    fn connection_type(&self) -> ConnectionBackendType {
        ConnectionBackendType::Socket
    }

    fn write(&self, bytes: &[u8]) -> Result<usize, zx::Status> {
        self.socket.as_ref().write(bytes)
    }

    fn is_closed(&self) -> bool {
        self.socket.is_closed()
    }

    fn into_fidl_channel(self: Box<Self>) -> Result<bredr::Channel, zx::Status> {
        let socket = self.into_zx_socket();
        Ok(bredr::Channel { socket: Some(socket), ..Default::default() })
    }
}

impl Stream for SocketConnection {
    type Item = Result<Vec<u8>, zx::Status>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut res = Vec::<u8>::new();
        loop {
            break match self.socket.poll_datagram(cx, &mut res) {
                Poll::Ready(Ok(0)) => continue,
                Poll::Ready(Ok(_size)) => Poll::Ready(Some(Ok(res))),
                Poll::Ready(Err(zx::Status::PEER_CLOSED)) => Poll::Ready(None),
                Poll::Ready(Err(e)) => Poll::Ready(Some(Err(e))),
                Poll::Pending => Poll::Pending,
            };
        }
    }
}

impl Sink<Vec<u8>> for SocketConnection {
    type Error = zx::Status;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let _ = Sink::poll_flush(self.as_mut(), cx)?;

        if self.send_buffer.len() >= SocketConnection::MAX_QUEUED_PACKETS {
            return Poll::Pending;
        }
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: Vec<u8>) -> Result<(), Self::Error> {
        self.get_mut().send_buffer.push_back(item);
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let this = self.get_mut();
        use futures::io::AsyncWrite;
        while let Some(item) = this.send_buffer.front() {
            let res = Pin::new(&mut this.socket).poll_write(cx, item).map_err(zx::Status::from);
            match res {
                Poll::Ready(Ok(size)) => {
                    if size == item.len() {
                        let _ = this.send_buffer.pop_front();
                    } else {
                        error!(
                            "Partial write in SocketConnection::Sink::poll_flush: wrote {} bytes of {} byte packet.",
                            size,
                            item.len()
                        );
                        let item = this.send_buffer.front_mut().unwrap();
                        *item = item.split_off(size);
                    }
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
        Pin::new(&mut this.socket).poll_flush(cx).map_err(zx::Status::from)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        ready!(Sink::poll_flush(self.as_mut(), cx))?;
        let this = self.get_mut();
        use futures::io::AsyncWrite as _;
        Pin::new(&mut this.socket).poll_close(cx).map_err(zx::Status::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{A2dpDirection, Channel, ChannelMode};
    use fidl::endpoints::create_request_stream;
    use fidl_fuchsia_bluetooth as fidl_bt;
    use fidl_fuchsia_bluetooth_bredr as bredr;
    use futures::stream::FusedStream;
    use futures::{SinkExt, StreamExt};
    use std::pin::pin;

    #[test]
    fn channel_sync_write() {
        let mut exec = fasync::TestExecutor::new();
        let (mut recv, send) = Channel::create();

        let heart: &[u8] = &[0xF0, 0x9F, 0x92, 0x96];
        let size = send.write(heart).expect("write to succeed");
        assert_eq!(size, heart.len());

        let mut recv_fut = recv.next();
        match exec.run_until_stalled(&mut recv_fut) {
            Poll::Ready(Some(Ok(bytes))) => {
                assert_eq!(heart, &bytes);
            }
            x => panic!("Expected Some(Ok(bytes)) from the stream, got {x:?}"),
        };
    }

    #[test]
    fn channel_from_fidl() {
        let _exec = fasync::TestExecutor::new();
        let empty = bredr::Channel::default();
        assert!(Channel::try_from(empty).is_err());

        let (remote, _local) = zx::Socket::create_datagram();

        let valid_fidl_channel = bredr::Channel {
            socket: Some(remote),
            channel_mode: Some(fidl_bt::ChannelMode::Basic),
            max_tx_sdu_size: Some(1004),
            ..Default::default()
        };

        let chan = Channel::try_from(valid_fidl_channel).expect("okay channel to be converted");

        assert_eq!(1004, chan.max_tx_size());
        assert_eq!(&ChannelMode::Basic, chan.channel_mode());
    }

    #[test]
    fn channel_closed() {
        let mut exec = fasync::TestExecutor::new();

        let (recv, send) = Channel::create();

        let closed_fut = recv.closed();
        let mut closed_fut = pin!(closed_fut);

        assert!(exec.run_until_stalled(&mut closed_fut).is_pending());
        assert!(!recv.is_closed());

        drop(send);

        assert!(exec.run_until_stalled(&mut closed_fut).is_ready());
        assert!(recv.is_closed());
    }

    #[test]
    fn direction_ext() {
        let mut exec = fasync::TestExecutor::new();

        let (remote, _local) = zx::Socket::create_datagram();
        let fidl_channel_no_ext = bredr::Channel {
            socket: Some(remote),
            channel_mode: Some(fidl_bt::ChannelMode::Basic),
            max_tx_sdu_size: Some(1004),
            ..Default::default()
        };
        let channel = Channel::try_from(fidl_channel_no_ext).unwrap();

        assert!(
            exec.run_singlethreaded(channel.set_audio_priority(A2dpDirection::Normal)).is_err()
        );
        assert!(exec.run_singlethreaded(channel.set_audio_priority(A2dpDirection::Sink)).is_err());

        let (remote, _local) = zx::Socket::create_datagram();
        let (client_end, mut direction_request_stream) =
            create_request_stream::<bredr::AudioDirectionExtMarker>();
        let fidl_channel_with_ext = bredr::Channel {
            socket: Some(remote),
            channel_mode: Some(fidl_bt::ChannelMode::Basic),
            max_tx_sdu_size: Some(1004),
            ext_direction: Some(client_end),
            ..Default::default()
        };

        let channel = Channel::try_from(fidl_channel_with_ext).unwrap();

        let audio_direction_fut = channel.set_audio_priority(A2dpDirection::Normal);
        let mut audio_direction_fut = pin!(audio_direction_fut);

        assert!(exec.run_until_stalled(&mut audio_direction_fut).is_pending());

        match exec.run_until_stalled(&mut direction_request_stream.next()) {
            Poll::Ready(Some(Ok(bredr::AudioDirectionExtRequest::SetPriority {
                priority,
                responder,
            }))) => {
                assert_eq!(bredr::A2dpDirectionPriority::Normal, priority);
                responder.send(Ok(())).expect("response to send cleanly");
            }
            x => panic!("Expected a item to be ready on the request stream, got {:?}", x),
        };

        match exec.run_until_stalled(&mut audio_direction_fut) {
            Poll::Ready(Ok(())) => {}
            _x => panic!("Expected ok result from audio direction response"),
        };

        let audio_direction_fut = channel.set_audio_priority(A2dpDirection::Sink);
        let mut audio_direction_fut = pin!(audio_direction_fut);

        assert!(exec.run_until_stalled(&mut audio_direction_fut).is_pending());

        match exec.run_until_stalled(&mut direction_request_stream.next()) {
            Poll::Ready(Some(Ok(bredr::AudioDirectionExtRequest::SetPriority {
                priority,
                responder,
            }))) => {
                assert_eq!(bredr::A2dpDirectionPriority::Sink, priority);
                responder
                    .send(Err(fidl_fuchsia_bluetooth::ErrorCode::Failed))
                    .expect("response to send cleanly");
            }
            x => panic!("Expected a item to be ready on the request stream, got {:?}", x),
        };

        match exec.run_until_stalled(&mut audio_direction_fut) {
            Poll::Ready(Err(_)) => {}
            _x => panic!("Expected error result from audio direction response"),
        };
    }

    #[test]
    fn flush_timeout() {
        let mut exec = fasync::TestExecutor::new();

        let (remote, _local) = zx::Socket::create_datagram();
        let fidl_channel_no_ext = bredr::Channel {
            socket: Some(remote),
            channel_mode: Some(fidl_bt::ChannelMode::Basic),
            max_tx_sdu_size: Some(1004),
            flush_timeout: Some(50_000_000), // 50 milliseconds
            ..Default::default()
        };
        let channel = Channel::try_from(fidl_channel_no_ext).unwrap();

        assert_eq!(Some(zx::MonotonicDuration::from_millis(50)), channel.flush_timeout());

        // Within 2 milliseconds, doesn't change.
        let res = exec.run_singlethreaded(
            channel.set_flush_timeout(Some(zx::MonotonicDuration::from_millis(49))),
        );
        assert_eq!(Some(zx::MonotonicDuration::from_millis(50)), res.expect("shouldn't error"));
        let res = exec.run_singlethreaded(
            channel.set_flush_timeout(Some(zx::MonotonicDuration::from_millis(51))),
        );
        assert_eq!(Some(zx::MonotonicDuration::from_millis(50)), res.expect("shouldn't error"));

        assert!(
            exec.run_singlethreaded(
                channel.set_flush_timeout(Some(zx::MonotonicDuration::from_millis(200)))
            )
            .is_err()
        );
        assert!(exec.run_singlethreaded(channel.set_flush_timeout(None)).is_err());

        let (remote, _local) = zx::Socket::create_datagram();
        let (client_end, mut l2cap_request_stream) =
            create_request_stream::<bredr::L2capParametersExtMarker>();
        let fidl_channel_with_ext = bredr::Channel {
            socket: Some(remote),
            channel_mode: Some(fidl_bt::ChannelMode::Basic),
            max_tx_sdu_size: Some(1004),
            flush_timeout: None,
            ext_l2cap: Some(client_end),
            ..Default::default()
        };

        let channel = Channel::try_from(fidl_channel_with_ext).unwrap();

        {
            let flush_timeout_fut = channel.set_flush_timeout(None);
            let mut flush_timeout_fut = pin!(flush_timeout_fut);

            // Requesting no change returns right away with no change.
            match exec.run_until_stalled(&mut flush_timeout_fut) {
                Poll::Ready(Ok(None)) => {}
                x => panic!("Expected no flush timeout to not stall, got {:?}", x),
            }
        }

        let req_duration = zx::MonotonicDuration::from_millis(42);

        {
            let flush_timeout_fut = channel.set_flush_timeout(Some(req_duration));
            let mut flush_timeout_fut = pin!(flush_timeout_fut);

            assert!(exec.run_until_stalled(&mut flush_timeout_fut).is_pending());

            match exec.run_until_stalled(&mut l2cap_request_stream.next()) {
                Poll::Ready(Some(Ok(bredr::L2capParametersExtRequest::RequestParameters {
                    request,
                    responder,
                }))) => {
                    assert_eq!(Some(req_duration.into_nanos()), request.flush_timeout);
                    // Send a different response
                    let params = fidl_bt::ChannelParameters {
                        flush_timeout: Some(50_000_000), // 50ms
                        ..Default::default()
                    };
                    responder.send(&params).expect("response to send cleanly");
                }
                x => panic!("Expected a item to be ready on the request stream, got {:?}", x),
            };

            match exec.run_until_stalled(&mut flush_timeout_fut) {
                Poll::Ready(Ok(Some(duration))) => {
                    assert_eq!(zx::MonotonicDuration::from_millis(50), duration)
                }
                x => panic!("Expected ready result from params response, got {:?}", x),
            };
        }

        // Channel should have recorded the new flush timeout.
        assert_eq!(Some(zx::MonotonicDuration::from_millis(50)), channel.flush_timeout());
    }

    #[test]
    fn audio_offload() {
        let _exec = fasync::TestExecutor::new();

        let (remote, _local) = zx::Socket::create_datagram();
        let fidl_channel_no_ext = bredr::Channel {
            socket: Some(remote),
            channel_mode: Some(fidl_bt::ChannelMode::Basic),
            max_tx_sdu_size: Some(1004),
            ..Default::default()
        };
        let channel = Channel::try_from(fidl_channel_no_ext).unwrap();

        assert!(channel.audio_offload().is_none());

        let (remote, _local) = zx::Socket::create_datagram();
        let (client_end, mut _audio_offload_ext_req_stream) =
            create_request_stream::<bredr::AudioOffloadExtMarker>();
        let fidl_channel_with_ext = bredr::Channel {
            socket: Some(remote),
            channel_mode: Some(fidl_bt::ChannelMode::Basic),
            max_tx_sdu_size: Some(1004),
            ext_audio_offload: Some(client_end),
            ..Default::default()
        };

        let channel = Channel::try_from(fidl_channel_with_ext).unwrap();

        let offload_ext = channel.audio_offload();
        assert!(offload_ext.is_some());
        // We can get the audio offload multiple times without dropping
        assert!(channel.audio_offload().is_some());
        // And with dropping
        drop(offload_ext);
        assert!(channel.audio_offload().is_some());
    }

    #[test]
    fn channel_sink() {
        let mut exec = fasync::TestExecutor::new();
        let (mut recv, mut send) = Channel::create();

        let data = vec![0x01, 0x02, 0x03, 0x04];
        let mut send_fut = send.send(data.clone());

        // The send should complete immediately as the socket has space.
        match exec.run_until_stalled(&mut send_fut) {
            Poll::Ready(Ok(())) => {}
            x => panic!("Expected Ready(Ok(())), got {:?}", x),
        }

        let mut recv_fut = recv.next();
        match exec.run_until_stalled(&mut recv_fut) {
            Poll::Ready(Some(Ok(bytes))) => assert_eq!(data, bytes),
            x => panic!("Expected successful read, got {x:?}"),
        }
    }

    #[test]
    fn channel_stream() {
        let mut exec = fasync::TestExecutor::new();
        let (remote, local) = zx::Socket::create_datagram();
        let mut recv = Channel::from_socket(remote, Channel::DEFAULT_MAX_TX).unwrap();
        let send = local;

        let mut stream_fut = recv.next();

        assert!(exec.run_until_stalled(&mut stream_fut).is_pending());

        let heart: &[u8] = &[0xF0, 0x9F, 0x92, 0x96];
        let _ = send.write(heart).expect("should write successfully");

        match exec.run_until_stalled(&mut stream_fut) {
            Poll::Ready(Some(Ok(bytes))) => {
                assert_eq!(heart.to_vec(), bytes);
            }
            x => panic!("Expected Some(Ok(bytes)) from the stream, got {x:?}"),
        }

        // After the sender is dropped, the stream should terminate.
        drop(send);

        let mut stream_fut = recv.next();
        match exec.run_until_stalled(&mut stream_fut) {
            Poll::Ready(None) => {}
            x => panic!("Expected None from the stream after close, got {x:?}"),
        }

        // It should continue to report terminated.
        assert!(recv.is_terminated());
    }
}
