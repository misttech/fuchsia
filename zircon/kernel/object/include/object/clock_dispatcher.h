// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_CLOCK_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_CLOCK_DISPATCHER_H_

#include <lib/object-constants.h>
#include <lib/page/size.h>
#include <zircon/rights.h>
#include <zircon/types.h>

#include <fbl/ref_ptr.h>
#include <kernel/ffi.h>
#include <object/dispatcher.h>
#include <object/handle.h>
#include <vm/vm_object_paged.h>

class ClockDispatcher;

extern "C" {
zx_status_t cpp_clock_dispatcher_create(
    uint64_t options, zx_time_t backstop_time, VmObjectPaged* vmo,
    ffi::Uninitialized<KernelHandle<ClockDispatcher>>* handle_out);

void rust_clock_dispatcher_state_init(void* state, const ClockDispatcher& disp, uint64_t options,
                                      zx_time_t backstop_time, VmObjectPaged* vmo);
void rust_clock_dispatcher_state_destroy(void* state);
Lock<CriticalMutex>* rust_clock_dispatcher_state_get_lock(const void* state);
zx_status_t rust_clock_dispatcher_get_name(const ClockDispatcher& disp, char* out_name);
zx_status_t rust_clock_dispatcher_set_name(ClockDispatcher& disp, const char* name, size_t len);
}  // extern "C"

class ClockDispatcher final : public Dispatcher {
 public:
  static inline constexpr uint64_t kMappedSize = kPageSize;

  ~ClockDispatcher() final;
  zx_obj_type_t get_type() const final { return ZX_OBJ_TYPE_CLOCK; }
  zx_koid_t get_related_koid() const final { return ZX_KOID_INVALID; }
  bool is_waitable() const final { return ZX_DEFAULT_CLOCK_RIGHTS & ZX_RIGHT_WAIT; }

  [[nodiscard]] zx_status_t get_name(char (&out_name)[ZX_MAX_NAME_LEN]) const final {
    return rust_clock_dispatcher_get_name(*this, out_name);
  }
  [[nodiscard]] zx_status_t set_name(const char* name, size_t len) final {
    return rust_clock_dispatcher_set_name(*this, name, len);
  }

  zx_status_t user_signal_self(uint32_t clear_mask, uint32_t set_mask) final {
    return UserSignalSelfSolo(this, clear_mask, set_mask, 0);
  }
  zx_status_t user_signal_peer(uint32_t clear_mask, uint32_t set_mask) final {
    return ZX_ERR_NOT_SUPPORTED;
  }

 protected:
  Lock<CriticalMutex>* get_lock() const final;

 private:
  friend zx_status_t cpp_clock_dispatcher_create(
      uint64_t options, zx_time_t backstop_time, VmObjectPaged* vmo,
      ffi::Uninitialized<KernelHandle<ClockDispatcher>>* handle_out);

  ClockDispatcher(uint64_t options, zx_time_t backstop_time, fbl::RefPtr<VmObjectPaged> vmo);

  OpaqueStorage<kClockDispatcherStateSize, kClockDispatcherStateAlign> opaque_storage_;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_CLOCK_DISPATCHER_H_
