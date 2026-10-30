// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/fit/defer.h>
#include <stdio.h>

#include <algorithm>
#include <map>
#include <optional>
#include <utility>
#include <vector>

#include <dev/arm_smmu/device_aspace.h>
#include <dev/arm_smmu/page_cache.h>
#include <dev/arm_smmu/vmsav8_64.h>
#include <vm/pinned_vm_object.h>
#include <zxtest/zxtest.h>

#include "cross_test_ffi.h"

namespace arm_smmu {

using namespace vmsav8_64;

namespace {

class MockVmObject : public VmObject {
 public:
  using InitList = std::initializer_list<std::map<size_t, paddr_t>::value_type>;

  MockVmObject(InitList init_list) : mappings_{init_list} {
    SetLookupHook([this](size_t offset, size_t size, paddr_t* out_paddr) -> zx_status_t {
      if (size != 4096) {  // 4k page size
        return ZX_ERR_INVALID_ARGS;
      }

      auto it = mappings_.find(offset);
      if (it != mappings_.end()) {
        *out_paddr = it->second;
        return ZX_OK;
      }

      return ZX_ERR_NOT_FOUND;
    });
  }

 private:
  std::map<size_t, paddr_t> mappings_;
};

}  // namespace

class DeviceAspaceTest : public zxtest::Test {
 public:
  static uint64_t StaticRootTtPaddr(const DeviceAspace* aspace) {
    if (aspace == nullptr || aspace->root_tt_page_ == nullptr) {
      return 0;
    }
    return aspace->root_tt_page_->paddr();
  }

  static zx_status_t StaticTestRegionContainedBy(const DeviceAspace* aspace, uint64_t base,
                                                 uint64_t size, bool is_allocated_set,
                                                 bool* out_contained) {
    if (aspace == nullptr || out_contained == nullptr) {
      return ZX_ERR_INVALID_ARGS;
    }
    const auto which = is_allocated_set ? RegionAllocator::TestRegionSet::Allocated
                                        : RegionAllocator::TestRegionSet::Available;
    const zx::result<bool> res =
        aspace->avail_regions_.TestRegionContainedBy({.base = base, .size = size}, which);
    if (res.is_error()) {
      return res.status_value();
    }
    *out_contained = res.value();
    return ZX_OK;
  }

 protected:
  struct AllocationHandle {
    uint64_t base = 0;
    uint64_t size = 0;
    DeviceAspace::Allocation cpp_alloc;
    RustDeviceAspaceAlloc* rust_alloc = nullptr;

    AllocationHandle() = default;
    AllocationHandle(const AllocationHandle&) = delete;
    AllocationHandle& operator=(const AllocationHandle&) = delete;
    AllocationHandle(AllocationHandle&& other) noexcept
        : base(other.base),
          size(other.size),
          cpp_alloc(ktl::move(other.cpp_alloc)),
          rust_alloc(std::exchange(other.rust_alloc, nullptr)) {}
    AllocationHandle& operator=(AllocationHandle&& other) noexcept {
      if (this != &other) {
        Reset();
        base = other.base;
        size = other.size;
        cpp_alloc = ktl::move(other.cpp_alloc);
        rust_alloc = std::exchange(other.rust_alloc, nullptr);
      }
      return *this;
    }
    ~AllocationHandle() { Reset(); }

    void Reset() {
      cpp_alloc.reset();
      if (rust_alloc != nullptr) {
        rust_device_aspace_drop_allocation(std::exchange(rust_alloc, nullptr));
      }
    }
  };

  struct TrackedMapping {
    fbl::RefPtr<MockVmObject> vmo;
    AllocationHandle alloc;
    uint32_t perms;
  };

  // 48 bit address space, starting at 1 MB
  static constexpr uint64_t kDefaultASpaceStart = uint64_t{1} << 20;
  static constexpr uint64_t kDefaultASpaceEnd = uint64_t{1} << 48;
  static constexpr uint64_t kDefaultASpaceLen = kDefaultASpaceEnd - kDefaultASpaceStart;

  // Don't cache any pages.  This makes it easier to assert that we have release
  // the proper number of pages as we go through the tests.
  static constexpr uint32_t kMaxCachePages = 0;

  static constexpr uint32_t kPageSize = 4096;
  static constexpr uint32_t kPageMask = kPageSize - 1;

  static void TlbInvalThunk(void* thiz, uint64_t base, uint64_t size) {
    reinterpret_cast<DeviceAspaceTest*>(thiz)->TlbInval(base, size);
  }

  void TearDown() override { EmergencyCleanup(); }

  void EmergencyCleanup() {
    PmmMock::Get().ClearAllocationLimit();
    // Drop all remaining allocations before freeing translation tables or destroying the aspace.
    tracked_mappings_.clear();
    if ((aspace_ != nullptr || rust_aspace_ != nullptr) && !tables_freed_) {
      AspaceFreeTranslationTables();
    }
    if (rust_aspace_ != nullptr) {
      rust_device_aspace_destroy(std::exchange(rust_aspace_, nullptr));
    }
    aspace_.reset();
    invalidations_.clear();
    tables_freed_ = false;
  }

  void InitBackend(BackendKind kind, const char* test_name) {
    EmergencyCleanup();

    kind_ = kind;
    printf("[Backend: %s] DeviceAspaceTest.%s\n", BackendTag(kind_), test_name);
    fflush(stdout);

    PmmMock::Get().ClearAllocationLimit();
    tracked_mappings_.clear();
    invalidations_.clear();
    tables_freed_ = false;

    if (kind_ == BackendKind::kRust) {
      RustDeviceAspace* raw = nullptr;
      const zx_status_t status =
          rust_device_aspace_create(kDefaultASpaceStart, kDefaultASpaceLen, kMaxCachePages, &raw);
      ASSERT_EQ(ZX_OK, status);
      rust_aspace_ = raw;
    } else {
      zx::result<ktl::unique_ptr<DeviceAspace>> aspace_res =
          DeviceAspace::Create(kDefaultASpaceStart, kDefaultASpaceLen, kMaxCachePages);
      ASSERT_EQ(ZX_OK, aspace_res.status_value());
      aspace_ = ktl::move(aspace_res.value());
    }
  }

  void CleanupBackend() {
    EXPECT_EQ(0u, tracked_mappings_.size());
    tracked_mappings_.clear();
    invalidations_.clear();

    const size_t pages_before = PmmMock::Get().GetAllocatedPageCount();
    AspaceFreeTranslationTables();
    const size_t pages_after = PmmMock::Get().GetAllocatedPageCount();
    const size_t remaining_cache_entries = AspaceCacheEntries();

    if (kind_ == BackendKind::kRust) {
      if (rust_aspace_ != nullptr) {
        rust_device_aspace_destroy(std::exchange(rust_aspace_, nullptr));
      }
    } else {
      aspace_.reset();
    }
    tables_freed_ = false;

    ASSERT_EQ(1u, invalidations_.size());
    EXPECT_EQ(0u, invalidations_[0].base);
    EXPECT_EQ(0u, invalidations_[0].size);
    EXPECT_EQ(pages_before, invalidations_[0].pmm_pages_during_inval);

    const size_t pages_freed = pages_before - pages_after;
    if (invalidations_[0].cache_entries_during_inval.has_value()) {
      EXPECT_EQ(pages_freed, *invalidations_[0].cache_entries_during_inval);
    }
    EXPECT_EQ(0u, remaining_cache_entries);
  }

  uint64_t AspaceStart() const {
    if (kind_ == BackendKind::kRust) {
      return rust_device_aspace_aspace_start(rust_aspace_);
    }
    return aspace_->aspace_start_;
  }

  uint64_t AspaceLen() const {
    if (kind_ == BackendKind::kRust) {
      return rust_device_aspace_aspace_len(rust_aspace_);
    }
    return aspace_->aspace_len_;
  }

  size_t AspaceCacheEntries() const {
    if (kind_ == BackendKind::kRust) {
      return rust_device_aspace_cache_entries(rust_aspace_);
    }
    return aspace_->page_cache().cache_entries();
  }

  uint64_t AspaceRootTtPaddr() const {
    if (kind_ == BackendKind::kRust) {
      return rust_aspace_ != nullptr ? rust_device_aspace_root_tt_paddr(rust_aspace_) : 0;
    }
    return StaticRootTtPaddr(aspace_.get());
  }

  zx::result<bool> AspaceTestRegionContainedBy(uint64_t base, uint64_t size,
                                               RegionAllocator::TestRegionSet set) const {
    const bool is_allocated_set = (set == RegionAllocator::TestRegionSet::Allocated);
    bool contained = false;
    zx_status_t status = ZX_OK;
    if (kind_ == BackendKind::kRust) {
      status = rust_device_aspace_test_region_contained_by(rust_aspace_, base, size,
                                                           is_allocated_set, &contained);
    } else {
      status = StaticTestRegionContainedBy(aspace_.get(), base, size, is_allocated_set, &contained);
    }
    if (status != ZX_OK) {
      return zx::error(status);
    }
    return zx::ok(contained);
  }

  zx::result<AllocationHandle> AspaceMap(const PinnedVmObject& pinned_vmo, uint32_t perms,
                                         ktl::optional<uint64_t> location = ktl::nullopt) {
    if (kind_ == BackendKind::kRust) {
      RustDeviceAspaceAlloc* raw_alloc = nullptr;
      uint64_t base = 0;
      uint64_t size = 0;
      const zx_status_t status = rust_device_aspace_map(
          rust_aspace_, &pinned_vmo, perms, TlbInvalThunk, this, location.has_value(),
          location.value_or(0), &raw_alloc, &base, &size);
      if (status != ZX_OK) {
        return zx::error(status);
      }
      AllocationHandle handle;
      handle.base = base;
      handle.size = size;
      handle.rust_alloc = raw_alloc;
      return zx::ok(std::move(handle));
    }
    zx::result<DeviceAspace::Allocation> alloc_res =
        aspace_->Map(pinned_vmo, perms, tlb_inval_op_, location);
    if (!alloc_res.is_ok()) {
      return zx::error(alloc_res.status_value());
    }
    AllocationHandle handle;
    handle.base = (*alloc_res)->base;
    handle.size = (*alloc_res)->size;
    handle.cpp_alloc = ktl::move(alloc_res.value());
    return zx::ok(std::move(handle));
  }

  void AspaceUnmap(AllocationHandle alloc) {
    if (kind_ == BackendKind::kRust) {
      rust_device_aspace_unmap(rust_aspace_, std::exchange(alloc.rust_alloc, nullptr),
                               TlbInvalThunk, this);
      return;
    }
    aspace_->Unmap(ktl::move(alloc.cpp_alloc), tlb_inval_op_);
  }

  void AspaceFreeTranslationTables() {
    tables_freed_ = true;
    if (kind_ == BackendKind::kRust) {
      rust_device_aspace_free_translation_tables(rust_aspace_, TlbInvalThunk, this);
      return;
    }
    aspace_->FreeTranslationTables(tlb_inval_op_);
  }

  // Helper method to map a PinnedVmObject and track the allocation.
  // Returns the allocated base virtual address, or an error status on failure.
  zx::result<uint64_t> MapAndTrack(fbl::RefPtr<MockVmObject> vmo, const PinnedVmObject& pinned_vmo,
                                   uint32_t perms,
                                   ktl::optional<uint64_t> location = ktl::nullopt) {
    invalidations_.clear();
    zx::result<AllocationHandle> alloc_res = AspaceMap(pinned_vmo, perms, location);
    if (!alloc_res.is_ok()) {
      return zx::error(alloc_res.status_value());
    }

    AllocationHandle alloc = std::move(alloc_res.value());
    const uint64_t base = alloc.base;
    const uint64_t size = alloc.size;

    EXPECT_EQ(alloc.size, pinned_vmo.size() - pinned_vmo.offset());
    EXPECT_GE(alloc.base, AspaceStart());
    EXPECT_LT(alloc.base, AspaceStart() + AspaceLen());

    if (invalidations_.size() != 1u) {
      EXPECT_EQ(1u, invalidations_.size());
      return zx::error(ZX_ERR_INTERNAL);
    }
    EXPECT_EQ(base, invalidations_[0].base);
    EXPECT_EQ(size, invalidations_[0].size);

    auto [it, inserted] =
        tracked_mappings_.emplace(base, TrackedMapping{vmo, std::move(alloc), perms});
    EXPECT_TRUE(inserted);

    return zx::ok(base);
  }

  // Helper method to unmap a tracked mapping by its base virtual address
  // and run verification.
  void UnmapAndUntrack(uint64_t base) {
    auto it = tracked_mappings_.find(base);
    ASSERT_TRUE(it != tracked_mappings_.end());

    AllocationHandle alloc = std::move(it->second.alloc);
    const uint64_t size = alloc.size;
    tracked_mappings_.erase(it);

    invalidations_.clear();
    const size_t pages_before = PmmMock::Get().GetAllocatedPageCount();
    AspaceUnmap(std::move(alloc));
    const size_t pages_after = PmmMock::Get().GetAllocatedPageCount();

    ASSERT_EQ(1u, invalidations_.size());
    EXPECT_EQ(base, invalidations_[0].base);
    EXPECT_EQ(size, invalidations_[0].size);
    EXPECT_EQ(pages_before, invalidations_[0].pmm_pages_during_inval);

    const size_t pages_freed = pages_before - pages_after;
    if (invalidations_[0].cache_entries_during_inval.has_value()) {
      EXPECT_EQ(pages_freed, *invalidations_[0].cache_entries_during_inval);
    }
    EXPECT_EQ(0u, AspaceCacheEntries());
  }

  // Helper to kick off the recursive verification of the page table against the
  // region allocator's state. This checks that every mapped and unmapped region
  // is correctly accounted for in the page table structure.
  void VerifyAllocatedRegions() {
    const uint64_t root_pa = AspaceRootTtPaddr();
    if (root_pa == 0) {
      EXPECT_EQ(0u, PmmMock::Get().GetAllocatedPageCount());
      return;
    }
    const uint64_t* pt_root = static_cast<const uint64_t*>(paddr_to_physmap(root_pa));
    size_t table_pages = VerifyAllocatedRegionsRecursive(pt_root, 0, 0);
    EXPECT_EQ(table_pages, PmmMock::Get().GetAllocatedPageCount());
  }

  // A callback we inject into Map and Unmap operations which the operation will
  // call in order to "invalidate our TLBs" at the proper points in the
  // map/unmap operations.
  //
  // Note: On `BackendKind::kRust`, `&mut RustDeviceAspace` is active on the stack across
  // `TlbInval`, so `rust_aspace_` must not be accessed reentrantly here. Checking
  // `pmm_pages_during_inval` before `page_cache.trim(0)` (paired with `pages_after` right after
  // `trim(0)`) verifies that reclaimed table pages remain held in the page cache until TLB
  // invalidation completes.
  void TlbInval(uint64_t base, uint64_t size) {
    invalidations_.push_back({
        .base = base,
        .size = size,
        .pmm_pages_during_inval = PmmMock::Get().GetAllocatedPageCount(),
        .cache_entries_during_inval =
            (kind_ == BackendKind::kCpp)
                ? std::optional<size_t>{aspace_->page_cache().cache_entries()}
                : std::nullopt,
    });
  }

 private:
  // Walk the page table tree recursively and verify that the translation tables
  // are in sync with the RegionAllocator's state.
  //
  // Specifically:
  // 1. Any valid leaf entry must be fully contained within the set of regions
  //    marked as "Allocated".
  // 2. Any valid leaf entry must map to the expected physical address of the
  //    corresponding tracked MockVmObject and have the correct access permissions.
  // 3. Any invalid entry (or portion of an invalid entry) that falls within the
  //    bounds of the active address space must be fully contained within the
  //    set of regions marked as "Available" (free/unmapped space).
  // 4. There cannot be any Block entries (only Table or Page entries).
  // 5. Invalid entries must be strictly zero, not just missing the valid bit.
  //
  // Returns the total number of translation table pages encountered, including the
  // current table.
  size_t VerifyAllocatedRegionsRecursive(const uint64_t* table, uint64_t vaddr, int level) {
    size_t table_pages = 1;

    const uint64_t aspace_start = AspaceStart();
    const uint64_t aspace_end = aspace_start + AspaceLen();
    for (uint32_t i = 0; i < kEntriesPerPage; ++i) {
      const uint64_t e = table[i];
      const uint64_t entry_vaddr = vaddr | (static_cast<uint64_t>(i) << (39 - 9 * level));
      const uint64_t entry_size = uint64_t{1} << (39 - 9 * level);
      const uint64_t entry_end = entry_vaddr + entry_size;

      if (IsValidEntry(e)) {
        EXPECT_FALSE(IsBlockEntry(e, level));

        if (level < 3 && IsTableEntry(e)) {
          const paddr_t next_pa = GetTableEntryPAddr(e);
          const uint64_t* next_table = static_cast<const uint64_t*>(paddr_to_physmap(next_pa));
          table_pages += VerifyAllocatedRegionsRecursive(next_table, entry_vaddr, level + 1);
        } else {
          {
            const zx::result<bool> res = AspaceTestRegionContainedBy(
                entry_vaddr, entry_size, RegionAllocator::TestRegionSet::Allocated);
            EXPECT_OK(res.status_value());
            EXPECT_TRUE(res.is_ok() && res.value());
          }

          bool found_mapping = false;
          // Find the first mapping whose base address is > entry_vaddr.
          // If such a mapping exists, the mapping which *might* contain
          // entry_vaddr must be the one immediately preceding it.
          // If it == begin(), it means either the map is empty, or all mappings
          // start after entry_vaddr, meaning this vaddr maps to nothing tracked.
          // If it != begin(), we can safely decrement it to find the candidate mapping
          // and check if its [base, base + size) range actually covers entry_vaddr.
          auto it = tracked_mappings_.upper_bound(entry_vaddr);
          if (it != tracked_mappings_.begin()) {
            --it;
            const auto& mapping = it->second;
            if (entry_vaddr >= mapping.alloc.base &&
                entry_vaddr < mapping.alloc.base + mapping.alloc.size) {
              found_mapping = true;

              paddr_t expected_pa = 0;
              zx_status_t status = mapping.vmo->LookupContiguous(entry_vaddr - mapping.alloc.base,
                                                                 kPageSize, &expected_pa);
              EXPECT_EQ(ZX_OK, status);
              EXPECT_EQ(expected_pa, e & kValidAddrMask);

              uint64_t expected_perms = GetPageEntryPerms(mapping.perms);
              constexpr uint64_t kPermBits = MakePerms(1, 1, 3);
              EXPECT_EQ(expected_perms, e & kPermBits);
            }
          }
          EXPECT_TRUE(found_mapping);
        }
      } else {
        EXPECT_EQ(e, 0u);
        const uint64_t check_start = std::max(entry_vaddr, aspace_start);
        const uint64_t check_end = std::min(entry_end, aspace_end);
        if (check_start < check_end) {
          const zx::result<bool> res = AspaceTestRegionContainedBy(
              check_start, check_end - check_start, RegionAllocator::TestRegionSet::Available);
          EXPECT_OK(res.status_value());
          EXPECT_TRUE(res.is_ok() && res.value());
        }
      }
    }

    return table_pages;
  }

 protected:
  struct InvalidationRecord {
    uint64_t base;
    uint64_t size;
    size_t pmm_pages_during_inval;
    std::optional<size_t> cache_entries_during_inval;
  };
  BackendKind kind_ = BackendKind::kCpp;
  bool tables_freed_ = false;
  std::vector<InvalidationRecord> invalidations_;

  ktl::unique_ptr<DeviceAspace> aspace_;
  RustDeviceAspace* rust_aspace_ = nullptr;
  std::map<uint64_t, TrackedMapping> tracked_mappings_;
  DeviceAspace::TlbInvalOp tlb_inval_op_{TlbInvalThunk, this};
};

uint64_t GetCppDeviceAspaceRootTtPaddr(const DeviceAspace* aspace) {
  return DeviceAspaceTest::StaticRootTtPaddr(aspace);
}

zx_status_t TestCppDeviceAspaceRegionContainedBy(const DeviceAspace* aspace, uint64_t base,
                                                 uint64_t size, bool is_allocated_set,
                                                 bool* out_contained) {
  return DeviceAspaceTest::StaticTestRegionContainedBy(aspace, base, size, is_allocated_set,
                                                       out_contained);
}

TEST_F(DeviceAspaceTest, MapAndUnmap) {
  for (BackendKind kind : {BackendKind::kCpp, BackendKind::kRust}) {
    ASSERT_NO_FATAL_FAILURE(InitBackend(kind, "MapAndUnmap"));
    // We should have exactly 1 page allocated at this point (for the root translation table page);
    // VerifyAllocatedRegions will check this.
    VerifyAllocatedRegions();

    // Create Mock VmObject and PinnedVmObject
    // Set up mock mapping: virtual offset to physical address
    fbl::RefPtr<MockVmObject> vmo = fbl::MakeRefCounted<MockVmObject>(
        MockVmObject::InitList{{0x0, 0x1000}, {0x1000, 0x2000}, {0x2000, 0x3000}});

    PinnedVmObject pinned_vmo(vmo, 0, 0x3000);  // 3 pages

    // Now perform the mapping operation, then verify that the page tables match what we
    // would expect based on our region allocator and tracked mappings state.  Test all
    // combinations of the IOMMU permission flags.
    for (uint32_t perms = 0; perms <= 0x7; ++perms) {
      zx::result<uint64_t> maybe_base = MapAndTrack(vmo, pinned_vmo, perms);
      ASSERT_TRUE(maybe_base.is_ok(), "Failed for perms 0x%x", perms);
      const uint64_t base = maybe_base.value();
      VerifyAllocatedRegions();

      // Unmap and revalidate the page tables once again.
      UnmapAndUntrack(base);
      VerifyAllocatedRegions();
    }

    ASSERT_NO_FATAL_FAILURE(CleanupBackend());
  }
}

TEST_F(DeviceAspaceTest, MapAtSpecificLocation) {
  for (BackendKind kind : {BackendKind::kCpp, BackendKind::kRust}) {
    ASSERT_NO_FATAL_FAILURE(InitBackend(kind, "MapAtSpecificLocation"));
    VerifyAllocatedRegions();

    // Create Mock VmObject and PinnedVmObject
    fbl::RefPtr<MockVmObject> vmo =
        fbl::MakeRefCounted<MockVmObject>(MockVmObject::InitList{{0x0, 0x1000}});
    PinnedVmObject pinned_vmo(vmo, 0, 0x1000);

    // Pick a location in the middle of our address space, and make sure it is page aligned.
    const uint64_t target_location =
        ((kDefaultASpaceStart + kDefaultASpaceEnd) / 2) & ~uint64_t{kPageMask};
    zx::result<uint64_t> maybe_base =
        MapAndTrack(vmo, pinned_vmo, IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE, target_location);

    ASSERT_TRUE(maybe_base.is_ok(), "Failed to map at 0x%lx (status %d)", target_location,
                maybe_base.status_value());
    EXPECT_EQ(maybe_base.value(), target_location);
    VerifyAllocatedRegions();

    // Unmap and revalidate the page tables once again.
    UnmapAndUntrack(maybe_base.value());
    VerifyAllocatedRegions();

    ASSERT_NO_FATAL_FAILURE(CleanupBackend());
  }
}

// Create a 2-page mapping which straddles all of the translation table levels.
//
// The first page should require an entry in the last slot of every level
// of the translation tables, while the second page should require an entry in
// the first slot of every level of the translation table.
TEST_F(DeviceAspaceTest, MapStraddlingTableLevels) {
  for (BackendKind kind : {BackendKind::kCpp, BackendKind::kRust}) {
    ASSERT_NO_FATAL_FAILURE(InitBackend(kind, "MapStraddlingTableLevels"));
    VerifyAllocatedRegions();

    // Create Mock VmObject and PinnedVmObject for 2 pages
    fbl::RefPtr<MockVmObject> vmo =
        fbl::MakeRefCounted<MockVmObject>(MockVmObject::InitList{{0x0, 0x1000}, {0x1000, 0x2000}});
    PinnedVmObject pinned_vmo(vmo, 0, 0x2000);

    // Pick a location which straddles all of the translation table levels.  The
    // L0/L1 boundary is at 1 << 39.
    const uint64_t target_location = (uint64_t{1} << 39) - kPageSize;
    zx::result<uint64_t> maybe_base =
        MapAndTrack(vmo, pinned_vmo, IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE, target_location);

    ASSERT_TRUE(maybe_base.is_ok(), "Failed to map at 0x%lx (status %d)", target_location,
                maybe_base.status_value());
    EXPECT_EQ(maybe_base.value(), target_location);
    VerifyAllocatedRegions();

    // Unmap and revalidate the page tables once again.
    UnmapAndUntrack(maybe_base.value());
    VerifyAllocatedRegions();

    ASSERT_NO_FATAL_FAILURE(CleanupBackend());
  }
}

// Test mapping which fails during translation table allocation.
// Ensure that the operation returns a proper error and that the page tables
// are properly reset (no leaked partial mappings or tables).
TEST_F(DeviceAspaceTest, MapFailureInjection) {
  for (BackendKind kind : {BackendKind::kCpp, BackendKind::kRust}) {
    ASSERT_NO_FATAL_FAILURE(InitBackend(kind, "MapFailureInjection"));

    for (uint64_t limit = 0; limit < 4; ++limit) {
      VerifyAllocatedRegions();

      // Create a 10-page Mock VmObject
      fbl::RefPtr<MockVmObject> vmo = fbl::MakeRefCounted<MockVmObject>(MockVmObject::InitList{
          {0x0000, 0x1000},
          {0x1000, 0x2000},
          {0x2000, 0x3000},
          {0x3000, 0x4000},
          {0x4000, 0x5000},
          {0x5000, 0x6000},
          {0x6000, 0x7000},
          {0x7000, 0x8000},
          {0x8000, 0x9000},
          {0x9000, 0xA000},
      });
      PinnedVmObject pinned_vmo(vmo, 0, 10 * kPageSize);

      // Pick a location which straddles the Level 2 (2 MB) boundary.
      // Pages 0-4 will be in Level 3 Table A (Level 2 index 0).
      // Pages 5-9 will be in Level 3 Table B (Level 2 index 1).
      const uint64_t target_location = (uint64_t{1} << 21) - (5 * kPageSize);

      // We expect 4 successful allocations for translation tables after the root
      // page:
      // 1. Level 1 table
      // 2. Level 2 table
      // 3. Level 3 table A (for the first part of the mapping)
      // 4. Level 3 table B (during Advance to the 2MB boundary at Page 5)
      //
      // By looping from 0 to 3, we test failing at each of these points.
      PmmMock::Get().SetAllocationLimit(limit);
      auto cleanup_limit = fit::defer([]() { PmmMock::Get().ClearAllocationLimit(); });

      invalidations_.clear();

      const size_t pages_before = PmmMock::Get().GetAllocatedPageCount();
      zx::result<AllocationHandle> alloc_res =
          AspaceMap(pinned_vmo, IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE, target_location);
      EXPECT_FALSE(alloc_res.is_ok());
      EXPECT_EQ(ZX_ERR_NO_MEMORY, alloc_res.status_value());
      const size_t pages_after = PmmMock::Get().GetAllocatedPageCount();

      // Since it failed, it should have unmapped and invalidated.
      ASSERT_EQ(1u, invalidations_.size());
      EXPECT_EQ(target_location, invalidations_[0].base);
      EXPECT_EQ(pinned_vmo.size(), invalidations_[0].size);

      EXPECT_EQ(pages_before, pages_after);
      EXPECT_EQ(pages_before + limit, invalidations_[0].pmm_pages_during_inval);
      if (invalidations_[0].cache_entries_during_inval.has_value()) {
        EXPECT_EQ(limit, *invalidations_[0].cache_entries_during_inval);
      }
      EXPECT_EQ(0u, AspaceCacheEntries());

      // Confirm that nothing remained allocated in the RegionAllocator and the
      // page tables were properly cleaned up.
      EXPECT_EQ(0u, tracked_mappings_.size());
      VerifyAllocatedRegions();

      // Clean up for the next iteration.
      VerifyAllocatedRegions();
      PmmMock::Get().ClearAllocationLimit();
      cleanup_limit.cancel();
    }

    ASSERT_NO_FATAL_FAILURE(CleanupBackend());
  }
}

// Test creating mappings that share various levels of the translation table.
TEST_F(DeviceAspaceTest, SharedTableLevels) {
  for (BackendKind kind : {BackendKind::kCpp, BackendKind::kRust}) {
    ASSERT_NO_FATAL_FAILURE(InitBackend(kind, "SharedTableLevels"));
    VerifyAllocatedRegions();

    // Create a 1-page Mock VmObject to use for all mappings.
    fbl::RefPtr<MockVmObject> vmo =
        fbl::MakeRefCounted<MockVmObject>(MockVmObject::InitList{{0x0, 0x1000}});
    PinnedVmObject pinned_vmo(vmo, 0, kPageSize);

    // Define a set of virtual addresses to map.
    // The first one is our "base".
    // The subsequent ones share L0, L1, L2, and L3 tables with the base respectively.
    constexpr uint64_t base_vaddr = uint64_t{2} << 39;
    constexpr uint64_t test_vaddrs[] = {
        base_vaddr,
        base_vaddr + (uint64_t{1} << 39),  // Different L0 index (shares L0 table/root)
        base_vaddr + (uint64_t{1} << 30),  // Different L1 index (shares L1 table)
        base_vaddr + (uint64_t{1} << 21),  // Different L2 index (shares L2 table)
        base_vaddr + (uint64_t{1} << 12),  // Different L3 index (shares L3 table)
    };

    // Map each of the addresses and verify consistency after each step.
    for (uint64_t vaddr : test_vaddrs) {
      ASSERT_TRUE(MapAndTrack(vmo, pinned_vmo, IOMMU_FLAG_PERM_READ, vaddr).is_ok());
      VerifyAllocatedRegions();
    }

    // Now unmap each of the mappings in reverse order and verify consistency at each step.
    for (size_t i = std::size(test_vaddrs); i > 0;) {
      UnmapAndUntrack(test_vaddrs[--i]);
      VerifyAllocatedRegions();
    }

    ASSERT_NO_FATAL_FAILURE(CleanupBackend());
  }
}

TEST_F(DeviceAspaceTest, MapAtLocationEdgeCases) {
  for (BackendKind kind : {BackendKind::kCpp, BackendKind::kRust}) {
    ASSERT_NO_FATAL_FAILURE(InitBackend(kind, "MapAtLocationEdgeCases"));
    VerifyAllocatedRegions();

    // Create Mock VmObject and PinnedVmObject of 2 pages (aligned size).
    fbl::RefPtr<MockVmObject> vmo =
        fbl::MakeRefCounted<MockVmObject>(MockVmObject::InitList{{0x0, 0x1000}, {0x1000, 0x2000}});
    PinnedVmObject pinned_vmo(vmo, 0, 2 * kPageSize);

    const uint64_t target_location =
        ((kDefaultASpaceStart + kDefaultASpaceEnd) / 2) & ~uint64_t{kPageMask};

    // 1. Either the location or size are not page aligned (expected: ZX_ERR_INVALID_ARGS)
    {
      // A. Location not page aligned
      const uint64_t unaligned_location = target_location + 1;
      invalidations_.clear();
      zx::result<AllocationHandle> alloc_res =
          AspaceMap(pinned_vmo, IOMMU_FLAG_PERM_READ, unaligned_location);
      EXPECT_EQ(ZX_ERR_INVALID_ARGS, alloc_res.status_value());
      EXPECT_TRUE(invalidations_.empty());

      // B. Size not page aligned
      PinnedVmObject unaligned_pinned_vmo(vmo, 0, (2 * kPageSize) - 1);
      invalidations_.clear();
      alloc_res = AspaceMap(unaligned_pinned_vmo, IOMMU_FLAG_PERM_READ, target_location);
      EXPECT_EQ(ZX_ERR_INVALID_ARGS, alloc_res.status_value());
      EXPECT_TRUE(invalidations_.empty());
    }

    // 2. The sum of location and size overflows a 64-bit unsigned int (expected:
    // ZX_ERR_INVALID_ARGS)
    {
      // Use a location close to uint64_t max such that location + size overflows.
      const uint64_t overflowing_location = uint64_t{0} - kPageSize;
      invalidations_.clear();
      zx::result<AllocationHandle> alloc_res =
          AspaceMap(pinned_vmo, IOMMU_FLAG_PERM_READ, overflowing_location);
      EXPECT_EQ(ZX_ERR_INVALID_ARGS, alloc_res.status_value());
      EXPECT_TRUE(invalidations_.empty());
    }

    // 3. The requested region goes outside the address space's valid address range (expected:
    // ZX_ERR_INVALID_ARGS)
    {
      // A. Region starts before first_valid_address()
      const uint64_t before_aspace_location = kDefaultASpaceStart - kPageSize;
      invalidations_.clear();
      zx::result<AllocationHandle> alloc_res =
          AspaceMap(pinned_vmo, IOMMU_FLAG_PERM_READ, before_aspace_location);
      EXPECT_EQ(ZX_ERR_INVALID_ARGS, alloc_res.status_value());
      EXPECT_TRUE(invalidations_.empty());

      // B. Region ends after last_valid_address()
      // kDefaultASpaceEnd is exclusive end of valid address range (i.e. last valid is
      // kDefaultASpaceEnd - 1)
      const uint64_t past_aspace_location = kDefaultASpaceEnd - kPageSize;
      invalidations_.clear();
      alloc_res = AspaceMap(pinned_vmo, IOMMU_FLAG_PERM_READ, past_aspace_location);
      EXPECT_EQ(ZX_ERR_INVALID_ARGS, alloc_res.status_value());
      EXPECT_TRUE(invalidations_.empty());
    }

    // 4. The requested region collides with a currently active allocation (expected:
    // ZX_ERR_ALREADY_EXISTS)
    {
      // Map first region successfully
      zx::result<uint64_t> first_base =
          MapAndTrack(vmo, pinned_vmo, IOMMU_FLAG_PERM_READ, target_location);
      ASSERT_TRUE(first_base.is_ok());
      VerifyAllocatedRegions();

      // Attempt to map another region overlapping the first region
      // A. Exactly same location
      invalidations_.clear();
      zx::result<AllocationHandle> alloc_res =
          AspaceMap(pinned_vmo, IOMMU_FLAG_PERM_READ, target_location);
      EXPECT_EQ(ZX_ERR_ALREADY_EXISTS, alloc_res.status_value());
      EXPECT_TRUE(invalidations_.empty());

      // B. Partially overlapping location (1 page offset)
      invalidations_.clear();
      alloc_res = AspaceMap(pinned_vmo, IOMMU_FLAG_PERM_READ, target_location + kPageSize);
      EXPECT_EQ(ZX_ERR_ALREADY_EXISTS, alloc_res.status_value());
      EXPECT_TRUE(invalidations_.empty());

      // Clean up the first allocation
      UnmapAndUntrack(first_base.value());
      VerifyAllocatedRegions();
    }

    ASSERT_NO_FATAL_FAILURE(CleanupBackend());
  }
}

}  // namespace arm_smmu
