// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ARM64_ARM64_FFI_H_
#define ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ARM64_ARM64_FFI_H_

#include <stdint.h>
#include <zircon/syscalls/debug.h>
#include <zircon/types.h>

extern "C" {

// Implemented in Rust (arch.rs); backs ArchIdlePowerThread::EnterIdleState().
void arch_enter_idle_state();

void cpp_arm64_get_tpidr_regs(uint64_t* tpidr_el0, uint64_t* tpidrro_el0);
void cpp_arm64_set_tpidr_regs(uint64_t tpidr_el0, uint64_t tpidrro_el0);
void cpp_arm64_enter_restricted_tpidr(uint64_t tpidr_el0, bool is_arm32);
uint64_t cpp_arm64_get_tpidr_el0();
zx_status_t cpp_arm64_get_general_regs(zx_thread_state_general_regs_t* regs);
zx_status_t cpp_arm64_set_general_regs(const zx_thread_state_general_regs_t* regs);

}  // extern "C"

#endif  // ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ARM64_ARM64_FFI_H_
