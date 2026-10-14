// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <kernel/ffi.h>
#include <platform/crashlog.h>

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_platform_crashlog_has_non_trivial_impl() {
  return PlatformCrashlog::HasNonTrivialImpl();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE PlatformCrashlog::Interface* cpp_platform_crashlog_get() {
  return &PlatformCrashlog::Get();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_platform_crashlog_enable_crashlog_uptime_updates(
    PlatformCrashlog::Interface* crashlog, bool enabled) {
  crashlog->EnableCrashlogUptimeUpdates(enabled);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE char* cpp_platform_crashlog_get_render_target(
    PlatformCrashlog::Interface* crashlog, size_t* out_len) {
  ktl::span<char> target = crashlog->GetRenderTarget();
  *out_len = target.size();
  return target.data();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_platform_crashlog_finalize(PlatformCrashlog::Interface* crashlog,
                                                      zircon_crash_reason_t reason, size_t amt) {
  crashlog->Finalize(reason, amt);
}

}  // extern "C"
