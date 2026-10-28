// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! UFS Query Request and Response UPIU builders and parsers for descriptors, flags, and attributes
//! (UFS 3.1/4.0 section 10.7.8 - 10.7.9).

use crate::UfsError;
use crate::constants::{DESCRIPTOR_HEADER_SIZE, QUERY_DATA_SEGMENT_SIZE, QUERY_HEADER_SIZE};
use crate::descriptors::{Descriptor, DescriptorType};
use crate::upiu::{UpiuHeader, UpiuTransactionCode};
use log::warn;
use static_assertions::const_assert_eq;
use zerocopy::byteorder::big_endian::{U16, U32};
use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout, Unaligned};

/// Query function codes carried in the UPIU header `function` byte (UFS 3.1/4.0 section
/// 10.7.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum QueryFunction {
    /// Standard read request (`0x01`).
    StandardRead = 0x01,
    /// Standard write request (`0x81`).
    StandardWrite = 0x81,
}

/// Query function opcodes (UFS 3.1/4.0 section 10.7.8.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum QueryOpcode {
    /// No operation.
    Nop = 0x00,
    /// Read a device, geometry, unit, or other descriptor.
    ReadDescriptor = 0x01,
    /// Write a configuration or other writable descriptor.
    WriteDescriptor = 0x02,
    /// Read a 32-bit device attribute.
    ReadAttribute = 0x03,
    /// Write a 32-bit device attribute.
    WriteAttribute = 0x04,
    /// Read a boolean device flag.
    ReadFlag = 0x05,
    /// Set a boolean device flag to 1.
    SetFlag = 0x06,
    /// Clear a boolean device flag to 0.
    ClearFlag = 0x07,
    /// Toggle a boolean device flag.
    ToggleFlag = 0x08,
}

/// UFS Query Flag identifiers (UFS 3.1/4.0 section 14.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Flag {
    /// Triggers device initialization (`fDeviceInit`, polled until cleared to 0).
    DeviceInit = 0x01,
    /// Enables power-on write protection for LUs whose `bLUWriteProtect` is `01h`
    /// (`fPowerOnWPEn`). Once set, it is cleared only by a power cycle or hardware reset.
    PowerOnWpEn = 0x03,
    /// Enables background flash maintenance operations (`fBackgroundOpsEn`).
    BackgroundOpsEn = 0x04,
    /// Enables SLC WriteBooster caching (`fWriteBoosterEn`).
    WriteBoosterEn = 0x0E,
    /// Enables manual WriteBooster buffer flushing (`fWBBufferFlushEn`).
    WbBufferFlushEn = 0x0F,
    /// Enables WriteBooster buffer flushing during hibernate (`fWBBufferFlushDuringHibernate`).
    WbBufferFlushDuringHibernate = 0x10,
}

/// UFS Query Attribute identifiers (UFS 3.1/4.0 section 14.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Attribute {
    /// Enabled boot LU (`bBootLunEn`).
    BootLunEn = 0x00,
    /// Current device power mode (`bCurrentPowerMode`).
    CurrentPowerMode = 0x02,
    /// Active ICC level (`bActiveICCLevel`).
    ActiveIccLevel = 0x03,
    /// Background operations status (`bBackgroundOpStatus`).
    BackgroundOpStatus = 0x05,
    /// Reference clock frequency (`bRefClkFreq`), see [`RefClkFreq`].
    RefClkFreq = 0x0A,
    /// Available WriteBooster buffer capacity (`bAvailableWBBufferSize`).
    AvailableWbBufferSize = 0x1D,
    /// WriteBooster buffer lifetime estimate (`bWBBufferLifeTimeEst`).
    WbBufferLifeTimeEst = 0x1E,
    /// Current WriteBooster buffer size (`dCurrentWBBufferSize`).
    CurrentWbBufferSize = 0x1F,
}

/// `bRefClkFreq` values (UFS 3.1/4.0 section 14.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum RefClkFreq {
    /// 19.2 MHz.
    Freq19_2MHz = 0x0,
    /// 26 MHz.
    Freq26MHz = 0x1,
    /// 38.4 MHz.
    Freq38_4MHz = 0x2,
    /// 52 MHz.
    Freq52MHz = 0x3,
}

/// Query UPIU (12-byte UPIU header + 20-byte transaction-specific fields + 256-byte data segment
/// = 288 bytes).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
#[repr(C)]
pub struct QueryUpiu {
    pub header: UpiuHeader,
    pub opcode: u8,
    pub idn: u8,
    pub index: u8,
    pub selector: u8,
    pub reserved_1: [u8; 2],
    /// Descriptor length in bytes (request: bytes wanted; response: bytes returned).
    pub length: U16,
    /// 32-bit big-endian attribute value at UPIU bytes `20..24`. For flag queries, the 1-byte
    /// `Flag Value` at wire byte 23 is the least-significant byte of this big-endian `U32`
    /// (`self.value.get() & 0xFF`).
    pub value: U32,
    pub reserved_2: [u8; 8],
    pub command_data: [u8; QueryUpiu::DATA_SEGMENT_SIZE],
}

impl Default for QueryUpiu {
    fn default() -> Self {
        Self::new_zeroed()
    }
}

const_assert_eq!(size_of::<QueryUpiu>(), 288);
const_assert_eq!(QueryUpiu::HEADER_SIZE + QueryUpiu::DATA_SEGMENT_SIZE, size_of::<QueryUpiu>());

impl QueryUpiu {
    /// Size in bytes of the fixed Query UPIU header + OSF (excluding `command_data`).
    pub const HEADER_SIZE: usize = QUERY_HEADER_SIZE;
    /// Size in bytes of the `command_data` data segment.
    pub const DATA_SEGMENT_SIZE: usize = QUERY_DATA_SEGMENT_SIZE;

    /// Creates a base Query UPIU with standard headers.
    pub fn new(opcode: QueryOpcode, idn: u8, index: u8, selector: u8) -> Self {
        let function = match opcode {
            QueryOpcode::ReadFlag | QueryOpcode::ReadAttribute | QueryOpcode::ReadDescriptor => {
                QueryFunction::StandardRead as u8
            }
            QueryOpcode::SetFlag
            | QueryOpcode::ClearFlag
            | QueryOpcode::ToggleFlag
            | QueryOpcode::WriteAttribute
            | QueryOpcode::WriteDescriptor => QueryFunction::StandardWrite as u8,
            // NOP has no query function; the field is reserved.
            QueryOpcode::Nop => 0x00,
        };
        let mut upiu = Self::new_zeroed();
        upiu.header.trans_type = UpiuTransactionCode::QueryRequest as u8;
        upiu.header.function = function;
        upiu.opcode = opcode as u8;
        upiu.idn = idn;
        upiu.index = index;
        upiu.selector = selector;
        upiu
    }

    /// Returns the active wire bytes of this Query Request UPIU: the 32-byte header/OSF plus the
    /// data segment declared by `header.data_segment_length` (non-zero only for `WriteDescriptor`).
    pub fn request_bytes(&self) -> &[u8] {
        let data_len =
            (self.header.data_segment_length.get() as usize).min(Self::DATA_SEGMENT_SIZE);
        &self.as_bytes()[..Self::HEADER_SIZE + data_len]
    }

    /// Creates a `ReadFlag` Query Request UPIU for the specified flag.
    pub fn read_flag(flag: Flag) -> Self {
        Self::new(QueryOpcode::ReadFlag, flag as u8, 0, 0)
    }

    /// Creates a `SetFlag` Query Request UPIU for the specified flag.
    pub fn set_flag(flag: Flag) -> Self {
        Self::set_flag_at_index(flag, 0)
    }

    /// Creates a `SetFlag` Query Request UPIU for the specified flag and `index`.
    pub fn set_flag_at_index(flag: Flag, index: u8) -> Self {
        Self::new(QueryOpcode::SetFlag, flag as u8, index, 0)
    }

    /// Creates a `ClearFlag` Query Request UPIU for the specified flag.
    pub fn clear_flag(flag: Flag) -> Self {
        Self::clear_flag_at_index(flag, 0)
    }

    /// Creates a `ClearFlag` Query Request UPIU for the specified flag and `index`.
    pub fn clear_flag_at_index(flag: Flag, index: u8) -> Self {
        Self::new(QueryOpcode::ClearFlag, flag as u8, index, 0)
    }

    /// Creates a `ReadDescriptor` Query Request UPIU for the specified typed descriptor `D`.
    pub fn read_typed_descriptor<D: Descriptor>(index: u8) -> Self {
        Self::read_descriptor(D::TYPE, index, std::mem::size_of::<D>() as u16)
    }

    /// Creates a `ReadDescriptor` Query Request UPIU for the specified descriptor type.
    pub fn read_descriptor(desc_type: DescriptorType, index: u8, length: u16) -> Self {
        Self::read_descriptor_raw(desc_type as u8, index, 0, length)
    }

    /// Creates a `ReadDescriptor` Query Request UPIU with raw IDN, index, selector, and length.
    pub fn read_descriptor_raw(idn: u8, index: u8, selector: u8, length: u16) -> Self {
        let mut q = Self::new(QueryOpcode::ReadDescriptor, idn, index, selector);
        q.length = U16::new(length);
        q
    }

    /// Creates a `WriteDescriptor` Query Request UPIU carrying the typed descriptor `D`.
    ///
    /// # Panics
    ///
    /// Panics if `size_of::<D>()` exceeds the 256-byte data segment. Every in-tree
    /// [`Descriptor`] is far smaller, so this is a programming error rather than a runtime
    /// condition.
    pub fn write_typed_descriptor<D: Descriptor>(index: u8, descriptor: &D) -> Self {
        Self::write_descriptor_raw(D::TYPE as u8, index, 0, descriptor.as_bytes())
            .expect("descriptor fits in the Query UPIU data segment")
    }

    /// Creates a `WriteDescriptor` Query Request UPIU for `desc_type` with raw payload bytes.
    ///
    /// # Errors
    ///
    /// Returns [`UfsError::InvalidParameter`] if `data` exceeds the 256-byte data segment.
    pub fn write_descriptor(
        desc_type: DescriptorType,
        index: u8,
        data: &[u8],
    ) -> Result<Self, UfsError> {
        Self::write_descriptor_raw(desc_type as u8, index, 0, data)
    }

    /// Creates a `WriteDescriptor` Query Request UPIU with raw IDN, index, selector, and payload
    /// (UFS 3.1/4.0 section 10.7.8.3).
    ///
    /// Sets `length` and the header `data_segment_length` to `data.len()` and copies `data` into
    /// `command_data`.
    ///
    /// # Errors
    ///
    /// Returns [`UfsError::InvalidParameter`] if `data` exceeds the 256-byte data segment.
    pub fn write_descriptor_raw(
        idn: u8,
        index: u8,
        selector: u8,
        data: &[u8],
    ) -> Result<Self, UfsError> {
        if data.len() > Self::DATA_SEGMENT_SIZE {
            return Err(UfsError::InvalidParameter);
        }
        let mut q = Self::new(QueryOpcode::WriteDescriptor, idn, index, selector);
        // Bounded by `DATA_SEGMENT_SIZE`, so this cannot truncate.
        let length = U16::new(data.len() as u16);
        q.length = length;
        q.header.data_segment_length = length;
        q.command_data[..data.len()].copy_from_slice(data);
        Ok(q)
    }

    /// Creates a `ReadAttribute` Query Request UPIU for a typed [`Attribute`] identifier with
    /// selector 0.
    pub fn read_attribute(attr: Attribute, index: u8) -> Self {
        Self::new(QueryOpcode::ReadAttribute, attr as u8, index, 0)
    }

    /// Creates a `WriteAttribute` Query Request UPIU for a typed [`Attribute`] identifier with
    /// selector 0.
    pub fn write_attribute(attr: Attribute, index: u8, value: u32) -> Self {
        Self::write_attribute_raw(attr as u8, index, 0, value)
    }

    /// Creates a `WriteAttribute` Query Request UPIU with raw IDN, index, selector, and value.
    pub fn write_attribute_raw(attr_idn: u8, index: u8, selector: u8, value: u32) -> Self {
        let mut q = Self::new(QueryOpcode::WriteAttribute, attr_idn, index, selector);
        q.value = U32::new(value);
        q
    }

    /// Returns the flag value (least significant byte of `value`).
    pub fn flag_value(&self) -> bool {
        (self.value.get() & 0xFF) != 0
    }

    /// Returns the attribute value in host byte order.
    pub fn value(&self) -> u32 {
        self.value.get()
    }

    /// Copies a typed descriptor out of a Query Response `command_data` payload.
    ///
    /// Only the first `min(length, bLength, size_of::<D>())` bytes are copied; any remaining
    /// fields of `D` (e.g. added by a newer spec revision than the device supports) are zero.
    ///
    /// # Errors
    ///
    /// Returns [`UfsError::IoError`] if the returned descriptor is shorter than its 2-byte header,
    /// or its `bDescriptorIDN` does not match `D::TYPE`, and [`UfsError::InvalidParameter`] if `D`
    /// does not fit in the 256-byte data segment.
    pub fn parse_descriptor<D: Descriptor>(&self) -> Result<D, UfsError> {
        let capacity = std::mem::size_of::<D>();
        if capacity > self.command_data.len() {
            return Err(UfsError::InvalidParameter);
        }
        let expected_idn = D::TYPE as u8;
        let returned = (self.length.get() as usize).min(self.command_data.len());
        let valid = returned.min(self.command_data[0] as usize).min(capacity);
        if valid < DESCRIPTOR_HEADER_SIZE
            || self.idn != expected_idn
            || self.command_data[1] != expected_idn
        {
            warn!(
                expected_idn,
                upiu_idn = self.idn,
                desc_idn = self.command_data[1],
                valid;
                "Malformed or mismatched descriptor in Query Response"
            );
            return Err(UfsError::IoError);
        }
        let mut descriptor = D::new_zeroed();
        descriptor.as_mut_bytes()[..valid].copy_from_slice(&self.command_data[..valid]);
        Ok(descriptor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptors::{DeviceDescriptor, UnitDescriptor};
    use zerocopy::byteorder::big_endian::U64;

    #[fuchsia::test]
    fn test_query_upiu_wire_format() {
        let request = QueryUpiu::read_typed_descriptor::<UnitDescriptor>(3);
        assert_eq!(request.request_bytes().len(), QueryUpiu::HEADER_SIZE);
        let bytes = request.as_bytes();
        assert_eq!(bytes[0], UpiuTransactionCode::QueryRequest as u8);
        assert_eq!(bytes[5], QueryFunction::StandardRead as u8);
        assert_eq!(bytes[12], QueryOpcode::ReadDescriptor as u8);
        assert_eq!(bytes[13], DescriptorType::Unit as u8);
        assert_eq!(bytes[14], 3);
        assert_eq!(&bytes[18..20], &[0x00, 0x2D]);

        let write = QueryUpiu::write_attribute(Attribute::WbBufferLifeTimeEst, 1, 0x0A0B0C0D);
        assert_eq!(write, QueryUpiu::write_attribute_raw(0x1E, 1, 0, 0x0A0B0C0D));
        let bytes = write.as_bytes();
        assert_eq!(bytes[5], QueryFunction::StandardWrite as u8);
        assert_eq!(&bytes[20..24], &[0x0A, 0x0B, 0x0C, 0x0D]);
    }

    #[fuchsia::test]
    fn test_write_descriptor_sets_data_segment() {
        let unit = UnitDescriptor {
            length: 45,
            descriptor_idn: DescriptorType::Unit as u8,
            logical_block_count: U64::new(0x1122),
            ..Default::default()
        };
        let request = QueryUpiu::write_typed_descriptor(2, &unit);
        assert_eq!(
            request,
            QueryUpiu::write_descriptor(DescriptorType::Unit, 2, unit.as_bytes()).unwrap()
        );
        assert_eq!(request.header.function, QueryFunction::StandardWrite as u8);
        assert_eq!(request.header.data_segment_length.get(), 45);
        assert_eq!(request.opcode, QueryOpcode::WriteDescriptor as u8);
        assert_eq!(request.idn, DescriptorType::Unit as u8);
        assert_eq!(request.index, 2);
        assert_eq!(request.length.get(), 45);
        assert_eq!(&request.command_data[..45], unit.as_bytes());
        assert_eq!(request.request_bytes().len(), QueryUpiu::HEADER_SIZE + 45);
        assert_eq!(&request.request_bytes()[QueryUpiu::HEADER_SIZE..], unit.as_bytes());

        // Payloads larger than the data segment are rejected.
        let too_big = [0u8; QueryUpiu::DATA_SEGMENT_SIZE + 1];
        assert_eq!(
            QueryUpiu::write_descriptor_raw(0x01, 0, 0, &too_big).unwrap_err(),
            UfsError::InvalidParameter
        );
        let max = [0xAAu8; QueryUpiu::DATA_SEGMENT_SIZE];
        let request = QueryUpiu::write_descriptor_raw(0x01, 0, 0, &max).unwrap();
        assert_eq!(request.request_bytes().len(), size_of::<QueryUpiu>());
    }

    #[fuchsia::test]
    fn test_parse_descriptor_validates_header() {
        let mut response = QueryUpiu::read_typed_descriptor::<UnitDescriptor>(0);
        let mut unit = UnitDescriptor {
            length: 45,
            descriptor_idn: DescriptorType::Unit as u8,
            logical_block_count: U64::new(0x1122),
            ..Default::default()
        };
        response.command_data[..45].copy_from_slice(unit.as_bytes());
        response.length = U16::new(45);
        let parsed: UnitDescriptor = response.parse_descriptor().unwrap();
        assert_eq!(parsed.logical_block_count.get(), 0x1122);
        // Parsing a Unit response as DeviceDescriptor is rejected by D::TYPE check.
        assert_eq!(response.parse_descriptor::<DeviceDescriptor>().unwrap_err(), UfsError::IoError);

        // A shorter descriptor is zero-extended.
        unit.length = 0x0B;
        response.command_data[..45].copy_from_slice(unit.as_bytes());
        let parsed: UnitDescriptor = response.parse_descriptor().unwrap();
        assert_eq!(parsed.logical_block_count.get(), 0);

        // Wrong IDN in payload.
        response.command_data[1] = DescriptorType::Device as u8;
        assert_eq!(response.parse_descriptor::<UnitDescriptor>().unwrap_err(), UfsError::IoError);
    }
}
