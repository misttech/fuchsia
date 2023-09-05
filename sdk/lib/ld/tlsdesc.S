// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/arch/asm.h>

#if defined(__aarch64__)

// The GOT slot contains the offset from TPIDR_EL0.  Just return it.
//
// Since the function is so tiny, keep it naturally-aligned (it's a mere 8
// bytes) just to be sure it can never straddle a cache line.
.function _ld_tlsdesc_runtime_static, export, align=8
  .cfi.tlsdesc

  // On AArch64 ILP32, GOT entries are 4 bytes, not 8.
# ifdef _LP64
#  define R0 x0
#  define ADDRSIZE 8
# else
#  define R0 w0
#  define ADDRSIZE 4
# endif

  // On entry x0 contains the argument: the address of the GOT slot pair.
  // The first word holds our own PC, the second the static TLS offset.
  ldr R0, [R0, #ADDRSIZE]

  // On exit x0 contains the return value: offset from $tp (TPIDR_EL0).
  .cfi_undefined R0

  ret
.end_function

#elif defined(__x86_64__)

// The GOT slot contains the offset from %fs.base.  Just return it.
//
// Since the function is so tiny, keep it naturally-aligned (it's actually
// only 5 bytes) just to be sure it can never straddle a cache line.
.function _ld_tlsdesc_runtime_static, export, align=8
  .cfi.tlsdesc

  // On entry %rax contains the argument: the address of the GOT slot pair.
  // The first word holds our own PC, the second the static TLS offset.
  //
  // Note that on x86-64 ILP32, GOT entries are still 8 bytes, to facilitate
  // use of the indirect addressing modes.
  mov 8(%rax), %rax

  // On exit %rax contains the return value: offset from $tp (%fs.base).
  .cfi_undefined %rax

  ret
.end_function

#elif defined(__riscv)

.macro .cfi.tlsdesc
  .cfi_return_column 64
  .cfi_register 64, t0
  .cfi_same_value t0
// .cfi_same_value everything but a0
.endm

// The GOT slot contains the offset from tp.  Just return it.
//
// Since the function is so tiny, keep it naturally-aligned just to be sure it
// can never straddle a cache line.
.function _ld_tlsdesc_runtime_static, export, align=4, cfi=custom
  .cfi.tlsdesc

# ifdef _LP64
#  define LOAD_ADDRSIZE ld
#  define ADDRSIZE 8
# else
#  define LOAD_ADDRSIZE lw
#  define ADDRSIZE 4
# endif

  // On entry a0 contains the argument: the address of the GOT slot range.
  // The first word holds our own PC, the second the static TLS offset.
  // Unlike other machines, RISC-V has more slots to spare after that.
  // But those are only useful in the dynamic-loading case.
  LOAD_ADDRSIZE a0, ADDRSIZE(a0)

  // On exit a0 contains the return value: offset from tp.
  .cfi_undefined a0

  // The caller's return address is in t0, with ra preserved.
  jr t0
.end_function

#else

// Not all machines have TLSDESC support specified in the psABI.

#endif
