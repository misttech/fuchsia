// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/arch/arm64/feature-asm.h>
#include <lib/arch/arm64/system-asm.h>
#include <lib/arch/asm.h>

// void ArmDropToEl1WithoutEl2Monitor();
//
// **NOTE:** This function can be called as a normal C function.  But it takes
// care to touch only a few registers.  It leaves the x0..x8 argument registers
// intact.  It uses only the following x9..x11 registers for scratch (and x30
// for return address).  Calls from assembly can rely on all other registers
// being preserved, as well as no references to sp or any memory, etc.

scratch .req x9
sysreg_scratch .req x10
hcrx_bits .req x11

// Do a tst on the value of a system register.
.macro mrs_tst sysreg, mask
  mrs sysreg_scratch, \sysreg
  tst sysreg_scratch, \mask
.endm

// Do an mrs with an immmediate or other operand only valid for mov, not mrs.
.macro msr_mov sysreg, value
  mov sysreg_scratch, \value
  msr \sysreg, sysreg_scratch
.endm

// Copy one system register to another.
.macro msr_mrs sysreg, value
  mrs sysreg_scratch, \value
  msr \sysreg, sysreg_scratch
.endm

.function ArmDropToEl1WithoutEl2Monitor, global
  mrs scratch, CurrentEL

  // Check the current exception level.
  cmp scratch, CURRENT_EL_EL_FIELD(1)
  beq .Ltarget
  cmp scratch, CURRENT_EL_EL_FIELD(2)
  beq .Lconfigure_el2
  // Otherwise, we are in EL3.
  //
  // Throw a fatal exception for now; support will be added for this case when
  // it can be developed against hardware that drops us there.
  udf #0

.Lconfigure_el2:
  // Clear the vector table for EL2.
  msr vbar_el2, xzr

  // Check for FEAT_HCX.
  mrs_tst id_aa64mmfr1_el1, #ID_AA64MMFR1_EL1_HCX
  beq .Lno_hcx

  // Set HCRX_EL2 bits so nothing traps to EL2 and EL2 doesn't constrain what
  // EL1 can do.  Unfortunately, the 0 state is not always that state.  Several
  // bits should be 1, but only if some FEAT_* is available.  The comments in
  // <lib/arch/arm64/system.h> for each HCRX_EL2 bit say whether each bit
  // should be set for some FEAT_*.  If more HCRX_EL2 bits are defined, then
  // new `set_hcrx_bits` invocations should be added here.

  mov hcrx_bits, xzr
  .macro set_hcrx_bits feature_reg, feature_mask, set_bit
    mov scratch, \set_bit
    mrs_tst \feature_reg, \feature_mask
    csel scratch, scratch, xzr, ne
    orr hcrx_bits, hcrx_bits, scratch
  .endm

  // FEAT_SYSREG128
  set_hcrx_bits id_aa64isar2_el1, #ID_AA64ISAR2_EL1_SYSREG_128, #HCRX_EL2_ENIDCP128

  // FEAT_D128
  set_hcrx_bits id_aa64mmfr3_el1, #ID_AA64MMFR3_EL1_D128, #HCRX_EL2_D128EN

  // FEAT_THE
  set_hcrx_bits id_aa64pfr1_el1, #ID_AA64PFR1_EL1_THE, #HCRX_EL2_PTTWI

  // FEAT_SCTLR2
  set_hcrx_bits id_aa64mmfr3_el1, #ID_AA64MMFR3_EL1_SCTLRX, #HCRX_EL2_SCTLR2EN

  // FEAT_TCR2
  set_hcrx_bits id_aa64mmfr3_el1, #ID_AA64MMFR3_EL1_TCRX, #HCRX_EL2_TCR2EN

  // FEAT_MOPS
  set_hcrx_bits id_aa64isar2_el1, #ID_AA64ISAR2_EL1_MOPS, #HCRX_EL2_MSCEN

  // FEAT_LS64
  set_hcrx_bits id_aa64isar1_el1, #ID_AA64ISAR1_EL1_LS64, \
           #(HCRX_EL2_ENASR | HCRX_EL2_ENALS | HCRX_EL2_ENAS0)

  .arch armv8.7-a
  msr hcrx_el2, hcrx_bits
.Lno_hcx:

  // Set EL1 to 64-bit, ensuring also that E2H is clear, as the assumed
  // layouts of CNTHCTL_EL2 and CPTR_EL2 depend on that. Also, ensure
  // that TSC is clear so that `smc` traps in EL3.
  msr_mov hcr_el2, #HCR_EL2_RW

  // Disable traps from EL1/EL0 regarding the physical counter.
  // Disable EL2 Event counter.
  // FEAT_ECV: Disable Enhanced Counter Virtualization feature and
  //   traps of virtual timer registers.
  msr_mov cnthctl_el2, #CNTHCTL_EL2_EL1PCEN | CNTHCTL_EL2_EL1PCTEN

  // Disable any virtual timer offset.
  msr cntvoff_el2, xzr

  // Clear stage 2 translation registers.
  // Should already be disabled by writing 0 to HCR_EL2.VM.
  msr tcr_el2, xzr
  msr vttbr_el2, xzr

  // Disable EL2 coprocessor traps.
  msr_mov cptr_el2, #CPTR_EL2_RES1

  // VMPIDR_EL2 and VPIDR_EL2 are what are returned from reads of MPIDR_EL1 and
  // MIDR_EL2 when in non-secure EL1, respectively. Reads of MPIDR_EL1 and
  // MIDR_EL1 when in EL2 return the real values, so we make sure now that
  // their VM*_EL2 counterparts reflect those values too.
  msr_mrs vmpidr_el2, mpidr_el1
  msr_mrs vpidr_el2, midr_el1

  // Check whether the GIC system registers are supported.
  mrs_tst id_aa64pfr0_el1, #ID_AA64PFR0_EL1_GIC
  beq .Lno_gic_sysregs

  // Enable the GIC system registers in EL2, and allow their use in EL1.
  mrs scratch, icc_sre_el2
  orr scratch, scratch, #ICC_SRE_EL2_ENABLE
  orr scratch, scratch, #ICC_SRE_EL2_SRE
  msr icc_sre_el2, scratch

  // Disable the GIC virtual CPU interface.
  msr ich_hcr_el2, xzr

.Lno_gic_sysregs:
  // Set the return address and exception level.
  adr scratch, .Ltarget
  msr elr_el2, scratch
  msr_mov spsr_el2, #SPSR_EL2_DAIF | SPSR_EL2_M_EL1H

.Ldrop_to_el1:
  // Reuse the current stack pointer after the drop to EL1.
  msr_mov sp_el1, sp

  isb
  eret
  speculation_postfence

.Ltarget:
  ret
  speculation_postfence
.end_function
