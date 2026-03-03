// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::dma_buffer::{
    ContiguousDmaBuffer, DiscontiguousDmaBuffer, DmaBuffer as _, WriteOnlySlice,
};
use fuchsia_sync::Mutex;
use log::warn;
use sdmmc_spec::{
    CQHCI_TASK_DESCRIPTOR_LIST_DCMD_SLOT, CommandQueueTDLDirectCmdEntry, CommandQueueTDLEntry,
    CommandQueueTransferDescriptor, Direction, MMC_BLOCK_SIZE, MmcCommand, TransferBytes,
};
use std::num::NonZeroU16;
use std::ops::Range;
use std::sync::Arc;
use zx::sys::zx_paddr_t;

type ContiguousPages = Range<zx_paddr_t>;

fn round_down(value: u64, modulus: u64) -> u64 {
    value - (value % modulus)
}

/// Compresses a sequence of equally-sized page ranges into as few contiguous ranges as possible,
/// each of which is no longer than a maximum length.
///
/// The iterator will emit less than or equal to the number of input ranges in most cases, except
/// when the maximum length is smaller than the size of the input ranges.
///
/// For example, with input ranges [0..10, 10..20, 30..40], the iterator would emit:
///   - [0..20, 30..40] if the maximum length is >= 20,
///   - [0..15, 15..20, 30..40] if the maximum length is 15,
///   - [0..5, 5..10, 10..15, 15..20, 30..35, 35..40] if the maximum length is 5.
struct ContiguousPagesIterParams<'a> {
    /// A list of physical addresses.  Each address represents the start of a pinned region of
    /// `granularity` bytes.
    pub addresses: &'a [zx_paddr_t],
    /// The length, in bytes, of each pinned region.
    pub granularity: usize,
    /// The maximum length, in bytes, of contiguous ranges that the iterator will emit.  Input
    /// ranges which exceed this will be split as needed.
    pub max_contiguity: usize,
    /// The initial offset into the first pinned region.  The first output region will point to the
    /// start of the first region plus this offset, and the rest of the regions will proceed from
    /// there.
    ///
    /// This field has no specific alignment requirements, but must be less than `granularity`.
    pub offset: usize,
    /// The size, in bytes, of the desired length to emit from the iterator.
    ///
    /// This field has no specific alignment requirements, but `output_offset + output_length` must
    /// not exceed the length described by `addresses` and `granularity`.
    pub length: usize,
}

#[derive(Debug)]
struct ContiguousPagesIter<'a> {
    addresses: &'a [zx_paddr_t],
    granularity: usize,
    // The maximum size of contiguous regions to emit.
    max_contiguity: usize,
    // The index into `addresses` which we're emitting from.
    index: usize,
    // The byte offset into the region at `addresses[index]` which we've already emitted up to.
    offset_from_index: usize,
    // The total number of output bytes remaining.
    output_bytes_left: usize,
}

impl<'a> ContiguousPagesIter<'a> {
    fn new(params: ContiguousPagesIterParams<'a>) -> Self {
        assert!(params.offset < params.granularity);
        assert!(params.offset + params.length <= params.addresses.len() * params.granularity);
        assert!(params.granularity > 0);
        assert!(params.max_contiguity > 0);
        Self {
            addresses: params.addresses,
            granularity: params.granularity,
            max_contiguity: params.max_contiguity,
            index: 0,
            offset_from_index: params.offset,
            output_bytes_left: params.length,
        }
    }

    // Returns the start and end of the output chunk starting at `offset` bytes into the
    // `addresses[idx]`.
    fn range_of(&self, idx: usize, offset: usize) -> (zx_paddr_t, zx_paddr_t) {
        let start = self.addresses[idx];
        let limit = std::cmp::min(self.max_contiguity, self.output_bytes_left);
        let end = std::cmp::min(start + self.granularity, start + offset + limit);
        (start + offset, end)
    }
}

impl<'a> Iterator for ContiguousPagesIter<'a> {
    type Item = ContiguousPages;

    fn next(&mut self) -> Option<Self::Item> {
        if self.output_bytes_left == 0 {
            return None;
        }
        if self.index == self.addresses.len() {
            return None;
        }

        let (start, mut end) = self.range_of(self.index, self.offset_from_index);
        let limit = std::cmp::min(self.max_contiguity, self.output_bytes_left);
        if end - start == limit {
            // We're emitting as much as we can.
            self.offset_from_index += limit;
            self.output_bytes_left -= limit;
            if self.offset_from_index >= self.granularity {
                self.offset_from_index = 0;
                self.index += 1;
            }
            return Some(start..end);
        }
        // We emitted less than the limit, so see if we can coalesce any contiguous ranges.
        self.offset_from_index = 0;
        self.index += 1;
        while self.index < self.addresses.len() && end - start < limit {
            let (next, next_end) = self.range_of(self.index, 0);
            if end != next {
                // Not contiguous
                break;
            }
            let new_end = std::cmp::min(start + limit, next_end);
            let added = new_end - end;
            if added == 0 {
                break;
            }
            end = new_end;
            if end < next_end {
                self.offset_from_index = added;
                break;
            } else {
                self.offset_from_index = 0;
                self.index += 1;
            }
        }

        self.output_bytes_left -= end - start;
        Some(start..end)
    }
}

struct TransferManagerInner {
    tdl_allocation_bitmap: u32,
}

impl TransferManagerInner {
    fn new() -> Self {
        Self { tdl_allocation_bitmap: 0 }
    }
}

/// An RAII guard for a transfer slot in the TDL.
pub struct TransferSlot {
    manager: Arc<TransferManager>,
    tdl_slot: u8,
}

impl Drop for TransferSlot {
    fn drop(&mut self) {
        let mut inner = self.manager.inner.lock();
        inner.tdl_allocation_bitmap &= !(1u32 << self.tdl_slot);
    }
}

/// An RAII guard for the DCMD slot in the TDL.
pub struct DcmdSlot<'a> {
    manager: &'a TransferManager,
}

impl Drop for DcmdSlot<'_> {
    fn drop(&mut self) {
        let mut inner = self.manager.inner.lock();
        inner.tdl_allocation_bitmap &= !(1u32 << CQHCI_TASK_DESCRIPTOR_LIST_DCMD_SLOT);
    }
}

/// TransferManager is responsible for keeping track of in-flight CQHCI requests.  It maintains the
/// state of the Task Descriptor List and other transfer descriptors, and interacts with the CQE via
/// registers to submit tasks.
pub struct TransferManager {
    tdl_buffer: ContiguousDmaBuffer,
    extra_descriptors_buffer: DiscontiguousDmaBuffer,
    bti: zx::Bti,
    max_transfer_blocks: u32,
    inner: Mutex<TransferManagerInner>,
}

impl std::fmt::Debug for TransferManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransferManager").finish_non_exhaustive()
    }
}

impl TransferManager {
    /// Calculates how large of a buffer needs to be allocated for transfer descriptors, and the
    /// maximum transfer size based on that.
    ///
    /// Returns (descriptor_buffer_size, max_transfer_blocks).
    pub fn extra_descriptors_dimensions() -> (usize, u32) {
        let page_size = zx::system_get_page_size() as usize;
        // With worst-case fragmentation, each transfer descriptor will point to just a single page.
        // We reserve one page for extra descriptors for each slot, so the maximum transfer size is
        // based on the number of single-page transfer descriptors which can fit into a page.
        let buffer_size = CQHCI_TASK_DESCRIPTOR_LIST_DCMD_SLOT as usize * page_size;
        let max_descriptors_per_page =
            page_size / std::mem::size_of::<CommandQueueTransferDescriptor>();
        let max_transfer_size = max_descriptors_per_page * page_size;
        let max_transfer_blocks = (max_transfer_size / MMC_BLOCK_SIZE as usize) as u32;
        (buffer_size, max_transfer_blocks)
    }

    pub fn new(
        tdl_buffer: ContiguousDmaBuffer,
        extra_descriptors_buffer: DiscontiguousDmaBuffer,
        bti: zx::Bti,
    ) -> Self {
        let (extra_descriptor_size, max_transfer_blocks) = Self::extra_descriptors_dimensions();
        assert!(extra_descriptors_buffer.size() >= extra_descriptor_size);
        Self {
            tdl_buffer,
            extra_descriptors_buffer,
            bti,
            max_transfer_blocks,
            inner: Mutex::new(TransferManagerInner::new()),
        }
    }

    /// Consumes the TransferManager and unpins its pinned DMA buffers.  This must be called
    /// explicitly (rather than simply dropping TransferManager).
    ///
    /// # Safety
    ///
    /// This MUST NOT be called while CQE is enabled, as this will unpin memory that might be
    /// accessed by the CQE.
    pub unsafe fn unpin_buffers(mut self) {
        if let Err(status) = unsafe { self.tdl_buffer.unpin() } {
            warn!(status:?; "Failed to unpin TDL");
        }
        if let Err(status) = unsafe { self.extra_descriptors_buffer.unpin() } {
            warn!(status:?; "Failed to unpin descriptors");
        }
    }

    /// Acquires the DCMD slot in the TDL, if available.
    pub fn acquire_dcmd_slot(&self) -> Option<DcmdSlot<'_>> {
        let mut inner = self.inner.lock();
        let dcmd_bit = 1u32 << CQHCI_TASK_DESCRIPTOR_LIST_DCMD_SLOT;
        if (inner.tdl_allocation_bitmap & dcmd_bit) != 0 {
            None
        } else {
            inner.tdl_allocation_bitmap |= dcmd_bit;
            Some(DcmdSlot { manager: self })
        }
    }

    /// Initializes the TDL entry for a DCMD.
    pub fn prepare_dcmd(
        &self,
        _slot: &DcmdSlot<'_>,
        command: MmcCommand,
        command_arg: u32,
    ) -> Result<(), zx::Status> {
        let tdl_entry = CommandQueueTDLDirectCmdEntry::new(command, command_arg);
        self.commit(CQHCI_TASK_DESCRIPTOR_LIST_DCMD_SLOT, tdl_entry)?;
        Ok(())
    }

    /// Acquires a slot in the TDL for a transfer, if available.
    pub fn acquire_transfer_slot(self: &Arc<Self>) -> Option<TransferSlot> {
        let mut inner = self.inner.lock();
        let tdl_slot = (inner.tdl_allocation_bitmap | (1 << CQHCI_TASK_DESCRIPTOR_LIST_DCMD_SLOT))
            .trailing_ones() as u8;
        if tdl_slot < CQHCI_TASK_DESCRIPTOR_LIST_DCMD_SLOT {
            inner.tdl_allocation_bitmap |= 1u32 << tdl_slot;
            Some(TransferSlot { manager: self.clone(), tdl_slot })
        } else {
            None
        }
    }

    /// Pins the specified region in the VMO and prepares transfer descriptors pointing to the
    /// range.
    ///
    /// 1. Pins the VMO's pages, determining the number of contiguous ranges (each of which needs
    ///    one transfer descriptor).
    /// 2: Prepare the transfer descriptors:
    ///    a) If there is a single contiguous range, uses the inline Transfer Descriptor in the TDL
    ///       to point directly to the region.
    ///    b) If there are multiple ranges, allocate and prepare a contiguous block of descriptors
    ///       outside the TDL, and points the TDL entry to this scatter/gather range.
    ///
    /// The returned `Transfer` MUST be explicitly completed by calling [`Transfer::complete`].
    pub fn prepare_transfer(
        self: &Arc<Self>,
        slot: TransferSlot,
        vmo: Arc<zx::Vmo>,
        vmo_offset: u64,
        block_offset: u64,
        block_count: u32,
        data_direction: Direction,
    ) -> Result<Transfer, zx::Status> {
        // NB: VMO offset does *not* need to be page-aligned.  The CQE is capable of DMAing to
        // arbitrarily aligned addresses.  This is consistent with the legacy SDHCI stack, and
        // generally with other block driver implementations (see blkdev_test_fifo_basic in
        // blktest.cc).
        if !vmo_offset.is_multiple_of(MMC_BLOCK_SIZE)
            || block_count > self.max_transfer_blocks
            || block_count > u16::MAX as u32
        {
            return Err(zx::Status::INVALID_ARGS);
        }
        let block_count =
            NonZeroU16::try_from(block_count as u16).map_err(|_| zx::Status::INVALID_ARGS)?;
        let length = block_count.get() as u64 * MMC_BLOCK_SIZE;
        let mut transfer = Transfer {
            slot,
            offset: block_offset * MMC_BLOCK_SIZE,
            length,
            pmt: None,
            buffers: TransferBuffers::None,
            data_direction,
        };
        let page_size = zx::system_get_page_size() as u64;
        // We have to pin a region that is aligned to `contiguity`.
        let contiguity = self.bti.info()?.minimum_contiguity;
        let aligned_vmo_offset = round_down(vmo_offset, contiguity);
        let end = (vmo_offset + length).next_multiple_of(page_size);
        let aligned_length = end - aligned_vmo_offset;

        let mut paddrs = vec![0; aligned_length.div_ceil(contiguity) as usize];
        let options =
            zx::BtiOptions::PERM_READ | zx::BtiOptions::PERM_WRITE | zx::BtiOptions::COMPRESS;
        let pmt = self.bti.pin(
            options,
            vmo.as_ref(),
            aligned_vmo_offset,
            aligned_length,
            &mut paddrs[..],
        )?;
        let unpin_guard = scopeguard::guard(pmt, move |pmt: zx::Pmt| {
            // SAFETY: We only call this branch upon failure, so the transfer won't be submitted to
            // hardware yet.
            let _ = unsafe { pmt.unpin() };
        });
        match data_direction {
            Direction::Read => {
                // We must pessimistically assume that there are pending writes to the VMO which
                // need to be flushed before we start doing the DMA.  If we didn't flush the writes,
                // they might get written out after we start to DMA, in which case they could stomp
                // the read bytes.
                // TODO(https://fxbug.dev/458084387): Consider eliding this when possible.
                vmo.op_range(zx::VmoOp::CACHE_CLEAN_INVALIDATE, vmo_offset, length)?;
            }
            Direction::Write => {
                // Ensure any cached writes are flushed to main memory.
                vmo.op_range(zx::VmoOp::CACHE_CLEAN, vmo_offset, length)?;
            }
        };
        let contig_ranges = ContiguousPagesIter::new(ContiguousPagesIterParams {
            addresses: &paddrs[..],
            granularity: contiguity as usize,
            max_contiguity: TransferBytes::MAX_BYTES,
            offset: (vmo_offset - aligned_vmo_offset) as usize,
            length: length as usize,
        });
        self.commit_transfer_task(&mut transfer, block_offset, block_count, contig_ranges)?;
        transfer.pmt = Some(scopeguard::ScopeGuard::into_inner(unpin_guard));
        Ok(transfer)
    }

    fn commit_transfer_task(
        &self,
        transfer: &mut Transfer,
        block_offset: u64,
        block_count: NonZeroU16,
        contig_regions: ContiguousPagesIter<'_>,
    ) -> Result<(), zx::Status> {
        let offset = transfer.tdl_slot() as usize * zx::system_get_page_size() as usize;
        let max_descriptors = zx::system_get_page_size() as usize
            / std::mem::size_of::<CommandQueueTransferDescriptor>();
        let mut num_descriptors = 0;
        let mut contig_regions = contig_regions.peekable();
        let first_region = contig_regions.next().unwrap();
        let tdl_entry = if contig_regions.peek().is_none() {
            transfer.buffers = TransferBuffers::Single(first_region.start);
            CommandQueueTDLEntry::single_buffer(
                transfer.data_direction,
                block_offset,
                block_count,
                first_region.start as u64,
            )
            .unwrap() // Unwrap OK, we already checked `block_count` is valid in the caller
        } else {
            self.extra_descriptors_buffer.write(
                offset,
                max_descriptors,
                |mut slice: WriteOnlySlice<'_, CommandQueueTransferDescriptor>| {
                    let mut i = 0;
                    let mut region = first_region;
                    while let Some(next) = contig_regions.next() {
                        debug_assert!(i < max_descriptors);
                        // Unwrap is OK since ContiguousPagesIter ensured that the ranges are not
                        // too big.
                        let length = TransferBytes::try_from(region.end - region.start).unwrap();
                        slice.set(
                            i,
                            CommandQueueTransferDescriptor::transfer(
                                region.start as u64,
                                length,
                                false,
                            ),
                        );
                        i += 1;
                        region = next;
                    }
                    // Unwrap is OK since ContiguousPagesIter ensured that the ranges are not too
                    // big.
                    let length = TransferBytes::try_from(region.end - region.start).unwrap();
                    slice.set(
                        i,
                        CommandQueueTransferDescriptor::transfer(region.start as u64, length, true),
                    );
                    num_descriptors = i + 1;
                },
            )?;
            let phys_address = self.extra_descriptors_buffer.phys_address_for(offset);
            transfer.buffers = TransferBuffers::ScatterGatherList(phys_address, num_descriptors);
            CommandQueueTDLEntry::scatter_gather_buffers(
                transfer.data_direction,
                block_offset,
                block_count,
                phys_address as u64,
            )
        };
        self.commit(transfer.tdl_slot(), tdl_entry)
    }

    fn commit<
        T: Copy + std::fmt::Debug + zerocopy::FromBytes + zerocopy::IntoBytes + zerocopy::Immutable,
    >(
        &self,
        tdl_slot: u8,
        tdl_entry: T,
    ) -> Result<(), zx::Status> {
        self.tdl_buffer.write(
            tdl_slot as usize * std::mem::size_of::<T>(),
            1,
            |mut slice: WriteOnlySlice<'_, T>| {
                slice.set(0, tdl_entry);
            },
        )
    }
}

#[derive(Debug)]
enum TransferBuffers {
    None,
    // The transfer descriptor in the TDL points directly to the buffer.
    Single(zx_paddr_t),
    // The transfer descriptor in the TDL points to a scatter/gather list
    ScatterGatherList(zx_paddr_t, usize),
}

pub struct Transfer {
    slot: TransferSlot,
    offset: u64,
    length: u64,
    pmt: Option<zx::Pmt>,
    buffers: TransferBuffers,
    data_direction: Direction,
}

impl Transfer {
    /// Completes the Transfer, unpinning memory pointed to by the Transfer.
    ///
    /// This MUST be called after the Transfer completes (successfully or not).
    pub fn complete(mut self) {
        if let Some(pmt) = self.pmt.take() {
            // SAFETY:  The transfer is complete, so the hardware will no longer access the pinned
            // memory.
            if let Err(status) = unsafe { pmt.unpin() } {
                warn!(status:?; "Failed to unpin PMT");
            }
        }
    }

    pub fn tdl_slot(&self) -> u8 {
        self.slot.tdl_slot
    }
}

impl std::fmt::Debug for Transfer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transfer")
            .field("tdl_slot", &self.tdl_slot())
            .field("buffers", &self.buffers)
            .field("data_direction", &self.data_direction)
            .field("offset", &self.offset)
            .field("length", &self.length)
            .finish_non_exhaustive()
    }
}

impl Drop for Transfer {
    fn drop(&mut self) {
        assert!(self.pmt.is_none(), "Transfer::complete was not called");
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU16;
    use std::sync::Arc;

    use super::*;
    use fake_bti::FakeBti;
    use sdmmc_spec::{CQHCI_TASK_DESCRIPTOR_LIST_NUM_SLOTS, CQHCI_TASK_DESCRIPTOR_LIST_SIZE};
    use zx::HandleBased as _;

    const TDL_BASE: zx_paddr_t = 2 * 1024 * 1024;
    const EXTRA_DESCRIPTORS_BASE: zx_paddr_t = 3 * 1024 * 1024;

    fn setup() -> (Arc<TransferManager>, FakeBti) {
        let bti = FakeBti::create().expect("Failed to create fake bti");

        let page_size = zx::system_get_page_size() as usize;
        let mut paddrs = vec![TDL_BASE];
        for i in 0..CQHCI_TASK_DESCRIPTOR_LIST_DCMD_SLOT as usize {
            paddrs.push(EXTRA_DESCRIPTORS_BASE + i * page_size);
        }
        bti.set_paddrs(&paddrs[..]);
        let vmar = fuchsia_runtime::vmar_root_self()
            .duplicate_handle(zx::Rights::SAME_RIGHTS)
            .expect("duplicate_handle failed");
        let tdl_buffer = ContiguousDmaBuffer::new(vmar, &*bti, CQHCI_TASK_DESCRIPTOR_LIST_SIZE)
            .expect("Failed to create TDL DMA buffer");
        assert_eq!(tdl_buffer.phys_address(), TDL_BASE);

        let extra_desriptors_size =
            zx::system_get_page_size() as usize * CQHCI_TASK_DESCRIPTOR_LIST_DCMD_SLOT as usize;
        let vmar = fuchsia_runtime::vmar_root_self()
            .duplicate_handle(zx::Rights::SAME_RIGHTS)
            .expect("duplicate_handle failed");
        let extra_descriptors_buffer =
            DiscontiguousDmaBuffer::new(vmar, &*bti, extra_desriptors_size)
                .expect("Failed to create descriptor DMA buffer");
        assert_eq!(extra_descriptors_buffer.phys_address_for(0), EXTRA_DESCRIPTORS_BASE);
        assert_eq!(extra_descriptors_buffer.phys_address_for(4096), EXTRA_DESCRIPTORS_BASE + 4096);

        bti.set_paddrs(&[]);
        (
            Arc::new(TransferManager::new(
                tdl_buffer,
                extra_descriptors_buffer,
                bti.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            )),
            bti,
        )
    }

    fn validate_tdl_entry(
        transfer_manager: &TransferManager,
        slot: u8,
        expected: CommandQueueTDLEntry,
    ) {
        let buf: Box<[CommandQueueTDLEntry]> = transfer_manager
            .tdl_buffer
            .vmo()
            .read_to_vec::<CommandQueueTDLEntry>(0, CQHCI_TASK_DESCRIPTOR_LIST_NUM_SLOTS as u64)
            .unwrap()
            .into_boxed_slice();
        assert_eq!(buf[slot as usize], expected);
    }

    fn validate_extra_transfer_descriptors(
        transfer_manager: &TransferManager,
        start_index: usize,
        expected: &[CommandQueueTransferDescriptor],
    ) {
        let buf: Box<[CommandQueueTransferDescriptor]> = transfer_manager
            .extra_descriptors_buffer
            .vmo()
            .read_to_vec::<CommandQueueTransferDescriptor>(
                (start_index * std::mem::size_of::<CommandQueueTransferDescriptor>()) as u64,
                expected.len() as u64,
            )
            .unwrap()
            .into_boxed_slice();
        assert_eq!(&buf[..], expected);
    }

    #[fuchsia::test]
    fn tdl_slots_freed_on_drop() {
        let (manager, _) = setup();
        let vmo = Arc::new(zx::Vmo::create(16384).unwrap());
        let mut off = 0;
        let mut transfers = vec![];
        let mut last_tdl_slot = None;
        for _ in 0..CQHCI_TASK_DESCRIPTOR_LIST_DCMD_SLOT {
            transfers.push(
                manager
                    .prepare_transfer(
                        manager.acquire_transfer_slot().unwrap(),
                        vmo.clone(),
                        off * 512,
                        off,
                        1,
                        Direction::Read,
                    )
                    .expect("prepare_transfer failed"),
            );
            let transfer = transfers.last().unwrap();
            assert!(transfer.pmt.is_some());
            if let Some(last) = last_tdl_slot {
                assert_ne!(transfer.tdl_slot(), last);
            }
            last_tdl_slot = Some(transfer.tdl_slot());
            off += 1;
        }
        // Should fail because all TDL slots are in use.
        assert!(manager.acquire_transfer_slot().is_none());

        // Drop one, ensure we can make progress
        transfers.pop().unwrap().complete();

        // Should succeed now
        let transfer = manager
            .prepare_transfer(
                manager.acquire_transfer_slot().unwrap(),
                vmo.clone(),
                0,
                0,
                1,
                Direction::Read,
            )
            .expect("prepare_transfer failed");
        transfer.complete();

        for transfer in transfers {
            transfer.complete();
        }
        unsafe { Arc::try_unwrap(manager).unwrap().unpin_buffers() };
    }

    #[fuchsia::test]
    fn single_buffer_transfer() {
        let (manager, fake_bti) = setup();
        fake_bti.set_paddrs(&[4096, 16384]);
        let vmo = Arc::new(zx::Vmo::create(4096).unwrap());
        let transfer = manager
            .prepare_transfer(
                manager.acquire_transfer_slot().unwrap(),
                vmo.clone(),
                0,
                0,
                1,
                Direction::Read,
            )
            .expect("prepare_transfer failed");
        assert!(transfer.pmt.is_some());
        validate_tdl_entry(
            &manager,
            0,
            CommandQueueTDLEntry::single_buffer(
                Direction::Read,
                0,
                NonZeroU16::try_from(1).unwrap(),
                4096,
            )
            .unwrap(),
        );
        let TransferBuffers::Single(ref paddr) = transfer.buffers else {
            panic!("No single paddr");
        };
        assert_eq!(*paddr, 4096);
        assert_eq!(*paddr, 4096);
        transfer.complete();
        unsafe { Arc::try_unwrap(manager).unwrap().unpin_buffers() };
    }

    #[fuchsia::test]
    fn multi_buffer_transfer() {
        let (manager, fake_bti) = setup();
        let vmo = Arc::new(zx::Vmo::create(64 * 512).unwrap());
        fake_bti.set_paddrs(&[4096, 16384, 32768]);
        let transfer = manager
            .prepare_transfer(
                manager.acquire_transfer_slot().unwrap(),
                vmo.clone(),
                512,
                10,
                16,
                Direction::Read,
            )
            .expect("prepare_transfer failed");
        assert!(transfer.pmt.is_some());
        validate_extra_transfer_descriptors(
            &manager,
            0,
            &[
                CommandQueueTransferDescriptor::transfer(
                    4096 + 512,
                    TransferBytes::try_from(4096 - 512).unwrap(),
                    false,
                ),
                CommandQueueTransferDescriptor::transfer(
                    16384,
                    TransferBytes::try_from(4096).unwrap(),
                    false,
                ),
                CommandQueueTransferDescriptor::transfer(
                    32768,
                    TransferBytes::try_from(512).unwrap(),
                    true,
                ),
            ],
        );
        validate_tdl_entry(
            &manager,
            0,
            CommandQueueTDLEntry::scatter_gather_buffers(
                Direction::Read,
                10,
                NonZeroU16::try_from(16).unwrap(),
                EXTRA_DESCRIPTORS_BASE as u64,
            ),
        );
        let TransferBuffers::ScatterGatherList(addr, len) = transfer.buffers else {
            panic!("No s/g list");
        };
        assert_eq!(addr, EXTRA_DESCRIPTORS_BASE);
        assert_eq!(len, 3);
        {
            // Ensure that another transfer will have a non-overlapping region in extra_descriptors
            fake_bti.set_paddrs(&[8192, 20480, 36864]);
            let transfer2 = manager
                .prepare_transfer(
                    manager.acquire_transfer_slot().unwrap(),
                    vmo.clone(),
                    512,
                    40,
                    16,
                    Direction::Write,
                )
                .expect("prepare_transfer failed");
            assert!(transfer.pmt.is_some());
            validate_tdl_entry(
                &manager,
                1,
                CommandQueueTDLEntry::scatter_gather_buffers(
                    Direction::Write,
                    40,
                    NonZeroU16::try_from(16).unwrap(),
                    EXTRA_DESCRIPTORS_BASE as u64 + zx::system_get_page_size() as u64,
                ),
            );
            transfer2.complete();
        }
        transfer.complete();
        unsafe { Arc::try_unwrap(manager).unwrap().unpin_buffers() };
    }

    #[fuchsia::test]
    fn max_size_transfer() {
        let (manager, fake_bti) = setup();
        let mut paddrs = vec![];
        // Set up paddrs so no regions are contiguous, so the max # of transfer descriptors are
        // used.
        let page_size = zx::system_get_page_size() as usize;
        let num_descriptors = page_size / std::mem::size_of::<CommandQueueTransferDescriptor>();
        for i in 0..num_descriptors {
            paddrs.push(0x20000 + (2 * i * page_size));
        }
        fake_bti.set_paddrs(&paddrs[..]);
        let vmo = Arc::new(zx::Vmo::create((num_descriptors * page_size) as u64).unwrap());
        let block_count = ((page_size * num_descriptors) / MMC_BLOCK_SIZE as usize) as u32;
        let transfer = manager
            .prepare_transfer(
                manager.acquire_transfer_slot().unwrap(),
                vmo.clone(),
                0,
                0,
                block_count,
                Direction::Read,
            )
            .expect("prepare_transfer failed");
        assert!(transfer.pmt.is_some());
        validate_tdl_entry(
            &manager,
            0,
            CommandQueueTDLEntry::scatter_gather_buffers(
                Direction::Read,
                0,
                NonZeroU16::try_from(block_count as u16).unwrap(),
                EXTRA_DESCRIPTORS_BASE as u64,
            ),
        );
        let mut expected_descriptors = vec![];
        for i in 0..num_descriptors {
            expected_descriptors.push(CommandQueueTransferDescriptor::transfer(
                paddrs[i] as u64,
                TransferBytes::try_from(page_size).unwrap(),
                i == num_descriptors - 1,
            ))
        }
        validate_extra_transfer_descriptors(&manager, 0, &expected_descriptors[..]);
        let TransferBuffers::ScatterGatherList(addr, len) = transfer.buffers else {
            panic!("No s/g list");
        };
        assert_eq!(addr, EXTRA_DESCRIPTORS_BASE);
        assert_eq!(len, num_descriptors);
        transfer.complete();
        unsafe { Arc::try_unwrap(manager).unwrap().unpin_buffers() };
    }

    #[fuchsia::test]
    fn multi_buffer_transfer_with_compressed_ranges() {
        let (manager, fake_bti) = setup();
        fake_bti.set_paddrs(&[4096, 8192, 16384, 20480, 65536]);
        let vmo = Arc::new(zx::Vmo::create(40 * 512).unwrap());
        let transfer = manager
            .prepare_transfer(
                manager.acquire_transfer_slot().unwrap(),
                vmo.clone(),
                0,
                100,
                40,
                Direction::Read,
            )
            .expect("prepare_transfer failed");
        assert!(transfer.pmt.is_some());
        validate_tdl_entry(
            &manager,
            0,
            CommandQueueTDLEntry::scatter_gather_buffers(
                Direction::Read,
                100,
                NonZeroU16::try_from(40).unwrap(),
                EXTRA_DESCRIPTORS_BASE as u64,
            ),
        );
        validate_extra_transfer_descriptors(
            &manager,
            0,
            &[
                CommandQueueTransferDescriptor::transfer(
                    4096,
                    TransferBytes::try_from(8192).unwrap(),
                    false,
                ),
                CommandQueueTransferDescriptor::transfer(
                    16384,
                    TransferBytes::try_from(8192).unwrap(),
                    false,
                ),
                CommandQueueTransferDescriptor::transfer(
                    65536,
                    TransferBytes::try_from(4096).unwrap(),
                    true,
                ),
            ],
        );
        let TransferBuffers::ScatterGatherList(addr, len) = transfer.buffers else {
            panic!("No s/g list");
        };
        assert_eq!(addr, EXTRA_DESCRIPTORS_BASE);
        assert_eq!(len, 3);
        transfer.complete();
        unsafe { Arc::try_unwrap(manager).unwrap().unpin_buffers() };
    }

    #[fuchsia::test]
    fn dcmd() {
        let (manager, _) = setup();
        {
            let slot = manager.acquire_dcmd_slot().unwrap();
            assert!(manager.acquire_dcmd_slot().is_none());
            manager.prepare_dcmd(&slot, MmcCommand::SendStatus, 0).expect("prepare_dcmd failed");
        }

        {
            let slot = manager.acquire_dcmd_slot().unwrap();
            manager.prepare_dcmd(&slot, MmcCommand::SendStatus, 0).expect("prepare_dcmd failed");
        }

        unsafe { Arc::try_unwrap(manager).unwrap().unpin_buffers() };
    }

    #[fuchsia::test]
    async fn overlapping_transfers() {
        let (manager, fake_bti) = setup();
        let vmo = Arc::new(zx::Vmo::create(65536).unwrap());

        fake_bti.set_paddrs(&[4096, 16384]);
        let transfer1 = manager
            .prepare_transfer(
                manager.acquire_transfer_slot().unwrap(),
                vmo.clone(),
                0,
                0,
                16,
                Direction::Read,
            )
            .expect("prepare_transfer 1 failed");

        fake_bti.set_paddrs(&[16384, 8192]);
        let transfer2 = manager
            .prepare_transfer(
                manager.acquire_transfer_slot().unwrap(),
                vmo.clone(),
                4096,
                16,
                16,
                Direction::Read,
            )
            .expect("prepare_transfer 2 failed");

        let page_size = zx::system_get_page_size() as usize;
        let descriptor_size = std::mem::size_of::<CommandQueueTransferDescriptor>();
        let descriptors_per_slot = page_size / descriptor_size;

        validate_extra_transfer_descriptors(
            &manager,
            transfer1.tdl_slot() as usize * descriptors_per_slot,
            &[
                CommandQueueTransferDescriptor::transfer(
                    4096,
                    TransferBytes::try_from(4096).unwrap(),
                    false,
                ),
                CommandQueueTransferDescriptor::transfer(
                    16384,
                    TransferBytes::try_from(4096).unwrap(),
                    true,
                ),
            ],
        );

        validate_extra_transfer_descriptors(
            &manager,
            transfer2.tdl_slot() as usize * descriptors_per_slot,
            &[
                CommandQueueTransferDescriptor::transfer(
                    16384,
                    TransferBytes::try_from(4096).unwrap(),
                    false,
                ),
                CommandQueueTransferDescriptor::transfer(
                    8192,
                    TransferBytes::try_from(4096).unwrap(),
                    true,
                ),
            ],
        );

        transfer1.complete();
        transfer2.complete();
        unsafe {
            Arc::try_unwrap(manager).expect("TransferManager still referenced").unpin_buffers()
        };
    }

    #[test]
    fn contiguous_pages_iter_empty() {
        let addresses = [];
        let iter = ContiguousPagesIter::new(ContiguousPagesIterParams {
            addresses: &addresses,
            granularity: 4096,
            max_contiguity: 8192,
            offset: 0,
            length: 0,
        });
        assert_eq!(iter.collect::<Vec<_>>(), vec![]);
    }

    #[test]
    fn contiguous_pages_iter_single_small() {
        let addresses = [0x1000];
        let iter = ContiguousPagesIter::new(ContiguousPagesIterParams {
            addresses: &addresses,
            granularity: 4096,
            max_contiguity: 8192,
            offset: 0,
            length: 4096,
        });
        assert_eq!(iter.collect::<Vec<_>>(), vec![0x1000..0x2000]);
    }

    #[test]
    fn contiguous_pages_iter_multiple_contiguous() {
        let addresses = [0x1000, 0x2000, 0x3000];
        let iter = ContiguousPagesIter::new(ContiguousPagesIterParams {
            addresses: &addresses,
            granularity: 4096,
            max_contiguity: 16384,
            offset: 0,
            length: 3 * 4096,
        });
        assert_eq!(iter.collect::<Vec<_>>(), vec![0x1000..0x4000]);
    }

    #[test]
    fn contiguous_pages_iter_multiple_non_contiguous() {
        let addresses = [0x1000, 0x3000, 0x5000];
        let iter = ContiguousPagesIter::new(ContiguousPagesIterParams {
            addresses: &addresses,
            granularity: 4096,
            max_contiguity: 16384,
            offset: 0,
            length: 3 * 4096,
        });
        assert_eq!(iter.collect::<Vec<_>>(), vec![0x1000..0x2000, 0x3000..0x4000, 0x5000..0x6000]);
    }

    #[test]
    fn contiguous_pages_iter_partial_last_page() {
        let addresses = [0x1000, 0x2000];
        let iter = ContiguousPagesIter::new(ContiguousPagesIterParams {
            addresses: &addresses,
            granularity: 4096,
            max_contiguity: 8192,
            offset: 0,
            length: 4096 + 1024,
        });
        assert_eq!(iter.collect::<Vec<_>>(), vec![0x1000..0x2400]);
    }

    #[test]
    fn contiguous_pages_iter_split_max_length() {
        let addresses = [0x1000, 0x2000, 0x3000, 0x4000];
        let iter = ContiguousPagesIter::new(ContiguousPagesIterParams {
            addresses: &addresses,
            granularity: 4096,
            max_contiguity: 8192,
            offset: 0,
            length: 4 * 4096,
        });
        assert_eq!(iter.collect::<Vec<_>>(), vec![0x1000..0x3000, 0x3000..0x5000]);
    }

    #[test]
    fn contiguous_pages_iter_split_single_page() {
        let addresses = [0x0, 0x1000];
        let iter = ContiguousPagesIter::new(ContiguousPagesIterParams {
            addresses: &addresses,
            granularity: 4096,
            max_contiguity: 2048,
            offset: 0,
            length: 2 * 4096,
        });
        assert_eq!(
            iter.collect::<Vec<_>>(),
            vec![0x0..0x800, 0x800..0x1000, 0x1000..0x1800, 0x1800..0x2000]
        );
    }

    #[test]
    fn contiguous_pages_iter_split_across_pages() {
        let addresses = [0, 10];
        let iter = ContiguousPagesIter::new(ContiguousPagesIterParams {
            addresses: &addresses,
            granularity: 10,
            max_contiguity: 8,
            offset: 0,
            length: 20,
        });
        assert_eq!(iter.collect::<Vec<_>>(), vec![0..8, 8..16, 16..20]);
    }

    #[test]
    fn contiguous_pages_iter_start_from_offset() {
        let addresses = [0, 10];
        let iter = ContiguousPagesIter::new(ContiguousPagesIterParams {
            addresses: &addresses,
            granularity: 10,
            max_contiguity: 8,
            offset: 1,
            length: 14,
        });
        assert_eq!(iter.collect::<Vec<_>>(), vec![1..9, 9..15]);
    }

    #[test]
    fn contiguous_pages_iter_exact_limit_match() {
        let addresses = [0x1000, 0x2000];
        let iter = ContiguousPagesIter::new(ContiguousPagesIterParams {
            addresses: &addresses,
            granularity: 4096,
            max_contiguity: 8192,
            offset: 0,
            length: 8192,
        });
        assert_eq!(iter.collect::<Vec<_>>(), vec![0x1000..0x3000]);
    }

    #[test]
    fn contiguous_pages_iter_unaligned_offset_and_length() {
        let addresses = [0x0, 0x1000];
        let iter = ContiguousPagesIter::new(ContiguousPagesIterParams {
            addresses: &addresses,
            granularity: 4096,
            max_contiguity: 8192,
            offset: 10,
            length: 4090,
        });
        assert_eq!(iter.collect::<Vec<_>>(), vec![10..4100]);
    }
}
