// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! UTP Transfer Request Descriptor (UTRD) layout, the UTRL, and per-slot request state (UFSHCI
//! 3.0/4.0 section 6.1.1).

use crate::UfsError;
use crate::constants::{
    COMMAND_TYPE_UFS_STORAGE, MAX_REQUEST_LIST_SIZE, OCS_OFFSET, TIMEOUT_RECLAIM_POLL_INTERVAL,
    UTP_COMMAND_DESCRIPTOR_SIZE, UTRD_COMMAND_TYPE_MASK, UTRD_COMMAND_TYPE_SHIFT,
    UTRD_DATA_DIRECTION_MASK, UTRD_DATA_DIRECTION_SHIFT, UTRD_INTERRUPT_BIT, UTRD_OCS_MASK,
};
use crate::dma_buffer::ContiguousDmaBuffer;
use crate::transfer_request_processor::{IoCompletion, IoTransfer};
use static_assertions::const_assert_eq;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// Bitmask with the low `count` bits set (`0..=64`).
pub(crate) const fn slot_mask(count: u8) -> u64 {
    if count >= 64 { u64::MAX } else { (1u64 << count) - 1 }
}

/// Overall Command Status (`OCS`) written by the host controller into UTRD DW2
/// (UFSHCI 3.0/4.0 section 6.1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum OverallCommandStatus {
    Success = 0x00,
    InvalidCommandTableAttributes = 0x01,
    InvalidPrdtAttributes = 0x02,
    MismatchDataBufferSize = 0x03,
    MismatchResponseUpiuSize = 0x04,
    CommunicationFailure = 0x05,
    Aborted = 0x06,
    HostControllerFatalError = 0x07,
    DeviceFatalError = 0x08,
    InvalidCryptoConfiguration = 0x09,
    GeneralCryptoError = 0x0A,
    /// Written by the driver before ringing the doorbell; the controller overwrites it on
    /// completion.
    Invalid = 0x0F,
}

impl OverallCommandStatus {
    /// Decodes a raw OCS value, returning `None` for reserved values.
    pub fn from_raw(value: u8) -> Option<Self> {
        Some(match value {
            0x00 => Self::Success,
            0x01 => Self::InvalidCommandTableAttributes,
            0x02 => Self::InvalidPrdtAttributes,
            0x03 => Self::MismatchDataBufferSize,
            0x04 => Self::MismatchResponseUpiuSize,
            0x05 => Self::CommunicationFailure,
            0x06 => Self::Aborted,
            0x07 => Self::HostControllerFatalError,
            0x08 => Self::DeviceFatalError,
            0x09 => Self::InvalidCryptoConfiguration,
            0x0A => Self::GeneralCryptoError,
            0x0F => Self::Invalid,
            _ => return None,
        })
    }
}

/// UTRD Data Direction (`DD`) field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum DataDirection {
    NoData = 0,
    /// Host to device (write).
    HostToDevice = 1,
    /// Device to host (read).
    DeviceToHost = 2,
}

/// UTP Transfer Request Descriptor (32 bytes, UFSHCI 3.0/4.0 section 6.1.1). Little-endian.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable,
)]
#[repr(C)]
pub struct TransferRequestDescriptor {
    /// DW0: Command Type (`CT`, 31:28), Data Direction (`DD`, 26:25), Interrupt (`I`, 24).
    pub dword0: u32,
    /// DW1: Data Unit Number Lower (`DUNL`).
    pub dword1: u32,
    /// DW2: Overall Command Status (`OCS`, 7:0).
    pub dword2: u32,
    /// DW3: Data Unit Number Upper (`DUNU`).
    pub dword3: u32,
    /// DW4: UTP Command Descriptor Base Address (`UCDBA`, 128-byte aligned).
    pub command_descriptor_base_addr: u32,
    /// DW5: UTP Command Descriptor Base Address Upper (`UCDBAU`).
    pub command_descriptor_base_addr_upper: u32,
    /// DW6: Response UPIU Offset (`RUO`, 31:16) and Length (`RUL`, 15:0), both in dwords.
    pub response_upiu_offset_and_length: u32,
    /// DW7: PRDT Offset (`PRDTO`, 31:16, in dwords) and Length (`PRDTL`, 15:0, in entries).
    pub prdt_offset_and_length: u32,
}

const_assert_eq!(std::mem::size_of::<TransferRequestDescriptor>(), 32);

impl TransferRequestDescriptor {
    /// Byte offset of DW2 (`OCS`) within the descriptor.
    pub const OCS_OFFSET: usize = OCS_OFFSET;
    /// UFS Storage command type.
    pub const COMMAND_TYPE_UFS_STORAGE: u8 = COMMAND_TYPE_UFS_STORAGE;

    /// Returns the Interrupt (`I`) bit.
    pub fn interrupt(&self) -> bool {
        (self.dword0 & UTRD_INTERRUPT_BIT) != 0
    }

    /// Sets or clears the Interrupt (`I`) bit.
    pub fn set_interrupt(&mut self, val: bool) {
        if val {
            self.dword0 |= UTRD_INTERRUPT_BIT;
        } else {
            self.dword0 &= !UTRD_INTERRUPT_BIT;
        }
    }

    /// Returns the raw 2-bit Data Direction (`DD`) field.
    pub fn data_direction(&self) -> u32 {
        (self.dword0 >> UTRD_DATA_DIRECTION_SHIFT) & UTRD_DATA_DIRECTION_MASK
    }

    /// Sets the Data Direction (`DD`) field.
    pub fn set_data_direction(&mut self, dir: DataDirection) {
        self.dword0 = (self.dword0 & !(UTRD_DATA_DIRECTION_MASK << UTRD_DATA_DIRECTION_SHIFT))
            | ((dir as u32) << UTRD_DATA_DIRECTION_SHIFT);
    }

    /// Returns the Command Type (`CT`) field.
    pub fn command_type(&self) -> u8 {
        ((self.dword0 >> UTRD_COMMAND_TYPE_SHIFT) & UTRD_COMMAND_TYPE_MASK) as u8
    }

    /// Sets the Command Type (`CT`) field.
    pub fn set_command_type(&mut self, val: u8) {
        self.dword0 = (self.dword0 & !(UTRD_COMMAND_TYPE_MASK << UTRD_COMMAND_TYPE_SHIFT))
            | ((u32::from(val) & UTRD_COMMAND_TYPE_MASK) << UTRD_COMMAND_TYPE_SHIFT);
    }

    /// Returns the raw Overall Command Status (`OCS`) field.
    pub fn overall_command_status(&self) -> u8 {
        (self.dword2 & UTRD_OCS_MASK) as u8
    }

    /// Sets the Overall Command Status (`OCS`) field.
    pub fn set_overall_command_status(&mut self, status: OverallCommandStatus) {
        self.dword2 = (self.dword2 & !UTRD_OCS_MASK) | status as u32;
    }

    /// Returns the Response UPIU offset in dwords.
    pub fn response_upiu_offset(&self) -> u16 {
        (self.response_upiu_offset_and_length >> 16) as u16
    }

    /// Returns the Response UPIU length in dwords.
    pub fn response_upiu_length(&self) -> u16 {
        (self.response_upiu_offset_and_length & 0xFFFF) as u16
    }

    /// Sets the Response UPIU offset and length, both in dwords.
    pub fn set_response_upiu_offset_and_length(&mut self, offset: u16, length: u16) {
        self.response_upiu_offset_and_length = ((offset as u32) << 16) | length as u32;
    }

    /// Returns the PRDT offset in dwords.
    pub fn prdt_offset(&self) -> u16 {
        (self.prdt_offset_and_length >> 16) as u16
    }

    /// Returns the PRDT entry count.
    pub fn prdt_length(&self) -> u16 {
        (self.prdt_offset_and_length & 0xFFFF) as u16
    }

    /// Sets the PRDT offset (in dwords) and entry count.
    pub fn set_prdt_offset_and_length(&mut self, offset: u16, length: u16) {
        self.prdt_offset_and_length = ((offset as u32) << 16) | length as u32;
    }
}

/// Lifecycle state of a UTRL slot.
///
/// `Free -> Reserved -> Scheduled -> {Completed, Timeout} -> Free`. The controller may access the
/// slot's buffers only in `Scheduled` and `Timeout`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    /// Idle and available for reservation.
    Free,
    /// Owned by a submitter that has not submitted the request yet.
    Reserved,
    /// Doorbell rung; the controller owns the slot.
    Scheduled,
    /// The doorbell bit cleared; `result` holds the outcome.
    Completed,
    /// Deadline passed and the request was aborted (`UTRLCLR`); waiting for the doorbell bit to
    /// clear.
    Timeout,
}

/// Who learns that a slot left `Scheduled`, and how.
#[derive(Debug, Default)]
pub(crate) enum SlotCompletion {
    /// Nobody: the completion path frees the slot once the controller is done with it.
    #[default]
    None,
    /// An async submitter awaiting the slot; it reads the response and frees the slot itself.
    Async(futures::channel::oneshot::Sender<SlotState>),
    /// A synchronous I/O request: the completion path decodes the response, frees the slot and
    /// delivers the outcome through `completion`.
    Io {
        /// LUN the command was addressed to; identifies the block server on completion.
        lun: u8,
        completion: IoCompletion,
        /// Read buffer whose cache is invalidated once the device has written it.
        read_back: Option<IoTransfer<'static>>,
    },
}

/// Per-slot state and UCD buffer.
#[derive(Debug)]
pub struct RequestSlot {
    pub(crate) state: SlotState,
    /// UCD holding the request UPIU, response UPIU and PRDT.
    pub(crate) command_descriptor_io: ContiguousDmaBuffer,
    /// Transport outcome recorded when the slot leaves `Scheduled`.
    pub(crate) result: Result<(), UfsError>,
    /// How the submitter learns that the slot left `Scheduled`.
    pub(crate) completion: SlotCompletion,
    /// Set while the slot carries a synchronous I/O request (`Scheduled` or `Timeout`); counted
    /// by [`RequestList::inflight_io`].
    pub(crate) sync_io: bool,
    /// Instant after which a `Scheduled` request is aborted.
    pub(crate) deadline: Option<zx::MonotonicInstant>,
    /// Pin of the client data VMO for the current request.
    pub(crate) pmt: Option<zx::Pmt>,
}

impl RequestSlot {
    /// Returns the current state.
    pub fn state(&self) -> SlotState {
        self.state
    }

    /// Returns the slot to `Free` and takes the pinned data VMO [`zx::Pmt`] so the caller can
    /// unpin it outside `request_list.lock()`.
    pub(crate) fn release(&mut self) -> Option<zx::Pmt> {
        self.state = SlotState::Free;
        self.completion = SlotCompletion::None;
        self.sync_io = false;
        self.deadline = None;
        self.pmt.take()
    }
}

/// UTRL DMA buffer plus per-slot state.
///
/// Dropping the list never unpins: pages the controller might still access are quarantined by
/// the kernel. See [`crate::transfer_request_processor::TransferRequestProcessor::quiesce`].
#[derive(Debug)]
pub struct RequestList {
    /// Array of [`TransferRequestDescriptor`]s.
    pub(crate) io_buffer: ContiguousDmaBuffer,
    pub(crate) request_slots: Vec<RequestSlot>,
    /// Bitmask of slots currently in [`SlotState::Free`].
    pub(crate) free_mask: u64,
    /// Bitmask of slots currently in [`SlotState::Scheduled`] or [`SlotState::Timeout`].
    pub(crate) scheduled_mask: u64,
    /// Set once the controller is quiesced; no further submissions are accepted.
    pub(crate) stopped: bool,
    /// Tasks to wake when an I/O slot is released (async reservations).
    pub(crate) slot_wakers: Vec<std::task::Waker>,
    /// Threads blocked in a synchronous I/O slot reservation; the condition variable is only
    /// signalled while this is non-zero.
    pub(crate) blocked_reservers: usize,
    /// Number of slots with [`RequestSlot::sync_io`] set; arms the synchronous I/O timeout task
    /// ([`crate::transfer_request_processor::TransferRequestProcessor::run_io_timeout_task`]).
    pub(crate) inflight_io: usize,
}

impl RequestList {
    /// Allocates a list of `slot_count` UTRDs and their UCDs.
    ///
    /// # Errors
    ///
    /// Returns `INVALID_ARGS` if `slot_count` is 0 or above [`MAX_REQUEST_LIST_SIZE`], or the
    /// DMA allocation error.
    pub fn new(bti: &zx::Bti, slot_count: u8) -> Result<Self, zx::Status> {
        if slot_count == 0 || slot_count > MAX_REQUEST_LIST_SIZE {
            return Err(zx::Status::INVALID_ARGS);
        }
        let io_buffer = ContiguousDmaBuffer::new(
            bti,
            slot_count as usize * std::mem::size_of::<TransferRequestDescriptor>(),
        )?;

        let mut request_slots = Vec::with_capacity(slot_count as usize);
        for _ in 0..slot_count {
            request_slots.push(RequestSlot {
                state: SlotState::Free,
                command_descriptor_io: ContiguousDmaBuffer::new(bti, UTP_COMMAND_DESCRIPTOR_SIZE)?,
                result: Ok(()),
                completion: SlotCompletion::None,
                sync_io: false,
                deadline: None,
                pmt: None,
            });
        }

        Ok(Self {
            io_buffer,
            request_slots,
            free_mask: slot_mask(slot_count),
            scheduled_mask: 0,
            stopped: false,
            slot_wakers: Vec::new(),
            blocked_reservers: 0,
            inflight_io: 0,
        })
    }

    /// Updates `slot`'s state and keeps `free_mask` and `scheduled_mask` in sync.
    ///
    /// # Panics
    ///
    /// Panics if `slot` is not below [`Self::slot_count`].
    pub(crate) fn set_slot_state(&mut self, slot: u8, state: SlotState) {
        let bit = 1u64 << slot;
        self.request_slots[slot as usize].state = state;
        match state {
            SlotState::Free => {
                self.free_mask |= bit;
                self.scheduled_mask &= !bit;
            }
            SlotState::Reserved | SlotState::Completed => {
                self.free_mask &= !bit;
                self.scheduled_mask &= !bit;
            }
            SlotState::Scheduled | SlotState::Timeout => {
                self.free_mask &= !bit;
                self.scheduled_mask |= bit;
            }
        }
        #[cfg(any(debug_assertions, test))]
        self.assert_masks_consistent();
    }

    /// Checks that `free_mask` and `scheduled_mask` match the per-slot states.
    ///
    /// # Panics
    ///
    /// Panics (debug builds and tests only) if either mask has drifted from the slot states.
    #[cfg(any(debug_assertions, test))]
    pub(crate) fn assert_masks_consistent(&self) {
        let mut expected_free = 0u64;
        let mut expected_scheduled = 0u64;
        for (i, slot) in self.request_slots.iter().enumerate() {
            let bit = 1u64 << i;
            match slot.state {
                SlotState::Free => expected_free |= bit,
                SlotState::Scheduled | SlotState::Timeout => expected_scheduled |= bit,
                SlotState::Reserved | SlotState::Completed => {}
            }
        }
        debug_assert_eq!(self.free_mask, expected_free, "free_mask out of sync with slot states");
        debug_assert_eq!(
            self.scheduled_mask, expected_scheduled,
            "scheduled_mask out of sync with slot states"
        );
    }

    /// Returns the slot at `slot`.
    ///
    /// # Errors
    ///
    /// Returns `InvalidSlot` if `slot` is out of range.
    pub fn slot(&self, slot: u8) -> Result<&RequestSlot, UfsError> {
        self.request_slots.get(slot as usize).ok_or(UfsError::InvalidSlot(slot as u32))
    }

    /// Returns the slot at `slot` mutably.
    ///
    /// # Errors
    ///
    /// Returns `InvalidSlot` if `slot` is out of range.
    pub(crate) fn slot_mut(&mut self, slot: u8) -> Result<&mut RequestSlot, UfsError> {
        self.request_slots.get_mut(slot as usize).ok_or(UfsError::InvalidSlot(slot as u32))
    }

    /// Returns the number of slots.
    pub fn slot_count(&self) -> u8 {
        self.request_slots.len() as u8
    }

    /// Returns when the synchronous I/O timeout task next needs to look at the slots in
    /// `io_mask`: the earliest deadline of a `Scheduled` slot carrying a synchronous request, or
    /// `now + TIMEOUT_RECLAIM_POLL_INTERVAL` if such a slot is in `Timeout` (awaiting its doorbell
    /// bit to clear after `UTRLCLR`, which raises no interrupt), whichever is earlier. `None` when
    /// no synchronous request is in flight.
    pub(crate) fn next_io_deadline(&self, io_mask: u64) -> Option<zx::MonotonicInstant> {
        let mut earliest: Option<zx::MonotonicInstant> = None;
        let mut remaining = self.scheduled_mask & io_mask;
        while remaining != 0 {
            let slot = &self.request_slots[remaining.trailing_zeros() as usize];
            remaining &= remaining - 1;
            if !slot.sync_io {
                continue;
            }
            let candidate = match slot.state {
                SlotState::Scheduled => slot.deadline,
                SlotState::Timeout => {
                    Some(zx::MonotonicInstant::after(TIMEOUT_RECLAIM_POLL_INTERVAL))
                }
                SlotState::Free | SlotState::Reserved | SlotState::Completed => None,
            };
            if let Some(candidate) = candidate {
                earliest = Some(earliest.map_or(candidate, |e| e.min(candidate)));
            }
        }
        earliest
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fake_bti::FakeBti;

    #[fuchsia::test]
    fn test_transfer_request_descriptor_fields() {
        let mut desc = TransferRequestDescriptor::default();
        assert_eq!(desc.data_direction(), DataDirection::NoData as u32);
        desc.set_data_direction(DataDirection::HostToDevice);
        assert_eq!(desc.data_direction(), DataDirection::HostToDevice as u32);
        desc.set_data_direction(DataDirection::DeviceToHost);
        assert_eq!(desc.data_direction(), DataDirection::DeviceToHost as u32);
        desc.set_command_type(1);
        desc.set_interrupt(true);
        assert_eq!(desc.dword0, 0x1500_0000);

        desc.set_overall_command_status(OverallCommandStatus::Invalid);
        assert_eq!(desc.overall_command_status(), 0x0F);
        desc.set_response_upiu_offset_and_length(8, 14);
        assert_eq!(desc.response_upiu_offset_and_length, 0x0008_000E);
        desc.set_prdt_offset_and_length(22, 3);
        assert_eq!((desc.prdt_offset(), desc.prdt_length()), (22, 3));
    }

    #[fuchsia::test]
    fn test_overall_command_status_decode() {
        assert_eq!(OverallCommandStatus::from_raw(0x00), Some(OverallCommandStatus::Success));
        assert_eq!(OverallCommandStatus::from_raw(0x06), Some(OverallCommandStatus::Aborted));
        assert_eq!(
            OverallCommandStatus::from_raw(0x0A),
            Some(OverallCommandStatus::GeneralCryptoError)
        );
        assert_eq!(OverallCommandStatus::from_raw(0x0F), Some(OverallCommandStatus::Invalid));
        assert_eq!(OverallCommandStatus::from_raw(0x0B), None);
    }

    #[fuchsia::test]
    fn test_request_list_creation() {
        let bti = FakeBti::create().expect("Failed to create fake bti");
        let list = RequestList::new(&bti, 32).expect("Failed to create RequestList");
        assert_eq!(list.slot_count(), 32);
        let slot = list.slot(0).unwrap();
        assert_eq!(slot.state(), SlotState::Free);
        assert_eq!(slot.command_descriptor_io.size(), 8192);
        assert_eq!(list.slot(32).unwrap_err(), UfsError::InvalidSlot(32));

        assert!(RequestList::new(&bti, 0).is_err());
        assert!(RequestList::new(&bti, 65).is_err());
    }
}
