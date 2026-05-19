// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_INCLUDE_DEV_ARM_SMMU_PAGE_CACHE_H_
#define ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_INCLUDE_DEV_ARM_SMMU_PAGE_CACHE_H_

#include <lib/arch/cache.h>
#include <lib/zx/result.h>
#include <stdint.h>
#include <zircon/listnode.h>

#include <vm/page.h>
#include <vm/physmap.h>
#include <vm/pmm.h>

namespace arm_smmu {

class PageCache {
 public:
  PageCache() { list_initialize(&page_cache_); }

  ~PageCache() {
    DEBUG_ASSERT(!cache_entries_);
    DEBUG_ASSERT(!in_flight_pages_);
    DEBUG_ASSERT(list_is_empty(&page_cache_));
  }

  // Return a page to the cache which the caller had fetched using GetPage.
  void ReturnPage(vm_page_t* page) {
    DEBUG_ASSERT(in_flight_pages_ > 0);
    list_add_head(&page_cache_, &page->queue_node);
    ++cache_entries_;
    --in_flight_pages_;
  }

  zx::result<vm_page_t*> GetPage() {
    vm_page_t* ret{nullptr};

    if (!list_is_empty(&page_cache_)) {
      DEBUG_ASSERT(cache_entries_ > 0);
      --cache_entries_;
      ++in_flight_pages_;
      return zx::ok(list_remove_head_type(&page_cache_, vm_page_t, queue_node));
    }

    DEBUG_ASSERT(cache_entries_ == 0);
    if (const zx_status_t status = pmm_alloc_page(PMM_ALLOC_FLAG_ANY, &ret); status != ZX_OK) {
      return zx::error(status);
    }

    // Pages from the PMM are not zeroed automatically for us.  Explicitly zero
    // the page now.  Note, we do not need to zero pages which come from the
    // cache.  All pages which came from the PMM are zeroed before being
    // returned from GetPage, and all pages returned to the cache need to be
    // zeroed by the user before going back into the cache.  Host tests of the
    // page cache and device aspace code currently verify this behavior.
    paddr_t page_paddr = ret->paddr();
    void* page_vaddr = paddr_to_physmap(page_paddr);
    memset(page_vaddr, 0, kPageSize);
    arch::CleanDataCacheRange(reinterpret_cast<uintptr_t>(page_vaddr), kPageSize);
    ++in_flight_pages_;

    return zx::ok(ret);
  }

  void Trim(uint32_t max_pages) {
    if (max_pages >= cache_entries_) {
      return;
    }

    // If max_pages is zero, just return our page cache list to the PMM.
    // Otherwise, count off max_pages from our list, then split our list and
    // return the extra pages to the PMM.
    if (max_pages == 0) {
      pmm_free(&page_cache_);
      DEBUG_ASSERT(list_is_empty(&page_cache_));
    } else {
      list_node* split_point = list_peek_head(&page_cache_);
      for (uint32_t i = 1; i < max_pages; ++i) {
        split_point = list_next(&page_cache_, split_point);
      }
      list_node free_me;
      list_split_after(&page_cache_, split_point, &free_me);
      pmm_free(&free_me);
      DEBUG_ASSERT(list_is_empty(&free_me));
    }

    cache_entries_ = max_pages;
  }

  uint32_t cache_entries() const { return cache_entries_; }
  uint32_t in_flight_pages() const { return in_flight_pages_; }

 private:
  list_node page_cache_;
  uint32_t cache_entries_{0};
  uint32_t in_flight_pages_{0};
};

}  // namespace arm_smmu

#endif  // ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_INCLUDE_DEV_ARM_SMMU_PAGE_CACHE_H_
