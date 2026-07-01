// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::att::bearer::{
    BearerRecvError, BearerRx, BearerSendError, BearerTx, DEFAULT_STARTING_MTU,
};
use crate::att::l2cap::{L2CapChannelRx, L2CapChannelTx};
use crate::att::pdu::{
    ErrorCode, ErrorRsp, ExchangeMtuReq, ExchangeMtuRsp, Header, Opcode, Packet, PacketBuilder,
};
use core::cmp::{max, min};
use core::mem::MaybeUninit;
use thiserror::Error;
use zerocopy::byteorder::little_endian::U16;
use zerocopy::{FromBytes, TryFromBytes};

#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientError {
    #[error("Underlying logical link was closed")]
    LinkClosed,
    #[error("Unexpected response opcode: {0:?}")]
    UnexpectedOpcode(Opcode),
    #[error("Error response from server: {0:?}")]
    ErrorResponse(ErrorCode),
    #[error("Invalid incoming data from server")]
    InvalidIncomingData,
}

/// ATT Client protocol wrapper.
pub struct Client<Tx, Rx> {
    bearer_tx: BearerTx<Tx>,
    bearer_rx: BearerRx<Rx>,
    preferred_mtu: u16,
}

impl<Tx, Rx> Client<Tx, Rx>
where
    Tx: L2CapChannelTx,
    Rx: L2CapChannelRx,
{
    /// Creates a new ATT Client instance.
    pub fn new(bearer_tx: BearerTx<Tx>, bearer_rx: BearerRx<Rx>, preferred_mtu: u16) -> Self {
        Self { bearer_tx, bearer_rx, preferred_mtu }
    }

    /// Helper to perform a sequential ATT request-response transaction.
    ///
    /// This method:
    /// 1. Formats and transmits the request packet over the ATT bearer.
    /// 2. Awaits the incoming response packet from the server.
    /// 3. Validates the response:
    ///    - If it matches `expected_rsp_opcode`, it is returned as `Ok`.
    ///    - If it is an `ErrorRsp` and corresponds to our request, the specific
    ///      `ErrorCode` is parsed and returned as a `ClientError::ErrorResponse`.
    ///    - Otherwise, returns `ClientError::UnexpectedOpcode`.
    async fn transaction<'a>(
        &mut self,
        req_opcode: Opcode,
        req_packet: &Packet,
        rx_buf: &'a mut [MaybeUninit<u8>],
        expected_rsp_opcode: Opcode,
    ) -> Result<&'a mut Packet, ClientError> {
        // Verify the provided buffer is large enough to hold any valid packet under the negotiated MTU.
        assert!(
            rx_buf.len() >= usize::from(self.bearer_tx.mtu()),
            "Programming error: provided buffer size is smaller than the negotiated MTU."
        );

        self.send_packet(req_packet).await?;

        let rx_packet = self.bearer_rx.next_packet(rx_buf).await.map_err(|e| match e {
            BearerRecvError::LinkClosed => ClientError::LinkClosed,
            BearerRecvError::BufferTooSmall => {
                panic!(
                    "Programming error: provided buffer size is smaller than the negotiated MTU."
                );
            }
            BearerRecvError::HeaderTooShort => ClientError::InvalidIncomingData,
            BearerRecvError::PacketTooLarge { .. } => ClientError::InvalidIncomingData,
            BearerRecvError::InvalidOpcode(_) => ClientError::InvalidIncomingData,
        })?;

        match rx_packet.header.opcode {
            opcode if opcode == expected_rsp_opcode => Ok(rx_packet),
            Opcode::ErrorRsp => {
                let err = ErrorRsp::try_read_from_bytes(&rx_packet.data[..])
                    .map_err(|_| ClientError::InvalidIncomingData)?;
                if err.request_opcode == req_opcode.into() {
                    Err(ClientError::ErrorResponse(err.error_code))
                } else {
                    Err(ClientError::UnexpectedOpcode(Opcode::ErrorRsp))
                }
            }
            other => Err(ClientError::UnexpectedOpcode(other)),
        }
    }

    /// Helper to send a single packet. Panics if the packet is too large for the negotiated MTU.
    async fn send_packet(&mut self, packet: &Packet) -> Result<(), ClientError> {
        self.bearer_tx.send(packet).await.map_err(|e| match e {
            BearerSendError::LinkClosed => ClientError::LinkClosed,
            BearerSendError::PacketTooLarge => {
                panic!("Programming error: outgoing packet size exceeds the negotiated MTU.");
            }
        })
    }

    /// Performs the Exchange MTU handshake procedure sequentially.
    ///
    /// Updates the negotiated MTU on the underlying bearer.
    ///
    /// see (Vol 3, Part G, Section 5.2.1) and (Vol 3, Part F, Section 3.4.2)
    pub async fn exchange_mtu(&mut self) -> Result<(), ClientError> {
        let builder = PacketBuilder {
            header: Header { opcode: Opcode::ExchangeMtuReq },
            payload: ExchangeMtuReq { client_rx_mtu: U16::new(self.preferred_mtu) },
        };
        let tx_packet = builder.as_packet();
        let mut rx_buf = [MaybeUninit::uninit(); DEFAULT_STARTING_MTU as usize];

        match self
            .transaction(Opcode::ExchangeMtuReq, tx_packet, &mut rx_buf, Opcode::ExchangeMtuRsp)
            .await
        {
            Ok(rx_packet) => {
                let rsp = ExchangeMtuRsp::read_from_bytes(&rx_packet.data[..])
                    .map_err(|_| ClientError::InvalidIncomingData)?;
                let server_mtu = rsp.server_rx_mtu.get();
                let negotiated_mtu = max(DEFAULT_STARTING_MTU, min(self.preferred_mtu, server_mtu));

                self.bearer_tx.set_mtu(negotiated_mtu);
                self.bearer_rx.set_mtu(negotiated_mtu);
                Ok(())
            }
            Err(ClientError::ErrorResponse(ErrorCode::RequestNotSupported)) => {
                // Safely recover by locking in the default fallback MTU
                self.bearer_tx.set_mtu(DEFAULT_STARTING_MTU);
                self.bearer_rx.set_mtu(DEFAULT_STARTING_MTU);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    pub fn mtu(&self) -> u16 {
        self.bearer_tx.mtu()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::att::l2cap::mock::setup_mock_channel;
    use sapphire_async::executor::BoundedExecutor;
    use sapphire_async::testing::TestExecutor;

    const CLIENT_PREFERRED_MTU: u16 = 512;
    const SERVER_MTU: u16 = 256;

    #[test]
    fn test_client_exchange_mtu_success() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, test_tx, test_rx) = setup_mock_channel(executor);

            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                BearerRx::new(app_channel.receiver),
                CLIENT_PREFERRED_MTU,
            );

            // Spawn mock server driver task
            let server_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 32];
                let mut server_rx_bearer = BearerRx::new(test_rx);
                let packet = server_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ExchangeMtuReq);

                let req = ExchangeMtuReq::read_from_bytes(&packet.data[..]).unwrap();
                assert_eq!(req.client_rx_mtu.get(), CLIENT_PREFERRED_MTU);

                // Reply with ExchangeMtuRsp containing 256-byte MTU
                let builder = PacketBuilder {
                    header: Header { opcode: Opcode::ExchangeMtuRsp },
                    payload: ExchangeMtuRsp { server_rx_mtu: U16::new(SERVER_MTU) },
                };

                let tx_packet = builder.as_packet();
                let mut server_tx_bearer = BearerTx::new(test_tx);
                server_tx_bearer.send(tx_packet).await.unwrap();
            });

            // Spawn client driver task
            let client_handle = executor.spawn(async move {
                client.exchange_mtu().await.expect("handshake completes");
                assert_eq!(client.mtu(), SERVER_MTU);
            });

            executor.run_until_stalled();

            assert!(server_handle.is_finished());
            assert!(client_handle.is_finished());
        });
    }

    #[test]
    fn test_client_exchange_mtu_unsupported_fallback() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, test_tx, test_rx) = setup_mock_channel(executor);

            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                BearerRx::new(app_channel.receiver),
                CLIENT_PREFERRED_MTU,
            );

            // Mock server responds with ErrorRsp for ExchangeMtuReq with error RequestNotSupported
            let server_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 32];
                let mut server_rx_bearer = BearerRx::new(test_rx);
                let packet = server_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ExchangeMtuReq);

                // ErrorResponse payload: request opcode 0x02, handle 0x0000, error code 0x06 (RequestNotSupported)
                let builder = PacketBuilder {
                    header: Header { opcode: Opcode::ErrorRsp },
                    payload: ErrorRsp {
                        request_opcode: Opcode::ExchangeMtuReq.into(),
                        attribute_handle: U16::new(0),
                        error_code: ErrorCode::RequestNotSupported,
                    },
                };

                let tx_packet = builder.as_packet();
                let mut server_tx_bearer = BearerTx::new(test_tx);
                server_tx_bearer.send(tx_packet).await.unwrap();
            });

            let client_handle = executor.spawn(async move {
                // Client must fall back to default 23-byte MTU
                client.exchange_mtu().await.expect("handshake completes");
                assert_eq!(client.mtu(), DEFAULT_STARTING_MTU);
            });

            executor.run_until_stalled();

            assert!(server_handle.is_finished());
            assert!(client_handle.is_finished());
        });
    }

    #[test]
    fn test_client_exchange_mtu_hard_error() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, test_tx, test_rx) = setup_mock_channel(executor);

            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                BearerRx::new(app_channel.receiver),
                CLIENT_PREFERRED_MTU,
            );

            // Mock server responds with ErrorRsp indicating InsufficientAuthentication
            let server_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 32];
                let mut server_rx_bearer = BearerRx::new(test_rx);
                let packet = server_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ExchangeMtuReq);

                let builder = PacketBuilder {
                    header: Header { opcode: Opcode::ErrorRsp },
                    payload: ErrorRsp {
                        request_opcode: Opcode::ExchangeMtuReq.into(),
                        attribute_handle: U16::new(0),
                        error_code: ErrorCode::InsufficientAuthentication,
                    },
                };

                let tx_packet = builder.as_packet();
                let mut server_tx_bearer = BearerTx::new(test_tx);
                server_tx_bearer.send(tx_packet).await.unwrap();
            });

            let client_handle = executor.spawn(async move {
                // Client must abort and propagate ClientError::ErrorResponse
                let res = client.exchange_mtu().await;
                assert_eq!(
                    res,
                    Err(ClientError::ErrorResponse(ErrorCode::InsufficientAuthentication))
                );
            });

            executor.run_until_stalled();

            assert!(server_handle.is_finished());
            assert!(client_handle.is_finished());
        });
    }
}
