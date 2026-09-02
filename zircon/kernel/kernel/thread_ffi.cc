// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <assert.h>
#include <lib/kconcurrent/chainlock.h>
#include <lib/kconcurrent/chainlock_transaction.h>
#include <zircon/types.h>

#include <arch/regs.h>
#include <kernel/deadline.h>
#include <kernel/ffi.h>
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

Thread* cpp_thread_create_default(const char* name, thread_start_routine entry, void* arg);
void cpp_thread_resume(Thread* thread);
zx_status_t cpp_thread_join(Thread* thread, int* out_retcode, zx_instant_mono_t deadline);
void cpp_thread_current_yield();
void cpp_thread_kill(Thread* thread);
zx_status_t cpp_thread_suspend(Thread* thread);
bool cpp_thread_is_blocked(Thread* thread);
Thread* cpp_thread_current_get();
FxtRef cpp_thread_fxt_ref(Thread* thread);
bool cpp_thread_preempt_set_timeslice_extension(zx_duration_mono_t duration);
void cpp_thread_preempt_clear_timeslice_extension();
void cpp_thread_preempt_disable();
void cpp_thread_preempt_enable();
void cpp_thread_preempt();
zx_status_t cpp_thread_current_sleep_relative(zx_duration_mono_t duration);
zx_status_t cpp_thread_current_sleep_etc(const Deadline* deadline, Interruptible interruptible,
                                         zx_instant_mono_t now);
zx_status_t cpp_thread_current_soft_fault(vaddr_t va, uint flags);
zx_status_t cpp_restricted_enter(uintptr_t vector_table_ptr, uintptr_t context);

void* cpp_thread_get_arch(Thread* thread) TA_NO_THREAD_SAFETY_ANALYSIS;
vaddr_t cpp_thread_get_stack_top(Thread* thread);
vaddr_t cpp_thread_get_shadow_call_base(Thread* thread);
void cpp_thread_dump_current_stack();
bool cpp_thread_is_user_state_saved(Thread* thread);
bool cpp_thread_is_running(const Thread* thread);
const char* cpp_thread_name(const Thread* thread);
void cpp_thread_process_pending_signals(void* frame);
bool cpp_thread_is_in_restricted_mode(Thread* thread);

Thread* cpp_thread_create_default(const char* name, thread_start_routine entry, void* arg) {
  return Thread::Create(name, entry, arg, DEFAULT_PRIORITY);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_resume(Thread* thread) {
  DEBUG_ASSERT(thread != nullptr);
  thread->Resume();
}

zx_status_t cpp_thread_join(Thread* thread, int* out_retcode, zx_instant_mono_t deadline) {
  DEBUG_ASSERT(thread != nullptr);
  return thread->Join(out_retcode, deadline);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_current_yield() { Thread::Current::Yield(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_kill(Thread* thread) {
  DEBUG_ASSERT(thread != nullptr);
  thread->Kill();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_thread_suspend(Thread* thread) {
  DEBUG_ASSERT(thread != nullptr);
  return thread->Suspend();
}

bool cpp_thread_is_blocked(Thread* thread) {
  DEBUG_ASSERT(thread != nullptr);
  SingleChainLockGuard guard{IrqSaveOption, thread->get_lock(), CLT_TAG("cpp_thread_is_blocked")};
  return thread->state() == THREAD_BLOCKED || thread->state() == THREAD_BLOCKED_READ_LOCK;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE Thread* cpp_thread_current_get() { return Thread::Current::Get(); }

FxtRef cpp_thread_fxt_ref(Thread* thread) {
  DEBUG_ASSERT(thread != nullptr);
  fxt::ThreadRef ref = thread->fxt_ref();
  return {.pid = ref.process().koid, .tid = ref.thread().koid};
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_thread_preempt_set_timeslice_extension(zx_duration_mono_t duration) {
  return Thread::Current::preemption_state().SetTimesliceExtension(duration);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_preempt_clear_timeslice_extension() {
  Thread::Current::preemption_state().ClearTimesliceExtension();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_preempt_disable() {
  Thread::Current::preemption_state().PreemptDisable();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_preempt_enable() {
  Thread::Current::preemption_state().PreemptReenable();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_preempt() { Thread::Current::Preempt(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_thread_current_sleep_relative(zx_duration_mono_t duration) {
  return Thread::Current::SleepRelative(duration);
}

zx_status_t cpp_thread_current_sleep_etc(const Deadline* deadline, Interruptible interruptible,
                                         zx_instant_mono_t now) {
  DEBUG_ASSERT(deadline != nullptr);
  return Thread::Current::SleepEtc(*deadline, interruptible, now);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_thread_current_soft_fault(vaddr_t va, uint flags) {
  return Thread::Current::SoftFault(va, flags);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE vaddr_t cpp_thread_get_stack_top(Thread* thread) { return thread->stack().top(); }

// TODO(https://fxbug.dev/537458631): Remove the annotation once cross-language
// inlining works.  This one matters: it is on the context switch path.
FFI_ALWAYS_INLINE void* cpp_thread_get_arch(Thread* thread) TA_NO_THREAD_SAFETY_ANALYSIS {
  return &thread->arch();
}

vaddr_t cpp_thread_get_shadow_call_base(Thread* thread) {
#if __has_feature(shadow_call_stack)
  return thread->stack().shadow_call_base();
#else
  return 0;
#endif
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_dump_current_stack() {
  Thread::Current::Get()->stack().DumpInfo(CRITICAL);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_thread_is_user_state_saved(Thread* thread) TA_NO_THREAD_SAFETY_ANALYSIS {
  return thread->IsUserStateSavedLocked();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_thread_is_running(const Thread* thread) TA_NO_THREAD_SAFETY_ANALYSIS {
  return thread->state() == THREAD_RUNNING;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const char* cpp_thread_name(const Thread* thread) TA_NO_THREAD_SAFETY_ANALYSIS {
  return thread->name();
}

void cpp_thread_process_pending_signals(void* frame) {
  Thread::Current::ProcessPendingSignals(GeneralRegsSource::Iframe, static_cast<iframe_t*>(frame));
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE RestrictedState* cpp_thread_current_restricted_state() {
  return Thread::Current::restricted_state();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_current_set_restricted_state(RestrictedState* raw_rs) {
  Thread::Current::Get()->set_restricted_state(raw_rs);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_thread_current_is_signaled() {
  return Thread::Current::Get()->IsSignaled();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_thread_current_check_for_restricted_kick() {
  return Thread::Current::CheckForRestrictedKick();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_thread_is_in_restricted_mode(Thread* thread) {
  DEBUG_ASSERT(thread != nullptr);
  return thread->in_restricted();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE VmAspace* cpp_thread_current_active_aspace() {
  return Thread::Current::active_aspace();
}

}  // extern "C"
