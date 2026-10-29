// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/arch/intrin.h>

#include <arch/arm64/arm64_ffi.h>
#include <arch/debugger.h>
#include <kernel/ffi.h>
#include <kernel/thread.h>

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_arm64_get_tpidr_regs(uint64_t* tpidr_el0, uint64_t* tpidrro_el0) {
  *tpidr_el0 = __arm_rsr64("tpidr_el0");
  *tpidrro_el0 = __arm_rsr64("tpidrro_el0");
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_arm64_set_tpidr_regs(uint64_t tpidr_el0, uint64_t tpidrro_el0) {
  __arm_wsr64("tpidr_el0", tpidr_el0);
  __arm_wsr64("tpidrro_el0", tpidrro_el0);
  Thread* thread = Thread::Current::Get();
  thread->arch().tpidr_el0 = tpidr_el0;
  thread->arch().tpidrro_el0 = tpidrro_el0;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_arm64_enter_restricted_tpidr(uint64_t tpidr_el0, bool is_arm32) {
  __arm_wsr64("tpidr_el0", tpidr_el0);
  if (is_arm32) {
    __arm_wsr64("tpidrro_el0", tpidr_el0);
  }
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint64_t cpp_arm64_get_tpidr_el0() { return __arm_rsr64("tpidr_el0"); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_arm64_get_general_regs(zx_thread_state_general_regs_t* regs) {
  return arch_get_general_regs(Thread::Current::Get(), regs);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t
cpp_arm64_set_general_regs(const zx_thread_state_general_regs_t* regs) {
  return arch_set_general_regs(Thread::Current::Get(), regs);
}

}  // extern "C"
