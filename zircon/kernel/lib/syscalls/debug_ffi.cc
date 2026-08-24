// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/console.h>
#include <lib/debuglog.h>
#include <lib/ktrace.h>
#include <lib/persistent-debuglog.h>

#include <kernel/ffi.h>
#include <platform/debug.h>

#include "debug_priv.h"

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" FFI_ALWAYS_INLINE void cpp_persistent_dlog_write(const char* ptr, size_t len) {
  persistent_dlog_write({ptr, len});
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" FFI_ALWAYS_INLINE zx_status_t cpp_console_run_script(const char* str) {
  return console_run_script(str);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" FFI_ALWAYS_INLINE zx_status_t cpp_ktrace_read_user(user_out_ptr<void> ptr,
                                                              uint32_t offset, size_t len,
                                                              size_t* out_actual) {
  zx::result<size_t> result = KTrace::GetInstance().ReadUser(ptr, offset, len);
  if (result.is_error()) {
    return result.status_value();
  }
  *out_actual = result.value();
  return ZX_OK;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" FFI_ALWAYS_INLINE zx_status_t cpp_ktrace_control(uint32_t action, uint32_t options) {
  return KTrace::GetInstance().Control(action, options);
}
