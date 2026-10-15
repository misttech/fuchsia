// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::sync::atomic::{AtomicU32, Ordering};
use debug::ltracef;
use fbl::{Recyclable, RefPtr, pin_make_ref_counted, ref_counted};
use ksync::{KMutex, RawSpinlock, guarded};
use pin_init::{pin_data, pinned_drop};
use zx_status::Status;
use zx_types::zx_info_msi_t;

use crate::counters::define_kcounter;
use crate::dev_interrupt::{MsiBlock, msi_alloc_block, msi_free_block, msi_is_supported};

define_kcounter!(MSI_CREATE_COUNT, "msi.create", Sum);
define_kcounter!(MSI_DESTROY_COUNT, "msi.destroy", Sum);

const LOCAL_TRACE: u32 = 0;

/// Function pointer type for allocating an MSI block.
pub(crate) type MsiAllocFn = fn(u32, bool, bool, &mut MsiBlock) -> Result<(), Status>;
/// Function pointer type for freeing an MSI block.
pub(crate) type MsiFreeFn = fn(&mut MsiBlock);
/// Function pointer type for checking whether MSI is supported by the platform.
pub(crate) type MsiSupportedFn = fn() -> bool;
/// For now limit the max number of allocations in a block to the limit of standard
/// MSI. MSI-X's enhanced allocation limits are not going to come into play until
/// we move interrupt allocation off of the bootstrap CPU.
pub(crate) type IdBitMaskType = u32;
/// Identifier for an MSI interrupt within an `MsiAllocation` block.
pub(crate) type MsiId = u32;
/// Maximum number of MSI allocations in a single block.
pub(crate) const MSI_ALLOCATION_COUNT_MAX: u32 = IdBitMaskType::BITS;

/// An MsiAllocation is a wrapper around an allocated block of MSI interrupts.
/// It allows for multiple MsiInterruptDispatchers to share an allocated block, and
/// synchronize access to an MSI capability dealing with multiple IRQs.
///
/// By default, all MSI Allocations use the platform's kernel msi_*
/// implementation for management of MSI blocks, but tests can override the
/// interface via create_with_fns() parameters. Since those methods are used in allocation
/// of interrupts but not dispatch the indirection of those calls is an
/// acceptable cost to have the benefit of not making the type signature more
/// complex with other compile-time approaches.
#[guarded]
#[ref_counted]
#[derive(Recyclable)]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct MsiAllocation {
    // Used to synchronize access to an MSI vector control register for MSI blocks
    // that consist of multiple vectors and MsiInterruptDispatchers. It is not
    // used to guard access to anything within the MsiAllocation itself.
    #[mutex]
    lock: KMutex<RawSpinlock>,
    // Function pointer for the MSI platform free function to facilitate unit tests
    // and free the block when the object is released.
    msi_free_fn: MsiFreeFn,
    block: MsiBlock,
    // A bitfield of MSI ids currently associated with MsiInterruptDispatchers.
    ids_in_use: AtomicU32,
}

impl MsiAllocation {
    /// Creates a new `MsiAllocation` object with `irq_cnt` interrupts.
    pub fn create(irq_cnt: u32) -> Result<RefPtr<Self>, Status> {
        // Defaults to allow test mocks to override.
        Self::create_with_fns(irq_cnt, msi_alloc_block, msi_free_block, msi_is_supported)
    }

    /// Creates a new `MsiAllocation` object with `irq_cnt` interrupts and custom MSI platform
    /// functions.
    pub(crate) fn create_with_fns(
        irq_cnt: u32,
        msi_alloc_fn: MsiAllocFn,
        msi_free_fn: MsiFreeFn,
        msi_support_fn: MsiSupportedFn,
    ) -> Result<RefPtr<Self>, Status> {
        if !msi_support_fn() {
            return Err(Status::NOT_SUPPORTED);
        }

        // Ensure the requested IRQs fit within the mask of permitted IRQs in an
        // allocation. MSI allocations must be a power of two.
        // MSI supports up to 32, MSI-X supports up to 2048.
        if irq_cnt == 0 || irq_cnt > MSI_ALLOCATION_COUNT_MAX || !irq_cnt.is_power_of_two() {
            return Err(Status::INVALID_ARGS);
        }

        let mut block = MsiBlock::default();
        msi_alloc_fn(
            irq_cnt, false, /* can_target_64bit */
            false, /* is_msix */
            &mut block,
        )?;
        debug_assert!(block.allocated);
        let msi_block = block;
        let mut cleanup = zr::defer(|| {
            if block.allocated {
                msi_free_fn(&mut block);
            }
        });

        ltracef!(
            "MSI Allocation: {{ tgr_addr = 0x{:x}, tgt_data = 0x{:08x}, base_irq_id = {} }}\n",
            msi_block.tgt_addr,
            msi_block.tgt_data,
            msi_block.base_irq_id
        );

        let msi = pin_make_ref_counted!(Self {
            lock <- KMutex::init(),
            msi_free_fn,
            block: msi_block,
            ids_in_use: AtomicU32::new(0),
        })
        .map_err(|_| Status::NO_MEMORY)?;

        MSI_CREATE_COUNT.add(1);
        cleanup.cancel();
        Ok(msi)
    }

    /// Returns `zx_info_msi_t` for this allocation.
    pub fn get_info(&self) -> zx_info_msi_t {
        zx_info_msi_t {
            target_addr: self.block.tgt_addr,
            target_data: self.block.tgt_data,
            base_irq_id: self.block.base_irq_id,
            num_irq: self.block.num_irq,
            interrupt_count: self.ids_in_use.load(Ordering::SeqCst).count_ones(),
        }
    }

    /// Returns a reference to the underlying `MsiBlock`.
    pub const fn block(&self) -> &MsiBlock {
        &self.block
    }

    /// Interface for `MsiInterruptDispatcher`s to reserve a given `msi_id` within this allocation
    /// for management.
    pub fn reserve_id(&self, msi_id: MsiId) -> Result<(), Status> {
        if msi_id >= self.block.num_irq {
            return Err(Status::INVALID_ARGS);
        }

        let mask: IdBitMaskType = 1 << msi_id;
        // `Ordering::Relaxed` suffices because only atomicity of the per-ID bit reservation is
        // needed; lifecycle ordering is established by dispatcher handle and `RefPtr`
        // synchronization.
        let prev_value = self.ids_in_use.fetch_or(mask, Ordering::Relaxed);
        if (prev_value & mask) != 0 {
            return Err(Status::ALREADY_BOUND);
        }
        Ok(())
    }

    /// Releases a previously reserved `msi_id` within this allocation.
    pub fn release_id(&self, msi_id: MsiId) -> Result<(), Status> {
        if msi_id >= self.block.num_irq {
            return Err(Status::INVALID_ARGS);
        }

        let mask: IdBitMaskType = 1 << msi_id;
        // `Ordering::Relaxed` suffices because only atomicity of the per-ID bit reservation is
        // needed; lifecycle ordering is established by dispatcher handle and `RefPtr`
        // synchronization.
        let prev_value = self.ids_in_use.fetch_and(!mask, Ordering::Relaxed);
        if (prev_value & mask) == 0 {
            return Err(Status::BAD_STATE);
        }
        Ok(())
    }

    /// Returns a reference to the allocation's spinlock.
    pub const fn lock(&self) -> &KMutex<MsiAllocationLockClass, RawSpinlock> {
        &self.lock
    }
}

#[pinned_drop]
impl PinnedDrop for MsiAllocation {
    fn drop(self: core::pin::Pin<&mut Self>) {
        debug_assert!(self.ids_in_use.load(Ordering::SeqCst) == 0);

        let this = self.project();
        if this.block.allocated {
            (*this.msi_free_fn)(this.block);
        }
        debug_assert!(!this.block.allocated);
        MSI_DESTROY_COUNT.add(1);
    }
}
