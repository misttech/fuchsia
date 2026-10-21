// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <assert.h>

#include <kernel/ffi.h>
#include <kernel/owned_wait_queue.h>
#include <ktl/memory.h>

extern "C" {

static_assert(sizeof(ffi::Uninitialized<Thread::UnblockList>) == sizeof(Thread::UnblockList));
static_assert(alignof(ffi::Uninitialized<Thread::UnblockList>) == alignof(Thread::UnblockList));

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
FFI_ALWAYS_INLINE bool cpp_thread_unblock_list_is_empty(const Thread::UnblockList* list) {
  DEBUG_ASSERT(list != nullptr);
  return list->is_empty();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_unblock_list_push_back(Thread::UnblockList* list,
                                                         Thread* thread) {
  DEBUG_ASSERT(list != nullptr);
  DEBUG_ASSERT(thread != nullptr);
  list->push_back(thread);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE Thread* cpp_thread_unblock_list_pop_front(Thread::UnblockList* list) {
  DEBUG_ASSERT(list != nullptr);
  return list->pop_front();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE Thread* cpp_thread_unblock_list_begin(const Thread::UnblockList* list) {
  DEBUG_ASSERT(list != nullptr);
  return list->begin().CopyPointer();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE Thread* cpp_thread_unblock_list_next(const Thread::UnblockList* list,
                                                       const Thread* thread) {
  DEBUG_ASSERT(list != nullptr);
  DEBUG_ASSERT(thread != nullptr);
  return (++list->make_iterator(*thread)).CopyPointer();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_owned_wait_queue_wake_requeue_hook_allow(
    OwnedWaitQueue::IWakeRequeueHook* hook, const Thread* t) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(hook != nullptr);
  DEBUG_ASSERT(t != nullptr);
  return hook->Allow(*t);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_owned_wait_queue_wake_requeue_hook_on_wake_or_requeue(
    OwnedWaitQueue::IWakeRequeueHook* hook, Thread* t) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(hook != nullptr);
  DEBUG_ASSERT(t != nullptr);
  hook->OnWakeOrRequeue(*t);
}

}  // extern "C"
