// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_WAIT_QUEUE_INTERNAL_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_WAIT_QUEUE_INTERNAL_H_

#include <lib/kconcurrent/chainlock_transaction.h>
#include <lib/ktrace.h>
#include <platform.h>
#include <zircon/errors.h>

#include <kernel/scheduler.h>
#include <kernel/thread.h>
#include <kernel/wait.h>

#ifndef WAIT_QUEUE_DEPTH_TRACING_ENABLED
#define WAIT_QUEUE_DEPTH_TRACING_ENABLED false
#endif

// Tell the static analyzer that we are allowed to read variables protected by a
// thread's lock, but don't actually perform any runtime checks. Sadly, the
// encapsulation and separation of the WaitQueueCollection from the actual
// WaitQueue makes it difficult to provide any stronger runtime checks.
//
// This is almost certainly *not* the function you should be looking for or
// using.  It is used exclusively by WaitQueueCollection code which needs to be
// able to examine the state of threads which are current in the collection
// itself.
//
// See the definition of MarkInWaitQueue for details, but the idea here is it
// is only safe to examine this state (without actually holding the thread's
// lock) while the thread exists in a wait queue, and only when holding the wait
// queue's lock.  It is *not* safe to mutate and of the thread's guarded members
// without actually holding the thread's lock.  In the limited number of places
// where we use this method, it should be obvious from context that the thread
// itself is a member of the wait queue (typically, it is used while enumerating
// the current members of the queue).
inline void MarkInWaitQueue(const Thread& t) TA_ASSERT_SHARED(t.get_lock()) {}

inline void WqTraceDepth(const WaitQueueCollection* collection, uint32_t depth) {
  if constexpr (WAIT_QUEUE_DEPTH_TRACING_ENABLED) {
    KTrace::Probe(KTrace::Context::Cpu, "wq_depth"_intern, reinterpret_cast<uint64_t>(collection),
                  static_cast<uint64_t>(depth));
  }
}

inline SchedDuration WaitQueueCollection::MinInheritableRelativeDeadline() const {
  if (threads_.is_empty()) {
    return SchedDuration::Max();
  }

  const Thread& root_thread = *threads_.root();
  MarkInWaitQueue(root_thread);

  return root_thread.wait_queue_state().subtree_min_deadline_;
}

inline void WaitQueueCollection::Insert(Thread* thread) {
  WqTraceDepth(this, Count() + 1);

  WaitQueueCollection::ThreadState& wq_state = thread->wait_queue_state();
  DEBUG_ASSERT(wq_state.blocked_threads_tree_sort_key_ == kPrimaryKeyZero);
  DEBUG_ASSERT(wq_state.subtree_min_deadline_ == SchedDuration::Max());

  const auto& sched_state = thread->scheduler_state();
  wq_state.blocked_threads_tree_sort_key_ = sched_state.finish_time();
  threads_.insert(thread);
}

inline void WaitQueueCollection::Remove(Thread* thread) {
  WqTraceDepth(this, Count() - 1);
  threads_.erase(*thread);

  // In a debug build, zero out the sort key now that we have left the
  // collection.  This can help to find bugs by allowing us to assert that the
  // value is zero during insertion, however it is not strictly needed in a
  // production build and can be skipped.
  WaitQueueCollection::ThreadState& wq_state = thread->wait_queue_state();
#ifdef DEBUG_ASSERT_IMPLEMENTED
  wq_state.blocked_threads_tree_sort_key_ = kPrimaryKeyZero;
#endif
}

// Notes for WaitQueue::BlockEtcPreamble and BlockEtcPostamble.
//
// Currently, there are two variants of WaitQueues in Zircon.  The standard
// WaitQueue (used for most tasks) and the specialized
// OwnedWaitQueues (used for mutexes/futexes/brwlocks, and anything else which
// needs to have a concept of priority inheritance).
//
// The "Block" operation for these two versions are _almost_ identical.  The
// only real difference between the two is that the OWQ implementation needs to
// stop after we have decided that we are actually going to block the thread,
// but before the timeout timer is armed and the thread is actually blocked, in
// order to update it's PI chain bookkeeping.
//
// Instead of duplicating the code, or exposing a code-injection
// mechanism into the public API, we split the code into two private
// inline member functions that we hide in instead.  The first
// (BlockEtcPreamble) performs all of the checks and bookkeeping up-to
// the point of arming the timer and blocking, the second
// (BlockEtcPostamble) finishes the job.
//
// The traditional WaitQueue implementation of
// WaitQueue::BlockEtc just calls these two functions back to back, relying on
// the inlining to generate the original function.  The OwnedWaitQueue
// implementation does the same, but injects its bookkeeping at the appropriate
// point.
//
// Nothing but these two specific pieces of code should *ever* need to
// call these functions.  Users should *always* be using either
// WaitQueue::BlockEtc/Block, or OwnedWaitQueue::BlockAndAssignOwner
// instead.
//
inline zx_status_t WaitQueueBase::BlockEtcPreamble(Thread* const current_thread,
                                                   const Deadline& deadline, uint signal_mask,
                                                   ResourceOwnership reason,
                                                   Interruptible interruptible)
    TA_REQ(get_lock(), current_thread->get_lock()) {
  return rust_wait_queue_base_block_etc_preamble(this, current_thread, &deadline, signal_mask,
                                                 reason, interruptible);
}

inline zx_status_t WaitQueueBase::BlockEtcPostamble(Thread* const current_thread,
                                                    const Deadline& deadline)
    TA_REQ(current_thread->get_lock()) TA_EXCL(get_lock()) {
  return rust_wait_queue_base_block_etc_postamble(this, current_thread, &deadline);
}

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_WAIT_QUEUE_INTERNAL_H_
