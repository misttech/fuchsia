// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_VM_ADDRESS_REGION_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_VM_ADDRESS_REGION_DISPATCHER_H_

#include <lib/object-constants.h>
#include <sys/types.h>
#include <zircon/syscalls/object.h>
#include <zircon/types.h>

#include <kernel/ffi.h>
#include <object/dispatcher.h>
#include <object/handle.h>
#include <object/opaque_storage.h>
#include <vm/vm_address_region.h>

class VmAddressRegionDispatcher;

extern "C" {
zx_status_t cpp_vmar_dispatcher_create(
    VmAddressRegion* vmar, arch_mmu_flags_t base_arch_mmu_flags,
    ffi::Uninitialized<KernelHandle<VmAddressRegionDispatcher>>* handle_out);

void rust_vm_address_region_dispatcher_state_init(void* state,
                                                  const VmAddressRegionDispatcher* disp,
                                                  VmAddressRegion* vmar,
                                                  arch_mmu_flags_t base_arch_mmu_flags);
void rust_vm_address_region_dispatcher_state_destroy(void* state);
Lock<CriticalMutex>* rust_vm_address_region_dispatcher_state_get_lock(const void* state);
zx_status_t rust_vmar_dispatcher_create(
    VmAddressRegion* vmar_raw, arch_mmu_flags_t base_arch_mmu_flags,
    ffi::Uninitialized<KernelHandle<VmAddressRegionDispatcher>>* handle_out,
    ffi::Uninitialized<zx_rights_t>* rights_out);
}

class VmAddressRegionDispatcher final : public Dispatcher {
 public:
  static zx_status_t Create(fbl::RefPtr<VmAddressRegion> vmar, arch_mmu_flags_t base_arch_mmu_flags,
                            KernelHandle<VmAddressRegionDispatcher>* handle, zx_rights_t* rights);

  ~VmAddressRegionDispatcher() final;

  zx_obj_type_t get_type() const final { return ZX_OBJ_TYPE_VMAR; }
  zx_koid_t get_related_koid() const final { return ZX_KOID_INVALID; }
  bool is_waitable() const final { return false; }

  zx_status_t user_signal_self(uint32_t clear_mask, uint32_t set_mask) final {
    return UserSignalSelfSolo(this, clear_mask, set_mask, 0);
  }
  zx_status_t user_signal_peer(uint32_t clear_mask, uint32_t set_mask) final {
    return ZX_ERR_NOT_SUPPORTED;
  }

 protected:
  Lock<CriticalMutex>* get_lock() const final;

 private:
  friend zx_status_t cpp_vmar_dispatcher_create(
      VmAddressRegion* vmar, arch_mmu_flags_t base_arch_mmu_flags,
      ffi::Uninitialized<KernelHandle<VmAddressRegionDispatcher>>* handle_out);
  VmAddressRegionDispatcher(fbl::RefPtr<VmAddressRegion> vmar,
                            arch_mmu_flags_t base_arch_mmu_flags);

  OpaqueStorage<kVmAddressRegionDispatcherStateSize, kVmAddressRegionDispatcherStateAlign>
      opaque_storage_;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_VM_ADDRESS_REGION_DISPATCHER_H_
