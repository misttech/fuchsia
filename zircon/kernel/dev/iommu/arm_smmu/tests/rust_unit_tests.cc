// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

// Host `zxtest` wrapper suite that invokes each of the 22 native Rust unit tests in
// `zircon/kernel/dev/iommu/arm_smmu/unit_tests.rs` and verifies zero PMM page leaks after each
// test.

#include <vm/pmm.h>
#include <zxtest/zxtest.h>

#include "cross_test_ffi.h"

namespace {

class RustUnitTests : public zxtest::Test {
 public:
  void SetUp() override {
    PmmMock::Get().ClearAllocationLimit();
    ASSERT_EQ(PmmMock::Get().GetAllocatedPageCount(), 0u);
  }

  void TearDown() override {
    PmmMock::Get().ClearAllocationLimit();
    ASSERT_EQ(PmmMock::Get().GetAllocatedPageCount(), 0u);
  }
};

TEST_F(RustUnitTests, SmmuModeDefaults) { ASSERT_TRUE(rust_unit_test_smmu_mode_defaults()); }

TEST_F(RustUnitTests, SmmuModeAdvanced) { ASSERT_TRUE(rust_unit_test_smmu_mode_advanced()); }

TEST_F(RustUnitTests, SmmuModeCaseInsensitive) {
  ASSERT_TRUE(rust_unit_test_smmu_mode_case_insensitive());
}

TEST_F(RustUnitTests, SmmuModeWhitespace) { ASSERT_TRUE(rust_unit_test_smmu_mode_whitespace()); }

TEST_F(RustUnitTests, SmmuModeValidation) { ASSERT_TRUE(rust_unit_test_smmu_mode_validation()); }

TEST_F(RustUnitTests, Vmsav8_64Descriptors) { ASSERT_TRUE(rust_unit_test_vmsav8_64_descriptors()); }

TEST_F(RustUnitTests, SmrValueAndIter) { ASSERT_TRUE(rust_unit_test_smr_value_and_iter()); }

TEST_F(RustUnitTests, Bitmask128) { ASSERT_TRUE(rust_unit_test_bitmask128()); }

TEST_F(RustUnitTests, PageCacheAddAndGet) { ASSERT_TRUE(rust_unit_test_page_cache_add_and_get()); }

TEST_F(RustUnitTests, PageCacheTrim) { ASSERT_TRUE(rust_unit_test_page_cache_trim()); }

TEST_F(RustUnitTests, PageCacheOom) { ASSERT_TRUE(rust_unit_test_page_cache_oom()); }

TEST_F(RustUnitTests, TranslationTableHelperWalkAndReclaim) {
  ASSERT_TRUE(rust_unit_test_translation_table_helper_walk_and_reclaim());
}

TEST_F(RustUnitTests, DeviceAspaceMapAndUnmap) {
  ASSERT_TRUE(rust_unit_test_device_aspace_map_and_unmap());
}

TEST_F(RustUnitTests, DeviceAspaceMapAtSpecificLocation) {
  ASSERT_TRUE(rust_unit_test_device_aspace_map_at_specific_location());
}

TEST_F(RustUnitTests, DeviceAspaceMapStraddlingTableLevels) {
  ASSERT_TRUE(rust_unit_test_device_aspace_map_straddling_table_levels());
}

TEST_F(RustUnitTests, DeviceAspaceMapFailureInjection) {
  ASSERT_TRUE(rust_unit_test_device_aspace_map_failure_injection());
}

TEST_F(RustUnitTests, DeviceAspaceSharedTableLevels) {
  ASSERT_TRUE(rust_unit_test_device_aspace_shared_table_levels());
}

TEST_F(RustUnitTests, DeviceAspaceMapAtLocationEdgeCases) {
  ASSERT_TRUE(rust_unit_test_device_aspace_map_at_location_edge_cases());
}

TEST_F(RustUnitTests, StreamMatchRegGroup) { ASSERT_TRUE(rust_unit_test_stream_match_reg_group()); }

TEST_F(RustUnitTests, ContextBank) { ASSERT_TRUE(rust_unit_test_context_bank()); }

TEST_F(RustUnitTests, RegistersAndBitfields) {
  ASSERT_TRUE(rust_unit_test_registers_and_bitfields());
}

TEST_F(RustUnitTests, EscapedName) { ASSERT_TRUE(rust_unit_test_escaped_name()); }

}  // namespace
