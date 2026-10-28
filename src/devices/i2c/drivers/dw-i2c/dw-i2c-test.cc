// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "dw-i2c.h"

#include <lib/driver/fake-clock/cpp/fake-clock.h>
#include <lib/driver/fake-platform-device/cpp/fake-pdev.h>
#include <lib/driver/fake-powerdomain/cpp/fake-powerdomain.h>
#include <lib/driver/fake-reset/cpp/fake-reset.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/fit/function.h>
#include <lib/zx/clock.h>
#include <lib/zx/interrupt.h>

#include <array>
#include <deque>
#include <initializer_list>
#include <mutex>
#include <optional>
#include <utility>
#include <vector>

#include <fake-mmio-reg/fake-mmio-reg.h>
#include <gtest/gtest.h>

#include "src/lib/testing/predicates/status.h"

namespace dw_i2c {

class TestDwI2c : public DwI2c {
 public:
  using DwI2c::set_transfer_timeout;

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

// Fake DesignWare I2C controller.
//
// Once given the driver's virtual interrupt via `set_interrupt()`, the fake also models the I2C bus
// and the level-triggered interrupt line, synchronously inside the driver's MMIO accesses:
//  - The bus only makes progress when the driver reads a zero InterruptStatusReg while no interrupt
//    is pending, i.e. just before the driver waits for the interrupt. At that point all queued
//    commands complete on the wire (or the transfer aborts, see `AbortNextTransfer()`), and the
//    interrupt is triggered if this asserted an unmasked status bit. The read still returns zero,
//    so the driver must be woken by the interrupt. A consequence of this model is that the driver
//    must re-read InterruptStatusReg after acknowledging the interrupt for the bus to advance.
//  - Whenever an unmasked status bit is asserted while no interrupt is pending, a level-triggered
//    interrupt would fire again. The fake detects this on every register access, re-triggers the
//    interrupt, and counts it in `spurious_retrigger_count()`.
class FakeDwI2cController {
 public:
  static constexpr size_t kRegCount = 0x100;
  static constexpr uint32_t kTxFifoDepth = 8;
  static constexpr uint32_t kRxFifoDepth = 8;

  // Register offsets.
  static constexpr size_t kTargetAddress = 0x04;  // TargetAddressReg
  static constexpr size_t kDataCommand = 0x10;    // DataCommandReg
  static constexpr size_t kIntrStat = 0x2c;       // InterruptStatusReg
  static constexpr size_t kIntrMask = 0x30;       // InterruptMaskReg
  static constexpr size_t kRawIntrStat = 0x34;    // RawInterruptStatusReg
  static constexpr size_t kRxTl = 0x38;           // RxFifoThresholdReg
  static constexpr size_t kTxTl = 0x3c;           // TxFifoThresholdReg
  static constexpr size_t kClrIntr = 0x40;        // ClearInterruptReg
  static constexpr size_t kClrTxAbrt = 0x54;      // ClearTxAbrtReg
  static constexpr size_t kClrStopDet = 0x60;     // ClearStopDetReg
  static constexpr size_t kEnable = 0x6c;         // EnableReg
  static constexpr size_t kTxFifoLevel = 0x74;    // TxFifoLevelReg
  static constexpr size_t kRxFifoLevel = 0x78;    // RxFifoLevelReg
  static constexpr size_t kTxAbrtSource = 0x80;   // TxAbrtSourceReg
  static constexpr size_t kEnableStatus = 0x9c;   // EnableStatusReg
  static constexpr size_t kCompParam1 = 0xf4;     // CompParam1Reg
  static constexpr size_t kCompType = 0xfc;       // CompTypeReg

  struct RecordedCommand {
    uint16_t target_addr;
    uint8_t data;
    bool is_read;
    bool start;
    bool stop;
  };

  FakeDwI2cController() : mmio_(sizeof(uint32_t), kRegCount) {
    for (size_t i = 0; i < kRegCount; ++i) {
      OnRead(i * sizeof(uint32_t), [this, i]() { return regs_[i]; });
      OnWrite(i * sizeof(uint32_t), [this, i](uint32_t val) { regs_[i] = val; });
    }

    // Writing queues TX/read commands; reading pops from RX FIFO.
    OnRead(kDataCommand, [this]() -> uint32_t {
      uint8_t byte = 0;
      if (!rx_fifo_.empty()) {
        byte = rx_fifo_.front();
        rx_fifo_.pop_front();
      }
      UpdateRxFifoStateLocked();
      return byte;
    });
    OnWrite(kDataCommand, [this](uint32_t val) {
      auto cmd = DataCommandReg::Get().FromValue(val);
      auto tar = TargetAddressReg::Get().FromValue(RegLocked(kTargetAddress));
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
      if (cmd.stop() != 0) {
        stop_pending_ = true;
      }
    });

    // InterruptStatusReg = RawInterruptStatusReg & InterruptMaskReg.
    OnRead(kIntrStat, [this]() {
      const uint32_t status = InterruptStatusLocked();
      if (status == 0 && irq_.is_valid() && !bus_stalled_ && !IsInterruptTriggered()) {
        AdvanceBusLocked();
        if (InterruptStatusLocked() != 0) {
          TriggerInterruptLocked();
        }
      }
      return status;
    });

    // The FIFO thresholds determine the raw RX_FULL / TX_EMPTY bits.
    OnWrite(kRxTl, [this](uint32_t val) {
      RegLocked(kRxTl) = val;
      UpdateRxFifoStateLocked();
    });
    OnWrite(kTxTl, [this](uint32_t val) {
      RegLocked(kTxTl) = val;
      UpdateTxFifoStateLocked();
    });

    // Reading the clear registers clears latched raw interrupt status bits. The fake only latches
    // TX_ABRT and STOP_DET.
    const uint32_t tx_abrt = RawInterruptStatusReg::Get().FromValue(0).set_tx_abrt(1).reg_value();
    const uint32_t stop_det = RawInterruptStatusReg::Get().FromValue(0).set_stop_det(1).reg_value();
    OnReadClearsRaw(kClrIntr, tx_abrt | stop_det);
    OnReadClearsRaw(kClrTxAbrt, tx_abrt);
    OnRead(kClrStopDet, [this, stop_det]() {
      RegLocked(kRawIntrStat) &= ~stop_det;
      if (retrigger_on_stop_det_clear_) {
        retrigger_on_stop_det_clear_ = false;
        TriggerInterruptLocked();
      }
      return 0u;
    });

    // EnableReg mirrors enable state into EnableStatusReg and flushes FIFOs when disabled.
    OnWrite(kEnable, [this](uint32_t val) {
      RegLocked(kEnable) = val;
      RegLocked(kEnableStatus) = val & 0x1u;
      if ((val & 0x1u) == 0) {
        FlushFifosLocked();
      }
      UpdateTxFifoStateLocked();
    });

    // Report FIFO depths of 8 for tx and rx.
    OnRead(kCompParam1, []() { return (kTxFifoDepth << 16) | (kRxFifoDepth << 8); });
    // Report kDwCompTypeNum (0x44570140) when powered.
    OnRead(kCompType, [this]() { return powered_ ? DwI2c::kDwCompTypeNum : 0u; });
  }

  fdf::MmioBuffer GetMmioBuffer() { return mmio_.GetMmioBuffer(); }

  uint32_t ReadReg(size_t offset) const {
    std::lock_guard lock(lock_);
    return regs_[offset / sizeof(uint32_t)];
  }

  void ResetWritableRegs() {
    std::lock_guard lock(lock_);
    regs_.fill(0);
    FlushFifosLocked();
  }

  void set_powered(bool powered) {
    std::lock_guard lock(lock_);
    powered_ = powered;
  }

  // Enables modelling of the bus and the level-triggered interrupt line. `irq` must be a duplicate
  // of the virtual interrupt given to the driver, and must be set before the driver starts.
  void set_interrupt(zx::interrupt irq) {
    std::lock_guard lock(lock_);
    irq_ = std::move(irq);
  }

  // Prevents the bus from making progress, so queued transfers never complete.
  void StallBus() {
    std::lock_guard lock(lock_);
    bus_stalled_ = true;
  }

  // Makes the next transfer on the bus fail with TX_ABRT, latching `abort_source` in
  // TxAbrtSourceReg (e.g. 0x1 for an address NACK).
  void AbortNextTransfer(uint32_t abort_source) {
    std::lock_guard lock(lock_);
    abort_source_ = abort_source;
  }

  // Triggers the interrupt once more when the driver next clears STOP_DET, i.e. while the driver
  // still has to acknowledge the interrupt that reported it.
  void RetriggerOnNextStopDetClear() {
    std::lock_guard lock(lock_);
    retrigger_on_stop_det_clear_ = true;
  }

  void QueueCannedRxBytes(std::initializer_list<uint8_t> bytes) {
    std::lock_guard lock(lock_);
    canned_rx_bytes_.insert(canned_rx_bytes_.end(), bytes);
  }

  std::vector<RecordedCommand> TakeTxCommands() {
    std::lock_guard lock(lock_);
    return std::exchange(tx_commands_, {});
  }

  // Returns true if the interrupt has been triggered and not yet acknowledged.
  //
  // `irq_` is set once before the driver starts, so this can read it without holding `lock_` (and
  // is also called from MMIO callbacks with `lock_` held).
  bool IsInterruptTriggered() const {
    zx_signals_t observed = 0;
    return irq_.is_valid() && irq_.wait_one(ZX_VIRTUAL_INTERRUPT_UNTRIGGERED,
                                            zx::time::infinite_past(), &observed) != ZX_OK;
  }

  // Number of times the fake triggered the interrupt, excluding spurious re-triggers.
  uint32_t irq_trigger_count() const {
    std::lock_guard lock(lock_);
    return irq_trigger_count_;
  }

  // Number of times an unmasked status bit was asserted while the interrupt wasn't pending (e.g.
  // the driver acknowledged the interrupt too early, or unmasked an already-asserted bit), so the
  // fake re-asserted the level-triggered interrupt.
  uint32_t spurious_retrigger_count() const {
    std::lock_guard lock(lock_);
    return spurious_retrigger_count_;
  }

 private:
  // Installs MMIO callbacks that run with `lock_` held, after re-triggering the interrupt if an
  // unmasked status bit is asserted while the interrupt isn't pending.
  void OnRead(size_t offset, fit::function<uint32_t()> read) {
    mmio_[offset].SetReadCallback([this, read = std::move(read)]() -> uint64_t {
      std::lock_guard lock(lock_);
      RetriggerIfLevelAssertedLocked();
      return read();
    });
  }
  void OnWrite(size_t offset, fit::function<void(uint32_t)> write) {
    mmio_[offset].SetWriteCallback([this, write = std::move(write)](uint64_t val) {
      std::lock_guard lock(lock_);
      RetriggerIfLevelAssertedLocked();
      write(static_cast<uint32_t>(val));
    });
  }
  // Installs a read callback that clears `bits` in RawInterruptStatusReg.
  void OnReadClearsRaw(size_t offset, uint32_t bits) {
    OnRead(offset, [this, bits]() {
      RegLocked(kRawIntrStat) &= ~bits;
      return 0u;
    });
  }

  uint32_t& RegLocked(size_t offset) { return regs_[offset / sizeof(uint32_t)]; }

  uint32_t InterruptStatusLocked() { return RegLocked(kRawIntrStat) & RegLocked(kIntrMask); }

  void TriggerInterruptLocked() {
    ++irq_trigger_count_;
    EXPECT_OK(irq_.trigger(0, zx::clock::get_boot()));
  }

  void RetriggerIfLevelAssertedLocked() {
    if (InterruptStatusLocked() != 0 && irq_.is_valid() && !IsInterruptTriggered()) {
      ++spurious_retrigger_count_;
      EXPECT_OK(irq_.trigger(0, zx::clock::get_boot()));
    }
  }

  // Completes (or aborts) all commands queued in the TX FIFO.
  void AdvanceBusLocked() {
    auto raw = RawInterruptStatusReg::Get().FromValue(RegLocked(kRawIntrStat));
    if (abort_source_) {
      RegLocked(kTxAbrtSource) = *std::exchange(abort_source_, std::nullopt);
      raw.set_tx_abrt(1);
    } else {
      rx_fifo_.insert(rx_fifo_.end(), pending_wire_rx_.begin(), pending_wire_rx_.end());
      if (stop_pending_) {
        raw.set_stop_det(1);
      }
    }
    RegLocked(kRawIntrStat) = raw.reg_value();
    tx_fifo_level_ = 0;
    pending_wire_rx_.clear();
    stop_pending_ = false;
    UpdateTxFifoStateLocked();
    UpdateRxFifoStateLocked();
  }

  void FlushFifosLocked() {
    tx_fifo_level_ = 0;
    pending_wire_rx_.clear();
    stop_pending_ = false;
    rx_fifo_.clear();
    UpdateRxFifoStateLocked();
  }

  void UpdateTxFifoStateLocked() {
    RegLocked(kTxFifoLevel) = tx_fifo_level_;
    const bool enabled = (RegLocked(kEnable) & 0x1u) != 0;
    const uint32_t tx_tl = RegLocked(kTxTl) & 0xffu;
    auto raw = RawInterruptStatusReg::Get().FromValue(RegLocked(kRawIntrStat));
    raw.set_tx_empty(enabled && tx_fifo_level_ <= tx_tl ? 1 : 0);
    RegLocked(kRawIntrStat) = raw.reg_value();
  }

  void UpdateRxFifoStateLocked() {
    RegLocked(kRxFifoLevel) = static_cast<uint32_t>(rx_fifo_.size());
    const uint32_t rx_tl = RegLocked(kRxTl) & 0xffu;
    auto raw = RawInterruptStatusReg::Get().FromValue(RegLocked(kRawIntrStat));
    raw.set_rx_full(!rx_fifo_.empty() && rx_fifo_.size() > rx_tl ? 1 : 0);
    RegLocked(kRawIntrStat) = raw.reg_value();
  }

  mutable std::mutex lock_;
  bool powered_ = true;
  uint32_t tx_fifo_level_ = 0;
  std::array<uint32_t, kRegCount> regs_{};
  std::vector<RecordedCommand> tx_commands_;
  std::deque<uint8_t> canned_rx_bytes_;
  // Bytes to be read by the queued read commands, and whether a queued command has STOP set.
  std::deque<uint8_t> pending_wire_rx_;
  bool stop_pending_ = false;
  std::deque<uint8_t> rx_fifo_;
  zx::interrupt irq_;
  bool bus_stalled_ = false;
  std::optional<uint32_t> abort_source_;
  bool retrigger_on_stop_det_clear_ = false;
  uint32_t irq_trigger_count_ = 0;
  uint32_t spurious_retrigger_count_ = 0;
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

// Fixture for Transact tests. The fake controller models the bus and the level-triggered interrupt
// line, so no extra test threads are needed: each transfer completes inside the driver's
// InterruptStatusReg read just before it waits for the interrupt. The bus only advances on a zero
// status read while no interrupt is pending, so the driver must re-read the status after acking.
class DwI2cTransactTest : public ::testing::Test {
 protected:
  using I2cImplOp = fuchsia_hardware_i2cimpl::wire::I2cImplOp;
  static constexpr uint16_t kAddress = 0x60;

  void SetUp() override {
    TestDwI2c::set_mmio(controller_.GetMmioBuffer());

    zx::interrupt interrupt;
    ASSERT_OK(zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt));
    zx::interrupt controller_irq;
    ASSERT_OK(interrupt.duplicate(ZX_RIGHT_SAME_RIGHTS, &controller_irq));
    controller_.set_interrupt(std::move(controller_irq));
    driver_test_.RunInEnvironmentTypeContext([&](auto& env) { env.Init(std::move(interrupt)); });

    ASSERT_OK(driver_test_.StartDriver());
    driver_started_ = true;

    zx::result client_end = driver_test_.Connect<fuchsia_hardware_i2cimpl::Service::Device>();
    ASSERT_OK(client_end);
    i2c_ = fdf::WireSyncClient(std::move(*client_end));
  }

  void TearDown() override {
    if (driver_started_) {
      EXPECT_OK(driver_test_.StopDriver());
    }
  }

  I2cImplOp WriteOp(const std::vector<uint8_t>& data, bool stop) {
    fidl::VectorView<uint8_t> bytes(arena_, data);
    return {
        .address = kAddress,
        .type = fuchsia_hardware_i2cimpl::wire::I2cImplOpType::WithWriteData(arena_, bytes),
        .stop = stop,
    };
  }

  static I2cImplOp ReadOp(uint32_t size, bool stop) {
    return {
        .address = kAddress,
        .type = fuchsia_hardware_i2cimpl::wire::I2cImplOpType::WithReadSize(size),
        .stop = stop,
    };
  }

  // Performs a transaction and returns the data received by its read ops.
  zx::result<std::vector<std::vector<uint8_t>>> Transact(std::vector<I2cImplOp> ops) {
    auto result = i2c_.buffer(arena_)->Transact(fidl::VectorView<I2cImplOp>::FromExternal(ops));
    EXPECT_OK(result.status());
    if (!result.ok()) {
      return zx::error(result.status());
    }
    if (result->is_error()) {
      return zx::error(result->error_value());
    }
    std::vector<std::vector<uint8_t>> reads;
    for (const auto& read : result->value()->read) {
      reads.emplace_back(read.data.begin(), read.data.end());
    }
    return zx::ok(std::move(reads));
  }

  // Checks that the driver masked interrupts, disabled the controller, and acknowledged the
  // interrupt only once it was no longer asserted.
  void ExpectIdle() {
    EXPECT_EQ(controller_.ReadReg(FakeDwI2cController::kIntrMask), 0u);
    EXPECT_EQ(controller_.ReadReg(FakeDwI2cController::kEnable), 0u);
    EXPECT_FALSE(controller_.IsInterruptTriggered());
    EXPECT_EQ(controller_.spurious_retrigger_count(), 0u);
  }

  // The controller must outlive the driver.
  FakeDwI2cController controller_;
  fdf_testing::BackgroundDriverTest<TestConfig> driver_test_;
  bool driver_started_ = false;
  fdf::WireSyncClient<fuchsia_hardware_i2cimpl::Device> i2c_;
  fdf::Arena arena_{'TEST'};
};

TEST_F(DwI2cTransactTest, WakesOnLevelIrqWithoutSpuriousRetrigger) {
  // Execute 3 back-to-back 2-byte writes followed by a 1-byte write + 2-byte read transaction.
  for (uint8_t i = 0; i < 3; ++i) {
    zx::result reads =
        Transact({WriteOp({static_cast<uint8_t>(0x10 + i), static_cast<uint8_t>(0x80 | i)},
                          /*stop=*/true)});
    ASSERT_OK(reads);
    EXPECT_TRUE(reads->empty());
    ExpectIdle();
  }

  controller_.QueueCannedRxBytes({0xa5, 0x5a});
  zx::result reads = Transact({WriteOp({0x42}, /*stop=*/false), ReadOp(2, /*stop=*/true)});
  ASSERT_OK(reads);
  ASSERT_EQ(reads->size(), 1u);
  EXPECT_EQ(reads->at(0), (std::vector<uint8_t>{0xa5, 0x5a}));
  ExpectIdle();

  // Each transaction is completed by a single STOP_DET interrupt.
  EXPECT_EQ(controller_.irq_trigger_count(), 4u);

  const std::vector<FakeDwI2cController::RecordedCommand> cmds = controller_.TakeTxCommands();
  ASSERT_EQ(cmds.size(), 9u);
  for (uint8_t i = 0; i < 3; ++i) {
    EXPECT_EQ(cmds[2 * i].data, 0x10 + i);
    EXPECT_FALSE(cmds[2 * i].stop);
    EXPECT_EQ(cmds[2 * i + 1].data, 0x80 | i);
    EXPECT_TRUE(cmds[2 * i + 1].stop);
  }
  EXPECT_EQ(cmds[6].target_addr, kAddress);
  EXPECT_FALSE(cmds[6].is_read);
  EXPECT_EQ(cmds[6].data, 0x42);
  EXPECT_FALSE(cmds[6].stop);
  EXPECT_TRUE(cmds[7].is_read);
  EXPECT_TRUE(cmds[7].start);
  EXPECT_FALSE(cmds[7].stop);
  EXPECT_TRUE(cmds[8].is_read);
  EXPECT_TRUE(cmds[8].stop);
}

TEST_F(DwI2cTransactTest, TxAbortMasksBeforeAck) {
  // Use a transfer longer than the TX FIFO so TX_EMPTY is still unmasked when the abort is handled.
  // The abort flushes the TX FIFO, asserting TX_EMPTY, so acknowledging the interrupt before
  // masking it would re-trigger the level-triggered interrupt.
  controller_.AbortNextTransfer(0x1);

  EXPECT_STATUS(Transact({WriteOp({0, 1, 2, 3, 4, 5}, /*stop=*/false),
                          WriteOp({6, 7, 8, 9, 10, 11}, /*stop=*/true)}),
                ZX_ERR_IO);
  EXPECT_EQ(controller_.irq_trigger_count(), 1u);
  ExpectIdle();
  // Nothing is transmitted after the abort.
  EXPECT_EQ(controller_.TakeTxCommands().size(), 8u);
}

TEST_F(DwI2cTransactTest, TimeoutMasksAndDisables) {
  controller_.StallBus();
  driver_test_.RunInDriverContext(
      [](TestDwI2c& driver) { driver.set_transfer_timeout(zx::msec(5)); });

  const zx::time start = zx::clock::get_monotonic();
  zx::result reads = Transact({WriteOp({0x01, 0x02}, /*stop=*/true)});
  const zx::duration elapsed = zx::clock::get_monotonic() - start;

  EXPECT_STATUS(reads, ZX_ERR_TIMED_OUT);
  EXPECT_GE(elapsed, zx::msec(5));
  EXPECT_EQ(controller_.irq_trigger_count(), 0u);
  ExpectIdle();
}

TEST_F(DwI2cTransactTest, MultiChunkTxEmptyAndStopDetLevelIrq) {
  // Two 6-byte WriteOps = 12 commands (> tx_fifo_depth_ == 8), while each op is <= kMaxTransfer
  // (8), forcing a priming 8-command chunk + TX_EMPTY wakeup + 4-command chunk + STOP_DET wakeup.
  const std::vector<uint8_t> chunk0 = {0x10, 0x11, 0x12, 0x13, 0x14, 0x15};
  const std::vector<uint8_t> chunk1 = {0x20, 0x21, 0x22, 0x23, 0x24, 0x25};
  ASSERT_OK(Transact({WriteOp(chunk0, /*stop=*/false), WriteOp(chunk1, /*stop=*/true)}));
  EXPECT_EQ(controller_.irq_trigger_count(), 2u);
  ExpectIdle();

  const std::vector<FakeDwI2cController::RecordedCommand> cmds = controller_.TakeTxCommands();
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
}

TEST_F(DwI2cTransactTest, DrainsRetriggeredInterruptOnAck) {
  // Trigger the interrupt again when ClearStopDetReg is read (before the end-of-transaction ack)
  // so acknowledging the first interrupt re-queues a packet on irq_port_.
  controller_.RetriggerOnNextStopDetClear();
  ASSERT_OK(Transact({WriteOp({0x33, 0x44}, /*stop=*/true)}));
  EXPECT_EQ(controller_.irq_trigger_count(), 2u);

  // Verify the re-queued interrupt packet was drained and acknowledged before Transact returned.
  ExpectIdle();

  // Verify a subsequent transaction still wakes cleanly via irq_port_.
  ASSERT_OK(Transact({WriteOp({0x33, 0x44}, /*stop=*/true)}));
  EXPECT_EQ(controller_.irq_trigger_count(), 3u);
  ExpectIdle();
}

}  // namespace dw_i2c
