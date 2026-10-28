// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_VM_PAGE_H_
#define ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_VM_PAGE_H_

#include <sys/types.h>
#include <zircon/assert.h>

#include <memory>

#include <fbl/intrusive_double_list.h>
#include <vm/physmap.h>

constexpr uint32_t kPageShift = 12;
const size_t kPageSize = 4096;
constexpr size_t kPageMask = kPageSize - 1;

constexpr bool IsPageRounded(uint64_t val) { return (val & kPageMask) == 0; }
constexpr uint64_t RoundUpPageSize(uint64_t val) {
  return (val + kPageMask) & ~uint64_t{kPageMask};
}

struct vm_page {
  struct PageDeleter {
    void operator()(uint8_t* p) { operator delete[](p, std::align_val_t(kPageSize)); }
  };

  paddr_t paddr() const { return physmap_to_paddr(paddr_priv.get()); }

  using NodeState =
      fbl::DoublyLinkedListNodeState<vm_page*, fbl::NodeOptions::AllowRemoveFromContainer>;
  NodeState queue_node;
  // CPU data-cache view (returned by paddr_to_physmap).
  std::unique_ptr<uint8_t[], PageDeleter> paddr_priv;
  // Point-of-Coherency (PoC) physical DRAM view observed by non-coherent SMMU
  // hardware table walks. Updated when arch::CleanDataCacheRange is called.
  std::unique_ptr<uint8_t[], PageDeleter> poc_dram_priv;
};

typedef struct vm_page vm_page_t;

struct vm_page_node_traits {
  static vm_page_t::NodeState& node_state(vm_page& obj) { return obj.queue_node; }
};

using VmPageDoublyLinkedList = fbl::DoublyLinkedList<vm_page_t*, fbl::DefaultObjectTag,
                                                     fbl::SizeOrder::N, vm_page_node_traits>;

#endif  // ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_VM_PAGE_H_
