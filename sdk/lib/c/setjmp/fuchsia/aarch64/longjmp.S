// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/arch/asm.h>
#include <zircon/tls.h>

#include "asm-linkage.h"
#include "../jmp_buf.h"


// CFI to compute regno as (temp ^ [x2, #8*index]).
.macro jb_cfi_mangled_temp regno, temp, index
  .ifgt (8 * \index) - 0x3f
    .error "offset too large for one-byte LEB128"
  .endif
  .cfi_escape DW_CFA_val_expression, \regno, 6, \
              DW_OP_breg(2), 8 * \index, DW_OP_deref, \
              DW_OP_breg(\temp), 0, DW_OP_xor
.endm

// CFI to compute regno as (mangled ^ mangler).
.macro jb_cfi_mangled_reg regno, mangled, mangler
  .cfi_escape DW_CFA_val_expression, \regno, 5, \
              DW_OP_breg(\mangled), 0, DW_OP_breg(\mangler), 0, DW_OP_xor
.endm

.macro restore_pair reg1, reg2, index
  ldp \reg1, \reg2, [x0, #8*\index]
  .cfi_same_value \reg1
  .cfi_same_value \reg2
.endm

jmp_buf.llvm_libc_function longjmp
  // The caller's x0 value is actually recoverable throughout, though
  // its default state is undefined (call-clobbered).  Likewise, the
  // caller's x1 value is recoverable until the apparent caller is setjmp.
  .cfi_same_value x0
  .cfi_same_value x1

  // The sanitizer runtime wants to be informed of non-local exits.
  // Call __asan_handle_no_return() before doing the actual longjmp.
#if __has_feature(address_sanitizer)
  // Save our incoming argument registers and return address on the stack
  // around calling __asan_handle_no_return.  We save the zero word (xzr)
  // just to keep the stack aligned correctly as required by the ABI.
  stp.spill x0, x1
  stp.spill x30, xzr
  .label LIBC_ASM_LINKAGE(longjmp_sanitizer_call), global
  bl __asan_handle_no_return
  .label LIBC_ASM_LINKAGE(longjmp_sanitizer_return), global
  ldp.reload x30, xzr
  ldp.reload x0, x1
#endif

  // Find the manglers.
  adr_global x2, LIBC_ASM_LINKAGE(gJmpBufManglers)

  // Load the words that need to be demangled into temporaries.
  // These just hold values that were in the jmp_buf, so we don't
  // care about leaking them.
  ldp x3, x4, [x0, #8*JB_PC]  // x3 <= mangled PC; x4 <= mangled FP
  ldp x5, x6, [x0, #8*JB_SP]  // x5 <= mangled SP; x6 <= mangled SCSP (x18)

  // The next instruction clobbers the state of longjmp's caller.  So from here
  // on, we'll use CFI that unwinds to setjmp's caller instead.  Both callers
  // have the same x0 value.  The CFA rule is unchanged and continues to report
  // longjmp's CFA (the SP on entry).  The setjmp caller's x1 was never saved.
  .cfi_undefined x1
  jmp_buf.cfi 0, 19, JB_X(19)
  jmp_buf.cfi 0, 20, JB_X(20)
  jmp_buf.cfi 0, 21, JB_X(21)
  jmp_buf.cfi 0, 22, JB_X(22)
  jmp_buf.cfi 0, 23, JB_X(23)
  jmp_buf.cfi 0, 24, JB_X(24)
  jmp_buf.cfi 0, 25, JB_X(25)
  jmp_buf.cfi 0, 26, JB_X(26)
  jmp_buf.cfi 0, 27, JB_X(27)
  jmp_buf.cfi 0, 28, JB_X(28)
  jb_cfi_mangled_temp 29, 4, JB_FP
  jb_cfi_mangled_temp 30, 3, JB_PC
  jb_cfi_mangled_temp 31, 5, JB_SP
  jmp_buf.cfi 0, 64+8, JB_D(8)
  jmp_buf.cfi 0, 64+9, JB_D(9)
  jmp_buf.cfi 0, 64+10, JB_D(10)
  jmp_buf.cfi 0, 64+11, JB_D(11)
  jmp_buf.cfi 0, 64+12, JB_D(12)
  jmp_buf.cfi 0, 64+13, JB_D(13)
  jmp_buf.cfi 0, 64+14, JB_D(14)

  // We don't want to leak the raw mangler values, so load them into
  // their target registers rather than temporaries so we don't have
  // more temporaries to clear.
  ldp x30, x29, [x2, #8*JB_PC]
  jb_cfi_mangled_reg 30, 3, 30        // x30 (LR) = x30 ^ x3
  jb_cfi_mangled_reg 29, 4, 29        // x29 (FP) = x29 ^ x4
  ldp x2, x18, [x2, #8*JB_SP]
  jb_cfi_mangled_reg 31, 5, 2         // sp = x2 ^ x5
  jb_cfi_mangled_reg 18, 6, 18        // x18 (SCSP) = x18 ^ x6

  // Demangle in place.
  eor x30, x30, x3    // PC (LR)
  .cfi_same_value x30
  eor x29, x29, x4    // FP
  .cfi_same_value x29
  eor x2, x2, x5      // SP
  .cfi_register sp, x2
  eor x18, x18, x6     // Shadow call stack pointer
  .cfi_same_value x18

  // Restore all the vanilla callee-saves registers.
  restore_pair x19, x20, JB_X(19)
  restore_pair x21, x22, JB_X(21)
  restore_pair x23, x24, JB_X(23)
  restore_pair x25, x26, JB_X(25)
  restore_pair x27, x28, JB_X(27)
  restore_pair  d8,  d9, JB_D(8)
  restore_pair d10, d11, JB_D(10)
  restore_pair d12, d13, JB_D(12)
  restore_pair d14, d15, JB_D(14)

  // Restore SP last.
  // After this, our CFA is setjmp's CFA rather than longjmp's CFA.
  mov sp, x2
  .cfi_restore sp

  // Don't leak the raw SP value.
  mov x2, xzr

  // w0 = w1 ?: 1
  cmp w1, wzr
  csinc w0, w1, wzr, ne
  .cfi_undefined x0

  ret

jmp_buf.end_function
