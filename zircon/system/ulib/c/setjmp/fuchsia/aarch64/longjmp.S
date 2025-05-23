// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/arch/asm.h>
#include <zircon/tls.h>

#include "asm-linkage.h"
#include "../jmp_buf.h"


// CFI to find regno at [x16, #8*index].
.macro jb_cfi regno, index
  .sleb128.size_dispatch jb_cfi.1byte, jb_cfi.2byte, 8 * \index, \regno
.endm
.macro jb_cfi.1byte offset, regno
  .cfi_escape DW_CFA_expression, \regno, 2, \
              DW_OP_breg(16), SLEB128_1BYTE(\offset)
.endm
.macro jb_cfi.2byte offset, regno
  .cfi_escape DW_CFA_expression, \regno, 3, \
              DW_OP_breg(16), SLEB128_2BYTE(\offset)
.endm

// CFI to compute regno as ([x0, #8*index] ^ [x16, #8*index]).
.macro jb_cfi_mangled regno, index
  .ifgt (8 * \index) - 0x7f
    .error "offset too large for one-byte LEB128"
  .endif
  .cfi_escape DW_CFA_val_expression, \regno, 7, \
              DW_OP_breg(0), 8 * \index, DW_OP_deref, \
              DW_OP_breg(16), 8 * \index, DW_OP_deref, DW_OP_xor
.endm

// CFI to compute regno as (mangled ^ mangler).
.macro jb_cfi_mangled_reg regno, mangled, mangler
  .cfi_escape DW_CFA_val_expression, \regno, 5, \
              DW_OP_breg(\mangled), 0, DW_OP_breg(\mangler), 0, DW_OP_xor
.endm

.macro restore_pair reg1, reg2, index
  ldp \reg1, \reg2, [x16, #8*\index]
  .cfi_same_value \reg1
  .cfi_same_value \reg2
.endm

jmp_buf.llvm_libc_function longjmp

  // The sanitizer runtime wants to be informed of non-local exits.
  // Call __asan_handle_no_return() before doing the actual longjmp.
#if __has_feature(address_sanitizer)
  // Save our incoming argument registers and return address on the stack
  // around calling __asan_handle_no_return.  We save the zero word (xzr)
  // just to keep the stack aligned correctly as required by the ABI.
  stp.spill x0, x1
  stp.spill x30, xzr
  bl __asan_handle_no_return
  ldp.reload x30, xzr
  ldp.reload x0, x1
#endif

  // Move the jmp_buf pointer to a temporary register.
  // We'll use x0 as a scratch register since we clobber it on return anyway.
  mov x16, x0
  .cfi_register x0, x16

  // Find the manglers.
  adr_global x0, LIBC_ASM_LINKAGE(gJmpBufManglers)

  // Load the words that need to be demangled into temporaries.
  // These just hold values that were in the jmp_buf, so we don't
  // care about leaking them.
.ifne JB_SP - JB_PC - 1
.error "JB_SP expected to follow JB_PC immediately"
.endif
  ldp x4, x5, [x16, #8*JB_PC]
.ifne JB_USP - JB_FP - 1
.error "JB_USP expected to follow JB_FP immediately"
.endif
  ldp x6, x7, [x16, #8*JB_FP]
  ldr x8, [x16, #8*JB_X(18)]

  // The next instruction clobbers the state of longjmp's caller.
  // So from here on, we'll use CFI that unwinds to setjmp's caller instead.
  // Both callers have the same x0 value, which we now have in x16.
  .cfi_undefined x1
  jb_cfi 19, JB_X(19)
  jb_cfi 20, JB_X(20)
  jb_cfi 21, JB_X(21)
  jb_cfi 22, JB_X(22)
  jb_cfi 23, JB_X(23)
  jb_cfi 24, JB_X(24)
  jb_cfi 25, JB_X(25)
  jb_cfi 26, JB_X(26)
  jb_cfi 27, JB_X(27)
  jb_cfi_mangled 29, JB_FP
  jb_cfi_mangled 30, JB_PC
  jb_cfi_mangled 31, JB_SP
  //jb_cfi_mangled ?, JB_USP  -- No DWARF register number for it!
  jb_cfi 64+8, JB_D(8)
  jb_cfi 64+9, JB_D(9)
  jb_cfi 64+10, JB_D(10)
  jb_cfi 64+11, JB_D(11)
  jb_cfi 64+12, JB_D(12)
  jb_cfi 64+13, JB_D(13)
  jb_cfi 64+14, JB_D(14)

  // We don't want to leak the raw mangler values, so load them into
  // their target registers rather than temporaries so we don't have
  // more temporaries to clear.
  ldp x30, x3, [x0, #8*JB_PC]
  jb_cfi_mangled_reg 30, 4, 30        // x30 (LR) = x30 ^ x4
  jb_cfi_mangled_reg 31, 5, 3         // sp = x3 ^ x5
  ldp x29, x2, [x0, #8*JB_FP]
  jb_cfi_mangled_reg 29, 6, 29        // x29 (FP) = x29 ^ x6
  //jb_cfi_mangled_reg ?, 7, 2        // unsafe_sp = x2 ^ x7
  ldr x9, [x0, #8*JB_X(18)]
  jb_cfi_mangled_reg 18, 8, 9        // x18 = x8 ^ x9

  // Get the thread pointer, where the unsafe SP is stored.
  // Reuse x0 so the address of gJmpBufManglers is not leaked.
  mrs x0, TPIDR_EL0

  // Demangle in place.
  eor x30, x30, x4    // PC (LR)
  .cfi_same_value x30
  eor x3, x3, x5      // SP
  .cfi_register sp, x3
  eor x29, x29, x6    // FP
  .cfi_same_value x29
  eor x2, x2, x7      // Unsafe SP
  //.cfi_register unsafe_sp, x2  -- No DWARF register number for it!
  eor x18, x8, x9     // Shadow call stack pointer
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
  str x2, [x0, #ZX_TLS_UNSAFE_SP_OFFSET]
  //.cfi_same_value unsafe_sp  -- No DWARF register number for it!
  mov sp, x3
  .cfi_same_value sp

  // Don't leak the raw SP values.
  mov x2, xzr
  mov x3, xzr

  // w0 = w1 ?: 1
  cmp w1, wzr
  csinc w0, w1, wzr, ne

  ret

.end_function
