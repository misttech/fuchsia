// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

// C++ half of the `SmmuEnv` environment seam.
//
// In standalone host test builds (`defined(ARM_SMMU_HOST_TEST)`), these `extern "C"` shims forward
// to the host simulation mocks (`PmmMock`, `ObjectPinTracker`, and thread sleep).

#include "smmu_env_ffi.h"

#include <stddef.h>
#include <stdint.h>
#include <zircon/types.h>

#include <kernel/ffi.h>

#if defined(ARM_SMMU_HOST_TEST)
#include <lib/arch/cache.h>
#include <zircon/errors.h>

#include <kernel/spinlock.h>
#include <vm/page.h>
#include <vm/physmap.h>
#include <vm/pinned_vm_object.h>
#include <vm/pmm.h>

static_assert(offsetof(vm_page_t, queue_node) == 0);
static_assert(sizeof(vm_page_t::NodeState) == 2 * sizeof(void*));
#endif

static_assert(sizeof(PinnedVmObject) == 3 * sizeof(uintptr_t));
static_assert(alignof(PinnedVmObject) == alignof(uintptr_t));

#if defined(ARM_SMMU_HOST_TEST)

extern "C" {

// Sleeps the current thread for `delay_ns` nanoseconds (relative).
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
zx_status_t cpp_arm_smmu_sleep_relative(zx_duration_t delay_ns) {
  Thread::Current::SleepRelative(delay_ns);
  return ZX_OK;
}

// Cleans CPU data cache lines covering `[start, start + len)` to the Point of Coherency (PoC).
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
void cpp_arm_smmu_clean_dcache_range(uintptr_t start, size_t len) {
  arch::CleanDataCacheRange(start, len);
}

// Notifies the host simulation hardware model of a PTE write before storing `*slot = new_val`.
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
void cpp_arm_smmu_on_pte_write(const uint64_t* slot, uint64_t new_val, uint32_t level) {
  arch::OnPteWrite(slot, new_val, level);
}

// Notifies the host simulation hardware model that `len` bytes of translation table memory at
// `table_vaddr` are about to be zero-filled.
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
void cpp_arm_smmu_on_table_zero(uintptr_t table_vaddr, size_t len) {
  arch::OnTableZero(table_vaddr, len);
}

// Allocates a single physical page from the host PmmMock.
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
zx_status_t cpp_arm_smmu_pmm_alloc_page(uint32_t alloc_flags, vm_page_t** out_page) {
  if (out_page == nullptr) {
    return ZX_ERR_INVALID_ARGS;
  }
  return pmm_alloc_page(alloc_flags, out_page);
}

// Returns a single physical page to the host PmmMock.
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
void cpp_arm_smmu_pmm_free_page(vm_page_t* page) {
  if (page == nullptr) {
    return;
  }
  VmPageDoublyLinkedList list;
  list.push_front(page);
  pmm_free(&list);
}

// Looks up the `vm_page_t*` corresponding to physical address `pa`.
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
vm_page_t* cpp_arm_smmu_paddr_to_vm_page(paddr_t pa) { return paddr_to_vm_page(pa); }

// Returns the physical address of `page`.
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
paddr_t cpp_arm_smmu_vm_page_paddr(const vm_page_t* page) {
  ZX_DEBUG_ASSERT(page != nullptr);
  return page->paddr();
}

// Converts a physical address `pa` to its mock physmap virtual address.
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
uintptr_t cpp_arm_smmu_paddr_to_physmap(paddr_t pa) {
  return reinterpret_cast<uintptr_t>(paddr_to_physmap(pa));
}

// Converts a mock physmap virtual address `vaddr` to its physical address.
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
paddr_t cpp_arm_smmu_physmap_to_paddr(uintptr_t vaddr) {
  return physmap_to_paddr(reinterpret_cast<const void*>(vaddr));
}

// Returns the byte offset of a C++ `PinnedVmObject`.
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
uint64_t cpp_arm_smmu_pinned_vmo_offset(const PinnedVmObject* pinned_vmo) {
  ZX_DEBUG_ASSERT(pinned_vmo != nullptr);
  return pinned_vmo->offset();
}

// Returns the size in bytes of a C++ `PinnedVmObject`.
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
uint64_t cpp_arm_smmu_pinned_vmo_size(const PinnedVmObject* pinned_vmo) {
  ZX_DEBUG_ASSERT(pinned_vmo != nullptr);
  return pinned_vmo->size();
}

// Looks up a contiguous physical range in the underlying `VmObject` of `pinned_vmo`.
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
zx_status_t cpp_arm_smmu_pinned_vmo_lookup_contiguous(const PinnedVmObject* pinned_vmo,
                                                      uint64_t offset, size_t size,
                                                      paddr_t* out_paddr) {
  if (pinned_vmo == nullptr || pinned_vmo->vmo() == nullptr || out_paddr == nullptr) {
    return ZX_ERR_INVALID_ARGS;
  }
  return pinned_vmo->vmo()->LookupContiguous(offset, size, out_paddr);
}

}  // extern "C"
#endif  // defined(ARM_SMMU_HOST_TEST)
