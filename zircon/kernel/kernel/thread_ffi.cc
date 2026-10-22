// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <assert.h>
#include <lib/fxt/interned_string.h>
#include <lib/kconcurrent/chainlock.h>
#include <lib/kconcurrent/chainlock_transaction.h>
#include <lib/stall.h>
#include <zircon/types.h>

#include <arch/regs.h>
#include <kernel/auto_preempt_disabler.h>
#include <kernel/deadline.h>
#include <kernel/ffi.h>
#include <kernel/mp.h>
#include <kernel/restricted.h>
#include <kernel/restricted_state.h>
#include <kernel/scheduler.h>
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

// `cpp_thread_get_lock` hands the C++ `ChainLock` to Rust as a `kconcurrent::ChainLock`, which is
// a 16-byte pair of atomics (lock token and contention state) with 8-byte alignment.
static_assert(sizeof(ChainLock) == 16);
static_assert(alignof(ChainLock) == 8);

static_assert(sizeof(SchedulerState::WaitQueueInheritedSchedulerState) == 64,
              "WaitQueueInheritedSchedulerState size mismatch");
static_assert(alignof(SchedulerState::WaitQueueInheritedSchedulerState) == 8,
              "WaitQueueInheritedSchedulerState alignment mismatch");
static_assert(offsetof(SchedulerState::WaitQueueInheritedSchedulerState, ipvs) == 0,
              "WaitQueueInheritedSchedulerState ipvs offset mismatch");
static_assert(offsetof(SchedulerState::WaitQueueInheritedSchedulerState, start_time) == 32,
              "WaitQueueInheritedSchedulerState start_time offset mismatch");
static_assert(offsetof(SchedulerState::WaitQueueInheritedSchedulerState, finish_time) == 40,
              "WaitQueueInheritedSchedulerState finish_time offset mismatch");
static_assert(offsetof(SchedulerState::WaitQueueInheritedSchedulerState, time_slice_ns) == 48,
              "WaitQueueInheritedSchedulerState time_slice_ns offset mismatch");
static_assert(offsetof(SchedulerState::WaitQueueInheritedSchedulerState, time_slice_used_ns) == 56,
              "WaitQueueInheritedSchedulerState time_slice_used_ns offset mismatch");

ThreadStallState cpp_thread_memory_stall_state(const Thread* thread);
void cpp_thread_set_memory_stall_state(Thread* thread, ThreadStallState value);
const SchedulerState* cpp_thread_scheduler_state(const Thread* thread);

Thread* cpp_thread_create_default(const char* name, thread_start_routine entry, void* arg);
Thread* cpp_thread_create_with_priority(const char* name, thread_start_routine entry, void* arg,
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
void cpp_thread_preempt_set_pending();
bool cpp_thread_preempt_is_enabled();
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
void cpp_thread_preempts_pending_add(Thread* thread, cpu_mask_t mask);
cpu_mask_t cpp_thread_preempts_pending(const Thread* thread);
void cpp_thread_preempts_pending_clear(Thread* thread);
bool cpp_thread_preempt_evaluate_timeslice_extension(Thread* thread);
uint32_t cpp_thread_preempt_eager_resched_disable_count(const Thread* thread);
void cpp_thread_set_ready(Thread* thread) TA_NO_THREAD_SAFETY_ANALYSIS;
Thread* cpp_unblock_list_pop_back(Thread::UnblockList* list);

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
FFI_ALWAYS_INLINE void cpp_scheduler_state_base_profile_init_deadline(
    ffi::Uninitialized<SchedulerState::BaseProfile>* out_profile, zx_duration_mono_t capacity_ns,
    zx_duration_mono_t deadline_ns) {
  out_profile->Initialize(SchedulerState::BaseProfile{
      SchedDeadlineParams{SchedDuration{capacity_ns}, SchedDuration{deadline_ns}}});
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_set_base_profile(Thread* thread,
                                                   const SchedulerState::BaseProfile* profile) {
  DEBUG_ASSERT(thread != nullptr);
  DEBUG_ASSERT(profile != nullptr);
  thread->SetBaseProfile(*profile);
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

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_thread_restricted_kick(Thread* thread) {
  DEBUG_ASSERT(thread != nullptr);
  return thread->RestrictedKick();
}

// Starts memory stall accounting for |thread| by moving it from the IgnoredKernelOnly stall
// state into Inactive. Preemption is disabled here, inside the shim, so that Clang can verify
// the preempt_disabled_token requirement of the accessors.
//
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_set_memory_stall_state_inactive(Thread* thread) {
  DEBUG_ASSERT(thread != nullptr);
  AnnotatedAutoPreemptDisabler preempt_disabler;
  DEBUG_ASSERT(thread->memory_stall_state() == ThreadStallState::IgnoredKernelOnly);
  thread->set_memory_stall_state(ThreadStallState::Inactive);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_current_exit(int retcode) { Thread::Current::Exit(retcode); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_current_kill() { Thread::Current::Kill(); }

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
FFI_ALWAYS_INLINE void cpp_thread_preempt_set_pending() {
  Thread::Current::preemption_state().PreemptSetPending();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_thread_preempt_is_enabled() {
  return Thread::Current::preemption_state().PreemptIsEnabled();
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

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE ChainLock* cpp_thread_get_lock(Thread* thread) TA_NO_THREAD_SAFETY_ANALYSIS {
  return &thread->get_lock();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_thread_wait_queue_state_blocked_status_locked(
    const Thread* thread) TA_NO_THREAD_SAFETY_ANALYSIS {
  return thread->wait_queue_state().BlockedStatus();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE ThreadDispatcher* cpp_thread_user_thread(Thread* thread) {
  return thread->user_thread();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_koid_t cpp_thread_tid(const Thread* thread) { return thread->tid(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint64_t cpp_thread_lock_flow_id(const Thread* thread) {
  return thread->lock_flow_id();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint64_t cpp_thread_take_next_lock_flow_id(Thread* thread) {
  return thread->TakeNextLockFlowId();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_preempts_pending_add(Thread* thread, cpu_mask_t mask) {
  thread->preemption_state().preempts_pending_add(mask);
}
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE cpu_mask_t cpp_thread_preempts_pending(const Thread* thread) {
  return thread->preemption_state().preempts_pending();
}
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_preempts_pending_clear(Thread* thread) {
  thread->preemption_state().preempts_pending_clear();
}
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_thread_preempt_evaluate_timeslice_extension(Thread* thread) {
  return thread->preemption_state().EvaluateTimesliceExtension();
}
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint32_t cpp_thread_preempt_eager_resched_disable_count(const Thread* thread) {
  return thread->preemption_state().EagerReschedDisableCount();
}
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_set_ready(Thread* thread) TA_NO_THREAD_SAFETY_ANALYSIS {
  thread->set_ready();
}
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE Thread* cpp_unblock_list_pop_back(Thread::UnblockList* list) {
  return list->pop_back();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void* cpp_thread_get_queue_node(Thread* t) TA_NO_THREAD_SAFETY_ANALYSIS {
  return &t->scheduler_queue_state().run_queue_node();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE SchedulerState* cpp_thread_get_scheduler_state(Thread* t)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return &t->scheduler_state();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE SchedulerState::WaitQueueInheritedSchedulerState* cpp_owq_get_inherited_state(
    OwnedWaitQueue* owq) TA_NO_THREAD_SAFETY_ANALYSIS {
  return owq->inherited_scheduler_state_storage();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const SchedulerState::WaitQueueInheritedSchedulerState*
cpp_owq_get_inherited_state_const(const OwnedWaitQueue* owq) TA_NO_THREAD_SAFETY_ANALYSIS {
  return owq->inherited_scheduler_state_storage();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE SchedUtilization cpp_scheduler_get_normalized_utilization(const Scheduler* sched)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return sched->normalized_utilization();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_thread_is_idle(const Thread* thread) TA_NO_THREAD_SAFETY_ANALYSIS {
  return thread->IsIdle();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_thread_has_migrate_fn(const Thread* thread)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return thread->has_migrate_fn();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_thread_migrate_pending(const Thread* thread)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return thread->migrate_pending();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_stall_accumulator_apply_context_switch(
    Thread* current_thread, Thread* next_thread) TA_NO_THREAD_SAFETY_ANALYSIS {
  StallAccumulator::ApplyContextSwitch(current_thread, next_thread);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_get_backtrace(Thread* thread, Backtrace* out_bt) {
  thread->GetBacktrace(*out_bt);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_dump(const Thread* thread, bool full) { thread->Dump(full); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_duration_mono_t cpp_thread_runtime(const Thread* thread) {
  return thread->Runtime();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint32_t cpp_thread_signals(const Thread* thread) {
  DEBUG_ASSERT(thread != nullptr);
  return thread->signals();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_thread_check_kill_or_suspend_signal(const Thread* thread) {
  DEBUG_ASSERT(thread != nullptr);
  return thread->CheckKillOrSuspendSignal();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE thread_state cpp_thread_state(const Thread* thread) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(thread != nullptr);
  return thread->state();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_set_blocked(Thread* thread) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(thread != nullptr);
  thread->set_blocked();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_set_blocked_read_lock(Thread* thread)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(thread != nullptr);
  thread->set_blocked_read_lock();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const WaitQueueCollection::ThreadState* cpp_thread_wait_queue_state(
    const Thread* thread) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(thread != nullptr);
  return &thread->wait_queue_state();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void* cpp_thread_get_list_lock() TA_NO_THREAD_SAFETY_ANALYSIS {
  return &Thread::get_list_lock();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE Thread* cpp_thread_id_to_thread_slow(zx_koid_t tid) TA_NO_THREAD_SAFETY_ANALYSIS {
  return thread_id_to_thread_slow(tid);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_dump_tid(zx_koid_t tid, bool full) { Thread::DumpTid(tid, full); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_dump_tid_during_panic(zx_koid_t tid,
                                                        bool full) TA_NO_THREAD_SAFETY_ANALYSIS {
  Thread::DumpTidDuringPanic(tid, full);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_dump_all(const char* match_owner, bool full) {
  Thread::DumpAll(match_owner, full);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_dump_all_during_panic(const char* match_owner,
                                                        bool full) TA_NO_THREAD_SAFETY_ANALYSIS {
  Thread::DumpAllDuringPanic(match_owner, full);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_current_reschedule() { Thread::Current::Reschedule(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE fbl::DoublyLinkedList<OwnedWaitQueue*>* cpp_thread_owned_wait_queues(
    Thread* thread) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(thread != nullptr);
  return &thread->owned_wait_queues_;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_thread_can_own_wait_queues(const Thread* thread)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(thread != nullptr);
  return thread->can_own_wait_queues_;
}

}  // extern "C"
