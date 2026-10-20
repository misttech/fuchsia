// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Register definitions and accessors for the ARM GICv3.
//!
//! The memory mapped registers live in three separate mappings: one for all of the distributor
//! registers, one for the redistributor registers (per cpu), and one for the ITS. Offsets defined
//! here are relative to the base of the mapping they belong to; the redistributor offsets are
//! relative to the base of the calling CPU's redistributor frame rather than taking a CPU index,
//! since the per-CPU frame is selected when constructing the bank.
//!
//! Only the registers the driver touches are defined.

use crate::arch_rs::arm64::{device_memory_barrier, isb};
use regio::arm64::{SysReg, spec};
use regio::{Offset, RwSafe, RwUnsafe};

/// Size of the distributor register mapping.
pub(super) const GICD_REG_SIZE: usize = 0x10000;

/// Size of the ITS register mapping: the control frame and the translation frame.
pub(super) const GITS_REG_SIZE: usize = 0x20000;

/// Offset of the SGI/PPI register frame within a redistributor's stride.
const GICR_SGI_BASE: usize = 0x10000;

//
// Distributor registers.
//

/// GICD_CTLR: Distributor Control Register.
pub(super) fn gicd_ctlr() -> Offset<u32, RwSafe> {
    Offset::new(0x0000)
}

/// GICD_TYPER: Interrupt Controller Type Register.
pub(super) fn gicd_typer() -> Offset<u32, RwSafe> {
    Offset::new(0x0004)
}

/// GICD_IGROUPR<n>: Interrupt Group Registers.
pub(super) fn gicd_igroupr(n: u32) -> Offset<u32, RwSafe> {
    Offset::new(0x0080 + (n as usize) * 4)
}

/// GICD_ISENABLER<n>: Interrupt Set-Enable Registers.
pub(super) fn gicd_isenabler(n: u32) -> Offset<u32, RwSafe> {
    Offset::new(0x0100 + (n as usize) * 4)
}

/// GICD_ICENABLER<n>: Interrupt Clear-Enable Registers.
pub(super) fn gicd_icenabler(n: u32) -> Offset<u32, RwSafe> {
    Offset::new(0x0180 + (n as usize) * 4)
}

/// GICD_ISPENDR<n>: Interrupt Set-Pending Registers.
pub(super) fn gicd_ispendr(n: u32) -> Offset<u32, RwSafe> {
    Offset::new(0x0200 + (n as usize) * 4)
}

/// GICD_ICPENDR<n>: Interrupt Clear-Pending Registers.
pub(super) fn gicd_icpendr(n: u32) -> Offset<u32, RwSafe> {
    Offset::new(0x0280 + (n as usize) * 4)
}

/// GICD_ICACTIVER<n>: Interrupt Clear-Active Registers.
pub(super) fn gicd_icactiver(n: u32) -> Offset<u32, RwSafe> {
    Offset::new(0x0380 + (n as usize) * 4)
}

/// GICD_ICFGR<n>: Interrupt Configuration Registers.
pub(super) fn gicd_icfgr(n: u32) -> Offset<u32, RwSafe> {
    Offset::new(0x0c00 + (n as usize) * 4)
}

/// GICD_IGRPMODR<n>: Interrupt Group Modifier Registers.
pub(super) fn gicd_igrpmodr(n: u32) -> Offset<u32, RwSafe> {
    Offset::new(0x0d00 + (n as usize) * 4)
}

/// GICD_IROUTER<n>: Interrupt Routing Registers. 64 bits wide.
pub(super) fn gicd_irouter(n: u32) -> Offset<u64, RwSafe> {
    Offset::new(0x6000 + (n as usize) * 8)
}

/// GICD_PIDR2: Peripheral ID2 Register.
pub(super) fn gicd_pidr2() -> Offset<u32, RwSafe> {
    Offset::new(0xffe8)
}

//
// GICD_CTLR bit definitions.
//

pub(super) const CTLR_ENABLE_G0: u32 = 1 << 0;
pub(super) const CTLR_ENABLE_G1NS: u32 = 1 << 1;
pub(super) const CTLR_ARE_S: u32 = 1 << 4;
pub(super) const GICD_CTLR_RWP: u32 = 1 << 31;

//
// GICD_TYPER bit definitions.
//

pub(super) const GICD_TYPER_LPIS: u32 = 1 << 17;

//
// GICD_PIDR2 bit definitions and masks.
//

pub(super) const GICD_PIDR2_ARCHREV_SHIFT: u32 = 4;
pub(super) const GICD_PIDR2_ARCHREV_MASK: u32 = 0xf;

//
// GICR_WAKER bit definitions and masks.
//

pub(super) const WAKER_CHILDREN_ASLEEP: u32 = 1 << 2;
pub(super) const WAKER_PROCESSOR_SLEEP: u32 = 1 << 1;

//
// Redistributor registers, relative to the base of a single redistributor's stride.
// Arranged as 2 or 3 banks (+1 empty bank) of 64KiB depending if it's GICv3 or GICv4.
//

/// GICR_CTLR: Redistributor Control Register.
pub(super) fn gicr_ctlr() -> Offset<u32, RwSafe> {
    Offset::new(0x0000)
}

/// GICR_TYPER: Redistributor Type Register. 64 bits wide.
pub(super) fn gicr_typer() -> Offset<u64, RwSafe> {
    Offset::new(0x0008)
}

/// GICR_WAKER: Redistributor Wake Register.
pub(super) fn gicr_waker() -> Offset<u32, RwSafe> {
    Offset::new(0x0014)
}

/// GICR_PROPBASER: Redistributor Properties Base Address Register. 64 bits wide.
/// Unsafe to write: it points the GIC at the LPI configuration table in memory.
pub(super) fn gicr_propbaser() -> Offset<u64, RwUnsafe> {
    Offset::new(0x0070)
}

/// GICR_PENDBASER: Redistributor LPI Pending Table Base Address Register. 64 bits wide.
/// Unsafe to write: it points the GIC at the LPI pending table in memory.
pub(super) fn gicr_pendbaser() -> Offset<u64, RwUnsafe> {
    Offset::new(0x0078)
}

/// GICR_INVLPIR: Redistributor Invalidate LPI Register. 64 bits wide.
pub(super) fn gicr_invlpir() -> Offset<u64, RwSafe> {
    Offset::new(0x00a0)
}

/// GICR_SYNCR: Redistributor Synchronize Register.
pub(super) fn gicr_syncr() -> Offset<u32, RwSafe> {
    Offset::new(0x00c0)
}

/// GICR_IGROUPR0: Interrupt Group Register 0 for SGIs and PPIs.
pub(super) fn gicr_igroupr0() -> Offset<u32, RwSafe> {
    Offset::new(GICR_SGI_BASE + 0x0080)
}

/// GICR_ISENABLER0: Interrupt Set-Enable Register 0 for SGIs and PPIs.
pub(super) fn gicr_isenabler0() -> Offset<u32, RwSafe> {
    Offset::new(GICR_SGI_BASE + 0x0100)
}

/// GICR_ICENABLER0: Interrupt Clear-Enable Register 0 for SGIs and PPIs.
pub(super) fn gicr_icenabler0() -> Offset<u32, RwSafe> {
    Offset::new(GICR_SGI_BASE + 0x0180)
}

/// GICR_ISPENDR0: Interrupt Set-Pending Register 0 for SGIs and PPIs.
pub(super) fn gicr_ispendr0() -> Offset<u32, RwSafe> {
    Offset::new(GICR_SGI_BASE + 0x0200)
}

/// GICR_ICPENDR0: Interrupt Clear-Pending Register 0 for SGIs and PPIs.
pub(super) fn gicr_icpendr0() -> Offset<u32, RwSafe> {
    Offset::new(GICR_SGI_BASE + 0x0280)
}

/// GICR_IGRPMOD0: Interrupt Group Modifier Register 0 for SGIs and PPIs.
pub(super) fn gicr_igrpmod0() -> Offset<u32, RwSafe> {
    Offset::new(GICR_SGI_BASE + 0x0d00)
}

//
// GICR_CTLR, GICR_TYPER, and LPI table bit definitions.
//

pub(super) const GICR_CTLR_ENABLE_LPIS: u32 = 1 << 0;
pub(super) const GICR_CTLR_RWP: u32 = 1 << 3;

pub(super) const GICR_TYPER_PLPIS: u64 = 1 << 0;
pub(super) const GICR_TYPER_DIRECT_LPI: u64 = 1 << 3;

pub(super) const GICR_PROPBASER_INNER_CACHE_NC: u64 = 1 << 7;
pub(super) const GICR_PROPBASER_INNER_CACHE_WB: u64 = 7 << 7;
pub(super) const GICR_PROPBASER_SHAREABILITY_MASK: u64 = 3 << 10;
pub(super) const GICR_PROPBASER_SHAREABILITY_ISH: u64 = 1 << 10;
pub(super) const GICR_PROPBASER_ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

pub(super) const GICR_PENDBASER_INNER_CACHE_NC: u64 = 1 << 7;
pub(super) const GICR_PENDBASER_INNER_CACHE_WB: u64 = 7 << 7;
pub(super) const GICR_PENDBASER_SHAREABILITY_MASK: u64 = 3 << 10;
pub(super) const GICR_PENDBASER_SHAREABILITY_ISH: u64 = 1 << 10;
pub(super) const GICR_PENDBASER_ADDR_MASK: u64 = 0x000f_ffff_ffff_0000;
pub(super) const GICR_PENDBASER_PTZ: u64 = 1 << 62;

//
// Interrupt Translation Service (ITS) registers, relative to the base of the ITS mapping.
//

/// GITS_CTLR: ITS Control Register.
pub(super) fn gits_ctlr() -> Offset<u32, RwSafe> {
    Offset::new(0x0000)
}

/// GITS_TYPER: ITS Type Register. 64 bits wide.
pub(super) fn gits_typer() -> Offset<u64, RwSafe> {
    Offset::new(0x0008)
}

/// GITS_CBASER: ITS Command Queue Descriptor. 64 bits wide.
/// Unsafe to write: it points the GIC at the command queue in memory.
pub(super) fn gits_cbaser() -> Offset<u64, RwUnsafe> {
    Offset::new(0x0080)
}

/// GITS_CWRITER: ITS Write Register. 64 bits wide.
/// Unsafe to write: it hands the ITS the commands below the new offset.
pub(super) fn gits_cwriter() -> Offset<u64, RwUnsafe> {
    Offset::new(0x0088)
}

/// GITS_CREADR: ITS Read Register. 64 bits wide.
pub(super) fn gits_creadr() -> Offset<u64, RwSafe> {
    Offset::new(0x0090)
}

/// GITS_BASER<n>: ITS Translation Table Descriptors. 64 bits wide.
/// Unsafe to write: it points the GIC at a translation table in memory.
pub(super) fn gits_baser(n: u32) -> Offset<u64, RwUnsafe> {
    Offset::new(0x0100 + (n as usize) * 8)
}

/// Offset of GITS_TRANSLATER, the MSI doorbell, from the ITS physical base.
pub(super) const GITS_TRANSLATER_OFFSET: u64 = 0x10040;

pub(super) const GITS_CTLR_ENABLE: u32 = 1 << 0;
pub(super) const GITS_CTLR_QUIESCENT: u32 = 1 << 31;

pub(super) const GITS_TYPER_PLPIS: u64 = 1 << 0;
pub(super) const GITS_TYPER_ITT_ENTRY_SIZE_SHIFT: u64 = 4;
pub(super) const GITS_TYPER_ITT_ENTRY_SIZE_MASK: u64 = 0xf;
pub(super) const GITS_TYPER_PTA: u64 = 1 << 19;

pub(super) const GITS_CBASER_VALID: u64 = 1 << 63;
pub(super) const GITS_CBASER_INNER_CACHE_NC: u64 = 1 << 59;
pub(super) const GITS_CBASER_INNER_CACHE_WB: u64 = 7 << 59;
pub(super) const GITS_CBASER_SHAREABILITY_MASK: u64 = 3 << 10;
pub(super) const GITS_CBASER_SHAREABILITY_ISH: u64 = 1 << 10;
pub(super) const GITS_CBASER_ADDR_MASK: u64 = 0x000f_ffff_ffff_0000;

pub(super) const GITS_CREADR_STALLED: u64 = 1 << 0;

pub(super) const GITS_BASER_VALID: u64 = 1 << 63;
pub(super) const GITS_BASER_INNER_CACHE_NC: u64 = 1 << 59;
pub(super) const GITS_BASER_INNER_CACHE_WB: u64 = 7 << 59;
pub(super) const GITS_BASER_TYPE_SHIFT: u64 = 56;
pub(super) const GITS_BASER_TYPE_MASK: u64 = 0x7;
pub(super) const GITS_BASER_SHAREABILITY_MASK: u64 = 3 << 10;
pub(super) const GITS_BASER_SHAREABILITY_ISH: u64 = 1 << 10;
pub(super) const GITS_BASER_PAGE_SIZE_MASK: u64 = 3 << 8;
pub(super) const GITS_BASER_PAGE_SIZE_16K: u64 = 1 << 8;
pub(super) const GITS_BASER_PAGE_SIZE_64K: u64 = 2 << 8;
pub(super) const GITS_BASER_ADDR_MASK_4K_16K: u64 = 0x000f_ffff_ffff_0000;
pub(super) const GITS_BASER_ADDR_MASK_64K: u64 = 0x0000_ffff_ffff_0000;

//
// CPU interface system registers.
//
// `ICC_SRE_EL1` is taken from `libarch`, which already models its layout. The remaining
// registers are accessed as raw 64 bit values.
//

const ICC_PMR_EL1: SysReg<spec::ICC_PMR_EL1, u64> = SysReg::new();
const ICC_IGRPEN1_EL1: SysReg<spec::ICC_IGRPEN1_EL1, u64> = SysReg::new();
const ICC_EOIR1_EL1: SysReg<spec::ICC_EOIR1_EL1, u64> = SysReg::new();
const ICC_IAR1_EL1: SysReg<spec::ICC_IAR1_EL1, u64> = SysReg::new();
const ICC_SGI1R_EL1: SysReg<spec::ICC_SGI1R_EL1, u64> = SysReg::new();
const ICH_HCR_EL2: SysReg<spec::ICH_HCR_EL2, u64> = SysReg::new();

/// Writes ICC_PMR_EL1, the interrupt priority mask.
pub(super) fn write_pmr(val: u32) {
    ICC_PMR_EL1.write(val as u64);
    isb();
    device_memory_barrier();
}

/// Writes ICC_IGRPEN1_EL1, enabling or disabling group 1 interrupts at the CPU interface.
pub(super) fn write_igrpen(val: u32) {
    ICC_IGRPEN1_EL1.write(val as u64);
    isb();
}

/// Reads the SRE (System Register Enable) bit of ICC_SRE_EL1.
pub(super) fn read_sre() -> bool {
    libarch::arm64::ICC_SRE_EL1.read().sre()
}

/// Sets the SRE (System Register Enable) bit of ICC_SRE_EL1.
pub(super) fn write_sre_enable() {
    let mut sre = libarch::arm64::ICC_SRE_EL1.read();
    libarch::arm64::ICC_SRE_EL1.write(*sre.set_sre(true));
    isb();
}

/// Writes ICC_EOIR1_EL1, signalling end of interrupt for the given vector.
pub(super) fn write_eoir(val: u32) {
    ICC_EOIR1_EL1.write(val as u64);
    isb();
}

/// Reads ICC_IAR1_EL1, acknowledging the highest priority pending interrupt.
pub(super) fn read_iar() -> u32 {
    let iar = ICC_IAR1_EL1.read();
    device_memory_barrier();
    iar as u32
}

/// Writes ICC_SGI1R_EL1, generating a software generated interrupt.
pub(super) fn write_sgi1r(val: u64) {
    ICC_SGI1R_EL1.write(val);
    isb();
    device_memory_barrier();
}

/// Writes ICH_HCR_EL2, the control register of the virtual CPU interface.
pub(super) fn write_ich_hcr(val: u64) {
    ICH_HCR_EL2.write(val);
    isb();
}
