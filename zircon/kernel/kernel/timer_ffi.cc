// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <new>

#include <kernel/ffi.h>
#include <kernel/timer.h>

static_assert(sizeof(Timer) == 72, "Timer size mismatch");
static_assert(alignof(Timer) == 8, "Timer alignment mismatch");

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_timer_init(Timer* timer, zx_clock_t clock_id) {
  DEBUG_ASSERT(timer != nullptr);
  new (timer) Timer(clock_id);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_timer_destroy(Timer* timer) {
  DEBUG_ASSERT(timer != nullptr);
  timer->~Timer();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_timer_set(Timer* timer, const Deadline* deadline,
                                     Timer::Callback callback, void* arg) {
  DEBUG_ASSERT(timer != nullptr);
  DEBUG_ASSERT(deadline != nullptr);
  timer->Set(*deadline, callback, arg);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_timer_cancel(Timer* timer) {
  DEBUG_ASSERT(timer != nullptr);
  return timer->Cancel();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_timer_trylock_or_cancel(Timer* timer, ChainLock* lock)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(timer != nullptr);
  DEBUG_ASSERT(lock != nullptr);
  return timer->TrylockOrCancel(*lock);
}

}  // extern "C"
