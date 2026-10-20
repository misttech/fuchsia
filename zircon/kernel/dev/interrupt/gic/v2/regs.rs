// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2012-2016 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Register definitions and accessors for the ARM GICv2.
//!
//! The registers are mapped in one segment at the GIC base address with all of the various
//! subsections of the GIC (distributor, CPU interface, virtual interface control and virtual
//! CPU interface) at runtime supplied offsets within it. The offsets defined here are therefore
//! absolute within that single mapping: each accessor adds the offset of the subsection it
//! belongs to, which is read from the statics the driver populates at init time.
//!
//! Only the registers the driver touches are defined.

use regio::{Offset, RwSafe};

/// Size of the CPU interface register aperture.
pub(super) const GICC_REG_SIZE: usize = 0x2000;
/// Size of the distributor register aperture.
pub(super) const GICD_REG_SIZE: usize = 0x1000;
/// Size of the virtual interface control register aperture.
pub(super) const GICH_REG_SIZE: usize = 0x1000;
/// Size of the virtual CPU interface register aperture.
pub(super) const GICV_REG_SIZE: usize = 0x2000;
/// Size of a single GICv2m MSI register frame.
pub(super) const GICV2M_FRAME_REG_SIZE: usize = 0x1000;

//
// CPU interface registers.
//

/// GICC_CTLR: CPU Interface Control Register, at offset 0x0000 of the CPU interface.
pub(super) fn gicc_ctlr() -> Offset<u32, RwSafe> {
    Offset::new(super::gicc_offset())
}

/// GICC_PMR: Interrupt Priority Mask Register.
pub(super) fn gicc_pmr() -> Offset<u32, RwSafe> {
    Offset::new(super::gicc_offset() + 0x0004)
}

/// GICC_IAR: Interrupt Acknowledge Register.
pub(super) fn gicc_iar() -> Offset<u32, RwSafe> {
    Offset::new(super::gicc_offset() + 0x000c)
}

/// GICC_EOIR: End of Interrupt Register.
pub(super) fn gicc_eoir() -> Offset<u32, RwSafe> {
    Offset::new(super::gicc_offset() + 0x0010)
}

/// GICC_IIDR: CPU Interface Identification Register.
pub(super) fn gicc_iidr() -> Offset<u32, RwSafe> {
    Offset::new(super::gicc_offset() + 0x00fc)
}

//
// Distributor registers.
//

/// GICD_CTLR: Distributor Control Register, at offset 0x000 of the distributor.
pub(super) fn gicd_ctlr() -> Offset<u32, RwSafe> {
    Offset::new(super::gicd_offset())
}

/// GICD_TYPER: Interrupt Controller Type Register.
pub(super) fn gicd_typer() -> Offset<u32, RwSafe> {
    Offset::new(super::gicd_offset() + 0x004)
}

/// GICD_ISENABLER<n>: Interrupt Set-Enable Registers.
pub(super) fn gicd_isenabler(n: u32) -> Offset<u32, RwSafe> {
    Offset::new(super::gicd_offset() + 0x100 + (n as usize) * 4)
}

/// GICD_ICENABLER<n>: Interrupt Clear-Enable Registers.
pub(super) fn gicd_icenabler(n: u32) -> Offset<u32, RwSafe> {
    Offset::new(super::gicd_offset() + 0x180 + (n as usize) * 4)
}

/// GICD_ISPENDR<n>: Interrupt Set-Pending Registers.
pub(super) fn gicd_ispendr(n: u32) -> Offset<u32, RwSafe> {
    Offset::new(super::gicd_offset() + 0x200 + (n as usize) * 4)
}

/// GICD_ICPENDR<n>: Interrupt Clear-Pending Registers.
pub(super) fn gicd_icpendr(n: u32) -> Offset<u32, RwSafe> {
    Offset::new(super::gicd_offset() + 0x280 + (n as usize) * 4)
}

/// GICD_ICACTIVER<n>: Interrupt Clear-Active Registers.
pub(super) fn gicd_icactiver(n: u32) -> Offset<u32, RwSafe> {
    Offset::new(super::gicd_offset() + 0x380 + (n as usize) * 4)
}

/// GICD_ITARGETSR<n>: Interrupt Processor Targets Registers.
pub(super) fn gicd_itargetsr(n: u32) -> Offset<u32, RwSafe> {
    Offset::new(super::gicd_offset() + 0x800 + (n as usize) * 4)
}

/// GICD_ICFGR<n>: Interrupt Configuration Registers.
pub(super) fn gicd_icfgr(n: u32) -> Offset<u32, RwSafe> {
    Offset::new(super::gicd_offset() + 0xc00 + (n as usize) * 4)
}

/// GICD_SGIR: Software Generated Interrupt Register.
pub(super) fn gicd_sgir() -> Offset<u32, RwSafe> {
    Offset::new(super::gicd_offset() + 0xf00)
}
