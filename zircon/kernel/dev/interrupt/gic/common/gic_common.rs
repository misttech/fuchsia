// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2013, Google Inc. All rights reserved.
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Definitions shared between the ARM GICv2 and GICv3 drivers. Keep in sync with
//! `<dev/interrupt/arm_gic_common.h>`.

use crate::pdev_interrupt::{InterruptHandler, InterruptVector};
use zx_status::Status;

/// First Private Peripheral Interrupt (PPI) INTID.
pub const GIC_BASE_PPI: u32 = 16;
/// First Shared Peripheral Interrupt (SPI) INTID.
pub const GIC_BASE_SPI: u32 = 32;

//
// GIC Revision.
//

pub const GICV2: u32 = 2;
pub const GICV3: u32 = 3;
pub const GICV4: u32 = 4;

//
// Flags for `arm_gic_sgi`.
//

pub const ARM_GIC_SGI_FLAG_TARGET_FILTER_MASK: u32 = 0x3;
/// Only forward the interrupt to CPUs that have the interrupt configured as group 1 (non-secure).
pub const ARM_GIC_SGI_FLAG_NS: u32 = 0x4;

/// Registers a software generated interrupt handler.
///
/// # Safety
///
/// The interrupt controller must be registered with the pdev interrupt layer.
pub unsafe fn register_sgi_handler(vector: u32, handler: InterruptHandler) -> Result<(), Status> {
    debug_assert!(vector < GIC_BASE_PPI);
    // SAFETY: This function's contract requires the interrupt controller to be registered with
    // the pdev interrupt layer, which is all `register_permanent_int_handler` requires.
    unsafe {
        crate::pdev_interrupt::register_permanent_int_handler(InterruptVector(vector), handler)
    }
}
