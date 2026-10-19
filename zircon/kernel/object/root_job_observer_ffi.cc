// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/object-constants.h>

#include <new>

#include <kernel/ffi.h>
#include <object/handle.h>
#include <object/job_dispatcher.h>
#include <object/process_dispatcher.h>
#include <object/root_job_observer.h>
#include <object/root_job_observer_ffi.h>
#include <object/signal_observer.h>
#include <object/thread_dispatcher.h>

class RootJobSignalObserver final : public SignalObserver {
 public:
  explicit RootJobSignalObserver(void* rust_ctx) : rust_ctx_(rust_ctx) {}
  void OnMatch(zx_signals_t signals, OwnedWaitQueue* queue_to_own) final {
    rust_root_job_observer_on_match(rust_ctx_, signals);
  }
  void OnCancel(zx_signals_t signals) final {
    rust_root_job_observer_on_cancel(rust_ctx_, signals);
  }

 private:
  void* rust_ctx_;
};

static_assert(sizeof(RootJobSignalObserver) == kRootJobSignalObserverSize);
static_assert(alignof(RootJobSignalObserver) == kRootJobSignalObserverAlign);

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_root_job_signal_observer_init(void* storage, void* rust_ctx) {
  new (storage) RootJobSignalObserver(rust_ctx);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_root_job_signal_observer_destroy(SignalObserver* observer) {
  static_cast<RootJobSignalObserver*>(observer)->~RootJobSignalObserver();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_test_process_is_dead(const ProcessDispatcher* process) {
  return process->state() == ProcessDispatcher::State::DEAD;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_test_thread_is_dying_or_dead(const ThreadDispatcher* thread) {
  return thread->IsDyingOrDead();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_test_process_is_running(const ProcessDispatcher* process) {
  return process->state() == ProcessDispatcher::State::RUNNING;
}

}  // extern "C"
