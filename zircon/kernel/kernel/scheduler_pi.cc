// Copyright 2022 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <kernel/ffi.h>
#include <kernel/owned_wait_queue.h>
#include <kernel/scheduler.h>
#include <ktl/memory.h>
#include <ktl/type_traits.h>

extern "C" {

// Helper FFI functions implemented in C++
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE WaitQueueBase* cpp_thread_get_blocking_wait_queue(const Thread* thread) {
  return Scheduler::GetBlockingWaitQueue(thread);
}
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_wait_queue_update_blocked_thread_effective_profile(WaitQueueBase* wq,
                                                                              Thread* thread) {
  Scheduler::UpdateBlockedThreadEffectiveProfile(wq, thread);
}
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_current_mono_time_and_boot_ticks(
    Scheduler::SchedMonoTimeAndBootTicks* out) {
  *out = Scheduler::CurrentMonoTimeAndBootTicks();
}
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_call_migrate_fn_locked(Thread* thread, int32_t stage)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  thread->CallMigrateFnLocked(static_cast<Thread::MigrateStage>(stage));
}
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const ChainLock* cpp_wait_queue_get_lock(const WaitQueueBase* wq)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return &wq->get_lock();
}
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_scheduler_save_state_list_push_front(Scheduler* sched, Thread* thread)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  sched->save_state_list_.push_front(thread);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_unblock_list_init(ffi::Uninitialized<Thread::UnblockList>* holder) {
  holder->Initialize();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_unblock_list_destroy(Thread::UnblockList* holder) {
  ktl::destroy_at(holder);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_unblock_list_push_back(Thread::UnblockList* holder, Thread* thread) {
  holder->push_back(thread);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE Thread* cpp_unblock_list_pop_front(Thread::UnblockList* holder) {
  return holder->pop_front();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_unblock_list_is_empty(const Thread::UnblockList* holder) {
  return holder->is_empty();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_scheduler_save_state_list_is_empty(Scheduler* sched)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return sched->save_state_list_.is_empty();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_scheduler_save_state_list_move(
    Scheduler* sched,
    ffi::Uninitialized<Thread::SaveStateList>* holder) TA_NO_THREAD_SAFETY_ANALYSIS {
  holder->Initialize(ktl::move(sched->save_state_list_));
}

// Moves the scheduler's save_state_list_ into |holder| under queue_lock_ if it
// is non-empty. Returns true if |holder| was initialized with the list.
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_scheduler_take_save_state_list(
    Scheduler* sched,
    ffi::Uninitialized<Thread::SaveStateList>* holder) TA_NO_THREAD_SAFETY_ANALYSIS {
  Guard<MonitoredSpinLock, NoIrqSave> queue_guard{&sched->queue_lock_, SOURCE_TAG};
  if (sched->save_state_list_.is_empty()) {
    return false;
  }
  holder->Initialize(ktl::move(sched->save_state_list_));
  return true;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE Thread* cpp_save_state_list_pop(Thread::SaveStateList* holder) {
  return holder->pop_front();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_save_state_list_destroy(Thread::SaveStateList* holder) {
  ktl::destroy_at(holder);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_call_migrate_fn_for_cpu(cpu_num_t cpu) {
  Thread::CallMigrateFnForCpu(cpu);
}

}  // extern "C"

void Scheduler::ThreadBaseProfileChanged(Thread& thread) {
  rust_kernel_scheduler_pi_thread_base_profile_changed(&thread);
}

template <>
void Scheduler::UpstreamThreadBaseProfileChanged<Thread>(const Thread& upstream, Thread& target) {
  rust_kernel_scheduler_pi_upstream_thread_base_profile_changed_thread(&upstream, &target);
}
template <>
void Scheduler::UpstreamThreadBaseProfileChanged<OwnedWaitQueue>(const Thread& upstream,
                                                                 OwnedWaitQueue& target) {
  rust_kernel_scheduler_pi_upstream_thread_base_profile_changed_owq(&upstream, &target);
}

template <>
void Scheduler::JoinNodeToPiGraph<Thread, Thread>(const Thread& upstream, Thread& target,
                                                  ForceInheritance force_inheritance) {
  rust_kernel_scheduler_pi_join_thread_thread(&upstream, &target, force_inheritance);
}
template <>
void Scheduler::JoinNodeToPiGraph<Thread, OwnedWaitQueue>(const Thread& upstream,
                                                          OwnedWaitQueue& target,
                                                          ForceInheritance force_inheritance) {
  rust_kernel_scheduler_pi_join_thread_owq(&upstream, &target, force_inheritance);
}
template <>
void Scheduler::JoinNodeToPiGraph<OwnedWaitQueue, Thread>(const OwnedWaitQueue& upstream,
                                                          Thread& target,
                                                          ForceInheritance force_inheritance) {
  rust_kernel_scheduler_pi_join_owq_thread(&upstream, &target, force_inheritance);
}
template <>
void Scheduler::JoinNodeToPiGraph<OwnedWaitQueue, OwnedWaitQueue>(
    const OwnedWaitQueue& upstream, OwnedWaitQueue& target, ForceInheritance force_inheritance) {
  rust_kernel_scheduler_pi_join_owq_owq(&upstream, &target, force_inheritance);
}

template <>
void Scheduler::SplitNodeFromPiGraph<Thread, Thread>(
    Thread& upstream, Thread& target, const SchedulerState::EffectiveProfile* old_target_ep) {
  // A target thread stores its effective profile prior to recomputing it, so
  // only OwnedWaitQueue targets supply the old effective profile.
  DEBUG_ASSERT(old_target_ep == nullptr);
  rust_kernel_scheduler_pi_split_thread_thread(&upstream, &target);
}
template <>
void Scheduler::SplitNodeFromPiGraph<Thread, OwnedWaitQueue>(
    Thread& upstream, OwnedWaitQueue& target,
    const SchedulerState::EffectiveProfile* old_target_ep) {
  rust_kernel_scheduler_pi_split_thread_owq(&upstream, &target, old_target_ep);
}
template <>
void Scheduler::SplitNodeFromPiGraph<OwnedWaitQueue, Thread>(
    OwnedWaitQueue& upstream, Thread& target,
    const SchedulerState::EffectiveProfile* old_target_ep) {
  DEBUG_ASSERT(old_target_ep == nullptr);
  rust_kernel_scheduler_pi_split_owq_thread(&upstream, &target);
}
template <>
void Scheduler::SplitNodeFromPiGraph<OwnedWaitQueue, OwnedWaitQueue>(
    OwnedWaitQueue& upstream, OwnedWaitQueue& target,
    const SchedulerState::EffectiveProfile* old_target_ep) {
  rust_kernel_scheduler_pi_split_owq_owq(&upstream, &target, old_target_ep);
}

static_assert(sizeof(Thread::SaveStateList) == 8);
static_assert(alignof(Thread::SaveStateList) == 8);
static_assert(sizeof(Thread::UnblockList) == 8);
static_assert(alignof(Thread::UnblockList) == 8);
