// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2016, Google Inc. All rights reserved.
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Support for the MSI extensions to the GICv2 architecture.  See the ARM Server
//! Base System Architecture v3.0 (ARM_DEN_0029) Appendix E for details.

use super::{arm_gicv2_read32, arm_gicv2_write32, regs};
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use debug::{dprintf, tracef};
use regio::{MmioBank, MmioPtr, Offset, RwSafe};
use zx_status::Status;

// Section 9.7
/// Type Register.
const MSI_TYPER_OFFSET: usize = 0x008;
/// Doorbell register (whack here for interrupt).
const MSI_SETSPI_NS_OFFSET: u64 = 0x040;
/// Interface ID register.
const MSI_IIDR_OFFSET: usize = 0xFCC;

// Section 9.9.1
const MIN_VALID_MSI_SPI: u32 = 32;
const MAX_VALID_MSI_SPI: u32 = 1020;

/// Maximum number of GICv2m register frames the driver tracks.
///
/// `arm_gicv2m_init` copies the frames into fixed size storage of this length. The sole caller,
/// `rust_arm_gicv2_init_post_vm`, describes exactly one frame.
const MAX_REG_FRAMES: usize = 1;

static G_REG_FRAMES: [AtomicU64; MAX_REG_FRAMES] = [const { AtomicU64::new(0) }; MAX_REG_FRAMES];
static G_REG_FRAMES_VIRT: [AtomicPtr<u32>; MAX_REG_FRAMES] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; MAX_REG_FRAMES];
static G_REG_FRAME_COUNT: AtomicU32 = AtomicU32::new(0);
/// Set once `arm_gicv2m_init` has stashed the frames.
static G_REG_FRAMES_STASHED: AtomicBool = AtomicBool::new(false);

/// Structure used to hold information about a GICv2m register frame
///
/// See [`arm_gicv2m_get_frame_info`].
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct ArmGicv2mFrameInfo {
    /// The first valid SPI ID in the frame
    pub start_spi_id: u32,
    /// The last valid SPI ID in the frame
    pub end_spi_id: u32,
    /// The physical address of the doorbell register
    pub doorbell: u64,
    /// The value of the Interface ID register
    pub iid: u32,
}

/// Reads a 32 bit register out of GICv2m register frame `frame_ndx`.
fn reg_rd(frame_ndx: usize, offset: usize) -> u32 {
    let virt = G_REG_FRAMES_VIRT[frame_ndx].load(Ordering::Relaxed);
    assert!(!virt.is_null(), "GICv2m register frame pointer is null");
    // SAFETY: `arm_gicv2m_init`'s contract makes every non-null `G_REG_FRAMES_VIRT` entry the
    // base of a `GICV2M_FRAME_REG_SIZE` device mapping that remains valid for the lifetime of
    // the kernel.
    let ptr = unsafe { MmioPtr::<u32, RwSafe>::new(virt) };
    let bank = MmioBank::new(ptr, regs::GICV2M_FRAME_REG_SIZE);
    // SAFETY: `offset` names a register within the bounds of the frame.
    unsafe { bank.at(Offset::<u32, RwSafe>::new(offset)) }.read()
}

/// Support for the MSI extensions to the GICv2 architecture.  See the ARM Server
/// Base System Architecture v3.0 (ARM_DEN_0029) Appendix E for details.
///
/// `reg_frames` holds the physical addresses of the 4k V2M register frames implemented by this
/// platform's GIC and `reg_frames_virt` the virtual addresses they are mapped at.
///
/// # Safety
///
/// Each entry of `reg_frames_virt` must be the base of a `GICV2M_FRAME_REG_SIZE` device mapping
/// of the matching `reg_frames` entry that remains valid for the lifetime of the kernel.
pub(super) unsafe fn arm_gicv2m_init(reg_frames: &[u64], reg_frames_virt: &[*mut u32]) {
    // Protect against double init.
    debug_assert!(!G_REG_FRAMES_STASHED.load(Ordering::Relaxed));
    debug_assert!(G_REG_FRAME_COUNT.load(Ordering::Relaxed) == 0);

    // If the user has no register frames, they should be using arm_gic, not
    // arm_gicv2m
    debug_assert!(!reg_frames.is_empty());
    debug_assert!(!reg_frames_virt.is_empty());

    assert!(reg_frames.len() == reg_frames_virt.len());
    assert!(reg_frames.len() <= MAX_REG_FRAMES);

    // Stash the frame info
    for (i, (phys, virt)) in reg_frames.iter().zip(reg_frames_virt.iter()).enumerate() {
        G_REG_FRAMES[i].store(*phys, Ordering::Relaxed);
        G_REG_FRAMES_VIRT[i].store(*virt, Ordering::Relaxed);
    }
    G_REG_FRAMES_STASHED.store(true, Ordering::Relaxed);
    G_REG_FRAME_COUNT.store(reg_frames.len() as u32, Ordering::Relaxed);

    // Walk the list of regions, and make sure that all of the controlled SPIs
    // are configured for edge triggered mode.
    for virt_index in 0..G_REG_FRAME_COUNT.load(Ordering::Relaxed) as usize {
        let type_reg = reg_rd(virt_index, MSI_TYPER_OFFSET);
        let base_spi = (type_reg >> 16) & 0x3FF;
        let num_spi = type_reg & 0x3FF;

        dprintf!(SPEW, "GICv2m {}: base spi {} count {}\n", virt_index, base_spi, num_spi);

        for spi_index in 0..num_spi {
            let spi_id = base_spi + spi_index;
            if !(MIN_VALID_MSI_SPI..=MAX_VALID_MSI_SPI).contains(&spi_id) {
                // TODO(https://fxbug.dev/537888583): The frame address printed here is indexed by
                // the SPI index rather than by the frame index, which looks like a typo. Out of
                // range indices print 0.
                tracef!(
                    "Invalid SPI ID ({}) found in GICv2m register frame @{:#x}\n",
                    spi_id,
                    G_REG_FRAMES
                        .get(spi_index as usize)
                        .map_or(0, |frame| frame.load(Ordering::Relaxed))
                );
                continue;
            }

            // TODO(https://fxbug.dev/537888583): This read-modify-write of GICD_ICFGR is not
            // serialized against the one in `gic_configure_interrupt`, which does hold `GicLock`.
            let reg_ndx = spi_id >> 4;
            let bit_shift = ((spi_id & 0xF) << 1) + 1;
            let mut reg_val = arm_gicv2_read32(regs::gicd_icfgr(reg_ndx));
            reg_val |= 0x1u32 << bit_shift;
            arm_gicv2_write32(regs::gicd_icfgr(reg_ndx), reg_val);
        }
    }
}

/// Fetch info about a specific GICv2m register frame.
///
/// `frame_ndx` is the index of the frame to fetch info for.
///
/// Returns a status code indicating the success or failure of the operation.
/// Status codes may include...
///  ++ `ZX_ERR_UNAVAILABLE` The GICv2m subsystem was never initialized
///  ++ `ZX_ERR_NOT_FOUND` frame_ndx is out of range
///  ++ `ZX_ERR_BAD_STATE` The frame index exists, but the registers in the frame
///     appear to be corrupt or invalid (internal error)
pub(super) fn arm_gicv2m_get_frame_info(frame_ndx: u32) -> Result<ArmGicv2mFrameInfo, Status> {
    let frame_count = G_REG_FRAME_COUNT.load(Ordering::Relaxed);
    if !G_REG_FRAMES_STASHED.load(Ordering::Relaxed) || frame_count == 0 {
        return Err(Status::UNAVAILABLE);
    }

    if frame_ndx >= frame_count {
        return Err(Status::NOT_FOUND);
    }

    let frame_ndx = frame_ndx as usize;
    let type_reg = reg_rd(frame_ndx, MSI_TYPER_OFFSET);
    let base_spi = (type_reg >> 16) & 0x3FF;
    let num_spi = type_reg & 0x3FF;
    let last_spi = (base_spi + num_spi).wrapping_sub(1);

    if num_spi == 0 || (base_spi < MIN_VALID_MSI_SPI) || (last_spi > MAX_VALID_MSI_SPI) {
        return Err(Status::BAD_STATE);
    }

    Ok(ArmGicv2mFrameInfo {
        start_spi_id: base_spi,
        end_spi_id: last_spi,
        doorbell: G_REG_FRAMES[frame_ndx].load(Ordering::Relaxed) + MSI_SETSPI_NS_OFFSET,
        iid: reg_rd(frame_ndx, MSI_IIDR_OFFSET),
    })
}
