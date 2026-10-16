// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_THREAD_FFI_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_THREAD_FFI_H_

#include <lib/fxt/interned_string.h>
#include <lib/kconcurrent/chainlock.h>
#include <lib/kconcurrent/chainlock_transaction.h>
#include <stdint.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <fbl/macros.h>
#include <kernel/ffi.h>
#include <kernel/thread.h>

// A thread's lock, held with interrupts disabled for the life of the object: the
// SingleChainLockGuard{IrqSaveOption, thread->get_lock(), CLT_TAG(...)} that a C++ scope would
// hold, for Rust to keep in pinned storage. The transaction inside records its own address, so the
// object must not move between construction and destruction.
class ThreadLockGuard {
 public:
  ThreadLockGuard(Thread* thread,
                  ChainLockTransaction::CallsiteInfo callsite_info) TA_NO_THREAD_SAFETY_ANALYSIS
      : guard_{IrqSaveOption, thread->get_lock(), callsite_info} {}

  DISALLOW_COPY_ASSIGN_AND_MOVE(ThreadLockGuard);

 private:
  SingleChainLockGuard<ChainLockTransaction::StateOptions::IrqSave> guard_;
};

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
__BEGIN_CDECLS

FFI_ALWAYS_INLINE void cpp_thread_lock_guard_init(ffi::Uninitialized<ThreadLockGuard>* guard,
                                                  Thread* thread, const fxt::InternedString* label,
                                                  uint32_t line);
FFI_ALWAYS_INLINE void cpp_thread_lock_guard_destroy(ThreadLockGuard* guard);
FFI_ALWAYS_INLINE void cpp_thread_current_migrate_to_cpu(cpu_num_t target_cpu);
FFI_ALWAYS_INLINE cpu_mask_t cpp_scheduler_state_hard_affinity(const SchedulerState* state);
FFI_ALWAYS_INLINE bool cpp_thread_preempt_is_enabled();

FFI_ALWAYS_INLINE zx_status_t cpp_thread_restricted_kick(Thread* thread);
FFI_ALWAYS_INLINE void cpp_thread_set_memory_stall_state_inactive(Thread* thread);
FFI_ALWAYS_INLINE void cpp_thread_current_exit(int retcode) __NO_RETURN;
FFI_ALWAYS_INLINE void cpp_thread_current_kill();

__END_CDECLS

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_THREAD_FFI_H_
