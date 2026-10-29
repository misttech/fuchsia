// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/drivers/intel-display/gtt.h"

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async-loop/loop.h>
#include <lib/driver/mmio/cpp/mmio-view.h>
#include <lib/driver/mmio/testing/cpp/test-helper.h>
#include <lib/driver/testing/cpp/scoped_global_logger.h>
#include <lib/fake-bti/bti.h>
#include <lib/zircon-internal/align.h>

#include <vector>

#include <gtest/gtest.h>

#include "src/devices/pci/testing/pci_protocol_fake.h"
#include "src/graphics/display/drivers/intel-display/registers.h"
#include "src/lib/testing/predicates/status.h"

namespace intel_display {

namespace {

const uint32_t kPageSize = zx_system_get_page_size();

// Initialize the GTT to the smallest allowed size (which is 2MB with the |gtt_size| bits of the
// graphics control register set to 0x01.
constexpr size_t kTableSize = (1 << 21);
void Configure2MbGtt(fdf::Pci& pci) {
  zx_status_t status = pci.WriteConfig16(registers::GmchGfxControl::kAddr, 0x40);
  EXPECT_OK(status);
}

class GttTest : public testing::Test {
 public:
  GttTest() : loop_(&kAsyncLoopConfigNeverAttachToThread) {}

  void SetUp() override {
    loop_.StartThread("pci-fidl-server-thread");
    pci_ = fake_pci_.SetUpFidlServer(loop_);
  }

 protected:
  fdf_testing::ScopedGlobalLogger logger_;
  async::Loop loop_;
  fdf::Pci pci_;
  pci::FakePciProtocol fake_pci_;
};

TEST_F(GttTest, InitWithZeroSizeGtt) {
  fdf::MmioBuffer mmio = fdf_testing::CreateMmioBuffer(kTableSize, ZX_CACHE_POLICY_CACHED);
  // The view stays valid after the move because |gtt| keeps the mapping.
  fdf::MmioView table = mmio.View(0);

  Gtt gtt;
  EXPECT_STATUS(ZX_ERR_INTERNAL, gtt.Init(pci_, std::move(mmio), 0));

  // No MMIO writes should have occurred.
  for (size_t i = 0; i < kTableSize / sizeof(uint64_t); i++) {
    ASSERT_EQ(0u, table.Read64(i * sizeof(uint64_t)));
  }
}

TEST_F(GttTest, InitGtt) {
  Configure2MbGtt(pci_);

  fdf::MmioBuffer mmio = fdf_testing::CreateMmioBuffer(kTableSize, ZX_CACHE_POLICY_CACHED);
  fdf::MmioView table = mmio.View(0);

  Gtt gtt;
  EXPECT_OK(gtt.Init(pci_, std::move(mmio), 0));

  // The table should contain 2MB / sizeof(uint64_t) 64-bit entries that map to the fake scratch
  // buffer. The "+ 1" marks bit 0 as 1 which denotes that's the page is present.
  uint64_t kBusPhysicalAddr = FAKE_BTI_PHYS_ADDR | 1;
  for (size_t i = 0; i < kTableSize / sizeof(uint64_t); i++) {
    ASSERT_EQ(kBusPhysicalAddr, table.Read64(i * sizeof(uint64_t)));
  }

  // Allocated GTT regions should start from base 0.
  std::unique_ptr<GttRegionImpl> region;
  EXPECT_OK(gtt.AllocRegion(kPageSize, kPageSize, &region));
  ASSERT_TRUE(region != nullptr);
  EXPECT_EQ(0u, region->base());
  EXPECT_EQ(kPageSize, region->size());
}

TEST_F(GttTest, InitGttWithFramebufferOffset) {
  Configure2MbGtt(pci_);

  // Treat the first 1024 bytes as the bootloader framebuffer region and initialize it to garbage.
  constexpr size_t kFbOffset = 1024;
  constexpr uint8_t kJunk = 0xFF;
  const size_t kFbPages = ZX_ROUNDUP(kFbOffset, kPageSize) / kPageSize;
  fdf::MmioBuffer mmio = fdf_testing::CreateMmioBuffer(kTableSize, ZX_CACHE_POLICY_CACHED);
  const std::vector<uint8_t> junk(kTableSize, kJunk);
  mmio.WriteBuffer(0, junk.data(), junk.size());
  fdf::MmioView table = mmio.View(0);

  Gtt gtt;
  EXPECT_OK(gtt.Init(pci_, std::move(mmio), kFbOffset));

  // The first page-aligned region of addresses should remain unmodified.
  for (size_t i = 0; i < kFbPages; i++) {
    ASSERT_EQ(kJunk, table.Read8(i));
  }

  // The table should contain 2MB / sizeof(uint64_t) 64-bit entries that map to the fake scratch
  // buffer. The "+ 1" marks bit 0 as 1 which denotes that's the page is present.
  uint64_t kBusPhysicalAddr = FAKE_BTI_PHYS_ADDR | 1;
  for (size_t i = kFbPages; i < kTableSize / sizeof(uint64_t); i++) {
    ASSERT_EQ(kBusPhysicalAddr, table.Read64(i * sizeof(uint64_t)));
  }

  // The first allocated GTT regions should exclude the framebuffer pages.
  std::unique_ptr<GttRegionImpl> region;
  EXPECT_OK(gtt.AllocRegion(kPageSize, kPageSize, &region));
  ASSERT_TRUE(region != nullptr);
  EXPECT_EQ(kFbPages * kPageSize, region->base());
  EXPECT_EQ(kPageSize, region->size());
}

}  // namespace

}  // namespace intel_display
