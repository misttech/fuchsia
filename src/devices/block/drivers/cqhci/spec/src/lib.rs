// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bitfield::bitfield;
use bitflags::bitflags;
use std::num::NonZeroU16;

/// The CQHCI spec requires 512 byte blocks (JESD84-B51A, 6.6.39.1)
pub const MMC_BLOCK_SIZE: u64 = 512;

// EXT_CSD fields (JESD84-B51A, 7.4)

pub const EXT_CSD_BARRIER_EN: usize = 31;
pub const EXT_CSD_BARRIER_ENABLED: u8 = 1;

pub const EXT_CSD_FLUSH_CACHE: usize = 32;
pub const EXT_CSD_FLUSH_CACHE_FLUSH: u8 = 0x1;
pub const EXT_CSD_FLUSH_CACHE_BARRIER: u8 = 0x2;

pub const EXT_CSD_CACHE_CTRL: usize = 33;
pub const EXT_CSD_CACHE_EN_MASK: u8 = 1;

pub const EXT_CSD_PARTITION_CONFIG: usize = 179;
pub const EXT_CSD_PARTITION_ACCESS_MASK: u8 = 0xf8;

pub const EXT_CSD_PARTITON_SWITCH_TIME: usize = 199;

pub const EXT_CSD_GENERIC_CMD6_TIME: usize = 248;

pub const EXT_CSD_BARRIER_SUPPORT: usize = 486;
pub const EXT_CSD_BARRIER_SUPPORT_MASK: u8 = 0x1;

#[derive(Clone, Copy, Debug, PartialEq, enumn::N)]
#[repr(u8)]
/// Command codes for MMC (JESD84-B51A, 6.10.4).
///
/// Only a limited subset which are useful for the CQHCI driver are included.
pub enum MmcCommand {
    Switch = 6,
    SendStatus = 13,
}

impl MmcCommand {
    fn response_type(&self) -> DcmdResponseType {
        match self {
            Self::Switch => DcmdResponseType::R1B,
            Self::SendStatus => DcmdResponseType::R1,
        }
    }
}

// Necessary for bitfield
impl From<MmcCommand> for u8 {
    fn from(value: MmcCommand) -> Self {
        value as u8
    }
}

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    /// Response for CMD13 SEND_STATUS (JESD84-B51A, 6.10.4)
    pub struct MmcSendStatusResponse: u32 {
        const CURRENT_STATE_STDBY = 0x3 << 9;
        const CURRENT_STATE_TRAN = 0x4 << 9;
        const CURRENT_STATE_DATA = 0x5 << 9;
        const CURRENT_STATE_RECV = 0x6 << 9;
        const CURRENT_STATE_SLP = 0xa << 9;
        const READY_FOR_DATA = 1 << 8;
        const SWITCH_ERR = 1 << 7;
        const EXCEPTION_EVENT = 1 << 6;
        const APP_CMD = 1 << 5;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Direction of data transfer.
pub enum Direction {
    Read,
    Write,
}

// All task descriptors have a constant act
const TASK_DESCRIPTOR_ACT: u8 = 0b101;

bitfield! {
    #[derive(
        Clone, Copy, Eq, PartialEq, zerocopy::FromBytes, zerocopy::IntoBytes, zerocopy::Immutable,
    )]
    /// A task descriptor in the CQHCI Task Descriptor List (JESD84-B51A, B.2.1)
    pub struct CommandQueueTaskDescriptor(u128);
    impl Debug;
    bool, valid, set_valid: 0;
    bool, end, set_end: 1;
    bool, int, set_int: 2;
    u8, act, set_act: 5, 3;
    bool, forced_programming, set_forced_programming: 6;
    u8, context_id, set_context_id: 10, 7;
    bool, tag_request, set_tag_request: 11;
    bool, data_direction, set_data_direction: 12;
    bool, priority, set_priority: 13;
    bool, qbr, set_qbr: 14;
    bool, reliable_write, set_reliable_write: 15;
    u16, block_count, set_block_count: 31, 16;
    u64, block_offset, set_block_offset: 95, 32;
    // 96..=127 reserved
}

impl CommandQueueTaskDescriptor {
    fn new(direction: Direction, block_offset: u64, block_count: NonZeroU16) -> Self {
        let mut this = Self(0);
        this.set_valid(true);
        this.set_end(true);
        this.set_int(true);
        this.set_act(TASK_DESCRIPTOR_ACT);
        this.set_data_direction(direction == Direction::Read);
        this.set_block_count(block_count.get());
        this.set_block_offset(block_offset);
        this
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DcmdResponseType {
    /// No response is expected for the command
    NoResponse = 0b00,
    /// Normal response expected
    R1 = 0b10,
    /// Like R1, but with an optional busy signal transmitted on the DATA line.
    R1B = 0b11,
}

impl DcmdResponseType {
    pub const R4: Self = Self::R1;
    pub const R5: Self = Self::R1;
}

// Necessary for bitfield
impl From<DcmdResponseType> for u8 {
    fn from(value: DcmdResponseType) -> Self {
        value as u8
    }
}

bitfield! {
    #[derive(
        Clone, Copy, Eq, PartialEq, zerocopy::FromBytes, zerocopy::IntoBytes, zerocopy::Immutable,
    )]
    /// A Direct Command task descriptor in the CQHCI Task Descriptor List (JESD84-B51A, B.2.3)
    pub struct CommandQueueDirectCmdTaskDescriptor(u128);
    impl Debug;
    pub bool, valid, set_valid: 0;
    pub bool, end, set_end: 1;
    pub bool, int, set_int: 2;
    pub u8, act, set_act: 5, 3;
    pub bool, qbr, set_qbr: 14;
    pub u8, from into MmcCommand, _, set_cmd_index: 21, 16;
    pub bool, cmd_timing, set_cmd_timing: 22;
    pub u8, from into DcmdResponseType, _, set_response_type: 24, 23;
    pub u32, cmd_arg, set_cmd_arg: 63, 32;
}

impl CommandQueueDirectCmdTaskDescriptor {
    fn new(command: MmcCommand, command_arg: u32) -> Self {
        let mut this = Self(0);
        this.set_valid(true);
        this.set_end(true);
        this.set_act(TASK_DESCRIPTOR_ACT);
        this.set_qbr(true);
        this.set_int(true);
        this.set_cmd_index(command);
        let response_type = command.response_type();
        this.set_response_type(response_type);
        // Whether the command may be sent to device during data activity or busy time.
        // From the spec: "NOTE Shall be set to 0 if response type is b11 (R1b)"
        this.set_cmd_timing(response_type != DcmdResponseType::R1B);
        this.set_cmd_arg(command_arg);
        this
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
/// A wrapper around the transfer length field in CQHCI transfer descriptors.
/// The raw value of 0 is interpreted as 64KiB, which is hidden behind this type for clarity
/// (JESD84-B51A, B.3.2).
pub struct TransferBytes(u16);

impl TransferBytes {
    /// The maximum number of bytes which can be referenced by a single transfer descriptor.
    pub const MAX_BYTES: usize = u16::MAX as usize + 1;

    /// The maximum number of blocks which can be referenced by a single transfer descriptor.
    pub const MAX_BLOCKS: u64 = Self::MAX_BYTES as u64 / MMC_BLOCK_SIZE;

    pub const MAX: Self = Self(0);
}

impl From<TransferBytes> for u32 {
    fn from(length: TransferBytes) -> u32 {
        if length == TransferBytes::MAX { TransferBytes::MAX_BYTES as u32 } else { length.0 as u32 }
    }
}

impl TryFrom<usize> for TransferBytes {
    type Error = usize;

    fn try_from(size: usize) -> Result<Self, Self::Error> {
        if size == 0 {
            Err(size)
        } else if size < Self::MAX_BYTES {
            debug_assert!(size <= u16::MAX as usize);
            Ok(Self(size as u16))
        } else if size == Self::MAX_BYTES {
            Ok(Self::MAX)
        } else {
            Err(size)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum TransferAct {
    /// The transfer descriptor points to a data region to read/write to.
    Tran = 0b100,
    /// The transfer descriptor points to a list of transfer descriptors.
    Link = 0b110,
}

// Necessary for bitfield
impl From<TransferAct> for u8 {
    fn from(value: TransferAct) -> Self {
        value as u8
    }
}

bitfield! {
    #[derive(
        Clone, Copy, Eq, PartialEq, zerocopy::FromBytes, zerocopy::IntoBytes, zerocopy::Immutable,
    )]
    /// A transfer descriptor in the CQHCI Task Descriptor List (JESD84-B51A, B.2.2).
    pub struct CommandQueueTransferDescriptor(u128);
    impl Debug;
    bool, valid, set_valid: 0;
    bool, end, set_end: 1;
    bool, int, set_int: 2;
    u8, from into TransferAct, _, set_act: 5, 3;
    // 6..=15 reserved
    u16, length, set_length: 31, 16;
    u64, address, set_address: 95, 32;
    // 96..=127 reserved
}

impl CommandQueueTransferDescriptor {
    /// Creates a new [`CommandQueueTransferDescriptor`] pointing to a data buffer.
    pub fn transfer(address: u64, length: TransferBytes, end: bool) -> Self {
        let mut this = Self(0);
        this.set_valid(true);
        this.set_end(end);
        this.set_int(false);
        this.set_act(TransferAct::Tran);
        this.set_length(length.0);
        this.set_address(address);
        this
    }

    /// Creates a new [`CommandQueueTransferDescriptor`] pointing to a list of transfer descriptors.
    pub fn link(address: u64) -> Self {
        let mut this = Self(0);
        this.set_valid(true);
        this.set_end(false);
        this.set_int(false);
        this.set_act(TransferAct::Link);
        this.set_address(address);
        this
    }
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, Eq, PartialEq, zerocopy::FromBytes, zerocopy::IntoBytes, zerocopy::Immutable,
)]
/// An entry in the CQHCI Task Descriptor List (JESD84-B51A, B.2).
///
/// Note that this assumes 16-byte descriptors.
pub struct CommandQueueTDLEntry {
    task: CommandQueueTaskDescriptor,
    transfer: CommandQueueTransferDescriptor,
}

impl CommandQueueTDLEntry {
    /// Creates a new [`CommandQueueTDLEntry`] which points to a single memory region at
    /// `phys_address`.
    ///
    /// The caller must ensure that `block_count` does not exceed the maximum transfer size of
    /// [`TransferBytes::MAX_BLOCKS`], otherwise an error is returned.
    pub fn single_buffer(
        direction: Direction,
        block_offset: u64,
        block_count: NonZeroU16,
        phys_address: u64,
    ) -> Result<Self, ()> {
        // Unwrap OK because the caller should never pass a block_count which would exceed 64KiB of
        // data.
        let length = TransferBytes::try_from(block_count.get() as usize * MMC_BLOCK_SIZE as usize)
            .map_err(|_| ())?;
        Ok(Self {
            task: CommandQueueTaskDescriptor::new(direction, block_offset, block_count),
            transfer: CommandQueueTransferDescriptor::transfer(phys_address, length, true),
        })
    }

    /// Creates a new [`CommandQueueTDLEntry`] which points to a list of
    /// [`CommandQueueTransferDescriptor`]s at `descriptors_phys_address`.  The caller must ensure
    /// that one or more descriptors, ending with one that has END set, is initialized at this
    /// address before submitting the task.
    pub fn scatter_gather_buffers(
        direction: Direction,
        block_offset: u64,
        block_count: NonZeroU16,
        descriptors_phys_address: u64,
    ) -> Self {
        debug_assert!(
            descriptors_phys_address
                .is_multiple_of(std::mem::align_of::<CommandQueueTransferDescriptor>() as u64)
        );
        Self {
            task: CommandQueueTaskDescriptor::new(direction, block_offset, block_count),
            transfer: CommandQueueTransferDescriptor::link(descriptors_phys_address),
        }
    }
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, Eq, PartialEq, zerocopy::FromBytes, zerocopy::IntoBytes, zerocopy::Immutable,
)]
/// A DCMD entry in the CQHCI Task Descriptor List (JESD84-B51A, B.2.2).
///
/// Note that this assumes 16-byte descriptors.
///
/// Should only be written into the DCMD slot in the TDL; regular transfers must be of type
/// [`CommandQueueTDLEntry`].
pub struct CommandQueueTDLDirectCmdEntry {
    task: CommandQueueDirectCmdTaskDescriptor,
    _transfer: u128,
}

impl CommandQueueTDLDirectCmdEntry {
    pub fn new(command: MmcCommand, command_arg: u32) -> Self {
        Self { task: CommandQueueDirectCmdTaskDescriptor::new(command, command_arg), _transfer: 0 }
    }
}

pub const CQHCI_TASK_DESCRIPTOR_LIST_NUM_SLOTS: usize = 32;
pub const CQHCI_TASK_DESCRIPTOR_LIST_DCMD_SLOT: u8 = 31;
pub const CQHCI_TASK_DESCRIPTOR_LIST_SIZE: usize =
    CQHCI_TASK_DESCRIPTOR_LIST_NUM_SLOTS * size_of::<CommandQueueTDLEntry>();

// CQHCI registers (JESD84-B51A, B.3.1)

pub const CQHCI_CQ_VER_OFFSET: usize = 0x0;
pub const CQHCI_CQ_CAP_OFFSET: usize = 0x4;
pub const CQHCI_CQ_CFG_OFFSET: usize = 0x8;
pub const CQHCI_CQ_CTL_OFFSET: usize = 0xC;
pub const CQHCI_CQ_IS_OFFSET: usize = 0x10;
pub const CQHCI_CQ_ISTE_OFFSET: usize = 0x14;
pub const CQHCI_CQ_ISGE_OFFSET: usize = 0x18;
pub const CQHCI_CQ_IC_OFFSET: usize = 0x1c;
pub const CQHCI_CQ_TDLBA_OFFSET: usize = 0x20;
pub const CQHCI_CQ_TDLBAU_OFFSET: usize = 0x24;
pub const CQHCI_CQ_TDBR_OFFSET: usize = 0x28;
pub const CQHCI_CQ_TCN_OFFSET: usize = 0x2C;
pub const CQHCI_CQ_DQS_OFFSET: usize = 0x30;
pub const CQHCI_CQ_DPT_OFFSET: usize = 0x34;
pub const CQHCI_CQ_TDPE_OFFSET: usize = 0x3C;
pub const CQHCI_CQ_SSC1_OFFSET: usize = 0x40;
pub const CQHCI_CQ_SSC2_OFFSET: usize = 0x44;
pub const CQHCI_CQ_CRDCT_OFFSET: usize = 0x48;
pub const CQHCI_CQ_RMEM_OFFSET: usize = 0x50;
pub const CQHCI_CQ_TERRI_OFFSET: usize = 0x54;
pub const CQHCI_CQ_CRI_OFFSET: usize = 0x58;
pub const CQHCI_CQ_CRA_OFFSET: usize = 0x5C;
pub const CQHCI_CQ_HCCAP_OFFSET: usize = 0x60;
pub const CQHCI_CQ_HCCFG_OFFSET: usize = 0x64;
// The following registers are valid iff CS is set in CQHCI_CQ_CAP_OFFSET.
pub const CQHCI_CQ_CRYPTO_NQP_OFFSET: usize = 0x70;
pub const CQHCI_CQ_CRYPTO_NQDUN_OFFSET: usize = 0x74;
pub const CQHCI_CQ_CRYPTO_NQIS_OFFSET: usize = 0x78;
pub const CQHCI_CQ_CRYPTO_NQIE_OFFSET: usize = 0x7C;
pub const CQHCI_CQ_CRYPTO_CAP_OFFSET: usize = 0x100;

bitfield! {
    #[derive(Clone, Copy)]
    pub struct CqhciCqCapsRegister(u32);
    impl Debug;
    pub u16, timer_clock_freq, set_timer_clock_freq: 9, 0;
    pub u8, timer_clock_freq_multiplier, set_timer_clock_freq_multiplier: 15, 12;
    pub bool, crypto_support, set_crypto_support: 28;
}

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct CqhciCqCfgRegister: u32 {
        const DCMD_ENABLE = 1 << 12;
        const TASK_DESC_128 = 1 << 8;  // If 0, 64-bit
        const CRYPTO_ENABLE = 1 << 1;
        const CQE_ENABLE = 1;
    }
}

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct CqhciCqCtlRegister: u32 {
        const CLEAR_ALL_TASKS = 1 << 8;
        const HALT = 1;
    }
}

bitfield! {
    #[derive(Clone, Copy)]
    pub struct CqhciCqSendStatusConfiguration1Register(u32);
    impl Debug;
    pub u16, ssc_idle_timer, set_ssc_idle_timer: 15, 0;
    pub u8, ssc_block_counter, set_ssc_block_counter: 19, 16;
}

bitfield! {
    #[derive(Clone, Copy)]
    pub struct CqhciCqSendStatusConfiguration2Register(u32);
    impl Debug;
    impl New;
    pub u16, rca, set_rca: 15, 0;
}

// TODO(https://fxbug.dev/42176727): Add crypto errors.
bitfield! {
    #[derive(Clone, Copy)]
    pub struct CqhciCqInterruptStatusRegister(u32);
    impl Debug;
    pub bool, halt_complete, set_halt_complete: 0;
    pub bool, task_complete, set_task_complete: 1;
    pub bool, response_error_detected, set_response_error_detected: 2;
    pub bool, task_cleared, set_task_cleared: 3;
    pub bool, general_crypto_error, set_general_crypto_error: 4;
    pub bool, invalid_crypto_config_error, set_invalid_crypto_config_error: 5;
    pub bool, device_exception_event, set_device_exception_event: 6;
    pub bool, host_controller_fatal_error, set_host_controller_fatal_error: 7;
}

impl CqhciCqInterruptStatusRegister {
    pub fn is_error(&self) -> bool {
        self.response_error_detected()
            || self.general_crypto_error()
            || self.invalid_crypto_config_error()
            || self.device_exception_event()
            || self.host_controller_fatal_error()
    }
}

bitfield! {
    #[derive(Clone, Copy)]
    pub struct CqhciCqInterruptStatusEnableRegister(u32);
    impl Debug;
    pub bool, halt_complete, set_halt_complete: 0;
    pub bool, task_complete, set_task_complete: 1;
    pub bool, response_error_detected, set_response_error_detected: 2;
    pub bool, task_cleared, set_task_cleared: 3;
    pub bool, general_crypto_error, set_general_crypto_error: 4;
    pub bool, invalid_crypto_config_error, set_invalid_crypto_config_error: 5;
    pub bool, device_exception_event, set_device_exception_event: 6;
    pub bool, host_controller_fatal_error, set_host_controller_fatal_error: 7;
}

impl CqhciCqInterruptStatusEnableRegister {
    pub fn disabled() -> Self {
        Self(0)
    }
    pub fn enabled() -> Self {
        Self(0xff)
    }
}

bitfield! {
    #[derive(Clone, Copy)]
    pub struct CqhciCqInterruptSignalEnableRegister(u32);
    impl Debug;
    pub bool, halt_complete, set_halt_complete: 0;
    pub bool, task_complete, set_task_complete: 1;
    pub bool, response_error_detected, set_response_error_detected: 2;
    pub bool, task_cleared, set_task_cleared: 3;
    pub bool, general_crypto_error, set_general_crypto_error: 4;
    pub bool, invalid_crypto_config_error, set_invalid_crypto_config_error: 5;
    pub bool, device_exception_event, set_device_exception_event: 6;
    pub bool, host_controller_fatal_error, set_host_controller_fatal_error: 7;
}

impl CqhciCqInterruptSignalEnableRegister {
    pub fn disabled() -> Self {
        Self(0)
    }
    pub fn enabled() -> Self {
        Self(0xff)
    }
}

bitfield! {
    #[derive(Clone, Copy)]
    pub struct CqhciCqInterruptCoalescingRegister(u32);
    impl Debug;
    pub u8, ic_timeout_value, set_ic_timeout_value: 6, 0;
    pub bool, ic_timeout_value_write_enable, set_ic_timeout_value_write_enable: 7;
    pub u8, ic_counter_threshold, set_ic_counter_threshold: 12, 8;
    pub bool, ic_counter_threshold_write_enable, set_ic_counter_threshold_write_enable: 15;
    pub bool, ic_counter_timer_reset, set_ic_counter_timer_reset: 16;
    pub bool, ic_status_bit, set_ic_status_bit: 20;
    pub bool, ic_enable, set_ic_enable: 31;
}

impl CqhciCqInterruptCoalescingRegister {
    pub fn disabled() -> Self {
        let mut this = Self(0);
        this.set_ic_enable(false);
        this
    }
}

bitfield! {
    #[derive(Clone, Copy)]
    pub struct CqhciCqTaskErrorRegister(u32);
    impl Debug;
    pub u8, response_mode_error_command_index, _: 5, 0;
    pub u8, response_mode_error_task_id, _: 12, 8;
    pub bool, response_mode_error_fields_valid, _: 15;
    pub u8, data_transfer_error_command_index, _: 21, 16;
    pub u8, data_transfer_error_task_id, _: 28, 24;
    pub bool, data_transfer_error_fields_valid, _: 31;
}
