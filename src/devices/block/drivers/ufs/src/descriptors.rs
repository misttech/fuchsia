// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Wire-format structures for UFS Device, Geometry, Unit, and RPMB descriptors
//! (UFS 3.1/4.0 section 14.1).
//!
//! Multi-byte fields are big-endian on the wire and use zerocopy `big_endian` types, so every
//! struct is align-1 (`Unaligned`) and needs no `repr(packed)`. Byte 0 is always `bLength`.

use static_assertions::const_assert_eq;
use std::mem::offset_of;
use zerocopy::byteorder::big_endian::{U16, U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

/// Device Descriptor `bWriteBoosterBufferType` values (UFS 3.1/4.0 section 14.1.4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WriteBoosterBufferType {
    LuDedicated = 0x00,
    Shared = 0x01,
}

impl WriteBoosterBufferType {
    /// Converts a raw `bWriteBoosterBufferType` byte into a [`WriteBoosterBufferType`].
    pub const fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0x00 => Some(Self::LuDedicated),
            0x01 => Some(Self::Shared),
            _ => None,
        }
    }
}

/// UFS Descriptor identification numbers (UFS 3.1/4.0 section 14.1.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DescriptorType {
    /// Device-level capabilities and configuration parameters.
    Device = 0x00,
    /// Provisioning and LUN partitioning configuration.
    Configuration = 0x01,
    /// Per-LUN parameters (or [`RpmbUnitDescriptor`] when queried with index `0xC4`).
    Unit = 0x02,
    /// MIPI UniPro and M-PHY version information.
    Interconnect = 0x04,
    /// Manufacturer, product, OEM, and serial number strings.
    String = 0x05,
    /// Raw flash capacity, segment size, and WriteBooster limits.
    Geometry = 0x07,
    /// Active, idle, and sleep current consumption tables.
    Power = 0x08,
    /// Pre-EOL status and NAND wear-leveling lifetime estimates.
    DeviceHealth = 0x09,
}

/// Wire-format UFS descriptor associated with a [`DescriptorType`] IDN.
pub trait Descriptor: FromBytes + IntoBytes + Immutable + KnownLayout {
    /// Expected `bDescriptorIDN` for this descriptor structure.
    const TYPE: DescriptorType;
}

/// Device Descriptor representation (89 bytes, UFS 3.1/4.0 section 14.1.4.2).
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
pub struct DeviceDescriptor {
    pub length: u8,
    pub descriptor_idn: u8,
    pub device: u8,
    pub device_class: u8,
    pub device_sub_class: u8,
    pub protocol: u8,
    pub number_lu: u8,
    pub number_wlu: u8,
    pub boot_enable: u8,
    pub descr_access_en: u8,
    pub init_power_mode: u8,
    pub high_priority_lun: u8,
    pub secure_removal_type: u8,
    pub security_lu: u8,
    pub background_ops_term_lat: u8,
    pub init_active_icc_level: u8,
    pub spec_version: U16,
    pub manufacture_date: U16,
    pub manufacturer_name: u8,
    pub product_name: u8,
    pub serial_number: u8,
    pub oem_id: u8,
    pub manufacturer_id: U16,
    pub ud0_base_offset: u8,
    pub ud_config_p_length: u8,
    pub device_rtt_cap: u8,
    pub periodic_rtc_update: U16,
    pub ufs_features_support: u8,
    pub ffu_timeout: u8,
    pub queue_depth: u8,
    pub device_version: U16,
    pub num_secure_wp_area: u8,
    pub psa_max_data_size: U32,
    pub psa_state_timeout: u8,
    pub product_revision_level: u8,
    pub reserved_2b: [u8; 5],
    pub reserved_ume: [u8; 16],
    pub reserved_hpb: [u8; 3],
    pub reserved_43: [u8; 12],
    pub extended_ufs_features_support: U32,
    pub write_booster_buffer_preserve_user_space_en: u8,
    pub write_booster_buffer_type: u8,
    pub num_shared_write_booster_buffer_alloc_units: U32,
}

impl Descriptor for DeviceDescriptor {
    const TYPE: DescriptorType = DescriptorType::Device;
}

const_assert_eq!(size_of::<DeviceDescriptor>(), 89);
// UFS 3.1/4.0 section 14.1.4.2: byte offsets of multi-byte and late fields.
const_assert_eq!(offset_of!(DeviceDescriptor, spec_version), 0x10);
const_assert_eq!(offset_of!(DeviceDescriptor, manufacturer_id), 0x18);
const_assert_eq!(offset_of!(DeviceDescriptor, periodic_rtc_update), 0x1D);
const_assert_eq!(offset_of!(DeviceDescriptor, device_version), 0x22);
const_assert_eq!(offset_of!(DeviceDescriptor, psa_max_data_size), 0x25);
const_assert_eq!(offset_of!(DeviceDescriptor, extended_ufs_features_support), 0x4F);
const_assert_eq!(offset_of!(DeviceDescriptor, write_booster_buffer_type), 0x54);
const_assert_eq!(offset_of!(DeviceDescriptor, num_shared_write_booster_buffer_alloc_units), 0x55);

/// Geometry Descriptor representation (87 bytes, UFS 3.1/4.0 section 14.1.4.4).
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
pub struct GeometryDescriptor {
    pub length: u8,
    pub descriptor_idn: u8,
    pub media_technology: u8,
    pub reserved_03: u8,
    /// Total raw device capacity in 512-byte units (`qTotalRawDeviceCapacity`).
    pub total_raw_device_capacity: U64,
    /// Maximum number of logical units (`0` = 8 LUs, `1` = 32 LUs).
    pub max_number_lu: u8,
    /// Erase block / segment size in 512-byte units (`dSegmentSize`).
    pub segment_size: U32,
    /// Allocation unit size in segments (`bAllocationUnitSize`).
    pub allocation_unit_size: u8,
    /// Minimum addressable block size in 512-byte units (`bMinAddrBlockSize`).
    pub min_addr_block_size: u8,
    /// Optimal read block size in 512-byte units (`bOptimalReadBlockSize`).
    pub optimal_read_block_size: u8,
    /// Optimal write block size in 512-byte units (`bOptimalWriteBlockSize`).
    pub optimal_write_block_size: u8,
    /// Maximum data-in buffer size in 512-byte units (`bMaxInBufferSize`).
    pub max_in_buffer_size: u8,
    /// Maximum data-out buffer size in 512-byte units (`bMaxOutBufferSize`).
    pub max_out_buffer_size: u8,
    /// Maximum number of 512-byte RPMB frames (each carrying a 256-byte data payload) per Security
    /// Protocol In/Out transfer.
    pub rpmb_read_write_size: u8,
    pub dynamic_capacity_resource_policy: u8,
    pub data_ordering: u8,
    pub max_context_id_number: u8,
    pub sys_data_tag_unit_size: u8,
    pub sys_data_tag_res_size: u8,
    pub supported_sec_r_types: u8,
    pub supported_memory_types: U16,
    pub system_code_max_n_alloc_u: U32,
    pub system_code_cap_adj_fac: U16,
    pub non_persist_max_n_alloc_u: U32,
    pub non_persist_cap_adj_fac: U16,
    pub enhanced_1_max_n_alloc_u: U32,
    pub enhanced_1_cap_adj_fac: U16,
    pub enhanced_2_max_n_alloc_u: U32,
    pub enhanced_2_cap_adj_fac: U16,
    pub enhanced_3_max_n_alloc_u: U32,
    pub enhanced_3_cap_adj_fac: U16,
    pub enhanced_4_max_n_alloc_u: U32,
    pub enhanced_4_cap_adj_fac: U16,
    pub optimal_logical_block_size: U32,
    pub reserved_hpb: [u8; 5],
    pub reserved_4d: [u8; 2],
    pub write_booster_buffer_max_n_alloc_units: U32,
    pub device_max_write_booster_lus: u8,
    pub write_booster_buffer_cap_adj_fac: u8,
    pub supported_write_booster_buffer_user_space_reduction_types: u8,
    pub supported_write_booster_buffer_types: u8,
}

impl Descriptor for GeometryDescriptor {
    const TYPE: DescriptorType = DescriptorType::Geometry;
}

const_assert_eq!(size_of::<GeometryDescriptor>(), 87);
// UFS 3.1/4.0 section 14.1.4.4: byte offsets of multi-byte and late fields.
const_assert_eq!(offset_of!(GeometryDescriptor, total_raw_device_capacity), 0x04);
const_assert_eq!(offset_of!(GeometryDescriptor, max_number_lu), 0x0C);
const_assert_eq!(offset_of!(GeometryDescriptor, segment_size), 0x0D);
const_assert_eq!(offset_of!(GeometryDescriptor, allocation_unit_size), 0x11);
const_assert_eq!(offset_of!(GeometryDescriptor, optimal_logical_block_size), 0x44);
const_assert_eq!(offset_of!(GeometryDescriptor, write_booster_buffer_max_n_alloc_units), 0x4F);

/// Unit Descriptor representation (45 bytes, UFS 3.1/4.0 section 14.1.4.5).
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
pub struct UnitDescriptor {
    pub length: u8,
    pub descriptor_idn: u8,
    pub unit_index: u8,
    pub lu_enable: u8,
    pub boot_lun_id: u8,
    pub lu_write_protect: u8,
    pub lu_queue_depth: u8,
    pub psa_sensitive: u8,
    pub memory_type: u8,
    pub data_reliability: u8,
    /// `log2` of the logical block size in bytes (`bLogicalBlockSize`, e.g. `12` = 4096 bytes).
    pub logical_block_size: u8,
    pub logical_block_count: U64,
    pub erase_block_size: U32,
    pub provisioning_type: u8,
    pub phy_mem_resource_count: U64,
    pub context_capabilities: U16,
    pub large_unit_granularity_m1: u8,
    pub reserved_hpb: [u8; 6],
    pub lu_num_write_booster_buffer_alloc_units: U32,
}

impl Descriptor for UnitDescriptor {
    const TYPE: DescriptorType = DescriptorType::Unit;
}

const_assert_eq!(size_of::<UnitDescriptor>(), 45);
// UFS 3.1/4.0 section 14.1.4.5: byte offsets of multi-byte and late fields.
const_assert_eq!(offset_of!(UnitDescriptor, logical_block_size), 0x0A);
const_assert_eq!(offset_of!(UnitDescriptor, logical_block_count), 0x0B);
const_assert_eq!(offset_of!(UnitDescriptor, erase_block_size), 0x13);
const_assert_eq!(offset_of!(UnitDescriptor, phy_mem_resource_count), 0x18);
const_assert_eq!(offset_of!(UnitDescriptor, context_capabilities), 0x20);
const_assert_eq!(offset_of!(UnitDescriptor, lu_num_write_booster_buffer_alloc_units), 0x29);

/// RPMB Unit Descriptor representation (35 bytes, UFS 3.1/4.0 section 14.1.4.6).
/// Read with Unit Descriptor index `0xC4` (RPMB well-known LU).
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
pub struct RpmbUnitDescriptor {
    pub length: u8,
    pub descriptor_idn: u8,
    pub unit_index: u8,
    pub lu_enable: u8,
    pub boot_lun_id: u8,
    pub lu_write_protect: u8,
    pub lu_queue_depth: u8,
    pub psa_sensitive: u8,
    pub memory_type: u8,
    pub reserved: u8,
    /// `log2` of the logical block size in bytes (`bLogicalBlockSize`, `8` = 256 bytes for RPMB).
    pub logical_block_size: u8,
    /// Number of 256-byte RPMB blocks (`qLogicalBlockCount`, each 128 KiB unit is 512 blocks).
    pub logical_block_count: U64,
    pub erase_block_size: U32,
    pub provisioning_type: u8,
    pub phy_mem_resource_count: U64,
    pub reserved_1: [u8; 3],
}

impl Descriptor for RpmbUnitDescriptor {
    const TYPE: DescriptorType = DescriptorType::Unit;
}

const_assert_eq!(size_of::<RpmbUnitDescriptor>(), 35);
