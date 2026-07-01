// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::att::AttributeHandle;
use crate::att::attribute::Attribute;
use crate::att::bearer::{
    BearerRecvError, BearerRx, BearerSendError, BearerTx, DEFAULT_STARTING_MTU, MAX_ATTRIBUTE_SIZE,
    MAX_SUPPORTED_MTU,
};
use crate::att::database::Database;
use crate::att::l2cap::{L2CapChannelRx, L2CapChannelTx};
use crate::att::pdu::{
    DynamicPacketBuilder, ErrorCode, ErrorRsp, ExchangeMtuReq, ExchangeMtuRsp, FindByTypeValueReq,
    FindInformationReq, FindInformationRspHeader, HandlesInformation, Header, InformationData,
    InformationData16, InformationData128, Opcode, Packet, PacketBuilder, ReadReq, UuidFormat,
};
use core::cmp::{max, min};
use core::convert::Infallible;
use core::mem::{MaybeUninit, size_of};
use sapphire_peer_cache::PeerId;
use thiserror::Error;
use zerocopy::byteorder::little_endian::U16;
use zerocopy::{FromBytes, Immutable, IntoBytes, TryFromBytes};

#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerError {
    #[error("Underlying logical link was closed")]
    LinkClosed,
}

#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
enum TransactionError {
    #[error(transparent)]
    ServerError(#[from] ServerError),
    #[error("Unexpected PDU opcode: {received_opcode:?}")]
    UnexpectedPdu { received_opcode: Opcode },
    #[error("Invalid PDU structure for request: {request_opcode:?}")]
    InvalidPdu { request_opcode: Opcode },
    #[error(
        "Error response {error_code:?} for request {request_opcode:?} on handle {attribute_handle:#06X}"
    )]
    ErrorResponse { request_opcode: Opcode, attribute_handle: u16, error_code: ErrorCode },
}

/// The ATT Server protocol wrapper.
pub struct Server<Tx, Rx, DB> {
    peer_id: PeerId,
    bearer_tx: BearerTx<Tx>,
    bearer_rx: BearerRx<Rx>,
    server_rx_mtu: u16,
    database: DB,
}

impl<Tx, Rx, DB> Server<Tx, Rx, DB>
where
    Tx: L2CapChannelTx,
    Rx: L2CapChannelRx,
    DB: Database,
{
    /// Creates a new ATT Server instance.
    pub fn new(
        peer_id: PeerId,
        bearer_tx: BearerTx<Tx>,
        bearer_rx: BearerRx<Rx>,
        server_rx_mtu: u16,
        database: DB,
    ) -> Self {
        assert!(
            usize::from(server_rx_mtu) <= MAX_SUPPORTED_MTU,
            "server_rx_mtu ({}) exceeds MAX_SUPPORTED_MTU ({})",
            server_rx_mtu,
            MAX_SUPPORTED_MTU
        );
        Self { peer_id, bearer_tx, bearer_rx, server_rx_mtu, database }
    }

    /// Runs the server receive loop, processing inbound requests sequentially
    /// until the underlying channel is closed or an error occurs.
    pub async fn run(&mut self) -> Result<Infallible, ServerError> {
        loop {
            self.handle_request().await?;
        }
    }

    /// Processes a single inbound request packet.
    pub async fn handle_request(&mut self) -> Result<(), ServerError> {
        // TODO(https://fxbug.dev/530178099): Reconsider stack allocation here, as it bloats the generated Future's
        // size. Consider storing a reusable buffer in Server or heap-allocating to match MTU.
        let mut rx_buf = [MaybeUninit::uninit(); MAX_SUPPORTED_MTU];
        let rx_packet = match self.bearer_rx.next_packet(&mut rx_buf).await {
            Ok(pkt) => pkt,
            // Channel disconnected. Terminate server.
            Err(BearerRecvError::LinkClosed) => return Err(ServerError::LinkClosed),

            // If the Attribute Opcode cannot be determined because the request was too short or
            // exceeded MAX_SUPPORTED_MTU, the server responds with an Error Response (Invalid PDU)
            // setting the Opcode In Error to 0x00.
            //
            // see Bluetooth Core Spec v6.0 (Vol 3, Part F, Section 3.4.1.1)
            Err(BearerRecvError::HeaderTooShort) => {
                self.send_error_response(0u8, None, ErrorCode::InvalidPdu).await?;
                return Ok(());
            }
            Err(BearerRecvError::BufferTooSmall) => {
                panic!(
                    "Programming error: provided buffer size is smaller than the negotiated MTU."
                );
            }

            // Packet size exceeds MTU. According to BT Spec, the server shall return an
            // Error Response with the error code set to Invalid PDU (0x04).
            //
            // see (Vol 3, Part F, 3.4.1.1)
            Err(BearerRecvError::PacketTooLarge { opcode }) => {
                self.send_error_response(opcode, None, ErrorCode::InvalidPdu).await?;
                return Ok(());
            }

            // Unknown or unsupported opcode. According to BT Spec, the server shall respond
            // with Request Not Supported (0x06).
            //
            // see (Vol 3, Part F, 3.4.1.1)
            Err(BearerRecvError::InvalidOpcode(raw_opcode)) => {
                self.send_error_response(raw_opcode, None, ErrorCode::RequestNotSupported).await?;
                return Ok(());
            }
        };

        let transaction_result = match rx_packet.header.opcode {
            Opcode::ExchangeMtuReq => self.handle_exchange_mtu(&rx_packet.data).await,
            Opcode::FindInformationReq => self.handle_find_information(&rx_packet.data).await,
            Opcode::FindByTypeValueReq => self.handle_find_by_type_value(&rx_packet.data).await,
            Opcode::ReadReq => self.handle_read(&rx_packet.data).await,
            other => Err(TransactionError::UnexpectedPdu { received_opcode: other }),
        };

        match transaction_result {
            Ok(()) => Ok(()),
            Err(TransactionError::ServerError(e)) => Err(e),
            Err(TransactionError::UnexpectedPdu { received_opcode }) => {
                self.send_error_response(received_opcode, None, ErrorCode::RequestNotSupported)
                    .await
            }
            Err(TransactionError::InvalidPdu { request_opcode }) => {
                self.send_error_response(request_opcode, None, ErrorCode::InvalidPdu).await
            }
            Err(TransactionError::ErrorResponse {
                request_opcode,
                attribute_handle,
                error_code,
            }) => {
                self.send_error_response(
                    request_opcode,
                    AttributeHandle::new(attribute_handle),
                    error_code,
                )
                .await
            }
        }
    }

    /// Handles an incoming Exchange MTU Request and responds with an Exchange MTU Response.
    /// Negotiates the ATT_MTU for the connection.
    ///
    /// see Bluetooth Core Spec v6.0 (Vol 3, Part F, Section 3.4.2.1) and (Vol 3, Part G, Section 5.2.1)
    async fn handle_exchange_mtu(&mut self, data: &[u8]) -> Result<(), TransactionError> {
        let req = ExchangeMtuReq::read_from_bytes(data)
            .map_err(|_| TransactionError::InvalidPdu { request_opcode: Opcode::ExchangeMtuReq })?;
        let client_mtu = req.client_rx_mtu.get();

        let negotiated_mtu = max(DEFAULT_STARTING_MTU, min(client_mtu, self.server_rx_mtu));

        // Update MTU for both active bearer halves
        self.bearer_tx.set_mtu(negotiated_mtu);
        self.bearer_rx.set_mtu(negotiated_mtu);

        // Respond with ExchangeMtuRsp containing our supported rx MTU
        let builder = PacketBuilder {
            header: Header { opcode: Opcode::ExchangeMtuRsp },
            payload: ExchangeMtuRsp { server_rx_mtu: U16::new(self.server_rx_mtu) },
        };

        let tx_packet = builder.as_packet();
        self.send_packet(tx_packet).await?;

        Ok(())
    }

    /// Handles an incoming Find Information Request, querying the database and sending a
    /// Find Information Response (or Error Response) back to the client.
    ///
    /// see Bluetooth Core Spec v6.0 (Vol 3, Part F, Section 3.4.3).
    async fn handle_find_information(&mut self, data: &[u8]) -> Result<(), TransactionError> {
        // Read request and validate handle range invariants.
        let req = FindInformationReq::read_from_bytes(data).map_err(|_| {
            TransactionError::InvalidPdu { request_opcode: Opcode::FindInformationReq }
        })?;
        let start = req.starting_handle.get();
        let end = req.ending_handle.get();

        if start > end {
            return Err(TransactionError::ErrorResponse {
                request_opcode: Opcode::FindInformationReq,
                attribute_handle: start,
                error_code: ErrorCode::InvalidHandle,
            });
        }

        let start_handle = to_handle(start, Opcode::FindInformationReq)?;
        let end_handle = to_handle(end, Opcode::FindInformationReq)?;
        let mut attributes = self.database.query_range(start_handle, end_handle).peekable();
        let format = match attributes.peek() {
            Some((_, attr)) => UuidFormat::from(*attr.uuid()),
            None => {
                return Err(TransactionError::ErrorResponse {
                    request_opcode: Opcode::FindInformationReq,
                    attribute_handle: start,
                    error_code: ErrorCode::AttributeNotFound,
                });
            }
        };

        let mut tx_buf = [0u8; MAX_SUPPORTED_MTU];
        assert!(
            tx_buf.len() >= usize::from(self.mtu()),
            "Programming error: transmission buffer size is smaller than the negotiated MTU."
        );

        // Serialize the PDU-specific header using PacketBuilder
        let header = PacketBuilder {
            header: Header { opcode: Opcode::FindInformationRsp },
            payload: FindInformationRspHeader { format },
        };

        // Pack as many contiguous, format-matching attributes as fit in the MTU.
        let tx_packet = match format {
            UuidFormat::Uuid16 => Self::pack_find_info_rsp::<InformationData16>(
                &mut tx_buf,
                header,
                self.mtu() as usize,
                attributes,
            ),
            UuidFormat::Uuid128 => Self::pack_find_info_rsp::<InformationData128>(
                &mut tx_buf,
                header,
                self.mtu() as usize,
                attributes,
            ),
        };

        self.send_packet(tx_packet).await?;

        Ok(())
    }

    fn pack_find_info_rsp<'a, 'buf, T>(
        tx_buf: &'buf mut [u8],
        header: impl IntoBytes + Immutable,
        mtu: usize,
        attributes: impl Iterator<Item = (AttributeHandle, &'a DB::Attr)>,
    ) -> &'buf Packet
    where
        T: InformationData,
        DB::Attr: 'a,
    {
        let mut builder = DynamicPacketBuilder::<_, T>::new(tx_buf, header, mtu);
        for (handle, attr) in
            attributes.take_while(|(_, attr)| UuidFormat::from(*attr.uuid()) == T::FORMAT)
        {
            let entry = T::try_from((handle.value(), attr.uuid()))
                .expect("UUID format matches but TryFrom failed");
            if builder.push(entry).is_err() {
                break;
            }
        }
        builder.as_packet()
    }

    /// Handles an incoming Find By Type Value Request.
    ///
    /// Queries the database for attributes matching the requested range, type, and value,
    /// and responds with their handle ranges. If no matches are found, returns `AttributeNotFound`.
    ///
    /// see Bluetooth Core Spec v6.0 (Vol 3, Part F, Section 3.4.3.3).
    async fn handle_find_by_type_value(&mut self, data: &[u8]) -> Result<(), TransactionError> {
        let req = FindByTypeValueReq::try_ref_from_bytes(data).map_err(|_| {
            TransactionError::InvalidPdu { request_opcode: Opcode::FindByTypeValueReq }
        })?;
        let start = req.header.starting_handle.get();
        let end = req.header.ending_handle.get();
        let attr_type = req.header.attribute_type.get();
        let requested_value = &req.value;

        if start > end {
            return Err(TransactionError::ErrorResponse {
                request_opcode: Opcode::FindByTypeValueReq,
                attribute_handle: start,
                error_code: ErrorCode::InvalidHandle,
            });
        }

        let mut tx_buf = [0u8; MAX_SUPPORTED_MTU];
        assert!(
            tx_buf.len() >= usize::from(self.mtu()),
            "Programming error: transmission buffer size is smaller than the negotiated MTU."
        );

        // DynamicPacketBuilder is used for a variable-length list of response entries.
        let header = Header { opcode: Opcode::FindByTypeValueRsp };

        let mut builder = DynamicPacketBuilder::<_, HandlesInformation>::new(
            &mut tx_buf,
            header,
            self.mtu() as usize,
        );

        let start_handle = to_handle(start, Opcode::FindByTypeValueReq)?;
        let end_handle = to_handle(end, Opcode::FindByTypeValueReq)?;
        let attributes = self.database.query_range(start_handle, end_handle).filter(|(_, attr)| {
            <[u8; 2]>::try_from(*attr.uuid())
                .is_ok_and(|bytes16| u16::from_le_bytes(bytes16) == attr_type)
        });
        for (handle, attr) in attributes {
            // TODO(https://fxbug.dev/527551044): Implement zero-copy value matching in Attribute trait to avoid stack copying in ATT Server
            // Should be done when production GATT Database is implemented (which will implement the Attribute trait)
            let mut read_buf = [0u8; MAX_ATTRIBUTE_SIZE];
            if let Ok(read_len) = attr.read_chunk(self.peer_id, 0, &mut read_buf).await {
                if read_len == requested_value.len() && &read_buf[..read_len] == requested_value {
                    let group_end = attr.group_end_handle().unwrap_or_else(|| handle.value());
                    let entry = HandlesInformation {
                        attribute_handle: U16::new(handle.value()),
                        group_end_handle: U16::new(group_end),
                    };
                    // Stop packing if adding the entry would exceed the negotiated MTU.
                    if builder.push(entry).is_err() {
                        break;
                    }
                }
            }
        }

        let tx_packet = builder.as_packet();
        if tx_packet.data.is_empty() {
            return Err(TransactionError::ErrorResponse {
                request_opcode: Opcode::FindByTypeValueReq,
                attribute_handle: start,
                error_code: ErrorCode::AttributeNotFound,
            });
        }

        self.send_packet(tx_packet).await?;

        Ok(())
    }

    /// Handles an incoming Read Request and responds with a Read Response containing the value.
    ///
    /// see Bluetooth Core Spec v6.0 (Vol 3, Part F, Section 3.4.4.1 & 3.4.4.2)
    async fn handle_read(&mut self, data: &[u8]) -> Result<(), TransactionError> {
        // Parse the incoming Read Request.
        let req = ReadReq::read_from_bytes(data)
            .map_err(|_| TransactionError::InvalidPdu { request_opcode: Opcode::ReadReq })?;

        let handle_val = req.attribute_handle.get();

        // Find the requested attribute in the local database.
        let handle = to_handle(handle_val, Opcode::ReadReq)?;
        let attr = self.database.find_attribute(handle).ok_or_else(|| {
            TransactionError::ErrorResponse {
                request_opcode: Opcode::ReadReq,
                attribute_handle: handle_val,
                error_code: ErrorCode::InvalidHandle,
            }
        })?;

        // Read the attribute value from the database, capped to the maximum possible response size.
        let mut val_buf = [0u8; MAX_ATTRIBUTE_SIZE];
        let val_buf = &mut val_buf[..self.mtu() as usize - size_of::<Header>()];
        let read_len = attr.read_chunk(self.peer_id, 0, val_buf).await.map_err(|error_code| {
            TransactionError::ErrorResponse {
                request_opcode: Opcode::ReadReq,
                attribute_handle: handle_val,
                error_code,
            }
        })?;

        // Format and send the Read Response.
        let mut tx_buf = [0u8; MAX_SUPPORTED_MTU];
        let mut builder = DynamicPacketBuilder::<_, u8>::new(
            &mut tx_buf,
            Header { opcode: Opcode::ReadRsp },
            self.mtu() as usize,
        );
        builder.extend_from_slice(&val_buf[..read_len]).expect("read response fits within MTU");
        self.send_packet(builder.as_packet()).await?;

        Ok(())
    }

    async fn send_packet(&mut self, packet: &Packet) -> Result<(), ServerError> {
        match self.bearer_tx.send(packet).await {
            Ok(()) => Ok(()),
            // Channel disconnected. Terminate server.
            Err(BearerSendError::LinkClosed) => Err(ServerError::LinkClosed),
            // Outgoing packet size exceeds MTU. This is a local logic error.
            Err(BearerSendError::PacketTooLarge) => {
                panic!("Programming error: outgoing packet size exceeds the negotiated MTU.");
            }
        }
    }

    /// Formats and transmits an ATT Error Response PDU.
    ///
    /// Accepts the `request_opcode` as an `impl Into<u8>` (which fits both the typed `Opcode`
    /// enum and raw `u8` invalid opcodes) to satisfy the Bluetooth Specification requirements
    /// for error reporting on unknown/invalid opcodes.
    async fn send_error_response(
        &mut self,
        request_opcode: impl Into<u8>,
        attribute_handle: Option<AttributeHandle>,
        error_code: ErrorCode,
    ) -> Result<(), ServerError> {
        let handle_raw = attribute_handle.map(|h| h.value()).unwrap_or(0);
        let builder = PacketBuilder {
            header: Header { opcode: Opcode::ErrorRsp },
            payload: ErrorRsp {
                request_opcode: request_opcode.into(),
                attribute_handle: U16::new(handle_raw),
                error_code,
            },
        };
        let tx_packet = builder.as_packet();
        self.send_packet(tx_packet).await
    }

    pub fn mtu(&self) -> u16 {
        self.bearer_tx.mtu()
    }
}

/// Converts a raw 16-bit value into a valid `AttributeHandle`.
///
/// If `val` is invalid (i.e. `0x0000`), returns an ATT Error Response with
/// `ErrorCode::InvalidHandle` for the given request `opcode`.
///
/// see Bluetooth Core Spec v6.0 (Vol 3, Part F, Section 3.2.2).
fn to_handle(val: u16, opcode: Opcode) -> Result<AttributeHandle, TransactionError> {
    AttributeHandle::try_from(val).map_err(|_| TransactionError::ErrorResponse {
        request_opcode: opcode,
        attribute_handle: val,
        error_code: ErrorCode::InvalidHandle,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::att::attribute::testing::MockAttribute;
    use crate::att::database::testing::MockDb;
    use crate::att::l2cap::mock::setup_mock_channel;
    use crate::att::pdu::{FindByTypeValueReqHeader, FindInformationRsp, InformationData16};

    use sapphire_async::executor::BoundedExecutor;
    use sapphire_async::testing::TestExecutor;
    use sapphire_uuid::Uuid;
    use zerocopy::{IntoBytes, TryFromBytes};

    fn h(val: u16) -> AttributeHandle {
        AttributeHandle::try_from(val).unwrap()
    }

    const CLIENT_PREFERRED_MTU: u16 = 512;
    const SERVER_MTU: u16 = 256;
    const TEST_RX_BUF_SIZE: usize = 64;

    #[test]
    fn test_server_handle_mtu_exchange_success() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, test_tx, test_rx) = setup_mock_channel(executor);

            let mut server = Server::new(
                PeerId::new(1).unwrap(),
                BearerTx::new(test_tx),
                BearerRx::new(test_rx),
                SERVER_MTU,
                MockDb::new(),
            );

            // Spawn client driver task
            let client_handle = executor.spawn(async move {
                // Send ExchangeMtuReq requesting 512-byte MTU
                let builder = PacketBuilder {
                    header: Header { opcode: Opcode::ExchangeMtuReq },
                    payload: ExchangeMtuReq { client_rx_mtu: U16::new(CLIENT_PREFERRED_MTU) },
                };

                let tx_packet = builder.as_packet();
                let mut client_tx_bearer = BearerTx::new(app_channel.sender);
                client_tx_bearer.send(tx_packet).await.unwrap();

                // Receive ExchangeMtuRsp from server
                let mut rx_buf = [MaybeUninit::uninit(); 32];
                let mut client_rx_bearer = BearerRx::new(app_channel.receiver);
                let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ExchangeMtuRsp);

                let rsp = ExchangeMtuRsp::read_from_bytes(&packet.data[..]).unwrap();
                assert_eq!(rsp.server_rx_mtu.get(), SERVER_MTU);
            });

            // Spawn server driver task
            let server_handle = executor.spawn(async move {
                let res = server.run().await;
                assert_eq!(res, Err(ServerError::LinkClosed));
                assert_eq!(server.mtu(), SERVER_MTU);
            });

            executor.run_until_stalled();

            assert!(client_handle.is_finished());
            assert!(server_handle.is_finished());
        });
    }

    #[test]
    fn test_server_handles_unsupported_request_and_continues() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, test_tx, test_rx) = setup_mock_channel(executor);

            let mut server = Server::new(
                PeerId::new(1).unwrap(),
                BearerTx::new(test_tx),
                BearerRx::new(test_rx),
                SERVER_MTU,
                MockDb::new(),
            );

            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 32];
                let mut client_rx_bearer = BearerRx::new(app_channel.receiver);
                let mut client_tx_bearer = BearerTx::new(app_channel.sender);

                // 1. Send ExchangeMtuRsp (0x03) as a request (valid opcode but unsupported request)
                let builder = PacketBuilder {
                    header: Header { opcode: Opcode::ExchangeMtuRsp },
                    payload: ExchangeMtuRsp { server_rx_mtu: U16::new(SERVER_MTU) },
                };
                let tx_packet = builder.as_packet();
                client_tx_bearer.send(tx_packet).await.unwrap();

                // Expect ErrorRsp indicating RequestNotSupported (0x06) for ExchangeMtuRsp (0x03)
                let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ErrorRsp);
                let err = ErrorRsp::try_read_from_bytes(&packet.data[..]).unwrap();
                assert_eq!(err.request_opcode, Opcode::ExchangeMtuRsp as u8);
                assert_eq!(err.error_code, ErrorCode::RequestNotSupported);

                // 2. Server should still be running! Send valid ExchangeMtuReq (0x02)
                let builder = PacketBuilder {
                    header: Header { opcode: Opcode::ExchangeMtuReq },
                    payload: ExchangeMtuReq { client_rx_mtu: U16::new(CLIENT_PREFERRED_MTU) },
                };
                let tx_packet = builder.as_packet();
                client_tx_bearer.send(tx_packet).await.unwrap();

                // Expect ExchangeMtuRsp from server
                let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ExchangeMtuRsp);
                let rsp = ExchangeMtuRsp::read_from_bytes(&packet.data[..]).unwrap();
                assert_eq!(rsp.server_rx_mtu.get(), SERVER_MTU);
            });

            let server_handle = executor.spawn(async move {
                let res = server.run().await;
                assert_eq!(res, Err(ServerError::LinkClosed));
            });

            executor.run_until_stalled();

            assert!(client_handle.is_finished());
            assert!(server_handle.is_finished());
        });
    }

    #[test]
    fn test_server_handles_invalid_payload_and_continues() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, test_tx, test_rx) = setup_mock_channel(executor);

            let mut server = Server::new(
                PeerId::new(1).unwrap(),
                BearerTx::new(test_tx),
                BearerRx::new(test_rx),
                SERVER_MTU,
                MockDb::new(),
            );

            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 32];
                let mut client_rx_bearer = BearerRx::new(app_channel.receiver);
                let mut client_tx_bearer = BearerTx::new(app_channel.sender);

                // 1. Send ExchangeMtuReq but with truncated (empty) payload
                let tx_packet =
                    Packet::try_ref_from_bytes(&[Opcode::ExchangeMtuReq as u8]).unwrap();
                client_tx_bearer.send(tx_packet).await.unwrap();

                // Expect ErrorRsp indicating InvalidPdu (0x04) for ExchangeMtuReq (0x02)
                let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ErrorRsp);
                let err = ErrorRsp::try_read_from_bytes(&packet.data[..]).unwrap();
                assert_eq!(err.request_opcode, Opcode::ExchangeMtuReq as u8);
                assert_eq!(err.error_code, ErrorCode::InvalidPdu);
            });

            let server_handle = executor.spawn(async move {
                let res = server.run().await;
                assert_eq!(res, Err(ServerError::LinkClosed));
            });

            executor.run_until_stalled();

            assert!(client_handle.is_finished());
            assert!(server_handle.is_finished());
        });
    }

    #[test]
    fn test_server_handles_large_packet_and_continues() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, test_tx, test_rx) = setup_mock_channel(executor);

            let mut server = Server::new(
                PeerId::new(1).unwrap(),
                BearerTx::new(test_tx),
                BearerRx::new(test_rx),
                SERVER_MTU,
                MockDb::new(),
            );

            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 32];
                let mut client_rx_bearer = BearerRx::new(app_channel.receiver);
                let mut sender = app_channel.sender;

                // 1. Bypass BearerTx and send a packet larger than MAX_SUPPORTED_MTU (519) directly over L2CAP
                let large_packet = [0u8; 600];
                sender.send(&large_packet).await.unwrap();

                // Expect ErrorRsp indicating InvalidPdu for opcode 0x00
                let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ErrorRsp);
                let err = ErrorRsp::try_read_from_bytes(&packet.data[..]).unwrap();
                assert_eq!(err.request_opcode, 0x00);
                assert_eq!(err.error_code, ErrorCode::InvalidPdu);

                // 2. Server should log the error and stay alive. Send a valid ExchangeMtuReq.
                let builder = PacketBuilder {
                    header: Header { opcode: Opcode::ExchangeMtuReq },
                    payload: ExchangeMtuReq { client_rx_mtu: U16::new(CLIENT_PREFERRED_MTU) },
                };
                let tx_packet = builder.as_packet();
                sender.send(tx_packet.as_bytes()).await.unwrap();

                // Expect ExchangeMtuRsp
                let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ExchangeMtuRsp);
                let rsp = ExchangeMtuRsp::read_from_bytes(&packet.data[..]).unwrap();
                assert_eq!(rsp.server_rx_mtu.get(), SERVER_MTU);
            });

            let server_handle = executor.spawn(async move {
                let res = server.run().await;
                assert_eq!(res, Err(ServerError::LinkClosed));
            });

            executor.run_until_stalled();

            assert!(client_handle.is_finished());
            assert!(server_handle.is_finished());
        });
    }

    #[test]
    fn test_server_handles_invalid_opcode() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, test_tx, test_rx) = setup_mock_channel(executor);

            let mut server = Server::new(
                PeerId::new(1).unwrap(),
                BearerTx::new(test_tx),
                BearerRx::new(test_rx),
                SERVER_MTU,
                MockDb::new(),
            );

            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 32];
                let mut client_rx_bearer = BearerRx::new(app_channel.receiver);
                let mut sender = app_channel.sender;

                // Send a packet with raw invalid opcode 0x99
                sender.send(&[0x99, 0x01, 0x02]).await.unwrap();

                // Expect ErrorRsp indicating RequestNotSupported for request opcode 0x99
                let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ErrorRsp);
                let err = ErrorRsp::try_read_from_bytes(&packet.data[..]).unwrap();
                assert_eq!(err.request_opcode, 0x99);
                assert_eq!(err.error_code, ErrorCode::RequestNotSupported);
            });

            let server_handle = executor.spawn(async move {
                let res = server.run().await;
                assert_eq!(res, Err(ServerError::LinkClosed));
            });

            executor.run_until_stalled();
            assert!(client_handle.is_finished());
            assert!(server_handle.is_finished());
        });
    }

    #[test]
    fn test_server_handles_exceeding_mtu_packet() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, test_tx, test_rx) = setup_mock_channel(executor);

            let mut server = Server::new(
                PeerId::new(1).unwrap(),
                BearerTx::new(test_tx),
                BearerRx::new(test_rx),
                SERVER_MTU,
                MockDb::new(),
            );

            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 64];
                let mut client_rx_bearer = BearerRx::new(app_channel.receiver);
                let mut sender = app_channel.sender;

                // Default MTU is 23. Send a 30-byte packet directly over L2CAP.
                let mut oversized = [0u8; 30];
                oversized[0] = Opcode::ExchangeMtuReq.into();
                sender.send(&oversized).await.unwrap();

                // Expect ErrorRsp indicating InvalidPdu for request opcode 0x02
                let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ErrorRsp);
                let err = ErrorRsp::try_read_from_bytes(&packet.data[..]).unwrap();
                assert_eq!(err.request_opcode, Opcode::ExchangeMtuReq as u8);
                assert_eq!(err.error_code, ErrorCode::InvalidPdu);
            });

            let server_handle = executor.spawn(async move {
                let res = server.run().await;
                assert_eq!(res, Err(ServerError::LinkClosed));
            });

            executor.run_until_stalled();
            assert!(client_handle.is_finished());
            assert!(server_handle.is_finished());
        });
    }

    #[test]
    fn test_server_handle_find_information_success() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, test_tx, test_rx) = setup_mock_channel(executor);

            let mut db = MockDb::new();
            let name_attr = MockAttribute::new(Uuid::from_u16(0x2A00), b"Sunstone"); // handle 1
            let custom_uuid =
                Uuid::from_le_bytes([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
            let custom_attr = MockAttribute::new(custom_uuid, b"Custom"); // handle 2
            db.insert(h(1), name_attr);
            db.insert(h(2), custom_attr);

            let mut server = Server::new(
                PeerId::new(1).unwrap(),
                BearerTx::new(test_tx),
                BearerRx::new(test_rx),
                SERVER_MTU,
                db,
            );

            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 64];
                let mut client_rx_bearer = BearerRx::new(app_channel.receiver);
                let mut client_tx_bearer = BearerTx::new(app_channel.sender);

                // 1. Send FindInformationReq for 1..=2
                let builder = PacketBuilder {
                    header: Header { opcode: Opcode::FindInformationReq },
                    payload: FindInformationReq {
                        starting_handle: U16::new(1),
                        ending_handle: U16::new(2),
                    },
                };
                client_tx_bearer.send(builder.as_packet()).await.unwrap();

                // Expect FindInformationRsp with only handle 1 (since handle 2 has a different format)
                let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::FindInformationRsp);

                let rsp =
                    FindInformationRsp::<InformationData16>::try_ref_from_bytes(&packet.data[..])
                        .unwrap();
                assert_eq!(rsp.format, UuidFormat::Uuid16);
                assert_eq!(rsp.info.len(), 1);
                assert_eq!(rsp.info[0].handle.get(), 1);
                assert_eq!(rsp.info[0].uuid, [0x00, 0x2a]);

                // 2. Send FindInformationReq for 1..=0xFFFF (querying past end of database)
                let builder = PacketBuilder {
                    header: Header { opcode: Opcode::FindInformationReq },
                    payload: FindInformationReq {
                        starting_handle: U16::new(1),
                        ending_handle: U16::new(0xFFFF),
                    },
                };
                client_tx_bearer.send(builder.as_packet()).await.unwrap();

                // Expect the same FindInformationRsp containing only handle 1
                let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::FindInformationRsp);

                let rsp =
                    FindInformationRsp::<InformationData16>::try_ref_from_bytes(&packet.data[..])
                        .unwrap();
                assert_eq!(rsp.format, UuidFormat::Uuid16);
                assert_eq!(rsp.info.len(), 1);
                assert_eq!(rsp.info[0].handle.get(), 1);
                assert_eq!(rsp.info[0].uuid, [0x00, 0x2a]);
            });

            let server_handle = executor.spawn(async move {
                let res = server.run().await;
                assert_eq!(res, Err(ServerError::LinkClosed));
            });

            executor.run_until_stalled();

            assert!(client_handle.is_finished());
            assert!(server_handle.is_finished());
        });
    }

    #[test]
    fn test_server_handle_find_information_errors() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, test_tx, test_rx) = setup_mock_channel(executor);

            let mut db = MockDb::new();
            let name_attr = MockAttribute::new(Uuid::from_u16(0x2A00), b"Sunstone"); // handle 1
            db.insert(h(1), name_attr);

            let mut server = Server::new(
                PeerId::new(1).unwrap(),
                BearerTx::new(test_tx),
                BearerRx::new(test_rx),
                SERVER_MTU,
                db,
            );

            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 64];
                let mut client_rx_bearer = BearerRx::new(app_channel.receiver);
                let mut client_tx_bearer = BearerTx::new(app_channel.sender);

                // 1. Invalid Handle (start = 0)
                let builder = PacketBuilder {
                    header: Header { opcode: Opcode::FindInformationReq },
                    payload: FindInformationReq {
                        starting_handle: U16::new(0),
                        ending_handle: U16::new(2),
                    },
                };
                client_tx_bearer.send(builder.as_packet()).await.unwrap();

                let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ErrorRsp);
                let err = ErrorRsp::try_read_from_bytes(&packet.data[..]).unwrap();
                assert_eq!(err.request_opcode, Opcode::FindInformationReq as u8);
                assert_eq!(err.attribute_handle.get(), 0);
                assert_eq!(err.error_code, ErrorCode::InvalidHandle);

                // 2. Invalid Handle (start > end)
                let builder = PacketBuilder {
                    header: Header { opcode: Opcode::FindInformationReq },
                    payload: FindInformationReq {
                        starting_handle: U16::new(3),
                        ending_handle: U16::new(2),
                    },
                };
                client_tx_bearer.send(builder.as_packet()).await.unwrap();

                let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ErrorRsp);
                let err = ErrorRsp::try_read_from_bytes(&packet.data[..]).unwrap();
                assert_eq!(err.request_opcode, Opcode::FindInformationReq as u8);
                assert_eq!(err.attribute_handle.get(), 3);
                assert_eq!(err.error_code, ErrorCode::InvalidHandle);

                // 3. Attribute Not Found (no attributes in 5..=10)
                let builder = PacketBuilder {
                    header: Header { opcode: Opcode::FindInformationReq },
                    payload: FindInformationReq {
                        starting_handle: U16::new(5),
                        ending_handle: U16::new(10),
                    },
                };
                client_tx_bearer.send(builder.as_packet()).await.unwrap();

                let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ErrorRsp);
                let err = ErrorRsp::try_read_from_bytes(&packet.data[..]).unwrap();
                assert_eq!(err.request_opcode, Opcode::FindInformationReq as u8);
                assert_eq!(err.attribute_handle.get(), 5);
                assert_eq!(err.error_code, ErrorCode::AttributeNotFound);
            });

            let server_handle = executor.spawn(async move {
                let res = server.run().await;
                assert_eq!(res, Err(ServerError::LinkClosed));
            });

            executor.run_until_stalled();

            assert!(client_handle.is_finished());
            assert!(server_handle.is_finished());
        });
    }

    #[test]
    fn test_server_handle_find_by_type_value_success() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, test_tx, test_rx) = setup_mock_channel(executor);

            let mut db = MockDb::new();
            // Handle 1: Primary Service (0x2800) with value 0x180D (Heart Rate), ends at 5
            let svc_attr = MockAttribute::new_grouped(Uuid::from_u16(0x2800), &[0x0D, 0x18], 5);
            // Handle 6: Primary Service (0x2800) with value 0x180F (Battery Service), ends at 8
            let svc_attr2 = MockAttribute::new_grouped(Uuid::from_u16(0x2800), &[0x0F, 0x18], 8);
            db.insert(h(1), svc_attr);
            db.insert(h(6), svc_attr2);

            let mut server = Server::new(
                PeerId::new(1).unwrap(),
                BearerTx::new(test_tx),
                BearerRx::new(test_rx),
                SERVER_MTU,
                db,
            );

            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 64];
                let mut client_rx_bearer = BearerRx::new(app_channel.receiver);
                let mut client_tx_bearer = BearerTx::new(app_channel.sender);

                // Send request for 0x2800 with value 0x180D
                let header = PacketBuilder {
                    header: Header { opcode: Opcode::FindByTypeValueReq },
                    payload: FindByTypeValueReqHeader {
                        starting_handle: U16::new(1),
                        ending_handle: U16::new(10),
                        attribute_type: U16::new(0x2800),
                    },
                };
                let mut tx_buf = [0u8; 64];
                let mut builder = DynamicPacketBuilder::<_, u8>::new(
                    &mut tx_buf,
                    header,
                    CLIENT_PREFERRED_MTU as usize,
                );
                builder.extend_from_slice(&[0x0D, 0x18]).unwrap();
                client_tx_bearer.send(builder.as_packet()).await.unwrap();

                // Expect FindByTypeValueRsp with entry [1, 5]
                let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::FindByTypeValueRsp);
                let entries = <[HandlesInformation]>::ref_from_bytes(&packet.data[..]).unwrap();
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].attribute_handle.get(), 1);
                assert_eq!(entries[0].group_end_handle.get(), 5);
            });

            let server_handle = executor.spawn(async move {
                let res = server.run().await;
                assert_eq!(res, Err(ServerError::LinkClosed));
            });

            executor.run_until_stalled();
            assert!(client_handle.is_finished());
            assert!(server_handle.is_finished());
        });
    }

    #[test]
    fn test_server_handle_find_by_type_value_errors() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, test_tx, test_rx) = setup_mock_channel(executor);

            let mut db = MockDb::new();
            let svc_attr = MockAttribute::new_grouped(Uuid::from_u16(0x2800), &[0x0D, 0x18], 5);
            db.insert(h(1), svc_attr);

            let mut server = Server::new(
                PeerId::new(1).unwrap(),
                BearerTx::new(test_tx),
                BearerRx::new(test_rx),
                SERVER_MTU,
                db,
            );

            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 64];
                let mut client_rx_bearer = BearerRx::new(app_channel.receiver);
                let mut client_tx_bearer = BearerTx::new(app_channel.sender);

                // 1. Attribute Not Found (value mismatch 0x180F)
                let header = PacketBuilder {
                    header: Header { opcode: Opcode::FindByTypeValueReq },
                    payload: FindByTypeValueReqHeader {
                        starting_handle: U16::new(1),
                        ending_handle: U16::new(10),
                        attribute_type: U16::new(0x2800),
                    },
                };
                let mut tx_buf = [0u8; 64];
                let mut builder = DynamicPacketBuilder::<_, u8>::new(
                    &mut tx_buf,
                    header,
                    CLIENT_PREFERRED_MTU as usize,
                );
                builder.extend_from_slice(&[0x0F, 0x18]).unwrap();
                client_tx_bearer.send(builder.as_packet()).await.unwrap();

                let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ErrorRsp);
                let err = ErrorRsp::try_read_from_bytes(&packet.data[..]).unwrap();
                assert_eq!(err.request_opcode, Opcode::FindByTypeValueReq as u8);
                assert_eq!(err.error_code, ErrorCode::AttributeNotFound);
            });

            let server_handle = executor.spawn(async move {
                let res = server.run().await;
                assert_eq!(res, Err(ServerError::LinkClosed));
            });

            executor.run_until_stalled();
            assert!(client_handle.is_finished());
            assert!(server_handle.is_finished());
        });
    }

    #[test]
    fn test_server_handle_read_success() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, test_tx, test_rx) = setup_mock_channel(executor);

            let mut db = MockDb::new();
            let name_attr = MockAttribute::new(Uuid::from_u16(0x2A00), b"Sunstone");
            db.insert(h(1), name_attr);

            let mut server = Server::new(
                PeerId::new(1).unwrap(),
                BearerTx::new(test_tx),
                BearerRx::new(test_rx),
                SERVER_MTU,
                db,
            );

            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 64];
                let mut client_rx_bearer = BearerRx::new(app_channel.receiver);
                let mut client_tx_bearer = BearerTx::new(app_channel.sender);

                // Send ReadReq for handle 1
                let builder = PacketBuilder {
                    header: Header { opcode: Opcode::ReadReq },
                    payload: ReadReq { attribute_handle: U16::new(1) },
                };
                client_tx_bearer.send(builder.as_packet()).await.unwrap();

                // Expect ReadRsp containing "Sunstone"
                let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ReadRsp);
                let expected: &[u8] = b"Sunstone";
                assert_eq!(packet.data, *expected);
            });

            let server_handle = executor.spawn(async move {
                let res = server.run().await;
                assert_eq!(res, Err(ServerError::LinkClosed));
            });

            executor.run_until_stalled();
            assert!(client_handle.is_finished());
            assert!(server_handle.is_finished());
        });
    }

    #[test]
    fn test_server_handle_read_invalid_handle() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, test_tx, test_rx) = setup_mock_channel(executor);

            let mut db = MockDb::new();
            let name_attr = MockAttribute::new(Uuid::from_u16(0x2A00), b"Sunstone");
            db.insert(h(1), name_attr);

            let mut server = Server::new(
                PeerId::new(1).unwrap(),
                BearerTx::new(test_tx),
                BearerRx::new(test_rx),
                SERVER_MTU,
                db,
            );

            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); TEST_RX_BUF_SIZE];
                let mut client_rx_bearer = BearerRx::new(app_channel.receiver);
                let mut client_tx_bearer = BearerTx::new(app_channel.sender);

                // Read Request for handle 0 (invalid handle value)
                let builder = PacketBuilder {
                    header: Header { opcode: Opcode::ReadReq },
                    payload: ReadReq { attribute_handle: U16::new(0) },
                };
                client_tx_bearer.send(builder.as_packet()).await.unwrap();

                // Expect ErrorRsp indicating InvalidHandle for handle 0
                let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ErrorRsp);
                let err = ErrorRsp::try_read_from_bytes(&packet.data[..]).unwrap();
                assert_eq!(err.request_opcode, Opcode::ReadReq as u8);
                assert_eq!(err.attribute_handle.get(), 0);
                assert_eq!(err.error_code, ErrorCode::InvalidHandle);
            });

            let server_handle = executor.spawn(async move {
                let res = server.run().await;
                assert_eq!(res, Err(ServerError::LinkClosed));
            });

            executor.run_until_stalled();
            assert!(client_handle.is_finished());
            assert!(server_handle.is_finished());
        });
    }

    #[test]
    fn test_server_handle_read_attribute_not_found() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, test_tx, test_rx) = setup_mock_channel(executor);

            let mut db = MockDb::new();
            let name_attr = MockAttribute::new(Uuid::from_u16(0x2A00), b"Sunstone");
            db.insert(h(1), name_attr);

            let mut server = Server::new(
                PeerId::new(1).unwrap(),
                BearerTx::new(test_tx),
                BearerRx::new(test_rx),
                SERVER_MTU,
                db,
            );

            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); TEST_RX_BUF_SIZE];
                let mut client_rx_bearer = BearerRx::new(app_channel.receiver);
                let mut client_tx_bearer = BearerTx::new(app_channel.sender);

                // Read Request for handle 99 (non-existent handle)
                let builder = PacketBuilder {
                    header: Header { opcode: Opcode::ReadReq },
                    payload: ReadReq { attribute_handle: U16::new(99) },
                };
                client_tx_bearer.send(builder.as_packet()).await.unwrap();

                // Expect ErrorRsp indicating InvalidHandle for handle 99
                let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ErrorRsp);
                let err = ErrorRsp::try_read_from_bytes(&packet.data[..]).unwrap();
                assert_eq!(err.request_opcode, Opcode::ReadReq as u8);
                assert_eq!(err.attribute_handle.get(), 99);
                assert_eq!(err.error_code, ErrorCode::InvalidHandle);
            });

            let server_handle = executor.spawn(async move {
                let res = server.run().await;
                assert_eq!(res, Err(ServerError::LinkClosed));
            });

            executor.run_until_stalled();
            assert!(client_handle.is_finished());
            assert!(server_handle.is_finished());
        });
    }

    #[test]
    fn test_server_handle_read_truncated() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, test_tx, test_rx) = setup_mock_channel(executor);

            let mut db = MockDb::new();
            // 30-byte long value
            let long_val = b"012345678901234567890123456789";
            let name_attr = MockAttribute::new(Uuid::from_u16(0x2A00), long_val);
            db.insert(h(1), name_attr);

            // Set server MTU to 23 bytes
            let mut server = Server::new(
                PeerId::new(1).unwrap(),
                BearerTx::new(test_tx),
                BearerRx::new(test_rx),
                23,
                db,
            );

            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 64];
                let mut client_rx_bearer = BearerRx::new(app_channel.receiver);
                let mut client_tx_bearer = BearerTx::new(app_channel.sender);

                // Send ReadReq for handle 1
                let builder = PacketBuilder {
                    header: Header { opcode: Opcode::ReadReq },
                    payload: ReadReq { attribute_handle: U16::new(1) },
                };
                client_tx_bearer.send(builder.as_packet()).await.unwrap();

                // Expect ReadRsp containing first 22 bytes of long_val (MTU - 1)
                let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ReadRsp);
                let expected: &[u8] = b"0123456789012345678901";
                assert_eq!(packet.data, *expected);
            });

            let server_handle = executor.spawn(async move {
                let res = server.run().await;
                assert_eq!(res, Err(ServerError::LinkClosed));
            });

            executor.run_until_stalled();
            assert!(client_handle.is_finished());
            assert!(server_handle.is_finished());
        });
    }
}
