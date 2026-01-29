// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_C_THREADS_ZXR_THREAD_H_
#define LIB_C_THREADS_ZXR_THREAD_H_

#include <stddef.h>
#include <stdint.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#ifdef __cplusplus
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
#endif

  zx_handle_t handle;
#ifdef __cplusplus
  std::atomic<State> state;
#else
  _Atomic(int) state;
#endif
} zxr_thread_t;
static_assert(sizeof(zxr_thread_t) == 8, "layout snafu");

// TODO(kulakowski) Document the possible zx_status_t values from these.

// Once started, threads can be either joined or detached. It is undefined
// behavior to join a thread multiple times or to join a detached thread.
// Some of the resources allocated to a thread are not collected until
// it returns and it is either joined or detached.

// If a thread is joined, the caller of zxr_thread_join blocks until
// the other thread is finished running.
zx_status_t zxr_thread_join(zxr_thread_t* thread);

// Indicates whether the thread has been detached.  The result is undefined
// if the thread is exiting or has exited.
bool zxr_thread_detached(zxr_thread_t* thread);

// Exit from the thread.  Equivalent to zxr_thread_exit unless the
// thread has been detached.  If it has been detached, then this does
// zx_vmar_unmap(vmar, addr, len) first, but in a way that permits
// unmapping the caller's own stack.  Iff it has been detached, then
// (*if_detached)(if_detached_arg) is called before unmapping the stack.
[[noreturn]] void zxr_thread_exit_unmap_if_detached(zxr_thread_t* thread,
                                                    void (*if_detached)(void*),
                                                    void* if_detached_arg, zx_handle_t vmar,
                                                    uintptr_t addr, size_t len);

__END_CDECLS

#endif  // LIB_C_THREADS_ZXR_THREAD_H_
