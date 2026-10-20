// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/suspend_wakeup_timer.h>

#include <kernel/ffi.h>
#include <ktl/memory.h>
#include <ktl/utility.h>

#ifdef __aarch64__
#include <dev/timer/armv7_mmio_timer.h>
#endif

static_assert(sizeof(SuspendWakeupTimer) == kSuspendWakeupTimerStorageSize);
static_assert(alignof(SuspendWakeupTimer) == kSuspendWakeupTimerStorageAlign);
static_assert(sizeof(SuspendWakeupTimer::Callback) == 16);
static_assert(alignof(SuspendWakeupTimer::Callback) == 8);
static_assert(sizeof(ffi::Uninitialized<SuspendWakeupTimer::Callback>) ==
              sizeof(SuspendWakeupTimer::Callback));
static_assert(alignof(ffi::Uninitialized<SuspendWakeupTimer::Callback>) ==
              alignof(SuspendWakeupTimer::Callback));

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_suspend_wakeup_timer_callback_move(
    ffi::Uninitialized<SuspendWakeupTimer::Callback>* dst, SuspendWakeupTimer::Callback* src) {
  dst->Initialize(ktl::move(*src));
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_suspend_wakeup_timer_callback_call(
    const SuspendWakeupTimer::Callback* cb, zx_instant_boot_t now, zx_instant_boot_t resume_at) {
  (*cb)(now, resume_at);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_suspend_wakeup_timer_callback_destroy(SuspendWakeupTimer::Callback* cb) {
  ktl::destroy_at(cb);
}

}  // extern "C"

#ifdef __aarch64__
static_assert(Armv7MmioTimer::kMaxTimers == 8);
static_assert(sizeof(Armv7MmioTimer::IrqHandlerResult) == sizeof(uint32_t));
static_assert(static_cast<uint32_t>(Armv7MmioTimer::IrqHandlerResult::RemainRegistered) == 0);
static_assert(static_cast<uint32_t>(Armv7MmioTimer::IrqHandlerResult::Unregister) == 1);

extern "C" {

// `context` is an opaque pointer to the Rust `SuspendWakeupTimer` registered by
// `cpp_armv7_mmio_timer_timer_set_handler`.
Armv7MmioTimer::IrqHandlerResult rust_armv7_mmio_suspend_wakeup_timer_irq_handler(
    const void* context);

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE Armv7MmioTimer* cpp_armv7_mmio_timer_get(size_t ndx) {
  return Armv7MmioTimer::Get(ndx);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE Armv7MmioTimer::Timer* cpp_armv7_mmio_timer_vct_timer(
    Armv7MmioTimer* timer_block) {
  return &timer_block->vct_timer();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE Armv7MmioTimer::Timer* cpp_armv7_mmio_timer_pct_timer(
    Armv7MmioTimer* timer_block) {
  return &timer_block->pct_timer();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_armv7_mmio_timer_timer_supported(const Armv7MmioTimer::Timer* timer) {
  return timer->supported();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_armv7_mmio_timer_timer_enabled(const Armv7MmioTimer::Timer* timer) {
  return timer->enabled();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_armv7_mmio_timer_timer_cancel(Armv7MmioTimer::Timer* timer) {
  return timer->CancelTimer();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_armv7_mmio_timer_timer_set_handler(Armv7MmioTimer::Timer* timer,
                                                                     const void* context,
                                                                     cpu_mask_t mask) {
  return timer->SetHandler(
      [context]() { return rust_armv7_mmio_suspend_wakeup_timer_irq_handler(context); }, mask);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t
cpp_armv7_mmio_timer_timer_reset_handler(Armv7MmioTimer::Timer* timer) {
  return timer->ResetHandler();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_armv7_mmio_timer_timer_set_relative_timer(
    Armv7MmioTimer::Timer* timer, zx_duration_t relative_timeout) {
  return timer->SetRelativeTimer(relative_timeout);
}

}  // extern "C"
#endif  // __aarch64__
