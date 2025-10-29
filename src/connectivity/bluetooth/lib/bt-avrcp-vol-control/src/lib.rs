// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Error, format_err};
use core::fmt;
use core::pin::Pin;
use core::task::{Context, Poll};
use fidl::endpoints::Proxy;
use fidl_fuchsia_bluetooth_avrcp::{
    ControllerEvent, ControllerEventStream, ControllerProxy, Notifications,
};
use fuchsia_bluetooth::types::PeerId;
use futures::ready;
use futures::stream::{FusedStream, Stream, StreamExt};

/// Represents a connection to a remote peer that supports Absolute Volume control.
pub struct AbsoluteVolumeControl {
    id: PeerId,
    proxy: ControllerProxy,
    stream: ControllerEventStream,
    is_terminated: bool,
}

impl fmt::Debug for AbsoluteVolumeControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AbsoluteVolumeControl")
            .field("id", &self.id)
            .field("proxy", &self.proxy)
            .field("is_terminated", &self.is_terminated)
            .finish_non_exhaustive()
    }
}

impl AbsoluteVolumeControl {
    /// Creates a new `AbsoluteVolumeControl` for the given peer.
    ///
    /// This will fail if the notification filter can't be set.
    pub fn new(id: PeerId, proxy: ControllerProxy) -> Result<Self, Error> {
        proxy.set_notification_filter(Notifications::VOLUME, 0)?;
        let stream = proxy.take_event_stream();
        Ok(Self { id, proxy, stream, is_terminated: false })
    }

    /// Sets the absolute volume on the remote peer.
    ///
    /// `volume_percentage` is a value between 0 and 100.
    ///
    /// Returns the volume that was set on the peer.
    pub async fn set_absolute_volume(&self, volume_percentage: u8) -> Result<u8, Error> {
        let avrcp_volume = (volume_percentage as u16 * 127 / 100) as u8;
        self.proxy.set_absolute_volume(avrcp_volume).await?.map_err(|e| format_err!("{e:?}"))
    }
}

impl Stream for AbsoluteVolumeControl {
    type Item = Result<u8, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.is_terminated {
            return Poll::Ready(None);
        }

        // The stream is terminated if the proxy channel is closed. This is a graceful termination.
        if self.proxy.is_closed() {
            self.is_terminated = true;
            return Poll::Ready(None);
        }

        let result = ready!(self.stream.poll_next_unpin(cx));
        match result {
            Some(Ok(ControllerEvent::OnNotification { notification, .. })) => {
                // Fire and forget. If this fails, `is_closed()` will be true on the next poll.
                let _ = self.proxy.notify_notification_handled();

                if let Some(volume) = notification.volume {
                    Poll::Ready(Some(Ok(volume)))
                } else {
                    // This is not a volume notification, but the stream is still open.
                    // Poll again on the next turn.
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
            // The underlying stream terminated gracefully. This means this stream is also gracefully finished.
            None => {
                self.is_terminated = true;
                Poll::Ready(None)
            }
            // An unexpected FIDL error occurred that wasn't a channel closure.
            Some(Err(e)) => {
                self.is_terminated = true;
                Poll::Ready(Some(Err(e.into())))
            }
        }
    }
}

impl FusedStream for AbsoluteVolumeControl {
    fn is_terminated(&self) -> bool {
        self.is_terminated
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use async_utils::PollExt;
    use fidl::endpoints::{RequestStream, create_proxy_and_stream};
    use fidl_fuchsia_bluetooth_avrcp::{ControllerMarker, ControllerRequest};
    use fuchsia_async as fasync;
    use futures::{StreamExt, pin_mut};

    #[fuchsia::test]
    fn test_new_success() {
        let mut exec = fasync::TestExecutor::new();
        let (proxy, mut stream) = create_proxy_and_stream::<ControllerMarker>();
        let id = PeerId(1);

        let avc = AbsoluteVolumeControl::new(id, proxy);
        assert!(avc.is_ok());

        // The server should receive a `SetNotificationFilter` request.
        let mut request_fut = stream.next();
        let request = exec.run_until_stalled(&mut request_fut).expect("stream item");
        match request {
            Some(Ok(ControllerRequest::SetNotificationFilter { notifications, .. })) => {
                assert_eq!(notifications, Notifications::VOLUME);
            }
            r => panic!("Expected SetNotificationFilter request, got {:?}", r),
        }
    }

    #[fuchsia::test]
    fn test_set_absolute_volume() {
        let mut exec = fasync::TestExecutor::new();
        let (proxy, mut stream) = create_proxy_and_stream::<ControllerMarker>();
        let id = PeerId(1);
        let avc = AbsoluteVolumeControl::new(id, proxy).expect("AVC creation failed");

        // Handle the `SetNotificationFilter` request from `new()`.
        let mut request_fut = stream.next();
        let request = exec.run_until_stalled(&mut request_fut).expect("stream item").expect("some");
        assert_matches!(request, Ok(ControllerRequest::SetNotificationFilter { .. }));

        let volume_to_set = 50;
        let expected_avrcp_volume = 63; // 50 * 127 / 100
        let expected_response_volume = 64;

        let set_volume_fut = avc.set_absolute_volume(volume_to_set);
        pin_mut!(set_volume_fut);

        exec.run_until_stalled(&mut set_volume_fut).expect_pending("should be pending");

        // The server should receive a `SetAbsoluteVolume` request.
        let mut request_fut = stream.next();
        let request = exec.run_until_stalled(&mut request_fut).expect("stream item");
        match request {
            Some(Ok(ControllerRequest::SetAbsoluteVolume { requested_volume, responder })) => {
                assert_eq!(requested_volume, expected_avrcp_volume);
                responder.send(Ok(expected_response_volume)).unwrap();
            }
            r => panic!("Expected SetAbsoluteVolume request, got {:?}", r),
        }

        // The `set_absolute_volume` future should now complete with the response volume.
        let set_volume_response =
            exec.run_until_stalled(&mut set_volume_fut).expect("stream item").expect("some");
        assert_eq!(set_volume_response, expected_response_volume);
    }

    #[fuchsia::test]
    fn test_volume_stream() {
        let mut exec = fasync::TestExecutor::new();
        let (proxy, mut stream) = create_proxy_and_stream::<ControllerMarker>();
        let id = PeerId(1);
        let avc = AbsoluteVolumeControl::new(id, proxy).expect("AVC creation failed");
        pin_mut!(avc);

        // Handle the `SetNotificationFilter` request from `new()`.
        let mut request_fut = stream.next();
        let request = exec.run_until_stalled(&mut request_fut).expect("stream item").expect("some");
        assert_matches!(request, Ok(ControllerRequest::SetNotificationFilter { .. }));

        // No volume change yet.
        let vol_change_fut = avc.next();
        pin_mut!(vol_change_fut);
        exec.run_until_stalled(&mut vol_change_fut).expect_pending("pending");

        // Mock the server sending a volume notification.
        let event =
            fidl_fuchsia_bluetooth_avrcp::Notification { volume: Some(100), ..Default::default() };
        stream.control_handle().send_on_notification(1000, &event).unwrap();

        let new_vol = exec
            .run_until_stalled(&mut vol_change_fut)
            .expect("stream item")
            .expect("some")
            .expect("ok");
        assert_eq!(new_vol, 100);

        // Should have acknowledged the notification.
        let mut request_fut = stream.next();
        let request = exec.run_until_stalled(&mut request_fut).expect("stream item").expect("some");
        assert_matches!(request, Ok(ControllerRequest::NotifyNotificationHandled { .. }));
    }

    #[fuchsia::test]
    fn test_stream_terminates() {
        let mut exec = fasync::TestExecutor::new();
        let (proxy, mut stream) = create_proxy_and_stream::<ControllerMarker>();
        let id = PeerId(1);
        let avc = AbsoluteVolumeControl::new(id, proxy).expect("AVC creation failed");

        // Handle the `SetNotificationFilter` request from `new()`.
        let mut request_fut = stream.next();
        let _ = exec.run_until_stalled(&mut request_fut).expect("stream item");

        pin_mut!(avc);
        let mut vol_change_fut = avc.next();

        // The future should be pending before any events.
        assert!(exec.run_until_stalled(&mut vol_change_fut).is_pending());

        // Drop the server-side stream. This will cause the client stream to terminate
        // gracefully on its next poll.
        drop(stream);

        // Poll will now see the closed channel and return Ready with None.
        assert!(exec.run_until_stalled(&mut vol_change_fut).expect("ready").is_none());
        assert!(avc.is_terminated());
    }
}
