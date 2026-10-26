// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_VCPU_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_VCPU_DISPATCHER_H_

#include <lib/object-constants.h>
#include <zircon/types.h>

#include <fbl/ref_ptr.h>
#include <kernel/ffi.h>
#include <ktl/unique_ptr.h>
#include <object/dispatcher.h>
#include <object/handle.h>

class GuestDispatcher;
class Vcpu;
class VcpuDispatcher;

extern "C" {
zx_status_t cpp_vcpu_dispatcher_create(
    GuestDispatcher* guest_dispatcher_raw, Vcpu* vcpu_raw,
    ffi::Uninitialized<KernelHandle<VcpuDispatcher>>* handle_out);

void rust_vcpu_dispatcher_state_init(void* state, void* disp, GuestDispatcher* guest_dispatcher,
                                     Vcpu* vcpu);
void rust_vcpu_dispatcher_state_destroy(void* state);
Lock<CriticalMutex>* rust_vcpu_dispatcher_state_get_lock(const void* state);
}

class VcpuDispatcher final : public Dispatcher {
 public:
  ~VcpuDispatcher() final;

  zx_obj_type_t get_type() const final { return ZX_OBJ_TYPE_VCPU; }
  zx_koid_t get_related_koid() const final { return ZX_KOID_INVALID; }
  bool is_waitable() const final { return true; }

  zx_status_t user_signal_self(uint32_t clear_mask, uint32_t set_mask) final {
    return UserSignalSelfSolo(this, clear_mask, set_mask, 0);
  }
  zx_status_t user_signal_peer(uint32_t clear_mask, uint32_t set_mask) final {
    return ZX_ERR_NOT_SUPPORTED;
  }

 protected:
  Lock<CriticalMutex>* get_lock() const final;

 private:
  friend zx_status_t cpp_vcpu_dispatcher_create(
      GuestDispatcher* guest_dispatcher_raw, Vcpu* vcpu_raw,
      ffi::Uninitialized<KernelHandle<VcpuDispatcher>>* handle_out);
  VcpuDispatcher(fbl::RefPtr<GuestDispatcher> guest_dispatcher, ktl::unique_ptr<Vcpu> vcpu);

  OpaqueStorage<kVcpuDispatcherStateSize, kVcpuDispatcherStateAlign> opaque_storage_;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_VCPU_DISPATCHER_H_
