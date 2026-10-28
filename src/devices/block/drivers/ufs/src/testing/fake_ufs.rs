// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Fake UFS host controller and device for unit and integration tests.

use crate::UfsError;
use crate::constants::{GEOMETRY_SIZE_UNIT, MIN_LOGICAL_BLOCK_SIZE, UTRD_OCS_MASK};
use crate::descriptors::{DescriptorType, DeviceDescriptor, GeometryDescriptor};
use crate::registers::{
    HostControllerStatusReg, InterruptStatusReg, PowerModeStatus, UicCommandArgument2Reg,
    UicCommandArgument3Reg, UicCommandReg, UtrListDoorBellReg,
};
use crate::request_list::{DataDirection, TransferRequestDescriptor};
use crate::scsi::{InquiryData, ReadCapacity10Data, ScsiOpcode};
use crate::transfer_request_processor::TransferRequestProcessor;
use mmio::region::MmioRegion;
use mmio::vmo::{VmoMapping, VmoMemory};
use mmio::{Mmio, MmioError, ReadableRegister, Register, WritableRegister};
use std::mem::size_of;
use std::sync::Arc;
use zerocopy::byteorder::big_endian::{U16, U32, U64};
use zerocopy::{FromBytes, IntoBytes};

/// `HCS.DP`: a device is attached to the link.
pub const HCS_DEVICE_PRESENT: u32 = 1 << 0;
/// `HCS.UCRDY`: the controller accepts a UIC command.
pub const HCS_UIC_COMMAND_READY: u32 = 1 << 3;
/// `CAP.AUTOH8`: the controller supports auto-hibernate.
pub const CAP_AUTO_HIBERNATE_SUPPORT: u32 = 1 << 23;

/// Returns an `HCS` value whose `UPMCRS` field reads `status`.
pub fn hcs_power_mode_status(status: PowerModeStatus) -> u32 {
    (status as u32) << 8
}

/// Fakes the UFS device behind the host controller: its descriptors and SCSI command responses.
pub struct FakeUfs {
    pub device_desc: DeviceDescriptor,
    pub geometry_desc: GeometryDescriptor,
}

impl FakeUfs {
    /// Creates a new `FakeUfs` with 4 logical units.
    pub fn new() -> Self {
        Self {
            device_desc: DeviceDescriptor {
                length: size_of::<DeviceDescriptor>() as u8,
                descriptor_idn: DescriptorType::Device as u8,
                device: 0x00,
                device_class: 0x00,
                device_sub_class: 0x01,
                protocol: 0x00,
                number_lu: 0x04,
                number_wlu: 0x04,
                init_power_mode: 0x01,
                high_priority_lun: 0x7F,
                spec_version: U16::new(0x0310),
                ud0_base_offset: 0x16,
                ud_config_p_length: 0x1A,
                ..Default::default()
            },
            geometry_desc: GeometryDescriptor {
                length: size_of::<GeometryDescriptor>() as u8,
                descriptor_idn: DescriptorType::Geometry as u8,
                total_raw_device_capacity: U64::new(0x8000), // 16 MiB in 512 B sectors
                max_number_lu: 0x01,
                segment_size: U32::new(GEOMETRY_SIZE_UNIT),
                allocation_unit_size: 1,
                ..Default::default()
            },
        }
    }

    /// Returns the data-in bytes a SCSI command `cdb` produces on the fake device, or
    /// `CheckCondition` for an unsupported read.
    pub fn execute_scsi_cmd(
        &mut self,
        cdb: &[u8],
        direction: DataDirection,
    ) -> Result<Vec<u8>, UfsError> {
        const TEST_UNIT_READY: u8 = ScsiOpcode::TestUnitReady as u8;
        const INQUIRY: u8 = ScsiOpcode::Inquiry as u8;
        const READ_CAPACITY_10: u8 = ScsiOpcode::ReadCapacity10 as u8;
        const READ_10: u8 = ScsiOpcode::Read10 as u8;
        const READ_16: u8 = ScsiOpcode::Read16 as u8;
        if self.device_desc.number_lu == 0 {
            return Err(UfsError::CheckCondition);
        }
        match *cdb.first().ok_or(UfsError::InvalidParameter)? {
            TEST_UNIT_READY => Ok(Vec::new()),
            INQUIRY => {
                let inquiry = InquiryData {
                    peripheral_device_type: 0x00,
                    rmb: 0x00,
                    version: 0x06,
                    response_data_format: 0x02,
                    additional_length: 31,
                    vendor_id: *b"FUCHSIA ",
                    product_id: *b"UFS MOCK DEVICE ",
                    product_revision: *b"1.00",
                    ..Default::default()
                };
                Ok(inquiry.as_bytes().to_vec())
            }
            READ_CAPACITY_10 => {
                // The geometry descriptor counts 512-byte sectors; expose 4 KiB blocks.
                let sectors = self.geometry_desc.total_raw_device_capacity.get();
                let capacity = ReadCapacity10Data {
                    returned_logical_block_address: U32::new((sectors / 8 - 1) as u32),
                    block_length_in_bytes: U32::new(MIN_LOGICAL_BLOCK_SIZE),
                };
                Ok(capacity.as_bytes().to_vec())
            }
            READ_10 | READ_16 => Ok(vec![0xAA; MIN_LOGICAL_BLOCK_SIZE as usize]),
            _ if direction == DataDirection::HostToDevice => Ok(Vec::new()),
            _ => Err(UfsError::CheckCondition),
        }
    }
}

/// Driver-side MMIO backed by a plain VMO that emulates the register semantics the driver relies
/// on: `IS` is write-1-to-clear and `UTRLDBR` is write-1-to-set. Other registers are plain memory;
/// the fake hardware updates them through [`FakeUtp::regs`] or its own VMO mapping.
pub struct FakeMmio {
    inner: MmioRegion<VmoMemory>,
}

impl FakeMmio {
    /// Maps `size` bytes of `vmo`.
    pub fn new(vmo: &zx::Vmo, size: usize) -> Self {
        let vmo = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        Self { inner: VmoMapping::map(0, size, vmo).unwrap() }
    }
}

impl Mmio for FakeMmio {
    fn len(&self) -> usize {
        self.inner.len()
    }

    fn align_offset(&self, align: usize) -> usize {
        self.inner.align_offset(align)
    }

    fn try_load8(&self, offset: usize) -> Result<u8, MmioError> {
        self.inner.try_load8(offset)
    }

    fn try_load16(&self, offset: usize) -> Result<u16, MmioError> {
        self.inner.try_load16(offset)
    }

    fn try_load32(&self, offset: usize) -> Result<u32, MmioError> {
        self.inner.try_load32(offset)
    }

    fn try_load64(&self, offset: usize) -> Result<u64, MmioError> {
        self.inner.try_load64(offset)
    }

    fn try_store8(&mut self, offset: usize, value: u8) -> Result<(), MmioError> {
        self.inner.try_store8(offset, value)
    }

    fn try_store16(&mut self, offset: usize, value: u16) -> Result<(), MmioError> {
        self.inner.try_store16(offset, value)
    }

    fn try_store32(&mut self, offset: usize, value: u32) -> Result<(), MmioError> {
        let value = match offset {
            InterruptStatusReg::OFFSET => self.inner.try_load32(offset)? & !value,
            UtrListDoorBellReg::OFFSET => self.inner.try_load32(offset)? | value,
            _ => value,
        };
        self.inner.try_store32(offset, value)
    }

    fn try_store64(&mut self, offset: usize, value: u64) -> Result<(), MmioError> {
        self.inner.try_store64(offset, value)
    }

    fn write_barrier(&self) {
        self.inner.write_barrier()
    }

    fn read_barrier(&self) {
        self.inner.read_barrier()
    }
}

/// Upper bound on any fake-hardware wait, so a driver regression fails instead of hanging.
pub const FAKE_HW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Polls `condition` every 100 us until it holds or [`FAKE_HW_TIMEOUT`] passes.
pub fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
    let start = std::time::Instant::now();
    while !condition() {
        if start.elapsed() > FAKE_HW_TIMEOUT {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_micros(100));
    }
    true
}

/// Controller-side view of a transfer request processor: registers, UTRL and UCDs.
pub struct FakeUtp {
    /// Raw register mapping (no W1C/W1S emulation).
    pub regs: MmioRegion<VmoMemory>,
    utrl: MmioRegion<VmoMemory>,
    ucds: Vec<MmioRegion<VmoMemory>>,
    /// Delivers a transfer completion interrupt to the processor, as the IRQ thread would.
    on_interrupt: Box<dyn Fn() + Send + Sync>,
}

/// Maps `size` bytes of `vmo` for the fake hardware. Register VMOs get the usual MMIO cache
/// policy; DMA buffers already carry the policy the driver set, which cannot change once mapped.
pub fn map_vmo(vmo: &zx::Vmo, size: usize) -> MmioRegion<VmoMemory> {
    let vmo = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
    let policy = match vmo.info().unwrap().cache_policy() {
        zx::CachePolicy::Cached => zx::CachePolicy::UnCachedDevice,
        policy => policy,
    };
    VmoMapping::map_with_cache_policy(0, size, vmo, policy).unwrap()
}

/// Reads a `T` at `base` with 32-bit loads, as the controller would.
fn load_struct<T: FromBytes + IntoBytes>(region: &MmioRegion<VmoMemory>, base: usize) -> T {
    let mut value = T::new_zeroed();
    for (i, chunk) in value.as_mut_bytes().chunks_exact_mut(4).enumerate() {
        chunk.copy_from_slice(&region.load32(base + i * 4).to_ne_bytes());
    }
    value
}

impl FakeUtp {
    /// Maps the register VMO and all of `processor`'s descriptor buffers. Completions raised
    /// through [`Self::complete`] are delivered to `processor` by interrupt (while it is alive).
    pub fn new<M: Mmio + Send + Sync + 'static>(
        mmio_vmo: &zx::Vmo,
        mmio_size: usize,
        processor: &Arc<TransferRequestProcessor<M>>,
    ) -> Self {
        let weak = Arc::downgrade(processor);
        let on_interrupt = Box::new(move || {
            if let Some(processor) = weak.upgrade() {
                processor.service_interrupt();
            }
        });
        let list = processor.request_list.lock();
        Self {
            regs: map_vmo(mmio_vmo, mmio_size),
            utrl: map_vmo(list.io_buffer.vmo(), list.io_buffer.size()),
            ucds: list
                .request_slots
                .iter()
                .map(|slot| {
                    map_vmo(slot.command_descriptor_io.vmo(), slot.command_descriptor_io.size())
                })
                .collect(),
            on_interrupt,
        }
    }

    /// Returns `UTRLDBR`.
    pub fn doorbell(&self) -> u32 {
        UtrListDoorBellReg::read(&self.regs).door_bell()
    }

    /// Waits for `slot`'s doorbell bit to be set.
    pub fn wait_for_doorbell(&self, slot: u8) -> bool {
        wait_until(|| self.doorbell() & (1 << slot) != 0)
    }

    /// Clears `slot`'s doorbell bit, as the controller does on completion. Raises no interrupt.
    pub fn clear_doorbell(&mut self, slot: u8) {
        let doorbell = self.doorbell() & !(1 << slot);
        self.regs.store32(UtrListDoorBellReg::OFFSET, doorbell);
    }

    /// Returns `slot`'s UTRD.
    pub fn utrd(&self, slot: u8) -> TransferRequestDescriptor {
        load_struct(&self.utrl, slot as usize * std::mem::size_of::<TransferRequestDescriptor>())
    }

    /// Writes `ocs` into `slot`'s UTRD. Raises no interrupt.
    pub fn set_ocs(&mut self, slot: u8, ocs: u8) {
        let offset = slot as usize * std::mem::size_of::<TransferRequestDescriptor>()
            + TransferRequestDescriptor::OCS_OFFSET;
        let dword2 = self.utrl.load32(offset);
        self.utrl.store32(offset, (dword2 & !UTRD_OCS_MASK) | ocs as u32);
    }

    /// Reads `len` bytes of `slot`'s UCD at `offset`.
    pub fn read_ucd(&self, slot: u8, offset: usize, len: usize) -> Vec<u8> {
        (0..len).map(|i| self.ucds[slot as usize].load8(offset + i)).collect()
    }

    /// Writes `bytes` into `slot`'s UCD at `offset`. Raises no interrupt.
    pub fn write_ucd(&mut self, slot: u8, offset: usize, bytes: &[u8]) {
        for (i, byte) in bytes.iter().enumerate() {
            self.ucds[slot as usize].store8(offset + i, *byte);
        }
    }

    /// Reads the request UPIU of `slot` as a `T`.
    pub fn request<T: FromBytes>(&self, slot: u8) -> T {
        T::read_from_bytes(&self.read_ucd(slot, 0, std::mem::size_of::<T>())).unwrap()
    }

    /// Sets `IS.UTRCS` and delivers the interrupt to the processor, as the controller and IRQ
    /// thread do once a doorbell bit has been cleared.
    pub fn raise_completion_interrupt(&mut self) {
        let mut is = InterruptStatusReg::read(&self.regs);
        is.set_utp_transfer_request_completion_status(true);
        is.write(&mut self.regs);
        (self.on_interrupt)();
    }

    /// Writes `response` at the UTRD's response offset, sets `ocs` and clears the doorbell bit.
    /// Raises no interrupt; see [`Self::complete`].
    pub fn write_completion(&mut self, slot: u8, response: &[u8], ocs: u8) {
        let offset = self.utrd(slot).response_upiu_offset() as usize * 4;
        self.write_ucd(slot, offset, response);
        self.set_ocs(slot, ocs);
        self.clear_doorbell(slot);
    }

    /// Completes `slot` the way the hardware does: [`Self::write_completion`] followed by
    /// [`Self::raise_completion_interrupt`].
    pub fn complete(&mut self, slot: u8, response: &[u8], ocs: u8) {
        self.write_completion(slot, response, ocs);
        self.raise_completion_interrupt();
    }
}

/// Maps `vmo` for the fake hardware and sets `HCS.UCRDY` on top of `hcs`.
pub fn uic_hardware(vmo: &zx::Vmo, hcs: u32) -> MmioRegion<VmoMemory> {
    let mut regs = map_vmo(vmo, 1024);
    regs.store32(HostControllerStatusReg::OFFSET, hcs | HCS_UIC_COMMAND_READY);
    regs
}

/// Completes the pending UIC command with `result` and `value` by setting `IS.UCCS`.
pub fn complete_uic_command(
    regs: &mut MmioRegion<VmoMemory>,
    result: u32,
    value: u32,
    extra_status: impl FnOnce(&mut InterruptStatusReg),
) {
    let mut arg2 = UicCommandArgument2Reg::from_raw(0);
    arg2.set_result_code(result);
    arg2.write(regs);
    UicCommandArgument3Reg::from_raw(value).write(regs);
    UicCommandReg::from_raw(0).write(regs);
    let mut is = InterruptStatusReg::read(regs);
    is.set_uic_command_completion_status(true);
    extra_status(&mut is);
    is.write(regs);
}

/// Returns the opcode of the UIC command currently in `UICCMD`.
pub fn uic_opcode(regs: &MmioRegion<VmoMemory>) -> u32 {
    UicCommandReg::read(regs).command_opcode()
}

mod tests {
    use super::*;

    #[fuchsia::test]
    fn test_execute_scsi_cmd() {
        let mut fake_ufs = FakeUfs::new();
        let read = DataDirection::DeviceToHost;
        let inq = fake_ufs.execute_scsi_cmd(&[ScsiOpcode::Inquiry as u8], read).unwrap();
        assert_eq!(inq.len(), 36);

        let rd = fake_ufs.execute_scsi_cmd(&[ScsiOpcode::Read10 as u8], read).unwrap();
        assert_eq!(rd.len(), 4096);

        let tur = fake_ufs.execute_scsi_cmd(&[ScsiOpcode::TestUnitReady as u8], read).unwrap();
        assert!(tur.is_empty());

        assert_eq!(fake_ufs.execute_scsi_cmd(&[], read), Err(UfsError::InvalidParameter));
        let write = DataDirection::HostToDevice;
        assert_eq!(fake_ufs.execute_scsi_cmd(&[0xFF], write), Ok(Vec::new()));
        assert_eq!(fake_ufs.execute_scsi_cmd(&[0xFF], read), Err(UfsError::CheckCondition));
    }
}
