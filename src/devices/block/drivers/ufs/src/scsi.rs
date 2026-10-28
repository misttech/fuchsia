// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! SCSI Command Descriptor Block (CDB) builders and parameter structures for UFS block and
//! security operations (UFS 3.1/4.0 section 11.3).
//!
//! Multi-byte parameter data fields are big-endian and use zerocopy `big_endian` types.
//!
//! Note: `INQUIRY` and `READ CAPACITY (10)/(16)` CDB builders and response parameter structs (plus
//! [`ScsiOpcode::ReportLuns`]) are provided for UFS SCSI command-set completeness and unit tests
//! (`FakeUfs`); production LUN geometry discovery reads UFS Query Descriptors (`UnitDescriptor`,
//! `DeviceDescriptor`, `GeometryDescriptor`).

use crate::constants::{
    CDB_FUA_BIT, MAX_LUN_INDEX, RPMB_SECURITY_PROTOCOL_SPECIFIC, SCSI_WELL_KNOWN_LUN_ID,
    SCSI_WELL_KNOWN_LUN_MASK, SERVICE_ACTION_READ_CAPACITY_16, UFS_SECURITY_PROTOCOL,
    UFS_WELL_KNOWN_LUN_ID,
};
use crate::request_list::DataDirection;
use static_assertions::const_assert_eq;
use zerocopy::byteorder::big_endian::{U16, U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

/// SCSI status codes returned in `RESPONSE` UPIU header byte 7 (SAM-5 section 5.3, UFS 3.1/4.0
/// section 10.7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ScsiStatus {
    Good = 0x00,
    CheckCondition = 0x02,
    Busy = 0x08,
    TaskSetFull = 0x28,
}

impl ScsiStatus {
    /// Converts a raw `RESPONSE` UPIU status byte into a known [`ScsiStatus`].
    pub const fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0x00 => Some(Self::Good),
            0x02 => Some(Self::CheckCondition),
            0x08 => Some(Self::Busy),
            0x28 => Some(Self::TaskSetFull),
            _ => None,
        }
    }

    /// Returns the raw wire value of this status code.
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// SCSI sense keys decoded from fixed- or descriptor-format sense data (SPC-4 section 4.5.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SenseKey {
    NotReady = 0x02,
    IllegalRequest = 0x05,
    UnitAttention = 0x06,
}

impl SenseKey {
    /// Converts a 4-bit raw sense key into a known [`SenseKey`].
    pub const fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0x02 => Some(Self::NotReady),
            0x05 => Some(Self::IllegalRequest),
            0x06 => Some(Self::UnitAttention),
            _ => None,
        }
    }

    /// Returns the raw 4-bit wire value of this sense key.
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Whether a `WRITE` sets Force Unit Access (`FUA`): the data must reach the medium before the
/// command completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fua {
    Off,
    On,
}

/// Which of `READ` or `WRITE` to issue for a block range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Read,
    Write { fua: Fua },
}

/// Translates a SCSI LUN into the UPIU LUN field (UFS 3.1/4.0 section 10.8.5).
///
/// # Errors
///
/// Returns `INVALID_ARGS` for an unknown address method or malformed well-known LUN, and
/// `OUT_OF_RANGE` for a regular LUN above 31.
pub fn ufs_lun_from_scsi_lun(scsi_lun: u16) -> Result<u8, zx::Status> {
    let lun = (scsi_lun & 0xFF) as u8;
    match scsi_lun & SCSI_WELL_KNOWN_LUN_MASK {
        SCSI_WELL_KNOWN_LUN_ID if lun < UFS_WELL_KNOWN_LUN_ID => Ok(lun | UFS_WELL_KNOWN_LUN_ID),
        SCSI_WELL_KNOWN_LUN_ID => Err(zx::Status::INVALID_ARGS),
        0 if u16::from(lun) <= MAX_LUN_INDEX => Ok(lun),
        0 => Err(zx::Status::OUT_OF_RANGE),
        _ => Err(zx::Status::INVALID_ARGS),
    }
}

/// Common SCSI opcodes handled by Universal Flash Storage controllers (UFS 3.1/4.0 section 11.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ScsiOpcode {
    /// Checks if the logical unit is ready (`TEST UNIT READY`, `0x00`).
    TestUnitReady = 0x00,
    /// Queries standard device identification (`INQUIRY`, `0x12`).
    Inquiry = 0x12,
    /// Controls logical unit power condition (`START STOP UNIT`, `0x1B`).
    StartStopUnit = 0x1B,
    /// Writes a device buffer (`WRITE BUFFER`, `0x3B`).
    WriteBuffer = 0x3B,
    /// Reads a device buffer (`READ BUFFER (10)`, `0x3C`).
    ReadBuffer = 0x3C,
    /// Reads 32-bit logical block capacity (`READ CAPACITY (10)`, `0x25`).
    ReadCapacity10 = 0x25,
    /// Reads logical blocks using 10-byte CDB (`READ (10)`, `0x28`).
    Read10 = 0x28,
    /// Writes logical blocks using 10-byte CDB (`WRITE (10)`, `0x2A`).
    Write10 = 0x2A,
    /// Flushes volatile write cache to non-volatile storage (`SYNCHRONIZE CACHE (10)`, `0x35`).
    SynchronizeCache10 = 0x35,
    /// Deallocates / trims logical block ranges (`UNMAP`, `0x42`).
    Unmap = 0x42,
    /// Reads logical blocks using 16-byte CDB (`READ (16)`, `0x88`).
    Read16 = 0x88,
    /// Writes logical blocks using 16-byte CDB (`WRITE (16)`, `0x8A`).
    Write16 = 0x8A,
    /// Service action in for 64-bit capacity (`READ CAPACITY (16)`, `0x9E`).
    ReadCapacity16 = 0x9E,
    /// Queries available logical units (`REPORT LUNS`, `0xA0`).
    ReportLuns = 0xA0,
    /// Receives security protocol frames such as RPMB (`SECURITY PROTOCOL IN`, `0xA2`).
    SecurityProtocolIn = 0xA2,
    /// Transmits security protocol frames such as RPMB (`SECURITY PROTOCOL OUT`, `0xB5`).
    SecurityProtocolOut = 0xB5,
}

/// Standard SCSI Inquiry Data response (36 bytes minimum).
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
pub struct InquiryData {
    pub peripheral_device_type: u8,
    pub rmb: u8,
    pub version: u8,
    pub response_data_format: u8,
    pub additional_length: u8,
    pub sccs_acc: u8,
    pub bque_encserv_vs_multip: u8,
    pub reladr_wbus16_sync_cmdque: u8,
    pub vendor_id: [u8; 8],
    pub product_id: [u8; 16],
    pub product_revision: [u8; 4],
}

/// Standard SCSI Read Capacity 10 parameter data (8 bytes).
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
pub struct ReadCapacity10Data {
    pub returned_logical_block_address: U32,
    pub block_length_in_bytes: U32,
}

impl ReadCapacity10Data {
    /// Returns the last logical block address in host endianness.
    pub fn lba(&self) -> u32 {
        self.returned_logical_block_address.get()
    }

    /// Returns the logical block length in bytes in host endianness.
    pub fn block_length(&self) -> u32 {
        self.block_length_in_bytes.get()
    }
}

/// Standard SCSI Read Capacity 16 parameter data (32 bytes).
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
pub struct ReadCapacity16Data {
    pub returned_logical_block_address: U64,
    pub block_length_in_bytes: U32,
    /// Protection information (byte 12): bits 3:1 are `P_TYPE` (protection type) and bit 0 is
    /// `PROT_EN` (protection information enabled). UFS devices do not support T10 PI, so this is
    /// always 0.
    pub protection: u8,
    /// Byte 13: bits 7:4 are `P_I_EXPONENT`; bits 3:0 are
    /// `LOGICAL BLOCKS PER PHYSICAL BLOCK EXPONENT`.
    pub logical_blocks_per_physical_block_exponent: u8,
    /// Bytes 14-15: bit 15 is `LBPME` (logical block provisioning management enabled), bit 14 is
    /// `LBPRZ` (unmapped blocks read as zero), and bits 13:0 are the lowest aligned LBA.
    pub lowest_aligned_logical_block_address: U16,
    pub reserved: [u8; 16],
}

const_assert_eq!(size_of::<InquiryData>(), 36);
const_assert_eq!(size_of::<ReadCapacity10Data>(), 8);
const_assert_eq!(size_of::<ReadCapacity16Data>(), 32);

impl ReadCapacity16Data {
    /// Returns the 64-bit last logical block address in host endianness.
    pub fn lba(&self) -> u64 {
        self.returned_logical_block_address.get()
    }

    /// Returns the logical block length in bytes in host endianness.
    pub fn block_length(&self) -> u32 {
        self.block_length_in_bytes.get()
    }
}

impl ScsiOpcode {
    /// Builds a 16-byte padded `INQUIRY` CDB.
    pub fn build_inquiry_cdb(allocation_len: u16) -> [u8; 16] {
        let mut cdb = [0u8; 16];
        cdb[0] = ScsiOpcode::Inquiry as u8;
        cdb[3..5].copy_from_slice(&allocation_len.to_be_bytes());
        cdb
    }

    /// Builds a 16-byte padded `READ CAPACITY (10)` CDB.
    pub fn build_read_capacity_10_cdb() -> [u8; 16] {
        let mut cdb = [0u8; 16];
        cdb[0] = ScsiOpcode::ReadCapacity10 as u8;
        cdb
    }

    /// Builds a 16-byte `READ CAPACITY (16)` CDB.
    pub fn build_read_capacity_16_cdb(allocation_len: u32) -> [u8; 16] {
        let mut cdb = [0u8; 16];
        cdb[0] = ScsiOpcode::ReadCapacity16 as u8;
        cdb[1] = SERVICE_ACTION_READ_CAPACITY_16;
        cdb[10..14].copy_from_slice(&allocation_len.to_be_bytes());
        cdb
    }

    /// Builds a 16-byte padded `TEST UNIT READY` CDB.
    pub fn build_test_unit_ready_cdb() -> [u8; 16] {
        let mut cdb = [0u8; 16];
        cdb[0] = ScsiOpcode::TestUnitReady as u8;
        cdb
    }

    fn build_rw_10_cdb(opcode: ScsiOpcode, fua: Fua, lba: u32, block_count: u16) -> [u8; 16] {
        let mut cdb = [0u8; 16];
        cdb[0] = opcode as u8;
        if fua == Fua::On {
            cdb[1] = CDB_FUA_BIT;
        }
        cdb[2..6].copy_from_slice(&lba.to_be_bytes());
        cdb[7..9].copy_from_slice(&block_count.to_be_bytes());
        cdb
    }

    /// Builds a 16-byte padded `READ (10)` CDB.
    pub fn build_read_10_cdb(lba: u32, block_count: u16) -> [u8; 16] {
        Self::build_rw_10_cdb(ScsiOpcode::Read10, Fua::Off, lba, block_count)
    }

    /// Builds a 16-byte padded `WRITE (10)` CDB.
    pub fn build_write_10_cdb(lba: u32, block_count: u16) -> [u8; 16] {
        Self::build_rw_10_cdb(ScsiOpcode::Write10, Fua::Off, lba, block_count)
    }

    fn build_rw_16_cdb(opcode: ScsiOpcode, fua: Fua, lba: u64, block_count: u32) -> [u8; 16] {
        let mut cdb = [0u8; 16];
        cdb[0] = opcode as u8;
        if fua == Fua::On {
            cdb[1] = CDB_FUA_BIT;
        }
        cdb[2..10].copy_from_slice(&lba.to_be_bytes());
        cdb[10..14].copy_from_slice(&block_count.to_be_bytes());
        cdb
    }

    /// Builds a 16-byte `READ (16)` CDB.
    pub fn build_read_16_cdb(lba: u64, block_count: u32) -> [u8; 16] {
        Self::build_rw_16_cdb(ScsiOpcode::Read16, Fua::Off, lba, block_count)
    }

    /// Builds a 16-byte `WRITE (16)` CDB.
    pub fn build_write_16_cdb(lba: u64, block_count: u32) -> [u8; 16] {
        Self::build_rw_16_cdb(ScsiOpcode::Write16, Fua::Off, lba, block_count)
    }

    /// Builds a `READ` or `WRITE` CDB (with Force Unit Access for `Direction::Write { fua: On }`),
    /// using the 10-byte form when `lba` and `block_count` fit and the 16-byte form otherwise.
    pub fn build_rw_cdb(direction: Direction, lba: u64, block_count: u32) -> [u8; 16] {
        let (opcode10, opcode16, fua) = match direction {
            Direction::Read => (ScsiOpcode::Read10, ScsiOpcode::Read16, Fua::Off),
            Direction::Write { fua } => (ScsiOpcode::Write10, ScsiOpcode::Write16, fua),
        };
        match (u32::try_from(lba), u16::try_from(block_count)) {
            (Ok(lba32), Ok(blocks16)) => Self::build_rw_10_cdb(opcode10, fua, lba32, blocks16),
            _ => Self::build_rw_16_cdb(opcode16, fua, lba, block_count),
        }
    }

    /// Builds a 16-byte padded `SYNCHRONIZE CACHE (10)` CDB.
    pub fn build_synchronize_cache_10_cdb() -> [u8; 16] {
        let mut cdb = [0u8; 16];
        cdb[0] = ScsiOpcode::SynchronizeCache10 as u8;
        cdb
    }

    /// Builds a 16-byte padded `UNMAP` CDB.
    pub fn build_unmap_cdb(param_list_len: u16) -> [u8; 16] {
        let mut cdb = [0u8; 16];
        cdb[0] = ScsiOpcode::Unmap as u8;
        cdb[7..9].copy_from_slice(&param_list_len.to_be_bytes());
        cdb
    }

    /// Builds a 16-byte padded `WRITE BUFFER` (`HostToDevice`) or `READ BUFFER (10)` CDB.
    /// `buffer_offset` and `length` are 24-bit.
    pub fn build_buffer_cdb(
        direction: DataDirection,
        mode: u8,
        buffer_id: u8,
        buffer_offset: u32,
        length: u32,
    ) -> [u8; 16] {
        let mut cdb = [0u8; 16];
        cdb[0] = match direction {
            DataDirection::HostToDevice => ScsiOpcode::WriteBuffer,
            DataDirection::DeviceToHost | DataDirection::NoData => ScsiOpcode::ReadBuffer,
        } as u8;
        cdb[1] = mode;
        cdb[2] = buffer_id;
        cdb[3..6].copy_from_slice(&buffer_offset.to_be_bytes()[1..]);
        cdb[6..9].copy_from_slice(&length.to_be_bytes()[1..]);
        cdb
    }

    /// Builds a 16-byte padded `SECURITY PROTOCOL OUT` (`0xB5`, `HostToDevice`) or
    /// `SECURITY PROTOCOL IN` (`0xA2`) CDB for RPMB.
    pub fn build_security_protocol_cdb(direction: DataDirection, transfer_len: u32) -> [u8; 16] {
        let mut cdb = [0u8; 16];
        cdb[0] = match direction {
            DataDirection::HostToDevice => ScsiOpcode::SecurityProtocolOut,
            DataDirection::DeviceToHost | DataDirection::NoData => ScsiOpcode::SecurityProtocolIn,
        } as u8;
        cdb[1] = UFS_SECURITY_PROTOCOL;
        cdb[2..4].copy_from_slice(&RPMB_SECURITY_PROTOCOL_SPECIFIC.to_be_bytes());
        cdb[6..10].copy_from_slice(&transfer_len.to_be_bytes());
        cdb
    }
}

/// SCSI UNMAP block descriptor (16 bytes).
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
pub struct UnmapBlockDescriptor {
    pub unmap_lba: U64,
    pub num_blocks: U32,
    pub reserved: [u8; 4],
}

impl UnmapBlockDescriptor {
    /// Creates a new UNMAP block descriptor.
    pub fn new(lba: u64, num_blocks: u32) -> Self {
        Self { unmap_lba: U64::new(lba), num_blocks: U32::new(num_blocks), reserved: [0; 4] }
    }
}

/// SCSI UNMAP parameter list with 1 block descriptor (24 bytes).
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
pub struct UnmapParameterList {
    pub unmap_data_length: U16,
    pub unmap_block_descriptor_data_length: U16,
    pub reserved: [u8; 4],
    pub descriptor: UnmapBlockDescriptor,
}

impl UnmapParameterList {
    /// Byte length of the `UNMAP DATA LENGTH` field itself (excluded from `unmap_data_length`).
    const DATA_LENGTH_FIELD_BYTES: usize = std::mem::size_of::<U16>();

    /// Creates a 24-byte UNMAP parameter list containing a single block range descriptor.
    pub fn new(lba: u64, num_blocks: u32) -> Self {
        const DATA_LEN: u16 = (std::mem::size_of::<UnmapParameterList>()
            - UnmapParameterList::DATA_LENGTH_FIELD_BYTES) as u16;
        const DESC_LEN: u16 = std::mem::size_of::<UnmapBlockDescriptor>() as u16;
        Self {
            unmap_data_length: U16::new(DATA_LEN),
            unmap_block_descriptor_data_length: U16::new(DESC_LEN),
            reserved: [0; 4],
            descriptor: UnmapBlockDescriptor::new(lba, num_blocks),
        }
    }
}

const_assert_eq!(size_of::<UnmapBlockDescriptor>(), 16);
const_assert_eq!(size_of::<UnmapParameterList>(), 24);

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn test_ufs_lun_from_scsi_lun() {
        assert_eq!(ufs_lun_from_scsi_lun(0), Ok(0));
        assert_eq!(ufs_lun_from_scsi_lun(31), Ok(31));
        assert_eq!(ufs_lun_from_scsi_lun(32), Err(zx::Status::OUT_OF_RANGE));
        assert_eq!(ufs_lun_from_scsi_lun(0x0080), Err(zx::Status::OUT_OF_RANGE));
        assert_eq!(ufs_lun_from_scsi_lun(0x0081), Err(zx::Status::OUT_OF_RANGE));
        // Well-known LUNs: REPORT LUNS, UFS Device, BOOT and RPMB.
        assert_eq!(ufs_lun_from_scsi_lun(0xC101), Ok(0x81));
        assert_eq!(ufs_lun_from_scsi_lun(0xC150), Ok(0xD0));
        assert_eq!(ufs_lun_from_scsi_lun(0xC130), Ok(0xB0));
        assert_eq!(ufs_lun_from_scsi_lun(0xC144), Ok(0xC4));
        assert_eq!(ufs_lun_from_scsi_lun(0xC180), Err(zx::Status::INVALID_ARGS));
        assert_eq!(ufs_lun_from_scsi_lun(0xC181), Err(zx::Status::INVALID_ARGS));
        assert_eq!(ufs_lun_from_scsi_lun(0x4001), Err(zx::Status::INVALID_ARGS));
    }

    #[fuchsia::test]
    fn test_cdb_builders() {
        let cdb = ScsiOpcode::build_read_16_cdb(0x0102030405060708, 0x090A0B0C);
        assert_eq!(cdb[0], ScsiOpcode::Read16 as u8);
        assert_eq!(&cdb[2..14], &[1, 2, 3, 4, 5, 6, 7, 8, 9, 0xA, 0xB, 0xC]);

        let cdb = ScsiOpcode::build_write_10_cdb(0xAABBCCDD, 8);
        assert_eq!(cdb[0], ScsiOpcode::Write10 as u8);
        assert_eq!(cdb[1], 0);
        assert_eq!(&cdb[2..9], &[0xAA, 0xBB, 0xCC, 0xDD, 0, 0, 8]);

        // build_rw_cdb selects 10-byte vs 16-byte and sets the FUA bit when requested.
        let write_fua = Direction::Write { fua: Fua::On };
        let write_10_fua = ScsiOpcode::build_rw_cdb(write_fua, 0xAABBCCDD, 8);
        assert_eq!(write_10_fua[0], ScsiOpcode::Write10 as u8);
        assert_eq!(write_10_fua[1], CDB_FUA_BIT);
        assert_eq!(&write_10_fua[2..9], &[0xAA, 0xBB, 0xCC, 0xDD, 0, 0, 8]);

        let write_16_fua = ScsiOpcode::build_rw_cdb(write_fua, 1 << 32, 8);
        assert_eq!(write_16_fua[0], ScsiOpcode::Write16 as u8);
        assert_eq!(write_16_fua[1], CDB_FUA_BIT);

        let read_10 = ScsiOpcode::build_rw_cdb(Direction::Read, 0xAABBCCDD, 8);
        assert_eq!(read_10[0], ScsiOpcode::Read10 as u8);
        assert_eq!(read_10[1], 0);

        let sec_in = ScsiOpcode::build_security_protocol_cdb(DataDirection::DeviceToHost, 512);
        assert_eq!(sec_in[0], ScsiOpcode::SecurityProtocolIn as u8);
        assert_eq!(&sec_in[1..4], &[0xEC, 0x00, 0x01]);
        assert_eq!(&sec_in[6..10], &512u32.to_be_bytes());
        let sec_out = ScsiOpcode::build_security_protocol_cdb(DataDirection::HostToDevice, 1024);
        assert_eq!(sec_out[0], ScsiOpcode::SecurityProtocolOut as u8);
        assert_eq!(&sec_out[6..10], &1024u32.to_be_bytes());

        let cdb = ScsiOpcode::build_inquiry_cdb(36);
        assert_eq!(cdb[0], ScsiOpcode::Inquiry as u8);
        assert_eq!(&cdb[3..5], &[0, 36]);
    }

    #[fuchsia::test]
    fn test_buffer_cdb() {
        let write = DataDirection::HostToDevice;
        let cdb = ScsiOpcode::build_buffer_cdb(write, 0x02, 0x07, 0x0A0B0C, 0x010203);
        assert_eq!(&cdb[..10], &[0x3B, 0x02, 0x07, 0x0A, 0x0B, 0x0C, 0x01, 0x02, 0x03, 0]);
        let cdb = ScsiOpcode::build_buffer_cdb(DataDirection::DeviceToHost, 0x1C, 0, 0, 4);
        assert_eq!(&cdb[..10], &[0x3C, 0x1C, 0, 0, 0, 0, 0, 0, 4, 0]);
    }

    #[fuchsia::test]
    fn test_unmap_structures() {
        let desc = UnmapBlockDescriptor::new(0x1000, 8);
        assert_eq!(desc.unmap_lba.get(), 0x1000);
        assert_eq!(desc.num_blocks.get(), 8);
        assert_eq!(&desc.as_bytes()[..12], &[0, 0, 0, 0, 0, 0, 0x10, 0, 0, 0, 0, 8]);

        let param_list = UnmapParameterList::new(0x2000, 16);
        assert_eq!(param_list.as_bytes().len(), 24);
        assert_eq!(param_list.unmap_data_length.get(), 22);
        assert_eq!(param_list.unmap_block_descriptor_data_length.get(), 16);

        let cdb = ScsiOpcode::build_unmap_cdb(24);
        assert_eq!(cdb[0], ScsiOpcode::Unmap as u8);
        assert_eq!(cdb[7], 0);
        assert_eq!(cdb[8], 24);
    }
}
