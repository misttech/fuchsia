// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/dwc2/dwc2.h"

#include <fidl/fuchsia.driver.compat/cpp/test_base.h>
#include <fidl/fuchsia.hardware.platform.device/cpp/fidl.h>
#include <lib/ddk/metadata.h>
#include <lib/driver/compat/cpp/compat.h>
#include <lib/driver/fake-mmio-reg/cpp/fake-mmio-reg.h>
#include <lib/driver/fake-platform-device/cpp/fake-pdev.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/fdf/dispatcher.h>
#include <lib/fit/result.h>
#include <lib/zx/eventpair.h>
#include <lib/zx/result.h>
#include <lib/zx/vmo.h>

#include <gtest/gtest.h>

#include "src/devices/usb/drivers/dwc2/dwc2_config.h"

namespace dwc2 {

namespace fpdev = fuchsia_hardware_platform_device;

class Environment : public fdf_testing::Environment {
 public:
  Environment() {
    fdf_fake::FakePDev::Config cfg{};
    cfg.mmios[0] = mmio_.GetMmioBuffer();
    cfg.use_fake_bti = true;
    cfg.use_fake_irq = true;

    pdev_.SetConfig(std::move(cfg));
  }

  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    static const fuchsia_hardware_usb_dwc2::Metadata kMetadata(
        {.dma_burst_len = fuchsia_hardware_usb_dwc2::DmaBurstLen::kSingle,
         .usb_turnaround_time = 10,
         .rx_fifo_size = 1024,
         .nptx_fifo_size = 1024,
         .tx_fifo_sizes = std::array<uint32_t, 15>{1024, 1024, 1024, 1024, 1024, 1024, 1024, 1024,
                                                   1024, 1024, 1024, 1024, 1024, 1024, 1024}});

    async_dispatcher_t* dispatcher = fdf::Dispatcher::GetCurrent()->async_dispatcher();

    pdev_.AddFidlMetadata(fuchsia_hardware_usb_dwc2::Metadata::kSerializableName, kMetadata);
    EXPECT_EQ(to_driver_vfs.AddService<fpdev::Service>(pdev_.GetInstanceHandler(dispatcher), "pdev")
                  .status_value(),
              ZX_OK);

    return zx::ok();
  }

 private:
  fdf_fake::FakePDev pdev_;
  fake_mmio::FakeMmioRegRegion mmio_{sizeof(uint32_t), 4096};
};

class Config {
 public:
  using DriverType = Dwc2;
  using EnvironmentType = Environment;
};

class Dwc2Test : public testing::Test {
 public:
  void SetUp() override {
    ASSERT_TRUE(dut_.StartDriverWithCustomStartArgs([](fdf::DriverStartArgs& args) {
                      dwc2_config::Config cfg;
                      cfg.enable_suspend() = false;
                      args.config(cfg.ToVmo());
                    })
                    .is_ok());
  }

  void TearDown() override { EXPECT_TRUE(dut_.StopDriver().is_ok()); }

 private:
  fdf_testing::ForegroundDriverTest<Config> dut_;
};

TEST_F(Dwc2Test, DriverLifetimeTest) {}

}  // namespace dwc2
