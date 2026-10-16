// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <new>

#include <kernel/ffi.h>
#include <kernel/semaphore.h>

static_assert(sizeof(Semaphore) == 8 + sizeof(WaitQueue));
static_assert(alignof(Semaphore) == 8);

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_semaphore_init(ffi::Uninitialized<Semaphore>* semaphore,
                                          int64_t initial_count) {
  semaphore->Initialize(initial_count);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_semaphore_destroy(Semaphore* semaphore) { semaphore->~Semaphore(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_semaphore_post(Semaphore* semaphore) { semaphore->Post(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_semaphore_wait_deadline(Semaphore* semaphore,
                                                          const Deadline* deadline) {
  return semaphore->Wait(*deadline);
}

}  // extern "C"
