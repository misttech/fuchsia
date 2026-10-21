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
#include <lib/ktrace.h>
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

// There are a limited number of operations which should never be done on a
// WaitQueue which happens to be an OwnedWaitQueue.  Specifically, blocking.
// Blocking on an OWQ should always go through the OWQ specific
// BlockAndAssignOwner.  Add a macro to check for that as well.
#define DEBUG_ASSERT_MAGIC_AND_NOT_OWQ(_queue)                                                     \
  do {                                                                                             \
    DEBUG_ASSERT_MSG(((_queue)->magic_ != OwnedWaitQueue::kOwnedMagic),                            \
                     "This operation should not be performed against the WaitQueue "               \
                     "API, use the OwnedWaitQueue API intead.");                                   \
    DEBUG_ASSERT_MSG(((_queue)->magic_ == WaitQueue::kMagic), "magic 0x%08x", ((_queue)->magic_)); \
  } while (false)

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

ChainLock::Result WaitQueueCollection::LockAll() {
  for (Thread& t : threads_) {
    // If we hit a conflict, we need to unlock everything and start again,
    // unwinding completely out of this function as we go.
    //
    // Note that this method relies on the enumeration order of the collection
    // being deterministic, which should always be the case for a fbl::WAVLTree.
    if (!t.get_lock().AcquireOrBackoff()) {
      for (Thread& unlock_me : threads_) {
        if (unlock_me.get_lock().MarkNeedsReleaseIfHeld()) {
          unlock_me.get_lock().Release();
        } else {
          break;
        }
      }
      return ChainLock::Result::Backoff;
    }
    t.get_lock().MarkReleased();
  }

  return ChainLock::Result::Ok;
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

void WaitQueue::DequeueThread(Thread* t, zx_status_t wait_queue_error) {
  rust_wait_queue_dequeue_thread(this, t, wait_queue_error);
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
  DEBUG_ASSERT_MAGIC_AND_NOT_OWQ(&queue);
  LockForWakeResult result;
  Thread* t;

  while ((t = queue.collection_.PeekFront()) != nullptr) {
    // Figure out of this thread a reader or a writer.  Note; the odd use of a
    // lambda here is to work around issues with the static analyzer.  We cannot
    // assert that we have read access to a capability in a function, and later
    // on assert that we have exclusive access (after having obtained the
    // capability exclusively).
    const bool is_reader = [&]() TA_REQ(queue.get_lock()) {
      AssertInWaitQueue(*t, queue);
      DEBUG_ASSERT((t->state() == THREAD_BLOCKED) || (t->state() == THREAD_BLOCKED_READ_LOCK));
      return (t->state() == THREAD_BLOCKED_READ_LOCK);
    }();

    // We should stop if we have already selected threads (meaning we are waking
    // readers), and this thread is not a reader.
    if ((result.count != 0) && (is_reader == false)) {
      return result;
    }

    // Looks like we want to wake this thread.  Try to lock it.
    if (!t->get_lock().AcquireOrBackoff()) {
      // Put each of the threads back where they belong before returning
      // nothing, unlocking them as we go.  Make sure to set the thread's
      // blocking wait queue pointer back to ourselves.
      while (!result.list.is_empty()) {
        Thread* return_to_queue = result.list.pop_front();
        return_to_queue->get_lock().AssertAcquired();
        DEBUG_ASSERT(return_to_queue->wait_queue_state().blocking_wait_queue_ == nullptr);
        return_to_queue->wait_queue_state().blocking_wait_queue_ = &queue;
        queue.collection_.Insert(return_to_queue);
        return_to_queue->get_lock().Release();
      }
      return ktl::nullopt;
    }

    // We got the thread's lock.  Move it from the collection to our list of
    // threads to wake, clearing the blocking queue and setting the block result
    // as we go.
    queue.DequeueThread(t, ZX_OK);
    result.list.push_back(t);
    ++result.count;

    t->get_lock().MarkReleased();

    // If we just locked a writer for wake, we are done.  We can only wake one writer at a time.
    if (is_reader == false) {
      DEBUG_ASSERT(result.count == 1);
      return result;
    }
  }

  // Out of threads to lock.  Whatever we have so far is our result.
  return result;
}

ktl::optional<Thread::UnblockList> WaitQueueLockOps::LockForWakeAllInPlace(WaitQueue& queue) {
  DEBUG_ASSERT_MAGIC_AND_NOT_OWQ(&queue);

  // Try to lock all of the threads, backing off if we have to.
  if (const ChainLock::Result res = queue.collection_.LockAll();
      res == ChainLock::Result::Backoff) {
    return ktl::nullopt;
  }

  // Now that we have all of the threads locked, we are committed to the
  // WakeAll operation, and we can add the threads to an UnblockList.
  //
  // TODO(https://fxbug.dev/473385363): The in-place operations prevent us from collapsing the
  // storage requirements of an UnblockList and WaitQueueCollection.
  Thread::UnblockList unblock_list;
  for (Thread& t : queue.collection_.threads()) {
    t.get_lock().AssertHeld();
    unblock_list.push_back(&t);
  }

  return unblock_list;
}

ktl::optional<Thread::UnblockList> WaitQueueLockOps::LockForWakeAllInQueue(
    WaitQueue& queue, zx_status_t wait_queue_error) {
  // TODO(johngro): Optimize this.  By removing all of the elements of the
  // collection one at a time, we are paying a rebalancing price we really
  // should not have to pay, since we are going to eventually remove all of
  // the elements, so maintaining balance is not really important.
  ktl::optional<Thread::UnblockList> unblock_list = LockForWakeAllInPlace(queue);
  if (unblock_list.has_value()) {
    for (Thread& t : *unblock_list) {
      t.get_lock().AssertHeld();
      queue.Dequeue(&t, wait_queue_error);
    }
  }
  return unblock_list;
}

ktl::optional<Thread::UnblockList> WaitQueueLockOps::LockForWakeOneInPlace(WaitQueue& queue) {
  DEBUG_ASSERT_MAGIC_AND_NOT_OWQ(&queue);

  if (Thread* t = queue.collection_.PeekFront(); t != nullptr) {
    if (!t->get_lock().AcquireOrBackoff()) {
      return ktl::nullopt;
    }

    // TODO(https://fxbug.dev/473385363): The in-place operations prevent us from collapsing the
    // storage requirements of an UnblockList and WaitQueueCollection.
    Thread::UnblockList unblock_list;
    unblock_list.push_back(t);
    t->get_lock().MarkReleased();

    return unblock_list;
  }

  return Thread::UnblockList{};
}
