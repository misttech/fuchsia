// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::att::l2cap::{L2CapChannelRx, L2CapChannelTx, L2CapRecvError, L2CapSendError};
use crate::att::pdu::{Header, Packet};
use core::mem::MaybeUninit;
use thiserror::Error;
use zerocopy::{IntoBytes, TryFromBytes};

/// The default starting ATT MTU size defined by the BT Core Spec
///
/// see (Vol 3, Part G, Section 5.2.1)
pub const DEFAULT_STARTING_MTU: u16 = 23;

/// ATT Bearer Errors
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum BearerError {
    #[error("Underlying logical link was closed")]
    LinkClosed,
    #[error("Incoming packet buffer too small to fit the received payload")]
    BufferTooSmall,
    #[error("Received packet too short to contain a valid ATT header")]
    HeaderTooShort,
    #[error("Packet size exceeds the negotiated ATT MTU boundary")]
    PacketTooLarge,
    #[error("Received packet contains an invalid or unsupported ATT opcode: {0:#04X}")]
    InvalidOpcode(u8),
}

/// Map raw L2CAP receiver errors into BearerErrors automatically.
impl From<L2CapRecvError> for BearerError {
    fn from(err: L2CapRecvError) -> Self {
        match err {
            L2CapRecvError::LinkClosed => Self::LinkClosed,
            L2CapRecvError::BufferTooSmall => Self::BufferTooSmall,
        }
    }
}

/// Map raw L2CAP sender errors into BearerErrors automatically.
impl From<L2CapSendError> for BearerError {
    fn from(err: L2CapSendError) -> Self {
        match err {
            L2CapSendError::LinkClosed => Self::LinkClosed,
            L2CapSendError::SduTooLarge => Self::PacketTooLarge,
        }
    }
}

/// The transmitting handle wrapper of our ATT Bearer.
pub struct BearerTx<Tx> {
    channel_tx: Tx,
    mtu: u16,
}

impl<Tx> BearerTx<Tx>
where
    Tx: L2CapChannelTx,
{
    /// Constructor wrapping a concrete L2Cap sender socket.
    pub fn new(channel_tx: Tx) -> Self {
        Self { channel_tx, mtu: DEFAULT_STARTING_MTU }
    }

    /// Transmits an ATT packet down the underlying L2CAP channel.
    pub async fn send(&mut self, packet: &Packet) -> Result<(), BearerError> {
        // 1. MTU Validation using the actual byte size of the unsized packet
        if core::mem::size_of_val(packet) > usize::from(self.mtu) {
            return Err(BearerError::PacketTooLarge);
        }

        // 2. Forward the verified packet bytes down to the L2CAP physical channel
        self.channel_tx.send(packet.as_bytes()).await?;
        Ok(())
    }
}

/// The receiving handle wrapper of our ATT Bearer.
pub struct BearerRx<Rx> {
    channel_rx: Rx,
    mtu: u16,
}

impl<Rx> BearerRx<Rx>
where
    Rx: L2CapChannelRx,
{
    /// Constructor wrapping a concrete L2Cap receiver socket.
    pub fn new(channel_rx: Rx) -> Self {
        Self { channel_rx, mtu: DEFAULT_STARTING_MTU }
    }

    /// Pulls the next incoming SDU from the channel, validates the header invariants,
    /// and returns a structured zero-copy reference to the parsed ATT Packet.
    pub async fn next_packet<'a>(
        &mut self,
        buf: &'a mut [MaybeUninit<u8>],
    ) -> Result<&'a mut Packet, BearerError> {
        // 1. Wait for raw SDU bytes from L2CAP receiver half
        let sdu = self.channel_rx.recv(buf).await?;

        // 2. Minimum Length Validation (must cover at least the header)
        if sdu.len() < core::mem::size_of::<Header>() {
            return Err(BearerError::HeaderTooShort);
        }

        // 3. MTU Validation (Peer protocol violation check):
        if sdu.len() > self.mtu.into() {
            return Err(BearerError::PacketTooLarge);
        }

        // 4. Validate and parse the structured ATT packet header.
        let raw_opcode = sdu[0];
        let packet =
            Packet::try_mut_from_bytes(sdu).map_err(|_| BearerError::InvalidOpcode(raw_opcode))?;

        Ok(packet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::att::l2cap::mock::setup_mock_channel;
    use crate::att::l2cap::{L2CapChannelRx, L2CapChannelTx};
    use crate::att::pdu::Opcode;
    use core::mem::MaybeUninit;
    use sapphire_async::executor::BoundedExecutor;
    use sapphire_async::testing::TestExecutor;

    #[test]
    fn test_bearer_tx_sends_to_channel() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, _test_tx, mut test_rx) = setup_mock_channel(executor);

            let send_handle = executor.spawn(async move {
                let mut bearer_tx = BearerTx::new(app_channel.sender);
                let raw_packet = [0x02, 0x01, 0x02];
                let packet = Packet::try_ref_from_bytes(&raw_packet[..]).unwrap();
                bearer_tx.send(packet).await.expect("send succeeds");
            });

            let verify_handle = executor.spawn(async move {
                let raw_packet = [0x02, 0x01, 0x02];
                let mut buf = [MaybeUninit::uninit(); 32];
                let recv_packet = test_rx.recv(&mut buf).await.expect("recv succeeds");
                assert_eq!(recv_packet, &raw_packet[..]);
            });

            executor.run_until_stalled();
            assert!(send_handle.is_finished());
            assert!(verify_handle.is_finished());
        });
    }

    #[test]
    fn test_bearer_rx_receives_from_channel() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, mut test_tx, _test_rx) = setup_mock_channel(executor);

            let verify_handle = executor.spawn(async move {
                let mut bearer_rx = BearerRx::new(app_channel.receiver);
                let mut buf = [MaybeUninit::uninit(); 32];
                let recv_packet = bearer_rx.next_packet(&mut buf).await.expect("recv succeeds");
                assert_eq!(recv_packet.header.opcode, Opcode::ExchangeMtuRsp);
                assert_eq!(&recv_packet.data, &[0x04, 0x05]);
            });

            let send_handle = executor.spawn(async move {
                let raw_packet = [0x03, 0x04, 0x05];
                test_tx.send(&raw_packet[..]).await.expect("send succeeds");
            });

            executor.run_until_stalled();
            assert!(send_handle.is_finished());
            assert!(verify_handle.is_finished());
        });
    }

    #[test]
    fn test_bearer_rx_rejects_empty_packet() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, mut test_tx, _test_rx) = setup_mock_channel(executor);

            let verify_handle = executor.spawn(async move {
                let mut bearer_rx = BearerRx::new(app_channel.receiver);
                let mut buf = [MaybeUninit::uninit(); 32];
                let result = bearer_rx.next_packet(&mut buf).await;
                assert_eq!(result.err(), Some(BearerError::HeaderTooShort));
            });

            let send_handle = executor.spawn(async move {
                test_tx.send(&[]).await.expect("send succeeds");
            });

            executor.run_until_stalled();
            assert!(send_handle.is_finished());
            assert!(verify_handle.is_finished());
        });
    }

    #[test]
    fn test_bearer_rx_rejects_invalid_opcode() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, mut test_tx, _test_rx) = setup_mock_channel(executor);

            let verify_handle = executor.spawn(async move {
                let mut bearer_rx = BearerRx::new(app_channel.receiver);
                let mut buf = [MaybeUninit::uninit(); 32];
                let result = bearer_rx.next_packet(&mut buf).await;
                assert_eq!(result.err(), Some(BearerError::InvalidOpcode(0xff)));
            });

            let send_handle = executor.spawn(async move {
                let raw_packet = [0xff, 0x01, 0x02];
                test_tx.send(&raw_packet[..]).await.expect("send succeeds");
            });

            executor.run_until_stalled();
            assert!(send_handle.is_finished());
            assert!(verify_handle.is_finished());
        });
    }

    #[test]
    fn test_bearer_rx_handles_link_closure() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, test_tx, _test_rx) = setup_mock_channel(executor);

            let verify_handle = executor.spawn(async move {
                let mut bearer_rx = BearerRx::new(app_channel.receiver);
                let mut buf = [MaybeUninit::uninit(); 32];
                let result = bearer_rx.next_packet(&mut buf).await;
                assert_eq!(result.err(), Some(BearerError::LinkClosed));
            });

            executor.run_until_stalled();
            assert!(!verify_handle.is_finished());

            drop(test_tx); // Abruptly close the transport link by dropping the sender

            executor.run_until_stalled();
            assert!(verify_handle.is_finished());
        });
    }

    #[test]
    fn test_bearer_tx_rejects_exceeding_mtu() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, _test_tx, _test_rx) = setup_mock_channel(executor);

            let mut bearer_tx = BearerTx::new(app_channel.sender);

            let send_handle = executor.spawn(async move {
                let mut oversized_packet = [0u8; DEFAULT_STARTING_MTU as usize + 1];
                oversized_packet[0] = 0x02; // ExchangeMtuReq
                let packet = Packet::try_ref_from_bytes(&oversized_packet[..]).unwrap();
                assert_eq!(bearer_tx.send(packet).await.err(), Some(BearerError::PacketTooLarge));
            });

            executor.run_until_stalled();
            assert!(send_handle.is_finished());
        });
    }

    #[test]
    fn test_bearer_rx_rejects_exceeding_mtu() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, mut test_tx, _test_rx) = setup_mock_channel(executor);

            let verify_handle = executor.spawn(async move {
                let mut bearer_rx = BearerRx::new(app_channel.receiver);
                let mut buf = [MaybeUninit::uninit(); 32];
                assert_eq!(
                    bearer_rx.next_packet(&mut buf).await.err(),
                    Some(BearerError::PacketTooLarge)
                );
            });

            let send_handle = executor.spawn(async move {
                let mut oversized_packet = [0u8; DEFAULT_STARTING_MTU as usize + 1];
                oversized_packet[0] = 0x02; // ExchangeMtuReq
                test_tx.send(&oversized_packet[..]).await.expect("send succeeds");
            });

            executor.run_until_stalled();
            assert!(send_handle.is_finished());
            assert!(verify_handle.is_finished());
        });
    }
}
