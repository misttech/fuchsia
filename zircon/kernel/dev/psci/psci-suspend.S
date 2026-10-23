// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <arch/asm_macros.h>
#include <dev/psci.h>
#include <lib/arch/arm64/feature-asm.h>
#include <lib/arch/asm.h>

// TODO(https://fxbug.dev/417553411): See about saving/restoring other
// architectural state including, sctlr2_el1 and tcr2_el1.

// PSTATE.PAN, accessed via its generic system register encoding so as not to
// require an assembler targeting armv8.1-a.  Only accessible when FEAT_PAN is
// implemented.
#define PAN_SYSREG s3_0_c4_c2_3

// Saves register state into the psci_cpu_resume_context pointed to by
// \context_reg.  Clobbers x4-x17, so \context_reg must not be one of them.
.macro save_resume_context context_reg
  mrs x4, daif
  mrs x5, sctlr_el1
  mrs x6, tcr_el1
  mrs x7, vbar_el1
  mrs x8, mdscr_el1
  mrs x9, cpacr_el1
  mrs x10, osdlr_el1
  mov x11, sp
  mrs x12, sp_el0
  mrs x13, contextidr_el1
  mrs x14, tpidr_el1
  mrs x15, tpidrro_el0
  mrs x16, tpidr_el0
  // Note: Vector/SVE registers (Z0-Z31, P0-P15, FFR) are not saved here because
  // PSCI CPU/system suspend is only invoked from kernel-only threads
  // (platform_suspend_cpu and platform_suspend_system assert user_thread == nullptr),
  // after any user thread's FP/SVE state has already been saved on context switch.
  mov x17, xzr
  tbz x9, #16, 0f
  // zcr_el1 (S3_0_c1_c2_0)
  mrs x17, S3_0_c1_c2_0
0:

  // Note, we're not saving ttbr0_el1 or ttbr1_el1 because we are relying on the
  // caller of |psci_do_resume| to restore those from global variables.
  stp x4, x5, [\context_reg, (0 * 16)]
  stp x6, x7, [\context_reg, (1 * 16)]
  stp x8, x9, [\context_reg, (2 * 16)]
  stp x10, x11, [\context_reg, (3 * 16)]
  stp x12, x13, [\context_reg, (4 * 16)]
  stp x14, x15, [\context_reg, (5 * 16)]
  stp x16, x17, [\context_reg, (6 * 16)]

  stp x18, x19, [\context_reg, (7 * 16)]
  stp x20, x21, [\context_reg, (8 * 16)]
  stp x22, x23, [\context_reg, (9 * 16)]
  stp x24, x25, [\context_reg, (10 * 16)]
  stp x26, x27, [\context_reg, (11 * 16)]
  stp x28, x29, [\context_reg, (12 * 16)]

  // PSTATE.PAN is not preserved across a powerdown.  When the CPU is powered
  // back on, it may begin executing with PAN clear (e.g. as established by the
  // firmware, or by |ArmDropToEl1|), so save it (if implemented) to be restored
  // by |psci_do_resume|.
  mov x4, xzr
  mrs x5, id_aa64mmfr1_el1
  tst x5, #ID_AA64MMFR1_EL1_PAN
  b.eq 1f
  mrs x4, PAN_SYSREG
1:
  stp lr, x4, [\context_reg, (13 * 16)]

  // When the kernel runs in EL2 (FEAT_VHE), the EL1 system registers saved
  // above are redirected to their EL2 counterparts (HCR_EL2.E2H), and the rest
  // of the EL2 state the kernel relies on is reinitialized by |ArmDropToEl1| on
  // resume, or set by the hypervisor on each guest entry.  However, VTCR_EL2 is
  // configured once per CPU (when the first guest is created) and is not
  // preserved across a powerdown, so save it to be restored by
  // |psci_do_resume|.
  mov x4, xzr
  mrs x5, CurrentEL
  cmp x5, #(2 << 2)
  b.ne 2f
  mrs x4, vtcr_el2
2:
  stp x4, xzr, [\context_reg, (14 * 16)]
.endm

// int64_t psci_do_suspend(psci_call_proc psci_call_routine,
//                         uint32_t power_state,
//                         paddr_t entry,
//                         psci_cpu_resume_context* context);
.function psci_do_suspend, global
  // x0 - psci_call_routine
  // w1 - power_state
  // x2 - entry
  // x3 - context
  save_resume_context x3

  // We're about to call psci_call_routine(PSCI64_CPU_SUSPEND, power_state,
  // entry, context), which will either return or branch to |entry|.  Before
  // issuing the call, stash the lr on the stack so that we can restore it after
  // the PSCI call returns and then return to *our* caller.
  mov x5, x0
  movlit w0, PSCI64_CPU_SUSPEND
  push_regs lr, xzr
  blr x5
  pop_regs lr, xzr
  ret
.end_function

// int64_t psci_do_system_suspend(psci_call_proc psci_call_routine,
//                                paddr_t entry,
//                                psci_cpu_resume_context* context);
.function psci_do_system_suspend, global
  // x0 - psci_call_routine
  // x1 - entry
  // x2 - context
  save_resume_context x2

  // We're about to call psci_call_routine(PSCI64_SYSTEM_SUSPEND, entry,
  // context, 0), which will either return or branch to |entry|.  Before
  // issuing the call, stash the lr on the stack so that we can restore it after
  // the PSCI call returns and then return to *our* caller.
  mov x5, x0
  movlit w0, PSCI64_SYSTEM_SUSPEND
  mov x3, xzr
  push_regs lr, xzr
  blr x5
  pop_regs lr, xzr
  ret
.end_function

// void psci_do_resume(psci_cpu_resume_context* context);
//
// Caller is expected to have enabled caches and mmu (including setting up
// ttbr0_el1, ttbr1_el1, mair_el1, tcr_el1), as well as spsel.
.function psci_do_resume, global
  // x0 - context

  ldp x4, x5, [x0, (0 * 16)]
  ldp x6, x7, [x0, (1 * 16)]
  ldp x8, x9, [x0, (2 * 16)]
  ldp x10, x11, [x0, (3 * 16)]
  ldp x12, x13, [x0, (4 * 16)]
  ldp x14, x15, [x0, (5 * 16)]
  ldp x16, x17, [x0, (6 * 16)]

  ldp x18, x19, [x0, (7 * 16)]
  ldp x20, x21, [x0, (8 * 16)]
  ldp x22, x23, [x0, (9 * 16)]
  ldp x24, x25, [x0, (10 * 16)]
  ldp x26, x27, [x0, (11 * 16)]
  ldp x28, x29, [x0, (12 * 16)]
  ldp lr, x2, [x0, (13 * 16)]

  msr tpidr_el0, x16
  msr tpidrro_el0, x15
  msr tpidr_el1, x14
  msr contextidr_el1, x13
  msr sp_el0, x12
  mov sp, x11
  msr osdlr_el1, x10
  msr cpacr_el1, x9
  tbz x9, #16, 0f
  isb
  // zcr_el1 (S3_0_c1_c2_0)
  msr S3_0_c1_c2_0, x17
0:
  msr mdscr_el1, x8

  msr vbar_el1, x7
  msr tcr_el1, x6
  isb
  msr sctlr_el1, x5
  isb

  // Restore PSTATE.PAN (if implemented).  See |save_resume_context|.
  mrs x1, id_aa64mmfr1_el1
  tst x1, #ID_AA64MMFR1_EL1_PAN
  b.eq 1f
  msr PAN_SYSREG, x2
1:

  // Restore VTCR_EL2 (if running in EL2).  See |save_resume_context|.
  mrs x1, CurrentEL
  cmp x1, #(2 << 2)
  b.ne 2f
  ldr x2, [x0, (14 * 16)]
  msr vtcr_el2, x2
  isb
2:

  msr daif, x4

  // Return 1 to indicate that we resumed from a "power down" state.
  mov x0, 1

  ret
.end_function
