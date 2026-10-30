// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/fit/defer.h>
#include <stdio.h>
#include <string.h>

#include <algorithm>
#include <utility>
#include <vector>

#include <dev/arm_smmu/page_cache.h>
#include <zxtest/zxtest.h>

#include "cross_test_ffi.h"

namespace arm_smmu {
namespace {

bool IsPageZeroed(const vm_page_t* page) {
  auto* p = static_cast<const uint8_t*>(paddr_to_physmap(page->paddr()));
  for (size_t i = 0; i < kPageSize; ++i) {
    if (p[i] != 0)
      return false;
  }
  return true;
}

class PageCacheBackend {
 public:
  explicit PageCacheBackend(BackendKind kind) : kind_(kind) {
    if (kind_ == BackendKind::kRust) {
      rust_cache_ = rust_page_cache_create();
      ZX_ASSERT(rust_cache_ != nullptr);
    }
  }

  PageCacheBackend(const PageCacheBackend&) = delete;
  PageCacheBackend& operator=(const PageCacheBackend&) = delete;

  ~PageCacheBackend() {
    if (rust_cache_ != nullptr) {
      rust_page_cache_destroy(std::exchange(rust_cache_, nullptr));
    }
  }

  zx::result<vm_page_t*> GetPage() {
    if (kind_ == BackendKind::kRust) {
      vm_page_t* page = nullptr;
      const zx_status_t status = rust_page_cache_get_page(rust_cache_, &page);
      if (status != ZX_OK) {
        return zx::error(status);
      }
      return zx::ok(page);
    }
    return cpp_cache_.GetPage();
  }

  void ReturnPage(vm_page_t* page) {
    if (kind_ == BackendKind::kRust) {
      rust_page_cache_return_page(rust_cache_, page);
      return;
    }
    cpp_cache_.ReturnPage(page);
  }

  void Trim(size_t max_cached) {
    if (kind_ == BackendKind::kRust) {
      rust_page_cache_trim(rust_cache_, max_cached);
      return;
    }
    cpp_cache_.Trim(static_cast<uint32_t>(max_cached));
  }

  size_t in_flight_pages() const {
    if (kind_ == BackendKind::kRust) {
      return rust_page_cache_in_flight_pages(rust_cache_);
    }
    return cpp_cache_.in_flight_pages();
  }

  size_t cache_entries() const {
    if (kind_ == BackendKind::kRust) {
      return rust_page_cache_cache_entries(rust_cache_);
    }
    return cpp_cache_.cache_entries();
  }

 private:
  BackendKind kind_;
  PageCache cpp_cache_;
  RustPageCache* rust_cache_ = nullptr;
};

TEST(PageCacheTest, AddAndGet) {
  for (BackendKind kind : {BackendKind::kCpp, BackendKind::kRust}) {
    printf("[Backend: %s] PageCacheTest.AddAndGet\n", BackendTag(kind));
    fflush(stdout);

    PageCacheBackend cache(kind);
    std::vector<vm_page_t*> checked_out;
    auto unwind_cleanup = fit::defer([&]() {
      for (vm_page_t* page : checked_out) {
        memset(paddr_to_physmap(page->paddr()), 0, kPageSize);
        cache.ReturnPage(page);
      }
      cache.Trim(0);
    });
    const auto untrack_page = [&](vm_page_t* page) {
      auto it = std::find(checked_out.begin(), checked_out.end(), page);
      if (it != checked_out.end()) {
        checked_out.erase(it);
      }
    };

    EXPECT_EQ(0, cache.in_flight_pages());
    EXPECT_EQ(0, cache.cache_entries());

    auto result1 = cache.GetPage();
    ASSERT_TRUE(result1.is_ok());
    vm_page_t* page1 = result1.value();
    checked_out.push_back(page1);
    EXPECT_TRUE(IsPageZeroed(page1));
    EXPECT_EQ(1, cache.in_flight_pages());
    EXPECT_EQ(0, cache.cache_entries());

    auto result2 = cache.GetPage();
    ASSERT_TRUE(result2.is_ok());
    vm_page_t* page2 = result2.value();
    checked_out.push_back(page2);
    EXPECT_TRUE(IsPageZeroed(page2));
    EXPECT_EQ(2, cache.in_flight_pages());
    EXPECT_EQ(0, cache.cache_entries());

    cache.ReturnPage(page1);
    untrack_page(page1);
    EXPECT_EQ(1, cache.in_flight_pages());
    EXPECT_EQ(1, cache.cache_entries());

    cache.ReturnPage(page2);
    untrack_page(page2);
    EXPECT_EQ(0, cache.in_flight_pages());
    EXPECT_EQ(2, cache.cache_entries());

    auto get_result1 = cache.GetPage();
    ASSERT_TRUE(get_result1.is_ok());
    checked_out.push_back(get_result1.value());
    EXPECT_EQ(get_result1.value(), page2);  // We expect stack behavior, not queue behavior.
    EXPECT_TRUE(IsPageZeroed(get_result1.value()));
    EXPECT_EQ(1, cache.in_flight_pages());
    EXPECT_EQ(1, cache.cache_entries());

    auto get_result2 = cache.GetPage();
    ASSERT_TRUE(get_result2.is_ok());
    checked_out.push_back(get_result2.value());
    EXPECT_EQ(get_result2.value(), page1);  // We expect stack behavior, not queue behavior.
    EXPECT_TRUE(IsPageZeroed(get_result2.value()));
    EXPECT_EQ(2, cache.in_flight_pages());
    EXPECT_EQ(0, cache.cache_entries());

    // Cleanup: return to cache and Trim(0)
    cache.ReturnPage(page1);
    untrack_page(page1);
    EXPECT_EQ(1, cache.in_flight_pages());
    EXPECT_EQ(1, cache.cache_entries());

    cache.ReturnPage(page2);
    untrack_page(page2);
    EXPECT_EQ(0, cache.in_flight_pages());
    EXPECT_EQ(2, cache.cache_entries());

    cache.Trim(0);
    EXPECT_EQ(0, cache.in_flight_pages());
    EXPECT_EQ(0, cache.cache_entries());

    EXPECT_EQ(0u, PmmMock::Get().GetAllocatedPageCount());
    unwind_cleanup.cancel();
  }
}

TEST(PageCacheTest, Trim) {
  for (BackendKind kind : {BackendKind::kCpp, BackendKind::kRust}) {
    printf("[Backend: %s] PageCacheTest.Trim\n", BackendTag(kind));
    fflush(stdout);

    PageCacheBackend cache(kind);
    std::vector<vm_page_t*> checked_out;
    auto unwind_cleanup = fit::defer([&]() {
      for (vm_page_t* page : checked_out) {
        memset(paddr_to_physmap(page->paddr()), 0, kPageSize);
        cache.ReturnPage(page);
      }
      cache.Trim(0);
    });

    vm_page_t* pages[5];
    for (int i = 0; i < 5; ++i) {
      auto result = cache.GetPage();
      ASSERT_TRUE(result.is_ok());
      pages[i] = result.value();
      checked_out.push_back(pages[i]);
      EXPECT_TRUE(IsPageZeroed(pages[i]));
      EXPECT_EQ(i + 1, cache.in_flight_pages());
      EXPECT_EQ(0, cache.cache_entries());
    }

    for (int i = 0; i < 5;) {
      vm_page_t* p = pages[i++];
      cache.ReturnPage(p);
      auto it = std::find(checked_out.begin(), checked_out.end(), p);
      if (it != checked_out.end()) {
        checked_out.erase(it);
      }
      EXPECT_EQ(5 - i, cache.in_flight_pages());
      EXPECT_EQ(i, cache.cache_entries());
    }

    for (int i = 5; i > 0;) {
      --i;
      cache.Trim(i);
      EXPECT_EQ(0, cache.in_flight_pages());
      EXPECT_EQ(i, cache.cache_entries());
    }

    EXPECT_EQ(0u, PmmMock::Get().GetAllocatedPageCount());
    unwind_cleanup.cancel();
  }
}

}  // namespace
}  // namespace arm_smmu
