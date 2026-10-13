// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/affine/ratio.h>

#include <kernel/ffi.h>
#include <platform/timer.h>

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_instant_mono_ticks_t cpp_timer_current_mono_ticks() {
  return timer_current_mono_ticks();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_instant_boot_ticks_t cpp_timer_current_boot_ticks() {
  return timer_current_boot_ticks();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_instant_mono_t cpp_current_mono_time() { return current_mono_time(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_instant_boot_t cpp_current_boot_time() { return current_boot_time(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE CppRatio cpp_timer_get_ticks_to_time_ratio() {
  const affine::Ratio& r = timer_get_ticks_to_time_ratio();
  return {r.numerator(), r.denominator()};
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_ticks_t cpp_timer_get_mono_ticks_offset() {
  return timer_get_mono_ticks_offset();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE CurrentTicksObservation cpp_timer_current_mono_and_boot_ticks() {
  return timer_current_mono_and_boot_ticks();
}

static_assert(sizeof(CurrentTicksObservation) == 16 && alignof(CurrentTicksObservation) == 8);

}  // extern "C"
