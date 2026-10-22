// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_WAIT_FFI_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_WAIT_FFI_H_

#include <zircon/compiler.h>
#include <zircon/types.h>

#include <kernel/ffi.h>

class WaitQueueCollection;
struct Thread;

__BEGIN_CDECLS

// Rust implementations of the WaitQueueCollection methods which mutate or walk the tree of
// blocked threads. The C++ WaitQueueCollection methods of the same name are thin forwarders to
// these. Every entry requires the lock of the WaitQueueBase which owns |collection| to be held.
void rust_wait_queue_collection_insert(WaitQueueCollection* collection, Thread* thread);
void rust_wait_queue_collection_remove(WaitQueueCollection* collection, Thread* thread);
// Returns a ChainLock::Result discriminant.
uint32_t rust_wait_queue_collection_lock_all(const WaitQueueCollection* collection);
zx_duration_mono_t rust_wait_queue_collection_min_inheritable_relative_deadline(
    const WaitQueueCollection* collection);
// Returns a SchedulerState::WaitQueueInheritedSchedulerState*, or nullptr if the collection is
// empty.
void* rust_wait_queue_collection_find_inherited_scheduler_state_storage(
    const WaitQueueCollection* collection);

// C++ side of WqTraceDepth, called by the Rust Insert/Remove.
FFI_ALWAYS_INLINE void cpp_wq_trace_depth(const WaitQueueCollection* collection, uint32_t depth);

__END_CDECLS

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_WAIT_FFI_H_
