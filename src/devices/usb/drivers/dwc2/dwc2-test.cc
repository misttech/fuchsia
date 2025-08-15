// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/dwc2/dwc2.h"

#include <fidl/fuchsia.driver.compat/cpp/test_base.h>
#include <fidl/fuchsia.hardware.platform.device/cpp/fidl.h>
#include <lib/ddk/metadata.h>
#include <lib/driver/fake-mmio-reg/cpp/fake-mmio-reg.h>
#include <lib/driver/fake-platform-device/cpp/fake-pdev.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/fdf/dispatcher.h>
#include <lib/fit/result.h>
#include <lib/zx/eventpair.h>
#include <lib/zx/result.h>
#include <lib/zx/vmo.h>

#include <cstdint>
#include <utility>

#include <gtest/gtest.h>

#include "src/devices/usb/drivers/dwc2/dwc2_config.h"

namespace dwc2 {

namespace fcompat = fuchsia_driver_compat;
namespace fpdev = fuchsia_hardware_platform_device;

class FakeCompatServer : public fidl::testing::TestBase<fcompat::Device> {
 public:
  fcompat::Service::InstanceHandler GetHandler() {
    async_dispatcher_t* dispatcher = fdf::Dispatcher::GetCurrent()->async_dispatcher();
    return fcompat::Service::InstanceHandler(
        {.device = bindings_.CreateHandler(this, dispatcher, fidl::kIgnoreBindingClosure)});
  }

  void GetMetadata(GetMetadataCompleter::Sync& completer) override {
    zx::vmo vmo;
    EXPECT_EQ(ZX_OK, zx::vmo::create(sizeof(dwc2_metadata_t), 0, &vmo));
    EXPECT_EQ(ZX_OK,
              vmo.write(reinterpret_cast<const void*>(&metadata_), 0, sizeof(dwc2_metadata_t)));

    fcompat::DeviceGetMetadataResponse response;
    response.metadata().emplace_back(DEVICE_METADATA_PRIVATE, std::move(vmo));

    completer.Reply(fit::ok(std::move(response)));
  }

  void NotImplemented_(const std::string& name, fidl::CompleterBase&) override {
    ADD_FAILURE() << name << "() unexpected";
  }

 private:
  fidl::ServerBindingGroup<fcompat::Device> bindings_;

  // clang-format off
  static constexpr dwc2_metadata_t metadata_{
      .dma_burst_len = 0,
      .usb_turnaround_time = 10,
      .rx_fifo_size = 1024,
      .nptx_fifo_size = 1024,
      .tx_fifo_sizes = {1024, 1024, 1024, 1024, 1024,
                        1024, 1024, 1024, 1024, 1024,
                        1024, 1024, 1024, 1024, 1024}
  };
  // clang-format on
};

class Environment : public fdf_testing::Environment {
 public:
  Environment() {
    fdf_fake::FakePDev::Config cfg{};
    cfg.mmios[0] = mmio_.GetMmioBuffer();
    cfg.use_fake_bti = true;
    cfg.use_fake_irq = true;

    pdev_.SetConfig(std::move(cfg));
  }

  zx::result<> Serve(fdf::OutgoingDirectory& outgoing) override {
    async_dispatcher_t* dispatcher = fdf::Dispatcher::GetCurrent()->async_dispatcher();

    EXPECT_TRUE(
        outgoing.AddService<fpdev::Service>(pdev_.GetInstanceHandler(dispatcher), "pdev").is_ok());

    // Compat is used to get legacy metadata.
    EXPECT_TRUE(outgoing.AddService<fcompat::Service>(compat_.GetHandler(), "pdev").is_ok());

    return zx::ok();
  }

 private:
  fdf_fake::FakePDev pdev_;
  fake_mmio::FakeMmioRegRegion mmio_{sizeof(uint32_t), 4096};
  FakeCompatServer compat_;
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
