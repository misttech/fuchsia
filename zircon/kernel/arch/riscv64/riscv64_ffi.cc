// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <arch/mp.h>
#include <arch/riscv64/mp.h>
#include <dev/interrupt.h>
#include <kernel/ffi.h>
#include <kernel/interrupt.h>

extern "C" {

uint32_t cpp_riscv64_curr_hart_id();
uint32_t cpp_riscv64_boot_hart_id();

FFI_ALWAYS_INLINE uint32_t cpp_riscv64_curr_hart_id() { return riscv64_curr_hart_id(); }
FFI_ALWAYS_INLINE uint32_t cpp_riscv64_boot_hart_id() { return riscv64_boot_hart_id(); }

zx_status_t cpp_interrupt_send_ipi(cpu_mask_t cpu_mask, uint8_t ipi);
void cpp_interrupt_init_percpu();
void cpp_int_handler_start(uint64_t* state);
uint32_t cpp_int_handler_finish(uint64_t* state);

FFI_ALWAYS_INLINE zx_status_t cpp_interrupt_send_ipi(cpu_mask_t cpu_mask, uint8_t ipi) {
  return interrupt_send_ipi(cpu_mask, static_cast<mp_ipi>(ipi));
}

FFI_ALWAYS_INLINE void cpp_interrupt_init_percpu() { interrupt_init_percpu(); }

FFI_ALWAYS_INLINE void cpp_int_handler_start(uint64_t* state) {
  int_handler_start(reinterpret_cast<int_handler_saved_state_t*>(state));
}

FFI_ALWAYS_INLINE uint32_t cpp_int_handler_finish(uint64_t* state) {
  return int_handler_finish(reinterpret_cast<int_handler_saved_state_t*>(state)) ? 1 : 0;
}

}  // extern "C"
