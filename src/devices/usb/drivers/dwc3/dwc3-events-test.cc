// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.usb.dci/cpp/wire.h>
#include <lib/fit/defer.h>

#include <atomic>

#include <gtest/gtest.h>

#include "src/devices/usb/drivers/dwc3/dwc3-regs.h"
#include "src/devices/usb/drivers/dwc3/dwc3-test-fixture.h"
#include "src/devices/usb/drivers/dwc3/dwc3.h"

namespace dwc3 {

constexpr uint32_t MakeDeviceEvent(uint32_t type) {
  return (type << 8) | 1;  // Bit 0 = 1 for non-endpoint device events
}
constexpr uint32_t kEvtDisconnect = MakeDeviceEvent(0);      // DEVT_DISCONNECT
constexpr uint32_t kEvtUsbReset = MakeDeviceEvent(1);        // DEVT_USB_RESET
constexpr uint32_t kEvtConnectionDone = MakeDeviceEvent(2);  // DEVT_CONNECTION_DONE

using Dwc3EventsTest = UnmanagedTestFixture;

// Verifies behavior on a simulated disconnect event.
TEST_F(Dwc3EventsTest, HandleDisconnectEvent) {
  std::atomic<bool> cmd_written = false;
  std::atomic<uint32_t> depcmd_val = 0;

  SetUpAndPowerOnDriver();

  FakeUsbDciInterface fake_dci;
  std::atomic<bool> set_connected_called = false;
  std::atomic<bool> is_connected_val = true;
  fake_dci.SetSetConnectedCallback([&](bool connected) {
    is_connected_val.store(connected);
    set_connected_called.store(true);
  });

  auto endpoints = fidl::CreateEndpoints<fuchsia_hardware_usb_dci::UsbDciInterface>();
  ASSERT_TRUE(endpoints.is_ok());

  // Use RAII guard to guarantee register callback cleanup on scope exit regardless of assertions.
  auto cleanup_callbacks = fit::defer([&]() {
    dut_.RunInEnvironmentTypeContext([&](Environment& env) {
      env.reg_region()[DEPCMD::Get(2).addr()].SetReadCallback(nullptr);
      env.reg_region()[DEPCMD::Get(2).addr()].SetWriteCallback(nullptr);
    });
  });

  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    auto& depcmd = env.reg_region()[DEPCMD::Get(2).addr()];

    depcmd.SetReadCallback([&]() -> uint32_t { return depcmd_val.load(); });
    depcmd.SetWriteCallback([&](uint64_t val_raw) {
      uint32_t val = static_cast<uint32_t>(val_raw);
      cmd_written.store(true);
      depcmd_val.store(val & ~(1 << 10));
    });
  });

  std::optional<fidl::ServerBindingRef<fuchsia_hardware_usb_dci::UsbDciInterface>> binding;
  dut_.RunInDriverContext([&](Dwc3& drv) {
    binding = fidl::BindServer(fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                               std::move(endpoints->server), &fake_dci);
    Dwc3TestHelper::BindDciInterface(drv, std::move(endpoints->client));

    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    EXPECT_NE(uep, nullptr);
    if (!uep) {
      return;
    }
    auto& ep = uep->ep;
    ep.transfer_state = Endpoint::TransferState::kActiveOngoing;
    Dwc3TestHelper::SetControllerStarted(drv, true);
    ep.rsrc_id = 1;
    uep->server->active_reqs.push(decltype(uep->server->active_reqs)::value_type{
        .request = usb::FidlRequest(
            std::move(fuchsia_hardware_usb_request::Request().defer_completion(false))),
        .total_trbs = 1,
        .completed_trbs = 0,
        .completed_bytes = 0,
    });

    // Simulate DEVT_DISCONNECT event
    Dwc3TestHelper::HandleEvent(drv, kEvtDisconnect);

    EXPECT_EQ(ep.transfer_state, Endpoint::TransferState::kCanceling);
    EXPECT_EQ(ep.rsrc_id, 1u);
  });

  dut_.runtime().RunUntil([&]() { return set_connected_called.load(); });

  EXPECT_TRUE(cmd_written.load());
  EXPECT_TRUE(set_connected_called.load());
  EXPECT_FALSE(is_connected_val.load());

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  EXPECT_EQ(dut_.StopDriver().status_value(), ZX_OK);
}

// Verifies behavior on a simulated USB reset event.
TEST_F(Dwc3EventsTest, HandleResetEvent) {
  std::atomic<bool> depcmd_written = false;
  std::atomic<bool> dcfg_written = false;
  std::atomic<uint32_t> depcmd_val = 0;
  std::atomic<uint32_t> dcfg_val = 0;

  SetUpAndPowerOnDriver();

  // Use RAII guard to guarantee register callback cleanup on scope exit regardless of assertions.
  auto cleanup_callbacks = fit::defer([&]() {
    dut_.RunInEnvironmentTypeContext([&](Environment& env) {
      env.reg_region()[DEPCMD::Get(0).addr()].SetReadCallback(nullptr);
      env.reg_region()[DEPCMD::Get(0).addr()].SetWriteCallback(nullptr);
      env.reg_region()[DCFG::Get().addr()].SetReadCallback(nullptr);
      env.reg_region()[DCFG::Get().addr()].SetWriteCallback(nullptr);
    });
  });

  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    auto& depcmd = env.reg_region()[DEPCMD::Get(0).addr()];
    auto& dcfg = env.reg_region()[DCFG::Get().addr()];

    depcmd.SetReadCallback([&]() -> uint32_t { return depcmd_val.load(); });
    depcmd.SetWriteCallback([&](uint64_t val_raw) {
      uint32_t val = static_cast<uint32_t>(val_raw);
      depcmd_written.store(true);
      depcmd_val.store(val & ~(1 << 10));
    });

    dcfg.SetReadCallback([&]() -> uint32_t { return dcfg_val.load(); });
    dcfg.SetWriteCallback([&](uint64_t val_raw) {
      uint32_t val = static_cast<uint32_t>(val_raw);
      dcfg_written.store(true);
      dcfg_val.store(val);
    });
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    // Simulate DEVT_USB_RESET event
    Dwc3TestHelper::HandleEvent(drv, kEvtUsbReset);
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
  });

  EXPECT_TRUE(depcmd_written.load());
  EXPECT_TRUE(dcfg_written.load());
  EXPECT_EQ(DCFG::Get().FromValue(dcfg_val.load()).DEVADDR(), 0u);

  EXPECT_EQ(dut_.StopDriver().status_value(), ZX_OK);
  dut_.runtime().RunUntilIdle();
}

// Verifies behavior on a simulated connection done event, including setting the speed.
TEST_F(Dwc3EventsTest, HandleConnectionDoneEvent) {
  std::atomic<bool> depcmd_written = false;
  std::atomic<bool> setspeed_called = false;
  std::atomic<uint32_t> depcmd_val = 0;
  std::atomic<fuchsia_hardware_usb_descriptor::wire::UsbSpeed> speed_passed =
      fuchsia_hardware_usb_descriptor::wire::UsbSpeed::kUndefined;

  SetUpAndPowerOnDriver();

  FakeUsbDciInterface fake_dci;
  fake_dci.SetSetSpeedCallback([&](fuchsia_hardware_usb_descriptor::wire::UsbSpeed speed) {
    setspeed_called.store(true);
    speed_passed.store(speed);
  });

  auto endpoints = fidl::CreateEndpoints<fuchsia_hardware_usb_dci::UsbDciInterface>();
  ASSERT_TRUE(endpoints.is_ok());

  // Use RAII guard to guarantee register callback cleanup on scope exit regardless of assertions.
  auto cleanup_callbacks = fit::defer([&]() {
    dut_.RunInEnvironmentTypeContext([&](Environment& env) {
      env.reg_region()[DSTS::Get().addr()].SetReadCallback(nullptr);
      env.reg_region()[DEPCMD::Get(0).addr()].SetReadCallback(nullptr);
      env.reg_region()[DEPCMD::Get(0).addr()].SetWriteCallback(nullptr);
    });
  });

  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    auto& dsts = env.reg_region()[DSTS::Get().addr()];
    auto& depcmd = env.reg_region()[DEPCMD::Get(0).addr()];

    // Mock DSTS to return CONNECTSPD_HIGH (0) and simulate successful controller halt to satisfy
    // Spec loops.
    dsts.SetReadCallback([]() -> uint32_t {
      return DSTS::Get()
          .FromValue(0)
          .set_CONNECTSPD(DSTS::CONNECTSPD_HIGH)
          .set_DEVCTRLHLT(1)
          .reg_value();
    });

    depcmd.SetReadCallback([&]() -> uint32_t { return depcmd_val.load(); });
    depcmd.SetWriteCallback([&](uint64_t val_raw) {
      uint32_t val = static_cast<uint32_t>(val_raw);
      depcmd_written.store(true);
      depcmd_val.store(val & ~(1 << 10));
    });
  });

  std::optional<fidl::ServerBindingRef<fuchsia_hardware_usb_dci::UsbDciInterface>> binding;
  dut_.RunInDriverContext([&](Dwc3& drv) {
    binding = fidl::BindServer(fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                               std::move(endpoints->server), &fake_dci);

    // Bind the client
    Dwc3TestHelper::BindDciInterface(drv, std::move(endpoints->client));

    // Simulate DEVT_CONNECTION_DONE event
    Dwc3TestHelper::HandleEvent(drv, kEvtConnectionDone);
  });

  dut_.runtime().RunUntil([&]() { return setspeed_called.load(); });

  EXPECT_TRUE(depcmd_written.load());
  EXPECT_TRUE(setspeed_called.load());
  EXPECT_EQ(speed_passed.load(), fuchsia_hardware_usb_descriptor::wire::UsbSpeed::kHigh);

  // Clean unbind: Zircon's BindServer automatically destroys the server unique_ptr on the
  // dispatcher thread post-unbind!
  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  EXPECT_EQ(dut_.StopDriver().status_value(), ZX_OK);
  dut_.runtime().RunUntilIdle();
}

// Verifies that injecting an unknown event does not crash the driver.
TEST_F(Dwc3EventsTest, HandleUnknownEvent) {
  SetUpAndPowerOnDriver();

  dut_.RunInDriverContext([&](Dwc3& drv) {
    // Inject an unassigned device event vector (type = 31, which is unhandled in dwc3-events.cc)
    uint32_t event = MakeDeviceEvent(31);

    Dwc3TestHelper::HandleEvent(drv, event);
  });
  // Verify that it doesn't crash!
  EXPECT_EQ(dut_.StopDriver().status_value(), ZX_OK);
}

// TODO(https://fxbug.dev/538237092): Re-enable once UsbDciInterface::Reset() is added.
// A USB Bus Reset (DEVT_USB_RESET) is an in-band bus signal that resets the device to
// the Default State without dropping physical D+/D- pull-ups. SetConnected(false)
// represents a physical cable unplug and must NOT be called on a bus reset, as doing
// so corrupts usb-peripheral state during host re-enumeration.
//
// Under b/538237092, we are adding a dedicated Reset() method to UsbDciInterface
// (DCI -> usb-peripheral) so DCI can notify usb-peripheral of in-band resets while
// preserving physical connection state (kHostConnected) and cleanly unconfiguring
// active functions via SetConfigured(false).
TEST_F(Dwc3EventsTest, DISABLED_VerifySetConnectedIsNotCalledOnHandleResetEvent) {
  SetUpAndPowerOnDriver();

  FakeUsbDciInterface fake_dci;
  auto endpoints = fidl::CreateEndpoints<fuchsia_hardware_usb_dci::UsbDciInterface>();
  ASSERT_TRUE(endpoints.is_ok());

  std::optional<fidl::ServerBindingRef<fuchsia_hardware_usb_dci::UsbDciInterface>> binding;
  dut_.RunInDriverContext([&](Dwc3& drv) {
    binding = fidl::BindServer(fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                               std::move(endpoints->server), &fake_dci);

    // Bind the mock client
    Dwc3TestHelper::BindDciInterface(drv, std::move(endpoints->client));

    // USB Reset (DEVT_USB_RESET) is an in-band bus reset signal received from the host while the
    // device remains physically attached.
    Dwc3TestHelper::HandleEvent(drv, kEvtUsbReset);
  });

  dut_.runtime().RunUntilIdle();

  // 1. Positive Assertion: Verify that SetSpeed WAS invoked on UsbDciInterface to notify
  // DCI of the post-reset link speed re-negotiation.
  EXPECT_TRUE(fake_dci.set_speed_called());

  // 2. Negative Assertion: Verify that SetConnected(false) was NEVER called, preserving physical
  // cable attachment state.
  EXPECT_FALSE(fake_dci.set_connected_called());

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  EXPECT_EQ(dut_.StopDriver().status_value(), ZX_OK);
}

}  // namespace dwc3
