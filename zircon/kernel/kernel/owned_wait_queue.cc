// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "kernel/owned_wait_queue.h"

#include <ktl/enforce.h>

SchedulerState::EffectiveProfile OwnedWaitQueue::GetEffectiveProfile() const
    TA_NO_THREAD_SAFETY_ANALYSIS {
  SchedulerState::EffectiveProfile ep{};
  rust_owned_wait_queue_get_effective_profile(this, &ep);
  return ep;
}

OwnedWaitQueue::~OwnedWaitQueue() { rust_owned_wait_queue_destroy(this); }

void OwnedWaitQueue::DisownAllQueues(Thread* t) TA_NO_THREAD_SAFETY_ANALYSIS {
  rust_owned_wait_queue_disown_all_queues(t);
}

OwnedWaitQueue::WakeThreadsResult OwnedWaitQueue::WakeThreadsLocked(
    Thread::UnblockList threads, IWakeRequeueHook& wake_hooks, WakeOption option,
    ForceInheritance force_inheritance) TA_NO_THREAD_SAFETY_ANALYSIS {
  WakeThreadsResult result;
  rust_owned_wait_queue_wake_threads_locked(this, &threads, &wake_hooks, option, force_inheritance,
                                            &result);
  return result;
}

bool OwnedWaitQueue::LockForWakeOperationOrBackoff(uint32_t max_wake, IWakeRequeueHook& wake_hooks,
                                                   Thread::UnblockList& unblock_list_out)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return rust_owned_wait_queue_lock_for_wake_operation_or_backoff(this, max_wake, &wake_hooks,
                                                                  &unblock_list_out);
}

ktl::optional<Thread::UnblockList> OwnedWaitQueue::LockForWakeOperationLocked(
    uint32_t max_wake, IWakeRequeueHook& wake_hooks) TA_NO_THREAD_SAFETY_ANALYSIS {
  Thread::UnblockList unblock_list;
  if (!rust_owned_wait_queue_lock_for_wake_operation_locked(this, max_wake, &wake_hooks,
                                                            &unblock_list)) {
    return ktl::nullopt;
  }
  return unblock_list;
}

bool OwnedWaitQueue::TryLockForBAAOOperationLocked(Thread* const current_thread, Thread* new_owner,
                                                   OwnedWaitQueue::BAAOLockingDetails& details_out)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return rust_owned_wait_queue_try_lock_for_baao_operation_locked(this, current_thread, new_owner,
                                                                  &details_out);
}

void OwnedWaitQueue::SetThreadBaseProfileAndPropagate(
    Thread& thread, const SchedulerState::BaseProfile& profile) TA_NO_THREAD_SAFETY_ANALYSIS {
  rust_owned_wait_queue_set_thread_base_profile_and_propagate(&thread, &profile);
}

void OwnedWaitQueue::ResetOwnerIfNoWaiters() TA_NO_THREAD_SAFETY_ANALYSIS {
  rust_owned_wait_queue_reset_owner_if_no_waiters(this);
}

zx_status_t OwnedWaitQueue::BlockAndAssignOwner(
    const Deadline& deadline, Thread* new_owner, ResourceOwnership resource_ownership,
    Interruptible interruptible) TA_NO_THREAD_SAFETY_ANALYSIS {
  return rust_owned_wait_queue_block_and_assign_owner(this, &deadline, new_owner,
                                                      resource_ownership, interruptible);
}

zx_status_t OwnedWaitQueue::BlockAndAssignOwnerLocked(
    Thread* const current_thread, const Deadline& deadline, const BAAOLockingDetails& details,
    ResourceOwnership resource_ownership, Interruptible interruptible,
    ForceInheritance force_inheritance) TA_NO_THREAD_SAFETY_ANALYSIS {
  return rust_owned_wait_queue_block_and_assign_owner_locked(this, current_thread, &deadline,
                                                             &details, resource_ownership,
                                                             interruptible, force_inheritance);
}

OwnedWaitQueue::WakeThreadsResult OwnedWaitQueue::WakeThreads(
    uint32_t wake_count, IWakeRequeueHook& wake_hooks) TA_NO_THREAD_SAFETY_ANALYSIS {
  WakeThreadsResult result;
  rust_owned_wait_queue_wake_threads(this, wake_count, &wake_hooks, &result);
  return result;
}

OwnedWaitQueue::WakeThreadsResult OwnedWaitQueue::WakeThreadAndAssignOwner(
    IWakeRequeueHook& wake_hooks) TA_NO_THREAD_SAFETY_ANALYSIS {
  WakeThreadsResult result;
  rust_owned_wait_queue_wake_thread_and_assign_owner(this, &wake_hooks, &result);
  return result;
}
