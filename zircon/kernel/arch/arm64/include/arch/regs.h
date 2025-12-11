// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
#ifndef ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_REGS_H_
#define ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_REGS_H_

#define ARM64_IFRAME_OFFSET_R (0 * 8)
#define ARM64_IFRAME_OFFSET_LR (30 * 8)
#define ARM64_IFRAME_OFFSET_USP (31 * 8)
#define ARM64_IFRAME_OFFSET_ELR (32 * 8)
#define ARM64_IFRAME_OFFSET_SPSR (33 * 8)
#define ARM64_IFRAME_SIZE ((30 + 4) * 8)

#ifndef __ASSEMBLER__

#include <stdint.h>
#include <zircon/compiler.h>

// Registers saved on entering the kernel via architectural exception.
struct iframe_t {
  uint64_t r[30];  // x0-x29
  uint64_t lr;     // x30 (arm64 lr)
  uint64_t usp;    // either SP_EL0 if from lower EL otherwise the SP before the iframe was pushed
  uint64_t elr;    // ELR_EL1
  uint64_t spsr;   // SPSR_EL1
};

static_assert(sizeof(iframe_t) % 16u == 0u);
static_assert(sizeof(iframe_t) == ARM64_IFRAME_SIZE);

// Registers in the iframe are logically laid out in specific pairs, and *must*
// either remain this way, or a lot of lower level ASM code will need to be
// updated.  The low level ASM code uses load/store pair instructions when
// restoring/saving register state from/to the iframe. The first 30 registers
// (r0-29) are implicitly paired together by nature of the C-array.  After that,
// the following layout rules need to be enforced:
//
// 1) [r[0], r[29]] are all paired.  IOW, r[N+1] must always follow r[N] for all
//    even N in the range [0, 2, 4, ... 28].
// 2) ELR/SPSR are a pair; SPSR must always immediately follow ELR.
// 3) LR/USP are a pair; USP must always immediately follow LR.
static_assert(__offsetof(iframe_t, r[0]) == ARM64_IFRAME_OFFSET_R);
static_assert(__offsetof(iframe_t, lr) == ARM64_IFRAME_OFFSET_LR);
static_assert(__offsetof(iframe_t, usp) == ARM64_IFRAME_OFFSET_USP);
static_assert(__offsetof(iframe_t, lr) + sizeof(uint64_t) == __offsetof(iframe_t, usp));
static_assert(__offsetof(iframe_t, elr) == ARM64_IFRAME_OFFSET_ELR);
static_assert(__offsetof(iframe_t, spsr) == ARM64_IFRAME_OFFSET_SPSR);
static_assert(__offsetof(iframe_t, elr) + sizeof(uint64_t) == __offsetof(iframe_t, spsr));

// Registers saved on entering the kernel via syscall.
using syscall_regs_t = iframe_t;

#endif  // !__ASSEMBLER__

#endif  // ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_REGS_H_
