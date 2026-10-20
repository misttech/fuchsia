// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2012-2016 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_DEV_INTERRUPT_GIC_V2_INCLUDE_DEV_INTERRUPT_ARM_GICV2_REGS_H_
#define ZIRCON_KERNEL_DEV_INTERRUPT_GIC_V2_INCLUDE_DEV_INTERRUPT_ARM_GICV2_REGS_H_

// The GICv2 driver is implemented in Rust (arm_gicv2.rs), which owns the mapping of all of the
// GICv2 registers: they live in one segment with the various subsections of the GIC at runtime
// supplied offsets within it. The accessors below expose the two apertures that consumers
// outside of the driver need; every other register is private to the driver.
extern "C" {

// Returns the virtual address of the GICV (virtual CPU interface) aperture, or null if the
// platform does not implement it.
void* rust_arm_gicv2_gicv_address();

// Returns the virtual address of the GICH (virtual interface control) aperture.
void* rust_arm_gicv2_gich_address();

}  // extern "C"

#endif  // ZIRCON_KERNEL_DEV_INTERRUPT_GIC_V2_INCLUDE_DEV_INTERRUPT_ARM_GICV2_REGS_H_
