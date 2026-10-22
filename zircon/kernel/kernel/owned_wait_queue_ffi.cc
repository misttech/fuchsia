// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <assert.h>

#include <kernel/ffi.h>
#include <kernel/owned_wait_queue.h>
#include <ktl/memory.h>
#include <ktl/utility.h>

namespace {

class RustWakeRequeueHookProxy final : public OwnedWaitQueue::IWakeRequeueHook {
 public:
  explicit RustWakeRequeueHookProxy(void* context) : context_(context) {}

  bool Allow(const Thread& t) override TA_REQ_SHARED(t.get_lock()) {
    return rust_owned_wait_queue_wake_requeue_hook_allow(context_, &t);
  }

  void OnWakeOrRequeue(Thread& t) override TA_REQ(t.get_lock()) {
    rust_owned_wait_queue_wake_requeue_hook_on_wake_or_requeue(context_, &t);
  }

 private:
  void* context_;
};

}  // namespace

extern "C" {

static_assert(sizeof(ffi::Uninitialized<Thread::UnblockList>) == sizeof(Thread::UnblockList));
static_assert(alignof(ffi::Uninitialized<Thread::UnblockList>) == alignof(Thread::UnblockList));
static_assert(sizeof(ffi::Uninitialized<OwnedWaitQueue>) == sizeof(OwnedWaitQueue));
static_assert(alignof(ffi::Uninitialized<OwnedWaitQueue>) == alignof(OwnedWaitQueue));
static_assert(sizeof(ffi::Uninitialized<SchedulerState::EffectiveProfile>) ==
              sizeof(SchedulerState::EffectiveProfile));
static_assert(alignof(ffi::Uninitialized<SchedulerState::EffectiveProfile>) ==
              alignof(SchedulerState::EffectiveProfile));

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_unblock_list_init(ffi::Uninitialized<Thread::UnblockList>* list) {
  DEBUG_ASSERT(list != nullptr);
  list->Initialize();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_unblock_list_destroy(Thread::UnblockList* list) {
  DEBUG_ASSERT(list != nullptr);
  ktl::destroy_at(list);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_owned_wait_queue_init(ffi::Uninitialized<OwnedWaitQueue>* wait_queue) {
  DEBUG_ASSERT(wait_queue != nullptr);
  wait_queue->Initialize();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_owned_wait_queue_disown_all_queues(Thread* t)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(t != nullptr);
  OwnedWaitQueue::DisownAllQueues(t);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_owned_wait_queue_get_effective_profile(
    const OwnedWaitQueue* wait_queue,
    ffi::Uninitialized<SchedulerState::EffectiveProfile>* out_profile)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  DEBUG_ASSERT(out_profile != nullptr);
  out_profile->Initialize(wait_queue->GetEffectiveProfile());
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_owned_wait_queue_set_thread_base_profile_and_propagate(
    Thread* thread, const SchedulerState::BaseProfile* profile) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(thread != nullptr);
  DEBUG_ASSERT(profile != nullptr);
  OwnedWaitQueue::SetThreadBaseProfileAndPropagate(*thread, *profile);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE ChainLock::Result cpp_owned_wait_queue_lock_pi_chain_thread(Thread* start)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(start != nullptr);
  return OwnedWaitQueue::LockPiChain(*start);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE ChainLock::Result cpp_owned_wait_queue_lock_pi_chain_queue(WaitQueueBase* start)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(start != nullptr);
  return OwnedWaitQueue::LockPiChain(*start);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_owned_wait_queue_assert_not_owned(const OwnedWaitQueue* wait_queue) {
  DEBUG_ASSERT(wait_queue != nullptr);
  wait_queue->AssertNotOwned();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_owned_wait_queue_assign_owner(
    OwnedWaitQueue* wait_queue, Thread* new_owner) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  return wait_queue->AssignOwner(new_owner);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_owned_wait_queue_assign_owner_locked(
    OwnedWaitQueue* wait_queue, Thread* new_owner) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  DEBUG_ASSERT(new_owner != nullptr);
  return wait_queue->AssignOwnerLocked(new_owner);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_owned_wait_queue_reset_owner_if_no_waiters(OwnedWaitQueue* wait_queue)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  wait_queue->ResetOwnerIfNoWaiters();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_owned_wait_queue_assert_safe_for_pooling(
    const OwnedWaitQueue* wait_queue) {
  DEBUG_ASSERT(wait_queue != nullptr);
  wait_queue->AssertSafeForPooling();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_owned_wait_queue_block_and_assign_owner(
    OwnedWaitQueue* wait_queue, const Deadline* deadline, Thread* new_owner,
    ResourceOwnership resource_ownership,
    Interruptible interruptible) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  DEBUG_ASSERT(deadline != nullptr);
  return wait_queue->BlockAndAssignOwner(*deadline, new_owner, resource_ownership, interruptible);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_owned_wait_queue_block_and_assign_owner_locked(
    OwnedWaitQueue* wait_queue, Thread* current_thread, const Deadline* deadline,
    const OwnedWaitQueue::BAAOLockingDetails* details, ResourceOwnership resource_ownership,
    Interruptible interruptible, ForceInheritance force_inheritance) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  DEBUG_ASSERT(current_thread != nullptr);
  DEBUG_ASSERT(deadline != nullptr);
  DEBUG_ASSERT(details != nullptr);
  return wait_queue->BlockAndAssignOwnerLocked(
      current_thread, *deadline, *details, resource_ownership, interruptible, force_inheritance);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_owned_wait_queue_cancel_baao_operation_locked(
    OwnedWaitQueue* wait_queue, Thread* current_thread,
    const OwnedWaitQueue::BAAOLockingDetails* details) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  DEBUG_ASSERT(current_thread != nullptr);
  DEBUG_ASSERT(details != nullptr);
  wait_queue->CancelBAAOOperationLocked(current_thread, *details);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_owned_wait_queue_lock_for_baao_operation_or_backoff(
    OwnedWaitQueue* wait_queue, Thread* current_thread, Thread* new_owner,
    OwnedWaitQueue::BAAOLockingDetails* details_out) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  DEBUG_ASSERT(current_thread != nullptr);
  DEBUG_ASSERT(details_out != nullptr);
  return wait_queue->LockForBAAOOperationOrBackoff(current_thread, new_owner, *details_out);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_owned_wait_queue_try_lock_for_baao_operation_locked(
    OwnedWaitQueue* wait_queue, Thread* current_thread, Thread* new_owner,
    OwnedWaitQueue::BAAOLockingDetails* details_out) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  DEBUG_ASSERT(current_thread != nullptr);
  DEBUG_ASSERT(details_out != nullptr);
  return wait_queue->TryLockForBAAOOperationLocked(current_thread, new_owner, *details_out);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_owned_wait_queue_wake_threads(
    OwnedWaitQueue* wait_queue, uint32_t wake_count, void* wake_hooks,
    OwnedWaitQueue::WakeThreadsResult* out_result) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  DEBUG_ASSERT(wake_hooks != nullptr);
  DEBUG_ASSERT(out_result != nullptr);
  RustWakeRequeueHookProxy wake_proxy(wake_hooks);
  *out_result = wait_queue->WakeThreads(wake_count, wake_proxy);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_owned_wait_queue_wake_thread_and_assign_owner(
    OwnedWaitQueue* wait_queue, void* wake_hooks,
    OwnedWaitQueue::WakeThreadsResult* out_result) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  DEBUG_ASSERT(wake_hooks != nullptr);
  DEBUG_ASSERT(out_result != nullptr);
  RustWakeRequeueHookProxy wake_proxy(wake_hooks);
  *out_result = wait_queue->WakeThreadAndAssignOwner(wake_proxy);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_owned_wait_queue_wake_threads_locked(
    OwnedWaitQueue* wait_queue, Thread::UnblockList* threads, void* wake_hooks,
    OwnedWaitQueue::WakeOption option, ForceInheritance force_inheritance,
    OwnedWaitQueue::WakeThreadsResult* out_result) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  DEBUG_ASSERT(threads != nullptr);
  DEBUG_ASSERT(wake_hooks != nullptr);
  DEBUG_ASSERT(out_result != nullptr);
  RustWakeRequeueHookProxy wake_proxy(wake_hooks);
  *out_result =
      wait_queue->WakeThreadsLocked(ktl::move(*threads), wake_proxy, option, force_inheritance);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_owned_wait_queue_lock_for_wake_operation_or_backoff(
    OwnedWaitQueue* wait_queue, uint32_t max_wake, void* wake_hooks,
    Thread::UnblockList* unblock_list_out) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  DEBUG_ASSERT(wake_hooks != nullptr);
  DEBUG_ASSERT(unblock_list_out != nullptr);
  RustWakeRequeueHookProxy wake_proxy(wake_hooks);
  return wait_queue->LockForWakeOperationOrBackoff(max_wake, wake_proxy, *unblock_list_out);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_owned_wait_queue_lock_for_wake_operation_locked(
    OwnedWaitQueue* wait_queue, uint32_t max_wake, void* wake_hooks,
    Thread::UnblockList* unblock_list_out) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  DEBUG_ASSERT(wake_hooks != nullptr);
  DEBUG_ASSERT(unblock_list_out != nullptr);
  RustWakeRequeueHookProxy wake_proxy(wake_hooks);
  ktl::optional<Thread::UnblockList> res =
      wait_queue->LockForWakeOperationLocked(max_wake, wake_proxy);
  if (!res.has_value()) {
    return false;
  }
  *unblock_list_out = ktl::move(*res);
  return true;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_owned_wait_queue_wake_and_requeue(
    OwnedWaitQueue* wait_queue, OwnedWaitQueue* requeue_target, Thread* new_requeue_owner,
    uint32_t wake_count, uint32_t requeue_count, void* wake_hooks, void* requeue_hooks,
    OwnedWaitQueue::WakeOption wake_option,
    OwnedWaitQueue::WakeThreadsResult* out_result) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  DEBUG_ASSERT(requeue_target != nullptr);
  DEBUG_ASSERT(wake_hooks != nullptr);
  DEBUG_ASSERT(requeue_hooks != nullptr);
  DEBUG_ASSERT(out_result != nullptr);
  RustWakeRequeueHookProxy wake_proxy(wake_hooks);
  RustWakeRequeueHookProxy requeue_proxy(requeue_hooks);
  *out_result = wait_queue->WakeAndRequeue(*requeue_target, new_requeue_owner, wake_count,
                                           requeue_count, wake_proxy, requeue_proxy, wake_option);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_owned_wait_queue_begin_propagate_remove_single_edge(
    Thread* upstream_node, OwnedWaitQueue* downstream_node, ForceInheritance force_inheritance,
    const SchedulerState::EffectiveProfile* old_target_ep) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(upstream_node != nullptr);
  DEBUG_ASSERT(downstream_node != nullptr);
  OwnedWaitQueue::BeginPropagate(*upstream_node, *downstream_node,
                                 OwnedWaitQueue::RemoveSingleEdgeOp, force_inheritance,
                                 old_target_ep);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_owned_wait_queue_update_sched_state_storage_thread_removed(
    OwnedWaitQueue* wait_queue, Thread* t) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  DEBUG_ASSERT(t != nullptr);
  wait_queue->UpdateSchedStateStorageThreadRemoved(*t);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_owned_wait_queue_unlock_pi_chain_common_wait_queue(
    WaitQueueBase* start, const void* stop_point) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(start != nullptr);
  OwnedWaitQueue::UnlockPiChainCommon(*start, stop_point);
}

}  // extern "C"
