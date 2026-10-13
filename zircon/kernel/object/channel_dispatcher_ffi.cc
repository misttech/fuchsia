// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/kconcurrent/chainlock_transaction.h>
#include <zircon/errors.h>

#include <fbl/alloc_checker.h>
#include <kernel/deadline.h>
#include <kernel/ffi.h>
#include <kernel/owned_wait_queue.h>
#include <kernel/thread.h>
#include <ktl/utility.h>
#include <object/channel_dispatcher.h>
#include <object/handle.h>

extern "C" {

zx_status_t cpp_channel_dispatcher_create(
    void* holder, ffi::Uninitialized<KernelHandle<ChannelDispatcher>>* handle_out) {
  fbl::AllocChecker ac;
  auto disp = fbl::AdoptRef(new (&ac) ChannelDispatcher(holder));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  handle_out->Initialize(ktl::move(disp));
  return ZX_OK;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_message_waiter_signal(OwnedWaitQueue* wait_queue, bool* signaled_out) {
  // TODO(https://fxbug.dev/477068635): Consider merging this logic back into OwnedWaitQueue.
  auto& wake_hooks = OwnedWaitQueue::default_wake_hooks();
  const auto do_transaction = [&] TA_REQ(chainlock_transaction_token,
                                         preempt_disabled_token) -> ChainLockTransaction::Result<> {
    if (Thread::UnblockList threads;
        wait_queue->LockForWakeOperationOrBackoff(UINT32_MAX, wake_hooks, threads)) {
      ChainLockTransaction::Finalize();
      *signaled_out = true;
      // TODO(https://fxbug.dev/42182908): Once fair-to-fair priority inheritance is implemented,
      // change this to `ForceInheritance::Yes`.
      wait_queue->WakeThreadsLocked(ktl::move(threads), wake_hooks,
                                    OwnedWaitQueue::WakeOption::AssignOwner, ForceInheritance::No);
      wait_queue->get_lock().Release();
      return ChainLockTransaction::Done;
    }
    return ChainLockTransaction::Action::Backoff;
  };
  ChainLockTransaction::UntilDone(EagerReschedDisableAndIrqSaveOption,
                                  CLT_TAG("OwnedWaitQueue::Signal"), do_transaction);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_message_waiter_wait(OwnedWaitQueue* wait_queue,
                                                      const bool* signaled,
                                                      const Deadline* deadline) {
  // TODO(https://fxbug.dev/477068635): Consider merging this logic back into OwnedWaitQueue.
  Thread* current_thread = Thread::Current::Get();
  const auto do_transaction =
      [&] TA_REQ(chainlock_transaction_token,
                 preempt_disabled_token) -> ChainLockTransaction::Result<zx_status_t> {
    wait_queue->get_lock().AcquireFirstInChain();
    if (*signaled) {
      wait_queue->get_lock().Release();
      return ZX_OK;
    }
    Thread* new_owner = wait_queue->owner();
    if (OwnedWaitQueue::BAAOLockingDetails details;
        wait_queue->TryLockForBAAOOperationLocked(current_thread, new_owner, details)) {
      ChainLockTransaction::Finalize();
      // TODO(https://fxbug.dev/42182908): Once fair-to-fair priority inheritance is implemented,
      // change this to `ForceInheritance::Yes`.
      const zx_status_t result = wait_queue->BlockAndAssignOwnerLocked(
          current_thread, *deadline, details, ResourceOwnership::Normal, Interruptible::Yes,
          ForceInheritance::No);
      current_thread->get_lock().Release();
      return result;
    }
    wait_queue->get_lock().Release();
    return ChainLockTransaction::Action::Backoff;
  };
  return ChainLockTransaction::UntilDone(EagerReschedDisableAndIrqSaveOption,
                                         CLT_TAG("OwnedWaitQueue::Wait"), do_transaction);
}

}  // extern "C"
