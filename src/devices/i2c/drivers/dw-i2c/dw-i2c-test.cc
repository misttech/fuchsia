// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "dw-i2c.h"

#include <lib/driver/fake-clock/cpp/fake-clock.h>
#include <lib/driver/fake-platform-device/cpp/fake-pdev.h>
#include <lib/driver/fake-powerdomain/cpp/fake-powerdomain.h>
#include <lib/driver/fake-reset/cpp/fake-reset.h>
#include <lib/driver/testing/cpp/driver_test.h>

#include <fake-mmio-reg/fake-mmio-reg.h>
#include <gtest/gtest.h>

#include "src/lib/testing/predicates/status.h"

namespace dw_i2c {

class TestDwI2c : public DwI2c {
 public:
  explicit TestDwI2c() : DwI2c() {}

  static DriverRegistration GetDriverRegistration() {
    return FUCHSIA_DRIVER_REGISTRATION_V1(fdf_internal::DriverServer2<TestDwI2c>::initialize,
                                          fdf_internal::DriverServer2<TestDwI2c>::destroy);
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
  static constexpr size_t kRegCount = 0x100;

  FakeDwI2cController() : mmio_(sizeof(uint32_t), kRegCount) {
    for (size_t i = 0; i < kRegCount; ++i) {
      size_t offset = i * sizeof(uint32_t);
      mmio_[offset].SetReadCallback([this, i]() { return regs_[i]; });
      mmio_[offset].SetWriteCallback(
          [this, i](uint64_t val) { regs_[i] = static_cast<uint32_t>(val); });
    }
    // Mock CompTypeReg (0xfc) to return kDwCompTypeNum (0x44570140)
    mmio_[0xfc].SetReadCallback([]() { return DwI2c::kDwCompTypeNum; });
    // Mock CompParam1Reg (0xf4) to return depths (8 for tx and rx)
    mmio_[0xf4].SetReadCallback([]() { return (8 << 16) | (8 << 8); });
  }

  fdf::MmioBuffer GetMmioBuffer() { return mmio_.GetMmioBuffer(); }

  uint32_t ReadReg(size_t offset) const { return regs_[offset / sizeof(uint32_t)]; }
  void ResetWritableRegs() { regs_.fill(0); }

  std::array<uint32_t, kRegCount> regs_{};
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
    EXPECT_OK(to_driver_vfs.AddService<fuchsia_hardware_clock::Service>(
        clock_bus_.CreateInstanceHandler(dispatcher), "bus"));
    EXPECT_OK(to_driver_vfs.AddService<fuchsia_hardware_clock::Service>(
        clock_regs_.CreateInstanceHandler(dispatcher), "registers"));
    EXPECT_OK(to_driver_vfs.AddService<fuchsia_hardware_powerdomain::Service>(
        powerdomain_.CreateInstanceHandler(), "power-domain"));
    EXPECT_OK(to_driver_vfs.AddService<fuchsia_hardware_reset::Service>(
        reset_.CreateInstanceHandler(), "reset"));
    return zx::ok();
  }

  fdf_fake::FakeClock& clock_bus() { return clock_bus_; }
  fdf_fake::FakeClock& clock_regs() { return clock_regs_; }
  fdf_fake::FakePowerDomain& powerdomain() { return powerdomain_; }
  fdf_fake::FakeReset& reset() { return reset_; }

 private:
  fdf_fake::FakePDev pdev_;
  fdf_fake::FakeClock clock_bus_;
  fdf_fake::FakeClock clock_regs_;
  fdf_fake::FakePowerDomain powerdomain_;
  fdf_fake::FakeReset reset_;
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

TEST(DwI2cTest, SuspendAndResume) {
  fdf_testing::BackgroundDriverTest<TestConfig> driver_test;

  zx::interrupt interrupt;
  ASSERT_EQ(ZX_OK, zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt));

  driver_test.RunInEnvironmentTypeContext([&](auto& env) { env.Init(std::move(interrupt)); });

  FakeDwI2cController controller;
  TestDwI2c::set_mmio(controller.GetMmioBuffer());

  ASSERT_OK(driver_test.StartDriver());

  driver_test.RunInEnvironmentTypeContext([](TestEnvironment& env) {
    EXPECT_TRUE(env.powerdomain().is_enabled());
    EXPECT_TRUE(env.clock_bus().enabled());
    EXPECT_TRUE(env.clock_regs().enabled());
    EXPECT_TRUE(env.reset().take_deasserted());
  });
  EXPECT_NE(controller.ReadReg(0x00), 0u);  // ControlReg
  EXPECT_NE(controller.ReadReg(0x14), 0u);  // StandardSpeedSclHighCountReg
  EXPECT_NE(controller.ReadReg(0x1c), 0u);  // FastSpeedSclHighCountReg
  EXPECT_NE(controller.ReadReg(0x3c), 0u);  // TxFifoThresholdReg
  EXPECT_NE(controller.ReadReg(0x7c), 0u);  // SdaHoldReg

  zx::result<> suspend_result = zx::error(ZX_ERR_INTERNAL);
  driver_test.RunInDriverContext([&](TestDwI2c& driver) {
    driver.SystemSuspend(
        fdf::SuspendCompleter([&](zx::result<> result) { suspend_result = result; }));
  });
  ASSERT_OK(suspend_result);

  driver_test.RunInEnvironmentTypeContext([](TestEnvironment& env) {
    EXPECT_FALSE(env.powerdomain().is_enabled());
    EXPECT_FALSE(env.clock_bus().enabled());
    EXPECT_FALSE(env.clock_regs().enabled());
  });

  // Simulate register state lost while power domain is disabled.
  controller.ResetWritableRegs();
  EXPECT_EQ(controller.ReadReg(0x00), 0u);
  EXPECT_EQ(controller.ReadReg(0x14), 0u);
  EXPECT_EQ(controller.ReadReg(0x1c), 0u);
  EXPECT_EQ(controller.ReadReg(0x3c), 0u);
  EXPECT_EQ(controller.ReadReg(0x7c), 0u);

  zx::result<> resume_result = zx::error(ZX_ERR_INTERNAL);
  driver_test.RunInDriverContext([&](TestDwI2c& driver) {
    driver.SystemResume(std::nullopt,
                        fdf::ResumeCompleter([&](zx::result<> result) { resume_result = result; }));
  });
  ASSERT_OK(resume_result);

  driver_test.RunInEnvironmentTypeContext([](TestEnvironment& env) {
    EXPECT_TRUE(env.powerdomain().is_enabled());
    EXPECT_TRUE(env.clock_bus().enabled());
    EXPECT_TRUE(env.clock_regs().enabled());
    EXPECT_TRUE(env.reset().take_deasserted());
  });
  EXPECT_NE(controller.ReadReg(0x00), 0u);  // ControlReg
  EXPECT_NE(controller.ReadReg(0x14), 0u);  // StandardSpeedSclHighCountReg
  EXPECT_NE(controller.ReadReg(0x1c), 0u);  // FastSpeedSclHighCountReg
  EXPECT_NE(controller.ReadReg(0x3c), 0u);  // TxFifoThresholdReg
  EXPECT_NE(controller.ReadReg(0x7c), 0u);  // SdaHoldReg

  ASSERT_OK(driver_test.StopDriver());
}

TEST(DwI2cTest, SuspendAndResumeRollbackOnError) {
  fdf_testing::BackgroundDriverTest<TestConfig> driver_test;

  zx::interrupt interrupt;
  ASSERT_EQ(ZX_OK, zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt));

  driver_test.RunInEnvironmentTypeContext([&](auto& env) { env.Init(std::move(interrupt)); });

  FakeDwI2cController controller;
  TestDwI2c::set_mmio(controller.GetMmioBuffer());

  ASSERT_OK(driver_test.StartDriver());

  // Make disabling clock_bus fail during SystemSuspend; clock_regs should be re-enabled.
  driver_test.RunInEnvironmentTypeContext(
      [](TestEnvironment& env) { env.clock_bus().set_disable_result(zx::error(ZX_ERR_IO)); });

  zx::result<> suspend_result = zx::ok();
  driver_test.RunInDriverContext([&](TestDwI2c& driver) {
    driver.SystemSuspend(
        fdf::SuspendCompleter([&](zx::result<> result) { suspend_result = result; }));
  });
  EXPECT_STATUS(suspend_result, ZX_ERR_IO);

  driver_test.RunInEnvironmentTypeContext([](TestEnvironment& env) {
    EXPECT_TRUE(env.powerdomain().is_enabled());
    EXPECT_TRUE(env.clock_bus().enabled());
    EXPECT_TRUE(env.clock_regs().enabled());
    env.clock_bus().set_disable_result(zx::ok());
  });

  // Now SystemSuspend should succeed.
  driver_test.RunInDriverContext([&](TestDwI2c& driver) {
    driver.SystemSuspend(
        fdf::SuspendCompleter([&](zx::result<> result) { suspend_result = result; }));
  });
  ASSERT_OK(suspend_result);

  // Make enabling clock_regs fail during SystemResume; clock_bus and powerdomain should roll back.
  driver_test.RunInEnvironmentTypeContext(
      [](TestEnvironment& env) { env.clock_regs().set_enable_result(zx::error(ZX_ERR_IO)); });

  zx::result<> resume_result = zx::ok();
  driver_test.RunInDriverContext([&](TestDwI2c& driver) {
    driver.SystemResume(std::nullopt,
                        fdf::ResumeCompleter([&](zx::result<> result) { resume_result = result; }));
  });
  EXPECT_STATUS(resume_result, ZX_ERR_IO);

  driver_test.RunInEnvironmentTypeContext([](TestEnvironment& env) {
    EXPECT_FALSE(env.powerdomain().is_enabled());
    EXPECT_FALSE(env.clock_bus().enabled());
    EXPECT_FALSE(env.clock_regs().enabled());
    env.clock_regs().set_enable_result(zx::ok());
  });

  // Now SystemResume should succeed.
  driver_test.RunInDriverContext([&](TestDwI2c& driver) {
    driver.SystemResume(std::nullopt,
                        fdf::ResumeCompleter([&](zx::result<> result) { resume_result = result; }));
  });
  ASSERT_OK(resume_result);

  ASSERT_OK(driver_test.StopDriver());
}

}  // namespace dw_i2c
