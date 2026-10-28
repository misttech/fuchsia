// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use dma_buffer::{ContiguousBuffer, PagedBuffer, PhysIter};
use log::warn;
use sdmmc_spec::{
    CQHCI_TASK_DESCRIPTOR_LIST_DCMD_SLOT, CommandQueueTDLDirectCmdEntry, CommandQueueTDLEntry,
    CommandQueueTransferDescriptor, CryptoParams, Direction, MMC_BLOCK_SIZE, MmcCommand,
    TransferBytes,
};
use std::num::NonZeroU16;
use std::sync::Arc;
use storage_device::buffer::OwnedBuffer;
use zx::sys::zx_paddr_t;

fn round_down(value: u64, modulus: u64) -> u64 {
    value - (value % modulus)
}

/// TransferManager is responsible for keeping track of in-flight CQHCI requests.  It maintains the
/// state of the Task Descriptor List and other transfer descriptors, and interacts with the CQE via
/// registers to submit tasks.
pub struct TransferManager {
    tdl_buffer: ContiguousBuffer,
    extra_descriptors_buffer: PagedBuffer,
    bti: zx::Bti,
    minimum_contiguity: u64,
    max_transfer_blocks: u32,
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
        tdl_buffer: ContiguousBuffer,
        extra_descriptors_buffer: PagedBuffer,
        bti: zx::Bti,
    ) -> Self {
        let (extra_descriptor_size, max_transfer_blocks) = Self::extra_descriptors_dimensions();
        assert!(extra_descriptors_buffer.size() >= extra_descriptor_size);
        let minimum_contiguity = bti.info().expect("Failed to get BTI info").minimum_contiguity;
        Self { tdl_buffer, extra_descriptors_buffer, bti, minimum_contiguity, max_transfer_blocks }
    }

    pub fn minimum_contiguity(&self) -> u64 {
        self.minimum_contiguity
    }

    pub fn bti(&self) -> &zx::Bti {
        &self.bti
    }

    /// Consumes the TransferManager and unpins its pinned DMA buffers.  This must be called
    /// explicitly (rather than simply dropping TransferManager).
    ///
    /// # Safety
    ///
    /// This MUST NOT be called while CQE is enabled, as this will unpin memory that might be
    /// accessed by the CQE.
    pub unsafe fn unpin_buffers(self) {
        if let Err(status) = unsafe { self.tdl_buffer.unpin() } {
            warn!(status:?; "Failed to unpin TDL");
        }
        if let Err(status) = unsafe { self.extra_descriptors_buffer.unpin() } {
            warn!(status:?; "Failed to unpin descriptors");
        }
    }

    pub fn tdl_address(&self) -> u64 {
        self.tdl_buffer.phys() as u64
    }

    /// Initializes the TDL entry for a DCMD.
    pub fn prepare_dcmd(&self, command: MmcCommand, command_arg: u32) -> Result<(), zx::Status> {
        let tdl_entry = CommandQueueTDLDirectCmdEntry::new(command, command_arg);
        self.commit(CQHCI_TASK_DESCRIPTOR_LIST_DCMD_SLOT, tdl_entry)?;
        Ok(())
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
    pub fn prepare_transfer(
        self: &Arc<Self>,
        slot: u8,
        vmo: Arc<zx::Vmo>,
        vmo_offset: u64,
        block_offset: u32,
        block_count: u32,
        data_direction: Direction,
        transfer_options: TransferOptions,
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
            tdl_slot: slot,
            vmo: vmo.clone(),
            vmo_offset,
            offset: block_offset as u64 * MMC_BLOCK_SIZE,
            length,
            pmt: None,
            buffers: TransferBuffers::None,
            data_direction,
        };
        let page_size = zx::system_get_page_size() as u64;
        // `zx_bti_pin` requires the pinned region's offset and length to be page-aligned.
        let contiguity = self.minimum_contiguity;
        let aligned_vmo_offset = round_down(vmo_offset, page_size);
        let end = (vmo_offset + length).next_multiple_of(page_size);
        let aligned_length = end - aligned_vmo_offset;

        let mut paddrs = vec![0; aligned_length.div_ceil(contiguity) as usize];
        let options =
            zx::BtiOptions::PERM_READ | zx::BtiOptions::PERM_WRITE | zx::BtiOptions::COMPRESS;
        let pmt = self
            .bti
            .pin(options, vmo.as_ref(), aligned_vmo_offset, aligned_length, &mut paddrs[..])
            .map_err(|error| {
                warn!(error:?; "Pin failed");
                zx::Status::IO
            })?;
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
        // When `contiguity` is `u64::MAX` (not a power of two), `paddrs.len() == 1` covers the
        // entire pinned range; pass `page_size` to satisfy `PhysIter`'s power-of-two requirement.
        let phys_iter_chunk_size =
            if contiguity.is_power_of_two() { contiguity as usize } else { page_size as usize };
        let contig_ranges = PhysIter::new_with_options(
            &paddrs[..],
            phys_iter_chunk_size,
            (vmo_offset - aligned_vmo_offset) as usize,
            length as usize,
            TransferBytes::MAX_BYTES,
            true,
        );
        self.commit_transfer_task(
            &mut transfer,
            block_offset,
            block_count,
            contig_ranges,
            transfer_options,
        )?;
        transfer.pmt = Some(scopeguard::ScopeGuard::into_inner(unpin_guard));
        Ok(transfer)
    }

    /// Prepares read transfer descriptors pointing to pre-pinned physical addresses.
    ///
    /// Unlike [`Self::prepare_transfer`], this function does not pin the VMO via BTI. Instead, it
    /// uses the pre-pinned physical addresses of `buffer` (which must have a power-of-two
    /// contiguity granularity not exceeding [`Self::minimum_contiguity`]), and sets `pmt: None` on
    /// the returned [`Transfer`].
    ///
    /// The caller is responsible for ensuring that `buffer` remains pinned and its physical
    /// addresses remain valid for the entire duration of the transfer until completion.
    pub fn prepare_read_transfer_pre_pinned(
        &self,
        slot: u8,
        buffer: &OwnedBuffer,
        block_offset: u32,
        transfer_options: TransferOptions,
    ) -> Result<Transfer, zx::Status> {
        let length = buffer.len();
        let vmo_offset = buffer.range().start;
        let block_size = MMC_BLOCK_SIZE as usize;
        if !vmo_offset.is_multiple_of(block_size) || !length.is_multiple_of(block_size) {
            return Err(zx::Status::INVALID_ARGS);
        }
        let block_count = u16::try_from(length / block_size)
            .ok()
            .and_then(NonZeroU16::new)
            .filter(|&count| u32::from(count.get()) <= self.max_transfer_blocks)
            .ok_or(zx::Status::INVALID_ARGS)?;

        let paddrs = buffer.paddrs().ok_or(zx::Status::INVALID_ARGS)?;
        let Some(contiguity) = buffer.contiguity() else {
            return Err(zx::Status::INVALID_ARGS);
        };
        // `PinnedBufferAllocator` clamps `contiguity` down to `chunk_size` (at least `page_size`)
        // when the BTI's `minimum_contiguity` exceeds `chunk_size` (such as `u64::MAX`).
        let page_size = zx::system_get_page_size() as u64;
        if !contiguity.is_power_of_two()
            || contiguity < std::cmp::min(self.minimum_contiguity, page_size)
            || contiguity > self.minimum_contiguity
        {
            return Err(zx::Status::INVALID_ARGS);
        }
        let vmo = buffer.vmo().ok_or(zx::Status::INVALID_ARGS)?;
        let contiguity = contiguity as usize;
        let offset = vmo_offset % contiguity;
        if paddrs.len() < (offset + length).div_ceil(contiguity) {
            return Err(zx::Status::INVALID_ARGS);
        }

        // We must pessimistically assume that there are pending writes to the VMO which need to be
        // flushed before we start doing the DMA.  If we didn't flush the writes, they might get
        // written out after we start to DMA, in which case they could stomp the read bytes.
        // TODO(https://fxbug.dev/458084387): Consider eliding this when possible.
        vmo.op_range(zx::VmoOp::CACHE_CLEAN_INVALIDATE, vmo_offset as u64, length as u64)?;

        let mut transfer = Transfer {
            tdl_slot: slot,
            vmo,
            vmo_offset: vmo_offset as u64,
            offset: block_offset as u64 * MMC_BLOCK_SIZE,
            length: length as u64,
            pmt: None,
            buffers: TransferBuffers::None,
            data_direction: Direction::Read,
        };

        let contig_ranges = PhysIter::new_with_options(
            paddrs,
            contiguity,
            offset,
            length,
            TransferBytes::MAX_BYTES,
            true,
        );
        self.commit_transfer_task(
            &mut transfer,
            block_offset,
            block_count,
            contig_ranges,
            transfer_options,
        )?;
        Ok(transfer)
    }

    fn commit_transfer_task(
        &self,
        transfer: &mut Transfer,
        block_offset: u32,
        block_count: NonZeroU16,
        contig_regions: PhysIter<'_>,
        transfer_options: TransferOptions,
    ) -> Result<(), zx::Status> {
        let page_size = zx::system_get_page_size() as usize;
        let slot = transfer.tdl_slot() as usize;
        let offset = slot * page_size;
        let max_descriptors = page_size / std::mem::size_of::<CommandQueueTransferDescriptor>();
        let mut contig_regions = contig_regions.peekable();
        let (first_addr, first_len) = contig_regions.next().ok_or(zx::Status::INVALID_ARGS)?;
        let crypto_params = if transfer_options.inline_crypto.is_enabled {
            let dun = u32::try_from(transfer_options.inline_crypto.dun)
                .map_err(|_| zx::Status::OUT_OF_RANGE)?;
            Some(CryptoParams { slot: transfer_options.inline_crypto.slot, dun })
        } else {
            None
        };
        let tdl_entry = if contig_regions.peek().is_none() {
            transfer.buffers = TransferBuffers::Single(first_addr);
            CommandQueueTDLEntry::single_buffer(
                transfer.data_direction,
                block_offset,
                block_count,
                first_addr as u64,
                transfer_options.queue_barrier,
                crypto_params,
            )
            .unwrap() // Unwrap OK, we already checked `block_count` is valid in the caller
        } else {
            let base_index = slot * max_descriptors;
            let mut i = 0;
            let mut region = (first_addr, first_len);
            while let Some(next) = contig_regions.next() {
                debug_assert!(i < max_descriptors);
                // Unwrap is OK since PhysIter ensured that the ranges are not too big.
                let length = TransferBytes::try_from(region.1).unwrap();
                self.extra_descriptors_buffer.write_element(
                    base_index + i,
                    &CommandQueueTransferDescriptor::transfer(region.0 as u64, length, false),
                );
                i += 1;
                region = next;
            }
            // Unwrap is OK since PhysIter ensured that the ranges are not too big.
            let length = TransferBytes::try_from(region.1).unwrap();
            self.extra_descriptors_buffer.write_element(
                base_index + i,
                &CommandQueueTransferDescriptor::transfer(region.0 as u64, length, true),
            );
            let num_descriptors = i + 1;
            self.extra_descriptors_buffer.cache_flush(
                offset,
                num_descriptors * std::mem::size_of::<CommandQueueTransferDescriptor>(),
            )?;
            let phys_address = self.extra_descriptors_buffer.phys()[slot];
            transfer.buffers = TransferBuffers::ScatterGatherList(phys_address, num_descriptors);
            CommandQueueTDLEntry::scatter_gather_buffers(
                transfer.data_direction,
                block_offset,
                block_count,
                phys_address as u64,
                transfer_options.queue_barrier,
                crypto_params,
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
        self.tdl_buffer.write_struct(tdl_slot as usize * std::mem::size_of::<T>(), &tdl_entry)
    }
}

// Used for testing.
#[allow(dead_code)]
#[derive(Debug)]
enum TransferBuffers {
    None,
    // The transfer descriptor in the TDL points directly to the buffer.
    Single(zx_paddr_t),
    // The transfer descriptor in the TDL points to a scatter/gather list
    ScatterGatherList(zx_paddr_t, usize),
}

pub struct Transfer {
    tdl_slot: u8,
    vmo: Arc<zx::Vmo>,
    vmo_offset: u64,
    offset: u64,
    length: u64,
    pmt: Option<zx::Pmt>,
    buffers: TransferBuffers,
    data_direction: Direction,
}

impl Transfer {
    /// Unpins memory pointed to by the Transfer.
    ///
    /// This MUST be called after the Transfer completes (successfully or not), or else pinned pages
    /// will be leaked.
    ///
    /// # SAFETY
    ///
    /// This MUST be called when the hardware will no longer access the memory pointed to by the
    /// Transfer (either before it was submitted, or after it completes).
    pub unsafe fn unpin(mut self) {
        if let Some(pmt) = self.pmt.take() {
            // SAFETY:  The transfer is complete, so the hardware will no longer access the pinned
            // memory.
            if let Err(status) = unsafe { pmt.unpin() } {
                warn!(status:?; "Failed to unpin PMT");
            }
        }
    }

    pub fn tdl_slot(&self) -> u8 {
        self.tdl_slot
    }

    #[allow(dead_code)]
    pub fn offset(&self) -> u64 {
        self.offset
    }

    #[allow(dead_code)]
    pub fn length(&self) -> u64 {
        self.length
    }

    #[allow(dead_code)]
    pub fn direction(&self) -> Direction {
        self.data_direction
    }

    #[allow(dead_code)]
    pub fn vmo(&self) -> &zx::Vmo {
        &self.vmo
    }

    #[allow(dead_code)]
    pub fn vmo_offset(&self) -> u64 {
        self.vmo_offset
    }

    pub fn opcode(&self) -> &'static str {
        self.data_direction.as_str()
    }

    /// Invalidate the cache for the VMO region used by this transfer.
    ///
    /// This must be called before the results of a read transfer are used.
    pub fn cache_invalidate(&self) {
        if self.data_direction == Direction::Read {
            if let Err(status) =
                self.vmo.op_range(zx::VmoOp::CACHE_CLEAN_INVALIDATE, self.vmo_offset, self.length)
            {
                warn!(status:?; "Failed to invalidate cache");
            }
        }
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
        assert!(self.pmt.is_none(), "Transfer::unpin was not called");
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TransferOptions {
    /// If set, the transfer will have the QBR flag set.
    ///
    /// The semantics are documented in JESD84-B51A B.2.6, but in brief, the QBR flag acts as a
    /// *full* barrier for a task, ensuring that:
    /// - all tasks which are active prior to the QBR task being started will complete before it is
    ///   submitted, and
    /// - all tasks which are issued whilst the QBR task is active will not be submitted until the
    ///   QBR task completes.
    ///
    /// Note that the QBR flag only controls the behaviour of the command queue.  The underlying MMC
    /// may reorder requests when it has a non-FIFO cache policy, and so additional commands are
    /// required for correct barrier behaviour.
    ///
    /// TODO(https://fxbug.dev/492496966): Using QBR for PRE_BARRIER is stricter than we need, and
    //// may have a performance impact.  Evaluate manually implementing PRE_BARRIER in software
    /// instead.
    pub queue_barrier: bool,
    pub inline_crypto: block_server::InlineCryptoOptions,
}

impl From<block_server::WriteOptions> for TransferOptions {
    fn from(options: block_server::WriteOptions) -> Self {
        Self {
            queue_barrier: options.flags.contains(block_server::WriteFlags::PRE_BARRIER),
            inline_crypto: options.inline_crypto,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU16;
    use std::sync::Arc;

    use super::*;
    use fake_bti::FakeBti;
    use sdmmc_spec::{CQHCI_TASK_DESCRIPTOR_LIST_NUM_SLOTS, CQHCI_TASK_DESCRIPTOR_LIST_SIZE};
    use storage_device::buffer_allocator::{BufferAllocator, BufferSource};
    use storage_device::pinned_buffer_allocator::PinnedBufferAllocator;

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
        let tdl_buffer = ContiguousBuffer::new(&bti, CQHCI_TASK_DESCRIPTOR_LIST_SIZE)
            .expect("Failed to create TDL DMA buffer");
        assert_eq!(tdl_buffer.phys(), TDL_BASE);

        let extra_descriptors_size =
            zx::system_get_page_size() as usize * CQHCI_TASK_DESCRIPTOR_LIST_DCMD_SLOT as usize;
        let extra_descriptors_buffer = PagedBuffer::new(&bti, extra_descriptors_size)
            .expect("Failed to create descriptor DMA buffer");
        assert_eq!(extra_descriptors_buffer.phys()[0], EXTRA_DESCRIPTORS_BASE);
        assert_eq!(extra_descriptors_buffer.phys()[1], EXTRA_DESCRIPTORS_BASE + 4096);

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
    fn single_buffer_transfer() {
        let (manager, fake_bti) = setup();
        fake_bti.set_paddrs(&[4096, 16384]);
        let vmo = Arc::new(zx::Vmo::create(4096).unwrap());
        let transfer = manager
            .prepare_transfer(0, vmo.clone(), 0, 0, 1, Direction::Read, TransferOptions::default())
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
                false,
                None,
            )
            .unwrap(),
        );
        let TransferBuffers::Single(ref paddr) = transfer.buffers else {
            panic!("No single paddr");
        };
        assert_eq!(*paddr, 4096);
        assert_eq!(*paddr, 4096);
        unsafe { transfer.unpin() };
        unsafe { Arc::try_unwrap(manager).unwrap().unpin_buffers() };
    }

    #[fuchsia::test]
    fn multi_buffer_transfer() {
        let (manager, fake_bti) = setup();
        let vmo = Arc::new(zx::Vmo::create(64 * 512).unwrap());
        fake_bti.set_paddrs(&[4096, 16384, 32768]);
        let transfer = manager
            .prepare_transfer(
                0,
                vmo.clone(),
                512,
                10,
                16,
                Direction::Read,
                TransferOptions::default(),
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
                false,
                None,
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
                    1,
                    vmo.clone(),
                    512,
                    40,
                    16,
                    Direction::Write,
                    TransferOptions::default(),
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
                    false,
                    None,
                ),
            );
            unsafe { transfer2.unpin() };
        }
        unsafe { transfer.unpin() };
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
                0,
                vmo.clone(),
                0,
                0,
                block_count,
                Direction::Read,
                TransferOptions::default(),
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
                false,
                None,
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
        unsafe { transfer.unpin() };
        unsafe { Arc::try_unwrap(manager).unwrap().unpin_buffers() };
    }

    #[fuchsia::test]
    fn multi_buffer_transfer_with_compressed_ranges() {
        let (manager, fake_bti) = setup();
        fake_bti.set_paddrs(&[4096, 8192, 16384, 20480, 65536]);
        let vmo = Arc::new(zx::Vmo::create(40 * 512).unwrap());
        let transfer = manager
            .prepare_transfer(
                0,
                vmo.clone(),
                0,
                100,
                40,
                Direction::Read,
                TransferOptions::default(),
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
                false,
                None,
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
        unsafe { transfer.unpin() };
        unsafe { Arc::try_unwrap(manager).unwrap().unpin_buffers() };
    }

    #[fuchsia::test]
    fn dcmd() {
        let (manager, _) = setup();
        manager.prepare_dcmd(MmcCommand::SendStatus, 0).expect("prepare_dcmd failed");
        unsafe { Arc::try_unwrap(manager).unwrap().unpin_buffers() };
    }

    #[fuchsia::test]
    async fn overlapping_transfers() {
        let (manager, fake_bti) = setup();
        let vmo = Arc::new(zx::Vmo::create(65536).unwrap());

        fake_bti.set_paddrs(&[4096, 16384]);
        let transfer1 = manager
            .prepare_transfer(0, vmo.clone(), 0, 0, 16, Direction::Read, TransferOptions::default())
            .expect("prepare_transfer 1 failed");

        fake_bti.set_paddrs(&[16384, 8192]);
        let transfer2 = manager
            .prepare_transfer(
                1,
                vmo.clone(),
                4096,
                16,
                16,
                Direction::Read,
                TransferOptions::default(),
            )
            .expect("prepare_transfer 2 failed");

        let page_size = zx::system_get_page_size() as usize;
        let descriptor_size = std::mem::size_of::<CommandQueueTransferDescriptor>();
        let descriptors_per_slot = page_size / descriptor_size;

        validate_extra_transfer_descriptors(
            &manager,
            0,
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
            1 * descriptors_per_slot,
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

        unsafe { transfer1.unpin() };
        unsafe { transfer2.unpin() };
        unsafe {
            Arc::try_unwrap(manager).expect("TransferManager still referenced").unpin_buffers()
        };
    }

    #[fuchsia::test]
    fn crypto_single_buffer_transfer() {
        let (manager, fake_bti) = setup();
        fake_bti.set_paddrs(&[4096]);
        let vmo = Arc::new(zx::Vmo::create(4096).unwrap());
        let transfer = manager
            .prepare_transfer(
                0,
                vmo.clone(),
                0,
                0,
                1,
                Direction::Read,
                TransferOptions {
                    queue_barrier: false,
                    inline_crypto: block_server::InlineCryptoOptions {
                        is_enabled: true,
                        slot: 5,
                        dun: 0x1234,
                    },
                },
            )
            .expect("prepare_transfer failed");
        assert!(transfer.pmt.is_some());

        // Validate that the TDL entry was written with crypto fields.
        let buf: Box<[CommandQueueTDLEntry]> = manager
            .tdl_buffer
            .vmo()
            .read_to_vec::<CommandQueueTDLEntry>(0, CQHCI_TASK_DESCRIPTOR_LIST_NUM_SLOTS as u64)
            .unwrap()
            .into_boxed_slice();
        let entry = buf[0];
        assert!(entry.task().ce());
        let cci = entry.task().cci();
        let dun = entry.task().dun();
        assert_eq!(cci, 5);
        assert_eq!(dun, 0x1234);

        unsafe {
            transfer.unpin();
            Arc::try_unwrap(manager).unwrap().unpin_buffers();
        }
    }

    fn test_pinned_allocator(
        fake_bti: &FakeBti,
        paddrs: &[zx_paddr_t],
    ) -> Arc<PinnedBufferAllocator> {
        fake_bti.set_paddrs(paddrs);
        let contiguity = zx::system_get_page_size() as u64;
        let source = BufferSource::new(paddrs.len() * contiguity as usize);
        Arc::new(PinnedBufferAllocator::with_chunk_size(
            MMC_BLOCK_SIZE as usize,
            source,
            fake_bti.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            contiguity,
            contiguity as usize,
        ))
    }

    #[fuchsia::test]
    fn pre_pinned_single_buffer_transfer() {
        let (manager, fake_bti) = setup();
        assert_eq!(manager.minimum_contiguity(), zx::system_get_page_size() as u64);
        let allocator = test_pinned_allocator(&fake_bti, &[4096]);
        let buffer = allocator.allocate_buffer_sync_owned(512);
        let transfer = manager
            .prepare_read_transfer_pre_pinned(0, &buffer, 0, TransferOptions::default())
            .expect("prepare_read_transfer_pre_pinned failed");
        assert!(transfer.pmt.is_none());
        assert_eq!(transfer.vmo().koid(), buffer.vmo().unwrap().koid());
        validate_tdl_entry(
            &manager,
            0,
            CommandQueueTDLEntry::single_buffer(
                Direction::Read,
                0,
                NonZeroU16::MIN,
                4096,
                false,
                None,
            )
            .unwrap(),
        );
        let TransferBuffers::Single(paddr) = transfer.buffers else {
            panic!("Expected single paddr");
        };
        assert_eq!(paddr, 4096);
        unsafe {
            Arc::try_unwrap(manager).unwrap().unpin_buffers();
        }
    }

    #[fuchsia::test]
    fn pre_pinned_multi_buffer_transfer() {
        let (manager, fake_bti) = setup();
        let allocator = test_pinned_allocator(&fake_bti, &[4096, 16384, 32768]);
        let buffer = allocator.allocate_buffer_sync_owned(16 * 512);
        let transfer = manager
            .prepare_read_transfer_pre_pinned(0, &buffer, 10, TransferOptions::default())
            .expect("prepare_read_transfer_pre_pinned failed");
        assert!(transfer.pmt.is_none());
        assert_eq!(transfer.vmo().koid(), buffer.vmo().unwrap().koid());
        validate_extra_transfer_descriptors(
            &manager,
            0,
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
        validate_tdl_entry(
            &manager,
            0,
            CommandQueueTDLEntry::scatter_gather_buffers(
                Direction::Read,
                10,
                NonZeroU16::try_from(16).unwrap(),
                EXTRA_DESCRIPTORS_BASE as u64,
                false,
                None,
            ),
        );
        let TransferBuffers::ScatterGatherList(addr, len) = transfer.buffers else {
            panic!("Expected s/g list");
        };
        assert_eq!(addr, EXTRA_DESCRIPTORS_BASE);
        assert_eq!(len, 2);
        unsafe {
            Arc::try_unwrap(manager).unwrap().unpin_buffers();
        }
    }

    #[fuchsia::test]
    fn pre_pinned_offset_buffer_transfer() {
        let (manager, fake_bti) = setup();
        let allocator = test_pinned_allocator(&fake_bti, &[4096]);
        let _pad = allocator.allocate_buffer_sync_owned(512);
        let buffer = allocator.allocate_buffer_sync_owned(512);
        assert_eq!(buffer.range().start, 512);
        let transfer = manager
            .prepare_read_transfer_pre_pinned(0, &buffer, 5, TransferOptions::default())
            .expect("prepare_read_transfer_pre_pinned failed");
        assert!(transfer.pmt.is_none());
        assert_eq!(transfer.vmo().koid(), buffer.vmo().unwrap().koid());
        validate_tdl_entry(
            &manager,
            0,
            CommandQueueTDLEntry::single_buffer(
                Direction::Read,
                5,
                NonZeroU16::MIN,
                4096 + 512,
                false,
                None,
            )
            .unwrap(),
        );
        let TransferBuffers::Single(paddr) = transfer.buffers else {
            panic!("Expected single paddr");
        };
        assert_eq!(paddr, 4096 + 512);
        unsafe {
            Arc::try_unwrap(manager).unwrap().unpin_buffers();
        }
    }

    #[fuchsia::test]
    fn pre_pinned_invalid_args() {
        let (manager, fake_bti) = setup();

        // Allocator buffer not pinned (buffer has no paddrs).
        let source = BufferSource::new(4096);
        let allocator = Arc::new(BufferAllocator::new(512, source));
        let unpinned_buffer = allocator.allocate_buffer_sync_owned(512);
        assert_eq!(
            manager
                .prepare_read_transfer_pre_pinned(
                    0,
                    &unpinned_buffer,
                    0,
                    TransferOptions::default(),
                )
                .err(),
            Some(zx::Status::INVALID_ARGS)
        );

        // Mismatched contiguity (pinned with contiguity != minimum_contiguity):
        fake_bti.set_paddrs(&[4096]);
        let source = BufferSource::new(4096);
        let pinned_allocator = Arc::new(PinnedBufferAllocator::new(
            512,
            source,
            fake_bti.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            8192,
        ));
        let buffer = pinned_allocator.allocate_buffer_sync_owned(512);
        assert_eq!(
            manager
                .prepare_read_transfer_pre_pinned(0, &buffer, 0, TransferOptions::default())
                .err(),
            Some(zx::Status::INVALID_ARGS)
        );

        // Unaligned length:
        fake_bti.set_paddrs(&[4096]);
        let unaligned_allocator = Arc::new(PinnedBufferAllocator::new(
            256,
            BufferSource::new(4096),
            fake_bti.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            4096,
        ));
        let unaligned_len_buf = unaligned_allocator.allocate_buffer_sync_owned(256);
        assert_eq!(
            manager
                .prepare_read_transfer_pre_pinned(
                    0,
                    &unaligned_len_buf,
                    0,
                    TransferOptions::default(),
                )
                .err(),
            Some(zx::Status::INVALID_ARGS)
        );
        unsafe {
            Arc::try_unwrap(manager).unwrap().unpin_buffers();
        }
    }

    #[fuchsia::test]
    fn crypto_dun_out_of_range() {
        let (manager, fake_bti) = setup();
        fake_bti.set_paddrs(&[4096]);
        let vmo = Arc::new(zx::Vmo::create(4096).unwrap());
        let err = manager
            .prepare_transfer(
                0,
                vmo.clone(),
                0,
                0,
                1,
                Direction::Read,
                TransferOptions {
                    queue_barrier: false,
                    inline_crypto: block_server::InlineCryptoOptions {
                        is_enabled: true,
                        slot: 5,
                        dun: 1u64 << 32,
                    },
                },
            )
            .expect_err("prepare_transfer with out-of-range DUN should fail");
        assert_eq!(err, zx::Status::OUT_OF_RANGE);
        unsafe {
            Arc::try_unwrap(manager).unwrap().unpin_buffers();
        }
    }

    #[fuchsia::test]
    fn u64_max_minimum_contiguity_transfers() {
        let (mut manager, fake_bti) = setup();
        Arc::get_mut(&mut manager).unwrap().minimum_contiguity = u64::MAX;
        assert_eq!(manager.minimum_contiguity(), u64::MAX);

        // 1. Direct transfer with non-zero VMO offset (512) within a single page.
        fake_bti.set_paddrs(&[0x22_0000]);
        let vmo = Arc::new(zx::Vmo::create(4096).unwrap());
        let transfer = manager
            .prepare_transfer(
                0,
                vmo.clone(),
                512,
                7,
                2,
                Direction::Read,
                TransferOptions::default(),
            )
            .expect("prepare_transfer with u64::MAX contiguity failed");
        assert!(transfer.pmt.is_some());
        validate_tdl_entry(
            &manager,
            0,
            CommandQueueTDLEntry::single_buffer(
                Direction::Read,
                7,
                NonZeroU16::try_from(2).unwrap(),
                0x22_0000 + 512,
                false,
                None,
            )
            .unwrap(),
        );
        unsafe {
            transfer.unpin();
        }

        // 2. Pre-pinned multi-chunk transfer with u64::MAX contiguity clamped to chunk_size.
        let page_size = zx::system_get_page_size() as usize;
        fake_bti.set_paddrs(&[0x22_0000, 0x35_0000]);
        let source = BufferSource::new(2 * page_size);
        let allocator = Arc::new(PinnedBufferAllocator::with_chunk_size(
            MMC_BLOCK_SIZE as usize,
            source,
            fake_bti.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            u64::MAX,
            page_size,
        ));
        let buffer = allocator.allocate_buffer_sync_owned(page_size + 512);
        assert_eq!(buffer.range(), 0..(page_size + 512));
        let pre_pinned_transfer = manager
            .prepare_read_transfer_pre_pinned(1, &buffer, 20, TransferOptions::default())
            .expect("prepare_read_transfer_pre_pinned with u64::MAX contiguity failed");
        assert!(pre_pinned_transfer.pmt.is_none());
        let descriptors_per_slot =
            page_size / std::mem::size_of::<CommandQueueTransferDescriptor>();
        validate_extra_transfer_descriptors(
            &manager,
            descriptors_per_slot,
            &[
                CommandQueueTransferDescriptor::transfer(
                    0x22_0000,
                    TransferBytes::try_from(page_size).unwrap(),
                    false,
                ),
                CommandQueueTransferDescriptor::transfer(
                    0x35_0000,
                    TransferBytes::try_from(512).unwrap(),
                    true,
                ),
            ],
        );
        drop(buffer);

        // 3. Pre-pinned non-zero-offset transfer within a page-aligned chunk.
        let _pad = allocator.allocate_buffer_sync_owned(512);
        let offset_buffer = allocator.allocate_buffer_sync_owned(512);
        assert_eq!(offset_buffer.range(), 512..1024);
        let offset_transfer = manager
            .prepare_read_transfer_pre_pinned(2, &offset_buffer, 30, TransferOptions::default())
            .expect("prepare_read_transfer_pre_pinned at offset failed");
        assert!(offset_transfer.pmt.is_none());
        validate_tdl_entry(
            &manager,
            2,
            CommandQueueTDLEntry::single_buffer(
                Direction::Read,
                30,
                NonZeroU16::MIN,
                0x22_0000 + 512,
                false,
                None,
            )
            .unwrap(),
        );
        unsafe {
            Arc::try_unwrap(manager).unwrap().unpin_buffers();
        }
    }
}
