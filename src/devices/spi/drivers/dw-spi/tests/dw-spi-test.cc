// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "../dw-spi.h"

#include <fidl/fuchsia.hardware.spiimpl/cpp/driver/fidl.h>
#include <lib/driver/fake-clock/cpp/fake-clock.h>
#include <lib/driver/fake-platform-device/cpp/fake-pdev.h>
#include <lib/driver/fake-powerdomain/cpp/fake-powerdomain.h>
#include <lib/driver/fake-reset/cpp/fake-reset.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/fzl/vmo-mapper.h>

#include <gtest/gtest.h>

#include "../registers.h"

namespace dw_spi {
namespace {

class DwSpiEnvironment : public fdf_testing::Environment {
 public:
  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    if (zx_status_t status = zx::vmo::create(kMmioSize, 0, &mmio_vmo_); status != ZX_OK) {
      return zx::error(status);
    }

    zx::vmo vmo;
    if (zx_status_t status = mmio_vmo_.duplicate(ZX_RIGHT_SAME_RIGHTS, &vmo); status != ZX_OK) {
      return zx::error(status);
    }

    auto mmio =
        fdf::MmioBuffer::Create(0, kMmioSize, std::move(vmo), ZX_CACHE_POLICY_UNCACHED_DEVICE);
    if (mmio.is_error()) {
      return mmio.take_error();
    }

    if (zx_status_t status = mapped_mmio_.Map(mmio_vmo_); status != ZX_OK) {
      return zx::error(status);
    }

    fdf_fake::FakePDev::Config config;
    config.mmios[0] = std::move(mmio.value());

    zx::interrupt interrupt;
    if (zx_status_t status = zx::interrupt::create({}, 0, ZX_INTERRUPT_VIRTUAL, &interrupt);
        status != ZX_OK) {
      return zx::error(status);
    }
    if (zx_status_t status = interrupt.duplicate(ZX_RIGHT_SAME_RIGHTS, &interrupt_);
        status != ZX_OK) {
      return zx::error(status);
    }
    config.irqs[0] = std::move(interrupt);
    config.irq_names["dw-spi"] = 0;

    pdev_.SetConfig(std::move(config));

    if (zx::result<> result = to_driver_vfs.AddService<fuchsia_hardware_platform_device::Service>(
            pdev_.GetInstanceHandler(fdf::Dispatcher::GetCurrent()->async_dispatcher()), "pdev");
        result.is_error()) {
      return result.take_error();
    }

    if (zx::result<> result = to_driver_vfs.AddService<fuchsia_hardware_powerdomain::Service>(
            power_domain_.CreateInstanceHandler(), "power-domain");
        result.is_error()) {
      return result.take_error();
    }

    if (zx::result<> result = to_driver_vfs.AddService<fuchsia_hardware_clock::Service>(
            clock_ssi_.CreateInstanceHandler(), "clock-ssi");
        result.is_error()) {
      return result.take_error();
    }

    if (zx::result<> result = to_driver_vfs.AddService<fuchsia_hardware_clock::Service>(
            clock_pclk_.CreateInstanceHandler(), "clock-pclk");
        result.is_error()) {
      return result.take_error();
    }

    if (zx::result<> result = to_driver_vfs.AddService<fuchsia_hardware_reset::Service>(
            reset_.CreateInstanceHandler(), "reset");
        result.is_error()) {
      return result.take_error();
    }

    return zx::ok();
  }

  fdf_fake::FakePowerDomain& power_domain() { return power_domain_; }
  fdf_fake::FakeClock& clock_ssi() { return clock_ssi_; }
  fdf_fake::FakeClock& clock_pclk() { return clock_pclk_; }
  fdf_fake::FakeReset& reset() { return reset_; }

  std::span<uint32_t> mmio() const {
    return {reinterpret_cast<uint32_t*>(mapped_mmio_.start()), kMmioSize / sizeof(uint32_t)};
  }

  zx::interrupt take_interrupt() { return std::move(interrupt_); }

 private:
  static constexpr size_t kMmioSize = 0x100;

  fdf_fake::FakePDev pdev_;
  fdf_fake::FakePowerDomain power_domain_;
  fdf_fake::FakeClock clock_ssi_{fdf::Dispatcher::GetCurrent()->async_dispatcher()};
  fdf_fake::FakeClock clock_pclk_{fdf::Dispatcher::GetCurrent()->async_dispatcher()};
  fdf_fake::FakeReset reset_;
  zx::vmo mmio_vmo_;
  fzl::VmoMapper mapped_mmio_;
  zx::interrupt interrupt_;
};

class DwSpiTestConfiguration final {
 public:
  using DriverType = DwSpiDriver;
  using EnvironmentType = DwSpiEnvironment;
};

class DwSpiTest : public ::testing::Test {
 protected:
  void SetUp() override {
    EXPECT_EQ(driver_test().StartDriver().status_value(), ZX_OK);
    mmio_ = driver_test().RunInEnvironmentTypeContext<std::span<uint32_t>>(
        [](DwSpiEnvironment& env) { return env.mmio(); });
  }

  void TearDown() override {
    EXPECT_EQ(driver_test().StopDriver().status_value(), ZX_OK);
    driver_test().ShutdownAndDestroyDriver();
  }

  fdf_testing::ForegroundDriverTest<DwSpiTestConfiguration>& driver_test() { return driver_test_; }

  std::span<uint32_t> mmio() const { return mmio_; }

 private:
  fdf_testing::ForegroundDriverTest<DwSpiTestConfiguration> driver_test_;
  std::span<uint32_t> mmio_;
};

TEST_F(DwSpiTest, StartStop) {
  EXPECT_TRUE(driver_test().RunInEnvironmentTypeContext<bool>(
      [](DwSpiEnvironment& env) { return env.power_domain().is_enabled(); }));
  EXPECT_TRUE(driver_test().RunInEnvironmentTypeContext<bool>(
      [](DwSpiEnvironment& env) { return env.clock_ssi().enabled(); }));
  EXPECT_TRUE(driver_test().RunInEnvironmentTypeContext<bool>(
      [](DwSpiEnvironment& env) { return env.clock_pclk().enabled(); }));
  EXPECT_TRUE(driver_test().RunInEnvironmentTypeContext<bool>(
      [](DwSpiEnvironment& env) { return env.reset().take_toggled(); }));

  // Verify CTRLR0
  // We expect SPI_FRF=0, FRF=0, DFS=7 (8-bit), TMOD=0
  EXPECT_EQ(mmio()[DW_SPI_CTRLR0 / 4], 7u);  // DFS=7, others 0

  // Verify SSIENR
  EXPECT_EQ(mmio()[DW_SPI_SSIENR / 4], 1u);

  // Verify BAUDR
  EXPECT_EQ(mmio()[DW_SPI_BAUDR / 4], 2u);

  // Verify IMR
  EXPECT_EQ(mmio()[DW_SPI_IMR / 4], 0u);
}

TEST_F(DwSpiTest, ChildNodeAdded) {
  driver_test().RunInNodeContext([](fdf_testing::TestNode& node) {
    EXPECT_NE(node.children().find("dw-spi"), node.children().cend());
  });
}

TEST_F(DwSpiTest, GetChipSelectCount) {
  zx::result client_end = driver_test().Connect<fuchsia_hardware_spiimpl::Service::Device>();
  ASSERT_TRUE(client_end.is_ok());

  fdf::Client spiimpl(std::move(client_end.value()), fdf::Dispatcher::GetCurrent()->get());

  spiimpl->GetChipSelectCount().ThenExactlyOnce(
      [&](fdf::Result<fuchsia_hardware_spiimpl::SpiImpl::GetChipSelectCount>& result) {
        ASSERT_TRUE(result.is_ok());
        EXPECT_EQ(result.value().count(), 1u);
        driver_test().runtime().Quit();
      });
  driver_test().runtime().Run();
}

}  // namespace
}  // namespace dw_spi
