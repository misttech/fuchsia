// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::att::AttributeHandle;
use crate::att::bearer::{
    AttReceiver, BearerRecvError, BearerSendError, BearerTx, DEFAULT_STARTING_MTU,
    MAX_SUPPORTED_MTU,
};
use crate::att::l2cap::{L2CapChannelRx, L2CapChannelTx};
use crate::att::pdu::{
    ATT_ERROR_RSP_SIZE, ATT_EXCHANGE_MTU_REQ_SIZE, ATT_EXCHANGE_MTU_RSP_SIZE,
    ATT_EXECUTE_WRITE_REQ_SIZE, ATT_FIND_BY_TYPE_VALUE_REQ_HEADER_SIZE,
    ATT_FIND_INFORMATION_REQ_SIZE, ATT_HANDLE_VALUE_CFM_SIZE, ATT_PREPARE_WRITE_HEADER_SIZE,
    ATT_READ_BLOB_REQ_SIZE, ATT_READ_BY_GROUP_TYPE_REQ_HEADER_SIZE,
    ATT_READ_BY_TYPE_REQ_HEADER_SIZE, ATT_READ_REQ_SIZE, ATT_WRITE_CMD_HEADER_SIZE,
    ATT_WRITE_REQ_HEADER_SIZE, DynamicPacketBuilder, ErrorCode, ExecuteWriteFlags,
    FindInformationRsp, HandlesInformation, InformationData16, InformationData128, Opcode, Packet,
    ReadByGroupTypeRsp, ReadByGroupTypeRspEntryHeader, ReadByTypeRsp, UuidFormat,
};
use crate::att::router::{BearerRouter, BearerRxHandle, RouteFilter};
use sapphire_emboss::att::{
    AttErrorRsp, AttExchangeMtuReqMut, AttExchangeMtuRsp, AttExecuteWriteReqMut,
    AttFindByTypeValueReqHeaderMut, AttFindInformationReqMut, AttHeader, AttHeaderMut,
    AttPrepareWriteHeaderMut, AttReadBlobReqMut, AttReadByGroupTypeReqHeaderMut,
    AttReadByTypeReqHeaderMut, AttReadReqMut, AttWriteCmdMut,
};

use core::cmp::{max, min};
use core::mem::{MaybeUninit, size_of};
use sapphire_common::Uuid;
use sapphire_sync::mutex::raw::RawMutex;
use thiserror::Error;
use zerocopy::byteorder::little_endian::U16;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, TryFromBytes};

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

/// A single handle-value pair returned by a Read By Type Response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttributeData<'a> {
    pub handle: AttributeHandle,
    pub value: &'a [u8],
}

/// A structured view over a Read By Type Response's handle-value pairs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadByTypeResults<'a> {
    length: usize,
    data: &'a [u8],
}

impl<'a> ReadByTypeResults<'a> {
    pub fn iter(&self) -> ReadByTypeIter<'a> {
        ReadByTypeIter { length: self.length, data: self.data }
    }
}

impl<'a> IntoIterator for ReadByTypeResults<'a> {
    type Item = Result<AttributeData<'a>, ClientError>;
    type IntoIter = ReadByTypeIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a, 'b> IntoIterator for &'a ReadByTypeResults<'a> {
    type Item = Result<AttributeData<'a>, ClientError>;
    type IntoIter = ReadByTypeIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

pub struct ReadByTypeIter<'a> {
    length: usize,
    data: &'a [u8],
}

impl<'a> ReadByTypeIter<'a> {
    fn next_chunk(&mut self) -> Result<AttributeData<'a>, ClientError> {
        let (chunk, rest) =
            self.data.split_at_checked(self.length).ok_or(ClientError::InvalidIncomingData)?;
        self.data = rest;

        let (handle_bytes, value) = chunk
            .split_at_checked(size_of::<AttributeHandle>())
            .ok_or(ClientError::InvalidIncomingData)?;
        let handle_u16 =
            U16::try_ref_from_bytes(handle_bytes).map_err(|_| ClientError::InvalidIncomingData)?;
        let handle = AttributeHandle::try_from(handle_u16.get())
            .map_err(|_| ClientError::InvalidIncomingData)?;

        Ok(AttributeData { handle, value })
    }
}

impl<'a> Iterator for ReadByTypeIter<'a> {
    type Item = Result<AttributeData<'a>, ClientError>;

    /// Parses and returns the next attribute handle-value entry from the response buffer.
    fn next(&mut self) -> Option<Self::Item> {
        // Check if the end of the data list has been reached.
        if self.data.is_empty() {
            return None;
        }
        Some(self.next_chunk())
    }
}

/// A single group handle-value entry returned by a Read By Group Type Response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttributeGroupData<'a> {
    pub handle: AttributeHandle,
    pub end_group_handle: AttributeHandle,
    pub value: &'a [u8],
}

/// A structured view over a Read By Group Type Response's group handle-value entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadByGroupTypeResults<'a> {
    length: usize,
    data: &'a [u8],
}

impl<'a> TryFrom<&'a [u8]> for ReadByGroupTypeResults<'a> {
    type Error = ClientError;

    /// Parses and validates a raw ATT Read By Group Type Response PDU payload
    /// into a structured results view.
    fn try_from(pdu_data: &'a [u8]) -> Result<Self, Self::Error> {
        let rsp = ReadByGroupTypeRsp::try_ref_from_bytes(pdu_data)
            .map_err(|_| ClientError::InvalidIncomingData)?;
        let length = usize::from(rsp.length);
        if length < size_of::<ReadByGroupTypeRspEntryHeader>() {
            return Err(ClientError::InvalidIncomingData);
        }
        if rsp.attribute_data_list.is_empty() || rsp.attribute_data_list.len() % length != 0 {
            return Err(ClientError::InvalidIncomingData);
        }
        Ok(Self { length, data: &rsp.attribute_data_list })
    }
}

impl<'a> IntoIterator for ReadByGroupTypeResults<'a> {
    type Item = Result<AttributeGroupData<'a>, ClientError>;
    type IntoIter = ReadByGroupTypeIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a, 'b> IntoIterator for &'a ReadByGroupTypeResults<'a> {
    type Item = Result<AttributeGroupData<'a>, ClientError>;
    type IntoIter = ReadByGroupTypeIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a> ReadByGroupTypeResults<'a> {
    pub fn iter(&self) -> ReadByGroupTypeIter<'a> {
        ReadByGroupTypeIter { length: self.length, data: self.data }
    }
}

pub struct ReadByGroupTypeIter<'a> {
    length: usize,
    data: &'a [u8],
}

impl<'a> ReadByGroupTypeIter<'a> {
    fn next_chunk(&mut self) -> Result<AttributeGroupData<'a>, ClientError> {
        let (chunk, rest) =
            self.data.split_at_checked(self.length).ok_or(ClientError::InvalidIncomingData)?;
        self.data = rest;

        let (header_bytes, value) = chunk
            .split_at_checked(size_of::<ReadByGroupTypeRspEntryHeader>())
            .ok_or(ClientError::InvalidIncomingData)?;

        let header = ReadByGroupTypeRspEntryHeader::try_ref_from_bytes(header_bytes)
            .map_err(|_| ClientError::InvalidIncomingData)?;

        let handle = AttributeHandle::try_from(header.attribute_handle.get())
            .map_err(|_| ClientError::InvalidIncomingData)?;
        let end_group_handle = AttributeHandle::try_from(header.end_group_handle.get())
            .map_err(|_| ClientError::InvalidIncomingData)?;

        Ok(AttributeGroupData { handle, end_group_handle, value })
    }
}

impl<'a> Iterator for ReadByGroupTypeIter<'a> {
    type Item = Result<AttributeGroupData<'a>, ClientError>;

    /// Parses and returns the next attribute group handle-value entry from the response buffer.
    fn next(&mut self) -> Option<Self::Item> {
        // Check if the end of the data list has been reached.
        if self.data.is_empty() {
            return None;
        }

        Some(self.next_chunk())
    }
}

/// ATT Client protocol wrapper.
pub struct Client<Tx, R> {
    bearer_tx: BearerTx<Tx>,
    bearer_rx: R,
    preferred_mtu: u16,
}

impl<Tx, R> Client<Tx, R>
where
    Tx: L2CapChannelTx,
    R: AttReceiver,
{
    /// Creates a new ATT Client instance.
    pub fn new(bearer_tx: BearerTx<Tx>, bearer_rx: R, preferred_mtu: u16) -> Self {
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

        let header = AttHeader::new(rx_packet.as_bytes());
        match header.attribute_opcode().try_read() {
            Ok(opcode) if opcode == expected_rsp_opcode => Ok(rx_packet),
            Ok(Opcode::ATT_ERROR_RSP) => {
                if rx_packet.as_bytes().len() != ATT_ERROR_RSP_SIZE {
                    return Err(ClientError::InvalidIncomingData);
                }
                let err = AttErrorRsp::new(rx_packet.as_bytes());
                let err_req_op = match err.request_opcode_in_error_uint().try_read() {
                    Ok(op) => op,
                    Err(_) => return Err(ClientError::InvalidIncomingData),
                };
                let err_code =
                    err.error_code().try_read().map_err(|_| ClientError::InvalidIncomingData)?;
                if err_req_op == u8::from(req_opcode) {
                    Err(ClientError::ErrorResponse(err_code))
                } else {
                    Err(ClientError::UnexpectedOpcode(Opcode::ATT_ERROR_RSP))
                }
            }
            Ok(other) => Err(ClientError::UnexpectedOpcode(other)),
            Err(_) => Err(ClientError::InvalidIncomingData),
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
        let mut req_buf = [0u8; ATT_EXCHANGE_MTU_REQ_SIZE];
        let mut view = AttExchangeMtuReqMut::new(&mut req_buf[..]);
        view.attribute_opcode().try_write(Opcode::ATT_EXCHANGE_MTU_REQ).expect("valid opcode");
        view.client_rx_mtu().try_write(self.preferred_mtu).expect("valid mtu");
        let tx_packet = Packet::try_ref_from_bytes(&req_buf[..]).expect("valid packet");
        let mut rx_buf = [MaybeUninit::uninit(); DEFAULT_STARTING_MTU as usize];

        match self
            .transaction(
                Opcode::ATT_EXCHANGE_MTU_REQ,
                tx_packet,
                &mut rx_buf,
                Opcode::ATT_EXCHANGE_MTU_RSP,
            )
            .await
        {
            Ok(rx_packet) => {
                if rx_packet.as_bytes().len() != ATT_EXCHANGE_MTU_RSP_SIZE {
                    return Err(ClientError::InvalidIncomingData);
                }
                let rsp = AttExchangeMtuRsp::new(rx_packet.as_bytes());
                let server_mtu =
                    rsp.server_rx_mtu().try_read().map_err(|_| ClientError::InvalidIncomingData)?;
                let negotiated_mtu = max(DEFAULT_STARTING_MTU, min(self.preferred_mtu, server_mtu));

                self.bearer_tx.set_mtu(negotiated_mtu);
                self.bearer_rx.set_mtu(negotiated_mtu);
                Ok(())
            }
            Err(ClientError::ErrorResponse(ErrorCode::REQUEST_NOT_SUPPORTED)) => {
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
        let mut tx_buf = [0u8; ATT_FIND_INFORMATION_REQ_SIZE];
        let mut view = AttFindInformationReqMut::new(&mut tx_buf[..]);
        view.attribute_opcode().try_write(Opcode::ATT_FIND_INFORMATION_REQ).expect("valid opcode");
        view.starting_handle().try_write(starting_handle.value()).expect("valid handle");
        view.ending_handle().try_write(ending_handle.value()).expect("valid handle");
        let tx_packet = Packet::try_ref_from_bytes(&tx_buf[..]).expect("valid packet");

        let rx_packet = self
            .transaction(
                Opcode::ATT_FIND_INFORMATION_REQ,
                tx_packet,
                rx_buf,
                Opcode::ATT_FIND_INFORMATION_RSP,
            )
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
        let total_size = ATT_FIND_BY_TYPE_VALUE_REQ_HEADER_SIZE + attribute_value.len();
        assert!(
            total_size <= self.effective_mtu(),
            "Programming error: request packet size exceeds negotiated MTU."
        );
        let mut tx_buf = [0u8; MAX_SUPPORTED_MTU];
        let mut view = AttFindByTypeValueReqHeaderMut::new(
            &mut tx_buf[..ATT_FIND_BY_TYPE_VALUE_REQ_HEADER_SIZE],
        );
        view.attribute_opcode()
            .try_write(Opcode::ATT_FIND_BY_TYPE_VALUE_REQ)
            .expect("valid opcode");
        view.starting_handle().try_write(starting_handle.value()).expect("valid handle");
        view.ending_handle().try_write(ending_handle.value()).expect("valid handle");
        view.attribute_type().try_write(attribute_type).expect("valid attribute type");
        tx_buf[ATT_FIND_BY_TYPE_VALUE_REQ_HEADER_SIZE..total_size].copy_from_slice(attribute_value);
        let tx_packet = Packet::try_ref_from_bytes(&tx_buf[..total_size]).expect("valid packet");

        let rx_packet = self
            .transaction(
                Opcode::ATT_FIND_BY_TYPE_VALUE_REQ,
                tx_packet,
                rx_buf,
                Opcode::ATT_FIND_BY_TYPE_VALUE_RSP,
            )
            .await?;

        let entries = <[HandlesInformation]>::ref_from_bytes(&rx_packet.data[..])
            .map_err(|_| ClientError::InvalidIncomingData)?;
        Ok(entries)
    }

    /// Sends a Read Request and awaits a Read Response.
    ///
    /// see Bluetooth Core Spec v6.0 (Vol 3, Part F, Section 3.4.4.1 & 3.4.4.2)
    pub async fn read<'a>(
        &mut self,
        handle: AttributeHandle,
        rx_buf: &'a mut [MaybeUninit<u8>],
    ) -> Result<&'a mut [u8], ClientError> {
        let mut buf = [0u8; ATT_READ_REQ_SIZE];
        let mut view = AttReadReqMut::new(&mut buf[..]);
        view.attribute_opcode().try_write(Opcode::ATT_READ_REQ).expect("valid opcode");
        view.attribute_handle().try_write(handle.value()).expect("valid handle");
        let tx_packet = Packet::try_ref_from_bytes(&buf[..]).expect("valid packet");

        // Perform the transaction and await the matching Read Response.
        let rsp_packet =
            self.transaction(Opcode::ATT_READ_REQ, tx_packet, rx_buf, Opcode::ATT_READ_RSP).await?;

        // Return the variable-length attribute value.
        Ok(&mut rsp_packet.data)
    }

    /// Sends a Read Blob Request and awaits a Read Blob Response.
    ///
    /// see Bluetooth Core Spec v6.0 (Vol 3, Part F, Section 3.4.4.3 & 3.4.4.4)
    pub async fn read_blob<'a>(
        &mut self,
        handle: AttributeHandle,
        offset: u16,
        rx_buf: &'a mut [MaybeUninit<u8>],
    ) -> Result<&'a mut [u8], ClientError> {
        let mut buf = [0u8; ATT_READ_BLOB_REQ_SIZE];
        let mut view = AttReadBlobReqMut::new(&mut buf[..]);
        view.attribute_opcode().try_write(Opcode::ATT_READ_BLOB_REQ).expect("valid opcode");
        view.attribute_handle().try_write(handle.value()).expect("valid handle");
        view.value_offset().try_write(offset).expect("valid offset");
        let tx_packet = Packet::try_ref_from_bytes(&buf[..]).expect("valid packet");

        // Perform the transaction and await the matching Read Blob Response.
        let rsp_packet = self
            .transaction(Opcode::ATT_READ_BLOB_REQ, tx_packet, rx_buf, Opcode::ATT_READ_BLOB_RSP)
            .await?;

        // Return the variable-length value chunk.
        Ok(&mut rsp_packet.data)
    }

    pub fn mtu(&self) -> u16 {
        self.bearer_tx.mtu()
    }

    fn effective_mtu(&self) -> usize {
        usize::try_from(self.mtu()).unwrap_or(usize::MAX)
    }

    /// Initiates a Read By Type procedure to obtain the values of attributes with a specific
    /// attribute type (UUID).
    ///
    /// see Bluetooth Core Spec v6.0 (Vol 3, Part F, Section 3.4.4.7 & 3.4.4.8).
    pub async fn read_by_type<'a>(
        &mut self,
        starting_handle: AttributeHandle,
        ending_handle: AttributeHandle,
        attribute_type: &Uuid,
        rx_buf: &'a mut [MaybeUninit<u8>],
    ) -> Result<ReadByTypeResults<'a>, ClientError> {
        // Serialize the variable-length UUID parameter onto the end of the request header.
        let type_bytes = attribute_type.as_bytes();
        let mut header = [0u8; ATT_READ_BY_TYPE_REQ_HEADER_SIZE];
        let mut view = AttReadByTypeReqHeaderMut::new(&mut header);
        view.attribute_opcode().try_write(Opcode::ATT_READ_BY_TYPE_REQ).expect("valid opcode");
        view.starting_handle().try_write(starting_handle.value()).expect("valid handle");
        view.ending_handle().try_write(ending_handle.value()).expect("valid handle");
        let mut tx_buf = [0u8; MAX_SUPPORTED_MTU];
        let mut builder =
            DynamicPacketBuilder::<_, u8>::new(&mut tx_buf, header, self.effective_mtu());
        builder
            .extend_from_slice(type_bytes)
            .expect("Programming error: request packet size exceeds negotiated MTU.");
        let tx_packet = builder.as_packet();

        // Perform the transaction and await the response.
        let rx_packet = self
            .transaction(
                Opcode::ATT_READ_BY_TYPE_REQ,
                tx_packet,
                rx_buf,
                Opcode::ATT_READ_BY_TYPE_RSP,
            )
            .await?;

        // Parse the response PDU.
        let rsp = ReadByTypeRsp::try_ref_from_bytes(&rx_packet.data)
            .map_err(|_| ClientError::InvalidIncomingData)?;
        let length = usize::from(rsp.length);

        // Each returned attribute-value pair must contain at least a handle.
        if length < size_of::<AttributeHandle>() {
            return Err(ClientError::InvalidIncomingData);
        }

        // The data list must not be empty and must partition cleanly into fixed-size entries.
        let data_list = &rsp.attribute_data_list;
        if data_list.is_empty() || data_list.len() % length != 0 {
            return Err(ClientError::InvalidIncomingData);
        }

        Ok(ReadByTypeResults { length, data: data_list })
    }

    /// Initiates a Read By Group Type procedure to obtain the values of attributes with a specific
    /// attribute group type (UUID).
    ///
    /// see Bluetooth Core Spec v6.0 (Vol 3, Part F, Section 3.4.4.9 & 3.4.4.10).
    pub async fn read_by_group_type<'a>(
        &mut self,
        starting_handle: AttributeHandle,
        ending_handle: AttributeHandle,
        attribute_group_type: &Uuid,
        rx_buf: &'a mut [MaybeUninit<u8>],
    ) -> Result<ReadByGroupTypeResults<'a>, ClientError> {
        // Serialize the variable-length UUID parameter onto the end of the request header.
        let type_bytes = attribute_group_type.as_bytes();
        let mut header = [0u8; ATT_READ_BY_GROUP_TYPE_REQ_HEADER_SIZE];
        let mut view = AttReadByGroupTypeReqHeaderMut::new(&mut header);
        view.attribute_opcode()
            .try_write(Opcode::ATT_READ_BY_GROUP_TYPE_REQ)
            .expect("valid opcode");
        view.starting_handle().try_write(starting_handle.value()).expect("valid handle");
        view.ending_handle().try_write(ending_handle.value()).expect("valid handle");
        let mut tx_buf = [0u8; MAX_SUPPORTED_MTU];
        let mut builder =
            DynamicPacketBuilder::<_, u8>::new(&mut tx_buf, header, self.effective_mtu());
        builder
            .extend_from_slice(type_bytes)
            .expect("Programming error: request packet size exceeds negotiated MTU.");
        let tx_packet = builder.as_packet();

        // Perform the transaction and await the response.
        let rx_packet = self
            .transaction(
                Opcode::ATT_READ_BY_GROUP_TYPE_REQ,
                tx_packet,
                rx_buf,
                Opcode::ATT_READ_BY_GROUP_TYPE_RSP,
            )
            .await?;

        // Parse and validate the response PDU.
        ReadByGroupTypeResults::try_from(&rx_packet.data[..])
    }

    /// Initiates a Write Request procedure to write the value of an attribute.
    ///
    /// see Bluetooth Core Spec v6.0 (Vol 3, Part F, Section 3.4.5.1 & 3.4.5.2).
    pub async fn write(
        &mut self,
        attribute_handle: AttributeHandle,
        attribute_value: &[u8],
        rx_buf: &mut [MaybeUninit<u8>],
    ) -> Result<(), ClientError> {
        let req_len = ATT_WRITE_REQ_HEADER_SIZE + attribute_value.len();
        assert!(
            req_len <= self.effective_mtu(),
            "Programming error: request packet size exceeds negotiated MTU."
        );
        assert!(
            req_len <= MAX_SUPPORTED_MTU,
            "Programming error: request packet size exceeds buffer."
        );
        let mut tx_buf = [0u8; MAX_SUPPORTED_MTU];
        let mut view = AttWriteCmdMut::new(&mut tx_buf[..ATT_WRITE_REQ_HEADER_SIZE]);
        view.attribute_opcode().try_write(Opcode::ATT_WRITE_REQ).expect("valid opcode");
        view.attribute_handle().try_write(attribute_handle.value()).expect("valid handle");
        tx_buf[ATT_WRITE_REQ_HEADER_SIZE..req_len].copy_from_slice(attribute_value);
        let tx_packet = Packet::try_ref_from_bytes(&tx_buf[..req_len]).expect("valid packet");

        let _rx_packet = self
            .transaction(Opcode::ATT_WRITE_REQ, tx_packet, rx_buf, Opcode::ATT_WRITE_RSP)
            .await?;

        Ok(())
    }

    /// Initiates a Write Command procedure to write the value of an attribute without response.
    ///
    /// see Bluetooth Core Spec v6.0 (Vol 3, Part F, Section 3.4.5.3).
    pub async fn write_command(
        &mut self,
        attribute_handle: AttributeHandle,
        attribute_value: &[u8],
    ) -> Result<(), ClientError> {
        let req_len = ATT_WRITE_CMD_HEADER_SIZE + attribute_value.len();
        assert!(
            req_len <= self.effective_mtu(),
            "Programming error: request packet size exceeds negotiated MTU."
        );
        assert!(
            req_len <= MAX_SUPPORTED_MTU,
            "Programming error: request packet size exceeds buffer."
        );
        let mut tx_buf = [0u8; MAX_SUPPORTED_MTU];
        let mut view = AttWriteCmdMut::new(&mut tx_buf[..ATT_WRITE_CMD_HEADER_SIZE]);
        view.attribute_opcode().try_write(Opcode::ATT_WRITE_CMD).expect("valid opcode");
        view.attribute_handle().try_write(attribute_handle.value()).expect("valid handle");
        tx_buf[ATT_WRITE_CMD_HEADER_SIZE..req_len].copy_from_slice(attribute_value);
        let tx_packet = Packet::try_ref_from_bytes(&tx_buf[..req_len]).expect("valid packet");

        self.send_packet(tx_packet).await?;
        Ok(())
    }

    /// Initiates a Prepare Write procedure to write a part of an attribute value.
    ///
    /// see Bluetooth Core Spec v6.0 (Vol 3, Part F, Section 3.4.6.1).
    pub async fn prepare_write(
        &mut self,
        attribute_handle: AttributeHandle,
        value_offset: u16,
        part_attribute_value: &[u8],
        rx_buf: &mut [MaybeUninit<u8>],
    ) -> Result<(), ClientError> {
        let req_len = ATT_PREPARE_WRITE_HEADER_SIZE + part_attribute_value.len();
        assert!(
            req_len <= self.effective_mtu(),
            "Programming error: request packet size exceeds negotiated MTU."
        );
        assert!(
            req_len <= MAX_SUPPORTED_MTU,
            "Programming error: request packet size exceeds buffer."
        );
        let mut tx_buf = [0u8; MAX_SUPPORTED_MTU];
        let mut view = AttPrepareWriteHeaderMut::new(&mut tx_buf[..ATT_PREPARE_WRITE_HEADER_SIZE]);
        view.attribute_opcode().try_write(Opcode::ATT_PREPARE_WRITE_REQ).expect("valid opcode");
        view.attribute_handle().try_write(attribute_handle.value()).expect("valid handle");
        view.value_offset().try_write(value_offset).expect("valid offset");
        tx_buf[ATT_PREPARE_WRITE_HEADER_SIZE..req_len].copy_from_slice(part_attribute_value);
        let tx_packet = Packet::try_ref_from_bytes(&tx_buf[..req_len]).expect("valid packet");

        let rx_packet = self
            .transaction(
                Opcode::ATT_PREPARE_WRITE_REQ,
                tx_packet,
                rx_buf,
                Opcode::ATT_PREPARE_WRITE_RSP,
            )
            .await?;

        if rx_packet.data != tx_packet.data {
            return Err(ClientError::InvalidIncomingData);
        }

        Ok(())
    }

    /// Initiates an Execute Write procedure to commit or cancel prepared writes.
    ///
    /// Note: Although the Execute Write Response has no payload, `rx_buf` must be
    /// provided to satisfy the transaction MTU safety guardrail and allow the caller
    /// to reuse their existing buffer without stack duplication.
    ///
    /// see Bluetooth Core Spec v6.0 (Vol 3, Part F, Section 3.4.6.3).
    pub async fn execute_write(
        &mut self,
        flags: ExecuteWriteFlags,
        rx_buf: &mut [MaybeUninit<u8>],
    ) -> Result<(), ClientError> {
        let mut buf = [0u8; ATT_EXECUTE_WRITE_REQ_SIZE];
        let mut view = AttExecuteWriteReqMut::new(&mut buf[..]);
        view.attribute_opcode().try_write(Opcode::ATT_EXECUTE_WRITE_REQ).expect("valid opcode");
        view.flags().try_write(flags).expect("valid flags");
        let tx_packet = Packet::try_ref_from_bytes(&buf[..]).expect("valid packet");

        let rx_packet = self
            .transaction(
                Opcode::ATT_EXECUTE_WRITE_REQ,
                tx_packet,
                rx_buf,
                Opcode::ATT_EXECUTE_WRITE_RSP,
            )
            .await?;

        if !rx_packet.data.is_empty() {
            return Err(ClientError::InvalidIncomingData);
        }

        Ok(())
    }

    /// Returns an asynchronous stream handle for consuming unsolicited server events
    /// pushed from the remote server endpoint.
    pub fn server_event_stream<'a, Rx: L2CapChannelRx, Mtx: RawMutex>(
        &self,
        rx_handle: BearerRxHandle<'a, Rx, Mtx>,
    ) -> ServerEventStream<Tx, BearerRxHandle<'a, Rx, Mtx>> {
        ServerEventStream { bearer_tx: self.bearer_tx.clone(), rx_handle }
    }
}

/// An individual unsolicited server event pushed from the remote server endpoint.
#[derive(TryFromBytes, KnownLayout, Immutable, IntoBytes, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct ServerEvent {
    attribute_handle: U16,
    value: [u8],
}

impl ServerEvent {
    /// Returns the handle of the attribute whose value was updated.
    pub fn handle(&self) -> AttributeHandle {
        AttributeHandle::new(self.attribute_handle.get()).expect("valid non-zero handle")
    }

    /// Returns the updated attribute value bytes.
    pub fn value(&self) -> &[u8] {
        &self.value
    }
}

/// An asynchronous stream consumption API for consuming unsolicited server events
/// pushed from the remote server endpoint.
#[derive(Debug)]
pub struct ServerEventStream<Tx, R> {
    bearer_tx: BearerTx<Tx>,
    rx_handle: R,
}

impl<'a, Tx: L2CapChannelTx, Rx: L2CapChannelRx, Mtx: RawMutex>
    ServerEventStream<Tx, BearerRxHandle<'a, Rx, Mtx>>
{
    pub fn new(router: &'a BearerRouter<Rx, Mtx>, bearer_tx: BearerTx<Tx>) -> Option<Self> {
        let rx_handle = router.route_to(RouteFilter::ServerEvents)?;
        Some(Self { bearer_tx, rx_handle })
    }
}

impl<Tx: L2CapChannelTx, R: AttReceiver> ServerEventStream<Tx, R> {
    /// Awaits the next server event frame from the network.
    /// Acknowledgment PDUs are automatically transmitted down the underlying bearer
    /// when required by the ATT protocol before returning.
    pub async fn next<'a>(
        &mut self,
        buf: &'a mut [MaybeUninit<u8>],
    ) -> Result<&'a ServerEvent, ClientError> {
        let packet = self.rx_handle.next_packet(buf).await.map_err(|e| match e {
            BearerRecvError::LinkClosed => ClientError::LinkClosed,
            BearerRecvError::BufferTooSmall => {
                panic!(
                    "Programming error: provided buffer size is smaller than the negotiated MTU."
                );
            }
            BearerRecvError::InvalidOpcode(_) => {
                panic!("Programming error: rx_handle returned an invalid opcode.");
            }
            BearerRecvError::HeaderTooShort | BearerRecvError::PacketTooLarge { .. } => {
                ClientError::InvalidIncomingData
            }
        })?;

        let event = ServerEvent::try_ref_from_bytes(&packet.data)
            .map_err(|_| ClientError::InvalidIncomingData)?;

        let header = AttHeader::new(packet.as_bytes());
        match header.attribute_opcode().try_read() {
            Ok(Opcode::ATT_HANDLE_VALUE_NTF) => Ok(event),
            Ok(Opcode::ATT_HANDLE_VALUE_IND) => {
                let mut cfm_buf = [0u8; ATT_HANDLE_VALUE_CFM_SIZE];
                let mut view = AttHeaderMut::new(&mut cfm_buf);
                view.attribute_opcode()
                    .try_write(Opcode::ATT_HANDLE_VALUE_CFM)
                    .expect("valid opcode");
                let tx_packet = Packet::try_ref_from_bytes(&cfm_buf).expect("valid packet");
                match self.bearer_tx.send(tx_packet).await {
                    Ok(()) => {}
                    Err(BearerSendError::LinkClosed) => return Err(ClientError::LinkClosed),
                    Err(BearerSendError::PacketTooLarge) => {
                        unreachable!("HandleValueCfm (1 byte) never exceeds minimum ATT MTU")
                    }
                }
                Ok(event)
            }
            _ => Err(ClientError::InvalidIncomingData),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::att::bearer::BearerRx;
    use crate::att::l2cap::mock::setup_mock_channel;
    use crate::att::pdu::{
        ATT_ERROR_RSP_SIZE, ATT_EXCHANGE_MTU_RSP_SIZE, ATT_EXECUTE_WRITE_RSP_SIZE,
        ATT_FIND_BY_TYPE_VALUE_REQ_HEADER_SIZE, ATT_HANDLE_VALUE_IND_HEADER_SIZE,
        ATT_PREPARE_WRITE_HEADER_SIZE, DynamicPacketBuilder, Header,
    };
    use sapphire_async::executor::BoundedExecutor;
    use sapphire_async::testing::TestExecutor;
    use sapphire_emboss::att::{
        AttErrorRspMut, AttExchangeMtuReq, AttExchangeMtuRspMut, AttExecuteWriteReq,
        AttFindByTypeValueReqHeader, AttFindInformationReq, AttHandleValueIndHeaderMut,
        AttHeaderMut, AttPrepareWriteHeader, AttPrepareWriteHeaderMut, AttReadBlobReq, AttReadReq,
        AttWriteCmd,
    };

    const CLIENT_PREFERRED_MTU: u16 = 512;
    const SERVER_MTU: u16 = 256;

    fn h(val: u16) -> AttributeHandle {
        AttributeHandle::try_from(val).unwrap()
    }

    fn make_error_rsp(
        opcode: Opcode,
        handle: u16,
        error_code: ErrorCode,
    ) -> [u8; ATT_ERROR_RSP_SIZE] {
        let mut buf = [0u8; ATT_ERROR_RSP_SIZE];
        let mut view = AttErrorRspMut::new(&mut buf[..]);
        view.attribute_opcode().try_write(Opcode::ATT_ERROR_RSP).unwrap();
        view.request_opcode_in_error_uint().try_write(u8::from(opcode)).unwrap();
        view.attribute_handle().try_write(handle).unwrap();
        view.error_code().try_write(error_code).unwrap();
        buf
    }

    #[test]
    fn test_client_exchange_mtu_success() {
        let (app_channel, test_tx, test_rx) = setup_mock_channel();
        let router = BearerRouter::<_>::new(app_channel.receiver);
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                router.route_to(RouteFilter::Responses).unwrap(),
                CLIENT_PREFERRED_MTU,
            );

            // Spawn mock server driver task
            let server_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 32];
                let mut server_rx_bearer = BearerRx::new(test_rx);
                let packet = server_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                let req = AttExchangeMtuReq::new(packet.as_bytes());
                assert_eq!(
                    req.attribute_opcode().try_read().unwrap(),
                    Opcode::ATT_EXCHANGE_MTU_REQ
                );
                assert_eq!(req.client_rx_mtu().try_read().unwrap(), CLIENT_PREFERRED_MTU);

                // Reply with ExchangeMtuRsp containing 256-byte MTU
                let mut rsp_buf = [0u8; ATT_EXCHANGE_MTU_RSP_SIZE];
                let mut view = AttExchangeMtuRspMut::new(&mut rsp_buf[..]);
                view.attribute_opcode().try_write(Opcode::ATT_EXCHANGE_MTU_RSP).unwrap();
                view.server_rx_mtu().try_write(SERVER_MTU).unwrap();

                let mut server_tx_bearer = BearerTx::new(test_tx);
                let tx_packet = Packet::try_ref_from_bytes(&rsp_buf[..]).unwrap();
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
        let (app_channel, test_tx, test_rx) = setup_mock_channel();
        let router = BearerRouter::<_>::new(app_channel.receiver);
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                router.route_to(RouteFilter::Responses).unwrap(),
                CLIENT_PREFERRED_MTU,
            );

            // Mock server responds with ErrorRsp for ExchangeMtuReq with error RequestNotSupported
            let server_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 32];
                let mut server_rx_bearer = BearerRx::new(test_rx);
                let packet = server_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ATT_EXCHANGE_MTU_REQ.into());

                let mut server_tx_bearer = BearerTx::new(test_tx);
                let err_buf = make_error_rsp(
                    Opcode::ATT_EXCHANGE_MTU_REQ,
                    0,
                    ErrorCode::REQUEST_NOT_SUPPORTED,
                );
                let tx_packet = Packet::try_ref_from_bytes(&err_buf[..]).unwrap();
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
        let (app_channel, test_tx, test_rx) = setup_mock_channel();
        let router = BearerRouter::<_>::new(app_channel.receiver);
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                router.route_to(RouteFilter::Responses).unwrap(),
                CLIENT_PREFERRED_MTU,
            );

            // Mock server responds with ErrorRsp indicating InsufficientAuthentication
            let server_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 32];
                let mut server_rx_bearer = BearerRx::new(test_rx);
                let packet = server_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ATT_EXCHANGE_MTU_REQ.into());

                let mut server_tx_bearer = BearerTx::new(test_tx);
                let err_buf = make_error_rsp(
                    Opcode::ATT_EXCHANGE_MTU_REQ,
                    0,
                    ErrorCode::INSUFFICIENT_AUTHENTICATION,
                );
                let tx_packet = Packet::try_ref_from_bytes(&err_buf[..]).unwrap();
                server_tx_bearer.send(tx_packet).await.unwrap();
            });

            let client_handle = executor.spawn(async move {
                // Client must abort and propagate ClientError::ErrorResponse
                let res = client.exchange_mtu().await;
                assert_eq!(
                    res,
                    Err(ClientError::ErrorResponse(ErrorCode::INSUFFICIENT_AUTHENTICATION))
                );
            });

            executor.run_until_stalled();

            assert!(server_handle.is_finished());
            assert!(client_handle.is_finished());
        });
    }

    #[test]
    fn test_client_find_information_success() {
        let (app_channel, server_tx, server_rx) = setup_mock_channel();
        let router = BearerRouter::<_>::new(app_channel.receiver);
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                router.route_to(RouteFilter::Responses).unwrap(),
                CLIENT_PREFERRED_MTU,
            );

            // Spawn mock server driver task
            let server_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 32];
                let mut server_rx_bearer = BearerRx::new(server_rx);
                let packet = server_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ATT_FIND_INFORMATION_REQ.into());

                let req = AttFindInformationReq::new(packet.as_bytes());
                assert_eq!(req.starting_handle().try_read().unwrap(), 1);
                assert_eq!(req.ending_handle().try_read().unwrap(), 10);

                // Respond with FindInformationRsp (0x05)
                // format: 0x01 (16-bit)
                // entries:
                // Handle 1: UUID 0x2A00
                // Handle 2: UUID 0x2A24
                let mut tx_buf = [0u8; 64];
                let header = [Opcode::ATT_FIND_INFORMATION_RSP as u8, UuidFormat::Uuid16 as u8];
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
        let (app_channel, server_tx, server_rx) = setup_mock_channel();
        let router = BearerRouter::<_>::new(app_channel.receiver);
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                router.route_to(RouteFilter::Responses).unwrap(),
                CLIENT_PREFERRED_MTU,
            );

            // Spawn mock server driver task
            let server_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 32];
                let mut server_rx_bearer = BearerRx::new(server_rx);
                let packet = server_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ATT_FIND_INFORMATION_REQ.into());

                // Respond with ErrorRsp (InvalidHandle)
                let mut server_tx_bearer = BearerTx::new(server_tx);
                let err_buf =
                    make_error_rsp(Opcode::ATT_FIND_INFORMATION_REQ, 10, ErrorCode::INVALID_HANDLE);
                let tx_packet = Packet::try_ref_from_bytes(&err_buf[..]).unwrap();
                server_tx_bearer.send(tx_packet).await.unwrap();
            });

            // Client task
            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 64];
                let res = client.find_information(h(10), h(20), &mut rx_buf).await;
                assert_eq!(res, Err(ClientError::ErrorResponse(ErrorCode::INVALID_HANDLE)));
            });

            executor.run_until_stalled();
            assert!(client_handle.is_finished());
            assert!(server_handle.is_finished());
        });
    }

    #[test]
    fn test_client_find_by_type_value_success() {
        let (app_channel, server_tx, server_rx) = setup_mock_channel();
        let router = BearerRouter::<_>::new(app_channel.receiver);
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                router.route_to(RouteFilter::Responses).unwrap(),
                CLIENT_PREFERRED_MTU,
            );

            let server_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 32];
                let mut server_rx_bearer = BearerRx::new(server_rx);
                let packet = server_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ATT_FIND_BY_TYPE_VALUE_REQ.into());

                let req_header = AttFindByTypeValueReqHeader::new(packet.as_bytes());
                assert_eq!(req_header.starting_handle().try_read().unwrap(), 1);
                assert_eq!(req_header.ending_handle().try_read().unwrap(), 10);
                assert_eq!(req_header.attribute_type().try_read().unwrap(), 0x2800);
                assert_eq!(
                    &packet.as_bytes()[ATT_FIND_BY_TYPE_VALUE_REQ_HEADER_SIZE..],
                    &[0x0D, 0x18][..]
                );

                let mut tx_buf = [0u8; 64];
                let header = [Opcode::ATT_FIND_BY_TYPE_VALUE_RSP as u8];
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
        let (app_channel, server_tx, server_rx) = setup_mock_channel();
        let router = BearerRouter::<_>::new(app_channel.receiver);
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                router.route_to(RouteFilter::Responses).unwrap(),
                CLIENT_PREFERRED_MTU,
            );

            let server_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 32];
                let mut server_rx_bearer = BearerRx::new(server_rx);
                let packet = server_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ATT_FIND_BY_TYPE_VALUE_REQ.into());

                let mut server_tx_bearer = BearerTx::new(server_tx);
                let err_buf = make_error_rsp(
                    Opcode::ATT_FIND_BY_TYPE_VALUE_REQ,
                    1,
                    ErrorCode::ATTRIBUTE_NOT_FOUND,
                );
                let tx_packet = Packet::try_ref_from_bytes(&err_buf[..]).unwrap();
                server_tx_bearer.send(tx_packet).await.unwrap();
            });

            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 64];
                let res = client
                    .find_by_type_value(h(1), h(10), 0x2800, &[0x0D, 0x18], &mut rx_buf)
                    .await;
                assert_eq!(res, Err(ClientError::ErrorResponse(ErrorCode::ATTRIBUTE_NOT_FOUND)));
            });

            executor.run_until_stalled();
            assert!(client_handle.is_finished());
            assert!(server_handle.is_finished());
        });
    }

    #[test]
    fn test_client_read_success() {
        let (app_channel, server_tx, server_rx) = setup_mock_channel();
        let router = BearerRouter::<_>::new(app_channel.receiver);
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                router.route_to(RouteFilter::Responses).unwrap(),
                CLIENT_PREFERRED_MTU,
            );

            let server_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 32];
                let mut server_rx_bearer = BearerRx::new(server_rx);
                let packet = server_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ATT_READ_REQ.into());

                let req = AttReadReq::new(packet.as_bytes());
                assert_eq!(req.attribute_handle().try_read().unwrap(), 1);

                let val = b"Sunstone";
                let mut tx_buf = [0u8; 64];
                let mut builder = DynamicPacketBuilder::<_, u8>::new(
                    &mut tx_buf,
                    Header::new(Opcode::ATT_READ_RSP),
                    CLIENT_PREFERRED_MTU as usize,
                );
                builder.extend_from_slice(val).unwrap();
                let tx_packet = builder.as_packet();
                let mut server_tx_bearer = BearerTx::new(server_tx);
                server_tx_bearer.send(tx_packet).await.unwrap();
            });

            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 64];
                let val = client.read(h(1), &mut rx_buf).await.unwrap();
                let expected: &[u8] = b"Sunstone";
                assert_eq!(val, expected);
            });

            executor.run_until_stalled();
            assert!(client_handle.is_finished());
            assert!(server_handle.is_finished());
        });
    }

    #[test]
    fn test_client_read_error() {
        let (app_channel, server_tx, server_rx) = setup_mock_channel();
        let router = BearerRouter::<_>::new(app_channel.receiver);
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                router.route_to(RouteFilter::Responses).unwrap(),
                CLIENT_PREFERRED_MTU,
            );

            let server_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 32];
                let mut server_rx_bearer = BearerRx::new(server_rx);
                let packet = server_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ATT_READ_REQ.into());

                let mut server_tx_bearer = BearerTx::new(server_tx);
                let err_buf = make_error_rsp(Opcode::ATT_READ_REQ, 1, ErrorCode::INVALID_HANDLE);
                let tx_packet = Packet::try_ref_from_bytes(&err_buf[..]).unwrap();
                server_tx_bearer.send(tx_packet).await.unwrap();
            });

            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 64];
                let res = client.read(h(1), &mut rx_buf).await;
                assert_eq!(res, Err(ClientError::ErrorResponse(ErrorCode::INVALID_HANDLE)));
            });

            executor.run_until_stalled();
            assert!(client_handle.is_finished());
            assert!(server_handle.is_finished());
        });
    }

    #[test]
    fn test_client_read_blob_success() {
        let (app_channel, server_tx, server_rx) = setup_mock_channel();
        let router = BearerRouter::<_>::new(app_channel.receiver);
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                router.route_to(RouteFilter::Responses).unwrap(),
                CLIENT_PREFERRED_MTU,
            );

            let server_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 32];
                let mut server_rx_bearer = BearerRx::new(server_rx);
                let packet = server_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ATT_READ_BLOB_REQ.into());

                let req = AttReadBlobReq::new(packet.as_bytes());
                assert_eq!(req.attribute_handle().try_read().unwrap(), 1);
                assert_eq!(req.value_offset().try_read().unwrap(), 2);

                let val = b"nstone"; // Part of "Sunstone" starting at offset 2
                let mut tx_buf = [0u8; 64];
                let mut builder = DynamicPacketBuilder::<_, u8>::new(
                    &mut tx_buf,
                    Header::new(Opcode::ATT_READ_BLOB_RSP),
                    CLIENT_PREFERRED_MTU as usize,
                );
                builder.extend_from_slice(val).unwrap();
                let tx_packet = builder.as_packet();
                let mut server_tx_bearer = BearerTx::new(server_tx);
                server_tx_bearer.send(tx_packet).await.unwrap();
            });

            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 64];
                let val = client.read_blob(h(1), 2, &mut rx_buf).await.unwrap();
                let expected: &[u8] = b"nstone";
                assert_eq!(val, expected);
            });

            executor.run_until_stalled();
            assert!(client_handle.is_finished());
            assert!(server_handle.is_finished());
        });
    }

    #[test]
    fn test_client_read_blob_error() {
        let (app_channel, server_tx, server_rx) = setup_mock_channel();
        let router = BearerRouter::<_>::new(app_channel.receiver);
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                router.route_to(RouteFilter::Responses).unwrap(),
                CLIENT_PREFERRED_MTU,
            );

            let server_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 32];
                let mut server_rx_bearer = BearerRx::new(server_rx);
                let packet = server_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ATT_READ_BLOB_REQ.into());

                let mut server_tx_bearer = BearerTx::new(server_tx);
                let err_buf =
                    make_error_rsp(Opcode::ATT_READ_BLOB_REQ, 1, ErrorCode::INVALID_OFFSET);
                let tx_packet = Packet::try_ref_from_bytes(&err_buf[..]).unwrap();
                server_tx_bearer.send(tx_packet).await.unwrap();
            });

            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 64];
                let res = client.read_blob(h(1), 100, &mut rx_buf).await;
                assert_eq!(res, Err(ClientError::ErrorResponse(ErrorCode::INVALID_OFFSET)));
            });

            executor.run_until_stalled();
            assert!(client_handle.is_finished());
            assert!(server_handle.is_finished());
        });
    }

    #[test]
    fn test_client_read_by_type_success() {
        use crate::att::pdu::ReadByTypeReq;
        let (app_channel, server_tx, server_rx) = setup_mock_channel();
        let router = BearerRouter::<_>::new(app_channel.receiver);
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                router.route_to(RouteFilter::Responses).unwrap(),
                CLIENT_PREFERRED_MTU,
            );

            let uuid = Uuid::from_u16(0x2800); // Primary Service 16-bit UUID
            const VALUE_SIZE: usize = 8; // b"PrimaryS".len()
            const ENTRY_SIZE: u8 = (size_of::<AttributeHandle>() + VALUE_SIZE) as u8;

            let server_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 32];
                let mut server_rx_bearer = BearerRx::new(server_rx);
                let packet = server_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ATT_READ_BY_TYPE_REQ.into());

                let req = ReadByTypeReq::try_ref_from_bytes(&packet.data[..]).unwrap();
                assert_eq!(req.header.starting_handle.get(), 1);
                assert_eq!(req.header.ending_handle.get(), 10);
                assert_eq!(&req.attribute_type, uuid.as_bytes());

                let mut tx_buf = [0u8; 64];
                let header = Header::new(Opcode::ATT_READ_BY_TYPE_RSP);
                let mut builder = DynamicPacketBuilder::<_, u8>::new(
                    &mut tx_buf,
                    header,
                    CLIENT_PREFERRED_MTU as usize,
                );
                builder.push(ENTRY_SIZE).unwrap();
                // Push entry 1: handle = 2, value = b"PrimaryS"
                builder.push(0x02).unwrap();
                builder.push(0x00).unwrap();
                builder.extend_from_slice(b"PrimaryS").unwrap();
                // Push entry 2: handle = 6, value = b"PrimaryS"
                builder.push(0x06).unwrap();
                builder.push(0x00).unwrap();
                builder.extend_from_slice(b"PrimaryS").unwrap();

                let tx_packet = builder.as_packet();
                let mut server_tx_bearer = BearerTx::new(server_tx);
                server_tx_bearer.send(tx_packet).await.unwrap();
            });

            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 64];
                let results = client.read_by_type(h(1), h(10), &uuid, &mut rx_buf).await.unwrap();

                // Verify IntoIterator for &results
                let mut count = 0;
                for entry in &results {
                    let entry = entry.unwrap();
                    if count == 0 {
                        assert_eq!(entry.handle, h(2));
                        assert_eq!(entry.value, b"PrimaryS");
                    } else if count == 1 {
                        assert_eq!(entry.handle, h(6));
                        assert_eq!(entry.value, b"PrimaryS");
                    }
                    count += 1;
                }
                assert_eq!(count, 2);

                // Verify IntoIterator for owned results
                let mut count_owned = 0;
                for entry in results {
                    let entry = entry.unwrap();
                    if count_owned == 0 {
                        assert_eq!(entry.handle, h(2));
                        assert_eq!(entry.value, b"PrimaryS");
                    } else if count_owned == 1 {
                        assert_eq!(entry.handle, h(6));
                        assert_eq!(entry.value, b"PrimaryS");
                    }
                    count_owned += 1;
                }
                assert_eq!(count_owned, 2);
            });

            executor.run_until_stalled();
            assert!(client_handle.is_finished());
            assert!(server_handle.is_finished());
        });
    }

    #[test]
    fn test_client_read_by_type_error() {
        let (app_channel, server_tx, server_rx) = setup_mock_channel();
        let router = BearerRouter::<_>::new(app_channel.receiver);
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                router.route_to(RouteFilter::Responses).unwrap(),
                CLIENT_PREFERRED_MTU,
            );

            let server_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 32];
                let mut server_rx_bearer = BearerRx::new(server_rx);
                let packet = server_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ATT_READ_BY_TYPE_REQ.into());

                let mut server_tx_bearer = BearerTx::new(server_tx);
                let err_buf =
                    make_error_rsp(Opcode::ATT_READ_BY_TYPE_REQ, 1, ErrorCode::ATTRIBUTE_NOT_FOUND);
                let tx_packet = Packet::try_ref_from_bytes(&err_buf[..]).unwrap();
                server_tx_bearer.send(tx_packet).await.unwrap();
            });

            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 64];
                let uuid = Uuid::from_u16(0x2800);
                let res = client.read_by_type(h(1), h(10), &uuid, &mut rx_buf).await;
                assert_eq!(res, Err(ClientError::ErrorResponse(ErrorCode::ATTRIBUTE_NOT_FOUND)));
            });

            executor.run_until_stalled();
            assert!(client_handle.is_finished());
            assert!(server_handle.is_finished());
        });
    }

    #[test]
    fn test_client_read_by_group_type_success() {
        use crate::att::pdu::ReadByGroupTypeReq;
        let (app_channel, server_tx, server_rx) = setup_mock_channel();
        let router = BearerRouter::<_>::new(app_channel.receiver);
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                router.route_to(RouteFilter::Responses).unwrap(),
                CLIENT_PREFERRED_MTU,
            );

            let uuid = Uuid::from_u16(0x2800); // Primary Service 16-bit UUID
            const VALUE_SIZE: usize = 2; // service UUID (0x1800/0x1801 is 2 bytes)
            let group_header_size = size_of::<ReadByGroupTypeRspEntryHeader>();
            let entry_size = (group_header_size + VALUE_SIZE) as u8;

            let server_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 32];
                let mut server_rx_bearer = BearerRx::new(server_rx);
                let packet = server_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ATT_READ_BY_GROUP_TYPE_REQ.into());

                let req = ReadByGroupTypeReq::try_ref_from_bytes(&packet.data[..]).unwrap();
                assert_eq!(req.header.starting_handle.get(), 1);
                assert_eq!(req.header.ending_handle.get(), 10);
                assert_eq!(&req.attribute_type, uuid.as_bytes());

                let mut tx_buf = [0u8; 64];
                let header = Header::new(Opcode::ATT_READ_BY_GROUP_TYPE_RSP);
                let mut builder = DynamicPacketBuilder::<_, u8>::new(
                    &mut tx_buf,
                    header,
                    CLIENT_PREFERRED_MTU as usize,
                );
                builder.push(entry_size).unwrap();
                // Push entry 1: handle = 2, group end = 5, value = 0x1801 (\x01\x18)
                builder.push(0x02).unwrap();
                builder.push(0x00).unwrap();
                builder.push(0x05).unwrap();
                builder.push(0x00).unwrap();
                builder.extend_from_slice(b"\x01\x18").unwrap();
                // Push entry 2: handle = 6, group end = 10, value = 0x1800 (\x00\x18)
                builder.push(0x06).unwrap();
                builder.push(0x00).unwrap();
                builder.push(0x0a).unwrap();
                builder.push(0x00).unwrap();
                builder.extend_from_slice(b"\x00\x18").unwrap();

                let tx_packet = builder.as_packet();
                let mut server_tx_bearer = BearerTx::new(server_tx);
                server_tx_bearer.send(tx_packet).await.unwrap();
            });

            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 64];
                let results =
                    client.read_by_group_type(h(1), h(10), &uuid, &mut rx_buf).await.unwrap();

                let mut iter = results.iter();
                let e1 = iter.next().unwrap().unwrap();
                assert_eq!(e1.handle, h(2));
                assert_eq!(e1.end_group_handle, h(5));
                assert_eq!(e1.value, b"\x01\x18");

                let e2 = iter.next().unwrap().unwrap();
                assert_eq!(e2.handle, h(6));
                assert_eq!(e2.end_group_handle, h(10));
                assert_eq!(e2.value, b"\x00\x18");

                assert!(iter.next().is_none());
            });

            executor.run_until_stalled();
            assert!(client_handle.is_finished());
            assert!(server_handle.is_finished());
        });
    }

    #[test]
    fn test_client_read_by_group_type_error() {
        let (app_channel, server_tx, server_rx) = setup_mock_channel();
        let router = BearerRouter::<_>::new(app_channel.receiver);
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                router.route_to(RouteFilter::Responses).unwrap(),
                CLIENT_PREFERRED_MTU,
            );

            let server_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 32];
                let mut server_rx_bearer = BearerRx::new(server_rx);
                let packet = server_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ATT_READ_BY_GROUP_TYPE_REQ.into());

                // Respond with ErrorRsp (UnsupportedGroupType)
                let mut server_tx_bearer = BearerTx::new(server_tx);
                let err_buf = make_error_rsp(
                    Opcode::ATT_READ_BY_GROUP_TYPE_REQ,
                    1,
                    ErrorCode::UNSUPPORTED_GROUP_TYPE,
                );
                let tx_packet = Packet::try_ref_from_bytes(&err_buf[..]).unwrap();
                server_tx_bearer.send(tx_packet).await.unwrap();
            });

            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 64];
                let uuid = Uuid::from_u16(0x2800);
                let res = client.read_by_group_type(h(1), h(10), &uuid, &mut rx_buf).await;
                assert_eq!(res, Err(ClientError::ErrorResponse(ErrorCode::UNSUPPORTED_GROUP_TYPE)));
            });

            executor.run_until_stalled();
            assert!(client_handle.is_finished());
            assert!(server_handle.is_finished());
        });
    }

    #[test]
    fn test_client_write_success() {
        let (app_channel, test_tx, test_rx) = setup_mock_channel();
        let router = BearerRouter::<_>::new(app_channel.receiver);
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                router.route_to(RouteFilter::Responses).unwrap(),
                CLIENT_PREFERRED_MTU,
            );

            // Server driver task
            let server_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 128];
                let mut server_rx_bearer = BearerRx::new(test_rx);
                let mut server_tx_bearer = BearerTx::new(test_tx);

                // 1. Await write request
                let packet = server_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ATT_WRITE_REQ.into());
                let req = AttWriteCmd::new(packet.as_bytes());
                assert_eq!(req.attribute_handle().try_read().unwrap(), 10);
                assert_eq!(&packet.as_bytes()[ATT_WRITE_REQ_HEADER_SIZE..], b"Sunstone");

                // 2. Respond with empty WriteRsp
                let mut rsp_buf = [0u8; 1];
                let mut rsp_view = AttHeaderMut::new(&mut rsp_buf[..]);
                rsp_view.attribute_opcode().try_write(Opcode::ATT_WRITE_RSP).unwrap();
                let tx_packet = Packet::try_ref_from_bytes(&rsp_buf[..]).unwrap();
                server_tx_bearer.send(tx_packet).await.unwrap();
            });

            // Client driver task
            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); CLIENT_PREFERRED_MTU as usize];
                client.write(h(10), b"Sunstone", &mut rx_buf).await.unwrap();
            });

            executor.run_until_stalled();
            assert!(server_handle.is_finished());
            assert!(client_handle.is_finished());
        });
    }

    #[test]
    fn test_client_write_error() {
        let (app_channel, test_tx, test_rx) = setup_mock_channel();
        let router = BearerRouter::<_>::new(app_channel.receiver);
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                router.route_to(RouteFilter::Responses).unwrap(),
                CLIENT_PREFERRED_MTU,
            );

            // Server driver task
            let server_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 128];
                let mut server_rx_bearer = BearerRx::new(test_rx);
                let mut server_tx_bearer = BearerTx::new(test_tx);

                // 1. Await write request
                let packet = server_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ATT_WRITE_REQ.into());
                let req = AttWriteCmd::new(packet.as_bytes());
                assert_eq!(req.attribute_handle().try_read().unwrap(), 10);

                // 2. Respond with ErrorRsp (WriteNotPermitted)
                let err_buf =
                    make_error_rsp(Opcode::ATT_WRITE_REQ, 10, ErrorCode::WRITE_NOT_PERMITTED);
                let tx_packet = Packet::try_ref_from_bytes(&err_buf[..]).unwrap();
                server_tx_bearer.send(tx_packet).await.unwrap();
            });

            // Client driver task
            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); CLIENT_PREFERRED_MTU as usize];
                let result = client.write(h(10), b"Sunstone", &mut rx_buf).await;
                assert_eq!(
                    result.err(),
                    Some(ClientError::ErrorResponse(ErrorCode::WRITE_NOT_PERMITTED))
                );
            });

            executor.run_until_stalled();
            assert!(server_handle.is_finished());
            assert!(client_handle.is_finished());
        });
    }

    #[test]
    fn test_client_write_command_success() {
        let (app_channel, _test_tx, test_rx) = setup_mock_channel();
        let router = BearerRouter::<_>::new(app_channel.receiver);
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                router.route_to(RouteFilter::Responses).unwrap(),
                CLIENT_PREFERRED_MTU,
            );

            // Server driver task
            let server_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 128];
                let mut server_rx_bearer = BearerRx::new(test_rx);
                let packet = server_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ATT_WRITE_CMD.into());
                let req = AttWriteCmd::new(packet.as_bytes());
                assert_eq!(req.attribute_handle().try_read().unwrap(), 12);
                assert_eq!(&packet.as_bytes()[ATT_WRITE_CMD_HEADER_SIZE..], b"SunstoneCmd");
            });

            // Client driver task
            let client_handle = executor.spawn(async move {
                client.write_command(h(12), b"SunstoneCmd").await.unwrap();
            });

            executor.run_until_stalled();
            assert!(server_handle.is_finished());
            assert!(client_handle.is_finished());
        });
    }

    #[test]
    fn test_client_write_command_link_closed() {
        let (app_channel, _test_tx, test_rx) = setup_mock_channel();
        let router = BearerRouter::<_>::new(app_channel.receiver);
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                router.route_to(RouteFilter::Responses).unwrap(),
                CLIENT_PREFERRED_MTU,
            );

            // Drop test_rx to simulate link closure
            drop(test_rx);

            // Client driver task
            let client_handle = executor.spawn(async move {
                let result = client.write_command(h(12), b"SunstoneCmd").await;
                assert_eq!(result.err(), Some(ClientError::LinkClosed));
            });

            executor.run_until_stalled();
            assert!(client_handle.is_finished());
        });
    }

    #[test]
    fn test_client_prepare_write_success() {
        let (app_channel, test_tx, test_rx) = setup_mock_channel();
        let router = BearerRouter::<_>::new(app_channel.receiver);
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                router.route_to(RouteFilter::Responses).unwrap(),
                CLIENT_PREFERRED_MTU,
            );

            let server_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 128];
                let mut server_rx = BearerRx::new(test_rx);
                let mut server_tx = BearerTx::new(test_tx);

                // Receive PrepareWriteReq
                let packet = server_rx.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ATT_PREPARE_WRITE_REQ.into());
                let req = AttPrepareWriteHeader::new(packet.as_bytes());
                assert_eq!(req.attribute_handle().try_read().unwrap(), 10);
                assert_eq!(req.value_offset().try_read().unwrap(), 0);
                assert_eq!(&packet.as_bytes()[ATT_PREPARE_WRITE_HEADER_SIZE..], b"Part1");

                // Echo back PrepareWriteRsp
                let mut rsp_buf = [0u8; 128];
                rsp_buf[..packet.as_bytes().len()].copy_from_slice(packet.as_bytes());
                let mut rsp_view = AttHeaderMut::new(&mut rsp_buf[..size_of::<Header>()]);
                rsp_view.attribute_opcode().try_write(Opcode::ATT_PREPARE_WRITE_RSP).unwrap();
                let tx_packet =
                    Packet::try_ref_from_bytes(&rsp_buf[..packet.as_bytes().len()]).unwrap();
                server_tx.send(tx_packet).await.unwrap();
            });

            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 128];
                client.prepare_write(h(10), 0, b"Part1", &mut rx_buf).await.unwrap();
            });

            executor.run_until_stalled();
            assert!(server_handle.is_finished());
            assert!(client_handle.is_finished());
        });
    }

    #[test]
    fn test_client_prepare_write_error() {
        let (app_channel, test_tx, test_rx) = setup_mock_channel();
        let router = BearerRouter::<_>::new(app_channel.receiver);
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                router.route_to(RouteFilter::Responses).unwrap(),
                CLIENT_PREFERRED_MTU,
            );

            let server_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 128];
                let mut server_rx = BearerRx::new(test_rx);
                let mut server_tx = BearerTx::new(test_tx);

                let _packet = server_rx.next_packet(&mut rx_buf).await.unwrap();

                // Respond with ErrorRsp (InvalidOffset)
                let err_buf =
                    make_error_rsp(Opcode::ATT_PREPARE_WRITE_REQ, 10, ErrorCode::INVALID_OFFSET);
                let tx_packet = Packet::try_ref_from_bytes(&err_buf[..]).unwrap();
                server_tx.send(tx_packet).await.unwrap();
            });

            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 128];
                let result = client.prepare_write(h(10), 5, b"Part1", &mut rx_buf).await;
                assert_eq!(
                    result.err(),
                    Some(ClientError::ErrorResponse(ErrorCode::INVALID_OFFSET))
                );
            });

            executor.run_until_stalled();
            assert!(server_handle.is_finished());
            assert!(client_handle.is_finished());
        });
    }

    #[test]
    fn test_client_prepare_write_invalid_echo() {
        let (app_channel, test_tx, test_rx) = setup_mock_channel();
        let router = BearerRouter::<_>::new(app_channel.receiver);
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                router.route_to(RouteFilter::Responses).unwrap(),
                CLIENT_PREFERRED_MTU,
            );

            let server_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 128];
                let mut server_rx = BearerRx::new(test_rx);
                let mut server_tx = BearerTx::new(test_tx);

                let _packet = server_rx.next_packet(&mut rx_buf).await.unwrap();

                // Respond with mismatched echoed payload (mismatched offset 1 instead of 0)
                const PART_VAL: &[u8] = b"Part1";
                let mut rsp_buf = [0u8; ATT_PREPARE_WRITE_HEADER_SIZE + PART_VAL.len()];
                let mut view =
                    AttPrepareWriteHeaderMut::new(&mut rsp_buf[..ATT_PREPARE_WRITE_HEADER_SIZE]);
                view.attribute_opcode().try_write(Opcode::ATT_PREPARE_WRITE_RSP).unwrap();
                view.attribute_handle().try_write(10).unwrap();
                view.value_offset().try_write(1).unwrap();
                rsp_buf[ATT_PREPARE_WRITE_HEADER_SIZE..].copy_from_slice(PART_VAL);
                let tx_packet = Packet::try_ref_from_bytes(&rsp_buf[..]).unwrap();
                server_tx.send(tx_packet).await.unwrap();
            });

            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 128];
                let result = client.prepare_write(h(10), 0, b"Part1", &mut rx_buf).await;
                assert_eq!(result.err(), Some(ClientError::InvalidIncomingData));
            });

            executor.run_until_stalled();
            assert!(server_handle.is_finished());
            assert!(client_handle.is_finished());
        });
    }

    #[test]
    fn test_client_execute_write_success() {
        let (app_channel, test_tx, test_rx) = setup_mock_channel();
        let router = BearerRouter::<_>::new(app_channel.receiver);
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                router.route_to(RouteFilter::Responses).unwrap(),
                CLIENT_PREFERRED_MTU,
            );

            let server_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 128];
                let mut server_rx = BearerRx::new(test_rx);
                let mut server_tx = BearerTx::new(test_tx);

                let packet = server_rx.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ATT_EXECUTE_WRITE_REQ.into());
                let req = AttExecuteWriteReq::new(packet.as_bytes());
                assert_eq!(req.flags().try_read().unwrap(), ExecuteWriteFlags::WRITE);

                let mut rsp_buf = [0u8; ATT_EXECUTE_WRITE_RSP_SIZE];
                let mut rsp_view = AttHeaderMut::new(&mut rsp_buf[..]);
                rsp_view.attribute_opcode().try_write(Opcode::ATT_EXECUTE_WRITE_RSP).unwrap();
                let tx_packet = Packet::try_ref_from_bytes(&rsp_buf[..]).unwrap();
                server_tx.send(tx_packet).await.unwrap();
            });

            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 128];
                client.execute_write(ExecuteWriteFlags::WRITE, &mut rx_buf).await.unwrap();
            });

            executor.run_until_stalled();
            assert!(server_handle.is_finished());
            assert!(client_handle.is_finished());
        });
    }

    #[test]
    fn test_client_execute_write_error() {
        let (app_channel, _test_tx, test_rx) = setup_mock_channel();
        let router = BearerRouter::<_>::new(app_channel.receiver);
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                router.route_to(RouteFilter::Responses).unwrap(),
                CLIENT_PREFERRED_MTU,
            );
            drop(test_rx); // Simulates LinkClosed

            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 128];
                let result = client.execute_write(ExecuteWriteFlags::WRITE, &mut rx_buf).await;
                assert_eq!(result.err(), Some(ClientError::LinkClosed));
            });

            executor.run_until_stalled();
            assert!(client_handle.is_finished());
        });
    }

    #[test]
    fn test_client_indication_auto_confirm_success() {
        let (app_channel, test_tx, test_rx) = setup_mock_channel();
        let router = BearerRouter::<_>::new(app_channel.receiver);

        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let mut event_stream =
                ServerEventStream::new(&router, BearerTx::new(app_channel.sender))
                    .expect("server event stream created");

            let server_handle = executor.spawn(async move {
                let mut server_tx_bearer = BearerTx::new(test_tx);
                let mut ind_buf = [0u8; ATT_HANDLE_VALUE_IND_HEADER_SIZE];
                let mut view = AttHandleValueIndHeaderMut::new(&mut ind_buf);
                view.attribute_opcode().try_write(Opcode::ATT_HANDLE_VALUE_IND).unwrap();
                view.attribute_handle().try_write(0x1234).unwrap();
                let tx_packet = Packet::try_ref_from_bytes(&ind_buf).unwrap();
                server_tx_bearer.send(tx_packet).await.unwrap();

                let mut rx_buf = [MaybeUninit::uninit(); 32];
                let mut server_rx_bearer = BearerRx::new(test_rx);
                let packet = server_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                assert_eq!(packet.header.opcode, Opcode::ATT_HANDLE_VALUE_CFM.into());
            });

            let client_handle = executor.spawn(async move {
                let mut rx_buf = [MaybeUninit::uninit(); 32];
                let event = event_stream.next(&mut rx_buf).await.unwrap();
                assert_eq!(event.handle().get(), 0x1234);
            });

            executor.run_until_stalled();
            assert!(server_handle.is_finished());
            assert!(client_handle.is_finished());
        });
    }
}
