// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "vm/vm_object_physical_ffi.h"

#include <zircon/types.h>

#include <kernel/ffi.h>

#include "vm/vm_object_physical.h"

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" {

FFI_ALWAYS_INLINE VmObjectPhysical* cpp_vm_object_physical_create(paddr_t base, size_t size,
                                                                  zx_status_t* out_status) {
  fbl::RefPtr<VmObjectPhysical> vmo;
  *out_status = VmObjectPhysical::Create(base, size, &vmo);
  return fbl::ExportToRawPtr(&vmo);
}

FFI_ALWAYS_INLINE VmObject* cpp_vm_object_physical_as_vm_object(VmObjectPhysical* vmo) {
  return static_cast<VmObject*>(vmo);
}

FFI_ALWAYS_INLINE VmObjectPhysical* cpp_vm_object_as_vm_object_physical(VmObject* vmo) {
  return DownCastVmObject<VmObjectPhysical>(vmo);
}

FFI_ALWAYS_INLINE arch_mmu_flags_t cpp_vm_object_physical_get_mapping_cache_policy_locked(
    const VmObjectPhysical* vmo) TA_NO_THREAD_SAFETY_ANALYSIS {
  return vmo->GetMappingCachePolicyLocked();
}

FFI_ALWAYS_INLINE uint32_t cpp_vm_object_physical_num_mappings_locked(const VmObjectPhysical* vmo)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return vmo->num_mappings_locked();
}

// Note: We return the `ShouldClear` timeslice extension state as a boolean back to Rust,
// and receive it back on release. This avoids using a `thread_local` variable to store the
// token, which would trigger page faults during early boot before TLS is initialized.
FFI_ALWAYS_INLINE bool cpp_vm_object_physical_child_list_lock_acquire()
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return VmObjectPhysical::ChildListLockAcquire() == CriticalMutex::ShouldClear::Yes;
}

FFI_ALWAYS_INLINE void cpp_vm_object_physical_child_list_lock_release(bool should_clear)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  VmObjectPhysical::ChildListLockRelease(should_clear ? CriticalMutex::ShouldClear::Yes
                                                      : CriticalMutex::ShouldClear::No);
}

FFI_ALWAYS_INLINE bool cpp_vm_object_physical_has_children_locked(const VmObjectPhysical* vmo)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return vmo->has_children_locked();
}

FFI_ALWAYS_INLINE void cpp_vm_object_physical_set_cache_policy_locked(
    VmObjectPhysical* vmo, arch_mmu_flags_t cache_policy) TA_NO_THREAD_SAFETY_ANALYSIS {
  vmo->set_cache_policy_locked(cache_policy);
}

}  // extern "C"
