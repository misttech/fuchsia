// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008-2015 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "kernel/wait.h"

#include <lib/fit/defer.h>
#include <lib/kconcurrent/chainlock.h>
#include <lib/kconcurrent/chainlock_transaction.h>
#include <platform.h>
#include <trace.h>
#include <zircon/errors.h>

#include <kernel/auto_preempt_disabler.h>
#include <kernel/owned_wait_queue.h>
#include <kernel/scheduler.h>
#include <kernel/thread.h>
#include <kernel/timer.h>
#include <kernel/wait_queue_internal.h>
#include <ktl/algorithm.h>
#include <ktl/utility.h>

#include <ktl/enforce.h>

#define LOCAL_TRACE 0

// Wait queues are building blocks that other locking primitives use to handle
// blocking threads.
void WaitQueueBase::TimeoutHandler(Timer* timer, zx_instant_mono_t now, void* arg) {
  rust_wait_queue_base_timeout_handler(timer, now, arg);
}

// Remove a thread from a wait queue, maintain the wait queue's internal count,
// and update the WaitQueue specific bookkeeping in the thread in the process.
void WaitQueueBase::Dequeue(Thread* t, zx_status_t wait_queue_error) {
  rust_wait_queue_base_dequeue(this, t, wait_queue_error);
}

void WaitQueueBase::ValidateQueue() const { rust_wait_queue_base_validate_queue(this); }

////////////////////////////////////////////////////////////////////////////////
//
// Begin user facing API
//
////////////////////////////////////////////////////////////////////////////////

/**
 * @brief  Block until a wait queue is notified, ignoring existing signals
 *         in |signal_mask|.
 *
 * This function puts the current thread at the end of a wait
 * queue and then blocks until some other thread wakes the queue
 * up again.
 *
 * @param  deadline       The time at which to abort the wait
 * @param  slack          The amount of time it is acceptable to deviate from deadline
 * @param  signal_mask    Mask of existing signals to ignore
 * @param  reason         Reason for the block
 * @param  interruptible  Whether the block can be interrupted
 *
 * If the deadline is zero, this function returns immediately with
 * ZX_ERR_TIMED_OUT.  If the deadline is ZX_TIME_INFINITE, this function
 * waits indefinitely.  Otherwise, this function returns with
 * ZX_ERR_TIMED_OUT when the deadline elapses.
 *
 * @return ZX_ERR_TIMED_OUT on timeout, else returns the return
 * value specified when the queue was woken by wait_queue_wake_one().
 */
zx_status_t WaitQueue::BlockEtc(Thread* const current_thread, const Deadline& deadline,
                                uint signal_mask, ResourceOwnership reason,
                                Interruptible interruptible) TA_NO_THREAD_SAFETY_ANALYSIS {
  return rust_wait_queue_block_etc(this, current_thread, &deadline, signal_mask, reason,
                                   interruptible);
}

/**
 * @brief  Wake up one thread sleeping on a wait queue
 *
 * This function removes one thread (if any) from the head of the wait queue and
 * makes it executable.  The new thread will be placed in the run queue.
 *
 * @param wait_queue_error  The return value which the new thread will receive
 * from wait_queue_block().
 *
 * @return  Whether a thread was woken
 */
bool WaitQueue::WakeOne(zx_status_t wait_queue_error) {
  return rust_wait_queue_wake_one(this, wait_queue_error);
}

// Same as WakeOne, but called with the queue's lock already held.  This call
// can fail (returning nullopt) in the case that a Backoff error is encountered,
// and we need to drop the queue's lock before starting again.  This version of
// WakeOne will continue to hold the queue's lock for the entire operation,
// instead of dropping it as soon as it has the target thread locked and removed
// from the queue.
ktl::optional<bool> WaitQueue::WakeOneLocked(zx_status_t wait_queue_error) {
  bool woken = false;
  if (!rust_wait_queue_wake_one_locked(this, wait_queue_error, &woken)) {
    return ktl::nullopt;
  }
  return woken;
}

/**
 * @brief  Wake all threads sleeping on a wait queue
 *
 * This function removes all threads (if any) from the wait queue and
 * makes them executable.  The new threads will be placed at the head of the
 * run queue.
 *
 * @param wait_queue_error  The return value which the new thread will receive
 * from wait_queue_block().
 *
 * @return  The number of threads woken
 */
void WaitQueue::WakeAll(zx_status_t wait_queue_error) {
  rust_wait_queue_wake_all(this, wait_queue_error);
}

ktl::optional<uint32_t> WaitQueue::WakeAllLocked(zx_status_t wait_queue_error) {
  uint32_t woken = 0;
  if (!rust_wait_queue_wake_all_locked(this, wait_queue_error, &woken)) {
    return ktl::nullopt;
  }
  return woken;
}

void WaitQueue::MoveThread(WaitQueue* source, WaitQueue* dest, Thread* t) {
  rust_wait_queue_move_thread(source, dest, t);
}

/**
 * @brief  Tear down a wait queue
 *
 * This panics if any threads were waiting on this queue, because that
 * would indicate a race condition for most uses of wait queues.  If a
 * thread is currently waiting, it could have been scheduled later, in
 * which case it would have called Block() on an invalid wait
 * queue.
 */
WaitQueueBase::~WaitQueueBase() { rust_wait_queue_base_destroy(this); }

/**
 * @brief  Wake a specific thread from a specific wait queue
 *
 * TODO(johngro): Update this comment.  UnblockThread does not actually remove
 * the thread from the wait queue, it simply finishes the unblock operation,
 * propagating any PI effects and dropping the PI lock chain starting from the
 * wait queue in the processes.
 *
 * This function extracts a specific thread from a wait queue, wakes it, puts it
 * into a Scheduler's run queue, and does a reschedule if necessary.  Callers of
 * this function must be sure that they are holding the locks for all of the
 * nodes in the PI chain, starting from the thread, before calling the function.
 * Static analysis can only ensure that the thread and its immediately
 * downstream blocking wait queue are locked.
 *
 * @param t  The thread to wake
 * @param wait_queue_error  The return value which the new thread will receive from
 * wait_queue_block().
 *
 * @return ZX_ERR_BAD_STATE if thread was not in any wait queue.
 */
zx_status_t WaitQueueBase::UnblockThread(Thread* t, zx_status_t wait_queue_error)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return rust_wait_queue_base_unblock_thread(this, t, wait_queue_error);
}

void WaitQueueBase::UpdateBlockedThreadEffectiveProfile(Thread& t) {
  rust_wait_queue_base_update_blocked_thread_effective_profile(this, &t);
}

ktl::optional<BrwLockOps::LockForWakeResult> BrwLockOps::LockForWake(WaitQueue& queue,
                                                                     zx_instant_mono_t now) {
  ktl::optional<LockForWakeResult> res{ktl::in_place};
  if (!rust_brw_lock_ops_lock_for_wake(&queue, now, &res->list, &res->count)) {
    res.reset();
  }
  return res;
}

ktl::optional<Thread::UnblockList> WaitQueueLockOps::LockForWakeAllInPlace(WaitQueue& queue) {
  ktl::optional<Thread::UnblockList> res{ktl::in_place};
  if (!rust_wait_queue_lock_ops_lock_for_wake_all_in_place(&queue, &*res)) {
    res.reset();
  }
  return res;
}

ktl::optional<Thread::UnblockList> WaitQueueLockOps::LockForWakeAllInQueue(
    WaitQueue& queue, zx_status_t wait_queue_error) {
  ktl::optional<Thread::UnblockList> res{ktl::in_place};
  if (!rust_wait_queue_lock_ops_lock_for_wake_all_in_queue(&queue, wait_queue_error, &*res)) {
    res.reset();
  }
  return res;
}

ktl::optional<Thread::UnblockList> WaitQueueLockOps::LockForWakeOneInPlace(WaitQueue& queue) {
  ktl::optional<Thread::UnblockList> res{ktl::in_place};
  if (!rust_wait_queue_lock_ops_lock_for_wake_one_in_place(&queue, &*res)) {
    res.reset();
  }
  return res;
}
