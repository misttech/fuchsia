// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_PAGE_CACHE_INCLUDE_LIB_PAGE_CACHE_H_
#define ZIRCON_KERNEL_LIB_PAGE_CACHE_INCLUDE_LIB_PAGE_CACHE_H_

#include <assert.h>
#include <lib/zx/result.h>
#include <zircon/types.h>

#include <ktl/utility.h>
#include <vm/page.h>
#include <vm/pmm.h>

// PageCache provides a front end to the PMM that reserves a given number of
// pages in per-CPU caches to reduce contention on the PMM.
//
// The implementation lives in Rust (`//zircon/kernel/lib/page_cache/page_cache.rs`).

namespace page_cache {
class PageCache;
}  // namespace page_cache

extern "C" {
zx_status_t rust_page_cache_create(size_t reserve_pages, page_cache::PageCache* out_cache);
void rust_page_cache_destroy(page_cache::PageCache* cache);
void rust_page_cache_seed_random_should_wait(page_cache::PageCache* cache);
zx_status_t rust_page_cache_allocate(page_cache::PageCache* cache, size_t page_count,
                                     uint alloc_flags, VmPageDoublyLinkedList* out_pages,
                                     size_t* out_available_pages);
void rust_page_cache_free(page_cache::PageCache* cache, VmPageDoublyLinkedList* page_list);
}

namespace page_cache {

class PageCache {
 public:
  // Creates a page cache with the given number of reserve pages per CPU.
  // Filling the per-CPU page caches is deferred until the first allocation
  // request.
  static zx::result<PageCache> Create(size_t reserve_pages) {
    PageCache cache;
    zx_status_t status = rust_page_cache_create(reserve_pages, &cache);
    if (status != ZX_OK) {
      return zx::error_result(status);
    }
    return zx::ok(ktl::move(cache));
  }

  constexpr PageCache() = default;
  ~PageCache() { Reset(); }

  // PageCache is not copiable.
  PageCache(const PageCache&) = delete;
  PageCache& operator=(const PageCache&) = delete;

  // PageCache is movable.
  PageCache(PageCache&& other) noexcept
      : reserve_pages_(ktl::exchange(other.reserve_pages_, 0)),
        per_cpu_caches_{
            ktl::exchange(other.per_cpu_caches_[0], 0),
            ktl::exchange(other.per_cpu_caches_[1], 0),
        } {}
  PageCache& operator=(PageCache&& other) noexcept {
    if (this != &other) {
      Reset();
      reserve_pages_ = ktl::exchange(other.reserve_pages_, 0);
      per_cpu_caches_[0] = ktl::exchange(other.per_cpu_caches_[0], 0);
      per_cpu_caches_[1] = ktl::exchange(other.per_cpu_caches_[1], 0);
    }
    return *this;
  }

  // Returns true if this PageCache instance is non-empty.
  explicit operator bool() const { return per_cpu_caches_[0] != 0 || per_cpu_caches_[1] != 0; }

  // Utility type for returning a list of pages via zx::result. Automatically
  // frees a non-empty list of pages on destruction to improve safety and
  // ergonomics.
  struct PageList : VmPageDoublyLinkedList {
    PageList() = default;

    ~PageList() {
      if (!is_empty()) {
        pmm_free(this);
      }
    }

    PageList(VmPageDoublyLinkedList&& other) { splice(end(), other); }

    PageList(PageList&& other) noexcept { splice(end(), other); }

    PageList& operator=(PageList&& other) noexcept {
      if (!is_empty()) {
        pmm_free(this);
      }
      splice(end(), other);
      return *this;
    }

    PageList(const PageList&) = delete;
    PageList& operator=(const PageList&) = delete;
  };

  struct AllocateResult {
    // The list of pages allocated by the request.
    PageList page_list{};

    // The number of pages remaining in the cache after the request.
    size_t available_pages{0};
  };

  // Allocates the given number of pages from the page cache. Falls back to the
  // PMM if the cache is insufficient to fulfill the request.
  zx::result<AllocateResult> Allocate(size_t page_count, uint alloc_flags = 0) {
    AllocateResult result;
    zx_status_t status = rust_page_cache_allocate(this, page_count, alloc_flags, &result.page_list,
                                                  &result.available_pages);
    if (status != ZX_OK) {
      return zx::error_result(status);
    }
    return zx::ok(ktl::move(result));
  }

  // Returns the given pages to the page cache. Excess pages are returned to the
  // PMM.
  void Free(PageList page_list) { rust_page_cache_free(this, &page_list); }

  size_t reserve_pages() const { return reserve_pages_; }

  void SeedRandomShouldWait() { rust_page_cache_seed_random_should_wait(this); }

 private:
  void Reset() {
    if (*this) {
      rust_page_cache_destroy(this);
      reserve_pages_ = 0;
      per_cpu_caches_[0] = 0;
      per_cpu_caches_[1] = 0;
    }
  }

  size_t reserve_pages_{0};
  uintptr_t per_cpu_caches_[2]{0, 0};
};

static_assert(sizeof(PageCache) == 24);
static_assert(alignof(PageCache) == 8);

}  // namespace page_cache

#endif  // ZIRCON_KERNEL_LIB_PAGE_CACHE_INCLUDE_LIB_PAGE_CACHE_H_
