// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/ktrace.h>
#include <stdint.h>

#include <kernel/ffi.h>
#include <kernel/wait_ffi.h>

#ifdef UNITTESTS_ENABLED
#include <stddef.h>

#include <kernel/thread.h>
#include <kernel/wait.h>
#endif

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

#ifdef UNITTESTS_ENABLED
// TODO: Remove these test-only shims once C++ `WaitQueueCollection`'s `fbl::WAVLTree` read view is
// retired.
uint32_t cpp_wait_queue_test_tree_size(const WaitQueueCollection* collection);
const Thread* cpp_wait_queue_test_tree_front(const WaitQueueCollection* collection);
const Thread* cpp_wait_queue_test_tree_nth(const WaitQueueCollection* collection, size_t n);

uint32_t cpp_wait_queue_test_tree_size(const WaitQueueCollection* collection) {
  return collection->Count();
}

const Thread* cpp_wait_queue_test_tree_front(const WaitQueueCollection* collection) {
  return collection->PeekFront();
}

// Walks the C++ view of the tree in order and returns the |n|th thread, or nullptr past the end.
const Thread* cpp_wait_queue_test_tree_nth(const WaitQueueCollection* collection, size_t n) {
  auto it = collection->threads().begin();
  const auto end = collection->threads().end();
  for (; it != end && n > 0; ++it, --n) {
  }
  return it == end ? nullptr : &*it;
}
#endif

}  // extern "C"
