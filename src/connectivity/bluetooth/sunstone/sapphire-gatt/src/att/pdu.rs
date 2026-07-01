// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Attribute Protocol (ATT) Packet Data Unit (PDU) definitions and parsing utilities.

use strum_macros::FromRepr;
use zerocopy::byteorder::little_endian::U16;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, TryFromBytes};

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
    fn test_exchange_mtu_rsp() {
        let rsp_bytes = [0x00, 0x01]; // 256 in little endian
        let parsed = ExchangeMtuRsp::read_from_bytes(&rsp_bytes[..]).unwrap();
        assert_eq!(parsed.server_rx_mtu.get(), 256);

        let new_rsp = ExchangeMtuRsp { server_rx_mtu: U16::new(256) };
        assert_eq!(new_rsp.as_bytes(), &rsp_bytes[..]);
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
