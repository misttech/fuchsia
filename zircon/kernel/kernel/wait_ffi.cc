// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <assert.h>

#include <kernel/ffi.h>
#include <kernel/wait.h>
#include <kernel/wait_queue_internal.h>

extern "C" {

static_assert(sizeof(ffi::Uninitialized<WaitQueueBase>) == sizeof(WaitQueueBase));
static_assert(alignof(ffi::Uninitialized<WaitQueueBase>) == alignof(WaitQueueBase));
static_assert(sizeof(ffi::Uninitialized<WaitQueue>) == sizeof(WaitQueue));
static_assert(alignof(ffi::Uninitialized<WaitQueue>) == alignof(WaitQueue));

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_wait_queue_base_init(ffi::Uninitialized<WaitQueueBase>* wait_queue,
                                                uint32_t magic) {
  DEBUG_ASSERT(wait_queue != nullptr);
  new (wait_queue->GetAddressUnchecked()) WaitQueueBase(magic);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint32_t cpp_wait_queue_base_magic(const WaitQueueBase* wait_queue) {
  DEBUG_ASSERT(wait_queue != nullptr);
  return wait_queue->magic();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_wait_queue_base_is_empty(const WaitQueueBase* wait_queue)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  return wait_queue->IsEmpty();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint32_t cpp_wait_queue_base_count(const WaitQueueBase* wait_queue)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  return wait_queue->Count();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t
cpp_wait_queue_base_unblock_thread(WaitQueueBase* wait_queue, Thread* t,
                                   zx_status_t wait_queue_error) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  DEBUG_ASSERT(t != nullptr);
  return wait_queue->UnblockThread(t, wait_queue_error);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE Thread* cpp_wait_queue_base_peek_front(WaitQueueBase* wait_queue)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  return wait_queue->PeekFront();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_wait_queue_base_block_etc_preamble(
    WaitQueueBase* wait_queue, Thread* current_thread, const Deadline* deadline,
    uint32_t signal_mask, ResourceOwnership reason,
    Interruptible interruptible) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  DEBUG_ASSERT(current_thread != nullptr);
  DEBUG_ASSERT(deadline != nullptr);
  return wait_queue->BlockEtcPreamble(current_thread, *deadline, signal_mask, reason,
                                      interruptible);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t
cpp_wait_queue_base_block_etc_postamble(WaitQueueBase* wait_queue, Thread* current_thread,
                                        const Deadline* deadline) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  DEBUG_ASSERT(current_thread != nullptr);
  DEBUG_ASSERT(deadline != nullptr);
  return wait_queue->BlockEtcPostamble(current_thread, *deadline);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_wait_queue_base_dequeue(WaitQueueBase* wait_queue, Thread* t,
                                                   zx_status_t wait_queue_error)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  DEBUG_ASSERT(t != nullptr);
  wait_queue->Dequeue(t, wait_queue_error);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_wait_queue_base_validate_queue(WaitQueueBase* wait_queue)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  wait_queue->ValidateQueue();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_wait_queue_base_timeout_handler(Timer* timer, zx_instant_mono_t now,
                                                           void* arg) {
  WaitQueueBase::TimeoutHandler(timer, now, arg);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_wait_queue_base_update_blocked_thread_effective_profile(
    WaitQueueBase* wait_queue, Thread* t) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  DEBUG_ASSERT(t != nullptr);
  wait_queue->UpdateBlockedThreadEffectiveProfile(*t);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_wait_queue_init(ffi::Uninitialized<WaitQueue>* wait_queue) {
  DEBUG_ASSERT(wait_queue != nullptr);
  wait_queue->Initialize();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t
cpp_wait_queue_block(WaitQueue* wait_queue, Thread* current_thread, const Deadline* deadline,
                     Interruptible interruptible) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  DEBUG_ASSERT(current_thread != nullptr);
  DEBUG_ASSERT(deadline != nullptr);
  return wait_queue->Block(current_thread, *deadline, interruptible);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_wait_queue_block_instant(
    WaitQueue* wait_queue, Thread* current_thread, zx_instant_mono_t deadline,
    Interruptible interruptible) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  DEBUG_ASSERT(current_thread != nullptr);
  return wait_queue->Block(current_thread, deadline, interruptible);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_wait_queue_block_etc(
    WaitQueue* wait_queue, Thread* current_thread, const Deadline* deadline, uint32_t signal_mask,
    ResourceOwnership reason, Interruptible interruptible) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  DEBUG_ASSERT(current_thread != nullptr);
  DEBUG_ASSERT(deadline != nullptr);
  return wait_queue->BlockEtc(current_thread, *deadline, signal_mask, reason, interruptible);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_wait_queue_wake_one(WaitQueue* wait_queue, zx_status_t wait_queue_error)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  return wait_queue->WakeOne(wait_queue_error);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_wait_queue_wake_all(WaitQueue* wait_queue, zx_status_t wait_queue_error)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  wait_queue->WakeAll(wait_queue_error);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_wait_queue_wake_one_locked(WaitQueue* wait_queue,
                                                      zx_status_t wait_queue_error, bool* out_woken)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  DEBUG_ASSERT(out_woken != nullptr);
  ktl::optional<bool> res = wait_queue->WakeOneLocked(wait_queue_error);
  if (!res.has_value()) {
    return false;
  }
  *out_woken = *res;
  return true;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_wait_queue_wake_all_locked(
    WaitQueue* wait_queue, zx_status_t wait_queue_error,
    uint32_t* out_woken) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  DEBUG_ASSERT(out_woken != nullptr);
  ktl::optional<uint32_t> res = wait_queue->WakeAllLocked(wait_queue_error);
  if (!res.has_value()) {
    return false;
  }
  *out_woken = *res;
  return true;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_wait_queue_dequeue_thread(
    WaitQueue* wait_queue, Thread* t, zx_status_t wait_queue_error) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(wait_queue != nullptr);
  DEBUG_ASSERT(t != nullptr);
  wait_queue->DequeueThread(t, wait_queue_error);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_wait_queue_move_thread(WaitQueue* source, WaitQueue* dest,
                                                  Thread* t) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(source != nullptr);
  DEBUG_ASSERT(dest != nullptr);
  DEBUG_ASSERT(t != nullptr);
  WaitQueue::MoveThread(source, dest, t);
}

}  // extern "C"
