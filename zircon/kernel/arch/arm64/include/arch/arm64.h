// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2014 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ARM64_H_
#define ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ARM64_H_

#ifndef __ASSEMBLER__

#include <lib/arch/asm.h>
#include <lib/arch/intrin.h>
#include <stdbool.h>
#include <sys/types.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <kernel/cpu.h>

struct iframe_t;

struct arch_exception_context {
  struct iframe_t* frame;
  uint64_t far;
  uint32_t esr;
  // The |user_synth_code| and |user_synth_data| fields have different values depending on the
  // exception type.
  //
  // 1) For ZX_EXCP_POLICY_ERROR, |user_synth_code| contains the type of the policy error (a
  // ZX_EXCP_POLICY_CODE_* value), and |user_synth_data| contains additional information relevant to
  // the policy error (e.g. the syscall number for ZX_EXCP_POLICY_CODE_BAD_SYSCALL).
  //
  // 2) For ZX_EXCP_FATAL_PAGE_FAULT, |user_synth_code| contains the |zx_status_t| error code
  // returned by the page fault handler, typecast to |uint32_t|. |user_synth_data| is 0.
  //
  // 3) For all other exception types, |user_synth_code| and |user_synth_data| are both set to 0.
  uint32_t user_synth_code;
  uint32_t user_synth_data;
};

// Implemented in or called from assembly.
extern "C" {
#if __has_feature(shadow_call_stack)
void arm64_uspace_entry(const iframe_t* iframe, vaddr_t kstack, vaddr_t scsp) __NO_RETURN;
#else
void arm64_uspace_entry(const iframe_t* iframe, vaddr_t kstack) __NO_RETURN;
#endif

extern arch::AsmLabel arm64_el1_exception;
extern arch::AsmLabel arm64_el1_exception_smccc11_workaround;
extern arch::AsmLabel arm64_el1_exception_smccc10_workaround;

void arm64_sync_exception(iframe_t* iframe, uint exception_flags, uint32_t esr);

void platform_irq(iframe_t* frame);
}  // extern C

inline uint64_t arm64_get_boot_el() { return __arm_rsr64("CurrentEL") >> 2; }

// Called during clock selection (if it is called at all) before secondary CPUs
// have started.
extern "C" void arm64_allow_pct_in_el0();

// Puts the calling cpu in a working state and reads its feature flags.
extern "C" void arm64_cpu_early_init();

// Allocates a stack for the secondary cpu with bootstrap data placed on it.
// Ready to be passed to the cpu when starting it for the first time.
// On success, |*out| is a virtual address near the top of the stack just below
// the bootstrap payload.
extern "C" zx_status_t arm64_create_secondary_stack(cpu_num_t cpu_num, uintptr_t* out);

// Frees a stack created by |arm64_create_secondary_stack|.
extern "C" zx_status_t arm64_free_secondary_stack(cpu_num_t cpu_num);

// Shortcuts for setting and clearing PSTATE.PAN.
inline void arm64_enable_pan() { __arm_wsr64("PAN", 1); }
inline void arm64_disable_pan() { __arm_wsr64("PAN", 0); }

#endif  // __ASSEMBLER__

// Used in above exception_flags arguments.
#define ARM64_EXCEPTION_FLAG_LOWER_EL (1 << 0)

// Used in the exceptions_c which argument.
#define ARM64_DISALLOWED_ARM32_SYSCALL (1 << 0)
#define ARM64_DISALLOWED_ARM32_SYNC_EXCEPTION (1 << 1)
#define ARM64_DISALLOWED_ARM32_ASYNC_EXCEPTION (1 << 2)

#endif  // ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ARM64_H_
