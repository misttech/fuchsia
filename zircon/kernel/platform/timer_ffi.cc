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
FFI_ALWAYS_INLINE zx_ticks_t cpp_ticks_per_second() { return ticks_per_second(); }

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
FFI_ALWAYS_INLINE zx_ticks_t cpp_timer_get_boot_ticks_offset() {
  return timer_get_boot_ticks_offset();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE CurrentTicksObservation cpp_timer_current_mono_and_boot_ticks() {
  return timer_current_mono_and_boot_ticks();
}

static_assert(sizeof(CurrentTicksObservation) == 16 && alignof(CurrentTicksObservation) == 8);

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_timer_set_ticks_to_time_ratio(CppRatio ticks_to_time) {
  timer_set_ticks_to_time_ratio({ticks_to_time.numerator, ticks_to_time.denominator});
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_timer_set_initial_ticks(uint64_t ticks) {
  timer_set_initial_ticks(ticks);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_timer_convert_mono_to_raw_ticks(zx_instant_mono_ticks_t mono_ticks,
                                                           zx_ticks_t* out_raw_ticks) {
  const ktl::optional<zx_ticks_t> raw_ticks = timer_convert_mono_to_raw_ticks(mono_ticks);
  if (!raw_ticks.has_value()) {
    return false;
  }
  *out_raw_ticks = raw_ticks.value();
  return true;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_platform_set_oneshot_timer(zx_ticks_t deadline) {
  return platform_set_oneshot_timer(deadline);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_platform_stop_timer() { platform_stop_timer(); }

}  // extern "C"
