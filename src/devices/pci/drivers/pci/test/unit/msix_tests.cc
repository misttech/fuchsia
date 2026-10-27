// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.pci/cpp/wire.h>
#include <fuchsia/hardware/pciroot/cpp/banjo.h>
#include <lib/driver/mmio/testing/cpp/test-helper.h>
#include <lib/pci/constants.h>
#include <lib/pci/hw.h>
#include <lib/zx/vmo.h>
#include <zircon/limits.h>
#include <zircon/syscalls/object.h>
#include <zircon/types.h>

#include <memory>
#include <optional>
#include <span>
#include <vector>

#include <fbl/hard_int.h>
#include <fbl/ref_ptr.h>
#include <gtest/gtest.h>

#include "src/devices/pci/drivers/pci/capabilities/msix.h"
#include "src/devices/pci/drivers/pci/test/fakes/fake_allocator.h"
#include "src/devices/pci/drivers/pci/test/fakes/fake_config.h"
#include "src/devices/pci/lib/cpp/config.h"
#include "src/lib/testing/predicates/status.h"

namespace pci {

namespace {
class PciCapabilityTests : public ::testing::Test {
 public:
  PciCapabilityTests() = default;
  fdf::MmioBuffer& mmio() { return *mmio_; }
  fdf::MmioView view() { return mmio_->View(0, mmio_->get_size()); }
  static fuchsia_hardware_pci::wire::Bdf bdf() {
    return {.bus_id = 0, .device_id = 0, .function_id = 0};
  }
  static Bar CreateBar(uint8_t bar_id, size_t size) {
    Bar bar = {
        .size = size,
        .bar_id = bar_id,
        .allocation =
            std::make_unique<FakeAllocation>(PCI_ADDRESS_SPACE_MEMORY, std::nullopt, size),
    };
    return bar;
  }

  static void ConfigureMsixCapability(Config& cfg, uint8_t tbar, uint8_t pbar, zx_paddr_t toffset,
                                      zx_paddr_t poffset, size_t vectors = 8, uint8_t base = 0) {
    MsixControlReg ctrl{};
    MsixTableReg table{};
    MsixPbaReg pba{};

    ctrl.set_table_size(vectors - 1);
    table.set_offset(toffset);
    table.set_bir(tbar);
    pba.set_offset(poffset);
    pba.set_bir(pbar);

    cfg.Write(PciReg16(static_cast<uint16_t>(base + MsixCapability::kMsixControlRegisterOffset)),
              ctrl.value);
    cfg.Write(PciReg32(static_cast<uint16_t>(base + MsixCapability::kMsixTableRegisterOffset)),
              table.reg_value());
    cfg.Write(PciReg32(static_cast<uint16_t>(base + MsixCapability::kMsixPbaRegisterOffset)),
              pba.reg_value());
  }

 protected:
  void SetUp() final {
    zx::vmo vmo;
    mmio_ = fdf_testing::CreateMmioBuffer(pci::kBaseConfigSize, ZX_CACHE_POLICY_UNCACHED);
  }

  void TearDown() final { mmio_->reset(); }

 private:
  std::optional<fdf::MmioBuffer> mmio_;
};

TEST_F(PciCapabilityTests, FixtureTest) {
  MmioConfig cfg = FakeMmioConfig(bdf(), view());
  size_t vectors = 8;
  uint8_t tbar = 1;
  uint8_t pbar = 2;
  zx_paddr_t toffset = 0x4000;
  zx_paddr_t poffset = 0x8000;
  ConfigureMsixCapability(cfg, tbar, pbar, toffset, poffset, vectors);
  ASSERT_EQ(mmio().Read16(MsixCapability::kMsixControlRegisterOffset), vectors - 1u);
  ASSERT_EQ(mmio().Read32(MsixCapability::kMsixTableRegisterOffset), toffset | tbar);
  ASSERT_EQ(mmio().Read32(MsixCapability::kMsixPbaRegisterOffset), poffset | pbar);
}

TEST_F(PciCapabilityTests, InitTest) {
  MmioConfig cfg = FakeMmioConfig(bdf(), view());
  ConfigureMsixCapability(cfg, /*tbar=*/1, /*pbar=*/1, /*toffset=*/0x4000, /*poffset=*/0x8000);
  MsixCapability msix(cfg, 0);
  std::vector<std::optional<Bar>> bars(kMaxBarCount);
  bars[1] = CreateBar(1, 0xC000);
  // Catch double initializations.
  ASSERT_OK(msix.Init(bars));
  ASSERT_STATUS(msix.Init(bars), ZX_ERR_BAD_STATE);
}

TEST_F(PciCapabilityTests, PbaSize) {
  MmioConfig cfg = FakeMmioConfig(bdf(), view());
  ConfigureMsixCapability(cfg, /*tbar=*/1, /*pbar=*/2, /*toffset=*/0x0, /*poffset=*/0x0,
                          /*vectors=*/MsixCapability::kMsixPbaBitsPerEntry);
  MsixCapability msix(cfg, 0);
  std::vector<std::optional<Bar>> bars(kMaxBarCount);
  bars[1] = CreateBar(1, MsixCapability::kMsixPbaBitsPerEntry * sizeof(MsixTable));
  // A PBA BAR sized to exactly 1 QWORD (8 bytes) must succeed. Under the previous
  // ((vectors / 64) + 1) formula this would require 16 bytes and fail.
  bars[2] = CreateBar(2, sizeof(uint64_t));
  ASSERT_OK(msix.Init(bars));
}

TEST_F(PciCapabilityTests, InitInvalidBars) {
  MmioConfig cfg = FakeMmioConfig(bdf(), view());
  std::vector<std::optional<Bar>> bars(kMaxBarCount);
  bars[0] = Bar{.size = 0x1000, .bar_id = 0, .is_mmio = true};  // no allocation
  bars[1] = CreateBar(1, 0x1000);
  bars[2] = CreateBar(2, 0x1000);

  // Type 0 endpoint: Table BIR exceeds kMaxBarCount (6).
  {
    ConfigureMsixCapability(cfg, /*tbar=*/6, /*pbar=*/1, /*toffset=*/0x0, /*poffset=*/0x0);
    MsixCapability msix(cfg, 0);
    EXPECT_EQ(msix.Init(bars), ZX_ERR_BAD_STATE);
  }
  // Type 0 endpoint: PBA BIR exceeds kMaxBarCount (6).
  {
    ConfigureMsixCapability(cfg, /*tbar=*/1, /*pbar=*/6, /*toffset=*/0x0, /*poffset=*/0x0);
    MsixCapability msix(cfg, 0);
    EXPECT_EQ(msix.Init(bars), ZX_ERR_BAD_STATE);
  }
  // Type 1 bridge: Table BIR (2) exceeds kBarRegsPerBridge (2), even with populated bars[2].
  {
    cfg.Write(Config::kHeaderType, kHeaderTypePciBridge);
    ConfigureMsixCapability(cfg, /*tbar=*/2, /*pbar=*/1, /*toffset=*/0x0, /*poffset=*/0x0,
                            /*vectors=*/8, /*base=*/0x20);
    MsixCapability msix(cfg, 0x20);
    EXPECT_EQ(msix.Init(bars), ZX_ERR_BAD_STATE);
  }
  // Type 1 bridge: PBA BIR (2) exceeds kBarRegsPerBridge (2), even with populated bars[2].
  {
    cfg.Write(Config::kHeaderType, kHeaderTypePciBridge);
    ConfigureMsixCapability(cfg, /*tbar=*/1, /*pbar=*/2, /*toffset=*/0x0, /*poffset=*/0x0,
                            /*vectors=*/8, /*base=*/0x20);
    MsixCapability msix(cfg, 0x20);
    EXPECT_EQ(msix.Init(bars), ZX_ERR_BAD_STATE);
  }
  // Unimplemented BAR (std::nullopt at index 3).
  {
    cfg.Write(Config::kHeaderType, 0);
    ConfigureMsixCapability(cfg, /*tbar=*/3, /*pbar=*/1, /*toffset=*/0x0, /*poffset=*/0x0);
    MsixCapability msix(cfg, 0);
    EXPECT_EQ(msix.Init(bars), ZX_ERR_BAD_STATE);
  }
  // Unallocated BAR (bars[0] has no allocation).
  {
    cfg.Write(Config::kHeaderType, 0);
    ConfigureMsixCapability(cfg, /*tbar=*/0, /*pbar=*/1, /*toffset=*/0x0, /*poffset=*/0x0);
    MsixCapability msix(cfg, 0);
    EXPECT_EQ(msix.Init(bars), ZX_ERR_BAD_STATE);
  }
}

}  // namespace
}  // namespace pci
