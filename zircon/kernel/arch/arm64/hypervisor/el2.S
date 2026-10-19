// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <arch/arm64/hypervisor/el2_state.h>
#include <arch/arm64/mmu.h>
#include <asm.h>
#include <lib/arch/arm64/feature-asm.h>
#include <lib/arch/asm.h>
#include <zircon/errors.h>

#define ESR_EL2_EC_FP                   0x07
#define ESR_EL2_EC_SHIFT                26

#define MDCR_EL2_TDOSA                  BIT_32(10)

// The switching code here can use the FP / vector registers, though
// most kernel code cannot.
.arch armv8.1-a
.arch_extension fp

.section .text.el2,"ax",@progbits
.align 12

// Temporary registers to use.
reg_xs  .req x9
reg_xt  .req x10
reg_xa  .req x11

.macro test_feature feat_reg, feat_mask, skip_label, feat_reg2=, feat_mask2=, scratch=reg_xa
    mrs \scratch, \feat_reg
    tst \scratch, \feat_mask
.ifnb \feat_reg2
    b.ne 0f
    mrs \scratch, \feat_reg2
    tst \scratch, \feat_mask2
.endif
    b.eq \skip_label
0:
.endm

.macro fp_state inst, off
    add reg_xs, reg_xs, \off
    \inst q0, q1, [reg_xs, FS_Q(0)]
    \inst q2, q3, [reg_xs, FS_Q(2)]
    \inst q4, q5, [reg_xs, FS_Q(4)]
    \inst q6, q7, [reg_xs, FS_Q(6)]
    \inst q8, q9, [reg_xs, FS_Q(8)]
    \inst q10, q11, [reg_xs, FS_Q(10)]
    \inst q12, q13, [reg_xs, FS_Q(12)]
    \inst q14, q15, [reg_xs, FS_Q(14)]
    \inst q16, q17, [reg_xs, FS_Q(16)]
    \inst q18, q19, [reg_xs, FS_Q(18)]
    \inst q20, q21, [reg_xs, FS_Q(20)]
    \inst q22, q23, [reg_xs, FS_Q(22)]
    \inst q24, q25, [reg_xs, FS_Q(24)]
    \inst q26, q27, [reg_xs, FS_Q(26)]
    \inst q28, q29, [reg_xs, FS_Q(28)]
    \inst q30, q31, [reg_xs, FS_Q(30)]
.ifc "\inst", "ldp"
    ldr reg_xt, [reg_xs, FS_FPSR]
    msr fpsr, reg_xt
    ldr reg_xt, [reg_xs, FS_FPCR]
    msr fpcr, reg_xt
.else
    mrs reg_xt, fpsr
    str reg_xt, [reg_xs, FS_FPSR]
    mrs reg_xt, fpcr
    str reg_xt, [reg_xs, FS_FPCR]
.endif
    sub reg_xs, reg_xs, \off
.endm

.macro system_register inst, off, sysreg
.ifc "\inst", "ldr"
    ldr reg_xa, [reg_xt, \off]
    msr \sysreg, reg_xa
.else
    mrs reg_xa, \sysreg
    str reg_xa, [reg_xt, \off]
.endif
.endm

// Save/restore unbanked EL0/EL1 system registers that are used by the EL2 host
// (E2H=1) and shared with the EL1/EL0 guest.
.macro host_system_state inst, off
    add reg_xt, reg_xs, \off
    system_register \inst, HSS_TPIDR_EL0, tpidr_el0
    system_register \inst, HSS_TPIDRRO_EL0, tpidrro_el0
    system_register \inst, HSS_CPACR_EL1, cpacr_el1
    system_register \inst, HSS_MDSCR_EL1, mdscr_el1
    system_register \inst, HSS_TPIDR_EL1, tpidr_el1
.ifc "\inst", "ldr"
    // Zircon maintains the invariant that sp_el0 is 0 while in the kernel
    // (user sp_el0 is saved/restored in the exception iframe).
    msr sp_el0, xzr
.endif
.endm

// Under FEAT_VHE (E2H=1), guest EL1 registers that are redirected for the EL2
// host are accessed via their *_el12 encodings so host EL2 state (SCTLR_EL2,
// TCR_EL2, TTBR0_EL2, TTBR1_EL2, MAIR_EL2, VBAR_EL2, etc.) remains untouched.
.macro guest_system_state inst, off
    add reg_xt, reg_xs, \off
    system_register \inst, SS_SP_EL0, sp_el0
    system_register \inst, SS_TPIDR_EL0, tpidr_el0
    system_register \inst, SS_TPIDRRO_EL0, tpidrro_el0
    system_register \inst, SS_CNTKCTL_EL1, cntkctl_el12
    system_register \inst, SS_CONTEXTIDR_EL1, contextidr_el12
    system_register \inst, SS_CPACR_EL1, cpacr_el12
    system_register \inst, SS_CSSELR_EL1, csselr_el1
    system_register \inst, SS_ELR_EL1, elr_el12
    system_register \inst, SS_ESR_EL1, esr_el12
    system_register \inst, SS_FAR_EL1, far_el12
    system_register \inst, SS_AFSR0_EL1, afsr0_el12
    system_register \inst, SS_AFSR1_EL1, afsr1_el12
    system_register \inst, SS_MAIR_EL1, mair_el12
    system_register \inst, SS_MDSCR_EL1, mdscr_el1
    system_register \inst, SS_PAR_EL1, par_el1
    system_register \inst, SS_SCTLR_EL1, sctlr_el12
.ifc "\inst", "str"
    // Clear SCTLR_EL12 so SCTLR_EL12.M is 0 while executing in the host.
    msr sctlr_el12, xzr
.endif
    system_register \inst, SS_SP_EL1, sp_el1
    system_register \inst, SS_SPSR_EL1, spsr_el12
    system_register \inst, SS_TCR_EL1, tcr_el12
    system_register \inst, SS_TPIDR_EL1, tpidr_el1
    system_register \inst, SS_TTBR0_EL1, ttbr0_el12
    system_register \inst, SS_TTBR1_EL1, ttbr1_el12
    system_register \inst, SS_VBAR_EL1, vbar_el12
    system_register \inst, SS_AMAIR_EL1, amair_el12
    system_register \inst, SS_ELR_EL2, elr_el2
    system_register \inst, SS_SPSR_EL2, spsr_el2
.endm

.macro host_state inst, off
    add reg_xt, reg_xs, \off
    \inst x18, x19, [reg_xt, HS_X(0)]
    \inst x20, x21, [reg_xt, HS_X(2)]
    \inst x22, x23, [reg_xt, HS_X(4)]
    \inst x24, x25, [reg_xt, HS_X(6)]
    \inst x26, x27, [reg_xt, HS_X(8)]
    \inst x28, x29, [reg_xt, HS_X(10)]
.ifc "\inst", "ldp"
    ldr x30, [reg_xt, HS_X(12)]
.else
    str x30, [reg_xt, HS_X(12)]
.endif
.endm

.macro guest_state inst
    \inst x0, x1, [reg_xs, GS_X(0)]
    \inst x2, x3, [reg_xs, GS_X(2)]
    \inst x4, x5, [reg_xs, GS_X(4)]
    \inst x6, x7, [reg_xs, GS_X(6)]
    \inst x10, x11, [reg_xs, GS_X(10)]
    \inst x12, x13, [reg_xs, GS_X(12)]
    \inst x14, x15, [reg_xs, GS_X(14)]
    \inst x16, x17, [reg_xs, GS_X(16)]
    \inst x18, x19, [reg_xs, GS_X(18)]
    \inst x20, x21, [reg_xs, GS_X(20)]
    \inst x22, x23, [reg_xs, GS_X(22)]
    \inst x24, x25, [reg_xs, GS_X(24)]
    \inst x26, x27, [reg_xs, GS_X(26)]
    \inst x28, x29, [reg_xs, GS_X(28)]
.ifc "\inst", "ldp"
    ldr x30, [reg_xs, GS_X(30)]
.else
    str x30, [reg_xs, GS_X(30)]
.endif
.endm

.macro guest_xs_state inst, reg
    \inst x8, \reg, [reg_xs, GS_X(8)]
.endm

.macro guest_enter_state
    ldr reg_xt, [reg_xs, GS_CNTV_CVAL_EL0]
    msr cntv_cval_el02, reg_xt
    ldr reg_xt, [reg_xs, GS_CNTV_CTL_EL0]
    msr cntv_ctl_el02, reg_xt
    ldr reg_xt, [reg_xs, GS_VMPIDR_EL2]
    msr vmpidr_el2, reg_xt
.endm

.macro guest_exit_state
    mrs reg_xt, cntv_ctl_el02
    str reg_xt, [reg_xs, GS_CNTV_CTL_EL0]
    mrs reg_xt, cntv_cval_el02
    str reg_xt, [reg_xs, GS_CNTV_CVAL_EL0]
    mrs reg_xt, esr_el2
    str reg_xt, [reg_xs, GS_ESR_EL2]
    mrs reg_xt, far_el2
    str reg_xt, [reg_xs, GS_FAR_EL2]
    mrs reg_xt, hpfar_el2
    // This is not described well in the manual, but HPFAR_EL2 does not contain
    // the lower 8 bits of the IPA, so it must be shifted.
    lsl reg_xt, reg_xt, 8
    str reg_xt, [reg_xs, GS_HPFAR_EL2]
.endm

.macro switch_to_guest
    msr vttbr_el2, x0
    isb
.endm

.macro switch_to_host
    msr vttbr_el2, xzr
    isb
.endm

.macro pop_stack
    add sp, sp, 16
.endm

.macro guest_exit return_code
    // We push reg_xs onto the stack so we have one scratch register. We only
    // use reg_xs here, so that we don't accidentally trample the guest state.
    str reg_xs, [sp, -16]!
    mov reg_xs, \return_code
    str reg_xs, [sp, 8]
.endm

.macro entry_sync return_code
.align 7
    guest_exit \return_code
    b el2_guest_exit_or_fp_enter
.endm

.macro entry_async return_code
.align 7
    guest_exit \return_code
    b el2_guest_exit
.endm

.macro entry_invalid_exception
.align 7
    // If we got to here, the exception came from the host or EL2.
    //
    // Hang, relying on the lockup detector or watchdog to kick in.
    1:
    b 1b
.endm

.align 11
FUNCTION_LABEL(arm64_el2_exec_table)
    /* exceptions from current EL, using SP0 */
    entry_invalid_exception
    entry_invalid_exception
    entry_invalid_exception
    entry_invalid_exception

    /* exceptions from current EL, using SPx */
    entry_invalid_exception
    entry_invalid_exception
    entry_invalid_exception
    entry_invalid_exception

    /* exceptions from lower EL, running arm64 */
    entry_sync  ZX_OK          // Exception
    entry_async ZX_ERR_NEXT    // IRQ
    entry_invalid_exception
    entry_async ZX_OK          // SError

    /* exceptions from lower EL, running arm32 */
    entry_invalid_exception
    entry_invalid_exception
    entry_invalid_exception
    entry_invalid_exception

// zx_status_t arm64_el2_tlbi_ipa(uint64_t vttbr, zx_vaddr_t addr, bool terminal);
//
// With FEAT_VHE, TLBI IPAS2* and TLBI VMALLE1IS/VMALLS12E1IS require
// HCR_EL2.TGE == 0 and the target guest's VTTBR_EL2 loaded.
FUNCTION(arm64_el2_tlbi_ipa)
    mrs reg_xs, daif
    msr daifset, #0xf
    switch_to_guest
    movlit reg_xt, HCR_EL2_RW | HCR_EL2_E2H
    msr hcr_el2, reg_xt
    isb

    // TLBI IPAS2* instructions take bits [51:12] of the IPA.
    lsr x1, x1, 12

    // Invalidate IPA. Based on ARM DEN 0024A, page 12-5.
    dsb ishst
    cbnz w2, .Lterminal
    tlbi ipas2e1is, x1
    b .Lsync
.Lterminal:
    tlbi ipas2le1is, x1
.Lsync:
    dsb ish
    tlbi vmalle1is
    dsb ish
    isb

    movlit reg_xt, HCR_EL2_RW | HCR_EL2_E2H | HCR_EL2_TGE
    msr hcr_el2, reg_xt
    isb
    switch_to_host
    msr daif, reg_xs
    mov x0, ZX_OK
    ret
END_FUNCTION(arm64_el2_tlbi_ipa)

// zx_status_t arm64_el2_tlbi_vmid(uint64_t vttbr);
FUNCTION(arm64_el2_tlbi_vmid)
    mrs reg_xs, daif
    msr daifset, #0xf
    switch_to_guest
    movlit reg_xt, HCR_EL2_RW | HCR_EL2_E2H
    msr hcr_el2, reg_xt
    isb

    // Invalidate VMID. Based on ARM DEN 0024A, page 12-5.
    dsb ishst
    tlbi vmalls12e1is
    dsb ish
    isb

    movlit reg_xt, HCR_EL2_RW | HCR_EL2_E2H | HCR_EL2_TGE
    msr hcr_el2, reg_xt
    isb
    switch_to_host
    msr daif, reg_xs
    mov x0, ZX_OK
    ret
END_FUNCTION(arm64_el2_tlbi_vmid)

// zx_status_t arm64_el2_enter(uint64_t vttbr, El2State* state, uint64_t hcr);
//
// Under FEAT_VHE, the host kernel executes in EL2h (E2H=1, TGE=1). Entering
// a guest is a direct function call on the current kernel thread's stack.
FUNCTION(arm64_el2_enter)
    // Save host DAIF and PAN on the stack, masking all host exceptions (D, A, I, F)
    // before switching world state.
    mrs reg_xs, daif
    msr daifset, #0xf
    mov reg_xt, xzr
    test_feature id_aa64mmfr1_el1, #ID_AA64MMFR1_EL1_PAN, .Lno_save_pan
    mrs reg_xt, pan
.Lno_save_pan:
    stp reg_xs, reg_xt, [sp, -16]!

    // Install the hypervisor guest-exit vector table for the duration of guest execution.
    adr_global reg_xs, arm64_el2_exec_table
    msr vbar_el2, reg_xs

    switch_to_guest

    // Save El2State pointer into tpidr_el2.
    msr tpidr_el2, x1
    mov reg_xs, x1

    // Set the hypervisor control register (has E2H=1, TGE=0, VM=1, etc.).
    msr hcr_el2, x2
    isb

    // If the guest is being run for the first time, invalidate all stage 1 and
    // stage 2 TLB entries for this VMID in case the VMID has been used previously.
    // (Must run with TGE=0.)
    ldrb w10, [reg_xs, ES_RESUME]
    cbnz w10, .Lresume
    tlbi vmalls12e1is
    mov w10, 1
    strb w10, [reg_xs, ES_RESUME]
    dsb ish
    isb

.Lresume:
    // Allow guest EL1&0 access to physical counter, but trap physical timer.
    movlit reg_xt, CNTHCTL_EL2_E2H1_EL1PCTEN
    msr cnthctl_el2, reg_xt

    // Trap any accesses to OS lock debug registers in the guest while preserving HPMN.
    mrs reg_xt, mdcr_el2
    orr reg_xt, reg_xt, #MDCR_EL2_TDOSA
    msr mdcr_el2, reg_xt

    host_state stp, HS_XREGS
    host_system_state str, HS_SYSTEM_STATE

    // Enable floating-point, SVE, and SME traps (E2H=1: FPEN = 0b00, ZEN = 0b00, SMEN = 0b00).
    // SVE and SME are not exposed to guests (see ID_AA64PFR0_EL1 / ID_AA64PFR1_EL1 in vmexit.cc),
    // so trapping them prevents guests from accessing host P0-P15, FFR, or SME state.
    msr cptr_el2, xzr
    isb

    guest_enter_state
    guest_system_state ldr, GS_SYSTEM_STATE
    guest_state ldp
    guest_xs_state ldp, reg_xs

    // Enter guest via exception return.
    eret
    SPECULATION_POSTFENCE
END_FUNCTION(arm64_el2_enter)

FUNCTION_LABEL(el2_guest_exit_or_fp_enter)
    // Check ESR_EL2.EC to determine whether the exception was due to a
    // floating-point trap.
    mrs reg_xs, esr_el2
    lsr reg_xs, reg_xs, ESR_EL2_EC_SHIFT
    cmp reg_xs, ESR_EL2_EC_FP
    b.eq el2_fp_enter

FUNCTION_LABEL(el2_guest_exit)
    // Load El2State from tpidr_el2.
    mrs reg_xs, tpidr_el2

    guest_state stp
    // Load reg_xs from the stack, and save it in GuestState.
    ldr reg_xt, [sp]
    guest_xs_state stp, reg_xt
    guest_system_state str, GS_SYSTEM_STATE
    guest_exit_state

    // With E2H=1, CPTR_EL2.FPEN (bits [21:20]) is 0b00 when trapping FP and
    // 0b11 when untrapped by el2_fp_enter.
    mrs reg_xt, cptr_el2
    tbz reg_xt, CPTR_EL2_E2H1_FPEN_SHIFT, .Lfp_done

    // Restore floating-point state if it was modified by the guest.
    fp_state stp, GS_FP_STATE
    fp_state ldp, HS_FP_STATE

.Lfp_done:
    host_system_state ldr, HS_SYSTEM_STATE
    host_state ldp, HS_XREGS

    // Disable virtual timer set by guest.
    msr cntv_ctl_el02, xzr

    // Restore VMPIDR_EL2 to match host MPIDR_EL1.
    mrs reg_xt, mpidr_el1
    msr vmpidr_el2, reg_xt

    // Load return code pushed by guest_exit macro.
    ldr x0, [sp, 8]
    pop_stack

    // Restore host MDCR_EL2, CNTHCTL_EL2, HCR_EL2, VBAR_EL2, PAN, and DAIF.
    mrs reg_xt, mdcr_el2
    bic reg_xt, reg_xt, #MDCR_EL2_TDOSA
    msr mdcr_el2, reg_xt
    movlit reg_xs, CNTHCTL_EL2_E2H1_EL1PTEN | CNTHCTL_EL2_E2H1_EL1PCTEN | \
                   CNTHCTL_EL2_E2H1_EL0VCTEN | CNTHCTL_EL2_E2H1_EL0PCTEN
    msr cnthctl_el2, reg_xs
    movlit reg_xt, HCR_EL2_RW | HCR_EL2_E2H | HCR_EL2_TGE
    msr hcr_el2, reg_xt
    isb

    switch_to_host

    adr_global reg_xs, arm64_el1_exception
    msr vbar_el2, reg_xs
    isb

    ldp reg_xs, reg_xt, [sp], 16
    test_feature id_aa64mmfr1_el1, #ID_AA64MMFR1_EL1_PAN, .Lno_restore_pan
    msr pan, reg_xt
.Lno_restore_pan:
    msr daif, reg_xs

    // Return to host caller (Vcpu::Enter).
    ret

FUNCTION_LABEL(el2_fp_enter)
    // Save reg_xt so we have an extra register to swap floating-point state.
    // We're returning to the guest so we don't need the return code in [sp, 8].
    str reg_xt, [sp, 8]

    // Disable floating-point traps (E2H=1: FPEN = 0b11) while keeping SVE and SME
    // trapped (ZEN = 0b00, SMEN = 0b00), and reset exception syndrome.
    movlit reg_xs, CPTR_EL2_E2H1_FPEN
    msr cptr_el2, reg_xs
    msr esr_el2, xzr
    isb

    // Load El2State from tpidr_el2.
    mrs reg_xs, tpidr_el2

    fp_state stp, HS_FP_STATE
    fp_state ldp, GS_FP_STATE

    // Return to guest.
    ldp reg_xs, reg_xt, [sp], 16
    eret
    SPECULATION_POSTFENCE
