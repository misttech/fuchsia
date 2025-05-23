// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/arch/asm.h>

#include "../needs-next.h"

NEEDS_NEXT_SYSCALL(zx_restricted_enter)

.function restricted_exit, global
  // Back from restricted mode
  // a0 holds the context, which is the stack pointer
  // a1 holds the reason code

  // Restore the stack pointer at the point of the restricted enter wrapper.
  mv sp, a0

  // Restore the shadow call stack pointer.
  ld gp, 8(sp)

  // Restore the callee saved registers.
  ld s0, 16(sp)
  ld s1, 24(sp)
  ld s2, 32(sp)
  ld s3, 40(sp)
  ld s4, 48(sp)
  ld s5, 56(sp)
  ld s6, 64(sp)
  ld s7, 72(sp)
  ld s8, 80(sp)
  ld s9, 88(sp)
  ld s10, 96(sp)
  ld s11, 104(sp)

  // Restore the return address.
  ld ra, 112(sp)

  // Restore the thread pointer.
  ld tp, 120(sp)

  // Move the reason code into the stored pointer.
  ld t3, (sp)
  sd a1, (t3)

  // Pop all the normal mode context off the stack.
  addi sp, sp, 128

  // Return to whatever address was in RA.
  // Make it appear as if the wrapper had returned ZX_OK.
  mv a0, zero
  ret
.end_function

.function restricted_enter_wrapper, global
  // Args 0 - 1 are already in a0 and a1.

  // Make space for all of the normal mode context on the stack.
  addi sp, sp, -128

  // Save the reason code pointer.
  sd a2, (sp)

  // Save the shadow call stack pointer.
  sd gp, 8(sp)

  // Save all of the callee saved registers.
  sd s0, 16(sp)
  sd s1, 24(sp)
  sd s2, 32(sp)
  sd s3, 40(sp)
  sd s4, 48(sp)
  sd s5, 56(sp)
  sd s6, 64(sp)
  sd s7, 72(sp)
  sd s8, 80(sp)
  sd s9, 88(sp)
  sd s10, 96(sp)
  sd s11, 104(sp)

  // Save the return address.
  sd ra, 112(sp)

  // Save the thread pointer.
  sd tp, 120(sp)

  // Pass the stack pointer as the context argument to the syscall.
  mv a2, sp

  call zx_restricted_enter

  // If we got here it must have failed.
  // Restore the return address from prior to the syscall. We have to do this
  // because RA is caller-saved.
  ld ra, 112(sp)
  // Reset the stack.
  addi sp, sp, 128
  ret
.end_function

// Load the contents of the array in *a0 to the FPU registers.
.function load_fpu_registers, global
  fld f0, (a0)
  fld f1, 8(a0)
  fld f2, 16(a0)
  fld f3, 24(a0)
  fld f4, 32(a0)
  fld f5, 40(a0)
  fld f6, 48(a0)
  fld f7, 56(a0)
  fld f8, 64(a0)
  fld f9, 72(a0)
  fld f10, 80(a0)
  fld f11, 88(a0)
  fld f12, 96(a0)
  fld f13, 104(a0)
  fld f14, 112(a0)
  fld f15, 120(a0)
  fld f16, 128(a0)
  fld f17, 136(a0)
  fld f18, 144(a0)
  fld f19, 152(a0)
  fld f20, 160(a0)
  fld f21, 168(a0)
  fld f22, 176(a0)
  fld f23, 184(a0)
  fld f24, 192(a0)
  fld f25, 200(a0)
  fld f26, 208(a0)
  fld f27, 216(a0)
  fld f28, 224(a0)
  fld f29, 232(a0)
  fld f30, 240(a0)
  fld f31, 248(a0)
  ret
.end_function

// Store the contents of the FPU registers into the array in *a0.
.function store_fpu_registers, global
  fsd f0, (a0)
  fsd f1, 8(a0)
  fsd f2, 16(a0)
  fsd f3, 24(a0)
  fsd f4, 32(a0)
  fsd f5, 40(a0)
  fsd f6, 48(a0)
  fsd f7, 56(a0)
  fsd f8, 64(a0)
  fsd f9, 72(a0)
  fsd f10, 80(a0)
  fsd f11, 88(a0)
  fsd f12, 96(a0)
  fsd f13, 104(a0)
  fsd f14, 112(a0)
  fsd f15, 120(a0)
  fsd f16, 128(a0)
  fsd f17, 136(a0)
  fsd f18, 144(a0)
  fsd f19, 152(a0)
  fsd f20, 160(a0)
  fsd f21, 168(a0)
  fsd f22, 176(a0)
  fsd f23, 184(a0)
  fsd f24, 192(a0)
  fsd f25, 200(a0)
  fsd f26, 208(a0)
  fsd f27, 216(a0)
  fsd f28, 224(a0)
  fsd f29, 232(a0)
  fsd f30, 240(a0)
  fsd f31, 248(a0)
  ret
.end_function