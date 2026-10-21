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
  void set_powered(bool powered) {
    mmio_[0xfc].SetReadCallback([powered]() { return powered ? DwI2c::kDwCompTypeNum : 0u; });
  }

  std::array<uint32_t, kRegCount> regs_{};
  ddk_fake::FakeMmioRegRegion mmio_;
};

class FakePinStates : public fidl::WireServer<fuchsia_hardware_pin::PinStates> {
 public:
  fuchsia_hardware_pin::PinStatesService::InstanceHandler CreateInstanceHandler(
      async_dispatcher_t* dispatcher) {
    return fuchsia_hardware_pin::PinStatesService::InstanceHandler({
        .device = bindings_.CreateHandler(this, dispatcher, fidl::kIgnoreBindingClosure),
    });
  }

  void set_on_select_state(fit::function<void(std::string_view)> callback) {
    on_select_state_ = std::move(callback);
  }

  const std::optional<std::string>& last_state() const { return last_state_; }

 private:
  void SelectState(fuchsia_hardware_pin::wire::PinStatesSelectStateRequest* request,
                   SelectStateCompleter::Sync& completer) override {
    std::string state(request->name.get());
    if (on_select_state_) {
      on_select_state_(state);
    }
    last_state_ = std::move(state);
    completer.ReplySuccess();
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_hardware_pin::PinStates> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  std::optional<std::string> last_state_;
  fit::function<void(std::string_view)> on_select_state_;
  fidl::ServerBindingGroup<fuchsia_hardware_pin::PinStates> bindings_;
};

// Reference-counted clock server that only gates the hardware clock on the 0 -> 1 transition and
// off on the 1 -> 0 transition, matching reference-counted platform clock managers (such as
// google-cpm).
class RefCountedClock final : public fidl::testing::WireTestBase<fuchsia_hardware_clock::Clock> {
 public:
  fuchsia_hardware_clock::Service::InstanceHandler CreateInstanceHandler(
      async_dispatcher_t* dispatcher) {
    return fuchsia_hardware_clock::Service::InstanceHandler({
        .clock = bindings_.CreateHandler(this, dispatcher, fidl::kIgnoreBindingClosure),
    });
  }

  void set_on_hw_gate_change(fit::function<void(bool)> callback) {
    on_hw_gate_change_ = std::move(callback);
  }

  // Simulates forced ambient suspend collapsing the parent power domain and gating off the
  // hardware clock without changing the software reference count in the clock manager.
  void ForceHardwareGateOff() {
    hw_gated_on_ = false;
    if (on_hw_gate_change_) {
      on_hw_gate_change_(false);
    }
  }

  uint32_t ref_count() const { return ref_count_; }
  uint32_t enable_calls() const { return enable_calls_; }
  uint32_t disable_calls() const { return disable_calls_; }
  bool hw_gated_on() const { return hw_gated_on_; }

 private:
  void Enable(EnableCompleter::Sync& completer) override {
    ++enable_calls_;
    if (ref_count_ == 0) {
      hw_gated_on_ = true;
      if (on_hw_gate_change_) {
        on_hw_gate_change_(true);
      }
    }
    ++ref_count_;
    completer.ReplySuccess();
  }

  void Disable(DisableCompleter::Sync& completer) override {
    ++disable_calls_;
    if (ref_count_ == 0) {
      completer.ReplySuccess();
      return;
    }
    --ref_count_;
    if (ref_count_ == 0) {
      hw_gated_on_ = false;
      if (on_hw_gate_change_) {
        on_hw_gate_change_(false);
      }
    }
    completer.ReplySuccess();
  }

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  uint32_t ref_count_ = 0;
  uint32_t enable_calls_ = 0;
  uint32_t disable_calls_ = 0;
  bool hw_gated_on_ = false;
  fit::function<void(bool)> on_hw_gate_change_;
  fidl::ServerBindingGroup<fuchsia_hardware_clock::Clock> bindings_;
};

// Reference-counted power domain server that tracks software enable/disable vote balance across
// driver restarts and suspend/resume cycles.
class RefCountedPowerDomain final
    : public fidl::testing::WireTestBase<fuchsia_hardware_powerdomain::Domain> {
 public:
  fuchsia_hardware_powerdomain::Service::InstanceHandler CreateInstanceHandler(
      async_dispatcher_t* dispatcher) {
    return fuchsia_hardware_powerdomain::Service::InstanceHandler({
        .domain = bindings_.CreateHandler(this, dispatcher, fidl::kIgnoreBindingClosure),
    });
  }

  uint32_t ref_count() const { return ref_count_; }
  uint32_t enable_calls() const { return enable_calls_; }
  uint32_t disable_calls() const { return disable_calls_; }
  bool is_enabled() const { return ref_count_ > 0; }

 private:
  void Enable(EnableCompleter::Sync& completer) override {
    ++enable_calls_;
    ++ref_count_;
    completer.ReplySuccess();
  }

  void Disable(DisableCompleter::Sync& completer) override {
    ++disable_calls_;
    if (ref_count_ > 0) {
      --ref_count_;
    }
    completer.ReplySuccess();
  }

  void IsEnabled(IsEnabledCompleter::Sync& completer) override {
    completer.ReplySuccess(ref_count_ > 0);
  }

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_hardware_powerdomain::Domain> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  uint32_t ref_count_ = 0;
  uint32_t enable_calls_ = 0;
  uint32_t disable_calls_ = 0;
  fidl::ServerBindingGroup<fuchsia_hardware_powerdomain::Domain> bindings_;
};

struct SharedRefCountedResources {
  RefCountedPowerDomain powerdomain;
  RefCountedClock clock_bus;
  RefCountedClock clock_regs;
};

class TestEnvironment : public fdf_testing::Environment {
 public:
  void Init(zx::interrupt interrupt, SharedRefCountedResources* shared_resources = nullptr) {
    std::map<uint32_t, zx::interrupt> irqs;
    irqs[0] = std::move(interrupt);
    pdev_.SetConfig({.irqs = std::move(irqs)});
    shared_resources_ = shared_resources;
  }

  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    async_dispatcher_t* dispatcher = fdf::Dispatcher::GetCurrent()->async_dispatcher();
    EXPECT_OK(to_driver_vfs.AddService<fuchsia_hardware_platform_device::Service>(
        pdev_.GetInstanceHandler(dispatcher), "pdev"));
    if (shared_resources_) {
      EXPECT_OK(to_driver_vfs.AddService<fuchsia_hardware_clock::Service>(
          shared_resources_->clock_bus.CreateInstanceHandler(dispatcher), "bus"));
      EXPECT_OK(to_driver_vfs.AddService<fuchsia_hardware_clock::Service>(
          shared_resources_->clock_regs.CreateInstanceHandler(dispatcher), "registers"));
      EXPECT_OK(to_driver_vfs.AddService<fuchsia_hardware_powerdomain::Service>(
          shared_resources_->powerdomain.CreateInstanceHandler(dispatcher), "power-domain"));
    } else {
      EXPECT_OK(to_driver_vfs.AddService<fuchsia_hardware_clock::Service>(
          clock_bus_.CreateInstanceHandler(dispatcher), "bus"));
      EXPECT_OK(to_driver_vfs.AddService<fuchsia_hardware_clock::Service>(
          clock_regs_.CreateInstanceHandler(dispatcher), "registers"));
      EXPECT_OK(to_driver_vfs.AddService<fuchsia_hardware_powerdomain::Service>(
          powerdomain_.CreateInstanceHandler(), "power-domain"));
    }
    EXPECT_OK(to_driver_vfs.AddService<fuchsia_hardware_pin::PinStatesService>(
        pin_states_.CreateInstanceHandler(dispatcher), "pin-states-0"));
    EXPECT_OK(to_driver_vfs.AddService<fuchsia_hardware_reset::Service>(
        reset_.CreateInstanceHandler(), "reset"));
    return zx::ok();
  }

  fdf_fake::FakeClock& clock_bus() { return clock_bus_; }
  fdf_fake::FakeClock& clock_regs() { return clock_regs_; }
  FakePinStates& pin_states() { return pin_states_; }
  fdf_fake::FakePowerDomain& powerdomain() { return powerdomain_; }
  fdf_fake::FakeReset& reset() { return reset_; }

 private:
  fdf_fake::FakePDev pdev_;
  fdf_fake::FakeClock clock_bus_;
  fdf_fake::FakeClock clock_regs_;
  FakePinStates pin_states_;
  fdf_fake::FakePowerDomain powerdomain_;
  fdf_fake::FakeReset reset_;
  SharedRefCountedResources* shared_resources_ = nullptr;
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
  driver_test.RunInEnvironmentTypeContext([](TestEnvironment& env) {
    EXPECT_TRUE(env.powerdomain().is_enabled());
    EXPECT_TRUE(env.clock_bus().enabled());
    EXPECT_TRUE(env.clock_regs().enabled());
  });

  ASSERT_TRUE(driver_test.StopDriver().is_ok());
  driver_test.RunInEnvironmentTypeContext([](TestEnvironment& env) {
    EXPECT_FALSE(env.powerdomain().is_enabled());
    EXPECT_FALSE(env.clock_bus().enabled());
    EXPECT_FALSE(env.clock_regs().enabled());
  });
}

// Regression test for b/575642111:
// When dw-i2c restarts (e.g. via `ffx driver restart`), Stop() must release its clock and power
// domain votes so the new driver instance starts with ref_count == 1 instead of 2. Otherwise,
// SystemSuspend() only decrements ref_count from 2 to 1, and if the platform power manager
// forcibly collapses the power domain during ambient suspend (gating off the hardware clocks),
// SystemResume()'s Enable() sees ref_count == 1 != 0, skips the hardware clock enable, and
// HostInit() reads 0x0 from CompTypeReg (0xfc), failing resume with ZX_ERR_NOT_SUPPORTED.
TEST(DwI2cTest, RestartThenForcedAmbientSuspendAndResume) {
  SharedRefCountedResources shared_resources;
  FakeDwI2cController controller;
  controller.set_powered(false);
  shared_resources.clock_regs.set_on_hw_gate_change(
      [&](bool gated_on) { controller.set_powered(gated_on); });

  // First driver instance (before driver restart).
  {
    fdf_testing::BackgroundDriverTest<TestConfig> driver_test;
    zx::interrupt interrupt;
    ASSERT_EQ(ZX_OK, zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt));
    driver_test.RunInEnvironmentTypeContext(
        [&](TestEnvironment& env) { env.Init(std::move(interrupt), &shared_resources); });
    TestDwI2c::set_mmio(controller.GetMmioBuffer());

    ASSERT_OK(driver_test.StartDriver());
    driver_test.RunInEnvironmentTypeContext([&](TestEnvironment&) {
      EXPECT_EQ(shared_resources.powerdomain.ref_count(), 1u);
      EXPECT_EQ(shared_resources.clock_bus.ref_count(), 1u);
      EXPECT_EQ(shared_resources.clock_regs.ref_count(), 1u);
      EXPECT_TRUE(shared_resources.clock_regs.hw_gated_on());
    });

    ASSERT_OK(driver_test.StopDriver());
    driver_test.RunInEnvironmentTypeContext([&](TestEnvironment&) {
      EXPECT_EQ(shared_resources.powerdomain.ref_count(), 0u);
      EXPECT_EQ(shared_resources.clock_bus.ref_count(), 0u);
      EXPECT_EQ(shared_resources.clock_regs.ref_count(), 0u);
      EXPECT_FALSE(shared_resources.clock_regs.hw_gated_on());
    });
  }

  // Second driver instance (after driver restart), followed by ambient suspend and resume.
  {
    fdf_testing::BackgroundDriverTest<TestConfig> driver_test;
    zx::interrupt interrupt;
    ASSERT_EQ(ZX_OK, zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt));
    driver_test.RunInEnvironmentTypeContext(
        [&](TestEnvironment& env) { env.Init(std::move(interrupt), &shared_resources); });
    TestDwI2c::set_mmio(controller.GetMmioBuffer());

    ASSERT_OK(driver_test.StartDriver());
    driver_test.RunInEnvironmentTypeContext([&](TestEnvironment&) {
      EXPECT_EQ(shared_resources.powerdomain.ref_count(), 1u);
      EXPECT_EQ(shared_resources.clock_bus.ref_count(), 1u);
      EXPECT_EQ(shared_resources.clock_regs.ref_count(), 1u);
      EXPECT_TRUE(shared_resources.clock_regs.hw_gated_on());
    });

    zx::result<> suspend_result = zx::error(ZX_ERR_INTERNAL);
    driver_test.RunInDriverContext([&](TestDwI2c& driver) {
      driver.SystemSuspend(
          fdf::SuspendCompleter([&](zx::result<> result) { suspend_result = result; }));
    });
    ASSERT_OK(suspend_result);

    // Simulate ambient suspend collapsing the LSIO_E power domain and gating off the hardware
    // clocks while register state is lost.
    controller.ResetWritableRegs();
    driver_test.RunInEnvironmentTypeContext([&](TestEnvironment&) {
      shared_resources.clock_bus.ForceHardwareGateOff();
      shared_resources.clock_regs.ForceHardwareGateOff();
    });

    zx::result<> resume_result = zx::error(ZX_ERR_INTERNAL);
    driver_test.RunInDriverContext([&](TestDwI2c& driver) {
      driver.SystemResume(
          std::nullopt, fdf::ResumeCompleter([&](zx::result<> result) { resume_result = result; }));
    });
    EXPECT_OK(resume_result);

    driver_test.RunInEnvironmentTypeContext([&](TestEnvironment&) {
      EXPECT_EQ(shared_resources.powerdomain.ref_count(), 1u);
      EXPECT_EQ(shared_resources.clock_bus.ref_count(), 1u);
      EXPECT_EQ(shared_resources.clock_regs.ref_count(), 1u);
      EXPECT_TRUE(shared_resources.clock_regs.hw_gated_on());
    });
    EXPECT_NE(controller.ReadReg(0x00), 0u);  // ControlReg

    ASSERT_OK(driver_test.StopDriver());
    driver_test.RunInEnvironmentTypeContext([&](TestEnvironment&) {
      EXPECT_EQ(shared_resources.powerdomain.ref_count(), 0u);
      EXPECT_EQ(shared_resources.clock_bus.ref_count(), 0u);
      EXPECT_EQ(shared_resources.clock_regs.ref_count(), 0u);
    });
  }
}

TEST(DwI2cTest, StopWhileSuspendedDoesNotDoubleDisable) {
  SharedRefCountedResources shared_resources;
  FakeDwI2cController controller;
  TestDwI2c::set_mmio(controller.GetMmioBuffer());

  fdf_testing::BackgroundDriverTest<TestConfig> driver_test;
  zx::interrupt interrupt;
  ASSERT_EQ(ZX_OK, zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt));
  driver_test.RunInEnvironmentTypeContext(
      [&](TestEnvironment& env) { env.Init(std::move(interrupt), &shared_resources); });

  ASSERT_OK(driver_test.StartDriver());

  zx::result<> suspend_result = zx::error(ZX_ERR_INTERNAL);
  driver_test.RunInDriverContext([&](TestDwI2c& driver) {
    driver.SystemSuspend(
        fdf::SuspendCompleter([&](zx::result<> result) { suspend_result = result; }));
  });
  ASSERT_OK(suspend_result);

  driver_test.RunInEnvironmentTypeContext([&](TestEnvironment&) {
    EXPECT_EQ(shared_resources.powerdomain.disable_calls(), 1u);
    EXPECT_EQ(shared_resources.clock_bus.disable_calls(), 1u);
    EXPECT_EQ(shared_resources.clock_regs.disable_calls(), 1u);
  });

  ASSERT_OK(driver_test.StopDriver());
  driver_test.RunInEnvironmentTypeContext([&](TestEnvironment&) {
    EXPECT_EQ(shared_resources.powerdomain.disable_calls(), 1u);
    EXPECT_EQ(shared_resources.clock_bus.disable_calls(), 1u);
    EXPECT_EQ(shared_resources.clock_regs.disable_calls(), 1u);
  });
}

TEST(DwI2cTest, StartRollbackOnError) {
  fdf_testing::BackgroundDriverTest<TestConfig> driver_test;

  zx::interrupt interrupt;
  ASSERT_EQ(ZX_OK, zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt));

  driver_test.RunInEnvironmentTypeContext([&](auto& env) { env.Init(std::move(interrupt)); });

  // Simulate CompTypeReg returning 0x0 so HostInit() fails with ZX_ERR_NOT_SUPPORTED during Start.
  FakeDwI2cController controller;
  controller.set_powered(false);
  TestDwI2c::set_mmio(controller.GetMmioBuffer());

  EXPECT_STATUS(driver_test.StartDriver(), ZX_ERR_NOT_SUPPORTED);

  driver_test.RunInEnvironmentTypeContext([](TestEnvironment& env) {
    EXPECT_FALSE(env.powerdomain().is_enabled());
    EXPECT_FALSE(env.clock_bus().enabled());
    EXPECT_FALSE(env.clock_regs().enabled());
  });
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

  // Simulate register and pin state lost while power domain is disabled.
  controller.ResetWritableRegs();
  controller.set_powered(false);
  driver_test.RunInEnvironmentTypeContext([&](TestEnvironment& env) {
    env.pin_states().set_on_select_state([&](std::string_view state) {
      if (state == "default") {
        controller.set_powered(true);
      }
    });
  });
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
    EXPECT_EQ(env.pin_states().last_state(), "default");
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
