// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/crashlog.h>
#include <stddef.h>
#include <stdio.h>

#include <kernel/ffi.h>

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_crashlog_panic_write(const char* data, size_t len) {
  stdout_panic_buffer.Write({data, len});
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE size_t cpp_crashlog_to_string(char* target_ptr, size_t target_len,
                                                zircon_crash_reason_t reason) {
  return crashlog_to_string({target_ptr, target_len}, reason);
}

}  // extern "C"
