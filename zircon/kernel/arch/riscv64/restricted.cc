// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
#include <arch.h>
#include <inttypes.h>
#include <trace.h>

#include <arch/debugger.h>
#include <arch/interrupt.h>
#include <arch/riscv64.h>
#include <arch/riscv64/feature.h>
#include <arch/vm.h>
#include <kernel/restricted_state.h>
#include <kernel/thread.h>

#define LOCAL_TRACE 0

extern "C" {
bool cpp_riscv64_ints_disabled();
uint64_t cpp_riscv64_get_sstatus_fp_v();
[[noreturn]] void cpp_riscv64_enter_uspace(const iframe_t* iframe);

bool cpp_riscv64_ints_disabled() { return arch_ints_disabled(); }

uint64_t cpp_riscv64_get_sstatus_fp_v() {
  return riscv64_csr_read(RISCV64_CSR_SSTATUS) &
         (RISCV64_CSR_SSTATUS_FS_MASK | RISCV64_CSR_SSTATUS_VS_MASK);
}

[[noreturn]] void cpp_riscv64_enter_uspace(const iframe_t* iframe) {
  arch_enter_uspace(iframe);
  __UNREACHABLE;
}
}
