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

static void wait_for_done(zxr_thread_t* thread, State old_state) {
  do {
    switch (_zx_futex_wait(thread->StateFutex(), static_cast<int>(old_state), ZX_HANDLE_INVALID,
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
  // Try to claim the join slot on this thread.
  if (auto old_state = thread->JoinOrDetachState(State::JOINED); !old_state) {
    wait_for_done(thread, State::JOINED);
  } else {
    switch (*old_state) {
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

  // Take the handle and synchronize with readers.  Then close the handle.
  if (zx::thread handle = thread->TakeHandle(State::DONE); !handle) {
    CRASH_WITH_UNIQUE_BACKTRACE();
  }

  return ZX_OK;
}

bool zxr_thread_detached(zxr_thread_t* thread) {
  return thread->state.load(std::memory_order_acquire) == State::DETACHED;
}
