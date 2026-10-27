// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "dw-i2c.h"

#include <lib/driver/fake-clock/cpp/fake-clock.h>
#include <lib/driver/fake-platform-device/cpp/fake-pdev.h>
#include <lib/driver/fake-powerdomain/cpp/fake-powerdomain.h>
#include <lib/driver/fake-reset/cpp/fake-reset.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/fit/defer.h>
#include <lib/fit/function.h>
#include <lib/zx/clock.h>

#include <array>
#include <atomic>
#include <condition_variable>
#include <deque>
#include <mutex>
#include <thread>
#include <vector>

#include <fake-mmio-reg/fake-mmio-reg.h>
#include <gtest/gtest.h>

#include "src/lib/testing/predicates/status.h"

namespace dw_i2c {

class TestDwI2c : public DwI2c {
 public:
  using DwI2c::kTransferTimeout;
  using DwI2c::set_transfer_timeout;
  using DwI2c::transfer_timeout;

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
  static constexpr uint32_t kTxFifoDepth = 8;
  static constexpr uint32_t kRxFifoDepth = 8;

  struct RecordedCommand {
    uint16_t target_addr;
    uint8_t data;
    bool is_read;
    bool start;
    bool stop;
  };

  FakeDwI2cController() : mmio_(sizeof(uint32_t), kRegCount) {
    for (size_t i = 0; i < kRegCount; ++i) {
      size_t offset = i * sizeof(uint32_t);
      mmio_[offset].SetReadCallback([this, i]() {
        std::lock_guard lock(lock_);
        return regs_[i];
      });
      mmio_[offset].SetWriteCallback([this, i](uint64_t val) {
        std::lock_guard lock(lock_);
        regs_[i] = static_cast<uint32_t>(val);
      });
    }

    // DataCommandReg (0x10): writing queues TX/read commands; reading pops from RX FIFO.
    mmio_[0x10].SetReadCallback([this]() {
      std::lock_guard lock(lock_);
      uint8_t byte = 0;
      if (!rx_fifo_.empty()) {
        byte = rx_fifo_.front();
        rx_fifo_.pop_front();
      }
      UpdateRxFifoStateLocked();
      return byte;
    });
    mmio_[0x10].SetWriteCallback([this](uint64_t val) {
      fit::function<void()> on_tx_fifo_full;
      fit::function<void()> on_stop;
      {
        std::lock_guard lock(lock_);
        auto cmd = DataCommandReg::Get().FromValue(static_cast<uint32_t>(val));
        auto tar = TargetAddressReg::Get().FromValue(regs_[0x04 / sizeof(uint32_t)]);
        tx_commands_.push_back(RecordedCommand{
            .target_addr = static_cast<uint16_t>(tar.target_address()),
            .data = static_cast<uint8_t>(cmd.data()),
            .is_read = cmd.command() != 0,
            .start = cmd.start() != 0,
            .stop = cmd.stop() != 0,
        });
        if (tx_fifo_level_ < kTxFifoDepth) {
          ++tx_fifo_level_;
        }
        UpdateTxFifoStateLocked();
        if (cmd.command() != 0) {
          uint8_t rx_byte = 0;
          if (!canned_rx_bytes_.empty()) {
            rx_byte = canned_rx_bytes_.front();
            canned_rx_bytes_.pop_front();
          }
          pending_wire_rx_.push_back(rx_byte);
        }
        if (tx_fifo_level_ == kTxFifoDepth && on_tx_fifo_full_) {
          on_tx_fifo_full = on_tx_fifo_full_.share();
        }
        if (cmd.stop() != 0 && on_stop_command_) {
          on_stop = on_stop_command_.share();
        }
      }
      if (on_tx_fifo_full) {
        on_tx_fifo_full();
      }
      if (on_stop) {
        on_stop();
      }
    });

    // InterruptStatusReg (0x2c) = RawInterruptStatusReg (0x34) & InterruptMaskReg (0x30).
    mmio_[0x2c].SetReadCallback([this]() {
      fit::function<void()> on_empty;
      uint32_t status = 0;
      {
        std::lock_guard lock(lock_);
        status = regs_[0x34 / sizeof(uint32_t)] & regs_[0x30 / sizeof(uint32_t)];
        if (status == 0 && on_empty_intr_stat_) {
          on_empty = on_empty_intr_stat_.share();
        }
      }
      if (on_empty) {
        on_empty();
      }
      return status;
    });

    // RxFifoThresholdReg (0x38) and TxFifoThresholdReg (0x3c) update raw RX_FULL / TX_EMPTY bits.
    mmio_[0x38].SetWriteCallback([this](uint64_t val) {
      std::lock_guard lock(lock_);
      regs_[0x38 / sizeof(uint32_t)] = static_cast<uint32_t>(val);
      UpdateRxFifoStateLocked();
    });
    mmio_[0x3c].SetWriteCallback([this](uint64_t val) {
      std::lock_guard lock(lock_);
      regs_[0x3c / sizeof(uint32_t)] = static_cast<uint32_t>(val);
      UpdateTxFifoStateLocked();
    });

    // ClearInterruptReg (0x40) clears latched raw interrupt status bits.
    mmio_[0x40].SetReadCallback([this]() {
      std::lock_guard lock(lock_);
      auto raw = RawInterruptStatusReg::Get().FromValue(regs_[0x34 / sizeof(uint32_t)]);
      raw.set_tx_abrt(0).set_activity(0).set_stop_det(0).set_start_det(0);
      regs_[0x34 / sizeof(uint32_t)] = raw.reg_value();
      return 0u;
    });
    mmio_[0x54].SetReadCallback([this]() {
      std::lock_guard lock(lock_);
      auto raw = RawInterruptStatusReg::Get().FromValue(regs_[0x34 / sizeof(uint32_t)]);
      raw.set_tx_abrt(0);
      regs_[0x34 / sizeof(uint32_t)] = raw.reg_value();
      return 0u;
    });
    mmio_[0x5c].SetReadCallback([this]() {
      std::lock_guard lock(lock_);
      auto raw = RawInterruptStatusReg::Get().FromValue(regs_[0x34 / sizeof(uint32_t)]);
      raw.set_activity(0);
      regs_[0x34 / sizeof(uint32_t)] = raw.reg_value();
      return 0u;
    });
    mmio_[0x60].SetReadCallback([this]() {
      fit::function<void()> on_clear_stop_det;
      {
        std::lock_guard lock(lock_);
        auto raw = RawInterruptStatusReg::Get().FromValue(regs_[0x34 / sizeof(uint32_t)]);
        raw.set_stop_det(0);
        regs_[0x34 / sizeof(uint32_t)] = raw.reg_value();
        if (on_clear_stop_det_) {
          on_clear_stop_det = on_clear_stop_det_.share();
        }
      }
      if (on_clear_stop_det) {
        on_clear_stop_det();
      }
      return 0u;
    });
    mmio_[0x64].SetReadCallback([this]() {
      std::lock_guard lock(lock_);
      auto raw = RawInterruptStatusReg::Get().FromValue(regs_[0x34 / sizeof(uint32_t)]);
      raw.set_start_det(0);
      regs_[0x34 / sizeof(uint32_t)] = raw.reg_value();
      return 0u;
    });

    // EnableReg (0x6c) mirrors enable state into EnableStatusReg (0x9c) and flushes FIFOs when
    // disabled.
    mmio_[0x6c].SetWriteCallback([this](uint64_t val) {
      std::lock_guard lock(lock_);
      const uint32_t reg_val = static_cast<uint32_t>(val);
      regs_[0x6c / sizeof(uint32_t)] = reg_val;
      regs_[0x9c / sizeof(uint32_t)] = reg_val & 0x1u;
      if ((reg_val & 0x1u) == 0) {
        tx_fifo_level_ = 0;
        pending_wire_rx_.clear();
        rx_fifo_.clear();
        UpdateRxFifoStateLocked();
      }
      UpdateTxFifoStateLocked();
    });

    // Mock CompParam1Reg (0xf4) to return depths (8 for tx and rx).
    mmio_[0xf4].SetReadCallback([]() { return (kTxFifoDepth << 16) | (kRxFifoDepth << 8); });
    // Mock CompTypeReg (0xfc) to return kDwCompTypeNum (0x44570140) when powered.
    mmio_[0xfc].SetReadCallback([this]() {
      std::lock_guard lock(lock_);
      return powered_ ? DwI2c::kDwCompTypeNum : 0u;
    });
  }

  fdf::MmioBuffer GetMmioBuffer() { return mmio_.GetMmioBuffer(); }

  uint32_t ReadReg(size_t offset) const {
    std::lock_guard lock(lock_);
    return regs_[offset / sizeof(uint32_t)];
  }

  void ResetWritableRegs() {
    std::lock_guard lock(lock_);
    regs_.fill(0);
    tx_fifo_level_ = 0;
    pending_wire_rx_.clear();
    rx_fifo_.clear();
  }

  void set_powered(bool powered) {
    std::lock_guard lock(lock_);
    powered_ = powered;
  }

  void set_on_tx_fifo_full(fit::function<void()> callback) {
    std::lock_guard lock(lock_);
    on_tx_fifo_full_ = std::move(callback);
  }

  void set_on_stop_command(fit::function<void()> callback) {
    std::lock_guard lock(lock_);
    on_stop_command_ = std::move(callback);
  }

  void set_on_empty_intr_stat(fit::function<void()> callback) {
    std::lock_guard lock(lock_);
    on_empty_intr_stat_ = std::move(callback);
  }

  void set_on_clear_stop_det(fit::function<void()> callback) {
    std::lock_guard lock(lock_);
    on_clear_stop_det_ = std::move(callback);
  }

  void QueueCannedRxBytes(std::initializer_list<uint8_t> bytes) {
    std::lock_guard lock(lock_);
    for (uint8_t b : bytes) {
      canned_rx_bytes_.push_back(b);
    }
  }

  // Simulates draining queued TX FIFO commands onto the wire mid-frame. Returns true if an
  // unmasked interrupt is now asserted.
  bool DrainTxFifoToWire() {
    std::lock_guard lock(lock_);
    tx_fifo_level_ = 0;
    UpdateTxFifoStateLocked();
    return IsLevelIrqAssertedLocked();
  }

  // Simulates completion of the queued I2C transfer: drains the TX FIFO, moves pending RX bytes
  // into the RX FIFO, and latches STOP_DET. Returns true if an unmasked interrupt is now asserted.
  bool CompleteWireTransfer() {
    std::lock_guard lock(lock_);
    tx_fifo_level_ = 0;
    UpdateTxFifoStateLocked();
    while (!pending_wire_rx_.empty()) {
      rx_fifo_.push_back(pending_wire_rx_.front());
      pending_wire_rx_.pop_front();
    }
    UpdateRxFifoStateLocked();
    auto raw = RawInterruptStatusReg::Get().FromValue(regs_[0x34 / sizeof(uint32_t)]);
    raw.set_stop_det(1);
    regs_[0x34 / sizeof(uint32_t)] = raw.reg_value();
    return IsLevelIrqAssertedLocked();
  }

  // Simulates a bus abort (e.g. NACK) latching TX_ABRT in RawInterruptStatusReg and TxAbrtSource.
  bool FailWireTransferWithAbort(uint32_t abort_source = 0x1u) {
    std::lock_guard lock(lock_);
    regs_[0x80 / sizeof(uint32_t)] = abort_source;
    auto raw = RawInterruptStatusReg::Get().FromValue(regs_[0x34 / sizeof(uint32_t)]);
    raw.set_tx_abrt(1);
    regs_[0x34 / sizeof(uint32_t)] = raw.reg_value();
    return IsLevelIrqAssertedLocked();
  }

  bool IsLevelIrqAsserted() const {
    std::lock_guard lock(lock_);
    return IsLevelIrqAssertedLocked();
  }

  std::vector<RecordedCommand> TakeTxCommands() {
    std::lock_guard lock(lock_);
    std::vector<RecordedCommand> cmds = std::move(tx_commands_);
    tx_commands_.clear();
    return cmds;
  }

 private:
  bool IsLevelIrqAssertedLocked() const {
    return (regs_[0x34 / sizeof(uint32_t)] & regs_[0x30 / sizeof(uint32_t)]) != 0;
  }

  void UpdateTxFifoStateLocked() {
    regs_[0x74 / sizeof(uint32_t)] = tx_fifo_level_;
    const bool enabled = (regs_[0x6c / sizeof(uint32_t)] & 0x1u) != 0;
    const uint32_t tx_tl = regs_[0x3c / sizeof(uint32_t)] & 0xffu;
    auto raw = RawInterruptStatusReg::Get().FromValue(regs_[0x34 / sizeof(uint32_t)]);
    raw.set_tx_empty(enabled && tx_fifo_level_ <= tx_tl ? 1 : 0);
    regs_[0x34 / sizeof(uint32_t)] = raw.reg_value();
  }

  void UpdateRxFifoStateLocked() {
    regs_[0x78 / sizeof(uint32_t)] = static_cast<uint32_t>(rx_fifo_.size());
    const uint32_t rx_tl = regs_[0x38 / sizeof(uint32_t)] & 0xffu;
    auto raw = RawInterruptStatusReg::Get().FromValue(regs_[0x34 / sizeof(uint32_t)]);
    raw.set_rx_full(!rx_fifo_.empty() && rx_fifo_.size() > rx_tl ? 1 : 0);
    regs_[0x34 / sizeof(uint32_t)] = raw.reg_value();
  }

  mutable std::mutex lock_;
  bool powered_ = true;
  uint32_t tx_fifo_level_ = 0;
  std::array<uint32_t, kRegCount> regs_{};
  std::vector<RecordedCommand> tx_commands_;
  std::deque<uint8_t> canned_rx_bytes_;
  std::deque<uint8_t> pending_wire_rx_;
  std::deque<uint8_t> rx_fifo_;
  fit::function<void()> on_tx_fifo_full_;
  fit::function<void()> on_stop_command_;
  fit::function<void()> on_empty_intr_stat_;
  fit::function<void()> on_clear_stop_det_;
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
  FakeDwI2cController controller;
  TestDwI2c::set_mmio(controller.GetMmioBuffer());

  fdf_testing::BackgroundDriverTest<TestConfig> driver_test;

  zx::interrupt interrupt;
  ASSERT_EQ(ZX_OK, zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt));

  driver_test.RunInEnvironmentTypeContext([&](auto& env) { env.Init(std::move(interrupt)); });

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
  // Simulate CompTypeReg returning 0x0 so HostInit() fails with ZX_ERR_NOT_SUPPORTED during Start.
  FakeDwI2cController controller;
  controller.set_powered(false);
  TestDwI2c::set_mmio(controller.GetMmioBuffer());

  fdf_testing::BackgroundDriverTest<TestConfig> driver_test;

  zx::interrupt interrupt;
  ASSERT_EQ(ZX_OK, zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt));

  driver_test.RunInEnvironmentTypeContext([&](auto& env) { env.Init(std::move(interrupt)); });

  EXPECT_STATUS(driver_test.StartDriver(), ZX_ERR_NOT_SUPPORTED);

  driver_test.RunInEnvironmentTypeContext([](TestEnvironment& env) {
    EXPECT_FALSE(env.powerdomain().is_enabled());
    EXPECT_FALSE(env.clock_bus().enabled());
    EXPECT_FALSE(env.clock_regs().enabled());
  });
}

TEST(DwI2cTest, SuspendAndResume) {
  FakeDwI2cController controller;
  TestDwI2c::set_mmio(controller.GetMmioBuffer());

  fdf_testing::BackgroundDriverTest<TestConfig> driver_test;

  zx::interrupt interrupt;
  ASSERT_EQ(ZX_OK, zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt));

  driver_test.RunInEnvironmentTypeContext([&](auto& env) { env.Init(std::move(interrupt)); });

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
  FakeDwI2cController controller;
  TestDwI2c::set_mmio(controller.GetMmioBuffer());

  fdf_testing::BackgroundDriverTest<TestConfig> driver_test;

  zx::interrupt interrupt;
  ASSERT_EQ(ZX_OK, zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt));

  driver_test.RunInEnvironmentTypeContext([&](auto& env) { env.Init(std::move(interrupt)); });

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

TEST(DwI2cTest, TransactWakesOnLevelIrqWithoutSpuriousRetrigger) {
  FakeDwI2cController controller;
  TestDwI2c::set_mmio(controller.GetMmioBuffer());

  fdf_testing::BackgroundDriverTest<TestConfig> driver_test;

  zx::interrupt interrupt;
  ASSERT_OK(zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt));
  zx::interrupt test_irq;
  ASSERT_OK(interrupt.duplicate(ZX_RIGHT_SAME_RIGHTS, &test_irq));

  driver_test.RunInEnvironmentTypeContext([&](auto& env) { env.Init(std::move(interrupt)); });

  ASSERT_OK(driver_test.StartDriver());

  driver_test.RunInDriverContext([](TestDwI2c& driver) {
    EXPECT_EQ(driver.transfer_timeout(), zx::sec(1));
    driver.set_transfer_timeout(zx::sec(10));
  });

  std::mutex wire_mutex;
  std::condition_variable wire_cv;
  bool stop_awaiting_empty_stat = false;
  uint32_t ready_transfers = 0;
  bool stop_wire_thread = false;
  std::atomic<uint32_t> irq_triggers{0};
  std::atomic<uint32_t> spurious_level_retriggers{0};

  // Wait until the STOP command is queued and interrupts are unmasked before completing the
  // transfer on the wire thread.
  controller.set_on_stop_command([&]() {
    std::lock_guard lock(wire_mutex);
    stop_awaiting_empty_stat = true;
  });
  controller.set_on_empty_intr_stat([&]() {
    std::lock_guard lock(wire_mutex);
    if (stop_awaiting_empty_stat) {
      stop_awaiting_empty_stat = false;
      ++ready_transfers;
      wire_cv.notify_one();
    }
  });

  // Re-triggers the interrupt if the driver acknowledges it while an unmasked interrupt status bit
  // is still asserted, matching level-triggered interrupt semantics.
  std::thread wire_thread([&]() {
    while (true) {
      {
        std::unique_lock lock(wire_mutex);
        wire_cv.wait(lock, [&]() { return ready_transfers > 0 || stop_wire_thread; });
        if (stop_wire_thread && ready_transfers == 0) {
          return;
        }
        --ready_transfers;
      }

      zx::nanosleep(zx::deadline_after(zx::usec(50)));
      EXPECT_TRUE(controller.CompleteWireTransfer());
      irq_triggers.fetch_add(1, std::memory_order_relaxed);
      EXPECT_OK(test_irq.trigger(0, zx::clock::get_boot()));

      while (controller.IsLevelIrqAsserted()) {
        zx_signals_t observed = 0;
        zx_status_t wait_status = test_irq.wait_one(ZX_VIRTUAL_INTERRUPT_UNTRIGGERED,
                                                    zx::deadline_after(zx::usec(10)), &observed);
        if (wait_status == ZX_OK && controller.IsLevelIrqAsserted()) {
          spurious_level_retriggers.fetch_add(1, std::memory_order_relaxed);
          EXPECT_OK(test_irq.trigger(0, zx::clock::get_boot()));
        }
      }
    }
  });

  auto join_wire_thread = fit::defer([&]() {
    controller.set_on_stop_command(nullptr);
    controller.set_on_empty_intr_stat(nullptr);
    {
      std::lock_guard lock(wire_mutex);
      stop_wire_thread = true;
      wire_cv.notify_all();
    }
    if (wire_thread.joinable()) {
      wire_thread.join();
    }
  });

  zx::result client_end = driver_test.Connect<fuchsia_hardware_i2cimpl::Service::Device>();
  ASSERT_OK(client_end);
  fdf::WireSyncClient<fuchsia_hardware_i2cimpl::Device> i2c(std::move(*client_end));
  fdf::Arena arena('TEST');

  // Execute 3 back-to-back 2-byte writes followed by a 1-byte write + 2-byte read transaction.
  for (uint8_t i = 0; i < 3; ++i) {
    std::array<uint8_t, 2> write_bytes = {static_cast<uint8_t>(0x10 + i),
                                          static_cast<uint8_t>(0x80 | i)};
    auto write_view =
        fidl::VectorView<uint8_t>::FromExternal(write_bytes.data(), write_bytes.size());
    std::array<fuchsia_hardware_i2cimpl::wire::I2cImplOp, 1> ops = {{
        {
            .address = 0x60,
            .type = fuchsia_hardware_i2cimpl::wire::I2cImplOpType::WithWriteData(
                fidl::ObjectView<fidl::VectorView<uint8_t>>::FromExternal(&write_view)),
            .stop = true,
        },
    }};
    auto result = i2c.buffer(arena)->Transact(
        fidl::VectorView<fuchsia_hardware_i2cimpl::wire::I2cImplOp>::FromExternal(ops.data(),
                                                                                  ops.size()));
    ASSERT_OK(result.status());
    ASSERT_TRUE(result->is_ok());
    EXPECT_EQ(result->value()->read.size(), 0u);
  }

  controller.QueueCannedRxBytes({0xa5, 0x5a});
  std::array<uint8_t, 1> reg_addr = {0x42};
  auto reg_view = fidl::VectorView<uint8_t>::FromExternal(reg_addr.data(), reg_addr.size());
  std::array<fuchsia_hardware_i2cimpl::wire::I2cImplOp, 2> rw_ops = {{
      {
          .address = 0x60,
          .type = fuchsia_hardware_i2cimpl::wire::I2cImplOpType::WithWriteData(
              fidl::ObjectView<fidl::VectorView<uint8_t>>::FromExternal(&reg_view)),
          .stop = false,
      },
      {
          .address = 0x60,
          .type = fuchsia_hardware_i2cimpl::wire::I2cImplOpType::WithReadSize(2),
          .stop = true,
      },
  }};
  auto rw_result = i2c.buffer(arena)->Transact(
      fidl::VectorView<fuchsia_hardware_i2cimpl::wire::I2cImplOp>::FromExternal(rw_ops.data(),
                                                                                rw_ops.size()));
  ASSERT_OK(rw_result.status());
  ASSERT_TRUE(rw_result->is_ok());
  ASSERT_EQ(rw_result->value()->read.size(), 1u);
  ASSERT_EQ(rw_result->value()->read[0].data.size(), 2u);
  EXPECT_EQ(rw_result->value()->read[0].data[0], 0xa5);
  EXPECT_EQ(rw_result->value()->read[0].data[1], 0x5a);

  join_wire_thread.call();

  EXPECT_EQ(irq_triggers.load(std::memory_order_relaxed), 4u);
  EXPECT_EQ(spurious_level_retriggers.load(std::memory_order_relaxed), 0u);
  EXPECT_FALSE(controller.IsLevelIrqAsserted());
  EXPECT_EQ(controller.ReadReg(0x30), 0u);  // InterruptMaskReg
  EXPECT_EQ(controller.ReadReg(0x6c), 0u);  // EnableReg

  zx_signals_t observed = 0;
  EXPECT_OK(
      test_irq.wait_one(ZX_VIRTUAL_INTERRUPT_UNTRIGGERED, zx::time::infinite_past(), &observed));

  const std::vector<FakeDwI2cController::RecordedCommand> cmds = controller.TakeTxCommands();
  ASSERT_EQ(cmds.size(), 9u);
  EXPECT_EQ(cmds[6].target_addr, 0x60);
  EXPECT_FALSE(cmds[6].is_read);
  EXPECT_EQ(cmds[6].data, 0x42);
  EXPECT_FALSE(cmds[6].stop);
  EXPECT_TRUE(cmds[7].is_read);
  EXPECT_TRUE(cmds[7].start);
  EXPECT_FALSE(cmds[7].stop);
  EXPECT_TRUE(cmds[8].is_read);
  EXPECT_TRUE(cmds[8].stop);

  ASSERT_OK(driver_test.StopDriver());
}

TEST(DwI2cTest, TransactTxAbortMasksBeforeAck) {
  FakeDwI2cController controller;
  TestDwI2c::set_mmio(controller.GetMmioBuffer());

  fdf_testing::BackgroundDriverTest<TestConfig> driver_test;

  zx::interrupt interrupt;
  ASSERT_OK(zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt));
  zx::interrupt test_irq;
  ASSERT_OK(interrupt.duplicate(ZX_RIGHT_SAME_RIGHTS, &test_irq));
  driver_test.RunInEnvironmentTypeContext([&](auto& env) { env.Init(std::move(interrupt)); });

  ASSERT_OK(driver_test.StartDriver());
  driver_test.RunInDriverContext(
      [](TestDwI2c& driver) { driver.set_transfer_timeout(zx::sec(10)); });

  std::mutex wire_mutex;
  std::condition_variable wire_cv;
  bool stop_awaiting_empty_stat = false;
  uint32_t ready_transfers = 0;
  bool stop_wire_thread = false;
  std::atomic<uint32_t> irq_triggers{0};
  std::atomic<uint32_t> spurious_level_retriggers{0};

  controller.set_on_stop_command([&]() {
    std::lock_guard lock(wire_mutex);
    stop_awaiting_empty_stat = true;
  });
  controller.set_on_empty_intr_stat([&]() {
    std::lock_guard lock(wire_mutex);
    if (stop_awaiting_empty_stat) {
      stop_awaiting_empty_stat = false;
      ++ready_transfers;
      wire_cv.notify_one();
    }
  });

  std::thread wire_thread([&]() {
    {
      std::unique_lock lock(wire_mutex);
      wire_cv.wait(lock, [&]() { return ready_transfers > 0 || stop_wire_thread; });
      if (stop_wire_thread && ready_transfers == 0) {
        return;
      }
      --ready_transfers;
    }
    EXPECT_TRUE(controller.FailWireTransferWithAbort(0x1u));
    irq_triggers.fetch_add(1, std::memory_order_relaxed);
    EXPECT_OK(test_irq.trigger(0, zx::clock::get_boot()));
    while (controller.IsLevelIrqAsserted()) {
      zx_signals_t observed = 0;
      zx_status_t wait_status = test_irq.wait_one(ZX_VIRTUAL_INTERRUPT_UNTRIGGERED,
                                                  zx::deadline_after(zx::usec(10)), &observed);
      if (wait_status == ZX_OK && controller.IsLevelIrqAsserted()) {
        spurious_level_retriggers.fetch_add(1, std::memory_order_relaxed);
        EXPECT_OK(test_irq.trigger(0, zx::clock::get_boot()));
      }
    }
  });

  auto join_wire_thread = fit::defer([&]() {
    controller.set_on_stop_command(nullptr);
    controller.set_on_empty_intr_stat(nullptr);
    {
      std::lock_guard lock(wire_mutex);
      stop_wire_thread = true;
      wire_cv.notify_all();
    }
    if (wire_thread.joinable()) {
      wire_thread.join();
    }
  });

  zx::result client_end = driver_test.Connect<fuchsia_hardware_i2cimpl::Service::Device>();
  ASSERT_OK(client_end);
  fdf::WireSyncClient<fuchsia_hardware_i2cimpl::Device> i2c(std::move(*client_end));
  fdf::Arena arena('TEST');

  std::array<uint8_t, 2> write_bytes = {0x01, 0x02};
  auto write_view = fidl::VectorView<uint8_t>::FromExternal(write_bytes.data(), write_bytes.size());
  std::array<fuchsia_hardware_i2cimpl::wire::I2cImplOp, 1> ops = {{
      {
          .address = 0x60,
          .type = fuchsia_hardware_i2cimpl::wire::I2cImplOpType::WithWriteData(
              fidl::ObjectView<fidl::VectorView<uint8_t>>::FromExternal(&write_view)),
          .stop = true,
      },
  }};
  auto result = i2c.buffer(arena)->Transact(
      fidl::VectorView<fuchsia_hardware_i2cimpl::wire::I2cImplOp>::FromExternal(ops.data(),
                                                                                ops.size()));
  join_wire_thread.call();

  ASSERT_OK(result.status());
  ASSERT_TRUE(result->is_error());
  EXPECT_STATUS(result->error_value(), ZX_ERR_IO);
  EXPECT_EQ(irq_triggers.load(std::memory_order_relaxed), 1u);
  EXPECT_EQ(spurious_level_retriggers.load(std::memory_order_relaxed), 0u);
  EXPECT_FALSE(controller.IsLevelIrqAsserted());
  EXPECT_EQ(controller.ReadReg(0x30), 0u);  // InterruptMaskReg
  EXPECT_EQ(controller.ReadReg(0x6c), 0u);  // EnableReg

  zx_signals_t observed = 0;
  EXPECT_OK(
      test_irq.wait_one(ZX_VIRTUAL_INTERRUPT_UNTRIGGERED, zx::time::infinite_past(), &observed));

  ASSERT_OK(driver_test.StopDriver());
}

TEST(DwI2cTest, TransactTimeoutMasksAndDisables) {
  FakeDwI2cController controller;
  TestDwI2c::set_mmio(controller.GetMmioBuffer());

  fdf_testing::BackgroundDriverTest<TestConfig> driver_test;

  zx::interrupt interrupt;
  ASSERT_OK(zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt));
  driver_test.RunInEnvironmentTypeContext([&](auto& env) { env.Init(std::move(interrupt)); });

  ASSERT_OK(driver_test.StartDriver());
  driver_test.RunInDriverContext(
      [](TestDwI2c& driver) { driver.set_transfer_timeout(zx::msec(5)); });

  zx::result client_end = driver_test.Connect<fuchsia_hardware_i2cimpl::Service::Device>();
  ASSERT_OK(client_end);
  fdf::WireSyncClient<fuchsia_hardware_i2cimpl::Device> i2c(std::move(*client_end));
  fdf::Arena arena('TEST');

  std::array<uint8_t, 2> write_bytes = {0x01, 0x02};
  auto write_view = fidl::VectorView<uint8_t>::FromExternal(write_bytes.data(), write_bytes.size());
  std::array<fuchsia_hardware_i2cimpl::wire::I2cImplOp, 1> ops = {{
      {
          .address = 0x60,
          .type = fuchsia_hardware_i2cimpl::wire::I2cImplOpType::WithWriteData(
              fidl::ObjectView<fidl::VectorView<uint8_t>>::FromExternal(&write_view)),
          .stop = true,
      },
  }};
  const zx::time start = zx::clock::get_monotonic();
  auto result = i2c.buffer(arena)->Transact(
      fidl::VectorView<fuchsia_hardware_i2cimpl::wire::I2cImplOp>::FromExternal(ops.data(),
                                                                                ops.size()));
  const zx::duration elapsed = zx::clock::get_monotonic() - start;

  ASSERT_OK(result.status());
  ASSERT_TRUE(result->is_error());
  EXPECT_STATUS(result->error_value(), ZX_ERR_TIMED_OUT);
  EXPECT_GE(elapsed, zx::msec(5));
  EXPECT_EQ(controller.ReadReg(0x30), 0u);  // InterruptMaskReg
  EXPECT_EQ(controller.ReadReg(0x6c), 0u);  // EnableReg

  ASSERT_OK(driver_test.StopDriver());
}

TEST(DwI2cTest, TransactMultiChunkTxEmptyAndStopDetLevelIrq) {
  FakeDwI2cController controller;
  TestDwI2c::set_mmio(controller.GetMmioBuffer());

  fdf_testing::BackgroundDriverTest<TestConfig> driver_test;

  zx::interrupt interrupt;
  ASSERT_OK(zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt));
  zx::interrupt test_irq;
  ASSERT_OK(interrupt.duplicate(ZX_RIGHT_SAME_RIGHTS, &test_irq));
  driver_test.RunInEnvironmentTypeContext([&](auto& env) { env.Init(std::move(interrupt)); });

  ASSERT_OK(driver_test.StartDriver());
  driver_test.RunInDriverContext(
      [](TestDwI2c& driver) { driver.set_transfer_timeout(zx::sec(10)); });

  std::mutex wire_mutex;
  std::condition_variable wire_cv;
  bool tx_fifo_full = false;
  bool empty_stat_after_fifo_full = false;
  bool stop_seen = false;
  uint32_t empty_stats_after_stop = 0;
  bool abort_wire_thread = false;
  std::atomic<uint32_t> irq_triggers{0};
  std::atomic<uint32_t> spurious_level_retriggers{0};

  controller.set_on_tx_fifo_full([&]() {
    std::lock_guard lock(wire_mutex);
    tx_fifo_full = true;
    empty_stat_after_fifo_full = false;
  });
  controller.set_on_empty_intr_stat([&]() {
    std::lock_guard lock(wire_mutex);
    if (tx_fifo_full && !empty_stat_after_fifo_full) {
      empty_stat_after_fifo_full = true;
      wire_cv.notify_one();
    }
    if (stop_seen) {
      ++empty_stats_after_stop;
      wire_cv.notify_one();
    }
  });
  controller.set_on_stop_command([&]() {
    std::lock_guard lock(wire_mutex);
    stop_seen = true;
    empty_stats_after_stop = 0;
    wire_cv.notify_one();
  });

  auto monitor_level_irq = [&]() {
    while (controller.IsLevelIrqAsserted()) {
      zx_signals_t observed = 0;
      zx_status_t wait_status = test_irq.wait_one(ZX_VIRTUAL_INTERRUPT_UNTRIGGERED,
                                                  zx::deadline_after(zx::usec(10)), &observed);
      if (wait_status == ZX_OK && controller.IsLevelIrqAsserted()) {
        spurious_level_retriggers.fetch_add(1, std::memory_order_relaxed);
        EXPECT_OK(test_irq.trigger(0, zx::clock::get_boot()));
      }
    }
  };

  std::thread wire_thread([&]() {
    {
      std::unique_lock lock(wire_mutex);
      wire_cv.wait(lock, [&]() {
        return (tx_fifo_full && empty_stat_after_fifo_full) || abort_wire_thread;
      });
      if (abort_wire_thread) {
        return;
      }
    }

    // Drain the initial 8-command TX FIFO chunk so TX_EMPTY wakes the driver for the next chunk.
    zx::nanosleep(zx::deadline_after(zx::usec(50)));
    EXPECT_TRUE(controller.DrainTxFifoToWire());
    irq_triggers.fetch_add(1, std::memory_order_relaxed);
    EXPECT_OK(test_irq.trigger(0, zx::clock::get_boot()));
    monitor_level_irq();

    // Wait for the remaining 4 commands (including STOP) to be queued and the TX_EMPTY interrupt
    // to be acknowledged before delivering STOP_DET.
    {
      std::unique_lock lock(wire_mutex);
      wire_cv.wait(
          lock, [&]() { return (stop_seen && empty_stats_after_stop >= 2) || abort_wire_thread; });
      if (abort_wire_thread) {
        return;
      }
    }
    zx_signals_t observed = 0;
    EXPECT_OK(test_irq.wait_one(ZX_VIRTUAL_INTERRUPT_UNTRIGGERED, zx::deadline_after(zx::sec(5)),
                                &observed));

    zx::nanosleep(zx::deadline_after(zx::usec(50)));
    EXPECT_TRUE(controller.CompleteWireTransfer());
    irq_triggers.fetch_add(1, std::memory_order_relaxed);
    EXPECT_OK(test_irq.trigger(0, zx::clock::get_boot()));
    monitor_level_irq();
  });

  auto join_wire_thread = fit::defer([&]() {
    controller.set_on_tx_fifo_full(nullptr);
    controller.set_on_empty_intr_stat(nullptr);
    controller.set_on_stop_command(nullptr);
    {
      std::lock_guard lock(wire_mutex);
      abort_wire_thread = true;
      wire_cv.notify_all();
    }
    if (wire_thread.joinable()) {
      wire_thread.join();
    }
  });

  zx::result client_end = driver_test.Connect<fuchsia_hardware_i2cimpl::Service::Device>();
  ASSERT_OK(client_end);
  fdf::WireSyncClient<fuchsia_hardware_i2cimpl::Device> i2c(std::move(*client_end));
  fdf::Arena arena('TEST');

  // Two 6-byte WriteOps = 12 commands (> tx_fifo_depth_ == 8), while each op is <= kMaxTransfer
  // (8), forcing a priming 8-command chunk + TX_EMPTY wakeup + 4-command chunk + STOP_DET wakeup.
  std::array<uint8_t, 6> chunk0 = {0x10, 0x11, 0x12, 0x13, 0x14, 0x15};
  std::array<uint8_t, 6> chunk1 = {0x20, 0x21, 0x22, 0x23, 0x24, 0x25};
  auto view0 = fidl::VectorView<uint8_t>::FromExternal(chunk0.data(), chunk0.size());
  auto view1 = fidl::VectorView<uint8_t>::FromExternal(chunk1.data(), chunk1.size());
  std::array<fuchsia_hardware_i2cimpl::wire::I2cImplOp, 2> ops = {{
      {
          .address = 0x60,
          .type = fuchsia_hardware_i2cimpl::wire::I2cImplOpType::WithWriteData(
              fidl::ObjectView<fidl::VectorView<uint8_t>>::FromExternal(&view0)),
          .stop = false,
      },
      {
          .address = 0x60,
          .type = fuchsia_hardware_i2cimpl::wire::I2cImplOpType::WithWriteData(
              fidl::ObjectView<fidl::VectorView<uint8_t>>::FromExternal(&view1)),
          .stop = true,
      },
  }};
  auto result = i2c.buffer(arena)->Transact(
      fidl::VectorView<fuchsia_hardware_i2cimpl::wire::I2cImplOp>::FromExternal(ops.data(),
                                                                                ops.size()));
  join_wire_thread.call();

  ASSERT_OK(result.status());
  ASSERT_TRUE(result->is_ok());
  EXPECT_EQ(irq_triggers.load(std::memory_order_relaxed), 2u);
  EXPECT_EQ(spurious_level_retriggers.load(std::memory_order_relaxed), 0u);
  EXPECT_FALSE(controller.IsLevelIrqAsserted());
  EXPECT_EQ(controller.ReadReg(0x30), 0u);  // InterruptMaskReg
  EXPECT_EQ(controller.ReadReg(0x6c), 0u);  // EnableReg

  zx_signals_t observed = 0;
  EXPECT_OK(
      test_irq.wait_one(ZX_VIRTUAL_INTERRUPT_UNTRIGGERED, zx::time::infinite_past(), &observed));

  const std::vector<FakeDwI2cController::RecordedCommand> cmds = controller.TakeTxCommands();
  ASSERT_EQ(cmds.size(), 12u);
  for (size_t i = 0; i < 6; ++i) {
    EXPECT_EQ(cmds[i].data, chunk0[i]);
    EXPECT_FALSE(cmds[i].is_read);
    EXPECT_FALSE(cmds[i].stop);
  }
  for (size_t i = 0; i < 6; ++i) {
    EXPECT_EQ(cmds[6 + i].data, chunk1[i]);
    EXPECT_FALSE(cmds[6 + i].is_read);
    EXPECT_EQ(cmds[6 + i].start, i == 0);
    EXPECT_EQ(cmds[6 + i].stop, i == 5);
  }

  ASSERT_OK(driver_test.StopDriver());
}

TEST(DwI2cTest, TransactDrainsRetriggeredInterruptOnAck) {
  FakeDwI2cController controller;
  TestDwI2c::set_mmio(controller.GetMmioBuffer());

  fdf_testing::BackgroundDriverTest<TestConfig> driver_test;

  zx::interrupt interrupt;
  ASSERT_OK(zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt));
  zx::interrupt test_irq;
  ASSERT_OK(interrupt.duplicate(ZX_RIGHT_SAME_RIGHTS, &test_irq));
  driver_test.RunInEnvironmentTypeContext([&](auto& env) { env.Init(std::move(interrupt)); });

  ASSERT_OK(driver_test.StartDriver());
  driver_test.RunInDriverContext(
      [](TestDwI2c& driver) { driver.set_transfer_timeout(zx::sec(10)); });

  std::mutex wire_mutex;
  std::condition_variable wire_cv;
  bool stop_awaiting_empty_stat = false;
  uint32_t ready_transfers = 0;
  bool stop_wire_thread = false;
  std::atomic<bool> inject_retrigger_on_clear{true};
  std::atomic<uint32_t> retrigger_count{0};

  controller.set_on_stop_command([&]() {
    std::lock_guard lock(wire_mutex);
    stop_awaiting_empty_stat = true;
  });
  controller.set_on_empty_intr_stat([&]() {
    std::lock_guard lock(wire_mutex);
    if (stop_awaiting_empty_stat) {
      stop_awaiting_empty_stat = false;
      ++ready_transfers;
      wire_cv.notify_one();
    }
  });
  // Trigger the interrupt again when ClearStopDetReg is read (before the end-of-transaction ack)
  // so acknowledging the first interrupt re-queues a packet on irq_port_.
  controller.set_on_clear_stop_det([&]() {
    if (inject_retrigger_on_clear.exchange(false, std::memory_order_relaxed)) {
      retrigger_count.fetch_add(1, std::memory_order_relaxed);
      EXPECT_OK(test_irq.trigger(0, zx::clock::get_boot()));
    }
  });

  std::thread wire_thread([&]() {
    while (true) {
      {
        std::unique_lock lock(wire_mutex);
        wire_cv.wait(lock, [&]() { return ready_transfers > 0 || stop_wire_thread; });
        if (stop_wire_thread && ready_transfers == 0) {
          return;
        }
        --ready_transfers;
      }
      zx::nanosleep(zx::deadline_after(zx::msec(1)));
      EXPECT_TRUE(controller.CompleteWireTransfer());
      EXPECT_OK(test_irq.trigger(0, zx::clock::get_boot()));
    }
  });

  auto join_wire_thread = fit::defer([&]() {
    controller.set_on_stop_command(nullptr);
    controller.set_on_empty_intr_stat(nullptr);
    controller.set_on_clear_stop_det(nullptr);
    {
      std::lock_guard lock(wire_mutex);
      stop_wire_thread = true;
      wire_cv.notify_all();
    }
    if (wire_thread.joinable()) {
      wire_thread.join();
    }
  });

  zx::result client_end = driver_test.Connect<fuchsia_hardware_i2cimpl::Service::Device>();
  ASSERT_OK(client_end);
  fdf::WireSyncClient<fuchsia_hardware_i2cimpl::Device> i2c(std::move(*client_end));
  fdf::Arena arena('TEST');

  std::array<uint8_t, 2> write_bytes = {0x33, 0x44};
  auto write_view = fidl::VectorView<uint8_t>::FromExternal(write_bytes.data(), write_bytes.size());
  std::array<fuchsia_hardware_i2cimpl::wire::I2cImplOp, 1> ops = {{
      {
          .address = 0x60,
          .type = fuchsia_hardware_i2cimpl::wire::I2cImplOpType::WithWriteData(
              fidl::ObjectView<fidl::VectorView<uint8_t>>::FromExternal(&write_view)),
          .stop = true,
      },
  }};
  auto result = i2c.buffer(arena)->Transact(
      fidl::VectorView<fuchsia_hardware_i2cimpl::wire::I2cImplOp>::FromExternal(ops.data(),
                                                                                ops.size()));
  ASSERT_OK(result.status());
  ASSERT_TRUE(result->is_ok());
  EXPECT_EQ(retrigger_count.load(std::memory_order_relaxed), 1u);

  // Verify the re-queued interrupt packet was drained and acknowledged before Transact returned.
  zx_signals_t observed = 0;
  EXPECT_OK(
      test_irq.wait_one(ZX_VIRTUAL_INTERRUPT_UNTRIGGERED, zx::time::infinite_past(), &observed));

  // Verify a subsequent transaction still wakes cleanly via irq_port_.
  auto result2 = i2c.buffer(arena)->Transact(
      fidl::VectorView<fuchsia_hardware_i2cimpl::wire::I2cImplOp>::FromExternal(ops.data(),
                                                                                ops.size()));
  join_wire_thread.call();

  ASSERT_OK(result2.status());
  ASSERT_TRUE(result2->is_ok());
  EXPECT_EQ(controller.ReadReg(0x30), 0u);  // InterruptMaskReg
  EXPECT_EQ(controller.ReadReg(0x6c), 0u);  // EnableReg
  EXPECT_OK(
      test_irq.wait_one(ZX_VIRTUAL_INTERRUPT_UNTRIGGERED, zx::time::infinite_past(), &observed));

  ASSERT_OK(driver_test.StopDriver());
}

}  // namespace dw_i2c
