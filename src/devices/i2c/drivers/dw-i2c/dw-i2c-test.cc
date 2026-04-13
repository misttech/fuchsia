// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "dw-i2c.h"

#include <lib/driver/fake-platform-device/cpp/fake-pdev.h>
#include <lib/driver/testing/cpp/driver_test.h>

#include <fake-mmio-reg/fake-mmio-reg.h>
#include <gtest/gtest.h>

#include "src/lib/testing/predicates/status.h"

namespace dw_i2c {

class TestDwI2c : public DwI2c {
 public:
  TestDwI2c(fdf::DriverStartArgs start_args, fdf::UnownedSynchronizedDispatcher driver_dispatcher)
      : DwI2c(std::move(start_args), std::move(driver_dispatcher)) {}

  static DriverRegistration GetDriverRegistration() {
    return FUCHSIA_DRIVER_REGISTRATION_V1(fdf_internal::DriverServer<TestDwI2c>::initialize,
                                          fdf_internal::DriverServer<TestDwI2c>::destroy);
  }

  static void set_mmio(fdf::MmioBuffer mmio) { mmio_.emplace(std::move(mmio)); }

 protected:
  zx::result<fdf::MmioBuffer> MapMmio(fdf::PDev& pdev) override {
    if (mmio_) {
      return zx::ok(*std::move(mmio_));
    }
    return zx::error(ZX_ERR_BAD_STATE);
  }

 private:
  static std::optional<fdf::MmioBuffer> mmio_;
};

std::optional<fdf::MmioBuffer> TestDwI2c::mmio_;

class FakeDwI2cController {
 public:
  FakeDwI2cController() : mmio_(sizeof(uint32_t), 0x100) {
    // Mock CompTypeReg (0xfc) to return kDwCompTypeNum (0x44570140)
    mmio_[0xfc].SetReadCallback([]() { return DwI2c::kDwCompTypeNum; });
    // Mock CompParam1Reg (0xf4) to return depths (8 for tx and rx)
    mmio_[0xf4].SetReadCallback([]() { return (8 << 16) | (8 << 8); });
  }

  fdf::MmioBuffer GetMmioBuffer() { return mmio_.GetMmioBuffer(); }

  ddk_fake::FakeMmioRegRegion mmio_;
};

class TestEnvironment : public fdf_testing::Environment {
 public:
  void Init(zx::interrupt interrupt) {
    std::map<uint32_t, zx::interrupt> irqs;
    irqs[0] = std::move(interrupt);
    pdev_.SetConfig({.irqs = std::move(irqs)});
  }

  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    async_dispatcher_t* dispatcher = fdf::Dispatcher::GetCurrent()->async_dispatcher();
    EXPECT_OK(to_driver_vfs.AddService<fuchsia_hardware_platform_device::Service>(
        pdev_.GetInstanceHandler(dispatcher), "pdev"));
    return zx::ok();
  }

 private:
  fdf_fake::FakePDev pdev_;
};

class TestConfig final {
 public:
  using DriverType = TestDwI2c;
  using EnvironmentType = TestEnvironment;
};

TEST(DwI2cTest, Lifecycle) {
  fdf_testing::BackgroundDriverTest<TestConfig> driver_test;

  zx::interrupt interrupt;
  ASSERT_EQ(ZX_OK, zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt));

  driver_test.RunInEnvironmentTypeContext([&](auto& env) { env.Init(std::move(interrupt)); });

  FakeDwI2cController controller;
  TestDwI2c::set_mmio(controller.GetMmioBuffer());

  ASSERT_TRUE(driver_test.StartDriver().is_ok());
  ASSERT_TRUE(driver_test.StopDriver().is_ok());
}

}  // namespace dw_i2c
