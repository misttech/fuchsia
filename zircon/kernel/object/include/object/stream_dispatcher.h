// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_STREAM_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_STREAM_DISPATCHER_H_

#include <lib/object-constants.h>
#include <zircon/rights.h>
#include <zircon/types.h>

#include <kernel/ffi.h>
#include <object/dispatcher.h>
#include <object/handle.h>

class StreamDispatcher;
class VmObjectDispatcher;
class VmObjectPaged;
class StreamSizeManager;

extern "C" {
zx_status_t cpp_stream_dispatcher_create(
    uint32_t options, const VmObjectDispatcher* vmo_dispatcher, zx_off_t seek,
    ffi::Uninitialized<KernelHandle<StreamDispatcher>>* handle_out);

void rust_stream_dispatcher_state_init(void* state, uint32_t options, zx_off_t seek,
                                       VmObjectPaged* vmo, StreamSizeManager* stream_size_manager);
void rust_stream_dispatcher_state_destroy(void* state);
Lock<CriticalMutex>* rust_stream_dispatcher_state_get_lock(const void* state);
}

class StreamDispatcher final : public Dispatcher {
 public:
  ~StreamDispatcher() final;

  zx_obj_type_t get_type() const final { return ZX_OBJ_TYPE_STREAM; }
  zx_koid_t get_related_koid() const final { return ZX_KOID_INVALID; }
  bool is_waitable() const final { return ZX_DEFAULT_STREAM_RIGHTS & ZX_RIGHT_WAIT; }

  zx_status_t user_signal_self(uint32_t clear_mask, uint32_t set_mask) final;
  zx_status_t user_signal_peer(uint32_t clear_mask, uint32_t set_mask) final {
    return ZX_ERR_NOT_SUPPORTED;
  }

 protected:
  Lock<CriticalMutex>* get_lock() const final;

 private:
  friend zx_status_t cpp_stream_dispatcher_create(
      uint32_t, const VmObjectDispatcher*, zx_off_t,
      ffi::Uninitialized<KernelHandle<StreamDispatcher>>*);

  explicit StreamDispatcher(uint32_t options, fbl::RefPtr<VmObjectPaged> vmo,
                            fbl::RefPtr<StreamSizeManager> stream_size_manager, zx_off_t seek);

  OpaqueStorage<kStreamDispatcherStateSize, kStreamDispatcherStateAlign> opaque_storage_;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_STREAM_DISPATCHER_H_
