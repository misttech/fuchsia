// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/arch/asm.h>
#include <zircon/tls.h>

#include "../jmp_buf.h"

jmp_buf.llvm_libc_function longjmp
  // The caller's a0 value is actually recoverable throughout, though
  // its default state is undefined (call-clobbered).
  .cfi_same_value a0

  // The sanitizer runtime wants to be informed of non-local exits.
  // Call __asan_handle_no_return() before doing the actual longjmp.
#if __has_feature(address_sanitizer)
  // Save our incoming argument registers and return address on the stack
  // around calling __asan_handle_no_return.
  add sp, sp, -32
  .cfi_adjust_cfa_offset 32
  sd ra, 0(sp)
  .cfi_rel_offset ra, 0
  sd a0, 8(sp)
  .cfi_rel_offset a0, 8
  sd a1, 16(sp)
  .label LIBC_ASM_LINKAGE(longjmp_sanitizer_call), global
  call __asan_handle_no_return
  .label LIBC_ASM_LINKAGE(longjmp_sanitizer_return), global
  ld ra, 0(sp)
  .cfi_same_value ra
  ld a0, 8(sp)
  .cfi_same_value a0
  ld a1, 16(sp)
  add sp, sp, 32
  .cfi_adjust_cfa_offset -32
#endif

  // Find the manglers.
  lla a2, LIBC_ASM_LINKAGE(gJmpBufManglers)

  // The PC (ra) and FP (s0) need to be demangled via temporaries first.
  ld a3, JB_PC*8(a0)
  ld a4, JB_FP*8(a0)
  ld a5, JB_PC*8(a2)
  ld a6, JB_FP*8(a2)

  // The next instruction clobbers the state of longjmp's caller.  So from here
  // on, we'll use CFI that unwinds to setjmp's caller instead.  Both callers
  // have the same a0 value (the jmp_buf).  The setjmp caller's a1 value cannot
  // be recovered.
  .cfi_undefined a1

  // CFI to compute regno as (\offset(a0) ^ \offset(a2)).
  .macro jb_cfi_mangled regno, index
    .ifgt (8 * \index) - 0x3f
      .error "offset too large for one-byte SLEB128"
    .endif
    .cfi_escape DW_CFA_val_expression, \regno, 7, \
                DW_OP_breg(10), (8 * \index), DW_OP_deref, \
                DW_OP_breg(12), (8 * \index), DW_OP_deref, DW_OP_xor
  .endm

  // CFI to compute x\regno as (x\tmpno ^ x\mangler).
  .macro jb_cfi_mangled_temp regno, tmpno, mangler
    .cfi_escape DW_CFA_val_expression, \regno, 5, \
                DW_OP_breg(\tmpno), 0, DW_OP_breg(\mangler), 0, DW_OP_xor
  .endm

  // CFI to find fs\n at JB_FS(\n)*8(a0).
  .macro jb_cfi_fs n
    .if \n < 2
      jmp_buf.cfi 10, (32 + 8 + \n), JB_FS(\n)
    .else
      jmp_buf.cfi 10, (32 + 18 + \n - 2), JB_FS(\n)
    .endif
  .endm

  jb_cfi_mangled_temp 1, 13, 15  // ra = x1, a3 = x13, a5 = x15
  jb_cfi_mangled_temp 8, 14, 16  // fp (s0) = x8, a4 = x14, a6 = x16
  jb_cfi_mangled 2, JB_SP        // sp = x2
  jb_cfi_mangled 3, JB_SCSP      // gp = x3

  jmp_buf.cfi 10, 9, JB_S(1)
  jmp_buf.cfi 10, 18, JB_S(2)
  jmp_buf.cfi 10, 19, JB_S(3)
  jmp_buf.cfi 10, 20, JB_S(4)
  jmp_buf.cfi 10, 21, JB_S(5)
  jmp_buf.cfi 10, 22, JB_S(6)
  jmp_buf.cfi 10, 23, JB_S(7)
  jmp_buf.cfi 10, 24, JB_S(8)
  jmp_buf.cfi 10, 25, JB_S(9)
  jmp_buf.cfi 10, 26, JB_S(10)
  jmp_buf.cfi 10, 27, JB_S(11)
#ifndef __riscv_float_abi_soft
  jb_cfi_fs 0
  jb_cfi_fs 1
  jb_cfi_fs 2
  jb_cfi_fs 3
  jb_cfi_fs 4
  jb_cfi_fs 5
  jb_cfi_fs 6
  jb_cfi_fs 7
  jb_cfi_fs 8
  jb_cfi_fs 9
  jb_cfi_fs 10
  jb_cfi_fs 11
#endif

  // Restore the vanilla call-saved registers.
  .macro restore_s n
    ld s\n, JB_S(\n)*8(a0)
    .cfi_same_value s\n
  .endm
  restore_s 1
  restore_s 2
  restore_s 3
  restore_s 4
  restore_s 5
  restore_s 6
  restore_s 7
  restore_s 8
  restore_s 9
  restore_s 10
  restore_s 11

#ifndef __riscv_float_abi_soft
  // Restore the call-saved FP registers.
  .macro restore_fs n
    fld fs\n, JB_FS(\n)*8(a0)
    .cfi_same_value fs\n
  .endm
  restore_fs 0
  restore_fs 1
  restore_fs 2
  restore_fs 3
  restore_fs 4
  restore_fs 5
  restore_fs 6
  restore_fs 7
  restore_fs 8
  restore_fs 9
  restore_fs 10
  restore_fs 11
#endif

  // Demangle and restore the fixed registers.
  xor ra, a3, a5
  .cfi_same_value ra

  xor fp, a4, a6
  .cfi_same_value fp

  ld a3, JB_SP*8(a0)
  ld a4, JB_SCSP*8(a0)
  ld a5, JB_SP*8(a2)
  jb_cfi_mangled_temp 2, 13, 15
  ld a6, JB_SCSP*8(a2)
  jb_cfi_mangled_temp 3, 14, 16

  // After this, the CFA is the original setjmp frame's CFA again.
  xor sp, a3, a5
  .cfi_restore sp

  xor gp, a4, a6
  .cfi_same_value gp

  // Don't leak raw values.
  mv a2, zero
  mv a3, zero
  mv a4, zero
  mv a5, zero
  mv a6, zero

  seqz a0, a1                   // a0 = (a1 == 0) ? 1 : 0
  .cfi_undefined a0             // Caller's a0 is no longer recoverable.
  add a0, a0, a1                // a0 += a1; AKA a0 = (a1 == 0) ? 1 : a1
  ret

jmp_buf.end_function
