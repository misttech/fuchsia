// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <assert.h>
#include <lib/kconcurrent/chainlock.h>
#include <lib/ktrace.h>

#include <kernel/ffi.h>
#include <kernel/wait.h>
#include <kernel/wait_ffi.h>
#include <ktl/utility.h>

static_assert(static_cast<uint32_t>(ChainLock::Result::Ok) == 0);
static_assert(static_cast<uint32_t>(ChainLock::Result::Retry) == 1);
static_assert(static_cast<uint32_t>(ChainLock::Result::Backoff) == 2);
static_assert(static_cast<uint32_t>(ChainLock::Result::Cycle) == 3);

#ifndef WAIT_QUEUE_DEPTH_TRACING_ENABLED
#define WAIT_QUEUE_DEPTH_TRACING_ENABLED false
#endif

namespace {

inline void WqTraceDepth(const WaitQueueCollection* collection, uint32_t depth) {
  if constexpr (WAIT_QUEUE_DEPTH_TRACING_ENABLED) {
    KTrace::Probe(KTrace::Context::Cpu, "wq_depth"_intern, reinterpret_cast<uint64_t>(collection),
                  static_cast<uint64_t>(depth));
  }
}

}  // namespace

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_wq_trace_depth(const WaitQueueCollection* collection, uint32_t depth) {
  WqTraceDepth(collection, depth);
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
