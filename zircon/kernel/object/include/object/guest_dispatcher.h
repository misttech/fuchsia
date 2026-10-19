// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_GUEST_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_GUEST_DISPATCHER_H_

#include <lib/object-constants.h>
#include <zircon/rights.h>
#include <zircon/syscalls/hypervisor.h>
#include <zircon/types.h>

#include <kernel/ffi.h>
#include <object/dispatcher.h>
#include <object/handle.h>
#include <object/opaque_storage.h>

class Guest;
class GuestDispatcher;

extern "C" {
zx_status_t cpp_guest_dispatcher_create(
    Guest* guest_raw, ffi::Uninitialized<KernelHandle<GuestDispatcher>>* guest_handle_out);

void rust_guest_dispatcher_state_init(void* state, void* disp, Guest* guest);
void rust_guest_dispatcher_state_destroy(void* state);
Lock<CriticalMutex>* rust_guest_dispatcher_state_get_lock(const void* state);
}

class GuestDispatcher final : public Dispatcher {
 public:
  static constexpr zx_rights_t default_rights() { return ZX_DEFAULT_GUEST_RIGHTS; }

  ~GuestDispatcher() final;

  zx_obj_type_t get_type() const final { return ZX_OBJ_TYPE_GUEST; }
  zx_koid_t get_related_koid() const final { return ZX_KOID_INVALID; }
  bool is_waitable() const final { return ZX_DEFAULT_GUEST_RIGHTS & ZX_RIGHT_WAIT; }

  zx_status_t user_signal_self(uint32_t clear_mask, uint32_t set_mask) final {
    return UserSignalSelfSolo(this, clear_mask, set_mask, 0);
  }
  zx_status_t user_signal_peer(uint32_t clear_mask, uint32_t set_mask) final {
    return ZX_ERR_NOT_SUPPORTED;
  }

 protected:
  Lock<CriticalMutex>* get_lock() const final;

 private:
  friend zx_status_t cpp_guest_dispatcher_create(
      Guest* guest_raw, ffi::Uninitialized<KernelHandle<GuestDispatcher>>* guest_handle_out);
  explicit GuestDispatcher(ktl::unique_ptr<Guest> guest);

  OpaqueStorage<kGuestDispatcherStateSize, kGuestDispatcherStateAlign> opaque_storage_;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_GUEST_DISPATCHER_H_
