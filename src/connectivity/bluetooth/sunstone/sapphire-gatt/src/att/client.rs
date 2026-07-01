// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::att::AttributeHandle;
use crate::att::bearer::{
    BearerRecvError, BearerRx, BearerSendError, BearerTx, DEFAULT_STARTING_MTU, MAX_SUPPORTED_MTU,
};
use crate::att::l2cap::{L2CapChannelRx, L2CapChannelTx};
use crate::att::pdu::{
    DynamicPacketBuilder, ErrorCode, ErrorRsp, ExchangeMtuReq, ExchangeMtuRsp,
    FindByTypeValueReqHeader, FindInformationReq, FindInformationRsp, HandlesInformation, Header,
    InformationData16, InformationData128, Opcode, Packet, PacketBuilder, UuidFormat,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveredInformation<'a> {
    Uuid16(&'a [InformationData16]),
    Uuid128(&'a [InformationData128]),
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

    /// Performs the ATT Find Information procedure to discover attribute handles
    /// and their associated UUIDs within a given handle range.
    ///
    /// The returned zero-copy data slice borrow-maps directly over the provided `rx_buf`.
    ///
    /// see Bluetooth Core Spec v6.0 (Vol 3, Part F, Section 3.4.3).
    pub async fn find_information<'a>(
        &mut self,
        starting_handle: AttributeHandle,
        ending_handle: AttributeHandle,
        rx_buf: &'a mut [MaybeUninit<u8>],
    ) -> Result<DiscoveredInformation<'a>, ClientError> {
        // Build and transmit the Find Information Request packet.
        let builder = PacketBuilder {
            header: Header { opcode: Opcode::FindInformationReq },
            payload: FindInformationReq {
                starting_handle: U16::new(starting_handle.value()),
                ending_handle: U16::new(ending_handle.value()),
            },
        };
        let tx_packet = builder.as_packet();

        let rx_packet = self
            .transaction(Opcode::FindInformationReq, tx_packet, rx_buf, Opcode::FindInformationRsp)
            .await?;

        // Parse the UUID format byte from the response header.
        if rx_packet.data.is_empty() {
            return Err(ClientError::InvalidIncomingData);
        }
        let format_byte = rx_packet.data[0];
        let format =
            UuidFormat::try_from(format_byte).map_err(|_| ClientError::InvalidIncomingData)?;

        match format {
            UuidFormat::Uuid16 => {
                let rsp = FindInformationRsp::<InformationData16>::try_ref_from_bytes(
                    &rx_packet.data[..],
                )
                .map_err(|_| ClientError::InvalidIncomingData)?;
                Ok(DiscoveredInformation::Uuid16(&rsp.info))
            }
            UuidFormat::Uuid128 => {
                let rsp = FindInformationRsp::<InformationData128>::try_ref_from_bytes(
                    &rx_packet.data[..],
                )
                .map_err(|_| ClientError::InvalidIncomingData)?;
                Ok(DiscoveredInformation::Uuid128(&rsp.info))
            }
        }
    }

    /// Initiates a Find By Type Value procedure to obtain the handle range (start and group
    /// end handle) of attributes with a specific 16-bit UUID type and value. Commonly used
    /// to discover the range of a specific service type (e.g. Heart Rate service).
    ///
    /// see Bluetooth Core Spec v6.0 (Vol 3, Part F, Section 3.4.3.3).
    pub async fn find_by_type_value<'a>(
        &mut self,
        starting_handle: AttributeHandle,
        ending_handle: AttributeHandle,
        attribute_type: u16, // 16-bit UUID only
        attribute_value: &[u8],
        rx_buf: &'a mut [MaybeUninit<u8>],
    ) -> Result<&'a [HandlesInformation], ClientError> {
        let header_builder = PacketBuilder {
            header: Header { opcode: Opcode::FindByTypeValueReq },
            payload: FindByTypeValueReqHeader {
                starting_handle: U16::new(starting_handle.value()),
                ending_handle: U16::new(ending_handle.value()),
                attribute_type: U16::new(attribute_type),
            },
        };
        let mut tx_buf = [0u8; MAX_SUPPORTED_MTU];
        let mut builder =
            DynamicPacketBuilder::<_, u8>::new(&mut tx_buf, header_builder, self.mtu() as usize);
        builder
            .extend_from_slice(attribute_value)
            .expect("Programming error: request packet size exceeds negotiated MTU.");
        let tx_packet = builder.as_packet();

        let rx_packet = self
            .transaction(Opcode::FindByTypeValueReq, tx_packet, rx_buf, Opcode::FindByTypeValueRsp)
            .await?;

        let entries = <[HandlesInformation]>::ref_from_bytes(&rx_packet.data[..])
            .map_err(|_| ClientError::InvalidIncomingData)?;
        Ok(entries)
    }

    pub fn mtu(&self) -> u16 {
        self.bearer_tx.mtu()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::att::l2cap::mock::setup_mock_channel;
    use crate::att::pdu::{DynamicPacketBuilder, FindByTypeValueReq, FindInformationRspHeader};
    use sapphire_async::executor::BoundedExecutor;
    use sapphire_async::testing::TestExecutor;

    const CLIENT_PREFERRED_MTU: u16 = 512;
    const SERVER_MTU: u16 = 256;

    fn h(val: u16) -> AttributeHandle {
        AttributeHandle::try_from(val).unwrap()
    }

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

    #[test]
    fn test_client_find_information_success() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, server_tx, server_rx) = setup_mock_channel(executor);

            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                BearerRx::new(app_channel.receiver),
                CLIENT_PREFERRED_MTU,
            );

            // Spawn mock server driver task
            let server_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 32];
                let mut server_rx_bearer = BearerRx::new(server_rx);
                let packet = server_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::FindInformationReq);

                let req = FindInformationReq::read_from_bytes(&packet.data[..]).unwrap();
                assert_eq!(req.starting_handle.get(), 1);
                assert_eq!(req.ending_handle.get(), 10);

                // Respond with FindInformationRsp (0x05)
                // format: 0x01 (16-bit)
                // entries:
                // Handle 1: UUID 0x2A00
                // Handle 2: UUID 0x2A24
                let mut tx_buf = [0u8; 64];
                let header = PacketBuilder {
                    header: Header { opcode: Opcode::FindInformationRsp },
                    payload: FindInformationRspHeader { format: UuidFormat::Uuid16 },
                };
                let mut builder = DynamicPacketBuilder::<_, InformationData16>::new(
                    &mut tx_buf,
                    header,
                    CLIENT_PREFERRED_MTU as usize,
                );

                let entry1 = InformationData16 { handle: U16::new(1), uuid: [0x00, 0x2a] };
                let entry2 = InformationData16 { handle: U16::new(2), uuid: [0x24, 0x2a] };
                builder.push(entry1).unwrap();
                builder.push(entry2).unwrap();

                let tx_packet = builder.as_packet();
                let mut server_tx_bearer = BearerTx::new(server_tx);
                server_tx_bearer.send(tx_packet).await.unwrap();
            });

            // Client task
            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 64];
                let info = client
                    .find_information(h(1), h(10), &mut rx_buf)
                    .await
                    .expect("find_information succeeds");

                match info {
                    DiscoveredInformation::Uuid16(entries) => {
                        assert_eq!(entries.len(), 2);
                        assert_eq!(entries[0].handle.get(), 1);
                        assert_eq!(entries[0].uuid, [0x00, 0x2a]);
                        assert_eq!(entries[1].handle.get(), 2);
                        assert_eq!(entries[1].uuid, [0x24, 0x2a]);
                    }
                    _ => panic!("Expected Uuid16 discovered info"),
                }
            });

            executor.run_until_stalled();
            assert!(client_handle.is_finished());
            assert!(server_handle.is_finished());
        });
    }

    #[test]
    fn test_client_find_information_error() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, server_tx, server_rx) = setup_mock_channel(executor);

            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                BearerRx::new(app_channel.receiver),
                CLIENT_PREFERRED_MTU,
            );

            // Spawn mock server driver task
            let server_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 32];
                let mut server_rx_bearer = BearerRx::new(server_rx);
                let packet = server_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::FindInformationReq);

                // Respond with ErrorRsp (InvalidHandle)
                let builder = PacketBuilder {
                    header: Header { opcode: Opcode::ErrorRsp },
                    payload: ErrorRsp {
                        request_opcode: Opcode::FindInformationReq as u8,
                        attribute_handle: U16::new(10),
                        error_code: ErrorCode::InvalidHandle,
                    },
                };
                let mut server_tx_bearer = BearerTx::new(server_tx);
                server_tx_bearer.send(builder.as_packet()).await.unwrap();
            });

            // Client task
            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 64];
                let res = client.find_information(h(10), h(20), &mut rx_buf).await;
                assert_eq!(res, Err(ClientError::ErrorResponse(ErrorCode::InvalidHandle)));
            });

            executor.run_until_stalled();
            assert!(client_handle.is_finished());
            assert!(server_handle.is_finished());
        });
    }

    #[test]
    fn test_client_find_by_type_value_success() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, server_tx, server_rx) = setup_mock_channel(executor);

            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                BearerRx::new(app_channel.receiver),
                CLIENT_PREFERRED_MTU,
            );

            let server_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 32];
                let mut server_rx_bearer = BearerRx::new(server_rx);
                let packet = server_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::FindByTypeValueReq);

                let req = FindByTypeValueReq::try_ref_from_bytes(&packet.data[..]).unwrap();
                assert_eq!(req.header.starting_handle.get(), 1);
                assert_eq!(req.header.ending_handle.get(), 10);
                assert_eq!(req.header.attribute_type.get(), 0x2800);
                assert_eq!(&req.value, &[0x0D, 0x18][..]);

                let mut tx_buf = [0u8; 64];
                let header = Header { opcode: Opcode::FindByTypeValueRsp };
                let mut builder = DynamicPacketBuilder::<_, HandlesInformation>::new(
                    &mut tx_buf,
                    header,
                    CLIENT_PREFERRED_MTU as usize,
                );
                let entry = HandlesInformation {
                    attribute_handle: U16::new(1),
                    group_end_handle: U16::new(5),
                };
                builder.push(entry).unwrap();
                let tx_packet = builder.as_packet();
                let mut server_tx_bearer = BearerTx::new(server_tx);
                server_tx_bearer.send(tx_packet).await.unwrap();
            });

            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 64];
                let results = client
                    .find_by_type_value(h(1), h(10), 0x2800, &[0x0D, 0x18], &mut rx_buf)
                    .await
                    .expect("find_by_type_value succeeds");

                assert_eq!(results.len(), 1);
                assert_eq!(results[0].attribute_handle.get(), 1);
                assert_eq!(results[0].group_end_handle.get(), 5);
            });

            executor.run_until_stalled();
            assert!(client_handle.is_finished());
            assert!(server_handle.is_finished());
        });
    }

    #[test]
    fn test_client_find_by_type_value_error() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, server_tx, server_rx) = setup_mock_channel(executor);

            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                BearerRx::new(app_channel.receiver),
                CLIENT_PREFERRED_MTU,
            );

            let server_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 32];
                let mut server_rx_bearer = BearerRx::new(server_rx);
                let packet = server_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::FindByTypeValueReq);

                let builder = PacketBuilder {
                    header: Header { opcode: Opcode::ErrorRsp },
                    payload: ErrorRsp {
                        request_opcode: Opcode::FindByTypeValueReq as u8,
                        attribute_handle: U16::new(1),
                        error_code: ErrorCode::AttributeNotFound,
                    },
                };
                let mut server_tx_bearer = BearerTx::new(server_tx);
                server_tx_bearer.send(builder.as_packet()).await.unwrap();
            });

            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 64];
                let res = client
                    .find_by_type_value(h(1), h(10), 0x2800, &[0x0D, 0x18], &mut rx_buf)
                    .await;
                assert_eq!(res, Err(ClientError::ErrorResponse(ErrorCode::AttributeNotFound)));
            });

            executor.run_until_stalled();
            assert!(client_handle.is_finished());
            assert!(server_handle.is_finished());
        });
    }
}
