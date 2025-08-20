// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <arch/arm64/asm.h>
#include <arch/arm64/mmu.h>
#include <arch/arm64.h>
#include <lib/thread-stack/abi.h>
#include <lib/arch/asm.h>
#include <arch/code-patches/case-id-asm.h>
#include <arch/defines.h>
#include <arch/kernel_aspace.h>
#include <lib/code-patching/asm.h>
#include <zircon/tls.h>

#ifndef __has_feature
#define __has_feature(x) 0
#endif

//
// Register use:
//   x0-x3   Arguments
//   x9-x14  Scratch
//   x21-x28 Globals
//
tmp                     .req x9
tmp2                    .req x10
tmp3                    .req x11
tmp4                    .req x12
tmp5                    .req x13

handoff_paddr           .req x21

// Collect timestamp in tmp, tmp2.  Also clobbers tmp3-5.
.macro sample_ticks
  mrs tmp, cntpct_el0
  mrs tmp2, cntvct_el0

  // Workaround for Cortex-A73 erratum 858921.
  // See kernel/dev/timer/arm_generic/arm_generic_timer.cc::read_cntpct_a73.
  mrs tmp3, cntpct_el0
  mrs tmp4, cntvct_el0
  eor tmp5, tmp, tmp3
  tst tmp5, #(1 << 32)
  csel tmp, tmp, tmp3, eq
  eor tmp5, tmp2, tmp4
  tst tmp5, #(1 << 32)
  csel tmp2, tmp2, tmp4, eq
.endm

// Store sample_ticks results in a uint64_t[2] location.
// Clobbers tmp3.
.macro store_ticks symbol
  // There is no reloc like :lo12: that works for stp's scaled immediate,
  // so the add after the adrp can't be folded into the store like with str.
  adr_global tmp3, \symbol
  stp tmp, tmp2, [tmp3]
.endm

// This code is purely position-independent and generates no relocations
// that need boot-time fixup.
.function _start, global
.label PhysbootHandoff, global
  // As early as possible collect the time stamp.
  sample_ticks

  // Record the entry time stamp.
  store_ticks kernel_entry_ticks

  // Clear any phys exception handlers.
  msr vbar_el1, xzr

  // Save the x0 argument in a register that won't be clobbered.
  mov handoff_paddr, x0

  // Disable trampoline page-table in ttbr0
  movlit tmp, MMU_TCR_FLAGS_KERNEL
  msr tcr_el1, tmp
  isb

  // Invalidate the entire TLB
  tlbi vmalle1
  dsb sy
  isb

  // set the per cpu pointer for cpu 0
  adr_global percpu_ptr, arm64_percpu_array

  mov x0, handoff_paddr
  bl lk_main
  b .
.end_function

// This symbol is used by gdb python to know the base of the kernel module
.label KERNEL_BASE_ADDRESS, global, value=KERNEL_BASE
