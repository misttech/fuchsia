// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Datagram socket tx metadata definitions.

use derivative::Derivative;
use net_types::ip::{GenericOverIp, Ip};
use netstack3_base::{ChecksumOffloadResult, WeakDeviceIdentifier};

use crate::internal::datagram::{DatagramSocketSpec, IpExt};

/// The tx metadata associated with a datagram socket.
#[derive(Derivative, GenericOverIp)]
#[generic_over_ip(I, Ip)]
#[derivative(Debug(bound = ""))]
pub struct TxMetadata<I: IpExt, D: WeakDeviceIdentifier, S: DatagramSocketSpec> {
    socket: S::WeakSocketId<I, D>,
    send_token: Option<S::SendToken>,
    checksum_offload_result: Option<ChecksumOffloadResult>,
}

impl<I: IpExt, D: WeakDeviceIdentifier, S: DatagramSocketSpec> TxMetadata<I, D, S> {
    /// Creates a new `TxMetadata`.
    pub(crate) fn new(socket: &S::SocketId<I, D>, send_token: S::SendToken) -> Self {
        Self {
            socket: S::downgrade_socket_id(socket),
            send_token: Some(send_token),
            checksum_offload_result: None,
        }
    }

    /// Gets the socket from which the packet originates.
    pub fn socket(&self) -> &S::WeakSocketId<I, D> {
        &self.socket
    }

    /// Returns the checksum offload result.
    pub fn checksum_offload_result(&self) -> Option<ChecksumOffloadResult> {
        self.checksum_offload_result.clone()
    }

    /// Sets the checksum offload result.
    pub fn set_checksum_offload_result(&mut self, result: Option<ChecksumOffloadResult>) {
        self.checksum_offload_result = result;
    }

    /// Splits metadata for multiple frames.
    ///
    /// Returns `(primary, secondary)`. The `primary` instance retains the buffer
    /// ownership token (`send_token`), while both instances retain socket
    /// attribution and checksum offload configuration.
    pub fn split_for_multiple_frames(self) -> (Self, Self) {
        let secondary = Self {
            socket: self.socket.clone(),
            send_token: None,
            checksum_offload_result: self.checksum_offload_result.clone(),
        };
        let primary = self;
        (primary, secondary)
    }
}

#[cfg(any(test, feature = "testutils"))]
impl<I: IpExt, D: WeakDeviceIdentifier, S: DatagramSocketSpec> PartialEq for TxMetadata<I, D, S> {
    fn eq(&self, other: &Self) -> bool {
        // Tx metadata is always a unique instance accompanying a frame and it's
        // not copiable. So it may only be equal to another instance if they're
        // the exact same object.
        core::ptr::eq(self, other)
    }
}
