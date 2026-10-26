// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_VM_OBJECT_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_VM_OBJECT_DISPATCHER_H_

#include <lib/object-constants.h>
#include <lib/zx/result.h>
#include <sys/types.h>
#include <zircon/rights.h>
#include <zircon/types.h>

#include <kernel/ffi.h>
#include <object/dispatcher.h>
#include <object/handle.h>
#include <object/opaque_storage.h>
#include <vm/stream_size_manager.h>
#include <vm/vm_object.h>

class VmObjectDispatcher;

extern "C" {
zx_status_t cpp_vm_object_dispatcher_create(
    VmObject* raw_vmo, StreamSizeManager* raw_ssm, uint32_t initial_mutability,
    ffi::Uninitialized<KernelHandle<VmObjectDispatcher>>* out_handle);
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE VmObjectChildObserver* cpp_vm_object_dispatcher_as_child_observer(
    VmObjectDispatcher* disp);

void rust_vm_object_dispatcher_state_init(void* state, const VmObjectDispatcher* disp,
                                          VmObject* vmo, StreamSizeManager* ssm,
                                          uint32_t initial_mutability);
void rust_vm_object_dispatcher_state_destroy(void* state);
Lock<CriticalMutex>* rust_vm_object_dispatcher_state_get_lock(const void* state);

zx_status_t rust_vm_object_dispatcher_create(
    VmObject* raw_vmo, uint64_t stream_size, uint32_t initial_mutability,
    ffi::Uninitialized<KernelHandle<VmObjectDispatcher>>* out_handle,
    ffi::Uninitialized<zx_rights_t>* out_rights);
const fbl::RefPtr<VmObject>* rust_vm_object_dispatcher_get_vmo(const VmObjectDispatcher* disp);
void rust_vm_object_dispatcher_on_zero_child(VmObjectDispatcher* disp);
void rust_vm_object_dispatcher_on_zero_handles(VmObjectDispatcher* disp);
zx_status_t rust_vm_object_dispatcher_get_name(const VmObjectDispatcher* disp,
                                               char (*out_name)[ZX_MAX_NAME_LEN]);
zx_status_t rust_vm_object_dispatcher_set_name(VmObjectDispatcher* disp, const char* name,
                                               size_t len);
zx_status_t rust_vm_object_dispatcher_stream_size_manager(
    const VmObjectDispatcher* disp, ffi::Uninitialized<fbl::RefPtr<StreamSizeManager>>* out_ssm);
zx_status_t rust_vm_object_dispatcher_create_child(
    const VmObjectDispatcher* disp, uint32_t options, uint64_t offset, uint64_t size,
    bool copy_name, ffi::Uninitialized<fbl::RefPtr<VmObject>>* out_child_vmo);
uint64_t rust_vm_object_dispatcher_get_stream_size(const VmObjectDispatcher* disp);
}

class VmObjectDispatcher final : public Dispatcher, public VmObjectChildObserver {
 public:
  // LINT.IfChange(InitialMutability)
  enum class InitialMutability : uint32_t {  // NOLINT(performance-enum-size)
    kMutable,
    kImmutable,
  };
  // LINT.ThenChange(//zircon/kernel/object/vm_object_dispatcher.rs:InitialMutability)

  static zx_status_t Create(fbl::RefPtr<VmObject> vmo, uint64_t stream_size,
                            InitialMutability initial_mutability,
                            KernelHandle<VmObjectDispatcher>* handle, zx_rights_t* rights);
  ~VmObjectDispatcher() final;

  // VmObjectChildObserver implementation.
  void OnZeroChild() final { rust_vm_object_dispatcher_on_zero_child(this); }

  // Dispatcher implementation.
  zx_obj_type_t get_type() const final { return ZX_OBJ_TYPE_VMO; }
  zx_koid_t get_related_koid() const final { return ZX_KOID_INVALID; }
  bool is_waitable() const final { return true; }

  zx_status_t user_signal_self(uint32_t clear_mask, uint32_t set_mask) final {
    return UserSignalSelfSolo(this, clear_mask, set_mask, 0);
  }
  zx_status_t user_signal_peer(uint32_t clear_mask, uint32_t set_mask) final {
    return ZX_ERR_NOT_SUPPORTED;
  }

  [[nodiscard]] zx_status_t get_name(char (&out_name)[ZX_MAX_NAME_LEN]) const final {
    return rust_vm_object_dispatcher_get_name(this, &out_name);
  }
  [[nodiscard]] zx_status_t set_name(const char* name, size_t len) final {
    return rust_vm_object_dispatcher_set_name(this, name, len);
  }
  void on_zero_handles() final { rust_vm_object_dispatcher_on_zero_handles(this); }

  zx::result<fbl::RefPtr<StreamSizeManager>> stream_size_manager() const TA_EXCL(get_lock());

  zx_status_t CreateChild(uint32_t options, uint64_t offset, uint64_t size, bool copy_name,
                          fbl::RefPtr<VmObject>* child_vmo) const;

  // Returns the number of bytes in the data stream stored within the VMO.
  //
  // This returns the property previously known as the content size.
  uint64_t GetStreamSize() const { return rust_vm_object_dispatcher_get_stream_size(this); }

  const fbl::RefPtr<VmObject>& vmo() const { return *rust_vm_object_dispatcher_get_vmo(this); }

 protected:
  Lock<CriticalMutex>* get_lock() const final;

 private:
  friend zx_status_t cpp_vm_object_dispatcher_create(
      VmObject* raw_vmo, StreamSizeManager* raw_ssm, uint32_t initial_mutability,
      ffi::Uninitialized<KernelHandle<VmObjectDispatcher>>* out_handle);

  explicit VmObjectDispatcher(fbl::RefPtr<VmObject> vmo,
                              fbl::RefPtr<StreamSizeManager> stream_size_manager,
                              InitialMutability initial_mutability);

  OpaqueStorage<kVmObjectDispatcherStateSize, kVmObjectDispatcherStateAlign> opaque_storage_;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_VM_OBJECT_DISPATCHER_H_
