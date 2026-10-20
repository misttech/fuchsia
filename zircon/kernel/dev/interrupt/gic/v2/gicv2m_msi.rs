// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2016, Google Inc. All rights reserved.
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Since ARM determines which GIC is used at runtime, this module sets up the gicv2m instances
//! of the MSI functions so that the `Gicv2` implementation of `InterruptControllerOps` can forward
//! its MSI methods to them.

use super::gicv2m::{ArmGicv2mFrameInfo, arm_gicv2m_get_frame_info};
use crate::dev_interrupt::MAX_MSI_IRQS;
use crate::pdev_interrupt::{
    InterruptHandler, InterruptVector, MsiBlock, mask_interrupt, register_int_handler,
    unmask_interrupt,
};
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, Ordering};
use debug::{ltracef, tracef};
use lazy_init::LazyInit;
use pow2_range_allocator::Pow2RangeAllocator;
use zx_status::Status;

const LOCAL_TRACE: u32 = 0;

/// The 32 and 64 bit MSI target allocators.
///
/// Constructed in `rust_arm_gicv2_init_early`; `arm_gicv2m_msi_init` sizes them with `init`.
static S_32BIT_TARGETS: LazyInit<Pow2RangeAllocator> = LazyInit::uninit();
static S_64BIT_TARGETS: LazyInit<Pow2RangeAllocator> = LazyInit::uninit();

static S_MSI_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Constructs the two MSI target allocators in place.
///
/// # Safety
///
/// Must only be called once during early single-threaded boot before multiple CPUs or threads
/// are active.
pub(super) unsafe fn init_allocators() {
    // SAFETY: Forwarded from `rust_arm_gicv2_init_early`, which carries the same contract, so
    // initialization cannot race with any other access to the allocators.
    unsafe {
        let Ok(()) = Pin::static_ref(&S_32BIT_TARGETS).init_pin(Pow2RangeAllocator::new());
        let Ok(()) = Pin::static_ref(&S_64BIT_TARGETS).init_pin(Pow2RangeAllocator::new());
    }
}

// TODO(https://fxbug.dev/537888583): `arm_gicv2m_msi_init` has no callers, so
// `S_MSI_INITIALIZED` is never set and `arm_gicv2m_msi_is_supported` always reports false. Wiring
// MSI up is a matter of calling it.
#[allow(dead_code)]
pub(super) fn arm_gicv2m_msi_init() -> Result<(), Status> {
    // `get()` reaches the allocator's `init` past `LazyInit`'s own `init`.
    if let Err(ret) = S_32BIT_TARGETS.get().init(MAX_MSI_IRQS as u32) {
        tracef!("Failed to initialize 32 bit allocation pool!\n");
        return Err(ret);
    }

    if let Err(ret) = S_64BIT_TARGETS.get().init(MAX_MSI_IRQS as u32) {
        tracef!("Failed to initialize 64 bit allocation pool!\n");
        S_32BIT_TARGETS.free();
        return Err(ret);
    }

    // TODO(johngro)
    //
    // Right now, the pow2 range allocator will not accept overlapping ranges.
    // It may be possible for fancy GIC implementations to have multiple MSI
    // frames aligned on 4k boundaries (for virtualisation) with either
    // completely or partially overlapping IRQ ranges.  If/when we need to deal
    // with hardware like this, we will need to come back here and make this
    // system more sophisticated.
    let mut ret: Result<(), Status> = Ok(());
    let mut i: u32 = 0;
    while let Ok(info) = arm_gicv2m_get_frame_info(i) {
        let pool = if (info.doorbell & 0xFFFFFFFF00000000) != 0 {
            &*S_64BIT_TARGETS
        } else {
            &*S_32BIT_TARGETS
        };

        let len = info.end_spi_id - info.start_spi_id + 1;
        ret = pool.add_range(info.start_spi_id, len);
        if let Err(status) = ret {
            tracef!(
                "Failed to add MSI IRQ range [{}, {}] to allocator (ret {}).\n",
                info.start_spi_id,
                len,
                status.into_raw()
            );
            break;
        }
        i += 1;
    }

    if ret.is_err() {
        S_32BIT_TARGETS.free();
        S_64BIT_TARGETS.free();
    }

    // TODO(https://fxbug.dev/537888583): The initialized flag is set even on the failure path
    // above, so a failed range add leaves `arm_gicv2m_msi_is_supported` reporting true over two
    // freed pools.
    S_MSI_INITIALIZED.store(true, Ordering::Relaxed);
    ret
}

/// Allocates a block of MSI compatible IRQ targets. See
/// `InterruptControllerOps::msi_alloc_block`.
pub(super) fn arm_gicv2m_msi_alloc_block(
    requested_irqs: u32,
    can_target_64bit: bool,
    _is_msix: bool,
    out_block: &mut MsiBlock,
) -> Result<(), Status> {
    if out_block.allocated {
        return Err(Status::BAD_STATE);
    }

    if requested_irqs == 0 || requested_irqs > MAX_MSI_IRQS as u32 {
        return Err(Status::INVALID_ARGS);
    }

    let mut alloc_result: Result<u32, Status> = Err(Status::INTERNAL);
    let mut is_32bit = false;
    // Round up to the power-of-two range size the allocator hands out.
    let alloc_size = requested_irqs.next_power_of_two();

    // If this MSI request can tolerate a 64 bit target address, start by
    // attempting to allocate from the 64 bit pool
    if can_target_64bit {
        alloc_result = S_64BIT_TARGETS.allocate_range(alloc_size);
    }

    // No allocation yet?  Fall back on the 32 bit pool
    if alloc_result.is_err() {
        alloc_result = S_32BIT_TARGETS.allocate_range(alloc_size);
        is_32bit = true;
    }

    // If we have not managed to allocate yet, then we fail
    let alloc_start = alloc_result?;

    // Find the target physical address for this allocation.
    //
    // TODO(johngro) : we could make this O(k) instead of O(n) by associating a
    // context pointer with ranges registered with the pow2 allocator.  Right
    // now, however, N tends to be 1, so it is difficult to be too concerned
    // about this.
    let mut ret: Result<ArmGicv2mFrameInfo, Status>;
    let mut i: u32 = 0;
    loop {
        ret = arm_gicv2m_get_frame_info(i);
        let Ok(info) = ret else {
            break;
        };

        let alloc_end = alloc_start + alloc_size - 1;

        if ((alloc_start >= info.start_spi_id) && (alloc_start <= info.end_spi_id))
            && ((alloc_end >= info.start_spi_id) && (alloc_end <= info.end_spi_id))
        {
            break;
        }
        i += 1;
    }

    // This should never ever fail
    debug_assert!(ret.is_ok());
    let info = match ret {
        Ok(info) => info,
        Err(status) => {
            let pool = if is_32bit { &*S_32BIT_TARGETS } else { &*S_64BIT_TARGETS };
            pool.free_range(alloc_start, alloc_size);
            return Err(status);
        }
    };

    ltracef!("success: base spi {} size {}\n", alloc_start, alloc_size);

    // Success!  Fill out the bookkeeping and we are done
    out_block.is_32bit = is_32bit;
    out_block.base_irq_id = alloc_start;
    out_block.num_irq = alloc_size;
    out_block.tgt_addr = info.doorbell;
    out_block.tgt_data = alloc_start;
    out_block.allocated = true;
    Ok(())
}

/// Returns `true` if MSI is supported. See `InterruptControllerOps::msi_is_supported`.
pub(super) fn arm_gicv2m_msi_is_supported() -> bool {
    S_MSI_INITIALIZED.load(Ordering::Relaxed)
}

/// Returns `true` if MSI masking is supported. See
/// `InterruptControllerOps::msi_supports_masking`.
pub(super) fn arm_gicv2m_msi_supports_masking() -> bool {
    S_MSI_INITIALIZED.load(Ordering::Relaxed)
}

/// Frees a previously allocated block of MSI IRQ targets. See
/// `InterruptControllerOps::msi_free_block`.
pub(super) fn arm_gicv2m_msi_free_block(block: &mut MsiBlock) {
    debug_assert!(block.allocated);

    // We stashed whether or not this came from the 32 bit pool in the platform context pointer
    let pool = if block.is_32bit { &*S_32BIT_TARGETS } else { &*S_64BIT_TARGETS };
    pool.free_range(block.base_irq_id, block.num_irq);
    *block = MsiBlock::default();
}

/// Registers a handler for one MSI within a block. See
/// `InterruptControllerOps::msi_register_handler`.
pub(super) fn arm_gicv2m_msi_register_handler(
    block: &MsiBlock,
    msi_id: u32,
    handler: InterruptHandler,
) {
    debug_assert!(block.allocated);
    debug_assert!(msi_id < block.num_irq);
    // SAFETY: The interrupt controller ops are registered before any MSI block can be allocated.
    let status =
        unsafe { register_int_handler(InterruptVector(block.base_irq_id + msi_id), handler) };
    debug_assert!(status.is_ok());
}

/// Masks or unmasks one MSI within a block. See
/// `InterruptControllerOps::msi_mask_unmask`.
pub(super) fn arm_gicv2m_msi_mask_unmask(block: &MsiBlock, msi_id: u32, mask: bool) {
    debug_assert!(block.allocated);
    debug_assert!(msi_id < block.num_irq);
    // SAFETY: The interrupt controller ops are registered before any MSI block can be allocated.
    unsafe {
        if mask {
            let _ = mask_interrupt(InterruptVector(block.base_irq_id + msi_id));
        } else {
            let _ = unmask_interrupt(InterruptVector(block.base_irq_id + msi_id));
        }
    }
}
