// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Attribute Protocol (ATT) Packet Data Unit (PDU) definitions and parsing utilities.

use core::cmp::min;
use core::mem::size_of;
use sapphire_uuid::Uuid;
use strum_macros::FromRepr;
use zerocopy::byteorder::little_endian::U16;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, TryFromBytes, Unaligned};

/// ATT opcodes
///
/// Defined in the Bluetooth core spec v6.0, Vol 3, Part F, 3.4.1.1
#[derive(
    TryFromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy, PartialEq, Eq, FromRepr,
)]
#[repr(u8)]
pub enum Opcode {
    ErrorRsp = 0x01,
    ExchangeMtuReq = 0x02,
    ExchangeMtuRsp = 0x03,
    FindInformationReq = 0x04,
    FindInformationRsp = 0x05,
    FindByTypeValueReq = 0x06,
    FindByTypeValueRsp = 0x07,
    ReadByTypeReq = 0x08,
    ReadByTypeRsp = 0x09,
    ReadReq = 0x0A,
    ReadRsp = 0x0B,
    ReadBlobReq = 0x0C,
    ReadBlobRsp = 0x0D,
    ReadByGroupTypeReq = 0x10,
    ReadByGroupTypeRsp = 0x11,
}

/// The UUID format types supported in Find Information Response.
#[derive(
    TryFromBytes,
    IntoBytes,
    KnownLayout,
    Immutable,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    FromRepr,
    Unaligned,
)]
#[repr(u8)]
pub enum UuidFormat {
    Uuid16 = 0x01,
    Uuid128 = 0x02,
}
impl TryFrom<u8> for UuidFormat {
    type Error = u8;

    fn try_from(val: u8) -> Result<Self, Self::Error> {
        Self::from_repr(val).ok_or(val)
    }
}

impl From<UuidFormat> for u8 {
    fn from(fmt: UuidFormat) -> Self {
        fmt as u8
    }
}

impl From<Uuid> for UuidFormat {
    /// Determines the serialization format for a given UUID.
    ///
    /// If the UUID can be represented as a 16-bit SIG UUID, returns `UuidFormat::Uuid16`.
    /// Otherwise, returns `UuidFormat::Uuid128`.
    fn from(uuid: Uuid) -> Self {
        if uuid.is_u16() { Self::Uuid16 } else { Self::Uuid128 }
    }
}

impl TryFrom<u8> for Opcode {
    type Error = u8;

    fn try_from(val: u8) -> Result<Self, Self::Error> {
        Self::from_repr(val).ok_or(val)
    }
}

impl From<Opcode> for u8 {
    fn from(op: Opcode) -> Self {
        op as u8
    }
}

/// ATT Error Codes
#[derive(
    TryFromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy, PartialEq, Eq, FromRepr,
)]
#[repr(u8)]
pub enum ErrorCode {
    InvalidHandle = 0x01,
    ReadNotPermitted = 0x02,
    WriteNotPermitted = 0x03,
    InvalidPdu = 0x04,
    InsufficientAuthentication = 0x05,
    RequestNotSupported = 0x06,
    InvalidOffset = 0x07,
    InsufficientAuthorization = 0x08,
    PrepareQueueFull = 0x09,
    AttributeNotFound = 0x0A,
    AttributeNotLong = 0x0B,
    InsufficientEncryptionKeySize = 0x0C,
    InvalidAttributeValueLength = 0x0D,
    UnlikelyError = 0x0E,
    InsufficientEncryption = 0x0F,
    UnsupportedGroupType = 0x10,
    InsufficientResources = 0x11,
    ValueNotAllowed = 0x13,
}

impl TryFrom<u8> for ErrorCode {
    type Error = u8;

    fn try_from(val: u8) -> Result<Self, Self::Error> {
        Self::from_repr(val).ok_or(val)
    }
}

/// A parsed view into any incoming packet's header.
#[derive(TryFromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C, packed)]
pub struct Header {
    pub opcode: Opcode,
}

/// A generic unsized ATT packet containing a verified header and variable payload data.
#[derive(TryFromBytes, KnownLayout, Immutable, IntoBytes, Debug)]
#[repr(C)]
pub struct Packet {
    pub header: Header,
    pub data: [u8],
}

/// A helper struct to build fixed-size outbound packets statically on the stack
/// without manual byte serialization or indexing.
#[derive(IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C, packed)]
pub struct PacketBuilder<P> {
    pub header: Header,
    pub payload: P,
}

impl<P> PacketBuilder<P>
where
    Self: IntoBytes + Immutable,
{
    /// Casts the builder reference directly to a validated read-only Packet reference.
    pub fn as_packet(&self) -> &Packet {
        Packet::try_ref_from_bytes(self.as_bytes()).expect("valid PacketBuilder layout")
    }
}

/// Parameters for Exchange MTU Request PDU (OpCode = 0x02)
///
/// (see Vol 3, Part F, 3.4.2.1)
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C, packed)]
pub struct ExchangeMtuReq {
    pub client_rx_mtu: U16,
}

/// Parameters for Exchange MTU Response PDU (OpCode = 0x03)
///
/// (see Vol 3, Part F, 3.4.2.2)
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C, packed)]
pub struct ExchangeMtuRsp {
    pub server_rx_mtu: U16,
}

/// Parameters for Find Information Request PDU (OpCode = 0x04)
///
/// (see Vol 3, Part F, 3.4.3.1)
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C, packed)]
pub struct FindInformationReq {
    pub starting_handle: U16,
    pub ending_handle: U16,
}

/// Parameters for Find Information Response PDU Header (OpCode = 0x05)
///
/// (see Vol 3, Part F, 3.4.3.2)
#[derive(TryFromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C, packed)]
pub struct FindInformationRspHeader {
    pub format: UuidFormat,
}

/// The Find Information Response PDU structure (OpCode = 0x05).
///
/// Contains the format byte indicating UUID size, and a variable-length list of entries.
#[derive(TryFromBytes, KnownLayout, Immutable, IntoBytes, Debug)]
#[repr(C)]
pub struct FindInformationRsp<T> {
    pub format: UuidFormat,
    pub info: [T],
}

/// A trait linking the Information Data structure types to their corresponding UUID format.
pub trait InformationData:
    IntoBytes + Immutable + KnownLayout + for<'a> TryFrom<(u16, &'a Uuid), Error = ()>
{
    const FORMAT: UuidFormat;
}

impl InformationData for InformationData16 {
    const FORMAT: UuidFormat = UuidFormat::Uuid16;
}

/// Information Data structure for 16-bit UUID format (Format = 0x01)
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C, packed)]
pub struct InformationData16 {
    pub handle: U16,
    pub uuid: [u8; 2],
}

impl TryFrom<(u16, &Uuid)> for InformationData16 {
    type Error = ();

    fn try_from((handle, uuid): (u16, &Uuid)) -> Result<Self, Self::Error> {
        let bytes: [u8; 2] = uuid.as_bytes().try_into().map_err(|_| ())?;
        Ok(Self { handle: U16::new(handle), uuid: bytes })
    }
}

impl TryFrom<(u16, &Uuid)> for InformationData128 {
    type Error = ();

    fn try_from((handle, uuid): (u16, &Uuid)) -> Result<Self, Self::Error> {
        let bytes: [u8; 16] = uuid.as_bytes().try_into().map_err(|_| ())?;
        Ok(Self { handle: U16::new(handle), uuid: bytes })
    }
}

/// Information Data structure for 128-bit UUID format (Format = 0x02)
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C, packed)]
pub struct InformationData128 {
    pub handle: U16,
    pub uuid: [u8; 16],
}

impl InformationData for InformationData128 {
    const FORMAT: UuidFormat = UuidFormat::Uuid128;
}

/// Parameters for Find By Type Value Request PDU (OpCode = 0x06).
///
/// see Bluetooth Core Spec v6.0 (Vol 3, Part F, Section 3.4.3.3).
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C, packed)]
pub struct FindByTypeValueReqHeader {
    pub starting_handle: U16,
    pub ending_handle: U16,
    pub attribute_type: U16, // 16-bit UUID only
}

/// The complete Find By Type Value Request PDU (OpCode = 0x06).
///
/// Contains the fixed header fields followed by the variable-length attribute value.
///
/// see Bluetooth Core Spec v6.0 (Vol 3, Part F, Section 3.4.3.3).
#[derive(TryFromBytes, KnownLayout, Immutable, IntoBytes, Debug)]
#[repr(C)]
pub struct FindByTypeValueReq {
    /// Fixed header fields of the request.
    pub header: FindByTypeValueReqHeader,
    /// Variable-length attribute value to search for.
    pub value: [u8],
}

/// Handles Information structure for Find By Type Value Response (OpCode = 0x07).
///
/// see Bluetooth Core Spec v6.0 (Vol 3, Part F, Section 3.4.3.4).
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C, packed)]
pub struct HandlesInformation {
    pub attribute_handle: U16,
    pub group_end_handle: U16,
}

/// Parameters for Error Response PDU (OpCode = 0x01)
///
/// (see Vol 3, Part F, 3.4.1.1)
#[derive(TryFromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C, packed)]
pub struct ErrorRsp {
    pub request_opcode: u8,
    pub attribute_handle: U16,
    pub error_code: ErrorCode,
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

/// Parameters for Read Request PDU (OpCode = 0x0A)
///
/// (see Vol 3, Part F, 3.4.4.1)
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C, packed)]
pub struct ReadReq {
    pub attribute_handle: U16,
}

/// Parameters for Read Blob Request PDU (OpCode = 0x0C)
///
/// see Bluetooth Core Spec v6.0 (Vol 3, Part F, Section 3.4.4.3).
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C, packed)]
pub struct ReadBlobReq {
    pub attribute_handle: U16,
    pub value_offset: U16,
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

    #[test]
    fn test_exchange_mtu_req() {
        let req_bytes = [0x00, 0x02]; // 512 in little endian
        let parsed = ExchangeMtuReq::read_from_bytes(&req_bytes[..]).unwrap();
        assert_eq!(parsed.client_rx_mtu.get(), 512);

        // Serialization check
        let new_req = ExchangeMtuReq { client_rx_mtu: U16::new(512) };
        assert_eq!(new_req.as_bytes(), &req_bytes[..]);
    }

    #[test]
    fn test_read_req() {
        let req_bytes = [0x01, 0x00]; // 1 in little endian
        let parsed = ReadReq::read_from_bytes(&req_bytes[..]).unwrap();
        assert_eq!(parsed.attribute_handle.get(), 1);

        // Serialization check
        let new_req = ReadReq { attribute_handle: U16::new(1) };
        assert_eq!(new_req.as_bytes(), &req_bytes[..]);
    }

    #[test]
    fn test_read_blob_req() {
        let req_bytes = [0x01, 0x00, 0x02, 0x00]; // handle = 1, offset = 2
        let parsed = ReadBlobReq::read_from_bytes(&req_bytes[..]).unwrap();
        assert_eq!(parsed.attribute_handle.get(), 1);
        assert_eq!(parsed.value_offset.get(), 2);

        // Serialization check
        let new_req = ReadBlobReq { attribute_handle: U16::new(1), value_offset: U16::new(2) };
        assert_eq!(new_req.as_bytes(), &req_bytes[..]);
    }

    #[test]
    fn test_read_by_type_req() {
        let req_bytes = [0x01, 0x00, 0x05, 0x00, 0x00, 0x28]; // start 0x0001, end 0x0005, type 0x2800 (Primary Service)
        let parsed = ReadByTypeReq::try_ref_from_bytes(&req_bytes[..]).unwrap();
        assert_eq!(parsed.header.starting_handle.get(), 1);
        assert_eq!(parsed.header.ending_handle.get(), 5);
        assert_eq!(parsed.attribute_type, [0x00, 0x28]);
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
        let req_bytes = [0x01, 0x00, 0x05, 0x00, 0x00, 0x28]; // start 0x0001, end 0x0005, type 0x2800 (Primary Service)
        let parsed = ReadByGroupTypeReq::try_ref_from_bytes(&req_bytes[..]).unwrap();
        assert_eq!(parsed.header.starting_handle.get(), 1);
        assert_eq!(parsed.header.ending_handle.get(), 5);
        assert_eq!(parsed.attribute_type, [0x00, 0x28]);
    }

    #[test]
    fn test_read_by_group_type_rsp_layout() {
        let rsp_bytes = [0x0A, 0x01, 0x02, 0x03, 0x04, 0x05]; // length = 10, data = [1, 2, 3, 4, 5]
        let parsed = ReadByGroupTypeRsp::try_ref_from_bytes(&rsp_bytes[..]).unwrap();
        assert_eq!(parsed.length, 10);
        assert_eq!(parsed.attribute_data_list, [0x01, 0x02, 0x03, 0x04, 0x05]);
    }

    #[test]
    fn test_exchange_mtu_rsp() {
        let rsp_bytes = [0x00, 0x01]; // 256 in little endian
        let parsed = ExchangeMtuRsp::read_from_bytes(&rsp_bytes[..]).unwrap();
        assert_eq!(parsed.server_rx_mtu.get(), 256);

        let new_rsp = ExchangeMtuRsp { server_rx_mtu: U16::new(256) };
        assert_eq!(new_rsp.as_bytes(), &rsp_bytes[..]);
    }

    #[test]
    fn test_find_information_req() {
        let req_bytes = [0x01, 0x00, 0xff, 0xff]; // start 0x0001, end 0xffff
        let parsed = FindInformationReq::read_from_bytes(&req_bytes[..]).unwrap();
        assert_eq!(parsed.starting_handle.get(), 1);
        assert_eq!(parsed.ending_handle.get(), 0xffff);

        let new_req =
            FindInformationReq { starting_handle: U16::new(1), ending_handle: U16::new(0xffff) };
        assert_eq!(new_req.as_bytes(), &req_bytes[..]);
    }

    #[test]
    fn test_find_information_rsp_header() {
        let hdr_bytes_16 = [0x01]; // format 0x01
        let parsed_16 = FindInformationRspHeader::try_read_from_bytes(&hdr_bytes_16[..]).unwrap();
        assert_eq!(parsed_16.format, UuidFormat::Uuid16);

        let hdr_bytes_128 = [0x02]; // format 0x02
        let parsed_128 = FindInformationRspHeader::try_read_from_bytes(&hdr_bytes_128[..]).unwrap();
        assert_eq!(parsed_128.format, UuidFormat::Uuid128);

        // Rejects invalid format
        let invalid_bytes = [0x03];
        assert!(FindInformationRspHeader::try_read_from_bytes(&invalid_bytes[..]).is_err());
    }

    #[test]
    fn test_information_data_16_slice_cast() {
        let data_bytes = [
            0x01, 0x00, 0x00, 0x2a, // handle 1, UUID 0x2A00
            0x05, 0x00, 0x19, 0x2a, // handle 5, UUID 0x2A19
        ];

        // Zero-copy cast slice of entries
        let entries = <[InformationData16]>::ref_from_bytes(&data_bytes[..]).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].handle.get(), 1);
        assert_eq!(entries[0].uuid, [0x00, 0x2a]);
        assert_eq!(entries[1].handle.get(), 5);
        assert_eq!(entries[1].uuid, [0x19, 0x2a]);
    }

    #[test]
    fn test_information_data_128_slice_cast() {
        let data_bytes = [
            0x0a, 0x00, // handle 10
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, // UUID
        ];

        let entries = <[InformationData128]>::ref_from_bytes(&data_bytes[..]).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].handle.get(), 10);
        assert_eq!(entries[0].uuid, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
    }

    #[test]
    fn test_find_information_rsp_decoding() {
        let bytes_16 = [0x01, 1, 0, 0, 0x2A]; // format Uuid16, handle 1, UUID 0x2A00
        let rsp_16 =
            FindInformationRsp::<InformationData16>::try_ref_from_bytes(&bytes_16[..]).unwrap();
        assert_eq!(rsp_16.format, UuidFormat::Uuid16);
        assert_eq!(rsp_16.info[0], InformationData16 { handle: U16::new(1), uuid: [0, 0x2A] });

        let bytes_128 = [0x02, 10, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        let rsp_128 =
            FindInformationRsp::<InformationData128>::try_ref_from_bytes(&bytes_128[..]).unwrap();
        assert_eq!(rsp_128.format, UuidFormat::Uuid128);
        assert_eq!(
            rsp_128.info[0],
            InformationData128 {
                handle: U16::new(10),
                uuid: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
            }
        );
    }

    #[test]
    fn test_find_by_type_value_req() {
        // start 0x0001, end 0x000A, type 0x2800
        let req_bytes = [0x01, 0x00, 0x0a, 0x00, 0x00, 0x28];
        let parsed = FindByTypeValueReqHeader::read_from_bytes(&req_bytes[..]).unwrap();
        assert_eq!(parsed.starting_handle.get(), 1);
        assert_eq!(parsed.ending_handle.get(), 10);
        assert_eq!(parsed.attribute_type.get(), 0x2800);
    }

    #[test]
    fn test_handles_information_slice_cast() {
        let rsp_bytes = [0x01, 0x00, 0x05, 0x00]; // handle 1, end_handle 5
        let entries = <[HandlesInformation]>::ref_from_bytes(&rsp_bytes[..]).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].attribute_handle.get(), 1);
        assert_eq!(entries[0].group_end_handle.get(), 5);
    }

    #[test]
    fn test_error_rsp() {
        let err_bytes = [0x02, 0x05, 0x00, 0x06]; // opcode 0x02, handle 0x0005, error code 0x06 (RequestNotSupported)
        let parsed = ErrorRsp::try_read_from_bytes(&err_bytes[..]).unwrap();
        assert_eq!(parsed.request_opcode, Opcode::ExchangeMtuReq.into());
        assert_eq!(parsed.attribute_handle.get(), 5);
        assert_eq!(parsed.error_code, ErrorCode::RequestNotSupported);

        let new_err = ErrorRsp {
            request_opcode: Opcode::ExchangeMtuReq.into(),
            attribute_handle: U16::new(5),
            error_code: ErrorCode::RequestNotSupported,
        };
        assert_eq!(new_err.as_bytes(), &err_bytes[..]);

        // Validation fails if error_code value is invalid
        let invalid_err_bytes = [0x02, 0x05, 0x00, 0xff]; // invalid error code 0xff
        let parse_result = ErrorRsp::try_read_from_bytes(&invalid_err_bytes[..]);
        assert!(parse_result.is_err());
    }

    #[test]
    fn test_error_code_try_from() {
        assert_eq!(ErrorCode::try_from(0x01), Ok(ErrorCode::InvalidHandle));
        assert_eq!(ErrorCode::try_from(0x06), Ok(ErrorCode::RequestNotSupported));
        assert_eq!(ErrorCode::try_from(0xff), Err(0xff));
    }

    #[test]
    fn test_header() {
        let hdr_bytes = [0x02];
        let parsed = Header::try_read_from_bytes(&hdr_bytes[..]).unwrap();
        assert_eq!(parsed.opcode, Opcode::ExchangeMtuReq);

        let new_hdr = Header { opcode: Opcode::ExchangeMtuReq };
        assert_eq!(new_hdr.as_bytes(), &hdr_bytes[..]);
    }

    #[test]
    fn test_header_invalid_opcode() {
        let invalid_hdr_bytes = [0xff];
        assert!(Header::try_read_from_bytes(&invalid_hdr_bytes[..]).is_err());
    }
}
