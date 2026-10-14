// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <assert.h>
#include <lib/fxt/interned_string.h>
#include <lib/kconcurrent/chainlock.h>
#include <lib/kconcurrent/chainlock_transaction.h>
#include <zircon/types.h>

#include <arch/regs.h>
#include <kernel/deadline.h>
#include <kernel/ffi.h>
#include <kernel/restricted.h>
#include <kernel/restricted_state.h>
#include <kernel/scheduler_state.h>
#include <kernel/thread.h>
#include <kernel/thread_ffi.h>
#include <ktl/memory.h>
#include <ktl/string_view.h>
#include <vm/vm_object_paged.h>

extern "C" {

// LINT.IfChange(FxtRef)
struct FxtRef {
  uint64_t pid;
  uint64_t tid;
};
// LINT.ThenChange(//zircon/kernel/kernel/thread.rs:FxtRef)

// LINT.IfChange(thread_state)
// Keep `enum thread_state` in sync with Rust's `ThreadStateKind`.
static_assert(sizeof(thread_state) == 1);
static_assert(THREAD_INITIAL == 0);
static_assert(THREAD_READY == 1);
static_assert(THREAD_RUNNING == 2);
static_assert(THREAD_BLOCKED == 3);
static_assert(THREAD_BLOCKED_READ_LOCK == 4);
static_assert(THREAD_SLEEPING == 5);
static_assert(THREAD_SUSPENDED == 6);
static_assert(THREAD_DEATH == 7);
// LINT.ThenChange(//zircon/kernel/kernel/scheduler_state.rs:ThreadStateKind)

// LINT.IfChange(ThreadStallState)
// Keep `enum class ThreadStallState` in sync with Rust's `ThreadStallState`. It has no fixed
// underlying type, so it is an `int`, which the Rust mirror matches with #[repr(i32)].
static_assert(sizeof(ThreadStallState) == 4);
static_assert(static_cast<int>(ThreadStallState::IgnoredKernelOnly) == 0);
static_assert(static_cast<int>(ThreadStallState::Inactive) == 1);
static_assert(static_cast<int>(ThreadStallState::Progressing) == 2);
static_assert(static_cast<int>(ThreadStallState::Stalling) == 3);
// LINT.ThenChange(//zircon/kernel/kernel/thread.rs:ThreadStallState)

ThreadStallState cpp_thread_memory_stall_state(const Thread* thread);
void cpp_thread_set_memory_stall_state(Thread* thread, ThreadStallState value);
const SchedulerState* cpp_thread_scheduler_state(const Thread* thread);

Thread* cpp_thread_create_default(const char* name, thread_start_routine entry, void* arg);
FFI_ALWAYS_INLINE Thread* cpp_thread_create_with_priority(const char* name,
                                                          thread_start_routine entry, void* arg,
                                                          int priority);
void cpp_scheduler_state_base_profile_init_fair(
    ffi::Uninitialized<SchedulerState::BaseProfile>* out_profile, int priority, bool inheritable);
Thread* cpp_thread_create_with_profile(const char* name_ptr, size_t name_len,
                                       thread_start_routine entry, void* arg,
                                       const SchedulerState::BaseProfile* profile);
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
void cpp_thread_preempt_reenable();
void cpp_thread_eager_resched_disable();
void cpp_thread_eager_resched_reenable();
uint32_t cpp_thread_preempt_disable_count();
uint32_t cpp_thread_eager_resched_disable_count();
void cpp_thread_preempt();
zx_status_t cpp_thread_current_sleep_etc(const Deadline* deadline, Interruptible interruptible,
                                         zx_instant_mono_t now);
zx_status_t cpp_thread_current_sleep(zx_instant_mono_t deadline);
zx_status_t cpp_thread_current_sleep_relative(zx_duration_mono_t duration);
zx_status_t cpp_thread_current_sleep_interruptible(zx_instant_mono_t deadline);
zx_status_t cpp_thread_current_soft_fault(vaddr_t va, uint flags);

void* cpp_thread_get_arch(Thread* thread) TA_NO_THREAD_SAFETY_ANALYSIS;
vaddr_t cpp_thread_get_stack_top(Thread* thread);
vaddr_t cpp_thread_get_shadow_call_base(Thread* thread);
void cpp_thread_dump_current_stack();
bool cpp_thread_is_user_state_saved_locked(Thread* thread);
bool cpp_thread_is_running(const Thread* thread);
const char* cpp_thread_name(const Thread* thread);
bool cpp_thread_is_user_thread(const Thread* thread);
VmAspace* cpp_thread_active_aspace(Thread* thread);
void cpp_thread_process_pending_signals(void* frame);
VmAspace* cpp_thread_current_active_aspace();

void cpp_thread_current_memory_allocation_state_enable();
void cpp_thread_current_memory_allocation_state_disable();
bool cpp_thread_current_memory_allocation_state_is_enabled();
void cpp_thread_current_signal_policy_exception(uint32_t policy_exception_code,
                                                uint32_t policy_exception_data);

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE Thread* cpp_thread_create_default(const char* name, thread_start_routine entry,
                                                    void* arg) {
  return Thread::Create(name, entry, arg, DEFAULT_PRIORITY);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE Thread* cpp_thread_create_with_priority(const char* name,
                                                          thread_start_routine entry, void* arg,
                                                          int priority) {
  return Thread::Create(name, entry, arg, priority);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_scheduler_state_base_profile_init_fair(
    ffi::Uninitialized<SchedulerState::BaseProfile>* out_profile, int priority, bool inheritable) {
  out_profile->Initialize(SchedulerState::BaseProfile{priority, inheritable});
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE Thread* cpp_thread_create_with_profile(
    const char* name_ptr, size_t name_len, thread_start_routine entry, void* arg,
    const SchedulerState::BaseProfile* profile) {
  DEBUG_ASSERT(profile != nullptr);
  return Thread::Create(ktl::string_view{name_ptr, name_len}, entry, arg, *profile);
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

static_assert(sizeof(ffi::Uninitialized<ThreadLockGuard>) == sizeof(ThreadLockGuard));
static_assert(alignof(ffi::Uninitialized<ThreadLockGuard>) == alignof(ThreadLockGuard));

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_lock_guard_init(ffi::Uninitialized<ThreadLockGuard>* guard,
                                                  Thread* thread, const fxt::InternedString* label,
                                                  uint32_t line) {
  DEBUG_ASSERT(guard != nullptr);
  DEBUG_ASSERT(thread != nullptr);
  guard->Initialize(thread, ChainLockTransaction::CallsiteInfo{label, line});
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_lock_guard_destroy(ThreadLockGuard* guard) {
  DEBUG_ASSERT(guard != nullptr);
  ktl::destroy_at(guard);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE Thread* cpp_thread_current_get() { return Thread::Current::Get(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE FxtRef cpp_thread_fxt_ref(Thread* thread) {
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
FFI_ALWAYS_INLINE void cpp_thread_preempt_reenable() {
  Thread::Current::preemption_state().PreemptReenable();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_eager_resched_disable() {
  Thread::Current::preemption_state().EagerReschedDisable();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_eager_resched_reenable() {
  Thread::Current::preemption_state().EagerReschedReenable();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint32_t cpp_thread_preempt_disable_count() {
  return Thread::Current::preemption_state().PreemptDisableCount();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint32_t cpp_thread_eager_resched_disable_count() {
  return Thread::Current::preemption_state().EagerReschedDisableCount();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_preempt() { Thread::Current::Preempt(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_thread_current_sleep_etc(const Deadline* deadline,
                                                           Interruptible interruptible,
                                                           zx_instant_mono_t now) {
  DEBUG_ASSERT(deadline != nullptr);
  return Thread::Current::SleepEtc(*deadline, interruptible, now);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_thread_current_sleep(zx_instant_mono_t deadline) {
  return Thread::Current::Sleep(deadline);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_thread_current_sleep_relative(zx_duration_mono_t duration) {
  return Thread::Current::SleepRelative(duration);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_thread_current_sleep_interruptible(zx_instant_mono_t deadline) {
  return Thread::Current::SleepInterruptible(deadline);
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

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE vaddr_t cpp_thread_get_shadow_call_base(Thread* thread) {
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
FFI_ALWAYS_INLINE bool cpp_thread_is_user_state_saved_locked(Thread* thread)
    TA_NO_THREAD_SAFETY_ANALYSIS {
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

// A thread's memory_stall_state is guarded by preempt_disabled_token. The accessors below drop
// thread safety analysis because the scheduler, their only caller, does not formally disable
// preemption when it switches threads, but in fact it is disabled, because the scheduler does not
// preempt itself.
//
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE ThreadStallState cpp_thread_memory_stall_state(const Thread* thread)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return thread->memory_stall_state();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_set_memory_stall_state(Thread* thread, ThreadStallState value)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  thread->set_memory_stall_state(value);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const SchedulerState* cpp_thread_scheduler_state(const Thread* thread)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return &thread->scheduler_state();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_process_pending_signals(void* frame) {
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
FFI_ALWAYS_INLINE bool cpp_thread_in_restricted(Thread* thread) {
  DEBUG_ASSERT(thread != nullptr);
  return thread->in_restricted();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_thread_is_user_thread(const Thread* thread) {
  DEBUG_ASSERT(thread != nullptr);
  return thread->user_thread() != nullptr;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE VmAspace* cpp_thread_active_aspace(Thread* thread) {
  DEBUG_ASSERT(thread != nullptr);
  return thread->active_aspace();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE VmAspace* cpp_thread_current_active_aspace() {
  return Thread::Current::active_aspace();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_current_memory_allocation_state_enable() {
  return Thread::Current::memory_allocation_state().Enable();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_current_memory_allocation_state_disable() {
  return Thread::Current::memory_allocation_state().Disable();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_thread_current_memory_allocation_state_is_enabled() {
  return Thread::Current::memory_allocation_state().IsEnabled();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_current_signal_policy_exception(uint32_t policy_exception_code,
                                                                  uint32_t policy_exception_data) {
  Thread::Current::SignalPolicyException(policy_exception_code, policy_exception_data);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE cpu_mask_t cpp_thread_get_cpu_affinity(const Thread* thread) {
  DEBUG_ASSERT(thread != nullptr);
  return thread->GetCpuAffinity();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE cpu_mask_t cpp_thread_set_cpu_affinity(Thread* thread, cpu_mask_t affinity) {
  DEBUG_ASSERT(thread != nullptr);
  return thread->SetCpuAffinity(affinity);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_current_migrate_to_cpu(cpu_num_t target_cpu) {
  Thread::Current::MigrateToCpu(target_cpu);
}

}  // extern "C"
