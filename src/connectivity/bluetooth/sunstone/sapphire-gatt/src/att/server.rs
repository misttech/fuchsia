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
    InformationData16, InformationData128, Opcode, Packet, PacketBuilder, ReadBlobReq,
    ReadByGroupTypeReq, ReadByGroupTypeRspEntryHeader, ReadByTypeReq, ReadReq, UuidFormat,
    WriteReq, WriteRsp,
};
use core::cmp::{max, min};
use core::convert::Infallible;
use core::mem::{MaybeUninit, size_of};
use sapphire_peer_cache::PeerId;
use sapphire_uuid::Uuid;
use thiserror::Error;
use zerocopy::byteorder::little_endian::U16;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, TryFromBytes};

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

/// A trait for common fields in ATT range request PDUs (Read By Type and Read By Group Type).
trait RangeRequest: TryFromBytes + Immutable + KnownLayout {
    fn starting_handle(&self) -> u16;
    fn ending_handle(&self) -> u16;
    fn attribute_type(&self) -> &[u8];
}

impl RangeRequest for ReadByTypeReq {
    fn starting_handle(&self) -> u16 {
        self.header.starting_handle.get()
    }
    fn ending_handle(&self) -> u16 {
        self.header.ending_handle.get()
    }
    fn attribute_type(&self) -> &[u8] {
        &self.attribute_type
    }
}

impl RangeRequest for ReadByGroupTypeReq {
    fn starting_handle(&self) -> u16 {
        self.header.starting_handle.get()
    }
    fn ending_handle(&self) -> u16 {
        self.header.ending_handle.get()
    }
    fn attribute_type(&self) -> &[u8] {
        &self.attribute_type
    }
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
            Opcode::ReadBlobReq => self.handle_read_blob(&rx_packet.data).await,
            Opcode::ReadByTypeReq => self.handle_read_by_type(&rx_packet.data).await,
            Opcode::ReadByGroupTypeReq => self.handle_read_by_group_type(&rx_packet.data).await,
            Opcode::WriteReq => self.handle_write_req(&rx_packet.data).await,
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
                self.effective_mtu(),
                attributes,
            ),
            UuidFormat::Uuid128 => Self::pack_find_info_rsp::<InformationData128>(
                &mut tx_buf,
                header,
                self.effective_mtu(),
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
            self.effective_mtu(),
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

        // Read the attribute value from the database, capped to the maximum possible response size.
        let mut val_buf = [0u8; MAX_ATTRIBUTE_SIZE];
        let response_capacity = self.effective_mtu() - size_of::<Header>();
        let read_len = self
            .read_attribute_at(handle_val, 0, Opcode::ReadReq, &mut val_buf[..response_capacity])
            .await?;

        // Format and send the Read Response.
        let mut tx_buf = [0u8; MAX_SUPPORTED_MTU];
        let mut builder = DynamicPacketBuilder::<_, u8>::new(
            &mut tx_buf,
            Header { opcode: Opcode::ReadRsp },
            self.effective_mtu(),
        );
        builder.extend_from_slice(&val_buf[..read_len]).expect("read response fits within MTU");
        self.send_packet(builder.as_packet()).await?;

        Ok(())
    }

    /// Handles an incoming Read Blob Request and responds with a Read Blob Response.
    ///
    /// see Bluetooth Core Spec v6.0 (Vol 3, Part F, Section 3.4.4.3 & 3.4.4.4)
    async fn handle_read_blob(&mut self, data: &[u8]) -> Result<(), TransactionError> {
        // Parse the incoming Read Blob Request.
        let req = ReadBlobReq::read_from_bytes(data)
            .map_err(|_| TransactionError::InvalidPdu { request_opcode: Opcode::ReadBlobReq })?;

        let handle_val = req.attribute_handle.get();
        let offset = req.value_offset.get();

        // Read the attribute chunk starting from the requested offset, capped to the response size.
        let mut val_buf = [0u8; MAX_ATTRIBUTE_SIZE];
        let response_capacity = self.effective_mtu() - size_of::<Header>();
        let read_len = self
            .read_attribute_at(
                handle_val,
                offset,
                Opcode::ReadBlobReq,
                &mut val_buf[..response_capacity],
            )
            .await?;

        // Format and send the Read Blob Response.
        let mut tx_buf = [0u8; MAX_SUPPORTED_MTU];
        let mut builder = DynamicPacketBuilder::<_, u8>::new(
            &mut tx_buf,
            Header { opcode: Opcode::ReadBlobRsp },
            self.effective_mtu(),
        );
        builder
            .extend_from_slice(&val_buf[..read_len])
            .expect("read blob response fits within MTU");
        self.send_packet(builder.as_packet()).await?;

        Ok(())
    }

    /// Shares range query execution logic for Read By Type and Read By Group Type requests.
    ///
    /// Senders format entry headers into a buffer using `write_entry_header` (returning Err on
    /// failure), which determines the first entry's header size
    /// before the output packet builder is initialized.
    ///
    /// see Bluetooth Core Spec v6.0 (Vol 3, Part F, Sections 3.4.4.7 to 3.4.4.10).
    async fn handle_read_by_type_generic<Req>(
        &mut self,
        payload: &[u8],
        req_opcode: Opcode,
        rsp_opcode: Opcode,
        mut write_entry_header: impl FnMut(
            &mut [u8],
            AttributeHandle,
            &DB::Attr,
        ) -> Result<usize, ErrorCode>,
    ) -> Result<(), TransactionError>
    where
        Req: RangeRequest + ?Sized,
    {
        // Extract common request parameters (starting handle, ending handle, and search UUID).
        let req = Req::try_ref_from_bytes(payload)
            .map_err(|_| TransactionError::InvalidPdu { request_opcode: req_opcode })?;

        let start_handle_val = req.starting_handle();
        let end_handle_val = req.ending_handle();

        let start_handle = to_handle(start_handle_val, req_opcode)?;
        let end_handle = to_handle(end_handle_val, req_opcode)?;

        if start_handle > end_handle {
            return Err(TransactionError::ErrorResponse {
                request_opcode: req_opcode,
                attribute_handle: start_handle_val,
                error_code: ErrorCode::InvalidHandle,
            });
        }

        let uuid = Uuid::try_from(req.attribute_type())
            .map_err(|_| TransactionError::InvalidPdu { request_opcode: req_opcode })?;

        const MAX_ENTRY_HEADER_SIZE: usize = size_of::<ReadByGroupTypeRspEntryHeader>();

        // Query range for matching attributes.
        let mut attributes = self
            .database
            .query_range(start_handle, end_handle)
            .filter(|(_, attr)| attr.uuid() == &uuid)
            .peekable();

        if attributes.peek().is_none() {
            return Err(TransactionError::ErrorResponse {
                request_opcode: req_opcode,
                attribute_handle: start_handle_val,
                error_code: ErrorCode::AttributeNotFound,
            });
        }

        // We must read the first attribute to establish the element length
        // before initializing the builder, so that the header is populated correctly.
        let (first_handle, first_attr) = attributes.next().unwrap();

        // Pre-validate grouping type constraints on the first matched attribute.
        let mut first_entry_header_buf = [0u8; MAX_ENTRY_HEADER_SIZE];
        let first_entry_header_len =
            match write_entry_header(&mut first_entry_header_buf, first_handle, first_attr) {
                Ok(len) => len,
                Err(error_code) => {
                    return Err(TransactionError::ErrorResponse {
                        request_opcode: req_opcode,
                        attribute_handle: start_handle_val,
                        error_code,
                    });
                }
            };

        // The Length field is 1 byte, so the maximum size of an entry is u8::MAX (255).
        let limit = self.effective_mtu();

        // Since each entry must contain the entry header,
        // the maximum value length is u8::MAX - entry header size.
        let max_value_len = u8::MAX as usize - first_entry_header_len;
        let min_payload_size = size_of::<Header>() + size_of::<u8>() + first_entry_header_len;

        // If the attribute value is longer than the remaining MTU space
        // or the max possible entry size, only the first chunk is read in this response
        //
        // (see Bluetooth Core Spec v6.0, Vol 3, Part F, Section 3.4.4.8 for Read By Type,
        // and Section 3.4.4.10 for Read By Group Type).
        //
        // Note: since the minimum ATT MTU is 23, `limit` is guaranteed to be larger than
        // `min_payload_size` (at most 6), so `first_read_limit` is always > 0.
        let first_read_limit = min(limit.saturating_sub(min_payload_size), max_value_len);
        debug_assert_ne!(first_read_limit, 0);

        let mut first_val_buf = [0u8; MAX_ATTRIBUTE_SIZE];
        let first_read_len = first_attr
            .read_chunk(self.peer_id, 0, &mut first_val_buf[..first_read_limit])
            .await
            .map_err(|error_code| TransactionError::ErrorResponse {
                request_opcode: req_opcode,
                attribute_handle: first_handle.value(),
                error_code,
            })?;

        // Initialize the output packet builder with the correct header length.
        let header = PacketBuilder {
            header: Header { opcode: rsp_opcode },
            payload: u8::try_from(first_entry_header_len + first_read_len)
                .expect("Range response payload length fits in u8"),
        };
        let mut tx_buf = [0u8; MAX_SUPPORTED_MTU];
        let mut builder = DynamicPacketBuilder::<_, u8>::new(&mut tx_buf, header, limit);

        builder
            .extend_from_slice(&first_entry_header_buf[..first_entry_header_len])
            .expect("First entry header should fit within allocated buffer");
        builder
            .extend_from_slice(&first_val_buf[..first_read_len])
            .expect("First entry value should fit within allocated buffer");

        // Pack matching attributes into response.
        for (handle, attr) in attributes {
            let mut entry_header_buf = [0u8; MAX_ENTRY_HEADER_SIZE];
            let entry_header_len = match write_entry_header(&mut entry_header_buf, handle, attr) {
                // Formatting succeeded; return the formatted header length.
                Ok(len) => len,
                // Formatting failed (e.g., unsupported group type); stop packing and
                // return the response accumulated so far.
                Err(_) => break,
            };

            if builder.len() + entry_header_len + first_read_len > limit {
                break;
            }

            let mut val_buf = [0u8; MAX_ATTRIBUTE_SIZE];
            // Read first_read_len + 1 to detect if the value is actually longer.
            // Slicing to exactly first_read_len would hide length mismatches.
            let Ok(read_len) =
                attr.read_chunk(self.peer_id, 0, &mut val_buf[..first_read_len + 1]).await
            else {
                // If reading a subsequent attribute fails, gracefully stop packing and
                // return the successful entries.
                break;
            };
            if read_len != first_read_len {
                // All entries in the response list must have the same length. If a subsequent
                // value's size differs, stop packing.
                break;
            }
            builder
                .extend_from_slice(&entry_header_buf[..entry_header_len])
                .expect("Subsequent entry header should fit based on length check");
            builder
                .extend_from_slice(&val_buf[..read_len])
                .expect("Subsequent entry value should fit based on length check");
        }

        self.send_packet(builder.as_packet()).await?;
        Ok(())
    }

    async fn handle_read_by_type(&mut self, payload: &[u8]) -> Result<(), TransactionError> {
        self.handle_read_by_type_generic::<ReadByTypeReq>(
            payload,
            Opcode::ReadByTypeReq,
            Opcode::ReadByTypeRsp,
            |buf, handle, _attr| {
                let handle_size = size_of::<AttributeHandle>();
                buf[..handle_size].copy_from_slice(&handle.value().to_le_bytes());
                Ok(handle_size)
            },
        )
        .await
    }

    /// Handles a Read By Group Type Request, querying the database for grouped attributes
    /// matching the given UUID group type and handle range, and returning a packed list
    /// of handles, end group handles, and values.
    ///
    /// see Bluetooth Core Spec v6.0 (Vol 3, Part F, Sections 3.4.4.9 & 3.4.4.10).
    async fn handle_read_by_group_type(&mut self, payload: &[u8]) -> Result<(), TransactionError> {
        self.handle_read_by_type_generic::<ReadByGroupTypeReq>(
            payload,
            Opcode::ReadByGroupTypeReq,
            Opcode::ReadByGroupTypeRsp,
            |buf, handle, attr| {
                let group_end = attr.group_end_handle().ok_or(ErrorCode::UnsupportedGroupType)?;
                let header = ReadByGroupTypeRspEntryHeader {
                    attribute_handle: U16::new(handle.value()),
                    end_group_handle: U16::new(group_end),
                };
                let header_size = size_of::<ReadByGroupTypeRspEntryHeader>();
                buf[..header_size].copy_from_slice(header.as_bytes());
                Ok(header_size)
            },
        )
        .await
    }

    /// Handles a Write Request, invoking a database write operation and returning an empty Write Response.
    ///
    /// see Bluetooth Core Spec v6.0 (Vol 3, Part F, Sections 3.4.5.1 & 3.4.5.2).
    async fn handle_write_req(&mut self, payload: &[u8]) -> Result<(), TransactionError> {
        let req = WriteReq::try_ref_from_bytes(payload)
            .map_err(|_| TransactionError::InvalidPdu { request_opcode: Opcode::WriteReq })?;

        let handle_val = req.header.attribute_handle.get();
        let handle = to_handle(handle_val, Opcode::WriteReq)?;

        let attr = self.database.find_attribute(handle).ok_or_else(|| {
            TransactionError::ErrorResponse {
                request_opcode: Opcode::WriteReq,
                attribute_handle: handle_val,
                error_code: ErrorCode::InvalidHandle,
            }
        })?;

        attr.write_chunk(self.peer_id, 0, &req.attribute_value).await.map_err(|error_code| {
            TransactionError::ErrorResponse {
                request_opcode: Opcode::WriteReq,
                attribute_handle: handle_val,
                error_code,
            }
        })?;

        let builder =
            PacketBuilder { header: Header { opcode: Opcode::WriteRsp }, payload: WriteRsp };
        self.send_packet(builder.as_packet()).await?;

        Ok(())
    }

    /// Helper to read an attribute value at an offset and cap it to a maximum buffer size.
    async fn read_attribute_at(
        &self,
        handle_val: u16,
        offset: u16,
        request_opcode: Opcode,
        buf: &mut [u8],
    ) -> Result<usize, TransactionError> {
        let handle = to_handle(handle_val, request_opcode)?;
        let attr = self.database.find_attribute(handle).ok_or_else(|| {
            TransactionError::ErrorResponse {
                request_opcode,
                attribute_handle: handle_val,
                error_code: ErrorCode::InvalidHandle,
            }
        })?;
        attr.read_chunk(self.peer_id, offset, buf).await.map_err(|error_code| {
            TransactionError::ErrorResponse {
                request_opcode,
                attribute_handle: handle_val,
                error_code,
            }
        })
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

    fn effective_mtu(&self) -> usize {
        usize::try_from(self.mtu()).unwrap_or(usize::MAX)
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
    use crate::att::client::ReadByGroupTypeResults;
    use crate::att::database::testing::MockDb;
    use crate::att::l2cap::mock::setup_mock_channel;
    use crate::att::pdu::{
        FindByTypeValueReqHeader, FindInformationRsp, InformationData16, ReadByGroupTypeReqHeader,
        ReadByTypeReqHeader, WriteReqHeader,
    };

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

    #[test]
    fn test_server_handle_read_blob_success() {
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

                // Send ReadBlobReq for handle 1, offset 3
                let builder = PacketBuilder {
                    header: Header { opcode: Opcode::ReadBlobReq },
                    payload: ReadBlobReq {
                        attribute_handle: U16::new(1),
                        value_offset: U16::new(3),
                    },
                };
                client_tx_bearer.send(builder.as_packet()).await.unwrap();

                // Expect ReadBlobRsp containing "stone"
                let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ReadBlobRsp);
                let expected: &[u8] = b"stone";
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
    fn test_server_handle_read_blob_invalid_offset() {
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

                // Send ReadBlobReq for handle 1, offset 10 (length of "Sunstone" is 8)
                let builder = PacketBuilder {
                    header: Header { opcode: Opcode::ReadBlobReq },
                    payload: ReadBlobReq {
                        attribute_handle: U16::new(1),
                        value_offset: U16::new(10),
                    },
                };
                client_tx_bearer.send(builder.as_packet()).await.unwrap();

                // Expect ErrorRsp indicating InvalidOffset
                let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ErrorRsp);
                let err = ErrorRsp::try_read_from_bytes(&packet.data[..]).unwrap();
                assert_eq!(err.request_opcode, Opcode::ReadBlobReq as u8);
                assert_eq!(err.attribute_handle.get(), 1);
                assert_eq!(err.error_code, ErrorCode::InvalidOffset);
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
    fn test_server_handle_read_blob_truncated() {
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

                // Send ReadBlobReq for handle 1, offset 5
                let builder = PacketBuilder {
                    header: Header { opcode: Opcode::ReadBlobReq },
                    payload: ReadBlobReq {
                        attribute_handle: U16::new(1),
                        value_offset: U16::new(5),
                    },
                };
                client_tx_bearer.send(builder.as_packet()).await.unwrap();

                // Expect ReadBlobRsp containing first 22 bytes starting from offset 5
                // long_val[5..] is "5678901234567890123456789"
                // 22 bytes from offset 5: "5678901234567890123456"
                let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ReadBlobRsp);
                let expected: &[u8] = b"5678901234567890123456";
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
    fn test_server_handle_read_by_type_success() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, test_tx, test_rx) = setup_mock_channel(executor);

            let mut db = MockDb::new();
            db.insert(h(2), MockAttribute::new(Uuid::from_u16(0x2A00), b"Sunstone"));
            db.insert(h(4), MockAttribute::new(Uuid::from_u16(0x2A00), b"Sapphire"));
            db.insert(h(6), MockAttribute::new(Uuid::from_u16(0x2A00), b"Gatt")); // different length!

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

                let uuid = Uuid::from_u16(0x2A00);
                let header_builder = PacketBuilder {
                    header: Header { opcode: Opcode::ReadByTypeReq },
                    payload: ReadByTypeReqHeader {
                        starting_handle: U16::new(1),
                        ending_handle: U16::new(10),
                    },
                };
                let mut tx_buf = [0u8; 64];
                let mut builder = DynamicPacketBuilder::<_, u8>::new(
                    &mut tx_buf,
                    header_builder,
                    SERVER_MTU as usize,
                );
                builder.extend_from_slice(uuid.as_bytes()).unwrap();
                client_tx_bearer.send(builder.as_packet()).await.unwrap();

                let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ReadByTypeRsp);

                const VALUE_SIZE: usize = 8;
                const ENTRY_SIZE: u8 = (size_of::<AttributeHandle>() + VALUE_SIZE) as u8;

                assert_eq!(packet.data[0], ENTRY_SIZE);

                // Entry 1 (handle 2, "Sunstone")
                let h1_val = u16::from_le_bytes([packet.data[1], packet.data[2]]);
                assert_eq!(h1_val, 2);
                assert_eq!(&packet.data[3..11], b"Sunstone");

                // Entry 2 (handle 4, "Sapphire")
                let h2_val = u16::from_le_bytes([packet.data[11], packet.data[12]]);
                assert_eq!(h2_val, 4);
                assert_eq!(&packet.data[13..21], b"Sapphire");

                assert_eq!(packet.data.len(), 1 + ENTRY_SIZE as usize * 2);
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
    fn test_server_handle_read_by_type_errors() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, test_tx, test_rx) = setup_mock_channel(executor);

            let mut db = MockDb::new();
            db.insert(h(2), MockAttribute::new(Uuid::from_u16(0x2A00), b"Sunstone"));

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

                // 1. Query for non-existent UUID 0x2A01
                let uuid = Uuid::from_u16(0x2A01);
                let header_builder = PacketBuilder {
                    header: Header { opcode: Opcode::ReadByTypeReq },
                    payload: ReadByTypeReqHeader {
                        starting_handle: U16::new(1),
                        ending_handle: U16::new(10),
                    },
                };
                let mut tx_buf = [0u8; 64];
                let mut builder = DynamicPacketBuilder::<_, u8>::new(
                    &mut tx_buf,
                    header_builder,
                    SERVER_MTU as usize,
                );
                builder.extend_from_slice(uuid.as_bytes()).unwrap();
                client_tx_bearer.send(builder.as_packet()).await.unwrap();

                let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ErrorRsp);
                let err = ErrorRsp::try_read_from_bytes(&packet.data[..]).unwrap();
                assert_eq!(err.request_opcode, Opcode::ReadByTypeReq as u8);
                assert_eq!(err.error_code, ErrorCode::AttributeNotFound);

                // 2. Query with invalid range (start > end)
                let uuid = Uuid::from_u16(0x2A00);
                let header_builder = PacketBuilder {
                    header: Header { opcode: Opcode::ReadByTypeReq },
                    payload: ReadByTypeReqHeader {
                        starting_handle: U16::new(10), // start = 10
                        ending_handle: U16::new(5),    // end = 5
                    },
                };
                let mut tx_buf = [0u8; 64];
                let mut builder = DynamicPacketBuilder::<_, u8>::new(
                    &mut tx_buf,
                    header_builder,
                    SERVER_MTU as usize,
                );
                builder.extend_from_slice(uuid.as_bytes()).unwrap();
                client_tx_bearer.send(builder.as_packet()).await.unwrap();

                let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ErrorRsp);
                let err = ErrorRsp::try_read_from_bytes(&packet.data[..]).unwrap();
                assert_eq!(err.request_opcode, Opcode::ReadByTypeReq as u8);
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
    fn test_server_handle_read_by_group_type_success() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, test_tx, test_rx) = setup_mock_channel(executor);

            let mut db = MockDb::new();
            db.insert(h(2), MockAttribute::new_grouped(Uuid::from_u16(0x2800), b"Service1", 5));
            db.insert(h(6), MockAttribute::new_grouped(Uuid::from_u16(0x2800), b"Service2", 10));

            let mut server = Server::new(
                PeerId::new(1).unwrap(),
                BearerTx::new(test_tx),
                BearerRx::new(test_rx),
                SERVER_MTU,
                db,
            );

            let client_handle = executor.spawn(async move {
                const NEGOTIATED_MTU: u16 = 64;
                let mut rx_buf = [MaybeUninit::uninit(); NEGOTIATED_MTU as usize];
                let mut client_rx_bearer = BearerRx::new(app_channel.receiver);
                let mut client_tx_bearer = BearerTx::new(app_channel.sender);

                // 1. MTU Exchange (negotiate NEGOTIATED_MTU bytes)
                let header_builder = PacketBuilder {
                    header: Header { opcode: Opcode::ExchangeMtuReq },
                    payload: ExchangeMtuReq { client_rx_mtu: U16::new(NEGOTIATED_MTU) },
                };
                let mut tx_buf = [0u8; NEGOTIATED_MTU as usize];
                let builder = DynamicPacketBuilder::<_, u8>::new(&mut tx_buf, header_builder, 23);
                client_tx_bearer.send(builder.as_packet()).await.unwrap();

                let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ExchangeMtuRsp);
                client_rx_bearer.set_mtu(NEGOTIATED_MTU);
                client_tx_bearer.set_mtu(NEGOTIATED_MTU);

                // 2. Read By Group Type Request
                let uuid = Uuid::from_u16(0x2800);
                let header_builder = PacketBuilder {
                    header: Header { opcode: Opcode::ReadByGroupTypeReq },
                    payload: ReadByGroupTypeReqHeader {
                        starting_handle: U16::new(1),
                        ending_handle: U16::new(10),
                    },
                };
                let mut tx_buf = [0u8; NEGOTIATED_MTU as usize];
                let mut builder = DynamicPacketBuilder::<_, u8>::new(
                    &mut tx_buf,
                    header_builder,
                    NEGOTIATED_MTU as usize,
                );
                builder.extend_from_slice(uuid.as_bytes()).unwrap();
                client_tx_bearer.send(builder.as_packet()).await.unwrap();

                let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ReadByGroupTypeRsp);

                // Parse the response using the client results parser and verify the returned
                // attribute group data list entries.
                let results = ReadByGroupTypeResults::try_from(&packet.data[..])
                    .expect("Server response should be a valid Read By Group Type response packet");
                let mut iter = results.iter();

                let e1 = iter.next().unwrap().unwrap();
                assert_eq!(e1.handle, h(2));
                assert_eq!(e1.end_group_handle, h(5));
                assert_eq!(e1.value, b"Service1");

                let e2 = iter.next().unwrap().unwrap();
                assert_eq!(e2.handle, h(6));
                assert_eq!(e2.end_group_handle, h(10));
                assert_eq!(e2.value, b"Service2");

                assert!(iter.next().is_none());
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
    fn test_server_handle_read_by_group_type_errors() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, test_tx, test_rx) = setup_mock_channel(executor);

            let mut db = MockDb::new();
            // Match but non-grouping type! (returns None for group_end_handle)
            db.insert(h(2), MockAttribute::new(Uuid::from_u16(0x2A00), b"Sunstone"));

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

                // 1. Query for grouping type 0x2A00 which contains a non-grouping attribute
                let uuid = Uuid::from_u16(0x2A00);
                let header_builder = PacketBuilder {
                    header: Header { opcode: Opcode::ReadByGroupTypeReq },
                    payload: ReadByGroupTypeReqHeader {
                        starting_handle: U16::new(1),
                        ending_handle: U16::new(10),
                    },
                };
                let mut tx_buf = [0u8; 64];
                let mut builder = DynamicPacketBuilder::<_, u8>::new(
                    &mut tx_buf,
                    header_builder,
                    SERVER_MTU as usize,
                );
                builder.extend_from_slice(uuid.as_bytes()).unwrap();
                client_tx_bearer.send(builder.as_packet()).await.unwrap();

                let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ErrorRsp);
                let err = ErrorRsp::try_read_from_bytes(&packet.data[..]).unwrap();
                assert_eq!(err.request_opcode, Opcode::ReadByGroupTypeReq as u8);
                assert_eq!(err.error_code, ErrorCode::UnsupportedGroupType);
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
    fn test_server_handle_read_by_group_type_mixed_lengths() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, test_tx, test_rx) = setup_mock_channel(executor);

            let mut db = MockDb::new();
            // Attribute 1 has value of length 8
            db.insert(h(2), MockAttribute::new_grouped(Uuid::from_u16(0x2800), b"Service1", 5));
            // Attribute 2 has value of length 11 (different!)
            db.insert(h(6), MockAttribute::new_grouped(Uuid::from_u16(0x2800), b"ServiceLong", 10));

            let mut server = Server::new(
                PeerId::new(1).unwrap(),
                BearerTx::new(test_tx),
                BearerRx::new(test_rx),
                SERVER_MTU,
                db,
            );

            let client_handle = executor.spawn(async move {
                const NEGOTIATED_MTU: u16 = 64;
                let mut rx_buf = [MaybeUninit::uninit(); NEGOTIATED_MTU as usize];
                let mut client_rx_bearer = BearerRx::new(app_channel.receiver);
                let mut client_tx_bearer = BearerTx::new(app_channel.sender);

                // 1. MTU Exchange
                let header_builder = PacketBuilder {
                    header: Header { opcode: Opcode::ExchangeMtuReq },
                    payload: ExchangeMtuReq { client_rx_mtu: U16::new(NEGOTIATED_MTU) },
                };
                let mut tx_buf = [0u8; NEGOTIATED_MTU as usize];
                let builder = DynamicPacketBuilder::<_, u8>::new(&mut tx_buf, header_builder, 23);
                client_tx_bearer.send(builder.as_packet()).await.unwrap();

                let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ExchangeMtuRsp);
                client_rx_bearer.set_mtu(NEGOTIATED_MTU);
                client_tx_bearer.set_mtu(NEGOTIATED_MTU);

                // 2. Read By Group Type Request
                let uuid = Uuid::from_u16(0x2800);
                let header_builder = PacketBuilder {
                    header: Header { opcode: Opcode::ReadByGroupTypeReq },
                    payload: ReadByGroupTypeReqHeader {
                        starting_handle: U16::new(1),
                        ending_handle: U16::new(10),
                    },
                };
                let mut tx_buf = [0u8; NEGOTIATED_MTU as usize];
                let mut builder = DynamicPacketBuilder::<_, u8>::new(
                    &mut tx_buf,
                    header_builder,
                    NEGOTIATED_MTU as usize,
                );
                builder.extend_from_slice(uuid.as_bytes()).unwrap();
                client_tx_bearer.send(builder.as_packet()).await.unwrap();

                let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ReadByGroupTypeRsp);

                // Parse the response and verify that only the first entry was packed
                // due to subsequent entries having different lengths.
                let results = ReadByGroupTypeResults::try_from(&packet.data[..])
                    .expect("Server response should be a valid Read By Group Type response packet");
                let mut iter = results.iter();

                let e1 = iter.next().unwrap().unwrap();
                assert_eq!(e1.handle, h(2));
                assert_eq!(e1.end_group_handle, h(5));
                assert_eq!(e1.value, b"Service1");

                assert!(iter.next().is_none());
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
    fn test_server_handle_write_success() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, test_tx, test_rx) = setup_mock_channel(executor);

            let mut db = MockDb::new();
            let attr = MockAttribute::new(Uuid::from_u16(0x2A00), b"InitialValue");
            db.insert(h(10), attr);

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

                // Write Request for handle 10, value "Sunstone"
                let header_builder = PacketBuilder {
                    header: Header { opcode: Opcode::WriteReq },
                    payload: WriteReqHeader { attribute_handle: U16::new(10) },
                };
                let mut tx_buf = [0u8; SERVER_MTU as usize];
                let mut builder = DynamicPacketBuilder::<_, u8>::new(
                    &mut tx_buf,
                    header_builder,
                    SERVER_MTU as usize,
                );
                builder.extend_from_slice(b"Sunstone").unwrap();
                client_tx_bearer.send(builder.as_packet()).await.unwrap();

                // Expect WriteRsp
                let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::WriteRsp);
                assert!(packet.data.is_empty());
            });

            let mut server_handle = executor.spawn(async move {
                let res = server.run().await;
                assert_eq!(res, Err(ServerError::LinkClosed));
                server
            });

            executor.run_until_stalled();
            assert!(client_handle.is_finished());
            assert!(server_handle.is_finished());

            // Verify value was written in database attribute
            let server = server_handle.get().unwrap();
            let db_attr = server.database.find_attribute(h(10)).unwrap();
            let mut check_buf = [0u8; 32];
            let read_len = executor.block_on(async {
                db_attr.read_chunk(PeerId::new(1).unwrap(), 0, &mut check_buf).await.unwrap()
            });
            assert_eq!(&check_buf[..read_len], b"Sunstone");
        });
    }

    #[test]
    fn test_server_handle_write_errors() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, test_tx, test_rx) = setup_mock_channel(executor);

            let mut db = MockDb::new();
            let attr = MockAttribute::new(Uuid::from_u16(0x2A00), b"Val");
            attr.set_write_error(ErrorCode::WriteNotPermitted);
            db.insert(h(10), attr);

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

                // Write Request for non-existent handle 99
                {
                    let header_builder = PacketBuilder {
                        header: Header { opcode: Opcode::WriteReq },
                        payload: WriteReqHeader { attribute_handle: U16::new(99) },
                    };
                    let mut tx_buf = [0u8; SERVER_MTU as usize];
                    let mut builder = DynamicPacketBuilder::<_, u8>::new(
                        &mut tx_buf,
                        header_builder,
                        SERVER_MTU as usize,
                    );
                    builder.extend_from_slice(b"Value").unwrap();
                    client_tx_bearer.send(builder.as_packet()).await.unwrap();

                    let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                    assert_eq!(packet.header.opcode, Opcode::ErrorRsp);
                    let err = ErrorRsp::try_read_from_bytes(&packet.data[..]).unwrap();
                    assert_eq!(err.request_opcode, Opcode::WriteReq as u8);
                    assert_eq!(err.attribute_handle.get(), 99);
                    assert_eq!(err.error_code, ErrorCode::InvalidHandle);
                }
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
