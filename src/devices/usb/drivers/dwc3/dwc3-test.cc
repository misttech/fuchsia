// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/dwc3/dwc3.h"

#include <fidl/fuchsia.hardware.clock/cpp/test_base.h>
#include <fidl/fuchsia.hardware.interconnect/cpp/fidl.h>
#include <fidl/fuchsia.hardware.platform.device/cpp/fidl.h>
#include <fidl/fuchsia.hardware.reset/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.dci/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.endpoint/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.phy/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.request/cpp/fidl.h>
#include <fidl/fuchsia.hardware.vreg/cpp/test_base.h>
#include <lib/driver/fake-clock/cpp/fake-clock.h>
#include <lib/driver/fake-reset/cpp/fake-reset.h>
#include <lib/driver/fake-vreg/cpp/fake-vreg.h>
#include <lib/fpromise/single_threaded_executor.h>
#include <lib/inspect/cpp/hierarchy.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/inspect/cpp/reader.h>
#include <lib/sync/cpp/completion.h>

#include <atomic>
#include <optional>
#include <set>

#include <fake-mmio-reg/fake-mmio-reg.h>
#include <gtest/gtest.h>
#include <usb/descriptors.h>

namespace fdescriptor = fuchsia_hardware_usb_descriptor;

#include "lib/driver/fake-platform-device/cpp/fake-pdev.h"
#include "lib/driver/testing/cpp/driver_test.h"
#include "src/devices/usb/drivers/dwc3/dwc3-regs.h"
#include "src/devices/usb/drivers/dwc3/dwc3-test-fixture.h"
#include "src/devices/usb/drivers/dwc3/dwc3_config.h"
#include "src/lib/testing/predicates/status.h"

namespace dwc3 {

TEST_F(ManagedTestFixture, Dfv2Lifecycle) {
  dut_.RunInNodeContext(
      [&](fdf_testing::TestNode& node) { EXPECT_EQ(node.children().size(), 1UL); });
}

TEST_F(UnmanagedTestFixture, ResourcesManagedInStart) {
  dut_.RunInEnvironmentTypeContext(
      [](Environment& env) { env.usb_phy().set_connection_status_observer_called(true); });

  zx::result start = dut_.StartDriverWithCustomStartArgs([](fdf::DriverStartArgs& args) {
    dwc3_config::Config cfg;
    cfg.enable_suspend() = false;
    cfg.bypass_platform_extension() = false;
    args.config(cfg.ToVmo());
  });
  ASSERT_TRUE(start.is_ok());

  dut_.RunInEnvironmentTypeContext([](Environment& env) {
    EXPECT_TRUE(env.vreg().enabled());
    EXPECT_FALSE(env.reset().take_toggled());
    EXPECT_TRUE(env.clock_xo().enabled());
    EXPECT_TRUE(env.clock_sleep().enabled());
    EXPECT_TRUE(env.clock_iface().enabled());
    EXPECT_TRUE(env.clock_core().enabled());
    EXPECT_TRUE(env.clock_utmi().enabled());
    EXPECT_TRUE(env.clock_bus_aggr().enabled());
  });

  dut_.runtime().RunUntilIdle();
  EXPECT_EQ(WaitForPhy(), ZX_OK);

  EXPECT_EQ(dut_.StopDriver().status_value(), ZX_OK);
}

TEST_F(UnmanagedTestFixture, PlatformExtensionBypass) {
  // Verify that when bypass_platform_extension is true, the platform extension is not created,
  // preventing it from making calls to external environment mocks during unit tests.
  dut_.RunInEnvironmentTypeContext(
      [](Environment& env) { env.usb_phy().set_connection_status_observer_called(true); });

  zx::result start = dut_.StartDriverWithCustomStartArgs([](fdf::DriverStartArgs& args) {
    dwc3_config::Config cfg;
    cfg.enable_suspend() = false;
    cfg.bypass_platform_extension() = true;
    args.config(cfg.ToVmo());
  });
  ASSERT_TRUE(start.is_ok());

  dut_.RunInDriverContext(
      [this](Dwc3& drv) { EXPECT_EQ(this->GetPlatformExtension(drv), nullptr); });

  EXPECT_EQ(dut_.StopDriver().status_value(), ZX_OK);
}

TEST_F(UnmanagedTestFixture, ConnectResetsHardwareWithoutPlatformExtension) {
  auto reset_count = std::make_shared<std::atomic<uint32_t>>(0);
  dut_.RunInEnvironmentTypeContext([this, reset_count](Environment& env) {
    auto& dctl_reg = env.reg_region()[DCTL::Get().addr()];
    dctl_reg.SetWriteCallback([this, reset_count](uint64_t val) {
      if (DCTL::Get().FromValue(static_cast<uint32_t>(val)).CSFTRST() == 1) {
        (*reset_count)++;
      }
      Write_DCTL(static_cast<uint32_t>(val));
    });
  });

  zx::result start = dut_.StartDriverWithCustomStartArgs([](fdf::DriverStartArgs& args) {
    dwc3_config::Config cfg;
    cfg.enable_suspend() = false;
    cfg.bypass_platform_extension() = true;
    args.config(cfg.ToVmo());
  });
  ASSERT_TRUE(start.is_ok());

  dut_.runtime().RunUntilIdle();
  EXPECT_EQ(ZX_OK, WaitForPhy());

  // Init() performs 1 reset at boot, and initial disconnected event performs 1 reset (total 2).
  EXPECT_EQ(2u, reset_count->load());

  dut_.RunInEnvironmentTypeContext([](Environment& env) { env.usb_phy().completion()->Reset(); });

  // Trigger connect event: should trigger ResetHw() when transitioning from !power_on_
  dut_.RunInEnvironmentTypeContext([](Environment& env) { env.usb_phy().TriggerConnection(true); });
  dut_.runtime().RunUntilIdle();
  EXPECT_EQ(ZX_OK, WaitForPhy());
  EXPECT_EQ(3u, reset_count->load());

  EXPECT_EQ(ZX_OK, dut_.StopDriver().status_value());
}

TEST_F(UnmanagedTestFixture, RedundantConnectSkipsHardwareResetWithoutPlatformExtension) {
  auto reset_count = std::make_shared<std::atomic<uint32_t>>(0);
  dut_.RunInEnvironmentTypeContext([this, reset_count](Environment& env) {
    env.usb_phy().set_initial_connected(true);
    auto& dctl_reg = env.reg_region()[DCTL::Get().addr()];
    dctl_reg.SetWriteCallback([this, reset_count](uint64_t val) {
      if (DCTL::Get().FromValue(static_cast<uint32_t>(val)).CSFTRST() == 1) {
        (*reset_count)++;
      }
      Write_DCTL(static_cast<uint32_t>(val));
    });
  });

  zx::result start = dut_.StartDriverWithCustomStartArgs([](fdf::DriverStartArgs& args) {
    dwc3_config::Config cfg;
    cfg.enable_suspend() = false;
    cfg.bypass_platform_extension() = true;
    args.config(cfg.ToVmo());
  });
  ASSERT_TRUE(start.is_ok());

  dut_.runtime().RunUntilIdle();
  EXPECT_EQ(ZX_OK, WaitForPhy());

  // Init() performs 1 reset; initial connected event skips reset since power_on_ is already true.
  EXPECT_EQ(1u, reset_count->load());

  dut_.RunInEnvironmentTypeContext([](Environment& env) { env.usb_phy().completion()->Reset(); });

  // A redundant connect event while already in powered state should not trigger a hardware reset.
  dut_.RunInEnvironmentTypeContext([](Environment& env) { env.usb_phy().TriggerConnection(true); });
  dut_.runtime().RunUntilIdle();
  EXPECT_EQ(ZX_OK, WaitForPhy());
  EXPECT_EQ(1u, reset_count->load());

  EXPECT_EQ(ZX_OK, dut_.StopDriver().status_value());
}

TEST_F(UnmanagedTestFixture, DisconnectResetsHardwareWithoutPlatformExtension) {
  auto reset_count = std::make_shared<std::atomic<uint32_t>>(0);
  dut_.RunInEnvironmentTypeContext([this, reset_count](Environment& env) {
    env.usb_phy().set_initial_connected(true);
    auto& dctl_reg = env.reg_region()[DCTL::Get().addr()];
    dctl_reg.SetWriteCallback([this, reset_count](uint64_t val) {
      if (DCTL::Get().FromValue(static_cast<uint32_t>(val)).CSFTRST() == 1) {
        (*reset_count)++;
      }
      Write_DCTL(static_cast<uint32_t>(val));
    });
  });

  zx::result start = dut_.StartDriverWithCustomStartArgs([](fdf::DriverStartArgs& args) {
    dwc3_config::Config cfg;
    cfg.enable_suspend() = false;
    cfg.bypass_platform_extension() = true;
    args.config(cfg.ToVmo());
  });
  ASSERT_TRUE(start.is_ok());

  dut_.runtime().RunUntilIdle();
  EXPECT_EQ(ZX_OK, WaitForPhy());

  // Init() performs 1 reset; initial connected event skips redundant reset since power_on_ is
  // already true.
  EXPECT_EQ(1u, reset_count->load());

  dut_.RunInEnvironmentTypeContext([](Environment& env) { env.usb_phy().completion()->Reset(); });

  // Trigger disconnect event: should trigger ResetHw() on disconnect
  dut_.RunInEnvironmentTypeContext(
      [](Environment& env) { env.usb_phy().TriggerConnection(false); });
  dut_.runtime().RunUntilIdle();
  EXPECT_EQ(ZX_OK, WaitForPhy());
  EXPECT_EQ(2u, reset_count->load());

  EXPECT_EQ(ZX_OK, dut_.StopDriver().status_value());
}

TEST_F(UnmanagedTestFixture, HotplugCycleResetsHardwareWithoutPlatformExtension) {
  auto reset_count = std::make_shared<std::atomic<uint32_t>>(0);
  dut_.RunInEnvironmentTypeContext([this, reset_count](Environment& env) {
    auto& dctl_reg = env.reg_region()[DCTL::Get().addr()];
    dctl_reg.SetWriteCallback([this, reset_count](uint64_t val) {
      if (DCTL::Get().FromValue(static_cast<uint32_t>(val)).CSFTRST() == 1) {
        (*reset_count)++;
      }
      Write_DCTL(static_cast<uint32_t>(val));
    });
  });

  zx::result start = dut_.StartDriverWithCustomStartArgs([](fdf::DriverStartArgs& args) {
    dwc3_config::Config cfg;
    cfg.enable_suspend() = false;
    cfg.bypass_platform_extension() = true;
    args.config(cfg.ToVmo());
  });
  ASSERT_TRUE(start.is_ok());

  dut_.runtime().RunUntilIdle();
  EXPECT_EQ(ZX_OK, WaitForPhy());

  // Init() reset (1) + initial disconnected reset (1) = 2.
  EXPECT_EQ(2u, reset_count->load());

  dut_.RunInEnvironmentTypeContext([](Environment& env) { env.usb_phy().completion()->Reset(); });

  // 1. Initial connect
  dut_.RunInEnvironmentTypeContext([](Environment& env) { env.usb_phy().TriggerConnection(true); });
  dut_.runtime().RunUntilIdle();
  EXPECT_EQ(ZX_OK, WaitForPhy());
  EXPECT_EQ(3u, reset_count->load());

  dut_.RunInEnvironmentTypeContext([](Environment& env) { env.usb_phy().completion()->Reset(); });

  // 2. Disconnect (unplug)
  dut_.RunInEnvironmentTypeContext(
      [](Environment& env) { env.usb_phy().TriggerConnection(false); });
  dut_.runtime().RunUntilIdle();
  EXPECT_EQ(ZX_OK, WaitForPhy());
  EXPECT_EQ(4u, reset_count->load());

  dut_.RunInEnvironmentTypeContext([](Environment& env) { env.usb_phy().completion()->Reset(); });

  // 3. Reconnect (replug)
  dut_.RunInEnvironmentTypeContext([](Environment& env) { env.usb_phy().TriggerConnection(true); });
  dut_.runtime().RunUntilIdle();
  EXPECT_EQ(ZX_OK, WaitForPhy());
  EXPECT_EQ(5u, reset_count->load());

  EXPECT_EQ(ZX_OK, dut_.StopDriver().status_value());
}

// Verifies that when the device is disconnected and no client has started the controller,
// evaluating Inspect lazy nodes avoids accessing hardware MMIO registers (which may be
// unpowered or clock-gated) and omits hardware register nodes from the Inspect tree.
TEST_F(UnmanagedTestFixture, InspectSkipsMmioWhenPowerOffAndControllerStopped) {
  auto gctl_read_count = InterceptGctlReads();

  ASSERT_OK(StartDriverWithoutPlatformExtension());

  dut_.runtime().RunUntilIdle();
  EXPECT_EQ(ZX_OK, WaitForPhy());

  // Combination 1: power_on_ = false, controller_started_ = false.
  dut_.RunInEnvironmentTypeContext([](Environment& env) {
    env.usb_phy().completion()->Reset();
    env.usb_phy().TriggerConnection(false);
  });
  dut_.runtime().RunUntilIdle();
  EXPECT_EQ(ZX_OK, WaitForPhy());

  gctl_read_count->store(0);

  inspect::Hierarchy hierarchy;
  dut_.RunInDriverContext([&](Dwc3& drv) {
    hierarchy =
        fpromise::run_single_threaded(inspect::ReadFromInspector(drv.inspector().inspector()))
            .take_value();
  });
  dut_.runtime().RunUntilIdle();

  const auto* dwc3_node = hierarchy.GetByPath({"dwc3"});
  ASSERT_NE(nullptr, dwc3_node);
  EXPECT_NE(nullptr, dwc3_node->node().get_property<inspect::UintPropertyValue>("time_start"));

  const auto* hw_state =
      dwc3_node->node().get_property<inspect::StringPropertyValue>("hardware_state");
  ASSERT_NE(nullptr, hw_state);
  EXPECT_EQ("powered_off", hw_state->value());

  EXPECT_EQ(nullptr, dwc3_node->GetByPath({"GCTL"}));
  EXPECT_EQ(nullptr, dwc3_node->GetByPath({"GSTS"}));
  EXPECT_EQ(nullptr, dwc3_node->GetByPath({"DCFG"}));
  EXPECT_EQ(nullptr, dwc3_node->GetByPath({"DCTL"}));
  EXPECT_EQ(nullptr, dwc3_node->GetByPath({"DSTS"}));
  EXPECT_EQ(0u, gctl_read_count->load());

  EXPECT_EQ(ZX_OK, dut_.StopDriver().status_value());
}

// Verifies that when PHY power is on (e.g. cable attached or default startup state) but no
// client has started the controller (e.g. peripheral function drivers are not bound),
// Inspect queries skip reading hardware registers to prevent bus faults on idle systems.
TEST_F(UnmanagedTestFixture, InspectSkipsMmioWhenPowerOnAndControllerStopped) {
  auto gctl_read_count = InterceptGctlReads();

  ASSERT_OK(StartDriverWithoutPlatformExtension());

  dut_.runtime().RunUntilIdle();
  EXPECT_EQ(ZX_OK, WaitForPhy());

  // Combination 2: power_on_ = true, controller_started_ = false.
  dut_.RunInEnvironmentTypeContext([](Environment& env) {
    env.usb_phy().completion()->Reset();
    env.usb_phy().TriggerConnection(true);
  });
  dut_.runtime().RunUntilIdle();
  EXPECT_EQ(ZX_OK, WaitForPhy());

  gctl_read_count->store(0);

  inspect::Hierarchy hierarchy;
  dut_.RunInDriverContext([&](Dwc3& drv) {
    hierarchy =
        fpromise::run_single_threaded(inspect::ReadFromInspector(drv.inspector().inspector()))
            .take_value();
  });
  dut_.runtime().RunUntilIdle();

  const auto* dwc3_node = hierarchy.GetByPath({"dwc3"});
  ASSERT_NE(nullptr, dwc3_node);
  EXPECT_NE(nullptr, dwc3_node->node().get_property<inspect::UintPropertyValue>("time_start"));

  const auto* hw_state =
      dwc3_node->node().get_property<inspect::StringPropertyValue>("hardware_state");
  ASSERT_NE(nullptr, hw_state);
  EXPECT_EQ("inactive", hw_state->value());

  EXPECT_EQ(nullptr, dwc3_node->GetByPath({"GCTL"}));
  EXPECT_EQ(nullptr, dwc3_node->GetByPath({"GSTS"}));
  EXPECT_EQ(nullptr, dwc3_node->GetByPath({"DCFG"}));
  EXPECT_EQ(nullptr, dwc3_node->GetByPath({"DCTL"}));
  EXPECT_EQ(nullptr, dwc3_node->GetByPath({"DSTS"}));
  EXPECT_EQ(0u, gctl_read_count->load());

  EXPECT_EQ(ZX_OK, dut_.StopDriver().status_value());
}

// Verifies that when the controller was previously started by a client but the cable has
// been unplugged (PHY power off), Inspect queries skip hardware MMIO reads to protect against
// accessing unpowered or halted controller registers.
TEST_F(UnmanagedTestFixture, InspectSkipsMmioWhenPowerOffAndControllerStarted) {
  auto gctl_read_count = InterceptGctlReads();

  ASSERT_OK(StartDriverWithoutPlatformExtension());

  dut_.runtime().RunUntilIdle();
  EXPECT_EQ(ZX_OK, WaitForPhy());

  auto dci_service = dut_.Connect<fuchsia_hardware_usb_dci::UsbDciService::Device>();
  ASSERT_TRUE(dci_service.is_ok()) << dci_service.status_string();
  fidl::WireSyncClient<fuchsia_hardware_usb_dci::UsbDci> dci{std::move(*dci_service)};
  ASSERT_OK(dci->StartController().status());

  // Trigger plug in first:
  dut_.RunInEnvironmentTypeContext([](Environment& env) {
    env.usb_phy().completion()->Reset();
    env.usb_phy().TriggerConnection(true);
  });
  dut_.runtime().RunUntilIdle();
  EXPECT_EQ(ZX_OK, WaitForPhy());

  // Combination 3: power_on_ = false, controller_started_ = true.
  dut_.RunInEnvironmentTypeContext([](Environment& env) {
    env.usb_phy().completion()->Reset();
    env.usb_phy().TriggerConnection(false);
  });
  dut_.runtime().RunUntilIdle();
  EXPECT_EQ(ZX_OK, WaitForPhy());

  // Reset counter to observe only the subsequent Inspect query.
  gctl_read_count->store(0);

  inspect::Hierarchy hierarchy;
  dut_.RunInDriverContext([&](Dwc3& drv) {
    hierarchy =
        fpromise::run_single_threaded(inspect::ReadFromInspector(drv.inspector().inspector()))
            .take_value();
  });
  dut_.runtime().RunUntilIdle();

  const auto* dwc3_node = hierarchy.GetByPath({"dwc3"});
  ASSERT_NE(nullptr, dwc3_node);
  EXPECT_NE(nullptr, dwc3_node->node().get_property<inspect::UintPropertyValue>("time_start"));

  const auto* hw_state =
      dwc3_node->node().get_property<inspect::StringPropertyValue>("hardware_state");
  ASSERT_NE(nullptr, hw_state);
  EXPECT_EQ("powered_off", hw_state->value());

  EXPECT_EQ(nullptr, dwc3_node->GetByPath({"GCTL"}));
  EXPECT_EQ(nullptr, dwc3_node->GetByPath({"GSTS"}));
  EXPECT_EQ(nullptr, dwc3_node->GetByPath({"DCFG"}));
  EXPECT_EQ(nullptr, dwc3_node->GetByPath({"DCTL"}));
  EXPECT_EQ(nullptr, dwc3_node->GetByPath({"DSTS"}));
  EXPECT_EQ(0u, gctl_read_count->load());

  EXPECT_EQ(ZX_OK, dut_.StopDriver().status_value());
}

// Verifies that when the controller is actively running in peripheral mode with PHY power on
// and a client started, Inspect queries successfully sample hardware registers (GCTL, GSTS,
// DCFG) and record their decoded fields into the Inspect hierarchy.
TEST_F(UnmanagedTestFixture, InspectSamplesMmioWhenPowerOnAndControllerStarted) {
  auto gctl_read_count = InterceptGctlReads();

  ASSERT_OK(StartDriverWithoutPlatformExtension());

  dut_.runtime().RunUntilIdle();
  EXPECT_EQ(ZX_OK, WaitForPhy());

  auto dci_service = dut_.Connect<fuchsia_hardware_usb_dci::UsbDciService::Device>();
  ASSERT_TRUE(dci_service.is_ok()) << dci_service.status_string();
  fidl::WireSyncClient<fuchsia_hardware_usb_dci::UsbDci> dci{std::move(*dci_service)};
  ASSERT_OK(dci->StartController().status());

  // Combination 4: power_on_ = true, controller_started_ = true.
  dut_.RunInEnvironmentTypeContext([](Environment& env) {
    env.usb_phy().completion()->Reset();
    env.usb_phy().TriggerConnection(true);
  });
  dut_.runtime().RunUntilIdle();
  EXPECT_EQ(ZX_OK, WaitForPhy());

  gctl_read_count->store(0);

  inspect::Hierarchy hierarchy;
  dut_.RunInDriverContext([&](Dwc3& drv) {
    hierarchy =
        fpromise::run_single_threaded(inspect::ReadFromInspector(drv.inspector().inspector()))
            .take_value();
  });
  dut_.runtime().RunUntilIdle();

  const auto* dwc3_node = hierarchy.GetByPath({"dwc3"});
  ASSERT_NE(nullptr, dwc3_node);
  EXPECT_NE(nullptr, dwc3_node->node().get_property<inspect::UintPropertyValue>("time_start"));
  EXPECT_EQ(nullptr,
            dwc3_node->node().get_property<inspect::StringPropertyValue>("hardware_state"));
  EXPECT_NE(nullptr, dwc3_node->GetByPath({"GCTL"}));
  EXPECT_NE(nullptr, dwc3_node->GetByPath({"GSTS"}));
  EXPECT_NE(nullptr, dwc3_node->GetByPath({"DCFG"}));
  EXPECT_NE(nullptr, dwc3_node->GetByPath({"DCTL"}));
  EXPECT_NE(nullptr, dwc3_node->GetByPath({"DSTS"}));
  EXPECT_GT(gctl_read_count->load(), 0u);

  EXPECT_EQ(ZX_OK, dut_.StopDriver().status_value());
}

TEST_F(UnmanagedTestFixture, Dfv2HwResetTimeout) {
  stuck_reset_test_.store(true);
  dut_.RunInEnvironmentTypeContext(
      [](Environment& env) { env.usb_phy().set_expect_connection_status_observer_call(false); });
  zx::result start = dut_.StartDriverWithCustomStartArgs([](fdf::DriverStartArgs& args) {
    dwc3_config::Config cfg;
    cfg.enable_suspend() = false;
    cfg.bypass_platform_extension() = true;
    args.config(cfg.ToVmo());
  });
  ASSERT_TRUE(start.is_error());
  ASSERT_EQ(start.error_value(), ZX_ERR_TIMED_OUT);

  dut_.RunInNodeContext(
      [&](fdf_testing::TestNode& node) { EXPECT_EQ(node.children().size(), 0UL); });

  // The dfv2 driver did not start, nothing to stop.
}

TEST_F(ManagedTestFixture, TestInspectMetrics) {
  namespace fdescriptor = fuchsia_hardware_usb_descriptor;
  const uint8_t ep_num = UsbAddressToEpNum(0x02);

  // Dynamic cable connection triggers automatic core wake-up and soft reset.
  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    // Mock GHWPARAMS0 to return MDWIDTH = 2 (128-bit = 16 bytes)
    auto& ghwparams0 = env.reg_region()[GHWPARAMS0::Get().addr()];
    ghwparams0.SetReadCallback([]() -> uint32_t {
      return GHWPARAMS0::Get().FromValue(0).set_DWC_USB31_MDWIDTH(2).reg_value();
    });

    // Mock GRXFIFOSIZ for FIFO 0 to have depth 64 (1024 bytes)
    auto& grxfifosiz0 = env.reg_region()[GRXFIFOSIZ::Get(0).addr()];
    grxfifosiz0.SetReadCallback(
        []() -> uint32_t { return GRXFIFOSIZ::Get(0).FromValue(0).set_RXFDEP(64).reg_value(); });
  });
  TriggerConnectionPlugIn(fdescriptor::UsbSpeed::kSuper);

  auto dci_service = dut_.Connect<fuchsia_hardware_usb_dci::UsbDciService::Device>();
  ASSERT_TRUE(dci_service.is_ok())
      << "Failed to connect to UsbDciService: " << dci_service.status_string();
  fidl::WireSyncClient<fuchsia_hardware_usb_dci::UsbDci> dci{std::move(*dci_service)};

  fdescriptor::wire::UsbEndpointDescriptor ep_desc{
      .b_length = sizeof(fdescriptor::wire::UsbEndpointDescriptor),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
      .b_endpoint_address = 0x02,  // EP 2 OUT
      .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
      .w_max_packet_size = 1024,
      .b_interval = 0,
  };
  fdescriptor::wire::UsbSsEpCompDescriptor ss_comp_desc{
      .b_length = sizeof(fdescriptor::wire::UsbSsEpCompDescriptor),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kSsEpCompanion),
      .b_max_burst = 0,
      .bm_attributes = 0,
      .w_bytes_per_interval = 0,
  };

  auto config_res = dci->ConfigureEndpoint(ep_desc, ss_comp_desc);
  ASSERT_TRUE(config_res.ok()) << "ConfigureEndpoint transport failed: "
                               << config_res.status_string();
  ASSERT_TRUE(config_res.value().is_ok()) << "ConfigureEndpoint protocol failed: "
                                          << zx_status_get_string(config_res.value().error_value());

  auto endpoints = fidl::CreateEndpoints<fuchsia_hardware_usb_endpoint::Endpoint>();
  ASSERT_TRUE(endpoints.is_ok()) << "Failed to create Endpoint channel endpoints: "
                                 << endpoints.status_string();
  auto [client_end, server_end] = std::move(*endpoints);

  auto conn_res = dci->ConnectToEndpoint(0x02, std::move(server_end));
  ASSERT_TRUE(conn_res.ok()) << "ConnectToEndpoint transport failed: " << conn_res.status_string();
  ASSERT_TRUE(conn_res.value().is_ok()) << "ConnectToEndpoint protocol failed: "
                                        << zx_status_get_string(conn_res.value().error_value());

  fidl::WireSyncClient<fuchsia_hardware_usb_endpoint::Endpoint> ep_client{std::move(client_end)};

  fidl::Arena arena;
  fidl::VectorView<fuchsia_hardware_usb_endpoint::wire::VmoInfo> vmo_infos(arena, 1);
  vmo_infos[0] =
      fuchsia_hardware_usb_endpoint::wire::VmoInfo::Builder(arena).id(1).size(4096).Build();

  auto reg_res = ep_client->RegisterVmos(vmo_infos);
  ASSERT_TRUE(reg_res.ok()) << "RegisterVmos transport failed: " << reg_res.status_string();

  fidl::VectorView<fuchsia_hardware_usb_request::wire::BufferRegion> regions(arena, 1);
  regions[0] = fuchsia_hardware_usb_request::wire::BufferRegion::Builder(arena)
                   .buffer(fuchsia_hardware_usb_request::wire::Buffer::WithVmoId(arena, 1))
                   .offset(0)
                   .size(1024)
                   .Build();

  auto req_info = fuchsia_hardware_usb_request::wire::RequestInfo::WithBulk(
      arena, fuchsia_hardware_usb_request::wire::BulkRequestInfo::Builder(arena).Build());

  fidl::VectorView<fuchsia_hardware_usb_request::wire::Request> reqs(arena, 1);
  reqs[0] = fuchsia_hardware_usb_request::wire::Request::Builder(arena)
                .data(regions)
                .defer_completion(false)
                .information(req_info)
                .Build();

  auto queue_res = ep_client->QueueRequests(reqs);
  ASSERT_TRUE(queue_res.ok()) << "QueueRequests transport failed: " << queue_res.status_string();

  // Synchronize one-way QueueRequests by calling two-way GetInfo.
  auto info_res = ep_client->GetInfo();
  ASSERT_TRUE(info_res.ok()) << "GetInfo failed: " << info_res.status_string();

  dut_.runtime().RunUntilIdle();

  // Trigger hardware interrupts to process queueing.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    TriggerConnectionDone(drv);
    TriggerEpTransferNotReady(drv, ep_num, 0);
  });
  dut_.runtime().RunUntilIdle();

  // Verify metrics during pending transfer (active TRB node exists).
  inspect::Hierarchy pending_hierarchy;
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 1u);

    // Assign hardware Resource ID 1 to the active transfer to prevent teardown panics.
    TriggerEpTransferStarted(drv, ep_num, 1);

    pending_hierarchy =
        fpromise::run_single_threaded(inspect::ReadFromInspector(drv.inspector().inspector()))
            .take_value();
  });

  const auto* pending_dwc3 = pending_hierarchy.GetByPath({"dwc3"});
  ASSERT_NE(pending_dwc3, nullptr)
      << "Inspect root node 'dwc3' was not found in pending hierarchy!";

  std::string ep_node_name = std::format("endpoint-0x{:02x}", ep_num);

  const auto* pending_active_trbs =
      pending_dwc3->GetByPath({"endpoints", ep_node_name, "trb_fifo", "active_trbs"});
  ASSERT_NE(pending_active_trbs, nullptr) << "Active TRBs node was not found in pending hierarchy!";
  const auto* trb0 = pending_active_trbs->GetByPath({"0"});
  ASSERT_NE(trb0, nullptr) << "Pending TRB at index 0 was not found!";

  // Assert existence of TRB fields and check value of hardware_owned.
  const auto* ptr_low = trb0->node().get_property<inspect::UintPropertyValue>("ptr_low");
  ASSERT_NE(ptr_low, nullptr) << "Pending TRB field 'ptr_low' was not found!";
  const auto* ptr_high = trb0->node().get_property<inspect::UintPropertyValue>("ptr_high");
  ASSERT_NE(ptr_high, nullptr) << "Pending TRB field 'ptr_high' was not found!";
  const auto* status = trb0->node().get_property<inspect::UintPropertyValue>("status");
  ASSERT_NE(status, nullptr) << "Pending TRB field 'status' was not found!";
  const auto* control = trb0->node().get_property<inspect::UintPropertyValue>("control");
  ASSERT_NE(control, nullptr) << "Pending TRB field 'control' was not found!";

  const auto* hwo = trb0->node().get_property<inspect::BoolPropertyValue>("hardware_owned");
  ASSERT_NE(hwo, nullptr) << "Pending TRB field 'hardware_owned' was not found!";
  EXPECT_TRUE(hwo->value());

  // Complete the transfer and evaluate final metrics.
  inspect::Hierarchy completed_hierarchy;
  dut_.RunInDriverContext([&](Dwc3& drv) {
    TriggerEpTransferComplete(drv, ep_num);
    completed_hierarchy =
        fpromise::run_single_threaded(inspect::ReadFromInspector(drv.inspector().inspector()))
            .take_value();
  });
  dut_.runtime().RunUntilIdle();

  const auto* dwc3_node = completed_hierarchy.GetByPath({"dwc3"});
  ASSERT_NE(dwc3_node, nullptr) << "Inspect root node 'dwc3' was not found in completed hierarchy!";

  const auto* time_start = dwc3_node->node().get_property<inspect::UintPropertyValue>("time_start");
  ASSERT_NE(time_start, nullptr) << "Root node property 'time_start' was not found!";

  const auto* history_node = dwc3_node->GetByPath({"event_history"});
  ASSERT_NE(history_node, nullptr) << "Inspect node 'event_history' was not found!";

  bool found_real_event = false;
  for (const auto& child : history_node->children()) {
    const auto* event_msg = child.node().get_property<inspect::StringPropertyValue>("event");
    if (event_msg != nullptr &&
        event_msg->value().find("USB Connection Done") != std::string::npos) {
      found_real_event = true;
      break;
    }
  }
  EXPECT_TRUE(found_real_event);

  const auto* endpoints_node = dwc3_node->GetByPath({"endpoints"});
  ASSERT_NE(endpoints_node, nullptr) << "Inspect node 'endpoints' was not found!";

  const auto* ep2_out = endpoints_node->GetByPath({ep_node_name});
  ASSERT_NE(ep2_out, nullptr) << "Endpoint node '" << ep_node_name
                              << "' was not found in completed hierarchy!";

  const auto* type = ep2_out->node().get_property<inspect::UintPropertyValue>("type");
  ASSERT_NE(type, nullptr) << "Endpoint 'type' property was not found!";
  EXPECT_EQ(type->value(),
            static_cast<uint64_t>(fidl::ToUnderlying(fdescriptor::EndpointType::kBulk)));

  const auto* enabled = ep2_out->node().get_property<inspect::BoolPropertyValue>("enabled");
  ASSERT_NE(enabled, nullptr) << "Endpoint 'enabled' property was not found!";
  EXPECT_TRUE(enabled->value());

  const auto* transfers =
      ep2_out->node().get_property<inspect::UintPropertyValue>("total_transfers");
  ASSERT_NE(transfers, nullptr) << "Endpoint 'total_transfers' property was not found!";
  EXPECT_EQ(transfers->value(), 1u);

  const auto* bytes = ep2_out->node().get_property<inspect::UintPropertyValue>("total_bytes");
  ASSERT_NE(bytes, nullptr) << "Endpoint 'total_bytes' property was not found!";
  EXPECT_EQ(bytes->value(), 1024u);

  const auto* fifo_node = ep2_out->GetByPath({"trb_fifo"});
  ASSERT_NE(fifo_node, nullptr) << "TRB FIFO inspect node 'trb_fifo' was not found!";

  const auto* total_slots =
      fifo_node->node().get_property<inspect::UintPropertyValue>("total_slots");
  ASSERT_NE(total_slots, nullptr) << "FIFO property 'total_slots' was not found!";
  EXPECT_GT(total_slots->value(), 0u);

  const auto* active_trbs = fifo_node->GetByPath({"active_trbs"});
  EXPECT_EQ(active_trbs, nullptr);
}

TEST_F(ManagedTestFixture, TestStopEventsMasksInterrupts) {
  auto evntintrptmask = std::make_shared<std::atomic<uint32_t>>(0);
  auto gevntcount = std::make_shared<std::atomic<uint32_t>>(0);
  auto gevntcount_reads = std::make_shared<std::atomic<uint32_t>>(0);

  // Use RAII guard to guarantee register callback cleanup on scope exit regardless of assertions.
  auto cleanup_callbacks = fit::defer([&]() {
    dut_.RunInEnvironmentTypeContext([](Environment& env) {
      env.reg_region()[GEVNTSIZ::Get(0).addr()].SetWriteCallback([](uint64_t) {});
      env.reg_region()[GEVNTCOUNT::Get(0).addr()].SetReadCallback([]() -> uint32_t { return 0; });
      env.reg_region()[GEVNTCOUNT::Get(0).addr()].SetWriteCallback([](uint64_t) {});
    });
  });

  dut_.RunInEnvironmentTypeContext(
      [evntintrptmask, gevntcount, gevntcount_reads](Environment& env) {
        auto& gevntsiz = env.reg_region()[GEVNTSIZ::Get(0).addr()];
        gevntsiz.SetWriteCallback([evntintrptmask](uint64_t val) {
          evntintrptmask->store(
              GEVNTSIZ::Get(0).FromValue(static_cast<uint32_t>(val)).EVNTINTRPTMASK());
        });

        auto& gevntcount_reg = env.reg_region()[GEVNTCOUNT::Get(0).addr()];
        gevntcount_reg.SetReadCallback([gevntcount_reads]() -> uint32_t {
          // Simulate 32 bytes of pending events.
          if (gevntcount_reads->fetch_add(1) == 0) {
            return 32;
          }
          return 0;  // Return 0 on subsequent reads to exit while loop.
        });
        gevntcount_reg.SetWriteCallback([gevntcount](uint64_t val) {
          // Capture the count that was written back to clear the pending events.
          gevntcount->store(GEVNTCOUNT::Get(0).FromValue(static_cast<uint32_t>(val)).EVNTCOUNT());
        });
      });

  namespace fdescriptor = fuchsia_hardware_usb_descriptor;
  TriggerConnectionPlugIn(fdescriptor::UsbSpeed::kSuper);
  dut_.runtime().RunUntilIdle();

  auto dci_service = dut_.Connect<fuchsia_hardware_usb_dci::UsbDciService::Device>();
  ASSERT_TRUE(dci_service.is_ok());
  fidl::WireSyncClient<fuchsia_hardware_usb_dci::UsbDci> dci{std::move(*dci_service)};

  auto res = dci->StopController();
  ASSERT_TRUE(res.ok());

  // Wait for the operations on the driver thread to finish
  dut_.runtime().RunUntilIdle();

  EXPECT_EQ(evntintrptmask->load(), 1u);
  EXPECT_EQ(gevntcount->load(), 32u);
}

TEST_F(ManagedTestFixture, ConfigureEndpoint_FifoTooSmall) {
  namespace fdescriptor = fuchsia_hardware_usb_descriptor;

  // Dynamic cable connection triggers automatic core wake-up and soft reset.
  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    // Mock GHWPARAMS0 to return MDWIDTH = 2 (128-bit = 16 bytes)
    auto& ghwparams0 = env.reg_region()[GHWPARAMS0::Get().addr()];
    ghwparams0.SetReadCallback([]() -> uint32_t {
      return GHWPARAMS0::Get().FromValue(0).set_DWC_USB31_MDWIDTH(2).reg_value();
    });

    // Mock GTXFIFOSIZ for FIFO 1 to have depth 8 (128 bytes)
    auto& gtxfifosiz1 = env.reg_region()[GTXFIFOSIZ::Get(1).addr()];
    gtxfifosiz1.SetReadCallback(
        []() -> uint32_t { return GTXFIFOSIZ::Get(1).FromValue(0).set_TXFDEP(8).reg_value(); });
  });
  TriggerConnectionPlugIn(fdescriptor::UsbSpeed::kSuper);

  auto dci_service = dut_.Connect<fuchsia_hardware_usb_dci::UsbDciService::Device>();
  ASSERT_TRUE(dci_service.is_ok())
      << "Failed to connect to UsbDciService: " << dci_service.status_string();
  fidl::WireSyncClient<fuchsia_hardware_usb_dci::UsbDci> dci{std::move(*dci_service)};

  // Configure EP1 IN (0x81) with max packet size 512 (which is > 128)
  fdescriptor::wire::UsbEndpointDescriptor ep_desc;
  ep_desc.b_endpoint_address = 0x81;
  ep_desc.bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk);
  ep_desc.w_max_packet_size = 512;
  ep_desc.b_interval = 0;

  auto result = dci->ConfigureEndpoint(ep_desc, {});
  ASSERT_TRUE(result.ok()) << "ConfigureEndpoint transport failed: " << result.status_string();
  ASSERT_TRUE(result.value().is_error());
  EXPECT_EQ(result.value().error_value(), ZX_ERR_INVALID_ARGS);
}
TEST_F(ManagedTestFixture, ConfigureEndpoint_FifoSufficient) {
  namespace fdescriptor = fuchsia_hardware_usb_descriptor;

  // Dynamic cable connection triggers automatic core wake-up and soft reset.
  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    // Mock GHWPARAMS0 to return MDWIDTH = 2 (128-bit = 16 bytes)
    auto& ghwparams0 = env.reg_region()[GHWPARAMS0::Get().addr()];
    ghwparams0.SetReadCallback([]() -> uint32_t {
      return GHWPARAMS0::Get().FromValue(0).set_DWC_USB31_MDWIDTH(2).reg_value();
    });

    // Mock GTXFIFOSIZ for FIFO 1 to have depth 32 (512 bytes)
    auto& gtxfifosiz1 = env.reg_region()[GTXFIFOSIZ::Get(1).addr()];
    gtxfifosiz1.SetReadCallback(
        []() -> uint32_t { return GTXFIFOSIZ::Get(1).FromValue(0).set_TXFDEP(32).reg_value(); });
  });
  TriggerConnectionPlugIn(fdescriptor::UsbSpeed::kSuper);

  auto dci_service = dut_.Connect<fuchsia_hardware_usb_dci::UsbDciService::Device>();
  ASSERT_TRUE(dci_service.is_ok())
      << "Failed to connect to UsbDciService: " << dci_service.status_string();
  fidl::WireSyncClient<fuchsia_hardware_usb_dci::UsbDci> dci{std::move(*dci_service)};

  // Configure EP1 IN (0x81) with max packet size 512 (which is == 512)
  fdescriptor::wire::UsbEndpointDescriptor ep_desc;
  ep_desc.b_endpoint_address = 0x81;
  ep_desc.bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk);
  ep_desc.w_max_packet_size = 512;
  ep_desc.b_interval = 0;

  auto result = dci->ConfigureEndpoint(ep_desc, {});
  ASSERT_TRUE(result.ok()) << "ConfigureEndpoint transport failed: " << result.status_string();
  ASSERT_TRUE(result.value().is_ok())
      << "ConfigureEndpoint failed: " << zx_status_get_string(result.value().error_value());
}

TEST_F(ManagedTestFixture, ConfigureEndpoint_FifoIndexOob) {
  namespace fdescriptor = fuchsia_hardware_usb_descriptor;

  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    // Mock GHWPARAMS3 to return DWC_USB31_NUM_IN_EPS = 4 (EP0 to EP3 IN)
    auto& ghwparams3 = env.reg_region()[GHWPARAMS3::Get().addr()];
    ghwparams3.SetReadCallback([]() -> uint32_t {
      return GHWPARAMS3::Get().FromValue(0).set_DWC_USB31_NUM_IN_EPS(4).reg_value();
    });
  });
  TriggerConnectionPlugIn(fdescriptor::UsbSpeed::kSuper);

  auto dci_service = dut_.Connect<fuchsia_hardware_usb_dci::UsbDciService::Device>();
  ASSERT_TRUE(dci_service.is_ok())
      << "Failed to connect to UsbDciService: " << dci_service.status_string();
  fidl::WireSyncClient<fuchsia_hardware_usb_dci::UsbDci> dci{std::move(*dci_service)};

  // Configure EP10 IN (0x8A) -> fifo_num = 5.
  // Since DWC_USB31_NUM_IN_EPS is 4, fifo_num 5 is OOB.
  fdescriptor::wire::UsbEndpointDescriptor ep_desc;
  ep_desc.b_endpoint_address = 0x8A;
  ep_desc.bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk);
  ep_desc.w_max_packet_size = 512;
  ep_desc.b_interval = 0;

  auto result = dci->ConfigureEndpoint(ep_desc, {});
  ASSERT_TRUE(result.ok()) << "ConfigureEndpoint transport failed: " << result.status_string();
  ASSERT_TRUE(result.value().is_error());
  EXPECT_EQ(result.value().error_value(), ZX_ERR_INVALID_ARGS);
}

TEST_F(ManagedTestFixture, ConfigureEndpoint_RxFifoTooSmall) {
  namespace fdescriptor = fuchsia_hardware_usb_descriptor;

  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    // Mock GHWPARAMS0 to return MDWIDTH = 2 (128-bit = 16 bytes)
    auto& ghwparams0 = env.reg_region()[GHWPARAMS0::Get().addr()];
    ghwparams0.SetReadCallback([]() -> uint32_t {
      return GHWPARAMS0::Get().FromValue(0).set_DWC_USB31_MDWIDTH(2).reg_value();
    });

    // Mock GRXFIFOSIZ for FIFO 0 to have depth 8 (128 bytes)
    auto& grxfifosiz0 = env.reg_region()[GRXFIFOSIZ::Get(0).addr()];
    grxfifosiz0.SetReadCallback(
        []() -> uint32_t { return GRXFIFOSIZ::Get(0).FromValue(0).set_RXFDEP(8).reg_value(); });
  });
  TriggerConnectionPlugIn(fdescriptor::UsbSpeed::kSuper);

  auto dci_service = dut_.Connect<fuchsia_hardware_usb_dci::UsbDciService::Device>();
  ASSERT_TRUE(dci_service.is_ok())
      << "Failed to connect to UsbDciService: " << dci_service.status_string();
  fidl::WireSyncClient<fuchsia_hardware_usb_dci::UsbDci> dci{std::move(*dci_service)};

  // Configure EP1 OUT (0x01) with max packet size 512 (which is > 128)
  fdescriptor::wire::UsbEndpointDescriptor ep_desc;
  ep_desc.b_endpoint_address = 0x01;
  ep_desc.bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk);
  ep_desc.w_max_packet_size = 512;
  ep_desc.b_interval = 0;

  auto result = dci->ConfigureEndpoint(ep_desc, {});
  ASSERT_TRUE(result.ok()) << "ConfigureEndpoint transport failed: " << result.status_string();
  ASSERT_TRUE(result.value().is_error());
  EXPECT_EQ(result.value().error_value(), ZX_ERR_INVALID_ARGS);
}

TEST_F(ManagedTestFixture, ConfigureEndpoint_RxFifoSufficient) {
  namespace fdescriptor = fuchsia_hardware_usb_descriptor;

  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    // Mock GHWPARAMS0 to return MDWIDTH = 2 (128-bit = 16 bytes)
    auto& ghwparams0 = env.reg_region()[GHWPARAMS0::Get().addr()];
    ghwparams0.SetReadCallback([]() -> uint32_t {
      return GHWPARAMS0::Get().FromValue(0).set_DWC_USB31_MDWIDTH(2).reg_value();
    });

    // Mock GRXFIFOSIZ for FIFO 0 to have depth 32 (512 bytes)
    auto& grxfifosiz0 = env.reg_region()[GRXFIFOSIZ::Get(0).addr()];
    grxfifosiz0.SetReadCallback(
        []() -> uint32_t { return GRXFIFOSIZ::Get(0).FromValue(0).set_RXFDEP(32).reg_value(); });
  });
  TriggerConnectionPlugIn(fdescriptor::UsbSpeed::kSuper);

  auto dci_service = dut_.Connect<fuchsia_hardware_usb_dci::UsbDciService::Device>();
  ASSERT_TRUE(dci_service.is_ok())
      << "Failed to connect to UsbDciService: " << dci_service.status_string();
  fidl::WireSyncClient<fuchsia_hardware_usb_dci::UsbDci> dci{std::move(*dci_service)};

  // Configure EP1 OUT (0x01) with max packet size 512 (which is == 512)
  fdescriptor::wire::UsbEndpointDescriptor ep_desc;
  ep_desc.b_endpoint_address = 0x01;
  ep_desc.bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk);
  ep_desc.w_max_packet_size = 512;
  ep_desc.b_interval = 0;

  auto result = dci->ConfigureEndpoint(ep_desc, {});
  ASSERT_TRUE(result.ok()) << "ConfigureEndpoint transport failed: " << result.status_string();
  ASSERT_TRUE(result.value().is_ok())
      << "ConfigureEndpoint failed: " << zx_status_get_string(result.value().error_value());
}

TEST_F(ManagedTestFixture, GetHardwareInfo) {
  namespace fdescriptor = fuchsia_hardware_usb_descriptor;

  // Dynamic cable connection triggers automatic core wake-up and soft reset.
  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    // Mock GHWPARAMS0 to return MDWIDTH = 2 (128-bit = 16 bytes)
    auto& ghwparams0 = env.reg_region()[GHWPARAMS0::Get().addr()];
    ghwparams0.SetReadCallback([]() -> uint32_t {
      return GHWPARAMS0::Get().FromValue(0).set_DWC_USB31_MDWIDTH(2).reg_value();
    });

    // Mock GTXFIFOSIZ for FIFO 1 and 2
    auto& gtxfifosiz1 = env.reg_region()[GTXFIFOSIZ::Get(1).addr()];
    gtxfifosiz1.SetReadCallback(
        []() -> uint32_t { return GTXFIFOSIZ::Get(1).FromValue(0).set_TXFDEP(256).reg_value(); });
    auto& gtxfifosiz2 = env.reg_region()[GTXFIFOSIZ::Get(2).addr()];
    gtxfifosiz2.SetReadCallback(
        []() -> uint32_t { return GTXFIFOSIZ::Get(2).FromValue(0).set_TXFDEP(128).reg_value(); });
  });
  TriggerConnectionPlugIn(fdescriptor::UsbSpeed::kSuper);

  auto dci_service = dut_.Connect<fuchsia_hardware_usb_dci::UsbDciService::Device>();
  ASSERT_TRUE(dci_service.is_ok())
      << "Failed to connect to UsbDciService: " << dci_service.status_string();
  fidl::SyncClient<fuchsia_hardware_usb_dci::UsbDci> dci{std::move(*dci_service)};

  auto result = dci->GetHardwareInfo();
  ASSERT_TRUE(result.is_ok()) << "GetHardwareInfo failed: "
                              << result.error_value().FormatDescription();

  auto& info = result.value().info();
  ASSERT_TRUE(info.endpoints().has_value());
  // 15 IN endpoints + 15 OUT endpoints = 30
  EXPECT_EQ(info.endpoints()->size(), 30u);

  std::set<uint8_t> actual_eps;
  std::set<uint8_t> expected_eps;
  for (uint8_t i = 1; i <= 15; i++) {
    expected_eps.insert(i);         // OUT
    expected_eps.insert(0x80 | i);  // IN
  }

  bool found_ep1_in = false;
  bool found_ep2_in = false;
  for (auto& ep : info.endpoints().value()) {
    ASSERT_TRUE(ep.ep_address().has_value());
    actual_eps.insert(ep.ep_address().value());

    // Verify supported types (Bulk and Interrupt)
    ASSERT_TRUE(ep.supported_types().has_value());
    EXPECT_EQ(ep.supported_types()->size(), 2u);

    auto& st_bulk = ep.supported_types().value()[0];
    auto& st_intr = ep.supported_types().value()[1];

    EXPECT_EQ(st_bulk.endpoint_type().value_or(fdescriptor::EndpointType::kControl),
              fdescriptor::EndpointType::kBulk);
    EXPECT_EQ(st_intr.endpoint_type().value_or(fdescriptor::EndpointType::kControl),
              fdescriptor::EndpointType::kInterrupt);

    if (ep.ep_address().value() == 0x81) {
      EXPECT_EQ(st_bulk.max_packet_size_limit().value_or(0), 4096u);
      found_ep1_in = true;
    } else if (ep.ep_address().value() == 0x82) {
      EXPECT_EQ(st_bulk.max_packet_size_limit().value_or(0), 2048u);
      found_ep2_in = true;
    } else if (ep.ep_address().value() & 0x80) {
      // Other IN EPs
      EXPECT_EQ(st_bulk.max_packet_size_limit().value_or(0), 0u);
    } else {
      // OUT EPs
      EXPECT_EQ(st_bulk.max_packet_size_limit().value_or(0), 1024u);
    }
  }
  EXPECT_EQ(actual_eps, expected_eps);
  EXPECT_TRUE(found_ep1_in);
  EXPECT_TRUE(found_ep2_in);
  EXPECT_EQ(info.supports_dynamic_ep_sizing().value_or(true), false);
}

typedef struct {
  // Full core_id + versioning information.
  uint32_t version_register;

  // True if the driver is expected to start.
  bool should_start;
} Param;

// clang-format off
const auto kCases = testing::Values(
    Param{0x00000000, false},
    Param{0xffffffff, false},
    Param{0x5500101a, false},
    Param{0x5532101a, false},
    Param{0x5533101a, true},
    Param{0x5533101a, true},
    Param{0x5533308a, true},
    Param{0x5533309a, true},
    Param{0x5533309b, true},
    Param{0x5533310a, true},
    Param{0x5533310b, true},
    Param{0x5533311a, true},
    Param{0x55333110, true},
    Param{0x5533401a, true},
    Param{0x5534101a, false});  // 5534 is invalid core id.
// clang-format on

using Parameterized = TestFixture<false, testing::TestWithParam<Param>>;

TEST_P(Parameterized, TestHwVersion) {
  Param p{GetParam()};

  ver_number_ = p.version_register;
  if (!p.should_start) {
    dut_.RunInEnvironmentTypeContext(
        [](Environment& env) { env.usb_phy().set_expect_connection_status_observer_call(false); });
  }

  zx::result start = dut_.StartDriverWithCustomStartArgs([](fdf::DriverStartArgs& args) {
    dwc3_config::Config cfg;
    cfg.enable_suspend() = false;
    cfg.bypass_platform_extension() = true;
    args.config(cfg.ToVmo());
  });

  ASSERT_EQ(start.is_ok(), p.should_start);

  dut_.runtime().RunUntilIdle();
  EXPECT_EQ(WaitForPhy(), ZX_OK);

  if (p.should_start) {
    EXPECT_EQ(dut_.StopDriver().status_value(), ZX_OK);
  }
}

// clang-format off
INSTANTIATE_TEST_SUITE_P(
    HwVersioningTest,
    Parameterized,
    kCases,
    [](const testing::TestParamInfo<Parameterized::ParamType>& info) {
      std::stringstream test_name;

      test_name << info.index << "_0x" << std::hex << info.param.version_register
          << (info.param.should_start ? "_START_OK" : "_START_FAIL");

      return test_name.str();
    });
// clang-format on

class InterruptModeration
    : public TestFixture<false, testing::TestWithParam<std::optional<uint32_t>>> {};

const auto kInterruptModerationCases =
    testing::Values(std::nullopt, 1, 1000, std::numeric_limits<uint32_t>::max(), 20000);

TEST_P(InterruptModeration, Values) {
  namespace fdescriptor = fuchsia_hardware_usb_descriptor;
  const std::optional<uint32_t> interrupt_moderation_us = GetParam();

  auto devimod_written_flag = std::make_shared<std::atomic<bool>>(false);
  auto devimod_written_val = std::make_shared<std::atomic<uint32_t>>(0);

  // Use RAII guard to guarantee register callback cleanup on scope exit regardless of assertions.
  auto cleanup_callbacks = fit::defer([&]() {
    dut_.RunInEnvironmentTypeContext([](Environment& env) {
      env.reg_region()[DEVIMOD::Get(0).addr()].SetWriteCallback([](uint64_t) {});
    });
  });

  dut_.RunInEnvironmentTypeContext([interrupt_moderation_us, devimod_written_val,
                                    devimod_written_flag](Environment& env) {
    if (interrupt_moderation_us.has_value()) {
      env.SetInterruptModerationUs(interrupt_moderation_us.value());
    }
    ddk_fake::FakeMmioReg& devimod = env.reg_region()[DEVIMOD::Get(0).addr()];
    devimod.SetWriteCallback([devimod_written_val, devimod_written_flag](uint64_t value) {
      devimod_written_val->store(DEVIMOD::Get(0).FromValue(static_cast<uint32_t>(value)).IMODI());
      devimod_written_flag->store(true);
    });
  });
  zx::result res = dut_.StartDriverWithCustomStartArgs([](fdf::DriverStartArgs& args) {
    dwc3_config::Config cfg;
    cfg.enable_suspend() = false;
    cfg.bypass_platform_extension() = true;
    args.config(cfg.ToVmo());
  });
  if (interrupt_moderation_us.has_value() &&
      static_cast<uint16_t>(interrupt_moderation_us.value()) >=
          std::numeric_limits<uint16_t>::max() / 4) {
    dut_.RunInEnvironmentTypeContext(
        [](Environment& env) { env.usb_phy().set_expect_connection_status_observer_call(false); });
    ASSERT_STATUS(res, ZX_ERR_OUT_OF_RANGE);
    return;
  }
  ASSERT_OK(res);
  ASSERT_OK(WaitForPhy());

  auto dci_service = dut_.Connect<fuchsia_hardware_usb_dci::UsbDciService::Device>();
  ASSERT_OK(dci_service.status_value());
  fidl::WireSyncClient<fuchsia_hardware_usb_dci::UsbDci> dci{std::move(*dci_service)};
  ASSERT_OK(dci->StartController().status());

  TriggerConnectionPlugIn(fdescriptor::UsbSpeed::kSuper);

  // Positively verify that StartPeripheralMode() completed its execution turn and became active.
  dut_.runtime().RunUntil([&]() {
    return dut_.RunInDriverContext<bool>(
        [](Dwc3& drv) { return Dwc3TestHelper::GetPowerOn(drv) && Dwc3TestHelper::IsActive(drv); });
  });

  if (interrupt_moderation_us.has_value() && interrupt_moderation_us.value() != 0) {
    EXPECT_TRUE(devimod_written_flag->load());
    EXPECT_EQ(devimod_written_val->load(), interrupt_moderation_us.value() * 4);
  } else {
    EXPECT_FALSE(devimod_written_flag->load()) << devimod_written_val->load();
  }

  EXPECT_OK(dut_.StopDriver().status_value());
}

INSTANTIATE_TEST_SUITE_P(InterruptModeration, InterruptModeration, kInterruptModerationCases,
                         [](const testing::TestParamInfo<std::optional<uint32_t>>& info)
                             -> std::string {
                           if (!info.param.has_value()) {
                             return "empty";
                           }
                           return std::format("{}", info.param.value());
                         });

TEST_F(UnmanagedTestFixture, FrameLengthAdjustmentSupportedVersion) {
  // Set version to 3.10a
  ver_number_ = 0x5533310a;

  auto gfladj_written_val = std::make_shared<std::atomic<uint32_t>>(0);
  auto gfladj_written_flag = std::make_shared<std::atomic<bool>>(false);

  auto cleanup_callbacks = fit::defer([&]() {
    dut_.RunInEnvironmentTypeContext([](Environment& env) {
      env.reg_region()[GFLADJ::Get().addr()].SetWriteCallback([](uint64_t) {});
    });
  });

  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    env.SetFrameLengthAdjustment(0xa0, /*refclk_lpm_sel=*/true);
    ddk_fake::FakeMmioReg& gfladj_reg = env.reg_region()[GFLADJ::Get().addr()];
    gfladj_reg.SetWriteCallback([gfladj_written_val, gfladj_written_flag](uint64_t value) {
      gfladj_written_val->store(static_cast<uint32_t>(value));
      gfladj_written_flag->store(true);
    });
  });

  zx::result res = dut_.StartDriverWithCustomStartArgs([](fdf::DriverStartArgs& args) {
    dwc3_config::Config cfg;
    cfg.enable_suspend() = false;
    cfg.bypass_platform_extension() = true;
    args.config(cfg.ToVmo());
  });
  ASSERT_OK(res);
  ASSERT_OK(WaitForPhy());

  dut_.RunInDriverContext([&](Dwc3& drv) {
    EXPECT_TRUE(drv.supports_gfladj());
    EXPECT_EQ(drv.fladj(), 0xa0u);
    EXPECT_TRUE(drv.fladj_refclk_lpm_sel());
  });

  EXPECT_TRUE(gfladj_written_flag->load());
  auto written = GFLADJ::Get().FromValue(gfladj_written_val->load());
  EXPECT_EQ(written.GFLADJ_30MHZ(), 0x20u);
  EXPECT_EQ(written.GFLADJ_30MHZ_SDBND_SEL(), 1u);
  EXPECT_EQ(written.GFLADJ_REFCLK_LPM_SEL(), 1u);

  EXPECT_OK(dut_.StopDriver().status_value());
}

TEST_F(UnmanagedTestFixture, FrameLengthAdjustmentZeroValue) {
  // Set version to 3.10a
  ver_number_ = 0x5533310a;

  auto gfladj_written_val = std::make_shared<std::atomic<uint32_t>>(0);
  auto gfladj_written_flag = std::make_shared<std::atomic<bool>>(false);

  auto cleanup_callbacks = fit::defer([&]() {
    dut_.RunInEnvironmentTypeContext([](Environment& env) {
      env.reg_region()[GFLADJ::Get().addr()].SetWriteCallback([](uint64_t) {});
    });
  });

  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    env.SetFrameLengthAdjustment(0x0, /*refclk_lpm_sel=*/false);
    ddk_fake::FakeMmioReg& gfladj_reg = env.reg_region()[GFLADJ::Get().addr()];
    gfladj_reg.SetWriteCallback([gfladj_written_val, gfladj_written_flag](uint64_t value) {
      gfladj_written_val->store(static_cast<uint32_t>(value));
      gfladj_written_flag->store(true);
    });
  });

  zx::result res = dut_.StartDriverWithCustomStartArgs([](fdf::DriverStartArgs& args) {
    dwc3_config::Config cfg;
    cfg.enable_suspend() = false;
    cfg.bypass_platform_extension() = true;
    args.config(cfg.ToVmo());
  });
  ASSERT_OK(res);
  ASSERT_OK(WaitForPhy());

  dut_.RunInDriverContext([&](Dwc3& drv) {
    EXPECT_TRUE(drv.supports_gfladj());
    EXPECT_EQ(drv.fladj(), 0x0u);
    EXPECT_FALSE(drv.fladj_refclk_lpm_sel());
  });

  EXPECT_TRUE(gfladj_written_flag->load());
  auto written = GFLADJ::Get().FromValue(gfladj_written_val->load());
  EXPECT_EQ(written.GFLADJ_30MHZ(), 0x0u);
  EXPECT_EQ(written.GFLADJ_30MHZ_SDBND_SEL(), 1u);
  EXPECT_EQ(written.GFLADJ_REFCLK_LPM_SEL(), 0u);

  EXPECT_OK(dut_.StopDriver().status_value());
}

TEST_F(UnmanagedTestFixture, FrameLengthAdjustmentRefclkLpmSelOnly) {
  // Set version to 3.10a
  ver_number_ = 0x5533310a;

  auto gfladj_written_val = std::make_shared<std::atomic<uint32_t>>(0);
  auto gfladj_written_flag = std::make_shared<std::atomic<bool>>(false);

  auto cleanup_callbacks = fit::defer([&]() {
    dut_.RunInEnvironmentTypeContext([](Environment& env) {
      env.reg_region()[GFLADJ::Get().addr()].SetWriteCallback([](uint64_t) {});
    });
  });

  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    env.SetFrameLengthAdjustment(std::nullopt, /*refclk_lpm_sel=*/true);
    ddk_fake::FakeMmioReg& gfladj_reg = env.reg_region()[GFLADJ::Get().addr()];
    gfladj_reg.SetWriteCallback([gfladj_written_val, gfladj_written_flag](uint64_t value) {
      gfladj_written_val->store(static_cast<uint32_t>(value));
      gfladj_written_flag->store(true);
    });
  });

  zx::result res = dut_.StartDriverWithCustomStartArgs([](fdf::DriverStartArgs& args) {
    dwc3_config::Config cfg;
    cfg.enable_suspend() = false;
    cfg.bypass_platform_extension() = true;
    args.config(cfg.ToVmo());
  });
  ASSERT_OK(res);
  ASSERT_OK(WaitForPhy());

  dut_.RunInDriverContext([&](Dwc3& drv) {
    EXPECT_TRUE(drv.supports_gfladj());
    EXPECT_FALSE(drv.fladj().has_value());
    EXPECT_TRUE(drv.fladj_refclk_lpm_sel());
  });

  EXPECT_TRUE(gfladj_written_flag->load());
  auto written = GFLADJ::Get().FromValue(gfladj_written_val->load());
  EXPECT_EQ(written.GFLADJ_30MHZ(), 0x0u);
  EXPECT_EQ(written.GFLADJ_30MHZ_SDBND_SEL(), 0u);
  EXPECT_EQ(written.GFLADJ_REFCLK_LPM_SEL(), 1u);

  EXPECT_OK(dut_.StopDriver().status_value());
}

TEST_F(UnmanagedTestFixture, FrameLengthAdjustmentUnsupportedVersion) {
  // 1.60a is default, which is prior to 2.50a
  ver_number_ = 0x5533160a;

  auto gfladj_written_flag = std::make_shared<std::atomic<bool>>(false);

  auto cleanup_callbacks = fit::defer([&]() {
    dut_.RunInEnvironmentTypeContext([](Environment& env) {
      env.reg_region()[GFLADJ::Get().addr()].SetWriteCallback([](uint64_t) {});
    });
  });

  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    env.SetFrameLengthAdjustment(0xa0);
    ddk_fake::FakeMmioReg& gfladj_reg = env.reg_region()[GFLADJ::Get().addr()];
    gfladj_reg.SetWriteCallback(
        [gfladj_written_flag](uint64_t value) { gfladj_written_flag->store(true); });
  });

  zx::result res = dut_.StartDriverWithCustomStartArgs([](fdf::DriverStartArgs& args) {
    dwc3_config::Config cfg;
    cfg.enable_suspend() = false;
    cfg.bypass_platform_extension() = true;
    args.config(cfg.ToVmo());
  });
  ASSERT_OK(res);
  ASSERT_OK(WaitForPhy());

  dut_.RunInDriverContext([&](Dwc3& drv) {
    EXPECT_FALSE(drv.supports_gfladj());
    EXPECT_EQ(drv.fladj(), 0xa0u);
  });

  EXPECT_FALSE(gfladj_written_flag->load());

  EXPECT_OK(dut_.StopDriver().status_value());
}

TEST_F(UnmanagedTestFixture, FrameLengthAdjustmentNoMetadata) {
  ver_number_ = 0x5533310a;

  auto gfladj_written_flag = std::make_shared<std::atomic<bool>>(false);

  auto cleanup_callbacks = fit::defer([&]() {
    dut_.RunInEnvironmentTypeContext([](Environment& env) {
      env.reg_region()[GFLADJ::Get().addr()].SetWriteCallback([](uint64_t) {});
    });
  });

  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    ddk_fake::FakeMmioReg& gfladj_reg = env.reg_region()[GFLADJ::Get().addr()];
    gfladj_reg.SetWriteCallback(
        [gfladj_written_flag](uint64_t value) { gfladj_written_flag->store(true); });
  });

  zx::result res = dut_.StartDriverWithCustomStartArgs([](fdf::DriverStartArgs& args) {
    dwc3_config::Config cfg;
    cfg.enable_suspend() = false;
    cfg.bypass_platform_extension() = true;
    args.config(cfg.ToVmo());
  });
  ASSERT_OK(res);
  ASSERT_OK(WaitForPhy());

  dut_.RunInDriverContext([&](Dwc3& drv) {
    EXPECT_TRUE(drv.supports_gfladj());
    EXPECT_FALSE(drv.fladj().has_value());
  });

  EXPECT_FALSE(gfladj_written_flag->load());

  EXPECT_OK(dut_.StopDriver().status_value());
}

class UnmanagedWakeLeaseTestFixture : public UnmanagedTestFixture {
 protected:
  void VerifyDisconnectHoldsWakeLease(bool bypass_platform_extension) {
    zx::result start = dut_.StartDriverWithCustomStartArgs(
        [bypass_platform_extension](fdf::DriverStartArgs& args) {
          dwc3_config::Config cfg;
          cfg.enable_suspend() = false;
          cfg.bypass_platform_extension() = bypass_platform_extension;
          args.config(cfg.ToVmo());
        });
    ASSERT_OK(start);
    dut_.runtime().RunUntilIdle();
    ASSERT_OK(WaitForPhy());

    dut_.RunInDriverContext([this, bypass_platform_extension](Dwc3& drv) {
      PlatformExtension* ext = this->GetPlatformExtension(drv);
      if (bypass_platform_extension) {
        EXPECT_EQ(ext, nullptr);
      } else {
        ASSERT_NE(ext, nullptr);
        EXPECT_TRUE(ext->PowersDownCoreOnDisconnect());
      }
    });

    FakeUsbDciInterface fake_dci;
    fake_dci.SetDeferSetConnectedFalse(true);
    libsync::Completion set_connected_false_received;
    fake_dci.SetSetConnectedCallback([&](bool connected) {
      if (!connected) {
        set_connected_false_received.Signal();
      }
    });
    auto binding = BindDciInterface(&fake_dci);

    auto dci_service = dut_.Connect<fuchsia_hardware_usb_dci::UsbDciService::Device>();
    ASSERT_OK(dci_service.status_value());
    fidl::WireSyncClient<fuchsia_hardware_usb_dci::UsbDci> dci{std::move(*dci_service)};
    ASSERT_OK(dci->StartController().status());

    // Connect and supply a wake lease via the PHY connection watcher.
    zx::eventpair lease_client, lease_peer;
    ASSERT_OK(zx::eventpair::create(0, &lease_client, &lease_peer));

    dut_.RunInEnvironmentTypeContext([](Environment& env) { env.usb_phy().completion()->Reset(); });
    dut_.RunInEnvironmentTypeContext(
        [&](Environment& env) { env.usb_phy().TriggerConnection(true, std::move(lease_client)); });
    dut_.runtime().RunUntilIdle();
    ASSERT_OK(WaitForPhy());

    dut_.RunInDriverContext([](Dwc3& drv) {
      EXPECT_TRUE(Dwc3TestHelper::HasConnectionLease(drv));
      EXPECT_TRUE(Dwc3TestHelper::IsActive(drv));
    });
    zx_signals_t observed = 0;
    EXPECT_EQ(lease_peer.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(), &observed),
              ZX_ERR_TIMED_OUT);

    // The initial (disconnected) watcher status already produced one drained report at start-up;
    // measure the disconnect below relative to the counts after the connect.
    size_t disconnected_reports_after_connect = 0;
    dut_.RunInEnvironmentTypeContext([&](Environment& env) {
      EXPECT_EQ(env.usb_phy().connected_reports(), 1u);
      disconnected_reports_after_connect = env.usb_phy().disconnected_reports();
    });

    // Trigger disconnect while fake_dci holds the SetConnected(false) completer.
    dut_.RunInEnvironmentTypeContext([](Environment& env) { env.usb_phy().completion()->Reset(); });
    dut_.RunInEnvironmentTypeContext(
        [](Environment& env) { env.usb_phy().TriggerConnection(false); });
    dut_.runtime().RunUntilIdle();
    ASSERT_TRUE(dut_.runtime().RunWithTimeoutOrUntil(
        [&]() { return set_connected_false_received.signaled(); }, zx::duration::infinite()));
    dut_.runtime().RunUntilIdle();

    ASSERT_TRUE(fake_dci.has_deferred_set_connected());

    // While SetConnected(false) is still in flight, the core clocks/vreg and connection wake lease
    // must remain active so usb-peripheral and bound functions can safely disable endpoints, while
    // is_active() is false and new ConfigureEndpoint / SetStall / ClearStall calls are rejected.
    // The PHY must not yet have been told the core is disconnected, nor may the connection watcher
    // have been re-armed: the PHY takes either as permission to remove the core's clocks, resets
    // and power.
    dut_.RunInDriverContext([](Dwc3& drv) {
      EXPECT_TRUE(Dwc3TestHelper::GetPowerOn(drv));
      EXPECT_FALSE(Dwc3TestHelper::IsActive(drv));
      EXPECT_TRUE(Dwc3TestHelper::HasConnectionLease(drv));
    });
    {
      auto disable_res = dci->DisableEndpoint(0x81);
      ASSERT_OK(disable_res.status());
      EXPECT_TRUE(disable_res->is_ok());

      fdescriptor::wire::UsbEndpointDescriptor ep_desc{
          .b_length = sizeof(fdescriptor::wire::UsbEndpointDescriptor),
          .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
          .b_endpoint_address = 0x81,
          .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
          .w_max_packet_size = 512,
      };
      auto config_res = dci->ConfigureEndpoint(ep_desc, {});
      ASSERT_OK(config_res.status());
      ASSERT_TRUE(config_res->is_error());
      EXPECT_STATUS(config_res->error_value(), ZX_ERR_IO_NOT_PRESENT);

      auto stall_res = dci->EndpointSetStall(0x81);
      ASSERT_OK(stall_res.status());
      ASSERT_TRUE(stall_res->is_error());
      EXPECT_STATUS(stall_res->error_value(), ZX_ERR_IO_NOT_PRESENT);

      auto clear_stall_res = dci->EndpointClearStall(0x81);
      ASSERT_OK(clear_stall_res.status());
      ASSERT_TRUE(clear_stall_res->is_error());
      EXPECT_STATUS(clear_stall_res->error_value(), ZX_ERR_IO_NOT_PRESENT);
    }
    dut_.RunInEnvironmentTypeContext([&](Environment& env) {
      EXPECT_EQ(env.usb_phy().disconnected_reports(), disconnected_reports_after_connect);
      EXPECT_FALSE(env.usb_phy().has_completer());
    });
    observed = 0;
    EXPECT_EQ(lease_peer.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(), &observed),
              ZX_ERR_TIMED_OUT);

    // Once SetConnected(false) completes, DWC3 powers down the core, releases the connection wake
    // lease, and only then notifies the PHY that disconnect teardown has drained and re-arms the
    // connection watcher.
    dut_.RunInDriverContext([&](Dwc3& drv) { fake_dci.CompleteDeferredSetConnected(ZX_OK); });
    dut_.runtime().RunUntilIdle();
    ASSERT_OK(WaitForPhy());

    observed = 0;
    EXPECT_OK(lease_peer.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite(), &observed));
    EXPECT_NE(observed & ZX_EVENTPAIR_PEER_CLOSED, 0u);
    // The drained report travels on the UsbPhy channel and may be processed by the fake PHY after
    // the re-armed watcher; poll for exactly one more report than before the disconnect.
    EXPECT_TRUE(dut_.runtime().RunWithTimeoutOrUntil(
        [&]() {
          size_t reports = 0;
          dut_.RunInEnvironmentTypeContext(
              [&](Environment& env) { reports = env.usb_phy().disconnected_reports(); });
          return reports == disconnected_reports_after_connect + 1;
        },
        zx::duration::infinite()));

    if (!bypass_platform_extension) {
      dut_.RunInEnvironmentTypeContext([](Environment& env) {
        EXPECT_FALSE(env.vreg().enabled());
        EXPECT_FALSE(env.clock_core().enabled());
      });
    }

    dut_.RunInDriverContext([](Dwc3& drv) {
      EXPECT_FALSE(Dwc3TestHelper::GetPowerOn(drv));
      EXPECT_FALSE(Dwc3TestHelper::HasConnectionLease(drv));
    });

    EXPECT_OK(dut_.StopDriver().status_value());
  }
};

TEST_F(UnmanagedWakeLeaseTestFixture, DisconnectHoldsWakeLeaseNoPlatformExtension) {
  VerifyDisconnectHoldsWakeLease(/*bypass_platform_extension=*/true);
}

TEST_F(UnmanagedWakeLeaseTestFixture, DisconnectHoldsWakeLeaseQualcommPlatformExtension) {
  VerifyDisconnectHoldsWakeLease(/*bypass_platform_extension=*/false);
}

TEST_F(UnmanagedWakeLeaseTestFixture, StopDriverWhileDisconnectInFlightPowersDownCore) {
  dut_.RunInEnvironmentTypeContext([](Environment& env) { env.set_serve_platform_mocks(true); });

  zx::result start = dut_.StartDriverWithCustomStartArgs([](fdf::DriverStartArgs& args) {
    dwc3_config::Config cfg;
    cfg.enable_suspend() = false;
    cfg.bypass_platform_extension() = false;
    args.config(cfg.ToVmo());
  });
  ASSERT_OK(start);
  dut_.runtime().RunUntilIdle();
  ASSERT_OK(WaitForPhy());

  FakeUsbDciInterface fake_dci;
  fake_dci.SetDeferSetConnectedFalse(true);
  libsync::Completion set_connected_false_received;
  fake_dci.SetSetConnectedCallback([&](bool connected) {
    if (!connected) {
      set_connected_false_received.Signal();
    }
  });
  auto binding = BindDciInterface(&fake_dci);

  zx::eventpair lease_client, lease_peer;
  ASSERT_OK(zx::eventpair::create(0, &lease_client, &lease_peer));

  dut_.RunInEnvironmentTypeContext([](Environment& env) { env.usb_phy().completion()->Reset(); });
  dut_.RunInEnvironmentTypeContext(
      [&](Environment& env) { env.usb_phy().TriggerConnection(true, std::move(lease_client)); });
  dut_.runtime().RunUntilIdle();
  ASSERT_OK(WaitForPhy());

  dut_.RunInEnvironmentTypeContext([](Environment& env) { env.usb_phy().completion()->Reset(); });
  dut_.RunInEnvironmentTypeContext(
      [](Environment& env) { env.usb_phy().TriggerConnection(false); });
  dut_.runtime().RunUntilIdle();
  ASSERT_TRUE(dut_.runtime().RunWithTimeoutOrUntil(
      [&]() { return set_connected_false_received.signaled(); }, zx::duration::infinite()));
  dut_.runtime().RunUntilIdle();

  ASSERT_TRUE(fake_dci.has_deferred_set_connected());

  // Stopping the driver while SetConnected(false) is still in flight drops the pending FIDL call
  // and synchronously completes the deferred PowerDownCore() before Stop() finishes. The connection
  // watcher is never re-armed after that (the driver is gone), so the fake PHY must not expect a
  // pending hanging get at the end of the test.
  dut_.RunInEnvironmentTypeContext(
      [](Environment& env) { env.usb_phy().set_expect_connection_status_observer_call(false); });
  EXPECT_OK(dut_.StopDriver().status_value());

  zx_signals_t observed = 0;
  EXPECT_OK(lease_peer.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite(), &observed));
  EXPECT_NE(observed & ZX_EVENTPAIR_PEER_CLOSED, 0u);

  dut_.RunInEnvironmentTypeContext([](Environment& env) {
    EXPECT_FALSE(env.vreg().enabled());
    EXPECT_FALSE(env.clock_core().enabled());
    EXPECT_FALSE(env.usb_phy().has_completer());
  });
}

TEST_F(UnmanagedTestFixture, SetConfigurationPreservesEp0AndIgnoresStaleCompletionAcrossBusReset) {
  FakeUsbDciInterface fake_dci;
  auto dalepena_val = std::make_shared<std::atomic<uint32_t>>(0);

  dut_.RunInEnvironmentTypeContext([dalepena_val](Environment& env) {
    auto& dalepena = env.reg_region()[DALEPENA::Get().addr()];
    dalepena.SetWriteCallback(
        [dalepena_val](uint64_t val_raw) { dalepena_val->store(static_cast<uint32_t>(val_raw)); });
    dalepena.SetReadCallback([dalepena_val]() -> uint64_t { return dalepena_val->load(); });
  });

  SetUpAndPowerOnDriver();

  auto cleanup_callbacks = fit::defer([&]() {
    dut_.RunInEnvironmentTypeContext([](Environment& env) {
      env.reg_region()[DALEPENA::Get().addr()].SetWriteCallback([](uint64_t) {});
      env.reg_region()[DALEPENA::Get().addr()].SetReadCallback([]() -> uint64_t { return 0; });
    });
  });

  auto binding = BindDciInterface(&fake_dci);

  uint64_t gen_before = 0;
  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::EnableEp0(drv);
    Dwc3TestHelper::SetEp0CurSpeed(drv, fdescriptor::wire::UsbSpeed::kSuper);
    Dwc3TestHelper::SetControllerStarted(drv, true);
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);

    EXPECT_TRUE(Dwc3TestHelper::IsEpEnabled(drv, 0));
    EXPECT_TRUE(Dwc3TestHelper::IsEpEnabled(drv, 1));
    EXPECT_EQ(dalepena_val->load() & 0x3u, 0x3u);
    EXPECT_EQ(Dwc3TestHelper::GetEp0CurSpeed(drv), fdescriptor::wire::UsbSpeed::kSuper);
    gen_before = Dwc3TestHelper::GetEp0SetupGeneration(drv);
  });

  // Defer the DCI Control reply to hold the transfer in-flight.
  libsync::Completion control_deferred;
  dut_.RunInDriverContext([&](Dwc3& drv) { fake_dci.SetDeferControl(true, &control_deferred); });

  // Send USB_REQ_SET_CONFIGURATION.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto setup = MakeSetupPacket(
        kStandardDeviceOut, fidl::ToUnderlying(fdescriptor::StandardRequest::kSetConfiguration), 1,
        0, 0);
    Dwc3TestHelper::SimulateSetupReceived(drv, setup);
  });

  control_deferred.Wait();
  dut_.RunInDriverContext([&](Dwc3& drv) { ASSERT_TRUE(fake_dci.has_deferred_control()); });

  // Verify that USB_REQ_SET_CONFIGURATION called ResetConfiguration() which resets user
  // endpoints without calling Ep0Reset(): EP0 OUT/IN remain enabled in software,
  // DALEPENA bits 0 and 1 remain set, and cur_speed is preserved.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    EXPECT_TRUE(Dwc3TestHelper::IsEpEnabled(drv, 0));
    EXPECT_TRUE(Dwc3TestHelper::IsEpEnabled(drv, 1));
    EXPECT_EQ(dalepena_val->load() & 0x3u, 0x3u);
    EXPECT_EQ(Dwc3TestHelper::GetEp0CurSpeed(drv), fdescriptor::wire::UsbSpeed::kSuper);
  });

  // Simulate a bus reset arriving while Control(SET_CONFIGURATION) is still pending.
  dut_.RunInDriverContext([&](Dwc3& drv) { Dwc3TestHelper::HandleResetEvent(drv); });
  dut_.runtime().RunUntilIdle();

  // Reset puts the device in kDefault state and increments setup_generation.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    EXPECT_EQ(Dwc3TestHelper::GetDeviceState(drv),
              fuchsia_hardware_usb_policy::wire::DeviceState::kDefault);
    EXPECT_GT(Dwc3TestHelper::GetEp0SetupGeneration(drv), gen_before);
  });

  // Now complete the stale deferred Control() call from before the reset.
  dut_.RunInDriverContext([&](Dwc3& drv) { fake_dci.CompleteDeferredControl(ZX_OK); });
  dut_.runtime().RunUntilIdle();

  // The stale completion must be ignored and must not transition device state to kConfigured.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    EXPECT_EQ(Dwc3TestHelper::GetDeviceState(drv),
              fuchsia_hardware_usb_policy::wire::DeviceState::kDefault);
  });

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, Ep0QueueSetupIncrementsGenerationAndDropsStaleControlCompletion) {
  FakeUsbDciInterface fake_dci;
  SetUpAndPowerOnDriver();

  auto binding = BindDciInterface(&fake_dci);

  uint64_t gen_before = 0;
  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::EnableEp0(drv);
    Dwc3TestHelper::SetControllerStarted(drv, true);
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);
    gen_before = Dwc3TestHelper::GetEp0SetupGeneration(drv);
  });

  // Defer the DCI Control reply to hold the transfer in-flight.
  libsync::Completion control_deferred;
  dut_.RunInDriverContext([&](Dwc3& drv) { fake_dci.SetDeferControl(true, &control_deferred); });

  // Send USB_REQ_SET_CONFIGURATION.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto setup = MakeSetupPacket(
        kStandardDeviceOut, fidl::ToUnderlying(fdescriptor::StandardRequest::kSetConfiguration), 1,
        0, 0);
    Dwc3TestHelper::SimulateSetupReceived(drv, setup);
  });

  control_deferred.Wait();
  dut_.RunInDriverContext([&](Dwc3& drv) { ASSERT_TRUE(fake_dci.has_deferred_control()); });

  // Simulate a new SETUP packet arriving, re-invoking Ep0QueueSetup on early abort.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::Ep0QueueSetup(drv);
    EXPECT_GT(Dwc3TestHelper::GetEp0SetupGeneration(drv), gen_before);
  });

  // Complete the stale deferred Control() call from the prior generation.
  dut_.RunInDriverContext([&](Dwc3& drv) { fake_dci.CompleteDeferredControl(ZX_OK); });
  dut_.runtime().RunUntilIdle();

  // The stale completion must be dropped and must not transition device state to kConfigured.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    EXPECT_NE(Dwc3TestHelper::GetDeviceState(drv),
              fuchsia_hardware_usb_policy::wire::DeviceState::kConfigured);
  });

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, StartPeripheralModeRestoresCoreRegistersAfterExternalReset) {
  ver_number_ = 0x5533310a;

  auto gsbuscfg0_val = std::make_shared<std::atomic<uint32_t>>(0);
  auto guctl2_val = std::make_shared<std::atomic<uint32_t>>(0);
  auto gfladj_val = std::make_shared<std::atomic<uint32_t>>(0);

  auto cleanup_callbacks = fit::defer([&]() {
    dut_.RunInEnvironmentTypeContext([](Environment& env) {
      env.reg_region()[GSBUSCFG0::Get().addr()].SetReadCallback([]() -> uint64_t { return 0; });
      env.reg_region()[GSBUSCFG0::Get().addr()].SetWriteCallback([](uint64_t) {});
      env.reg_region()[GUCTL2::Get().addr()].SetWriteCallback([](uint64_t) {});
      env.reg_region()[GFLADJ::Get().addr()].SetWriteCallback([](uint64_t) {});
    });
  });

  dut_.RunInEnvironmentTypeContext([gsbuscfg0_val, guctl2_val, gfladj_val](Environment& env) {
    env.SetFrameLengthAdjustment(0xa0, /*refclk_lpm_sel=*/true);
    env.reg_region()[GSBUSCFG0::Get().addr()].SetReadCallback(
        []() -> uint64_t { return 0xabcd00ffu; });
    env.reg_region()[GSBUSCFG0::Get().addr()].SetWriteCallback(
        [gsbuscfg0_val](uint64_t val) { gsbuscfg0_val->store(static_cast<uint32_t>(val)); });
    env.reg_region()[GUCTL2::Get().addr()].SetWriteCallback(
        [guctl2_val](uint64_t val) { guctl2_val->store(static_cast<uint32_t>(val)); });
    env.reg_region()[GFLADJ::Get().addr()].SetWriteCallback(
        [gfladj_val](uint64_t val) { gfladj_val->store(static_cast<uint32_t>(val)); });
  });

  SetUpAndPowerOnDriver();

  auto dci_service = dut_.Connect<fuchsia_hardware_usb_dci::UsbDciService::Device>();
  ASSERT_TRUE(dci_service.is_ok());
  fidl::WireSyncClient<fuchsia_hardware_usb_dci::UsbDci> dci{std::move(*dci_service)};

  // Clear captured register values right before StartController() to simulate an external PHY/core
  // reset occurring between ResetHw() and StartPeripheralMode().
  gsbuscfg0_val->store(0);
  guctl2_val->store(0);
  gfladj_val->store(0);

  auto start_res = dci->StartController();
  ASSERT_TRUE(start_res.ok());
  ASSERT_TRUE(start_res.value().is_ok());

  EXPECT_EQ(gsbuscfg0_val->load(), 0xabcd0002u);
  EXPECT_EQ(GUCTL2::Get().FromValue(guctl2_val->load()).Rst_actbitlater(), 0u);
  auto gfladj = GFLADJ::Get().FromValue(gfladj_val->load());
  EXPECT_EQ(gfladj.GFLADJ_30MHZ(), 0x20u);
  EXPECT_EQ(gfladj.GFLADJ_30MHZ_SDBND_SEL(), 1u);
  EXPECT_EQ(gfladj.GFLADJ_REFCLK_LPM_SEL(), 1u);

  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, ResetHwRestoresPrtcapdirDevice) {
  auto gctl_val = std::make_shared<std::atomic<uint32_t>>(0);

  auto cleanup_callbacks = fit::defer([&]() {
    dut_.RunInEnvironmentTypeContext([](Environment& env) {
      env.reg_region()[GCTL::Get().addr()].SetReadCallback([]() -> uint64_t { return 0; });
      env.reg_region()[GCTL::Get().addr()].SetWriteCallback([](uint64_t) {});
    });
  });

  dut_.RunInEnvironmentTypeContext([gctl_val](Environment& env) {
    // Simulate register state where GCTL.PRTCAPDIR is 0 (hardware default after CSFTRST).
    auto& gctl_reg = env.reg_region()[GCTL::Get().addr()];
    gctl_reg.SetReadCallback([gctl_val]() -> uint64_t { return gctl_val->load(); });
    gctl_reg.SetWriteCallback(
        [gctl_val](uint64_t val) { gctl_val->store(static_cast<uint32_t>(val)); });
    gctl_val->store(GCTL::Get().FromValue(0).reg_value());
  });

  zx::result start = dut_.StartDriverWithCustomStartArgs([](fdf::DriverStartArgs& args) {
    dwc3_config::Config cfg;
    cfg.enable_suspend() = false;
    cfg.bypass_platform_extension() = true;
    args.config(cfg.ToVmo());
  });
  ASSERT_TRUE(start.is_ok());

  dut_.RunInEnvironmentTypeContext([](Environment& env) {
    auto gctl =
        GCTL::Get().FromValue(static_cast<uint32_t>(env.reg_region()[GCTL::Get().addr()].Read()));
    EXPECT_EQ(gctl.PRTCAPDIR(), GCTL::PRTCAPDIR_DEVICE);
  });

  EXPECT_EQ(ZX_OK, dut_.StopDriver().status_value());
}

TEST_F(UnmanagedTestFixture, Usb2OnlyModeConfiguredFromPhyMetadata) {
  auto guctl1_val = std::make_shared<std::atomic<uint32_t>>(0);
  auto dcfg_val = std::make_shared<std::atomic<uint32_t>>(0);

  dut_.RunInEnvironmentTypeContext([guctl1_val, dcfg_val](Environment& env) {
    fphy::Metadata metadata;
    std::vector<fphy::UsbPhyMode> modes;
    modes.push_back(fphy::UsbPhyMode{{
        .protocol = fphy::ProtocolVersion::kUsb20,
        .dr_mode = fphy::Mode::kPeripheral,
        .is_otg_capable = false,
    }});
    metadata.usb_phy_modes() = std::move(modes);
    env.SetPhyMetadata(std::move(metadata));

    env.reg_region()[GUCTL1::Get().addr()].SetWriteCallback(
        [guctl1_val](uint64_t val) { guctl1_val->store(static_cast<uint32_t>(val)); });
    env.reg_region()[DCFG::Get().addr()].SetWriteCallback(
        [dcfg_val](uint64_t val) { dcfg_val->store(static_cast<uint32_t>(val)); });
  });

  zx::result start = dut_.StartDriverWithCustomStartArgs([](fdf::DriverStartArgs& args) {
    dwc3_config::Config cfg;
    cfg.enable_suspend() = false;
    cfg.bypass_platform_extension() = true;
    args.config(cfg.ToVmo());
  });
  ASSERT_OK(start);
  dut_.runtime().RunUntilIdle();
  ASSERT_OK(WaitForPhy());

  dut_.RunInDriverContext(
      [](Dwc3& drv) { EXPECT_EQ(drv.max_speed(), fdescriptor::UsbSpeed::kHigh); });

  auto dci_service = dut_.Connect<fuchsia_hardware_usb_dci::UsbDciService::Device>();
  ASSERT_OK(dci_service.status_value());
  fidl::WireSyncClient<fuchsia_hardware_usb_dci::UsbDci> dci{std::move(*dci_service)};
  ASSERT_OK(dci->StartController().status());

  TriggerConnectionPlugIn(fdescriptor::UsbSpeed::kHigh);

  dut_.runtime().RunUntil([&]() {
    return dut_.RunInDriverContext<bool>(
        [](Dwc3& drv) { return Dwc3TestHelper::GetPowerOn(drv) && Dwc3TestHelper::IsActive(drv); });
  });

  EXPECT_EQ(GUCTL1::Get().FromValue(guctl1_val->load()).DEV_FORCE_20_CLK_FOR_30_CLK(), 1u);
  EXPECT_EQ(DCFG::Get().FromValue(dcfg_val->load()).DEVSPD(), DCFG::DEVSPD_HIGH);

  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, FailedConnectPowersDownNotifiesPhyAndRearmsWatcher) {
  ASSERT_OK(dut_.StartDriverWithCustomStartArgs([](fdf::DriverStartArgs& args) {
                  dwc3_config::Config cfg;
                  cfg.enable_suspend() = false;
                  cfg.bypass_platform_extension() = true;
                  args.config(cfg.ToVmo());
                })
                .status_value());
  ASSERT_OK(WaitForPhy());

  zx::eventpair lease_client, lease_peer;
  ASSERT_OK(zx::eventpair::create(0, &lease_client, &lease_peer));

  size_t disconnected_reports_before = 0;
  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    disconnected_reports_before = env.usb_phy().disconnected_reports();
    env.usb_phy().completion()->Reset();
  });

  // Force DCTL.CSFTRST to stay asserted so ResetHw() times out on connect.
  stuck_reset_test_.store(true);
  auto clear_stuck_reset = fit::defer([this]() { stuck_reset_test_.store(false); });

  dut_.RunInEnvironmentTypeContext(
      [&](Environment& env) { env.usb_phy().TriggerConnection(true, std::move(lease_client)); });
  dut_.runtime().RunUntilIdle();

  // Watcher must re-arm and PHY must receive ConnectStatusChanged(false) with core off.
  ASSERT_OK(WaitForPhy());
  EXPECT_TRUE(dut_.runtime().RunWithTimeoutOrUntil(
      [&]() {
        size_t reports = 0;
        dut_.RunInEnvironmentTypeContext(
            [&](Environment& env) { reports = env.usb_phy().disconnected_reports(); });
        return reports == disconnected_reports_before + 1;
      },
      zx::duration::infinite()));

  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    EXPECT_EQ(env.usb_phy().connected_reports(), 0u);
    EXPECT_EQ(env.usb_phy().disconnected_reports(), disconnected_reports_before + 1);
  });

  dut_.RunInDriverContext([](Dwc3& drv) {
    EXPECT_FALSE(Dwc3TestHelper::GetPowerOn(drv));
    EXPECT_FALSE(Dwc3TestHelper::HasConnectionLease(drv));
    EXPECT_EQ(Dwc3TestHelper::GetConnectFailures(drv), 1u);
  });

  zx_signals_t observed = 0;
  EXPECT_OK(lease_peer.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite(), &observed));
  EXPECT_NE(observed & ZX_EVENTPAIR_PEER_CLOSED, 0u);

  stuck_reset_test_.store(false);
  EXPECT_OK(dut_.StopDriver().status_value());
}

}  // namespace dwc3
