// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <platform.h>
#include <sys/types.h>
#include <zircon/types.h>

#include <arch/interrupt.h>
#include <arch/ops.h>
#include <arch/user_copy.h>
#include <kernel/ffi.h>

namespace {

FFI_ALWAYS_INLINE zx_status_t capture_faults_result(UserCopyCaptureFaultsResult res,
                                                    vaddr_t* fault_va, uint* fault_flags) {
  if (res.status != ZX_OK) {
    if (res.fault_info.has_value()) {
      *fault_va = res.fault_info->pf_va;
      *fault_flags = res.fault_info->pf_flags;
    } else {
      *fault_va = 0;
      *fault_flags = 0;
    }
  }
  return res.status;
}

}  // namespace

extern "C" {

bool cpp_arch_ints_disabled();
void cpp_arch_disable_ints();
void cpp_arch_enable_ints();
interrupt_saved_state_t cpp_arch_interrupt_save();
void cpp_arch_interrupt_restore(interrupt_saved_state_t state);
cpu_num_t cpp_arch_curr_cpu_num();
uint32_t cpp_arch_max_num_cpus();
zx_status_t cpp_arch_copy_from_user(void* dst, const void* src, size_t len);
zx_status_t cpp_arch_copy_to_user(void* dst, const void* src, size_t len);
zx_status_t cpp_arch_copy_from_user_capture_faults(void* dst, const void* src, size_t len,
                                                   vaddr_t* fault_va, uint* fault_flags);
zx_status_t cpp_arch_copy_to_user_capture_faults(void* dst, const void* src, size_t len,
                                                 vaddr_t* fault_va, uint* fault_flags);

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_arch_ints_disabled() { return arch_ints_disabled(); }
FFI_ALWAYS_INLINE void cpp_arch_disable_ints() { arch_disable_ints(); }
FFI_ALWAYS_INLINE void cpp_arch_enable_ints() { arch_enable_ints(); }
FFI_ALWAYS_INLINE interrupt_saved_state_t cpp_arch_interrupt_save() {
  return arch_interrupt_save();
}
FFI_ALWAYS_INLINE void cpp_arch_interrupt_restore(interrupt_saved_state_t state) {
  arch_interrupt_restore(state);
}
FFI_ALWAYS_INLINE cpu_num_t cpp_arch_curr_cpu_num() { return arch_curr_cpu_num(); }
FFI_ALWAYS_INLINE uint32_t cpp_arch_max_num_cpus() { return arch_max_num_cpus(); }
FFI_ALWAYS_INLINE zx_status_t cpp_arch_copy_from_user(void* dst, const void* src, size_t len) {
  return arch_copy_from_user(dst, src, len);
}
FFI_ALWAYS_INLINE zx_status_t cpp_arch_copy_to_user(void* dst, const void* src, size_t len) {
  return arch_copy_to_user(dst, src, len);
}
FFI_ALWAYS_INLINE zx_status_t cpp_arch_copy_from_user_capture_faults(void* dst, const void* src,
                                                                     size_t len, vaddr_t* fault_va,
                                                                     uint* fault_flags) {
  return capture_faults_result(arch_copy_from_user_capture_faults(dst, src, len), fault_va,
                               fault_flags);
}
FFI_ALWAYS_INLINE zx_status_t cpp_arch_copy_to_user_capture_faults(void* dst, const void* src,
                                                                   size_t len, vaddr_t* fault_va,
                                                                   uint* fault_flags) {
  return capture_faults_result(arch_copy_to_user_capture_faults(dst, src, len), fault_va,
                               fault_flags);
}

}  // extern "C"
