// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/dwc3/dwc3.h"

#include <fidl/fuchsia.hardware.clock/cpp/test_base.h>
#include <fidl/fuchsia.hardware.interconnect/cpp/fidl.h>
#include <fidl/fuchsia.hardware.platform.device/cpp/fidl.h>
#include <fidl/fuchsia.hardware.reset/cpp/test_base.h>
#include <fidl/fuchsia.hardware.usb.phy/cpp/fidl.h>

#include <optional>

#include <fake-mmio-reg/fake-mmio-reg.h>
#include <gtest/gtest.h>

#include "lib/driver/fake-platform-device/cpp/fake-pdev.h"
#include "lib/driver/testing/cpp/driver_test.h"
#include "src/devices/usb/drivers/dwc3/dwc3-regs.h"
#include "src/devices/usb/drivers/dwc3/dwc3_config.h"
namespace dwc3 {

namespace fclock = fuchsia_hardware_clock;
namespace fhi = fuchsia_hardware_interconnect;
namespace fpdev = fuchsia_hardware_platform_device;
namespace fphy = fuchsia_hardware_usb_phy;
namespace freset = fuchsia_hardware_reset;

class FakeReset : public fidl::testing::TestBase<freset::Reset> {
 public:
  freset::Service::InstanceHandler GetInstanceHandler(async_dispatcher_t* dispatcher) {
    return freset::Service::InstanceHandler({
        .reset = bindings_.CreateHandler(this, dispatcher, fidl::kIgnoreBindingClosure),
    });
  }

  bool toggled() {
    const bool value = toggled_;
    toggled_ = false;
    return value;
  }

 private:
  void ToggleWithTimeout(ToggleWithTimeoutRequest& request,
                         ToggleWithTimeoutCompleter::Sync& completer) override {
    toggled_ = true;
    completer.Reply(zx::ok());
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<freset::Reset> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {
    FAIL();
  }

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override { FAIL(); }

  bool toggled_ = false;
  fidl::ServerBindingGroup<freset::Reset> bindings_;
};

class FakeClock : public fidl::testing::TestBase<fuchsia_hardware_clock::Clock> {
 public:
  fclock::Service::InstanceHandler GetInstanceHandler(async_dispatcher_t* dispatcher) {
    return fclock::Service::InstanceHandler({
        .clock = bindings_.CreateHandler(this, dispatcher, fidl::kIgnoreBindingClosure),
    });
  }

  bool enabled() const { return enabled_; }

 private:
  void Enable(EnableCompleter::Sync& completer) override {
    enabled_ = true;
    completer.Reply(zx::ok());
  }

  void Disable(DisableCompleter::Sync& completer) override {
    enabled_ = false;
    completer.Reply(zx::ok());
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_hardware_clock::Clock> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {
    FAIL();
  }

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override { FAIL(); }

  bool enabled_ = false;
  fidl::ServerBindingGroup<fuchsia_hardware_clock::Clock> bindings_;
};

class FakeUsbPhy : public fidl::Server<fphy::UsbPhy>, public fidl::Server<fphy::ConnectionWatcher> {
 public:
  ~FakeUsbPhy() override {
    EXPECT_TRUE(watch_connection_status_changed_called_);
    EXPECT_TRUE(completer_.has_value());
  }

  fuchsia_hardware_usb_phy::Service::InstanceHandler GetUsbPhyInstanceHandler(
      async_dispatcher_t* dispatcher) {
    return fuchsia_hardware_usb_phy::Service::InstanceHandler({
        .device = bindings_.CreateHandler(this, dispatcher, fidl::kIgnoreBindingClosure),
    });
  }

  fuchsia_hardware_usb_phy::ConnectionWatcherService::InstanceHandler
  GetConnectionWatcherInstanceHandler(async_dispatcher_t* dispatcher) {
    return fuchsia_hardware_usb_phy::ConnectionWatcherService::InstanceHandler({
        .watcher = watcher_bindings_.CreateHandler(this, dispatcher, fidl::kIgnoreBindingClosure),
    });
  }

 private:
  void ConnectStatusChanged(ConnectStatusChangedRequest& request,
                            ConnectStatusChangedCompleter::Sync& completer) override {
    completer.Reply(zx::ok());
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_hardware_usb_phy::UsbPhy> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {
    FDF_LOG(ERROR, "Unknown method %lu", metadata.method_ordinal);
  }

  void WatchConnectStatusChanged(WatchConnectStatusChangedCompleter::Sync& completer) override {
    if (!watch_connection_status_changed_called_) {
      completer.Reply(zx::ok(false));
      watch_connection_status_changed_called_ = true;
      return;
    }

    ASSERT_FALSE(completer_.has_value());
    completer_.emplace(completer.ToAsync());
  }

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_hardware_usb_phy::ConnectionWatcher> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override {
    FDF_LOG(ERROR, "Unknown method %lu", metadata.method_ordinal);
  }

  fidl::ServerBindingGroup<fuchsia_hardware_usb_phy::UsbPhy> bindings_;
  fidl::ServerBindingGroup<fuchsia_hardware_usb_phy::ConnectionWatcher> watcher_bindings_;

  bool watch_connection_status_changed_called_ = false;
  std::optional<WatchConnectStatusChangedCompleter::Async> completer_;
};

class FakePath final : public fidl::Server<fhi::Path> {
 public:
  explicit FakePath() = default;
  virtual ~FakePath() = default;

  fhi::PathService::InstanceHandler GetInstanceHandler(async_dispatcher_t* dispatcher) {
    return fhi::PathService::InstanceHandler({
        .path = bindings_.CreateHandler(this, dispatcher, fidl::kIgnoreBindingClosure),
    });
  }

  void SetBandwidth(SetBandwidthRequest& request, SetBandwidthCompleter::Sync& completer) override {
    completer.Reply(zx::ok());
  }
  void handle_unknown_method(fidl::UnknownMethodMetadata<fhi::Path> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {}

 private:
  fidl::ServerBindingGroup<fhi::Path> bindings_;
};

class Environment : public fdf_testing::Environment {
 public:
  Environment() {
    auto config = fdf_fake::FakePDev::Config{};
    config.mmios[0] = reg_region_.GetMmioBuffer();
    config.use_fake_bti = true;
    config.use_fake_irq = true;

    pdev_.SetConfig(std::move(config));
  }

  zx::result<> Serve(fdf::OutgoingDirectory& directory) override {
    auto* dispatcher = fdf::Dispatcher::GetCurrent()->async_dispatcher();

    zx::result result =
        directory.AddService<fpdev::Service>(pdev_.GetInstanceHandler(dispatcher), "pdev");
    EXPECT_TRUE(result.is_ok());

    result = directory.AddService<fhi::PathService>(path_.GetInstanceHandler(dispatcher),
                                                    "interconnect-usb-ddr");
    EXPECT_TRUE(result.is_ok());

    result = directory.AddService<fhi::PathService>(path_.GetInstanceHandler(dispatcher),
                                                    "interconnect-usb-ipa");
    EXPECT_TRUE(result.is_ok());

    result = directory.AddService<fhi::PathService>(path_.GetInstanceHandler(dispatcher),
                                                    "interconnect-ddr-usb");
    EXPECT_TRUE(result.is_ok());

    result = directory.AddService<fphy::Service>(usb_phy_.GetUsbPhyInstanceHandler(dispatcher),
                                                 "dwc3-phy");
    EXPECT_TRUE(result.is_ok());

    result = directory.AddService<fphy::ConnectionWatcherService>(
        usb_phy_.GetConnectionWatcherInstanceHandler(dispatcher), "dwc3-phy");
    EXPECT_TRUE(result.is_ok());

    result = directory.AddService<fclock::Service>(clock_.GetInstanceHandler(dispatcher), "xo");
    EXPECT_TRUE(result.is_ok());

    result =
        directory.AddService<fclock::Service>(clock_.GetInstanceHandler(dispatcher), "sleep-clk");
    EXPECT_TRUE(result.is_ok());

    result =
        directory.AddService<fclock::Service>(clock_.GetInstanceHandler(dispatcher), "iface-clk");
    EXPECT_TRUE(result.is_ok());

    result =
        directory.AddService<fclock::Service>(clock_.GetInstanceHandler(dispatcher), "core-clk");
    EXPECT_TRUE(result.is_ok());

    result =
        directory.AddService<fclock::Service>(clock_.GetInstanceHandler(dispatcher), "utmi-clk");
    EXPECT_TRUE(result.is_ok());

    result = directory.AddService<fclock::Service>(clock_.GetInstanceHandler(dispatcher),
                                                   "bus-aggr-clk");
    EXPECT_TRUE(result.is_ok());

    result = directory.AddService<freset::Service>(reset_.GetInstanceHandler(dispatcher), "reset");
    EXPECT_TRUE(result.is_ok());

    return zx::ok();
  }

  ddk_fake::FakeMmioRegRegion& reg_region() { return reg_region_; }

  const FakeClock& clock() const { return clock_; }
  FakeReset& reset() { return reset_; }

 private:
  static constexpr size_t kRegSize = sizeof(uint32_t);
  static constexpr size_t kMmioRegionSize = 64 << 10;
  static constexpr size_t kRegCount = kMmioRegionSize / kRegSize;

  fdf_fake::FakePDev pdev_;
  ddk_fake::FakeMmioRegRegion reg_region_{kRegSize, kRegCount};
  FakePath path_;
  FakeUsbPhy usb_phy_;
  FakeClock clock_;
  FakeReset reset_;
};

class Config final {
 public:
  using DriverType = Dwc3;
  using EnvironmentType = Environment;
};

// Test is templated on a parameter which, if true, will have the harness start and stop the driver.
// Otherwise, it is the individual test(s) responsibility to start and stop the driver.
template <bool manage_lifetime>
class TestFixture : public testing::Test {
 public:
  void SetUp() override {
    dut_.RunInEnvironmentTypeContext([&](Environment& env) {
      auto& hwparams3 = env.reg_region()[GHWPARAMS3::Get().addr()];
      auto& ver_reg = env.reg_region()[USB31_VER_NUMBER::Get().addr()];
      auto& dctl_reg = env.reg_region()[DCTL::Get().addr()];

      hwparams3.SetReadCallback([this]() -> uint64_t { return Read_GHWPARAMS3(); });
      ver_reg.SetReadCallback([this]() -> uint64_t { return Read_USB31_VER_NUMBER(); });
      dctl_reg.SetReadCallback([this]() -> uint64_t { return Read_DCTL(); });
      dctl_reg.SetWriteCallback([this](uint64_t val) { return Write_DCTL(val); });
    });

    if (manage_lifetime) {
      ASSERT_TRUE(dut_.StartDriverWithCustomStartArgs([](fdf::DriverStartArgs& args) {
                        dwc3_config::Config cfg;
                        cfg.enable_suspend() = false;
                        args.config(cfg.ToVmo());
                      })
                      .is_ok());
    }
  }

  void TearDown() override {
    if (manage_lifetime) {
      EXPECT_EQ(ZX_OK, dut_.StopDriver().status_value());
    }
  }

 protected:
  // Section 1.3.22 of the DWC3 Programmer's guide
  //
  // DWC_USB31_CACHE_TOTAL_XFER_RESOURCES : 32
  // DWC_USB31_NUM_IN_EPS                 : 16
  // DWC_USB31_NUM_EPS                    : 32
  // DWC_USB31_VENDOR_CTL_INTERFACE       : 0
  // DWC_USB31_HSPHY_DWIDTH               : 2
  // DWC_USB31_HSPHY_INTERFACE            : 1
  // DWC_USB31_SSPHY_INTERFACE            : 2
  uint64_t Read_GHWPARAMS3() { return 0x10420086; }

  // Section 1.3.45 of the DWC3 Programmer's guide
  uint64_t Read_USB31_VER_NUMBER() { return 0x31363061; }  // 1.60a

  // Section 1.4.2 of the DWC3 Programmer's guide
  uint64_t Read_DCTL() { return dctl_val_; }
  void Write_DCTL(uint64_t val) {
    constexpr uint32_t kUnwriteableMask =
        (1 << 29) | (1 << 17) | (1 << 16) | (1 << 15) | (1 << 14) | (1 << 13) | (1 << 0);
    ZX_ASSERT(val <= std::numeric_limits<uint32_t>::max());
    dctl_val_ = static_cast<uint32_t>(val & ~kUnwriteableMask);

    // Immediately clear the soft reset bit if we are not testing the soft reset
    // timeout behavior.
    if (!stuck_reset_test_) {
      dctl_val_ = DCTL::Get().FromValue(dctl_val_).set_CSFTRST(0).reg_value();
    }
  }

  uint32_t dctl_val_ = DCTL::Get().FromValue(0).set_LPM_NYET_thres(0xF).reg_value();
  bool stuck_reset_test_{false};

  fdf_testing::ForegroundDriverTest<Config> dut_;
};

using ManagedTestFixture = TestFixture<true>;
using UnmanagedTestFixture = TestFixture<false>;

TEST_F(ManagedTestFixture, Dfv2Lifecycle) {
  dut_.RunInNodeContext(
      [&](fdf_testing::TestNode& node) { EXPECT_EQ(1UL, node.children().size()); });
}

TEST_F(ManagedTestFixture, ResourcesManagedInStart) {
  dut_.RunInEnvironmentTypeContext([](Environment& env) {
    EXPECT_FALSE(env.reset().toggled());
    EXPECT_TRUE(env.clock().enabled());
  });
}

TEST_F(UnmanagedTestFixture, Dfv2HwResetTimeout) {
  stuck_reset_test_ = true;
  zx::result start = dut_.StartDriverWithCustomStartArgs([](fdf::DriverStartArgs& args) {
    dwc3_config::Config cfg;
    cfg.enable_suspend() = false;
    args.config(cfg.ToVmo());
  });
  ASSERT_TRUE(start.is_error());
  ASSERT_EQ(ZX_ERR_TIMED_OUT, start.error_value());

  dut_.RunInNodeContext(
      [&](fdf_testing::TestNode& node) { EXPECT_EQ(0UL, node.children().size()); });

  // The dfv2 driver did not start, nothing to stop.
}

}  // namespace dwc3
