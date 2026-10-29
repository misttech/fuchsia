// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_VM_OBJECT_PHYSICAL_FFI_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_VM_OBJECT_PHYSICAL_FFI_H_

#include <zircon/compiler.h>
#include <zircon/types.h>

#include "vm/vm_object_physical.h"

__BEGIN_CDECLS

VmObjectPhysical* cpp_vm_object_physical_create(paddr_t base, size_t size, zx_status_t* out_status);
VmObject* cpp_vm_object_physical_as_vm_object(VmObjectPhysical* vmo);
VmObjectPhysical* cpp_vm_object_as_vm_object_physical(VmObject* vmo);
arch_mmu_flags_t cpp_vm_object_physical_get_mapping_cache_policy_locked(
    const VmObjectPhysical* vmo);
uint32_t cpp_vm_object_physical_num_mappings_locked(const VmObjectPhysical* vmo);
bool cpp_vm_object_physical_child_list_lock_acquire();
void cpp_vm_object_physical_child_list_lock_release(bool should_clear);
bool cpp_vm_object_physical_has_children_locked(const VmObjectPhysical* vmo);
void cpp_vm_object_physical_set_cache_policy_locked(VmObjectPhysical* vmo,
                                                    arch_mmu_flags_t cache_policy);

__END_CDECLS

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_VM_OBJECT_PHYSICAL_FFI_H_
