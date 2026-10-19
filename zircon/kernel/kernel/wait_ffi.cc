// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <assert.h>

#include <kernel/ffi.h>
#include <kernel/wait.h>
#include <kernel/wait_queue_internal.h>
#include <ktl/utility.h>

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_wait_queue_collection_insert(
    WaitQueueCollection* collection, Thread* thread) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(collection != nullptr);
  DEBUG_ASSERT(thread != nullptr);
  collection->Insert(thread);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_wait_queue_collection_remove(
    WaitQueueCollection* collection, Thread* thread) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(collection != nullptr);
  DEBUG_ASSERT(thread != nullptr);
  collection->Remove(thread);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_wait_queue_lock_ops_lock_for_wake_all_in_queue(
    WaitQueue* queue, zx_status_t wait_queue_error,
    void* unblock_list_out) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(queue != nullptr);
  DEBUG_ASSERT(unblock_list_out != nullptr);
  ktl::optional<Thread::UnblockList> res =
      WaitQueueLockOps::LockForWakeAllInQueue(*queue, wait_queue_error);
  if (!res.has_value()) {
    return false;
  }
  *static_cast<Thread::UnblockList*>(unblock_list_out) = ktl::move(*res);
  return true;
}

}  // extern "C"
