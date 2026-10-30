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

#include <arch/arm64/periphmap.h>
#include <kernel/ffi.h>

#if defined(ARM_SMMU_HOST_TEST)
#include <lib/arch/cache.h>
#include <lib/boot-options/boot-options.h>
#include <lib/root_resource_filter.h>
#include <string.h>
#include <zircon/errors.h>
#include <zircon/syscalls/resource.h>

#include <dev/interrupt.h>
#include <hwreg/mmio.h>
#include <kernel/spinlock.h>
#include <ktl/utility.h>
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

// Converts a peripheral physical address to its virtual address (`0` if unmapped).
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
uintptr_t cpp_arm_smmu_periph_paddr_to_vaddr(paddr_t paddr) { return periph_paddr_to_vaddr(paddr); }

// Converts a peripheral virtual address back to its physical address (`0` if unmapped).
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
paddr_t cpp_arm_smmu_periph_vaddr_to_paddr(uintptr_t vaddr) { return periph_vaddr_to_paddr(vaddr); }

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

// Allocates a single physical page from the host `PmmMock`.
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
zx_status_t cpp_arm_smmu_pmm_alloc_page(uint32_t alloc_flags, vm_page_t** out_page) {
  if (out_page == nullptr) {
    return ZX_ERR_INVALID_ARGS;
  }
  return pmm_alloc_page(alloc_flags, out_page);
}

// Returns a single physical page to the host `PmmMock`.
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

// Resets a C++ `PinnedVmObject` in place, unpinning its pages and releasing its VMO reference.
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
void cpp_arm_smmu_pinned_vmo_reset(void* pinned_vmo) {
  if (pinned_vmo != nullptr) {
    static_cast<PinnedVmObject*>(pinned_vmo)->reset();
  }
}

// Move-constructs a C++ `PinnedVmObject` at `dst` from `src`, leaving `*src` empty.
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
void cpp_arm_smmu_pinned_vmo_move_construct(void* dst, void* src) {
  ZX_DEBUG_ASSERT(dst != nullptr);
  ZX_DEBUG_ASSERT(src != nullptr);
  new (dst) PinnedVmObject(ktl::move(*static_cast<PinnedVmObject*>(src)));
}

// Copies `BootOptions::Get()->arm_smmu_mode` into `out_buf` (up to `out_cap` bytes) and returns
// the number of bytes written.
size_t cpp_arm_smmu_get_boot_option_mode(uint8_t* out_buf, size_t out_cap) {
  if (out_buf == nullptr || out_cap == 0) {
    return 0;
  }
  const auto& mode_str = BootOptions::Get()->arm_smmu_mode;
  const size_t len = strnlen(mode_str.data(), mode_str.size());
  const size_t copy_len = len < out_cap ? len : out_cap;
  memcpy(out_buf, mode_str.data(), copy_len);
  return copy_len;
}

// Adds `[base, base + size)` to the mock root resource filter deny list.
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
void cpp_arm_smmu_root_resource_filter_add_deny_region(uint64_t base, size_t size, uint32_t kind) {
  root_resource_filter_add_deny_region(base, size, static_cast<zx_rsrc_kind_t>(kind));
}

// Configures interrupt `vector` with `trigger_mode` and `polarity`.
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
zx_status_t cpp_arm_smmu_configure_interrupt(uint32_t vector, uint32_t trigger_mode,
                                             uint32_t polarity) {
  return configure_interrupt(vector, static_cast<interrupt_trigger_mode>(trigger_mode),
                             static_cast<interrupt_polarity>(polarity));
}

// Masks interrupt `vector`.
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
zx_status_t cpp_arm_smmu_mask_interrupt(uint32_t vector) { return mask_interrupt(vector); }

// Unmasks interrupt `vector`.
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
zx_status_t cpp_arm_smmu_unmask_interrupt(uint32_t vector) { return unmask_interrupt(vector); }

// Registers `handler(cookie)` for interrupt `vector`.
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
zx_status_t cpp_arm_smmu_register_int_handler(uint32_t vector, void* cookie,
                                              void (*handler)(void*)) {
  if (handler == nullptr) {
    return register_int_handler(vector, nullptr);
  }
  return register_int_handler(vector, interrupt_handler_t(cookie, handler));
}

// Unregisters the interrupt handler for `vector`.
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
zx_status_t cpp_arm_smmu_unregister_int_handler(uint32_t vector) {
  return register_int_handler(vector, nullptr);
}

// Reads a 32-bit register at `base + offset` via the host `FakeSmmuMmioRouter` / `RegisterMmio`.
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
uint32_t cpp_arm_smmu_mmio_read32(uintptr_t base, uint32_t offset) {
  return hwreg::RegisterMmio(reinterpret_cast<volatile void*>(base)).Read<uint32_t>(offset);
}

// Writes a 32-bit register at `base + offset` via the host `FakeSmmuMmioRouter` / `RegisterMmio`.
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
void cpp_arm_smmu_mmio_write32(uintptr_t base, uint32_t offset, uint32_t val) {
  hwreg::RegisterMmio(reinterpret_cast<volatile void*>(base)).Write<uint32_t>(val, offset);
}

// Reads a 64-bit register at `base + offset` via the host `FakeSmmuMmioRouter` / `RegisterMmio`.
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
uint64_t cpp_arm_smmu_mmio_read64(uintptr_t base, uint32_t offset) {
  return hwreg::RegisterMmio(reinterpret_cast<volatile void*>(base)).Read<uint64_t>(offset);
}

// Writes a 64-bit register at `base + offset` via the host `FakeSmmuMmioRouter` / `RegisterMmio`.
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
void cpp_arm_smmu_mmio_write64(uintptr_t base, uint32_t offset, uint64_t val) {
  hwreg::RegisterMmio(reinterpret_cast<volatile void*>(base)).Write<uint64_t>(val, offset);
}

// Returns the number of physical pages currently allocated from the host `PmmMock`.
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
size_t cpp_arm_smmu_pmm_allocated_page_count(void) {
  return PmmMock::Get().GetAllocatedPageCount();
}

// Sets the maximum number of allocated physical pages allowed by the host `PmmMock`.
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
void cpp_arm_smmu_pmm_set_allocation_limit(uint64_t limit) {
  PmmMock::Get().SetAllocationLimit(limit);
}

// Clears any physical page allocation limit on the host `PmmMock`.
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
void cpp_arm_smmu_pmm_clear_allocation_limit(void) { PmmMock::Get().ClearAllocationLimit(); }

// Reads a 64-bit word from the host `PmmMock` PoC DRAM view at physical address `pa`.
bool cpp_arm_smmu_pmm_read_poc_dram64(uint64_t pa, uint64_t* out_val) {
  if (out_val == nullptr) {
    return false;
  }
  const auto res = PmmMock::Get().ReadPocDram64(pa);
  if (!res.has_value()) {
    return false;
  }
  *out_val = *res;
  return true;
}

// Reads a 64-bit word from the host `PmmMock` CPU L1/L2 cache view at physical address `pa`.
bool cpp_arm_smmu_pmm_read_cpu_cache64(uint64_t pa, uint64_t* out_val) {
  if (out_val == nullptr) {
    return false;
  }
  const auto res = PmmMock::Get().ReadCpuCache64(pa);
  if (!res.has_value()) {
    return false;
  }
  *out_val = *res;
  return true;
}

}  // extern "C"
#endif  // defined(ARM_SMMU_HOST_TEST)
