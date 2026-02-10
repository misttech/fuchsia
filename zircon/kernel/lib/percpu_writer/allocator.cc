// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/percpu_writer/kernel_aspace_allocator.h>

#include <fbl/alloc_checker.h>
#include <vm/vm_aspace.h>

namespace percpu_writer {

ktl::byte* KernelAspaceAllocator::Allocate(uint32_t size, const char* buffer_name) {
  VmAspace* kaspace = VmAspace::kernel_aspace();
  void* ptr;
  const zx_status_t status = kaspace->Alloc(buffer_name, size, &ptr, 0, VmAspace::VMM_FLAG_COMMIT,
                                            ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE);
  if (status != ZX_OK) {
    return nullptr;
  }
  return static_cast<ktl::byte*>(ptr);
}

void KernelAspaceAllocator::Free(ktl::byte* ptr) {
  if (ptr != nullptr) {
    VmAspace* kaspace = VmAspace::kernel_aspace();
    kaspace->FreeRegion(reinterpret_cast<vaddr_t>(ptr));
  }
}
}  // namespace percpu_writer
