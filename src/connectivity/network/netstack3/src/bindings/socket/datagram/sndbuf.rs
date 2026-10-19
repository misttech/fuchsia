// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Datagram socket send buffer tracking.
//!
//! This module provides [`SendBuffer`] for tracking in-flight egress packet
//! buffer utilization on datagram (UDP and ICMP Echo) sockets against
//! configurable buffer limits.
//!
//! When transmitting packets, bindings allocates space via
//! [`SendBuffer::acquire_token`], which returns a [`SendBufferToken`]. This
//! token is passed to Netstack3 core alongside the packet and held in
//! `TxMetadata` until the packet is transmitted by the device or dropped along
//! the egress path. Dropping the token automatically releases the allocated
//! buffer space and updates socket writability signals on the
//! [`SocketEventPair`].

use std::sync::Arc;

use fidl_fuchsia_posix as fposix;
use netstack3_core::socket::{SendBufferFullError, SendBufferSpace, SendBufferTracking};
use netstack3_core::types::{BufferSizeSettings, PositiveIsize};
use thiserror::Error;

use crate::bindings::BindingsCtx;
use crate::bindings::socket::IntoErrno;
use crate::bindings::socket::event_pair::SocketEventPair;

/// A send buffer manager for a datagram socket.
#[derive(Debug, Clone)]
pub(crate) struct SendBuffer(Arc<SendBufferTracking<SocketEventPair, BindingsCtx>>);

/// An opaque token representing space acquired in the send buffer for an in-flight packet.
///
/// Serves as the concrete [`netstack3_core::udp::UdpBindingsTypes::SendToken`] and
/// [`netstack3_core::icmp_echo::IcmpEchoBindingsTypes::SendToken`] implementation for
/// Netstack3 bindings.
///
/// Implements RAII: when created, memory has been accounted against the socket's
/// [`SendBufferTracking`]. When dropped (either when the packet is transmitted by the
/// underlying device or dropped due to an error along the egress path), the allocated
/// send buffer space is released back to the socket's [`SendBufferTracking`]. If the
/// buffer was previously full and newly available space crosses below the threshold,
/// writability is signaled on the socket event pair to notify waiting writers.
#[derive(Debug)]
pub struct SendBufferToken {
    tracking: Arc<SendBufferTracking<SocketEventPair, BindingsCtx>>,
    space: Option<SendBufferSpace>,
}

impl Drop for SendBufferToken {
    fn drop(&mut self) {
        if let Some(space) = self.space.take() {
            self.tracking.release(space);
        }
    }
}

#[derive(Error, Debug, PartialEq, Eq)]
pub(crate) enum SendBufferError {
    #[error("send buffer full")]
    SendBufferFull,
    #[error("invalid message length")]
    InvalidLength,
}

impl From<SendBufferFullError> for SendBufferError {
    fn from(SendBufferFullError: SendBufferFullError) -> Self {
        Self::SendBufferFull
    }
}

impl IntoErrno for SendBufferError {
    fn to_errno(&self) -> fposix::Errno {
        match self {
            SendBufferError::SendBufferFull => fposix::Errno::Eagain,
            SendBufferError::InvalidLength => fposix::Errno::Emsgsize,
        }
    }
}

impl SendBuffer {
    pub(crate) fn new(
        listener: SocketEventPair,
        settings: &BufferSizeSettings<PositiveIsize>,
    ) -> Self {
        Self(Arc::new(SendBufferTracking::new(settings.default(), listener)))
    }

    pub(crate) fn set_capacity(
        &self,
        capacity: usize,
        settings: &BufferSizeSettings<PositiveIsize>,
    ) {
        let capacity = PositiveIsize::new_unsigned(capacity.max(settings.min().into()))
            .unwrap_or_else(|| settings.max())
            .min(settings.max());
        self.0.set_capacity(capacity);
    }

    pub(crate) fn capacity(&self) -> usize {
        self.0.capacity().into()
    }

    #[cfg(test)]
    pub(crate) fn available(&self) -> usize {
        self.0.available().map(Into::into).unwrap_or(0)
    }

    /// Acquires a send token for sending a datagram of `size` bytes.
    pub(crate) fn acquire_token(&self, size: usize) -> Result<SendBufferToken, SendBufferError> {
        let size = PositiveIsize::new_unsigned(size).ok_or(SendBufferError::InvalidLength)?;
        let space = self.0.acquire(size)?;
        Ok(SendBufferToken { tracking: self.0.clone(), space: Some(space) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;

    const MIN_CAPACITY: usize = 100;
    const DEFAULT_CAPACITY: usize = 1000;
    const MAX_CAPACITY: usize = 10000;

    const PACKET_SIZE_1: usize = 128;
    const PACKET_SIZE_2: usize = 148;
    const PACKET_SIZE_3: usize = 78;

    fn test_settings() -> BufferSizeSettings<PositiveIsize> {
        BufferSizeSettings::new(
            PositiveIsize::new(MIN_CAPACITY as isize).unwrap(),
            PositiveIsize::new(DEFAULT_CAPACITY as isize).unwrap(),
            PositiveIsize::new(MAX_CAPACITY as isize).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn test_send_buffer_acquire_and_release() {
        let (local, _peer) = SocketEventPair::create();
        let settings = test_settings();
        let sndbuf = SendBuffer::new(local, &settings);
        assert_eq!(sndbuf.capacity(), DEFAULT_CAPACITY);
        assert_eq!(sndbuf.available(), DEFAULT_CAPACITY);

        // Acquire 128 bytes.
        let token = sndbuf.acquire_token(PACKET_SIZE_1).expect("acquire token");
        assert_eq!(sndbuf.available(), DEFAULT_CAPACITY - PACKET_SIZE_1);

        // Acquire another 148 bytes.
        let token2 = sndbuf.acquire_token(PACKET_SIZE_2).expect("acquire token");
        assert_eq!(sndbuf.available(), DEFAULT_CAPACITY - PACKET_SIZE_1 - PACKET_SIZE_2);

        // Dropping token releases space.
        drop(token);
        assert_eq!(sndbuf.available(), DEFAULT_CAPACITY - PACKET_SIZE_2);

        drop(token2);
        assert_eq!(sndbuf.available(), DEFAULT_CAPACITY);
    }

    #[test]
    fn test_send_buffer_full() {
        let (local, _peer) = SocketEventPair::create();
        let settings = test_settings();
        let sndbuf = SendBuffer::new(local, &settings);

        sndbuf.set_capacity(MIN_CAPACITY, &settings);
        assert_eq!(sndbuf.capacity(), MIN_CAPACITY);
        assert_eq!(sndbuf.available(), MIN_CAPACITY);

        // Acquire 78 bytes.
        let token = sndbuf.acquire_token(PACKET_SIZE_3).expect("acquire token");
        assert_eq!(sndbuf.available(), MIN_CAPACITY - PACKET_SIZE_3);

        // Second packet overcommits the remaining 22 bytes.
        let token2 = sndbuf.acquire_token(PACKET_SIZE_3).expect("acquire token with overcommit");
        assert_eq!(sndbuf.available(), 0);

        // Third attempt when buffer is full (available <= 0) -> SendBufferFull.
        assert_matches!(sndbuf.acquire_token(PACKET_SIZE_3), Err(SendBufferError::SendBufferFull));

        drop(token);
        drop(token2);
        assert_eq!(sndbuf.available(), MIN_CAPACITY);
    }

    #[test]
    fn test_send_buffer_limits() {
        let (local, _peer) = SocketEventPair::create();
        let settings = test_settings();
        let sndbuf = SendBuffer::new(local, &settings);

        sndbuf.set_capacity(MIN_CAPACITY / 2, &settings);
        assert_eq!(sndbuf.capacity(), MIN_CAPACITY);

        sndbuf.set_capacity(MAX_CAPACITY * 2, &settings);
        assert_eq!(sndbuf.capacity(), MAX_CAPACITY);
    }

    #[test]
    fn test_send_buffer_invalid_length() {
        let (local, _peer) = SocketEventPair::create();
        let settings = test_settings();
        let sndbuf = SendBuffer::new(local, &settings);

        assert_matches!(sndbuf.acquire_token(usize::MAX), Err(SendBufferError::InvalidLength));
        assert_matches!(sndbuf.acquire_token(0), Err(SendBufferError::InvalidLength));
    }
}
