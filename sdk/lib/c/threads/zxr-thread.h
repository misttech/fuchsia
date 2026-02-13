// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_C_THREADS_ZXR_THREAD_H_
#define LIB_C_THREADS_ZXR_THREAD_H_

#include <lib/zircon-internal/unique-backtrace.h>
#include <stddef.h>
#include <stdint.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#ifdef __cplusplus
#include <lib/zx/thread.h>

#include <atomic>
#include <optional>
#endif

__BEGIN_CDECLS

typedef struct zxr_thread {
#ifdef __cplusplus
  // A zxr_thread_t starts its life JOINABLE.
  // - If someone calls zxr_thread_join on it, it transitions to JOINED.
  // - If someone calls ThreadDetach on it, it transitions to DETACHED.
  // - When it begins exiting, the EXITING state is entered.
  // - When it is no longer using its memory and handle resources, it transitions
  //   to DONE.  If the thread was DETACHED prior to EXITING, this transition MAY
  //   not happen.
  // No other transitions occur.
  enum class State : int {
    JOINABLE,
    DETACHED,
    JOINED,
    EXITING,
    DONE,
    FREED,
  };

  zx_futex_t* StateFutex() { return reinterpret_cast<zx_futex_t*>(&state); }

  // Claim the thread as JOINED or DETACHED.  Returns std::nullopt on success,
  // which only happens if the previous state was JOINABLE.  On failure, it
  // returns the actual previous state.
  std::optional<State> JoinOrDetachState(State new_state) {
    if (State old_state = State::JOINABLE; !state.compare_exchange_strong(
            old_state, new_state, std::memory_order_acq_rel, std::memory_order_acquire))
        [[unlikely]] {
      return old_state;
    }
    return std::nullopt;
  }

  // Extract the thread handle.  Synchronizes with readers by setting the state
  // to FREED and checks the given expected state for consistency.
  zx::thread TakeHandle(State expected_state) {
    zx::thread taken{std::exchange(handle, ZX_HANDLE_INVALID)};
    if (!state.compare_exchange_strong(expected_state, State::FREED, std::memory_order_acq_rel,
                                       std::memory_order_acquire)) {
      CRASH_WITH_UNIQUE_BACKTRACE();
    }
    return taken;
  }
#endif

  zx_handle_t handle;
#ifdef __cplusplus
  std::atomic<State> state;
#else
  _Atomic(int) state;
#endif
} zxr_thread_t;
static_assert(sizeof(zxr_thread_t) == 8, "layout snafu");

__END_CDECLS

#endif  // LIB_C_THREADS_ZXR_THREAD_H_
