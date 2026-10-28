// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_VM_PMM_H_
#define ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_VM_PMM_H_

#include <lib/fit/defer.h>
#include <lib/fit/function.h>
#include <sys/types.h>
#include <zircon/assert.h>
#include <zircon/types.h>

#include <memory>
#include <mutex>
#include <optional>
#include <unordered_map>

#include <vm/page.h>

#define PMM_ALLOC_FLAG_ANY 0

class PmmMock {
 public:
  using PteWriteObserver = fit::function<void(paddr_t table_pa, uint32_t index, uint64_t old_val,
                                              uint64_t new_val, uint32_t level)>;
  using CacheCleanObserver = fit::function<void(paddr_t page_pa, size_t offset, size_t size)>;
  using PageFreeObserver = fit::function<void(paddr_t page_pa)>;

  static PmmMock& Get() { return instance_; }

  zx_status_t AllocPage(uint32_t alloc_flags, vm_page_t** p);
  void Free(VmPageDoublyLinkedList* list);
  size_t GetAllocatedPageCount();
  vm_page_t* PaddrToVmPage(paddr_t pa);

  void SetAllocationLimit(uint64_t limit);
  void ClearAllocationLimit();

  void CleanDataCacheRange(vaddr_t addr, size_t size);
  void OnPteWrite(uint64_t* slot, uint64_t new_val, uint32_t level);
  // Reports that [addr, addr + size) of a translation table page is about to be
  // zero-filled. Observers see it as a clear of every non-zero descriptor in
  // the range, at level kPteLevelUnknown.
  void OnTableZero(vaddr_t addr, size_t size);
  // The |level| passed to the PTE write observer when the writer did not say
  // which level it believes the table is at.
  static constexpr uint32_t kPteLevelUnknown = UINT32_MAX;

  // Read a 64-bit word from the PoC DRAM view (observed by SMMU hardware).
  std::optional<uint64_t> ReadPocDram64(paddr_t pa);
  // Read a 64-bit word from the CPU cache view.
  std::optional<uint64_t> ReadCpuCache64(paddr_t pa);

  void SetObservers(PteWriteObserver on_pte_write, CacheCleanObserver on_cache_clean,
                    PageFreeObserver on_page_free);
  void ClearObservers();

 private:
  PmmMock() = default;
  ~PmmMock() = default;

  static PmmMock instance_;

  std::unordered_map<paddr_t, std::unique_ptr<vm_page_t>> allocated_pages_;
  std::mutex lock_;
  std::optional<uint64_t> alloc_limit_;

  PteWriteObserver on_pte_write_;
  CacheCleanObserver on_cache_clean_;
  PageFreeObserver on_page_free_;
};

inline PmmMock PmmMock::instance_;

zx_status_t pmm_alloc_page(uint32_t alloc_flags, vm_page_t** p);
zx_status_t pmm_alloc_page(uint32_t alloc_flags, vm_page_t** p, paddr_t* pa);

vm_page_t* paddr_to_vm_page(paddr_t pa);

void pmm_free(VmPageDoublyLinkedList* list);

#endif  // ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_VM_PMM_H_
