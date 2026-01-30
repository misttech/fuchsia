// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/zircon-internal/unique-backtrace.h>
#include <lib/zx/result.h>
#include <zircon/syscalls.h>

#include <tuple>

#include "thread-list.h"
#include "thread-storage.h"
#include "thread.h"

namespace LIBC_NAMESPACE_DECL {

using ThreadState = zxr_thread_t::State;

zx::result<intptr_t> ThreadJoin(Thread& thread) {
  auto wait = [&thread](ThreadState old_state) {
    do {
      switch (_zx_futex_wait(thread.zxr_thread.StateFutex(), static_cast<int>(old_state),
                             ZX_HANDLE_INVALID, ZX_TIME_INFINITE)) {
        case ZX_ERR_BAD_STATE:  // Never blocked because it had changed.
        case ZX_OK:             // Woke up because it might have changed.
          old_state = thread.zxr_thread.state.load(std::memory_order_acquire);
          break;
        default:
          CRASH_WITH_UNIQUE_BACKTRACE();
      }

      // Wait until we reach the DONE state, even if we observe the
      // intermediate EXITING state.
    } while (old_state == ThreadState::JOINED || old_state == ThreadState::EXITING);

    if (old_state != ThreadState::DONE)
      CRASH_WITH_UNIQUE_BACKTRACE();
  };

  // Try to claim the join slot on this thread.
  if (auto old_state = thread.zxr_thread.JoinOrDetachState(ThreadState::JOINED); !old_state) {
    wait(ThreadState::JOINED);
  } else {
    switch (*old_state) {
      case ThreadState::JOINED:
      case ThreadState::DETACHED:
        return zx::error{ZX_ERR_INVALID_ARGS};

      case ThreadState::EXITING:
        // Since it is undefined to call ThreadJoin on a thread that has
        // already been detached or joined, we assume the state prior to
        // EXITING was JOINABLE, and act as if we had successfully transitioned
        // to JOINED.
        wait(ThreadState::EXITING);
        [[fallthrough]];

      case ThreadState::DONE:
        break;

      default:
        CRASH_WITH_UNIQUE_BACKTRACE();
    }
  }

  // Take the handle and synchronize with readers.  Then close the handle.
  if (zx::thread handle = thread.zxr_thread.TakeHandle(ThreadState::DONE); !handle) {
    CRASH_WITH_UNIQUE_BACKTRACE();
  }

  // Now the Thread object can be removed from the list of all threads.
  AllThreads().erase(thread);

  // Extract the return value passed to ThreadExit().
  intptr_t value = std::exchange(thread.join_value, 0);

  // Move the ThreadStorage out of the Thread itself; it's inside that storage.
  // Then immediately let the storage object die, freeing the thread block.
  std::ignore = ThreadStorage::FromThread(thread, true);

  return zx::ok(value);
}

}  // namespace LIBC_NAMESPACE_DECL
