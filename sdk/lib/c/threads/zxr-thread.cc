// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "zxr-thread.h"

#include <lib/elfldltl/machine.h>
#include <lib/zircon-internal/unique-backtrace.h>
#include <lib/zx/thread.h>
#include <stddef.h>
#include <stdint.h>
#include <zircon/syscalls.h>

#include <atomic>
#include <utility>

using State = zxr_thread_t::State;

// Put the thread into EXITING state.  Returns the previous state.
static State begin_exit(zxr_thread_t* thread) {
  return thread->state.exchange(State::EXITING, std::memory_order_release);
}

// Claim the thread as JOINED or DETACHED.  Returns true on success, which only
// happens if the previous state was JOINABLE.  Always returns the previous state.
static bool claim_thread(zxr_thread_t* thread, State new_state, State* old_state) {
  *old_state = State::JOINABLE;
  return thread->state.compare_exchange_strong(*old_state, new_state, std::memory_order_acq_rel,
                                               std::memory_order_acquire);
}

// Extract the handle from the thread structure. Synchronizes with readers by
// setting the state to FREED and checks the given expected state for consistency.
static zx_handle_t take_handle(zxr_thread_t* thread, State expected_state) {
  const zx_handle_t tmp = thread->handle;
  thread->handle = ZX_HANDLE_INVALID;

  if (!thread->state.compare_exchange_strong(
          expected_state, State::FREED, std::memory_order_acq_rel, std::memory_order_acquire)) {
    CRASH_WITH_UNIQUE_BACKTRACE();
  }

  return tmp;
}

static zx_futex_t* state_futex(zxr_thread_t* thread) {
  return reinterpret_cast<zx_futex_t*>(&thread->state);
}

[[noreturn]] static void exit_non_detached(zxr_thread_t* thread) {
  // Wake the _zx_futex_wait in zxr_thread_join (below), and then die.
  // This has to be done with the special four-in-one vDSO call because
  // as soon as the state transitions to DONE, the joiner is free to unmap
  // our stack out from under us.  Note there is a benign race here still: if
  // the address is unmapped and our futex_wake fails, it's OK; if the memory
  // is reused for something else and our futex_wake tickles somebody
  // completely unrelated, well, that's why futex_wait can always have
  // spurious wakeups.
  _zx_futex_wake_handle_close_thread_exit(state_futex(thread), 1, static_cast<int>(State::DONE),
                                          ZX_HANDLE_INVALID);
  CRASH_WITH_UNIQUE_BACKTRACE();
}

[[noreturn]] void zxr_thread_exit_unmap_if_detached(zxr_thread_t* thread,
                                                    void (*if_detached)(void*),
                                                    void* if_detached_arg,

                                                    zx_handle_t vmar, uintptr_t addr, size_t len) {
  const State old_state = begin_exit(thread);
  switch (old_state) {
    case State::DETACHED: {
      (*if_detached)(if_detached_arg);
      const zx_handle_t handle = take_handle(thread, State::EXITING);
      _zx_vmar_unmap_handle_close_thread_exit(vmar, addr, len, handle);
      break;
    }
    // See comments in thread_trampoline.
    case State::JOINABLE:
    case State::JOINED:
      exit_non_detached(thread);
      break;
    default:
      break;
  }

  // Cannot be in DONE or the EXITING and reach here.
  CRASH_WITH_UNIQUE_BACKTRACE();
}

static void wait_for_done(zxr_thread_t* thread, State old_state) {
  do {
    switch (_zx_futex_wait(state_futex(thread), static_cast<int>(old_state), ZX_HANDLE_INVALID,
                           ZX_TIME_INFINITE)) {
      case ZX_ERR_BAD_STATE:  // Never blocked because it had changed.
      case ZX_OK:             // Woke up because it might have changed.
        old_state = thread->state.load(std::memory_order_acquire);
        break;
      default:
        CRASH_WITH_UNIQUE_BACKTRACE();
    }
    // Wait until we reach the DONE state, even if we observe the
    // intermediate EXITING state.
  } while (old_state == State::JOINED || old_state == State::EXITING);

  if (old_state != State::DONE)
    CRASH_WITH_UNIQUE_BACKTRACE();
}

zx_status_t zxr_thread_join(zxr_thread_t* thread) {
  State old_state;
  // Try to claim the join slot on this thread.
  if (claim_thread(thread, State::JOINED, &old_state)) {
    wait_for_done(thread, State::JOINED);
  } else {
    switch (old_state) {
      case State::JOINED:
      case State::DETACHED:
        return ZX_ERR_INVALID_ARGS;
      case State::EXITING:
        // Since it is undefined to call zxr_thread_join on a thread
        // that has already been detached or joined, we assume the state
        // prior to EXITING was JOINABLE, and act as if we had
        // successfully transitioned to JOINED.
        wait_for_done(thread, State::EXITING);
        __FALLTHROUGH;
      case State::DONE:
        break;
      default:
        CRASH_WITH_UNIQUE_BACKTRACE();
    }
  }

  // Take the handle and synchronize with readers.
  const zx_handle_t handle = take_handle(thread, State::DONE);
  if (handle == ZX_HANDLE_INVALID || zx_handle_close(handle) != ZX_OK) {
    CRASH_WITH_UNIQUE_BACKTRACE();
  }

  return ZX_OK;
}

zx_status_t zxr_thread_detach(zxr_thread_t* thread) {
  State old_state;
  // Try to claim the join slot on this thread on behalf of the thread.
  if (!claim_thread(thread, State::DETACHED, &old_state)) {
    switch (old_state) {
      case State::DETACHED:
      case State::JOINED:
        return ZX_ERR_INVALID_ARGS;
      case State::EXITING: {
        // Since it is undefined behavior to call zxr_thread_detach on a
        // thread that has already been detached or joined, we assume
        // the state prior to EXITING was JOINABLE.  However, since the
        // thread is already shutting down, it is too late to tell it to
        // clean itself up.  Since the thread is still running, we cannot
        // just return ZX_ERR_BAD_STATE, which would suggest we couldn't detach and
        // the thread has already finished running.  Instead, we call join,
        // which will return soon due to the thread being actively shutting down,
        // and then return ZX_ERR_BAD_STATE to tell the caller that they
        // must manually perform any post-join work.
        const zx_status_t ret = zxr_thread_join(thread);
        if (unlikely(ret != ZX_OK)) {
          if (unlikely(ret != ZX_ERR_INVALID_ARGS)) {
            CRASH_WITH_UNIQUE_BACKTRACE();
          }
          return ret;
        }
      }
        // Fall-through to DONE case.
        __FALLTHROUGH;
      case State::DONE:
        return ZX_ERR_BAD_STATE;
      default:
        CRASH_WITH_UNIQUE_BACKTRACE();
    }
  }

  return ZX_OK;
}

bool zxr_thread_detached(zxr_thread_t* thread) {
  return thread->state.load(std::memory_order_acquire) == State::DETACHED;
}
