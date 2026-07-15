// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_USB_DRIVERS_DWC3_DWC3_TEST_FIXTURE_H_
#define SRC_DEVICES_USB_DRIVERS_DWC3_DWC3_TEST_FIXTURE_H_

#include <fidl/fuchsia.hardware.clock/cpp/test_base.h>
#include <fidl/fuchsia.hardware.interconnect/cpp/fidl.h>
#include <fidl/fuchsia.hardware.platform.device/cpp/fidl.h>
#include <fidl/fuchsia.hardware.reset/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.dci/cpp/wire.h>
#include <fidl/fuchsia.hardware.usb.endpoint/cpp/wire.h>
#include <fidl/fuchsia.hardware.usb.phy/cpp/fidl.h>
#include <fidl/fuchsia.hardware.vreg/cpp/test_base.h>
#include <lib/driver/fake-clock/cpp/fake-clock.h>
#include <lib/driver/fake-reset/cpp/fake-reset.h>
#include <lib/driver/fake-vreg/cpp/fake-vreg.h>
#include <lib/fit/defer.h>
#include <lib/sync/cpp/completion.h>

#include <algorithm>
#include <optional>

#include <fake-mmio-reg/fake-mmio-reg.h>
#include <gtest/gtest.h>

#include "lib/driver/fake-platform-device/cpp/fake-pdev.h"
#include "lib/driver/testing/cpp/driver_test.h"
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

class Ep0TestHelper {
 public:
  using State = Dwc3::Ep0::State;
  static void HandleEp0TransferCompleteEvent(Dwc3& drv, uint8_t ep_num) {
    if (!drv.ep0_.shared_fifo.IsEmpty()) {
      dwc3_trb_t* trb = drv.ep0_.shared_fifo.current_read();
      trb->control &= ~TRB_HWO;
    }
    drv.HandleEp0TransferCompleteEvent(ep_num);
  }

  static void SimulateGhostTransferCompleteEvent(Dwc3& drv, uint8_t ep_num) {
    drv.HandleEp0TransferCompleteEvent(ep_num);
  }
  static void HandleEp0TransferNotReadyEvent(Dwc3& drv, uint8_t ep_num, uint32_t stage) {
    drv.HandleEp0TransferNotReadyEvent(ep_num, stage);
  }
  static void SetEp0State(Dwc3& drv, Dwc3::Ep0::State state) { drv.ep0_.state = state; }
  static void Ep0Reset(Dwc3& drv) { drv.Ep0Reset(); }
  static void Ep0QueueSetup(Dwc3& drv) { drv.Ep0QueueSetup(); }
  static Dwc3::Ep0::State GetEp0State(Dwc3& drv) { return drv.ep0_.state; }
  static fuchsia_hardware_usb_policy::wire::DeviceState GetDeviceState(Dwc3& drv) {
    return drv.device_state_;
  }
  static void SetPowerOn(Dwc3& drv, bool power_on) { drv.power_on_ = power_on; }
  static void SetDriverStopping(Dwc3& drv, bool driver_stopping) {
    // Stubbed out: driver_stopping_ is not in production yet.
    (void)drv;
    (void)driver_stopping;
  }
  static void SetDeviceState(Dwc3& drv, fuchsia_hardware_usb_policy::DeviceState state) {
    drv.SetDeviceState(state);
  }
  static PlatformExtension* GetPlatformExtension(Dwc3& drv) {
    return drv.platform_extension_.get();
  }
  static void HandleEvent(Dwc3& drv, uint32_t event) { drv.HandleEvent(event); }
  static void CmdEpSetConfig(Dwc3& drv, Dwc3::Endpoint& ep, bool modify) {
    drv.CmdEpSetConfig(ep, modify);
  }
  static void CmdEpStartTransfer(Dwc3& drv, Dwc3::Endpoint& ep, zx_paddr_t trb_phys) {
    drv.CmdEpStartTransfer(ep, trb_phys);
  }
  static void CmdEpEndTransfer(Dwc3& drv, Dwc3::Endpoint& ep) { drv.CmdEpEndTransfer(ep); }
  static void ForceCmdEpEndTransfer(Dwc3& drv, uint8_t ep_num) {
    // Stubbed out: ForceCmdEpEndTransfer is not in production yet.
    (void)drv;
    (void)ep_num;
  }
  static void CmdEpSetStall(Dwc3& drv, Dwc3::Endpoint& ep) { drv.CmdEpSetStall(ep); }
  static void CmdEpClearStall(Dwc3& drv, Dwc3::Endpoint& ep) { drv.CmdEpClearStall(ep); }
  static void CmdStartNewConfig(Dwc3& drv, Dwc3::Endpoint& ep, uint32_t rsrc_id_base) {
    drv.CmdStartNewConfig(ep, rsrc_id_base);
  }
  static void CmdEpTransferConfig(Dwc3& drv, Dwc3::Endpoint& ep) { drv.CmdEpTransferConfig(ep); }
  static zx::result<> InitFifo(Dwc3& drv, uint8_t ep_num) {
    auto* uep = drv.get_user_endpoint(ep_num);
    if (!uep)
      return zx::error(ZX_ERR_NOT_FOUND);
    return uep->fifo.Init(drv.bti_, true);
  }
  static void HandleEpTransferCompleteEvent(Dwc3& drv, uint8_t ep_num) {
    drv.HandleEpTransferCompleteEvent(ep_num);
  }
  static void HandleEpTransferStartedEvent(Dwc3& drv, uint8_t ep_num, uint32_t rsrc_id) {
    drv.HandleEpTransferStartedEvent(ep_num, rsrc_id);
  }
  static void EpReset(Dwc3& drv, Dwc3::Endpoint& ep) { drv.EpReset(ep); }
  static zx_status_t ResetHw(Dwc3& drv, bool is_resume) {
    // Stubbed out: ResetHw in current production takes 0 arguments.
    (void)is_resume;
    return drv.ResetHw();
  }
  static void SetEnableSuspend(Dwc3& drv, bool enable) {
    if (drv.config_.has_value()) {
      drv.config_->enable_suspend() = enable;
    }
  }
  static bool GetPowerOn(Dwc3& drv) { return drv.power_on_; }
  static zx_status_t EpSetStall(Dwc3& drv, Dwc3::Endpoint& ep, bool stall) {
    return drv.EpSetStall(ep, stall);
  }
  static void SetDeviceAddress(Dwc3& drv, uint32_t address) { drv.SetDeviceAddress(address); }
  static bool IsFifoEmpty(Dwc3& drv) { return drv.ep0_.shared_fifo.IsEmpty(); }
  static void SetXferInProgress(Dwc3& drv, uint8_t ep_num, bool in_progress) {
    auto state = in_progress ? dwc3::Dwc3::Endpoint::TransferState::kActiveSingle
                             : dwc3::Dwc3::Endpoint::TransferState::kIdle;
    if (ep_num < 2) {
      ((ep_num == 0) ? drv.ep0_.out : drv.ep0_.in).transfer_state = state;
    } else {
      auto* uep = drv.get_user_endpoint(ep_num);
      if (uep) {
        uep->ep.transfer_state = state;
      }
    }
  }
  static void SetEpRsrcId(Dwc3& drv, uint8_t ep_num, uint32_t rsrc_id) {
    if (ep_num < 2) {
      ((ep_num == 0) ? drv.ep0_.out : drv.ep0_.in).rsrc_id = rsrc_id;
    } else {
      auto* uep = drv.get_user_endpoint(ep_num);
      if (uep) {
        uep->ep.rsrc_id = rsrc_id;
      }
    }
  }
  static void SetEpPendingCancel(Dwc3& drv, uint8_t ep_num, bool pending) {
    // Stubbed out: pending_cancel is not in production yet.
    (void)drv;
    (void)ep_num;
    (void)pending;
  }

  static bool GetEpPendingCancel(Dwc3& drv, uint8_t ep_num) {
    // Stubbed out: pending_cancel is not in production yet.
    (void)drv;
    (void)ep_num;
    return false;
  }
  static Dwc3::UserEndpoint* GetUserEndpoint(Dwc3& drv, uint8_t ep_num) {
    return drv.get_user_endpoint(ep_num);
  }
  static size_t GetQueuedReqsSize(Dwc3& drv, uint8_t ep_num) {
    auto* uep = drv.get_user_endpoint(ep_num);
    if (uep && uep->server.has_value()) {
      return uep->server->queued_reqs.size();
    }
    return 0;
  }

  static void EpSetConfig(Dwc3& drv, Dwc3::Endpoint& ep, bool enable) {
    drv.EpSetConfig(ep, enable);
  }
  static void EpEnable(Dwc3& drv, Dwc3::Endpoint& ep, bool enable) { drv.EpEnable(ep, enable); }
  static async_dispatcher_t* GetDispatcher(Dwc3& drv) { return drv.dispatcher(); }
  static void SetControllerStarted(Dwc3& drv, bool started) { drv.controller_started_ = started; }
  static void HandleIrq(Dwc3& drv, async_dispatcher_t* dispatcher, async::IrqBase* irq,
                        zx_status_t status, const zx_packet_interrupt_t* interrupt) {
    drv.HandleIrq(dispatcher, irq, status, interrupt);
  }
  static void SetCurSetup(Dwc3& drv, const fuchsia_hardware_usb_descriptor::wire::UsbSetup& setup) {
    drv.ep0_.cur_setup = setup;
  }
  static void* GetEp0BufferVirt(Dwc3& drv) { return drv.ep0_.buffer->virt(); }
  static size_t GetEp0BufferSize(Dwc3& drv) { return drv.ep0_.buffer->size(); }
  static bool IsEp0OutStalled(Dwc3& drv) { return drv.ep0_.out.stalled; }
  static void SetEp0OutEnabled(Dwc3& drv, bool enabled) { drv.ep0_.out.enabled = enabled; }
  static void SetEp0InEnabled(Dwc3& drv, bool enabled) { drv.ep0_.in.enabled = enabled; }
  static void PushTrbToSharedFifo(Dwc3& drv, const dwc3_trb_t& trb) {
    dwc3_trb_t* ptr = drv.ep0_.shared_fifo.AdvanceWrite();
    *ptr = trb;
    drv.ep0_.shared_fifo.Write(ptr);
  }
  static void HandleResetEvent(Dwc3& drv) { drv.HandleResetEvent(); }

  static void ClearSharedFifo(Dwc3& drv) { drv.ep0_.shared_fifo.Clear(); }
  static bool IsSharedFifoEmpty(Dwc3& drv) { return drv.ep0_.shared_fifo.IsEmpty(); }

  static void BindDciInterface(Dwc3& drv,
                               fidl::ClientEnd<fuchsia_hardware_usb_dci::UsbDciInterface> client) {
    drv.dci_intf_.Bind(std::move(client), drv.dispatcher());
  }

  // Simulation constructs for EP0
  static void SimulateSetupReceived(Dwc3& drv,
                                    const fuchsia_hardware_usb_descriptor::wire::UsbSetup& setup) {
    void* buf = drv.ep0_.buffer->virt();
    std::memcpy(buf, &setup, sizeof(setup));

    ZX_ASSERT_MSG(!drv.ep0_.shared_fifo.IsEmpty(), "SimulateSetupReceived called on empty FIFO!");
    dwc3_trb_t* trb = drv.ep0_.shared_fifo.current_read();
    trb->control &= ~TRB_HWO;

    drv.HandleEp0TransferCompleteEvent(0);  // EP0 OUT
  }

  static void SimulateDataOutPhase(Dwc3& drv, uint32_t received_len) {
    ZX_ASSERT_MSG(!drv.ep0_.shared_fifo.IsEmpty(), "SimulateDataOutPhase called on empty FIFO!");
    dwc3_trb_t* trb = drv.ep0_.shared_fifo.current_read();
    trb->status = TRB_BUFSIZ(static_cast<uint32_t>(drv.ep0_.buffer->size() - received_len));

    trb->control &= ~TRB_HWO;
    drv.HandleEp0TransferCompleteEvent(0);  // EP0 OUT
  }
};

class FakeUsbPhy : public fidl::Server<fphy::UsbPhy>, public fidl::Server<fphy::ConnectionWatcher> {
 public:
  ~FakeUsbPhy() override {
    if (expect_connection_status_observer_call_) {
      EXPECT_TRUE(connection_status_observer_called_);
      EXPECT_TRUE(completer_.has_value());
    }
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

  void set_connection_status_observer_called(bool set) { connection_status_observer_called_ = set; }

  void set_initial_connected(bool set) { initial_connected_ = set; }
  void set_expect_connection_status_observer_call(bool expect) {
    expect_connection_status_observer_call_ = expect;
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

  void TriggerDisconnect() { TriggerConnection(false); }

  libsync::Completion* completion() { return &completion_; }

 private:
  void ConnectStatusChanged(ConnectStatusChangedRequest& request,
                            ConnectStatusChangedCompleter::Sync& completer) override {
    completer.Reply(zx::ok());
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<fphy::UsbPhy> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {
    fdf::error("Unknown method {}", metadata.method_ordinal);
  }

  void WatchConnectStatusChanged(WatchConnectStatusChangedRequest& request,
                                 WatchConnectStatusChangedCompleter::Sync& completer) override {
    if (!connection_status_observer_called_) {
      fuchsia_hardware_usb_phy::ConnectionWatcherWatchConnectStatusChangedResponse response{{
          .connected = initial_connected_,
          .wake_lease = {},
      }};
      completer.Reply(zx::ok(std::move(response)));

      connection_status_observer_called_ = true;
      return;
    }

    ASSERT_FALSE(completer_.has_value());
    completer_.emplace(completer.ToAsync());
    completion_.Signal();
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<fphy::ConnectionWatcher> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {
    fdf::error("Unknown method {}", metadata.method_ordinal);
  }

  fidl::ServerBindingGroup<fphy::UsbPhy> bindings_;
  fidl::ServerBindingGroup<fphy::ConnectionWatcher> watcher_bindings_;

  bool connection_status_observer_called_ = false;
  bool initial_connected_ = false;
  bool expect_connection_status_observer_call_ = true;
  std::optional<WatchConnectStatusChangedCompleter::Async> completer_;
  libsync::Completion completion_;  // Signaled when the above completer_ is saved.
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

    result =
        directory.AddService<fclock::Service>(clock_xo_.CreateInstanceHandler(dispatcher), "xo");
    EXPECT_TRUE(result.is_ok());

    result = directory.AddService<fclock::Service>(clock_sleep_.CreateInstanceHandler(dispatcher),
                                                   "sleep-clk");
    EXPECT_TRUE(result.is_ok());

    result = directory.AddService<fclock::Service>(clock_iface_.CreateInstanceHandler(dispatcher),
                                                   "iface-clk");
    EXPECT_TRUE(result.is_ok());

    result = directory.AddService<fclock::Service>(clock_core_.CreateInstanceHandler(dispatcher),
                                                   "core-clk");
    EXPECT_TRUE(result.is_ok());

    result = directory.AddService<fclock::Service>(clock_utmi_.CreateInstanceHandler(dispatcher),
                                                   "utmi-clk");
    EXPECT_TRUE(result.is_ok());

    result = directory.AddService<fclock::Service>(
        clock_bus_aggr_.CreateInstanceHandler(dispatcher), "bus-aggr-clk");
    EXPECT_TRUE(result.is_ok());

    if (serve_platform_mocks_) {
      result = directory.AddService<freset::Service>(reset_.CreateInstanceHandler(), "reset");
      EXPECT_TRUE(result.is_ok());

      result = directory.AddService<fvreg::Service>(vreg_.CreateInstanceHandler(), "regulator");
      EXPECT_TRUE(result.is_ok());
    }

    return zx::ok();
  }

  // Note: Only intended for teardown, does not restore default mock behaviors.
  void Reset() {
    for (size_t i = 0; i < kRegCount; i++) {
      reg_region_[i * kRegSize].SetReadCallback([]() { return 0; });
      reg_region_[i * kRegSize].SetWriteCallback([](uint64_t value) {});
    }
    serve_platform_mocks_ = true;
  }

  ddk_fake::FakeMmioRegRegion& reg_region() { return reg_region_; }

  FakeUsbPhy& usb_phy() { return usb_phy_; }
  const fdf_fake::FakeClock& clock_xo() const { return clock_xo_; }
  const fdf_fake::FakeClock& clock_sleep() const { return clock_sleep_; }
  const fdf_fake::FakeClock& clock_iface() const { return clock_iface_; }
  const fdf_fake::FakeClock& clock_core() const { return clock_core_; }
  const fdf_fake::FakeClock& clock_utmi() const { return clock_utmi_; }
  const fdf_fake::FakeClock& clock_bus_aggr() const { return clock_bus_aggr_; }
  fdf_fake::FakeReset& reset() { return reset_; }
  const fdf_fake::FakeVreg& vreg() const { return vreg_; }

  void set_serve_platform_mocks(bool serve) { serve_platform_mocks_ = serve; }
  static constexpr size_t kRegSize = sizeof(uint32_t);
  static constexpr size_t kMmioRegionSize = 0x10'0000;
  static constexpr size_t kRegCount = kMmioRegionSize / kRegSize;

 private:
  fdf_fake::FakePDev pdev_;
  ddk_fake::FakeMmioRegRegion reg_region_{kRegSize, kRegCount};
  FakePath path_;
  FakeUsbPhy usb_phy_;
  fdf_fake::FakeClock clock_xo_;
  fdf_fake::FakeClock clock_sleep_;
  fdf_fake::FakeClock clock_iface_;
  fdf_fake::FakeClock clock_core_;
  fdf_fake::FakeClock clock_utmi_;
  fdf_fake::FakeClock clock_bus_aggr_;
  fdf_fake::FakeReset reset_;
  fdf_fake::FakeVreg vreg_;
  bool serve_platform_mocks_ = true;
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
    vbus_high_ = false;

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
    vbus_high_ = false;

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
    if (DCTL::Get().FromValue(val).CSFTRST() == 1 && vbus_high_) {
      ADD_FAILURE()
          << "BUG TRIPPED: CSFTRST asserted while physically connected to host! PMIC over-current crowbar spike imminent!";
    }

    constexpr uint32_t kUnwriteableMask =
        (1 << 29) | (1 << 17) | (1 << 16) | (1 << 15) | (1 << 14) | (1 << 13) | (1 << 0);
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
  std::atomic<bool> vbus_high_{false};

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

class TestEndpointEventHandler
    : public fidl::SyncEventHandler<fuchsia_hardware_usb_endpoint::Endpoint> {
 public:
  TestEndpointEventHandler(bool& completed, zx_status_t& status, size_t* length = nullptr)
      : completed_(completed), status_(status), length_(length) {}
  void OnCompletion(
      fidl::Event<fuchsia_hardware_usb_endpoint::Endpoint::OnCompletion>& event) override {
    completed_ = true;
    if (!event.completion().empty()) {
      if (event.completion()[0].status().has_value()) {
        status_ = *event.completion()[0].status();
      }
      if (length_ && event.completion()[0].transfer_size().has_value()) {
        *length_ = *event.completion()[0].transfer_size();
      }
    }
  }

 private:
  bool& completed_;
  zx_status_t& status_;
  size_t* length_;
};

class FakeUsbDciInterface : public fidl::WireServer<fuchsia_hardware_usb_dci::UsbDciInterface> {
 public:
  using ControlCallback = std::function<void(fuchsia_hardware_usb_descriptor::wire::UsbSetup,
                                             cpp20::span<const uint8_t>)>;
  using SetConnectedCallback = std::function<void(bool connected)>;
  using SetSpeedCallback =
      std::function<void(fuchsia_hardware_usb_descriptor::wire::UsbSpeed speed)>;

  void SetControlCallback(ControlCallback cb) { control_cb_ = std::move(cb); }
  void SetControlStatus(zx_status_t status) { control_status_ = status; }
  void SetReadData(std::vector<uint8_t> data) { read_data_ = std::move(data); }
  void SetSetConnectedCallback(SetConnectedCallback cb) { set_connected_cb_ = std::move(cb); }
  void SetSetSpeedCallback(SetSpeedCallback cb) { set_speed_cb_ = std::move(cb); }

  void Control(ControlRequestView request, ControlCompleter::Sync& completer) override {
    control_called_ = true;

    if (control_cb_) {
      control_cb_(request->setup,
                  cpp20::span<const uint8_t>(request->write.data(), request->write.size()));
    }

    if (control_status_ != ZX_OK) {
      completer.Reply(zx::error(control_status_));
      return;
    }

    if ((request->setup.bm_request_type & USB_DIR_MASK) == USB_DIR_IN) {
      uint8_t* data = read_data_.data();
      size_t size = read_data_.size();
      response_.read = fidl::VectorView<uint8_t>::FromExternal(data, size);
    }
    completer.Reply(zx::ok(&response_));
  }

  void SetConnected(SetConnectedRequestView request,
                    SetConnectedCompleter::Sync& completer) override {
    set_connected_called_ = true;
    if (set_connected_cb_) {
      set_connected_cb_(request->is_connected);
    }
    completer.Reply(zx::ok());
  }

  void SetSpeed(SetSpeedRequestView request, SetSpeedCompleter::Sync& completer) override {
    set_speed_called_ = true;
    if (set_speed_cb_) {
      set_speed_cb_(request->speed);
    }
    completer.Reply(zx::ok());
  }

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_hardware_usb_dci::UsbDciInterface> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override {}

  bool control_called() const { return control_called_; }
  bool set_connected_called() const { return set_connected_called_; }
  bool set_speed_called() const { return set_speed_called_; }

 private:
  bool control_called_ = false;
  bool set_connected_called_ = false;
  bool set_speed_called_ = false;
  ControlCallback control_cb_;
  SetConnectedCallback set_connected_cb_;
  SetSpeedCallback set_speed_cb_;
  fuchsia_hardware_usb_dci::wire::UsbDciInterfaceControlResponse response_;
  zx_status_t control_status_ = ZX_OK;
  std::vector<uint8_t> read_data_;
};

class UnmanagedTestFixture : public TestFixture<false> {
 public:
  using TestFixture<false>::TestFixture;

  fidl::SyncClient<fuchsia_hardware_usb_dci::UsbDci> ConnectController() {
    auto client_end = dut_.Connect<fuchsia_hardware_usb_dci::UsbDciService::Device>();
    ZX_ASSERT(client_end.is_ok());
    return fidl::SyncClient<fuchsia_hardware_usb_dci::UsbDci>(std::move(client_end.value()));
  }

  void SetUpAndPowerOnDriver() {
    dut_.RunInEnvironmentTypeContext([](Environment& env) {
      env.usb_phy().set_initial_connected(true);
      env.reg_region()[DEPCMD::Get(0).addr()].SetReadCallback([]() -> uint32_t { return 0; });
      env.reg_region()[DEPCMD::Get(1).addr()].SetReadCallback([]() -> uint32_t { return 0; });
    });

    ASSERT_TRUE(dut_.StartDriverWithCustomStartArgs([](fdf::DriverStartArgs& args) {
                      dwc3_config::Config cfg;
                      cfg.enable_suspend() = false;
                      args.config(cfg.ToVmo());
                    })
                    .is_ok());

    // Wait for driver to be powered on after potential restart to stabilize dispatcher
    EXPECT_TRUE(dut_.runtime().RunWithTimeoutOrUntil(
        [&]() {
          bool powered = false;
          dut_.RunInDriverContext([&](Dwc3& drv) {
            powered = (Ep0TestHelper::GetDeviceState(drv) ==
                       fuchsia_hardware_usb_policy::wire::DeviceState::kPowered);
          });
          return powered;
        },
        zx::sec(10)));
  }

  void TearDownAndPowerOffDriver() {
    dut_.RunInDriverContext([&](Dwc3& drv) {
      Ep0TestHelper::SetEpRsrcId(drv, 0, 2);
      Ep0TestHelper::SetEpRsrcId(drv, 1, 2);
    });
    EXPECT_EQ(ZX_OK, dut_.StopDriver().status_value());
  }

  void BindDciInterfaceWithoutServer() {
    dut_.RunInDriverContext([&](Dwc3& drv) {
      auto [client_end, server_end] =
          fidl::Endpoints<fuchsia_hardware_usb_dci::UsbDciInterface>::Create();
      Ep0TestHelper::BindDciInterface(drv, std::move(client_end));
    });
  }

  template <typename Server, typename UnbindCallback = std::nullptr_t>
  auto BindDciInterface(Server* server, UnbindCallback&& unbind_cb = nullptr) {
    std::optional<fidl::ServerBindingRef<fuchsia_hardware_usb_dci::UsbDciInterface>> binding;
    dut_.RunInDriverContext([&](Dwc3& drv) {
      auto [client_end, server_end] =
          fidl::Endpoints<fuchsia_hardware_usb_dci::UsbDciInterface>::Create();
      if constexpr (std::is_same_v<std::decay_t<UnbindCallback>, std::nullptr_t>) {
        binding = fidl::BindServer(fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                                   std::move(server_end), server);
      } else {
        binding = fidl::BindServer(fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                                   std::move(server_end), server,
                                   std::forward<UnbindCallback>(unbind_cb));
      }
      Ep0TestHelper::BindDciInterface(drv, std::move(client_end));
    });
    return binding;
  }

  static fuchsia_hardware_usb_descriptor::wire::UsbSetup MakeSetupPacket(uint8_t bm_request_type,
                                                                         uint8_t b_request,
                                                                         uint16_t w_value,
                                                                         uint16_t w_index,
                                                                         uint16_t w_length) {
    fuchsia_hardware_usb_descriptor::wire::UsbSetup setup;
    setup.bm_request_type = bm_request_type;
    setup.b_request = b_request;
    setup.w_value = w_value;
    setup.w_index = w_index;
    setup.w_length = w_length;
    return setup;
  }

  static fuchsia_hardware_usb_descriptor::wire::UsbSetup MakeGetDescriptorSetup(
      uint16_t length = 18) {
    fuchsia_hardware_usb_descriptor::wire::UsbSetup setup;
    setup.bm_request_type = USB_DIR_IN | USB_TYPE_STANDARD | USB_RECIP_DEVICE;
    setup.b_request = USB_REQ_GET_DESCRIPTOR;
    setup.w_value = static_cast<uint16_t>(USB_DT_DEVICE << 8);
    setup.w_index = 0;
    setup.w_length = length;
    return setup;
  }
};
}  // namespace dwc3

#endif  // SRC_DEVICES_USB_DRIVERS_DWC3_DWC3_TEST_FIXTURE_H_
