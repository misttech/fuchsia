// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_USB_DRIVERS_DWC3_DWC3_TEST_FIXTURE_H_
#define SRC_DEVICES_USB_DRIVERS_DWC3_DWC3_TEST_FIXTURE_H_

#include <fidl/fuchsia.hardware.clock/cpp/test_base.h>
#include <fidl/fuchsia.hardware.interconnect/cpp/fidl.h>
#include <fidl/fuchsia.hardware.platform.device/cpp/fidl.h>
#include <fidl/fuchsia.hardware.reset/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.phy/cpp/fidl.h>
#include <fidl/fuchsia.hardware.vreg/cpp/test_base.h>
#include <lib/driver/fake-clock/cpp/fake-clock.h>
#include <lib/driver/fake-platform-device/cpp/fake-pdev.h>
#include <lib/driver/fake-reset/cpp/fake-reset.h>
#include <lib/driver/fake-vreg/cpp/fake-vreg.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/sync/cpp/completion.h>

#include <optional>

#include <fake-mmio-reg/fake-mmio-reg.h>
#include <gtest/gtest.h>

#include "src/devices/usb/drivers/dwc3/dwc3-regs.h"
#include "src/devices/usb/drivers/dwc3/dwc3.h"
#include "src/devices/usb/drivers/dwc3/dwc3_config.h"

namespace dwc3 {

namespace fclock = fuchsia_hardware_clock;
namespace fhi = fuchsia_hardware_interconnect;
namespace fpdev = fuchsia_hardware_platform_device;
namespace fphy = fuchsia_hardware_usb_phy;
namespace freset = fuchsia_hardware_reset;
namespace fvreg = fuchsia_hardware_vreg;

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

  void set_watch_connection_status_changed_called(bool set) {
    watch_connection_status_changed_called_ = set;
  }

  void TriggerConnection(bool connected) {
    ZX_ASSERT(completer_.has_value());
    fuchsia_hardware_usb_phy::ConnectionWatcherWatchConnectStatusChangedResponse response{{
        .connected = connected,
        .wake_lease = {},
    }};
    completer_->Reply(zx::ok(std::move(response)));
    completer_.reset();
  }

  libsync::Completion* completion() { return &completion_; }

 private:
  void ConnectStatusChanged(ConnectStatusChangedRequest& request,
                            ConnectStatusChangedCompleter::Sync& completer) override {
    completer.Reply(zx::ok());
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_hardware_usb_phy::UsbPhy> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {
    fdf::error("Unknown method {}", metadata.method_ordinal);
  }

  void WatchConnectStatusChanged(WatchConnectStatusChangedRequest& request,
                                 WatchConnectStatusChangedCompleter::Sync& completer) override {
    if (!watch_connection_status_changed_called_) {
      fuchsia_hardware_usb_phy::ConnectionWatcherWatchConnectStatusChangedResponse response{{
          .connected = false,
          .wake_lease = {},
      }};
      completer.Reply(zx::ok(std::move(response)));

      watch_connection_status_changed_called_ = true;
      return;
    }

    ASSERT_FALSE(completer_.has_value());
    completer_.emplace(completer.ToAsync());
    completion_.Signal();
  }

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_hardware_usb_phy::ConnectionWatcher> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override {
    fdf::error("Unknown method {}", metadata.method_ordinal);
  }

  fidl::ServerBindingGroup<fuchsia_hardware_usb_phy::UsbPhy> bindings_;
  fidl::ServerBindingGroup<fuchsia_hardware_usb_phy::ConnectionWatcher> watcher_bindings_;

  bool watch_connection_status_changed_called_ = false;
  std::optional<WatchConnectStatusChangedCompleter::Async> completer_;
  libsync::Completion completion_;  // Signled when the above completer_ is saved.
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

    result = directory.AddService<fclock::Service>(clock_.CreateInstanceHandler(dispatcher), "xo");
    EXPECT_TRUE(result.is_ok());

    result = directory.AddService<fclock::Service>(clock_.CreateInstanceHandler(dispatcher),
                                                   "sleep-clk");
    EXPECT_TRUE(result.is_ok());

    result = directory.AddService<fclock::Service>(clock_.CreateInstanceHandler(dispatcher),
                                                   "iface-clk");
    EXPECT_TRUE(result.is_ok());

    result =
        directory.AddService<fclock::Service>(clock_.CreateInstanceHandler(dispatcher), "core-clk");
    EXPECT_TRUE(result.is_ok());

    result =
        directory.AddService<fclock::Service>(clock_.CreateInstanceHandler(dispatcher), "utmi-clk");
    EXPECT_TRUE(result.is_ok());

    result = directory.AddService<fclock::Service>(clock_.CreateInstanceHandler(dispatcher),
                                                   "bus-aggr-clk");
    EXPECT_TRUE(result.is_ok());

    result = directory.AddService<freset::Service>(reset_.CreateInstanceHandler(), "reset");
    EXPECT_TRUE(result.is_ok());

    result = directory.AddService<fvreg::Service>(vreg_.CreateInstanceHandler(), "regulator");
    EXPECT_TRUE(result.is_ok());

    return zx::ok();
  }

  // Note: Only intended for teardown, does not restore default mock behaviors.
  void Reset() {
    for (size_t i = 0; i < kRegCount; i++) {
      reg_region_[i * kRegSize].SetReadCallback([]() { return 0; });
      reg_region_[i * kRegSize].SetWriteCallback([](uint64_t value) {});
    }
  }

  ddk_fake::FakeMmioRegRegion& reg_region() { return reg_region_; }

  FakeUsbPhy& usb_phy() { return usb_phy_; }
  const fdf_fake::FakeClock& clock() const { return clock_; }
  fdf_fake::FakeReset& reset() { return reset_; }
  const fdf_fake::FakeVreg& vreg() const { return vreg_; }

  static constexpr size_t kRegSize = sizeof(uint32_t);
  static constexpr size_t kMmioRegionSize = 0x10'0000;
  static constexpr size_t kRegCount = kMmioRegionSize / kRegSize;

 private:
  fdf_fake::FakePDev pdev_;
  ddk_fake::FakeMmioRegRegion reg_region_{kRegSize, kRegCount};
  FakePath path_;
  FakeUsbPhy usb_phy_;
  fdf_fake::FakeClock clock_;
  fdf_fake::FakeReset reset_;
  fdf_fake::FakeVreg vreg_;
};

class Config final {
 public:
  using DriverType = Dwc3;
  using EnvironmentType = Environment;
};

// Test is templated on a parameter which, if true, will have the harness start and stop the driver.
// Otherwise, it is the individual test(s) responsibility to start and stop the driver.
template <bool manage_lifetime, typename gtest_base = testing::Test>
class TestFixture : public gtest_base {
 public:
  using Endpoint = Dwc3::Endpoint;
  using TransferState = Dwc3::Endpoint::TransferState;

  static Dwc3::UserEndpoint& GetUserEndpoint(Dwc3& drv, uint8_t ep_num) {
    auto* uep = drv.get_user_endpoint(ep_num);
    ZX_ASSERT(uep != nullptr);
    return *uep;
  }

  static uint8_t UsbAddressToEpNum(uint8_t addr) { return Dwc3::UsbAddressToEpNum(addr); }

  static const zx::bti& GetBti(const Dwc3& drv) { return drv.bti_; }

  static void TriggerEpTransferNotReady(Dwc3& drv, uint8_t ep_num, uint32_t stage) {
    drv.HandleEpTransferNotReadyEvent(ep_num, stage);
  }

  static void TriggerEpTransferComplete(Dwc3& drv, uint8_t ep_num) {
    auto* uep = drv.get_user_endpoint(ep_num);
    ZX_ASSERT(uep != nullptr);

    if (uep->fifo.GetActiveCount() > 0) {
      dwc3_trb_t* trb = uep->fifo.read_;
      trb->control &= ~TRB_HWO;
      trb->status = 0;  // Set residual byte count to 0 (indicating successful full transfer!)
      uep->fifo.Write(trb, 1);
    }

    drv.HandleEpTransferCompleteEvent(ep_num);

    // In production, SendCompletions is called at the end of the global event
    // interrupt handler loop (once per interrupt batch), rather than from
    // inside the individual endpoint event handlers. We simulate that final
    // step here so that completions are immediately dispatched to test
    // clients.
    uep->server->SendCompletions();
  }

  static void TriggerEpTransferInProgress(Dwc3& drv, uint8_t ep_num) {
    auto* uep = drv.get_user_endpoint(ep_num);
    ZX_ASSERT(uep != nullptr);

    if (uep->fifo.GetActiveCount() > 0) {
      dwc3_trb_t* trb = uep->fifo.read_;
      trb->control &= ~TRB_HWO;
      trb->status = 0;
      uep->fifo.Write(trb, 1);
    }

    drv.HandleEpTransferInProgressEvent(ep_num);

    // In production, SendCompletions is called at the end of the global event
    // interrupt handler loop (once per interrupt batch), rather than from
    // inside the individual endpoint event handlers. We simulate that final
    // step here so that completions are immediately dispatched to test
    // clients.
    uep->server->SendCompletions();
  }

  static void TriggerEpTransferStarted(Dwc3& drv, uint8_t ep_num, uint32_t rsrc_id) {
    drv.HandleEpTransferStartedEvent(ep_num, rsrc_id);
  }

  static void TriggerEpTransferEnded(Dwc3& drv, uint8_t ep_num) {
    drv.HandleEpTransferEndedEvent(ep_num);
  }

  static void TriggerConnectionDone(Dwc3& drv) { drv.HandleConnectionDoneEvent(); }

 protected:
  PlatformExtension* GetPlatformExtension(Dwc3& drv) { return drv.platform_extension_.get(); }

 public:
  void TriggerConnectionPlugIn(fuchsia_hardware_usb_descriptor::UsbSpeed speed) {
    namespace fdescriptor = fuchsia_hardware_usb_descriptor;
    dut_.RunInEnvironmentTypeContext([&](Environment& env) {
      auto& dsts_reg = env.reg_region()[DSTS::Get().addr()];
      dsts_reg.SetReadCallback([speed]() -> uint32_t {
        uint32_t speed_val = 0;
        if (speed == fdescriptor::UsbSpeed::kSuper) {
          speed_val = DSTS::CONNECTSPD_SUPER;
        }
        return DSTS::Get().FromValue(0).set_CONNECTSPD(speed_val).reg_value();
      });
      env.usb_phy().TriggerConnection(true);
    });

    // Deterministic synchronization: Wait for the driver dispatcher to process the event.
    dut_.runtime().RunUntil(
        [&]() { return dut_.RunInDriverContext<bool>([](Dwc3& drv) { return drv.power_on(); }); });
  }

  void SetUp() override {
    stuck_reset_test_ = false;
    dut_.RunInEnvironmentTypeContext([&](Environment& env) {
      auto& hwparams3 = env.reg_region()[GHWPARAMS3::Get().addr()];
      auto& dctl_reg = env.reg_region()[DCTL::Get().addr()];
      auto& gsnpsid_reg = env.reg_region()[GSNPSID::Get().addr()];

      hwparams3.SetReadCallback([this]() -> uint32_t { return Read_GHWPARAMS3(); });
      dctl_reg.SetReadCallback([this]() -> uint32_t { return Read_DCTL(); });
      dctl_reg.SetWriteCallback(
          [this](uint64_t val) { return Write_DCTL(static_cast<uint32_t>(val)); });
      gsnpsid_reg.SetReadCallback([this]() -> uint32_t { return Read_GSNPSID(); });
    });

    if (manage_lifetime) {
      ASSERT_TRUE(dut_.StartDriverWithCustomStartArgs([](fdf::DriverStartArgs& args) {
                        dwc3_config::Config cfg;
                        cfg.enable_suspend() = false;
                        cfg.bypass_platform_extension() = true;
                        args.config(cfg.ToVmo());
                      })
                      .is_ok());
      ASSERT_EQ(ZX_OK, WaitForPhy());
    }
  }

  void TearDown() override {
    stuck_reset_test_ = false;

    dut_.runtime().RunUntilIdle();
    if (manage_lifetime) {
      EXPECT_EQ(ZX_OK, WaitForPhy());
      EXPECT_EQ(ZX_OK, dut_.StopDriver().status_value());
    }

    // Explicitly reset mock hardware state and sync the environment dispatcher.
    // This fully destroys the mock VMOs and guarantees no leaked state
    // across parallel test runs.
    dut_.RunInEnvironmentTypeContext([](Environment& env) { env.Reset(); });

    dut_.runtime().RunUntilIdle();
  }

 protected:
  // Section 1.2.22 of the DWC3 Programmer's guide
  //
  // DWC_USB31_CACHE_TOTAL_XFER_RESOURCES : 32
  // DWC_USB31_NUM_IN_EPS                 : 16
  // DWC_USB31_NUM_EPS                    : 32
  // DWC_USB31_VENDOR_CTL_INTERFACE       : 0
  // DWC_USB31_HSPHY_DWIDTH               : 2
  // DWC_USB31_HSPHY_INTERFACE            : 1
  // DWC_USB31_SSPHY_INTERFACE            : 2
  uint32_t Read_GHWPARAMS3() { return 0x10420086; }

  uint32_t ver_number_{0x5533160a};  // 1.60a by default

  // Section 1.4.2 of the DWC3 Programmer's guide
  uint32_t Read_DCTL() { return dctl_val_.load(); }
  void Write_DCTL(uint32_t val) {
    constexpr uint32_t kUnwriteableMask =
        (1 << 29) | (1 << 17) | (1 << 16) | (1 << 15) | (1 << 14) | (1 << 13) | (1 << 0);
    ZX_ASSERT(val <= std::numeric_limits<uint32_t>::max());
    uint32_t updated_val = static_cast<uint32_t>(val & ~kUnwriteableMask);

    if (!stuck_reset_test_) {
      updated_val = DCTL::Get().FromValue(updated_val).set_CSFTRST(0).reg_value();
    }
    dctl_val_.store(updated_val);
  }

  // Section 1.2.9 of the DWC3 Programmer's guide
  //
  // core_id = 0x5533
  // version = 1.60a
  uint32_t Read_GSNPSID() { return ver_number_; }

  std::atomic<uint32_t> dctl_val_{DCTL::Get().FromValue(0).set_LPM_NYET_thres(0xF).reg_value()};
  std::atomic<bool> stuck_reset_test_{false};

  fdf_testing::BackgroundDriverTest<Config> dut_;

  // There's an inherent race in the way this test is set up between the three threads: the
  // foreground testing thread, the background driver thread, and the environment thread the fakes
  // are running on. Driving the driver's dispatcher to an idle state and then tearing down the test
  // will race with the environment's dispatcher execution of the Watch handler. If the environment
  // dispatcher is torn down before the side effects of the Watch handler execute, ~FakeUsbPhy()
  // will sometimes fail. To resolve this race, the foreground testing thread needs to be
  // synchronized against the environment thread and wait for the fakes to catch up.
  zx_status_t WaitForPhy() {
    libsync::Completion* comp{nullptr};
    dut_.RunInEnvironmentTypeContext([&](Environment& env) { comp = env.usb_phy().completion(); });
    return comp->Wait(zx::min(1));
  }
};

using ManagedTestFixture = TestFixture<true>;
using UnmanagedTestFixture = TestFixture<false>;

}  // namespace dwc3

#endif  // SRC_DEVICES_USB_DRIVERS_DWC3_DWC3_TEST_FIXTURE_H_
