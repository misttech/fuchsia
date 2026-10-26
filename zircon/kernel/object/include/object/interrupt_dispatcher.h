// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_INTERRUPT_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_INTERRUPT_DISPATCHER_H_

#include <lib/object-constants.h>
#include <lib/wake-vector.h>
#include <zircon/types.h>

#include <kernel/ffi.h>
#include <object/dispatcher.h>

class InterruptDispatcher;

extern "C" {

using InterruptKindInitFn = void (*)(const InterruptDispatcher* disp, void* ctx);

zx_status_t cpp_interrupt_dispatcher_create(uint32_t flags, uint32_t options, void* ctx,
                                            InterruptKindInitFn init_fn, InterruptDispatcher** out);
void cpp_interrupt_dispatcher_wake_event_initialize(const InterruptDispatcher* disp);
void cpp_interrupt_dispatcher_wake_event_destroy(const InterruptDispatcher* disp);
void cpp_interrupt_dispatcher_wake_event_trigger(const InterruptDispatcher* disp,
                                                 zx_instant_boot_t trigger_time);
void cpp_interrupt_dispatcher_wake_event_acknowledge(const InterruptDispatcher* disp,
                                                     bool remain_signaled);

void rust_interrupt_dispatcher_state_init(const InterruptDispatcher* disp, void* state,
                                          uint32_t flags, uint32_t options, void* ctx,
                                          InterruptKindInitFn init_fn);
void rust_interrupt_dispatcher_state_destroy(void* state);
Lock<CriticalMutex>* rust_interrupt_dispatcher_state_get_lock(const void* state);
void rust_interrupt_dispatcher_on_zero_handles(const InterruptDispatcher* disp);
bool rust_interrupt_dispatcher_is_virtual(const InterruptDispatcher* disp);
void rust_interrupt_dispatcher_get_diagnostics(
    const InterruptDispatcher* disp, wake_vector::WakeVector::Diagnostics* diagnostics_out);
zx_status_t rust_interrupt_dispatcher_wait_for_interrupt(const InterruptDispatcher* disp,
                                                         zx_time_t* out_timestamp);
zx_status_t rust_interrupt_dispatcher_ack(const InterruptDispatcher* disp);

}  // extern "C"

class InterruptDispatcher final : public Dispatcher, public wake_vector::WakeVector {
 public:
  ~InterruptDispatcher() final;
  InterruptDispatcher& operator=(const InterruptDispatcher&) = delete;

  zx_obj_type_t get_type() const final { return ZX_OBJ_TYPE_INTERRUPT; }
  zx_koid_t get_related_koid() const final { return ZX_KOID_INVALID; }
  bool is_waitable() const final { return true; }

  zx_status_t user_signal_self(uint32_t clear_mask, uint32_t set_mask) final {
    return UserSignalSelfSolo(
        this, clear_mask, set_mask,
        rust_interrupt_dispatcher_is_virtual(this) ? ZX_VIRTUAL_INTERRUPT_UNTRIGGERED : 0);
  }
  zx_status_t user_signal_peer(uint32_t clear_mask, uint32_t set_mask) final {
    return ZX_ERR_NOT_SUPPORTED;
  }

  void on_zero_handles() final { rust_interrupt_dispatcher_on_zero_handles(this); }

  void GetDiagnostics(WakeVector::Diagnostics& diagnostics_out) const final;

  zx_status_t WaitForInterrupt(zx_time_t* out_timestamp);
  zx_status_t Ack();

 protected:
  Lock<CriticalMutex>* get_lock() const final;

 private:
  friend zx_status_t cpp_interrupt_dispatcher_create(uint32_t flags, uint32_t options, void* ctx,
                                                     InterruptKindInitFn init_fn,
                                                     InterruptDispatcher** out);
  friend void cpp_interrupt_dispatcher_wake_event_initialize(const InterruptDispatcher* disp);
  friend void cpp_interrupt_dispatcher_wake_event_destroy(const InterruptDispatcher* disp);
  friend void cpp_interrupt_dispatcher_wake_event_trigger(const InterruptDispatcher* disp,
                                                          zx_instant_boot_t trigger_time);
  friend void cpp_interrupt_dispatcher_wake_event_acknowledge(const InterruptDispatcher* disp,
                                                              bool remain_signaled);

  InterruptDispatcher(uint32_t flags, uint32_t options, void* ctx, InterruptKindInitFn init_fn);

  OpaqueStorage<kInterruptDispatcherStateSize, kInterruptDispatcherStateAlign> opaque_storage_;
  mutable wake_vector::WakeEvent wake_event_;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_INTERRUPT_DISPATCHER_H_
