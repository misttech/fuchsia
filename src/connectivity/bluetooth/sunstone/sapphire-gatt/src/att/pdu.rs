// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Attribute Protocol (ATT) Packet Data Unit (PDU) definitions and parsing utilities.

use core::cmp::min;
use core::mem::size_of;
use sapphire_common::Uuid;
pub use sapphire_emboss::att::{
    AttExecuteWriteFlag as ExecuteWriteFlags, AttFindInformationRspHeader, AttHandlesInformation,
    AttInformationData16, AttInformationData128, AttOpcode as Opcode, AttUuidFormat as UuidFormat,
    ErrorCode,
};
use zerocopy::byteorder::little_endian::U16;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, TryFromBytes};

/// Helper to determine the ATT UUID format from a UUID.
pub fn uuid_to_format(uuid: &Uuid) -> UuidFormat {
    if uuid.is_u16() { UuidFormat::BIT16 } else { UuidFormat::BIT128 }
}

/// A parsed view into any incoming packet's header.
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C, packed)]
pub struct Header {
    pub opcode: u8,
}

impl Header {
    pub fn new(opcode: Opcode) -> Self {
        Self { opcode: opcode as u8 }
    }
}

impl From<Opcode> for Header {
    fn from(opcode: Opcode) -> Self {
        Self::new(opcode)
    }
}

/// A generic unsized ATT packet containing a verified header and variable payload data.
#[derive(TryFromBytes, KnownLayout, Immutable, IntoBytes, Debug)]
#[repr(C)]
pub struct Packet {
    pub header: Header,
    pub data: [u8],
}

/// Fixed protocol wire sizes (in bytes) for Emboss ATT PDUs.
///
/// (see Vol 3, Part F, Section 3.4)
pub const ATT_HEADER_SIZE: usize = 1;
pub const ATT_ERROR_RSP_SIZE: usize = 5;
pub const ATT_EXCHANGE_MTU_REQ_SIZE: usize = 3;
pub const ATT_EXCHANGE_MTU_RSP_SIZE: usize = 3;
pub const ATT_FIND_INFORMATION_REQ_SIZE: usize = 5;
pub const ATT_FIND_INFORMATION_RSP_HEADER_SIZE: usize = 2;
pub const ATT_INFORMATION_DATA_16_SIZE: usize = 4;
pub const ATT_INFORMATION_DATA_128_SIZE: usize = 18;
pub const ATT_FIND_BY_TYPE_VALUE_REQ_HEADER_SIZE: usize = 7;
pub const ATT_HANDLES_INFORMATION_SIZE: usize = 4;
pub const ATT_READ_REQ_SIZE: usize = 3;
pub const ATT_READ_BLOB_REQ_SIZE: usize = 5;
pub const ATT_READ_BY_TYPE_REQ_HEADER_SIZE: usize = 5;
pub const ATT_READ_BY_GROUP_TYPE_REQ_HEADER_SIZE: usize = 5;
pub const ATT_WRITE_REQ_HEADER_SIZE: usize = 3;
pub const ATT_WRITE_RSP_SIZE: usize = 1;
pub const ATT_WRITE_CMD_HEADER_SIZE: usize = 3;
pub const ATT_PREPARE_WRITE_HEADER_SIZE: usize = 5;
pub const ATT_EXECUTE_WRITE_REQ_SIZE: usize = 2;
pub const ATT_EXECUTE_WRITE_RSP_SIZE: usize = 1;
pub const ATT_HANDLE_VALUE_NTF_HEADER_SIZE: usize = 3;
pub const ATT_HANDLE_VALUE_IND_HEADER_SIZE: usize = 3;
pub const ATT_HANDLE_VALUE_CFM_SIZE: usize = 1;

/// Result of a Find Information procedure (see Vol 3, Part F, Section 3.4.3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveredInformation<'a> {
    Uuid16(PduList<'a, ATT_INFORMATION_DATA_16_SIZE>),
    Uuid128(PduList<'a, ATT_INFORMATION_DATA_128_SIZE>),
}

/// A zero-copy list of fixed-size PDU entries (N bytes per element).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PduList<'a, const N: usize>(&'a [u8]);

impl<'a, const N: usize> PduList<'a, N> {
    pub fn new(data: &'a [u8]) -> Option<Self> {
        if data.is_empty() || data.len() % N != 0 { None } else { Some(Self(data)) }
    }

    pub fn len(&self) -> usize {
        self.0.len() / N
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&'a [u8]> {
        let offset = index * N;
        self.0.get(offset..offset + N)
    }

    pub fn iter(&self) -> impl Iterator<Item = &'a [u8]> {
        self.0.chunks_exact(N)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushError {
    BufferFull,
}

/// A generic, stateful builder for serializing dynamically sized ATT response PDUs (DSTs)
/// containing a variable-length list of entries of type `T` directly into a byte buffer.
///
/// The caller must serialize any PDU-specific headers into the buffer *before* initializing
/// the builder, specifying the `header_len`. The builder will write entries starting after
/// the header, ensuring that entries do not exceed the buffer capacity or the negotiated MTU.
pub struct DynamicPacketBuilder<'a, H, T> {
    buf: &'a mut [u8],
    offset: usize,
    limit: usize,
    _phantom: core::marker::PhantomData<(H, T)>,
}

impl<'a, H: IntoBytes + Immutable, T: IntoBytes + Immutable + KnownLayout>
    DynamicPacketBuilder<'a, H, T>
{
    /// Creates a new builder for serializing entries of type `T` into the buffer.
    ///
    /// Writes the `header` directly into the start of the buffer.
    /// Asserts that the buffer and MTU limit are large enough to contain the header.
    pub fn new(buf: &'a mut [u8], header: H, mtu: usize) -> Self {
        let header_len = size_of::<H>();
        assert!(buf.len() >= header_len, "buffer too small for header");
        assert!(mtu >= header_len, "MTU too small for header");

        buf[..header_len].copy_from_slice(header.as_bytes());

        let limit = min(buf.len(), mtu);
        Self { buf, offset: header_len, limit, _phantom: core::marker::PhantomData }
    }

    /// Returns the current serialized length of the packet.
    pub fn len(&self) -> usize {
        self.offset
    }

    /// Attempts to serialize a single entry into the buffer.
    ///
    /// Returns `Err(PushError::BufferFull)` if adding the entry would exceed the negotiated
    /// MTU limit or the buffer capacity.
    pub fn push(&mut self, entry: T) -> Result<(), PushError> {
        let entry_size = size_of::<T>();
        if self.offset + entry_size > self.limit {
            return Err(PushError::BufferFull);
        }
        self.buf[self.offset..self.offset + entry_size].copy_from_slice(entry.as_bytes());
        self.offset += entry_size;
        Ok(())
    }

    /// Attempts to serialize a slice of entries into the buffer.
    ///
    /// Returns `Err(PushError::BufferFull)` if adding the entries would exceed the negotiated
    /// MTU limit or the buffer capacity.
    pub fn extend_from_slice(&mut self, entries: &[T]) -> Result<(), PushError> {
        let entries_size = entries.len() * size_of::<T>();
        if self.offset + entries_size > self.limit {
            return Err(PushError::BufferFull);
        }
        self.buf[self.offset..self.offset + entries_size].copy_from_slice(entries.as_bytes());
        self.offset += entries_size;
        Ok(())
    }

    /// Consumes the builder and returns the serialized packet view of the written data.
    pub fn as_packet(self) -> &'a Packet {
        Packet::try_ref_from_bytes(&self.buf[..self.offset]).expect(
            "Programming error: serialized DynamicPacketBuilder violates Packet layout constraints.",
        )
    }
}

/// Parameters for Read By Type Request PDU Header (OpCode = 0x08)
///
/// see Bluetooth Core Spec v6.0 (Vol 3, Part F, Section 3.4.4.7).
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C, packed)]
pub struct ReadByTypeReqHeader {
    pub starting_handle: U16,
    pub ending_handle: U16,
}

/// The complete Read By Type Request PDU (OpCode = 0x08).
///
/// see Bluetooth Core Spec v6.0 (Vol 3, Part F, Section 3.4.4.7).
#[derive(TryFromBytes, KnownLayout, Immutable, IntoBytes, Debug)]
#[repr(C)]
pub struct ReadByTypeReq {
    pub header: ReadByTypeReqHeader,
    pub attribute_type: [u8], // 2 bytes (16-bit UUID) or 16 bytes (128-bit UUID)
}

/// The complete Read By Type Response PDU (OpCode = 0x09).
///
/// see Bluetooth Core Spec v6.0 (Vol 3, Part F, Section 3.4.4.8).
///
/// NOTE: The individual elements inside `attribute_data_list` are not represented
/// as static structs because they contain variable-length attribute values. Instead,
/// the server packs them dynamically, and the client parses them using an iterator.
#[derive(TryFromBytes, KnownLayout, Immutable, IntoBytes, Debug)]
#[repr(C)]
pub struct ReadByTypeRsp {
    pub length: u8,
    pub attribute_data_list: [u8],
}

/// Parameters for Read By Group Type Request PDU Header (OpCode = 0x10)
///
/// see Bluetooth Core Spec v6.0 (Vol 3, Part F, Section 3.4.4.9).
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C, packed)]
pub struct ReadByGroupTypeReqHeader {
    pub starting_handle: U16,
    pub ending_handle: U16,
}

/// The complete Read By Group Type Request PDU (OpCode = 0x10).
///
/// see Bluetooth Core Spec v6.0 (Vol 3, Part F, Section 3.4.4.9).
#[derive(TryFromBytes, KnownLayout, Immutable, IntoBytes, Debug)]
#[repr(C)]
pub struct ReadByGroupTypeReq {
    pub header: ReadByGroupTypeReqHeader,
    pub attribute_type: [u8], // 2 bytes (16-bit UUID) or 16 bytes (128-bit UUID)
}

/// The header format for each entry inside the Read By Group Type Response's attribute data list.
///
/// see Bluetooth Core Spec v6.0 (Vol 3, Part F, Section 3.4.4.10).
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C, packed)]
pub struct ReadByGroupTypeRspEntryHeader {
    pub attribute_handle: U16,
    pub end_group_handle: U16,
}

/// The complete Read By Group Type Response PDU (OpCode = 0x11).
///
/// see Bluetooth Core Spec v6.0 (Vol 3, Part F, Section 3.4.4.10).
#[derive(TryFromBytes, KnownLayout, Immutable, IntoBytes, Debug)]
#[repr(C)]
pub struct ReadByGroupTypeRsp {
    pub length: u8,
    pub attribute_data_list: [u8],
}

#[cfg(test)]
mod tests {
    use super::*;
    use sapphire_emboss::att::{
        AttErrorRsp, AttExchangeMtuReq, AttExchangeMtuRsp, AttExecuteWriteReq,
        AttFindByTypeValueReqHeader, AttFindInformationReq, AttHandleValueIndHeader,
        AttHandleValueNtfHeader, AttPrepareWriteHeader, AttReadBlobReq,
        AttReadByGroupTypeReqHeader, AttReadByTypeReqHeader, AttReadReq, AttWriteCmd,
    };

    #[test]
    fn test_exchange_mtu_req() {
        let req_bytes = [0x02, 0x00, 0x02]; // 512 in little endian
        let view = AttExchangeMtuReq::new(&req_bytes[..]);
        assert_eq!(view.attribute_opcode().try_read().unwrap(), Opcode::ATT_EXCHANGE_MTU_REQ);
        assert_eq!(view.client_rx_mtu().try_read().unwrap(), 512);

        let short_view = AttExchangeMtuReq::new(&req_bytes[..1]);
        assert!(short_view.client_rx_mtu().try_read().is_err());
    }

    #[test]
    fn test_exchange_mtu_rsp() {
        let rsp_bytes = [0x03, 0x00, 0x01]; // 256 in little endian
        let view = AttExchangeMtuRsp::new(&rsp_bytes[..]);
        assert_eq!(view.attribute_opcode().try_read().unwrap(), Opcode::ATT_EXCHANGE_MTU_RSP);
        assert_eq!(view.server_rx_mtu().try_read().unwrap(), 256);
    }

    #[test]
    fn test_read_req() {
        let req_bytes = [0x0A, 0x01, 0x00]; // opcode 0x0A, handle 1 in little endian
        let view = AttReadReq::new(&req_bytes[..]);
        assert_eq!(view.attribute_opcode().try_read().unwrap(), Opcode::ATT_READ_REQ);
        assert_eq!(view.attribute_handle().try_read().unwrap(), 1);

        let short_view = AttReadReq::new(&req_bytes[..1]);
        assert!(short_view.attribute_handle().try_read().is_err());
    }

    #[test]
    fn test_read_blob_req() {
        let req_bytes = [0x0C, 0x01, 0x00, 0x02, 0x00]; // opcode 0x0C, handle 1, offset 2
        let view = AttReadBlobReq::new(&req_bytes[..]);
        assert_eq!(view.attribute_opcode().try_read().unwrap(), Opcode::ATT_READ_BLOB_REQ);
        assert_eq!(view.attribute_handle().try_read().unwrap(), 1);
        assert_eq!(view.value_offset().try_read().unwrap(), 2);

        let short_view = AttReadBlobReq::new(&req_bytes[..2]);
        assert!(short_view.value_offset().try_read().is_err());
    }

    #[test]
    fn test_read_by_type_req() {
        let req_bytes = [0x08, 0x01, 0x00, 0x05, 0x00]; // opcode 0x08, start 0x0001, end 0x0005
        let view = AttReadByTypeReqHeader::new(&req_bytes[..]);
        assert_eq!(view.attribute_opcode().try_read().unwrap(), Opcode::ATT_READ_BY_TYPE_REQ);
        assert_eq!(view.starting_handle().try_read().unwrap(), 1);
        assert_eq!(view.ending_handle().try_read().unwrap(), 5);

        let short_view = AttReadByTypeReqHeader::new(&req_bytes[..2]);
        assert!(short_view.ending_handle().try_read().is_err());
    }

    #[test]
    fn test_read_by_type_rsp_layout() {
        let rsp_bytes = [0x0A, 0x01, 0x02, 0x03]; // length = 10, data = [1, 2, 3]
        let parsed = ReadByTypeRsp::try_ref_from_bytes(&rsp_bytes[..]).unwrap();
        assert_eq!(parsed.length, 10);
        assert_eq!(parsed.attribute_data_list, [0x01, 0x02, 0x03]);
    }

    #[test]
    fn test_read_by_group_type_req() {
        let req_bytes = [0x10, 0x01, 0x00, 0x05, 0x00]; // opcode 0x10, start 0x0001, end 0x0005
        let view = AttReadByGroupTypeReqHeader::new(&req_bytes[..]);
        assert_eq!(view.attribute_opcode().try_read().unwrap(), Opcode::ATT_READ_BY_GROUP_TYPE_REQ);
        assert_eq!(view.starting_handle().try_read().unwrap(), 1);
        assert_eq!(view.ending_handle().try_read().unwrap(), 5);

        let short_view = AttReadByGroupTypeReqHeader::new(&req_bytes[..2]);
        assert!(short_view.ending_handle().try_read().is_err());
    }

    #[test]
    fn test_read_by_group_type_rsp_layout() {
        let rsp_bytes = [0x0A, 0x01, 0x02, 0x03, 0x04, 0x05]; // length = 10, data = [1, 2, 3, 4, 5]
        let parsed = ReadByGroupTypeRsp::try_ref_from_bytes(&rsp_bytes[..]).unwrap();
        assert_eq!(parsed.length, 10);
        assert_eq!(parsed.attribute_data_list, [0x01, 0x02, 0x03, 0x04, 0x05]);
    }

    #[test]
    fn test_find_information_req() {
        let req_bytes = [0x04, 0x01, 0x00, 0xff, 0xff]; // opcode 0x04, start 0x0001, end 0xffff
        let view = AttFindInformationReq::new(&req_bytes[..]);
        assert_eq!(view.attribute_opcode().try_read().unwrap(), Opcode::ATT_FIND_INFORMATION_REQ);
        assert_eq!(view.starting_handle().try_read().unwrap(), 1);
        assert_eq!(view.ending_handle().try_read().unwrap(), 0xffff);

        let short_view = AttFindInformationReq::new(&req_bytes[..2]);
        assert!(short_view.ending_handle().try_read().is_err());
    }

    #[test]
    fn test_find_information_rsp_header() {
        let hdr_bytes_16 = [0x05, 0x01]; // opcode 0x05, format 0x01
        let view_16 = AttFindInformationRspHeader::new(&hdr_bytes_16[..]);
        assert_eq!(
            view_16.attribute_opcode().try_read().unwrap(),
            Opcode::ATT_FIND_INFORMATION_RSP
        );
        assert_eq!(view_16.format().try_read().unwrap(), UuidFormat::BIT16);

        let hdr_bytes_128 = [0x05, 0x02]; // opcode 0x05, format 0x02
        let view_128 = AttFindInformationRspHeader::new(&hdr_bytes_128[..]);
        assert_eq!(view_128.format().try_read().unwrap(), UuidFormat::BIT128);

        // Rejects invalid format
        let invalid_bytes = [0x05, 0x03];
        let invalid_view = AttFindInformationRspHeader::new(&invalid_bytes[..]);
        assert!(invalid_view.format().try_read().is_err());
    }

    #[test]
    fn test_information_data_16_slice_cast() {
        let data_bytes = [
            0x01, 0x00, 0x00, 0x2a, // handle 1, UUID 0x2A00
        ];
        let view = AttInformationData16::new(&data_bytes[..]);
        assert_eq!(view.attribute_handle().try_read().unwrap(), 1);
        assert_eq!(view.uuid().try_read().unwrap(), 0x2A00);
    }

    #[test]
    fn test_information_data_128_slice_cast() {
        let data_bytes = [
            0x0a, 0x00, // handle 10
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, // UUID
        ];
        let view = AttInformationData128::new(&data_bytes[..]);
        assert_eq!(view.attribute_handle().try_read().unwrap(), 10);
    }

    #[test]
    fn test_find_information_rsp_decoding() {
        let bytes_16 = [1, 0, 0, 0x2A]; // handle 1, UUID 0x2A00
        let list = PduList::<ATT_INFORMATION_DATA_16_SIZE>::new(&bytes_16[..]).unwrap();
        let discovered = DiscoveredInformation::Uuid16(list);
        match discovered {
            DiscoveredInformation::Uuid16(entries) => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries.get(0), Some(&bytes_16[..]));
            }
            _ => panic!("Expected Uuid16"),
        }
    }

    #[test]
    fn test_find_by_type_value_req() {
        let req_bytes = [0x06, 0x01, 0x00, 0x0a, 0x00, 0x00, 0x28]; // opcode 0x06, start 1, end 10, type 0x2800
        let view = AttFindByTypeValueReqHeader::new(&req_bytes[..]);
        assert_eq!(view.attribute_opcode().try_read().unwrap(), Opcode::ATT_FIND_BY_TYPE_VALUE_REQ);
        assert_eq!(view.starting_handle().try_read().unwrap(), 1);
        assert_eq!(view.ending_handle().try_read().unwrap(), 10);
        assert_eq!(view.attribute_type().try_read().unwrap(), 0x2800);

        let short_view = AttFindByTypeValueReqHeader::new(&req_bytes[..4]);
        assert!(short_view.attribute_type().try_read().is_err());
    }

    #[test]
    fn test_handles_information_slice_cast() {
        let rsp_bytes = [0x01, 0x00, 0x05, 0x00]; // handle 1, end_handle 5
        let view = AttHandlesInformation::new(&rsp_bytes[..]);
        assert_eq!(view.attribute_handle().try_read().unwrap(), 1);
        assert_eq!(view.group_end_handle().try_read().unwrap(), 5);
    }

    #[test]
    fn test_error_rsp() {
        let err_bytes = [0x01, 0x02, 0x05, 0x00, 0x06]; // opcode 0x02, handle 0x0005, error 0x06 (RequestNotSupported)
        let view = AttErrorRsp::new(&err_bytes[..]);
        assert_eq!(view.attribute_opcode().try_read().unwrap(), Opcode::ATT_ERROR_RSP);
        assert_eq!(
            view.request_opcode_in_error_uint().try_read().unwrap(),
            u8::from(Opcode::ATT_EXCHANGE_MTU_REQ)
        );
        assert_eq!(view.attribute_handle().try_read().unwrap(), 5);
        assert_eq!(view.error_code().try_read().unwrap(), ErrorCode::REQUEST_NOT_SUPPORTED);

        let invalid_bytes = [0x01, 0x02, 0x05, 0x00, 0xff];
        let invalid_view = AttErrorRsp::new(&invalid_bytes[..]);
        assert!(invalid_view.error_code().try_read().is_err());
    }

    #[test]
    fn test_header() {
        let hdr_bytes = [0x02];
        let parsed = Header::read_from_bytes(&hdr_bytes).unwrap();
        assert_eq!(parsed.opcode, u8::from(Opcode::ATT_EXCHANGE_MTU_REQ));

        let new_hdr = Header::new(Opcode::ATT_EXCHANGE_MTU_REQ);
        assert_eq!(new_hdr.as_bytes(), &hdr_bytes);
    }

    #[test]
    fn test_write_req() {
        let req_bytes = [0x12, 0x01, 0x00]; // opcode 0x12 (ATT_WRITE_REQ), handle 0x0001
        let view = AttWriteCmd::new(&req_bytes[..]);
        assert_eq!(view.attribute_opcode().try_read().unwrap(), Opcode::ATT_WRITE_REQ);
        assert_eq!(view.attribute_handle().try_read().unwrap(), 1);

        let short_view = AttWriteCmd::new(&req_bytes[..1]);
        assert!(short_view.attribute_handle().try_read().is_err());
    }

    #[test]
    fn test_prepare_write_req() {
        let req_bytes = [0x16, 0x01, 0x00, 0x05, 0x00]; // opcode 0x16 (ATT_PREPARE_WRITE_REQ), handle 1, offset 5
        let view = AttPrepareWriteHeader::new(&req_bytes[..]);
        assert_eq!(view.attribute_opcode().try_read().unwrap(), Opcode::ATT_PREPARE_WRITE_REQ);
        assert_eq!(view.attribute_handle().try_read().unwrap(), 1);
        assert_eq!(view.value_offset().try_read().unwrap(), 5);

        let short_view = AttPrepareWriteHeader::new(&req_bytes[..2]);
        assert!(short_view.value_offset().try_read().is_err());
    }

    #[test]
    fn test_execute_write_req() {
        let req_bytes = [0x18, 0x01]; // opcode 0x18, flag 0x01 (WRITE)
        let view = AttExecuteWriteReq::new(&req_bytes[..]);
        assert_eq!(view.attribute_opcode().try_read().unwrap(), Opcode::ATT_EXECUTE_WRITE_REQ);
        assert_eq!(view.flags().try_read().unwrap(), ExecuteWriteFlags::WRITE);

        let short_view = AttExecuteWriteReq::new(&req_bytes[..1]);
        assert!(short_view.flags().try_read().is_err());
    }

    #[test]
    fn test_handle_value_ntf() {
        let ntf_bytes = [0x1B, 0x05, 0x00]; // opcode 0x1B (ATT_HANDLE_VALUE_NTF), handle 5
        let view = AttHandleValueNtfHeader::new(&ntf_bytes[..]);
        assert_eq!(view.attribute_opcode().try_read().unwrap(), Opcode::ATT_HANDLE_VALUE_NTF);
        assert_eq!(view.attribute_handle().try_read().unwrap(), 5);

        let short_view = AttHandleValueNtfHeader::new(&ntf_bytes[..1]);
        assert!(short_view.attribute_handle().try_read().is_err());
    }

    #[test]
    fn test_handle_value_ind() {
        let ind_bytes = [0x1D, 0x05, 0x00]; // opcode 0x1D (ATT_HANDLE_VALUE_IND), handle 5
        let view = AttHandleValueIndHeader::new(&ind_bytes[..]);
        assert_eq!(view.attribute_opcode().try_read().unwrap(), Opcode::ATT_HANDLE_VALUE_IND);
        assert_eq!(view.attribute_handle().try_read().unwrap(), 5);

        let short_view = AttHandleValueIndHeader::new(&ind_bytes[..1]);
        assert!(short_view.attribute_handle().try_read().is_err());
    }
}
