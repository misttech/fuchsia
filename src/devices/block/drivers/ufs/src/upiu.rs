// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! UFS Protocol Information Unit (UPIU) header, command, response, and NOP packet definitions
//! (UFS 3.1/4.0 section 10.5 - 10.7).
//!
//! Multi-byte fields are big-endian on the wire and use zerocopy `big_endian` types.

use crate::constants::{
    FLAG_OVERFLOW, FLAG_READ, FLAG_UNDERFLOW, FLAG_WRITE, TRANSACTION_CODE_MASK,
};
use crate::request_list::DataDirection;
use static_assertions::const_assert_eq;
use zerocopy::byteorder::big_endian::{U16, U32};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

/// UPIU transaction type codes (UFS 3.1/4.0 section 10.5). Bit 5 (`0x20`) is 0 for host-to-device
/// requests and 1 for device-to-host responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum UpiuTransactionCode {
    /// Host-to-device NOP OUT request (`0x00`).
    NopOut = 0x00,
    /// Host-to-device SCSI Command request (`0x01`).
    Command = 0x01,
    /// Host-to-device Data Out payload (`0x02`).
    DataOut = 0x02,
    /// Host-to-device Task Management request (`0x04`).
    TaskManagementRequest = 0x04,
    /// Host-to-device Query request (`0x16`).
    QueryRequest = 0x16,
    /// Device-to-host NOP IN response (`0x20`).
    NopIn = 0x20,
    /// Device-to-host SCSI Command response (`0x21`).
    Response = 0x21,
    /// Device-to-host Data In payload (`0x22`).
    DataIn = 0x22,
    /// Device-to-host Task Management response (`0x24`).
    TaskManagementResponse = 0x24,
    /// Device-to-host Ready To Transfer notification (`0x31`).
    ReadyToTransfer = 0x31,
    /// Device-to-host Query response (`0x36`).
    QueryResponse = 0x36,
    /// Device-to-host Reject UPIU (`0x3F`).
    RejectUpiu = 0x3F,
}

/// Basic UPIU Header representation (12 bytes, UFS 3.1/4.0 section 10.6.2).
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    FromBytes,
    IntoBytes,
    KnownLayout,
    Immutable,
    Unaligned,
)]
#[repr(C)]
pub struct UpiuHeader {
    /// Transaction Type: HD (bit 7), DD (bit 6), and transaction code (bits 5:0).
    pub trans_type: u8,
    /// Transaction-specific flags; see [`CommandUpiu`] and [`ResponseUpiu`] constants.
    pub flags: u8,
    pub lun: u8,
    pub task_tag: u8,
    pub cmd_set_type_and_initiator_id: u8,
    pub function: u8,
    pub response: u8,
    pub status: u8,
    pub ehs_length: u8,
    pub device_information: u8,
    /// Data segment length in bytes.
    pub data_segment_length: U16,
}

impl UpiuHeader {
    /// Returns the lower 6-bit transaction code from `trans_type`.
    pub fn trans_code(&self) -> u8 {
        self.trans_type & TRANSACTION_CODE_MASK
    }
}

/// UFS COMMAND UPIU representation (32 bytes, UFS 3.1/4.0 section 10.7.1).
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    FromBytes,
    IntoBytes,
    KnownLayout,
    Immutable,
    Unaligned,
)]
#[repr(C)]
pub struct CommandUpiu {
    pub header: UpiuHeader,
    pub expected_data_transfer_length: U32,
    pub cdb: [u8; 16],
}

impl CommandUpiu {
    /// Header flag `W`: the command has a Data-Out (write) phase.
    pub const FLAG_WRITE: u8 = FLAG_WRITE;
    /// Header flag `R`: the command has a Data-In (read) phase.
    pub const FLAG_READ: u8 = FLAG_READ;

    /// Constructs a new SCSI Command UPIU with the given 16-byte CDB and transfer direction.
    ///
    /// `R` and `W` are both 0 when `expected_transfer_len` is 0 or `direction` is `NoData`.
    pub fn new(cdb: [u8; 16], expected_transfer_len: u32, direction: DataDirection) -> Self {
        let flags = match (expected_transfer_len, direction) {
            (0, _) | (_, DataDirection::NoData) => 0,
            (_, DataDirection::HostToDevice) => Self::FLAG_WRITE,
            (_, DataDirection::DeviceToHost) => Self::FLAG_READ,
        };

        Self {
            header: UpiuHeader {
                trans_type: UpiuTransactionCode::Command as u8,
                flags,
                ..Default::default()
            },
            expected_data_transfer_length: U32::new(expected_transfer_len),
            cdb,
        }
    }
}

/// UFS RESPONSE UPIU representation (56 bytes, UFS 3.1/4.0 section 10.7.2).
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    FromBytes,
    IntoBytes,
    KnownLayout,
    Immutable,
    Unaligned,
)]
#[repr(C)]
pub struct ResponseUpiu {
    pub header: UpiuHeader,
    pub residual_transfer_count: U32,
    pub reserved: [u8; 16],
    pub sense_data_length: U16,
    pub sense_data: [u8; 18],
    /// Pads the UPIU to 56 bytes so the PRDT that follows it stays 8-byte aligned.
    pub padding: [u8; 4],
}

impl ResponseUpiu {
    /// Header flag `O`: the command required more data than `Expected Data Transfer Length`
    /// allowed (residual overflow).
    pub const FLAG_OVERFLOW: u8 = FLAG_OVERFLOW;
    /// Header flag `U`: the device transferred less data than expected (residual underflow).
    pub const FLAG_UNDERFLOW: u8 = FLAG_UNDERFLOW;
}

/// UFS NOP OUT UPIU representation (32 bytes, UFS 3.1/4.0 section 10.7.11).
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    FromBytes,
    IntoBytes,
    KnownLayout,
    Immutable,
    Unaligned,
)]
#[repr(C)]
pub struct NopOutUpiu {
    pub header: UpiuHeader,
    pub reserved: [u8; 20],
}

impl NopOutUpiu {
    /// Creates a default NOP OUT UPIU for verifying UTP layer connectivity.
    pub fn new() -> Self {
        Self {
            header: UpiuHeader {
                trans_type: UpiuTransactionCode::NopOut as u8,
                ..Default::default()
            },
            reserved: [0; 20],
        }
    }
}

/// UFS NOP IN UPIU (32 bytes, UFS 3.1/4.0 section 10.7.12), the device's reply to a NOP OUT.
///
/// It has the same wire layout as [`NopOutUpiu`] and differs only in `header.trans_type`
/// ([`UpiuTransactionCode::NopIn`]), so it shares the struct. The alias names the direction at
/// parse sites; a NOP IN is only ever read back from a UCD, never built with [`NopOutUpiu::new`].
pub type NopInUpiu = NopOutUpiu;

const_assert_eq!(size_of::<UpiuHeader>(), 12);
const_assert_eq!(size_of::<CommandUpiu>(), 32);
const_assert_eq!(size_of::<ResponseUpiu>(), 56);
const_assert_eq!(size_of::<NopOutUpiu>(), 32);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scsi::ScsiOpcode;

    #[fuchsia::test]
    fn test_upiu_header() {
        let header = UpiuHeader { trans_type: 0x81, ..Default::default() };
        assert_eq!(header.trans_code(), 0x01);
    }

    #[fuchsia::test]
    fn test_command_upiu_wire_format() {
        let cdb = ScsiOpcode::build_read_10_cdb(0x01020304, 0x0506);
        let read = CommandUpiu::new(cdb, 0x1000, DataDirection::DeviceToHost);
        let bytes = read.as_bytes();
        assert_eq!(bytes[0], UpiuTransactionCode::Command as u8);
        assert_eq!(bytes[1], CommandUpiu::FLAG_READ);
        assert_eq!(&bytes[12..16], &[0x00, 0x00, 0x10, 0x00]);
        assert_eq!(&bytes[16..26], &[0x28, 0, 1, 2, 3, 4, 0, 5, 6, 0]);

        let write = CommandUpiu::new(cdb, 512, DataDirection::HostToDevice);
        assert_eq!(write.as_bytes()[1], CommandUpiu::FLAG_WRITE);

        // No data phase: neither R nor W.
        let no_data = CommandUpiu::new(cdb, 0, DataDirection::HostToDevice);
        assert_eq!(no_data.as_bytes()[1], 0);
        let no_data = CommandUpiu::new(cdb, 512, DataDirection::NoData);
        assert_eq!(no_data.as_bytes()[1], 0);
    }
}
