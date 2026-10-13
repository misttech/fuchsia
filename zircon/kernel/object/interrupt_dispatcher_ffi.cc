// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <inttypes.h>
#include <lib/counters.h>

#include <fbl/alloc_checker.h>
#include <kernel/ffi.h>
#include <object/interrupt_dispatcher.h>

KCOUNTER(dispatcher_interrupt_create_count, "dispatcher.interrupt.create")
KCOUNTER(dispatcher_interrupt_destroy_count, "dispatcher.interrupt.destroy")

InterruptDispatcher::InterruptDispatcher(uint32_t flags, uint32_t options, void* ctx,
                                         InterruptKindInitFn init_fn)
    : Dispatcher(0u), WakeVector(&InterruptDispatcher::wake_event_), wake_event_(*this) {
  DISPATCHER_VERIFY_OFFSET(InterruptDispatcher, kInterruptDispatcherStateOffset);
  rust_interrupt_dispatcher_state_init(this, &opaque_storage_, flags, options, ctx, init_fn);
  kcounter_add(dispatcher_interrupt_create_count, 1);
}

InterruptDispatcher::~InterruptDispatcher() {
  rust_interrupt_dispatcher_state_destroy(&opaque_storage_);
  kcounter_add(dispatcher_interrupt_destroy_count, 1);
}

Lock<CriticalMutex>* InterruptDispatcher::get_lock() const {
  return rust_interrupt_dispatcher_state_get_lock(&opaque_storage_);
}

void InterruptDispatcher::GetDiagnostics(WakeVector::Diagnostics& diagnostics_out) const {
  rust_interrupt_dispatcher_get_diagnostics(this, &diagnostics_out);
}

zx_status_t InterruptDispatcher::WaitForInterrupt(zx_time_t* out_timestamp) {
  return rust_interrupt_dispatcher_wait_for_interrupt(this, out_timestamp);
}

zx_status_t InterruptDispatcher::Ack() { return rust_interrupt_dispatcher_ack(this); }

extern "C" {

zx_status_t cpp_interrupt_dispatcher_create(uint32_t flags, uint32_t options, void* ctx,
                                            InterruptKindInitFn init_fn,
                                            InterruptDispatcher** out) {
  fbl::AllocChecker ac;
  auto disp = fbl::AdoptRef(new (&ac) InterruptDispatcher(flags, options, ctx, init_fn));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  *out = fbl::ExportToRawPtr(&disp);
  return ZX_OK;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_interrupt_dispatcher_wake_event_initialize(
    const InterruptDispatcher* disp) {
  disp->wake_event_.Initialize();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_interrupt_dispatcher_wake_event_destroy(
    const InterruptDispatcher* disp) {
  disp->wake_event_.Destroy();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_interrupt_dispatcher_wake_event_trigger(const InterruptDispatcher* disp,
                                                                   zx_instant_boot_t trigger_time) {
  disp->wake_event_.Trigger(trigger_time);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_interrupt_dispatcher_wake_event_acknowledge(
    const InterruptDispatcher* disp, bool remain_signaled) {
  disp->wake_event_.Acknowledge(remain_signaled
                                    ? wake_vector::WakeEvent::AckBehavior::RemainSignaled
                                    : wake_vector::WakeEvent::AckBehavior::ClearSignaled);
}

static_assert(sizeof(wake_vector::WakeVector::Diagnostics) == 48);
static_assert(alignof(wake_vector::WakeVector::Diagnostics) == 8);

}  // extern "C"
