// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bitflags::bitflags;
use std::mem::{offset_of, size_of};
use zerocopy::byteorder::network_endian::{U16, U32, U64};
use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout, Unaligned};

pub(crate) const GVNIC_VERSION_STRING: &[u8] = b"Fuchsia gvnic driver 0.1\n";

bitflags! {
    #[derive(Clone, Copy, Default)]
    #[cfg_attr(test, derive(Debug, PartialEq, Eq))]
    pub(crate) struct DeviceStatus: u32 {
        const LINK_STATUS = 1 << 2;
        const DEVICE_IS_RESET = 1 << 4;
    }

    #[derive(Clone, Copy, Default)]
    #[cfg_attr(test, derive(Debug, PartialEq, Eq))]
    pub(crate) struct DriverStatus: u32 {
        const RUN = 1 << 0;
        const RESET = 1 << 1;
    }

    #[derive(Clone, Copy, Default)]
    #[cfg_attr(test, derive(Debug, PartialEq, Eq))]
    pub(crate) struct IrqDoorbellFlags: u32 {
        const ACK = 1 << 31;
        const EVENT = 1 << 29;
    }

    #[derive(Clone, Copy, Default)]
    #[cfg_attr(test, derive(Debug, PartialEq, Eq))]
    pub(crate) struct RxFlags: u16 {
        const ERR = 0x1 << 11;
        const CONT = 0x1 << 13;
    }
}

pub(crate) const GVNIC_REGISTER_BAR: u32 = 0;
#[cfg(test)]
pub(crate) const GVNIC_MSIX_BAR: u32 = 1;
pub(crate) const GVNIC_DOORBELL_BAR: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy, Default, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub(crate) struct GvnicRegisters {
    pub(crate) dev_status: U32,
    pub(crate) drv_status: U32,
    pub(crate) max_tx_queues: U32,
    pub(crate) max_rx_queues: U32,
    pub(crate) admin_queue_pfn: U32,
    pub(crate) admin_queue_doorbell: U32,
    pub(crate) admin_queue_counter: U32,
    pub(crate) reserved: [u8; 2],
    pub(crate) dma_mask: u8,
    pub(crate) driver_version: u8,
    pub(crate) admin_queue_base_address_hi: U32,
    pub(crate) admin_queue_base_address_lo: U32,
    pub(crate) admin_queue_length: U16,
    pub(crate) padding: [u8; 6],
}

const _: () = assert!(size_of::<GvnicRegisters>() == 48);

pub(crate) const REG_DEV_STATUS_OFFSET: usize = offset_of!(GvnicRegisters, dev_status);
pub(crate) const REG_DRV_STATUS_OFFSET: usize = offset_of!(GvnicRegisters, drv_status);
pub(crate) const REG_MAX_TX_QUEUES_OFFSET: usize = offset_of!(GvnicRegisters, max_tx_queues);
pub(crate) const REG_MAX_RX_QUEUES_OFFSET: usize = offset_of!(GvnicRegisters, max_rx_queues);
pub(crate) const REG_ADMIN_QUEUE_PFN_OFFSET: usize = offset_of!(GvnicRegisters, admin_queue_pfn);
pub(crate) const REG_ADMIN_QUEUE_DOORBELL_OFFSET: usize =
    offset_of!(GvnicRegisters, admin_queue_doorbell);
pub(crate) const REG_ADMIN_QUEUE_COUNTER_OFFSET: usize =
    offset_of!(GvnicRegisters, admin_queue_counter);
pub(crate) const REG_DRIVER_VERSION_OFFSET: usize = offset_of!(GvnicRegisters, driver_version);
pub(crate) const REG_ADMIN_QUEUE_BASE_ADDRESS_HI_OFFSET: usize =
    offset_of!(GvnicRegisters, admin_queue_base_address_hi);
pub(crate) const REG_ADMIN_QUEUE_BASE_ADDRESS_LO_OFFSET: usize =
    offset_of!(GvnicRegisters, admin_queue_base_address_lo);
pub(crate) const REG_ADMIN_QUEUE_LENGTH_OFFSET: usize =
    offset_of!(GvnicRegisters, admin_queue_length);

#[repr(u32)]
#[derive(Clone, Copy)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub(crate) enum AdminqOpcode {
    DescribeDevice = 0x1,
    ConfigureDeviceResources = 0x2,
    RegisterPageList = 0x3,
    CreateTxQueue = 0x5,
    CreateRxQueue = 0x6,
}

pub(crate) trait AdminqCommand: IntoBytes + Immutable {
    const OPCODE: AdminqOpcode;
}

pub(crate) const GVNIC_ADMINQ_STATUS_PASSED: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy, Default, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub(crate) struct GvnicDeviceDescriptor {
    pub(crate) max_registered_pages: U64,
    pub(crate) reserved1: U16,
    pub(crate) tx_queue_entries: U16,
    pub(crate) rx_queue_entries: U16,
    pub(crate) default_num_queues: U16,
    pub(crate) mtu: U16,
    pub(crate) counters: U16,
    pub(crate) tx_pages_per_qpl: U16,
    pub(crate) rx_pages_per_qpl: U16,
    pub(crate) mac: [u8; 6],
    pub(crate) num_device_options: U16,
    pub(crate) total_length: U16,
    pub(crate) reserved2: [u8; 6],
}

const _: () = assert!(size_of::<GvnicDeviceDescriptor>() == 40);

pub(crate) const GVNIC_DEVICE_DESCRIPTOR_VERSION: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy, Default, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub(crate) struct GvnicAdminqDescribeDevice {
    pub(crate) device_descriptor_addr: U64,
    pub(crate) device_descriptor_version: U32,
    pub(crate) available_length: U32,
}

impl AdminqCommand for GvnicAdminqDescribeDevice {
    const OPCODE: AdminqOpcode = AdminqOpcode::DescribeDevice;
}

const _: () = assert!(size_of::<GvnicAdminqDescribeDevice>() == 16);

pub(crate) const GVNIC_GQI_QPL_FORMAT: u8 = 0x2;

#[repr(C)]
#[derive(Clone, Copy, Default, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub(crate) struct GvnicAdminqConfigureDeviceResources {
    pub(crate) counter_array: U64,
    pub(crate) irq_db_addr: U64,
    pub(crate) num_counters: U32,
    pub(crate) num_irq_dbs: U32,
    pub(crate) irq_db_stride: U32,
    pub(crate) ntfy_blk_msix_base_idx: U32,
    pub(crate) queue_format: u8,
}

impl AdminqCommand for GvnicAdminqConfigureDeviceResources {
    const OPCODE: AdminqOpcode = AdminqOpcode::ConfigureDeviceResources;
}

const _: () = assert!(size_of::<GvnicAdminqConfigureDeviceResources>() == 33);

#[repr(C)]
#[derive(Clone, Copy, Default, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub(crate) struct GvnicAdminqRegisterPageList {
    pub(crate) page_list_id: U32,
    pub(crate) num_pages: U32,
    pub(crate) page_address_list_addr: U64,
    pub(crate) page_size: U64,
}

impl AdminqCommand for GvnicAdminqRegisterPageList {
    const OPCODE: AdminqOpcode = AdminqOpcode::RegisterPageList;
}

const _: () = assert!(size_of::<GvnicAdminqRegisterPageList>() == 24);

#[repr(C)]
#[derive(Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub(crate) struct GvnicQueueResources {
    pub(crate) db_index: U32,
    pub(crate) counter_index: U32,
    pub(crate) reserved: [u8; 56],
}

impl Default for GvnicQueueResources {
    fn default() -> Self {
        Self::new_zeroed()
    }
}

const _: () = assert!(size_of::<GvnicQueueResources>() == 64);

#[repr(C)]
#[derive(Clone, Copy, Default, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub(crate) struct GvnicAdminqCreateTxQueue {
    pub(crate) queue_id: U32,
    pub(crate) reserved: U32,
    pub(crate) queue_resources_addr: U64,
    pub(crate) tx_ring_addr: U64,
    pub(crate) queue_page_list_id: U32,
    pub(crate) ntfy_id: U32,
    pub(crate) tx_comp_ring_addr: U64,
    pub(crate) tx_ring_size: U16,
}

impl AdminqCommand for GvnicAdminqCreateTxQueue {
    const OPCODE: AdminqOpcode = AdminqOpcode::CreateTxQueue;
}

const _: () = assert!(size_of::<GvnicAdminqCreateTxQueue>() == 42);

#[repr(C)]
#[derive(Clone, Copy, Default, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub(crate) struct GvnicAdminqCreateRxQueue {
    pub(crate) queue_id: U32,
    pub(crate) index: U32,
    pub(crate) reserved: U32,
    pub(crate) ntfy_id: U32,
    pub(crate) queue_resources_addr: U64,
    pub(crate) rx_desc_ring_addr: U64,
    pub(crate) rx_data_ring_addr: U64,
    pub(crate) queue_page_list_id: U32,
    pub(crate) rx_ring_size: U16,
    pub(crate) packet_buffer_size: U16,
}

impl AdminqCommand for GvnicAdminqCreateRxQueue {
    const OPCODE: AdminqOpcode = AdminqOpcode::CreateRxQueue;
}

const _: () = assert!(size_of::<GvnicAdminqCreateRxQueue>() == 48);

const GVNIC_ADMINQ_ENTRY_PAYLOAD_LEN: usize = 56;

#[repr(C)]
#[derive(Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub(crate) struct GvnicAdminqEntry {
    pub(crate) opcode: U32,
    pub(crate) status: U32,
    payload: [u8; GVNIC_ADMINQ_ENTRY_PAYLOAD_LEN],
}

impl Default for GvnicAdminqEntry {
    fn default() -> Self {
        Self::new_zeroed()
    }
}

impl GvnicAdminqEntry {
    pub(crate) fn new_command(opcode: AdminqOpcode, bytes: &[u8]) -> Self {
        let mut entry = Self { opcode: U32::new(opcode as u32), ..Self::default() };
        entry.payload[..bytes.len()].copy_from_slice(bytes);
        entry
    }

    #[cfg(test)]
    pub(crate) fn read_payload<T: FromBytes + KnownLayout + Immutable>(&self) -> T {
        let (val, _) = T::read_from_prefix(&self.payload).expect("payload fits in 56 bytes");
        val
    }
}

const _: () = assert!(size_of::<GvnicAdminqEntry>() == 64);

pub(crate) const GVNIC_TX_DESC_TYPE_PKT: u8 = 0x0 << 4;

#[repr(C)]
#[derive(Clone, Copy, Default, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub(crate) struct GvnicTxPktDesc {
    pub(crate) type_flags: u8,
    pub(crate) checksum_offset: u8,
    pub(crate) l4_offset: u8,
    pub(crate) descriptor_count: u8,
    pub(crate) len: U16,
    pub(crate) seg_len: U16,
    pub(crate) seg_addr: U64,
}

const _: () = assert!(size_of::<GvnicTxPktDesc>() == 16);

#[repr(C)]
#[derive(Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub(crate) struct GvnicRxDesc {
    pub(crate) padding: [u8; 48],
    pub(crate) rss_hash: U32,
    pub(crate) gro_mss: U16,
    pub(crate) reserved: U16,
    pub(crate) header_length: u8,
    pub(crate) packet_data_offset: u8,
    pub(crate) checksum: U16,
    pub(crate) length: U16,
    pub(crate) flags_seq: U16,
}

impl Default for GvnicRxDesc {
    fn default() -> Self {
        Self::new_zeroed()
    }
}

const _: () = assert!(size_of::<GvnicRxDesc>() == 64);
const _: () = assert!(offset_of!(GvnicRxDesc, checksum) == 58);
const _: () = assert!(offset_of!(GvnicRxDesc, length) == 60);
const _: () = assert!(offset_of!(GvnicRxDesc, flags_seq) == 62);

#[repr(C)]
#[derive(Clone, Copy, Default, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub(crate) struct GvnicRxDataRingSlot {
    pub(crate) qpl_offset: U64,
}

const _: () = assert!(size_of::<GvnicRxDataRingSlot>() == 8);
