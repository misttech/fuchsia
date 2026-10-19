// Copyright 2022 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_IO_BUFFER_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_IO_BUFFER_DISPATCHER_H_

#include <lib/object-constants.h>
#include <zircon/types.h>

#include <kernel/ffi.h>
#include <object/dispatcher.h>
#include <object/handle.h>
#include <object/opaque_storage.h>
#include <vm/vm_object.h>

class IoBufferDispatcher;

enum class IobEndpointId : size_t {
  Ep0 = 0,
  Ep1 = 1,
};

DECLARE_PEERED_DISPATCHER_RUST_PROTOS(IoBufferDispatcher, rust_io_buffer_dispatcher)

extern "C" {
zx_status_t cpp_io_buffer_dispatcher_create(
    void* holder, size_t endpoint_id, void* shared_state,
    ffi::Uninitialized<KernelHandle<IoBufferDispatcher>>* handle_out);
// Performs the C++ multiple-inheritance upcast from IoBufferDispatcher* to VmObjectChildObserver*.
VmObjectChildObserver* cpp_io_buffer_dispatcher_as_child_observer(IoBufferDispatcher* disp);

void rust_io_buffer_dispatcher_on_zero_child(IoBufferDispatcher* disp);
zx_status_t rust_io_buffer_dispatcher_get_name(const IoBufferDispatcher* disp,
                                               char (*out_name)[ZX_MAX_NAME_LEN]);
zx_status_t rust_io_buffer_dispatcher_set_name(IoBufferDispatcher* disp, const char* name,
                                               size_t len);
size_t rust_io_buffer_dispatcher_region_count(const IoBufferDispatcher* disp);
}  // extern "C"

class IoBufferDispatcher final : public Dispatcher, public VmObjectChildObserver {
 public:
  explicit IoBufferDispatcher(void* holder, IobEndpointId endpoint_id, void* shared_state);
  ~IoBufferDispatcher() final;

  DECLARE_PEERED_DISPATCHER_RUST_METHODS(rust_io_buffer_dispatcher, ZX_OBJ_TYPE_IOB, true)

  void OnZeroChild() final { rust_io_buffer_dispatcher_on_zero_child(this); }

  [[nodiscard]] zx_status_t get_name(char (&out_name)[ZX_MAX_NAME_LEN]) const final {
    return rust_io_buffer_dispatcher_get_name(this, &out_name);
  }
  [[nodiscard]] zx_status_t set_name(const char* name, size_t len) final {
    return rust_io_buffer_dispatcher_set_name(this, name, len);
  }

  size_t RegionCount() const { return rust_io_buffer_dispatcher_region_count(this); }

 protected:
  Lock<CriticalMutex>* get_lock() const final;

 private:
  OpaqueStorage<kIoBufferDispatcherStateSize, kIoBufferDispatcherStateAlign> opaque_storage_;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_IO_BUFFER_DISPATCHER_H_
