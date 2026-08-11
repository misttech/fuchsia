// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <assert.h>
#include <lib/kconcurrent/chainlock.h>
#include <lib/kconcurrent/chainlock_transaction.h>
#include <zircon/types.h>

#include <kernel/restricted.h>
#include <kernel/restricted_state.h>
#include <kernel/thread.h>
#include <vm/vm_object_paged.h>

extern "C" {

// LINT.IfChange(FxtRef)
struct FxtRef {
  uint64_t pid;
  uint64_t tid;
};
// LINT.ThenChange(//zircon/kernel/kernel/thread.rs:FxtRef)

void* cpp_thread_create_default(const char* name, thread_start_routine entry, void* arg);
void cpp_thread_resume(void* thread);
zx_status_t cpp_thread_join(void* thread, int* out_retcode, zx_instant_mono_t deadline);
void cpp_thread_current_yield();
void cpp_thread_kill(void* thread);
bool cpp_thread_is_blocked(void* thread);
void* cpp_thread_current_get();
FxtRef cpp_thread_fxt_ref(void* thread);
bool cpp_thread_preempt_set_timeslice_extension(zx_duration_mono_t duration);
void cpp_thread_preempt_clear_timeslice_extension();
void cpp_thread_preempt_disable();
void cpp_thread_preempt_enable();
zx_status_t cpp_thread_current_sleep_relative(zx_duration_mono_t duration);
zx_status_t cpp_thread_current_soft_fault(vaddr_t va, uint flags);
zx_status_t cpp_restricted_enter(uintptr_t vector_table_ptr, uintptr_t context);

void* cpp_thread_create_default(const char* name, thread_start_routine entry, void* arg) {
  return Thread::Create(name, entry, arg, DEFAULT_PRIORITY);
}

void cpp_thread_resume(void* thread) {
  DEBUG_ASSERT(thread != nullptr);
  static_cast<Thread*>(thread)->Resume();
}

zx_status_t cpp_thread_join(void* thread, int* out_retcode, zx_instant_mono_t deadline) {
  DEBUG_ASSERT(thread != nullptr);
  return static_cast<Thread*>(thread)->Join(out_retcode, deadline);
}

void cpp_thread_current_yield() { Thread::Current::Yield(); }

void cpp_thread_kill(void* thread) {
  DEBUG_ASSERT(thread != nullptr);
  static_cast<Thread*>(thread)->Kill();
}

bool cpp_thread_is_blocked(void* thread) {
  DEBUG_ASSERT(thread != nullptr);
  Thread* t = static_cast<Thread*>(thread);
  SingleChainLockGuard guard{IrqSaveOption, t->get_lock(), CLT_TAG("cpp_thread_is_blocked")};
  return t->state() == THREAD_BLOCKED || t->state() == THREAD_BLOCKED_READ_LOCK;
}

void* cpp_thread_current_get() { return Thread::Current::Get(); }

FxtRef cpp_thread_fxt_ref(void* thread) {
  DEBUG_ASSERT(thread != nullptr);
  Thread* t = static_cast<Thread*>(thread);
  fxt::ThreadRef ref = t->fxt_ref();
  return {.pid = ref.process().koid, .tid = ref.thread().koid};
}

bool cpp_thread_preempt_set_timeslice_extension(zx_duration_mono_t duration) {
  return Thread::Current::preemption_state().SetTimesliceExtension(duration);
}

void cpp_thread_preempt_clear_timeslice_extension() {
  Thread::Current::preemption_state().ClearTimesliceExtension();
}

void cpp_thread_preempt_disable() { Thread::Current::preemption_state().PreemptDisable(); }

void cpp_thread_preempt_enable() { Thread::Current::preemption_state().PreemptReenable(); }

zx_status_t cpp_thread_current_sleep_relative(zx_duration_mono_t duration) {
  return Thread::Current::SleepRelative(duration);
}

zx_status_t cpp_thread_current_soft_fault(vaddr_t va, uint flags) {
  return Thread::Current::SoftFault(va, flags);
}

zx_status_t cpp_restricted_enter(uintptr_t vector_table_ptr, uintptr_t context) {
  return RestrictedEnter(vector_table_ptr, context);
}

}  // extern "C"
