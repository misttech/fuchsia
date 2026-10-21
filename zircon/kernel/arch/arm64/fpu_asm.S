// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2015 Google Inc. All rights reserved
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <arch/arch_thread.h>
#include <lib/arch/asm.h>

// These routines move user FPU state between the FP / vector registers (and,
// with SVE, the predicate registers and FFR) and a struct fpstate, so they use
// those registers. Compiled kernel code (built without FP / SIMD / SVE) never
// holds values in v0-v31 or p0-p15, so the restore leaves nothing for a caller
// to expect back in v8-v15 or p4-p15, and neither routine emits CFI for them.
.arch_extension fp

// Run `\inst q<2n>, q<2n+1>` on each 32-byte pair of fpstate->regs at x0.
.macro fpstate_regs inst
  \inst q0, q1, [x0, #(FPSTATE_REGS_OFFSET + 0 * 32)]
  \inst q2, q3, [x0, #(FPSTATE_REGS_OFFSET + 1 * 32)]
  \inst q4, q5, [x0, #(FPSTATE_REGS_OFFSET + 2 * 32)]
  \inst q6, q7, [x0, #(FPSTATE_REGS_OFFSET + 3 * 32)]
  \inst q8, q9, [x0, #(FPSTATE_REGS_OFFSET + 4 * 32)]
  \inst q10, q11, [x0, #(FPSTATE_REGS_OFFSET + 5 * 32)]
  \inst q12, q13, [x0, #(FPSTATE_REGS_OFFSET + 6 * 32)]
  \inst q14, q15, [x0, #(FPSTATE_REGS_OFFSET + 7 * 32)]
  \inst q16, q17, [x0, #(FPSTATE_REGS_OFFSET + 8 * 32)]
  \inst q18, q19, [x0, #(FPSTATE_REGS_OFFSET + 9 * 32)]
  \inst q20, q21, [x0, #(FPSTATE_REGS_OFFSET + 10 * 32)]
  \inst q22, q23, [x0, #(FPSTATE_REGS_OFFSET + 11 * 32)]
  \inst q24, q25, [x0, #(FPSTATE_REGS_OFFSET + 12 * 32)]
  \inst q26, q27, [x0, #(FPSTATE_REGS_OFFSET + 13 * 32)]
  \inst q28, q29, [x0, #(FPSTATE_REGS_OFFSET + 14 * 32)]
  \inst q30, q31, [x0, #(FPSTATE_REGS_OFFSET + 15 * 32)]
.endm

// Run `\inst p<n>, [x9, #<n>, mul vl]` for p0-p15. At the 128-bit vector
// length each predicate is 2 bytes, so x9 points at fpstate->p_regs.
.macro fpstate_p_regs inst
  .irp n, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15
    \inst p\n, [x9, #\n, mul vl]
  .endr
.endm

// void arm64_fpu_restore_fpstate(const struct fpstate* fpstate, bool sve)
//
// p0-p15 and FFR are loaded only if `sve`.
.function arm64_fpu_restore_fpstate, global
  .llvm.stack_size 0
  fpstate_regs ldp

  tbz w1, #0, .Lrestore_no_sve
  .arch_extension sve
  add x9, x0, #FPSTATE_P_REGS_OFFSET
  add x10, x0, #FPSTATE_FFR_OFFSET
  ldr p0, [x10]
  wrffr p0.b
  fpstate_p_regs ldr
  .arch_extension nosve
.Lrestore_no_sve:

  // FPCR and FPSR are 32-bit fields written to the 64-bit registers.
  ldr w8, [x0, #FPSTATE_FPCR_OFFSET]
  msr fpcr, x8
  ldr w8, [x0, #FPSTATE_FPSR_OFFSET]
  msr fpsr, x8

  ret
  speculation_postfence
.end_function

// void arm64_fpu_save_fpstate(struct fpstate* fpstate, bool sve)
//
// p0-p15 and FFR are stored only if `sve`.
.function arm64_fpu_save_fpstate, global
  .llvm.stack_size 0
  fpstate_regs stp

  tbz w1, #0, .Lsave_no_sve
  .arch_extension sve
  add x9, x0, #FPSTATE_P_REGS_OFFSET
  add x10, x0, #FPSTATE_FFR_OFFSET
  fpstate_p_regs str
  // FFR is read through p0, which then gets its own value back.
  rdffr p0.b
  str p0, [x10]
  ldr p0, [x9]
  .arch_extension nosve
.Lsave_no_sve:

  // FPCR and FPSR are read as 64-bit registers into 32-bit fields.
  mrs x8, fpcr
  str w8, [x0, #FPSTATE_FPCR_OFFSET]
  mrs x8, fpsr
  str w8, [x0, #FPSTATE_FPSR_OFFSET]

  ret
  speculation_postfence
.end_function
