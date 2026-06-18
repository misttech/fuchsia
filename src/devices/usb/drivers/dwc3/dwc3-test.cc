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

#include <fake-mmio-reg/fake-mmio-reg.h>
#include <gtest/gtest.h>
#include <usb/descriptors.h>

#include "lib/driver/fake-platform-device/cpp/fake-pdev.h"
#include "lib/driver/testing/cpp/driver_test.h"
#include "src/devices/usb/drivers/dwc3/dwc3-regs.h"
#include "src/devices/usb/drivers/dwc3/dwc3-test-fixture.h"
#include "src/devices/usb/drivers/dwc3/dwc3_config.h"

namespace dwc3 {

TEST_F(ManagedTestFixture, Dfv2Lifecycle) {
  dut_.RunInNodeContext(
      [&](fdf_testing::TestNode& node) { EXPECT_EQ(1UL, node.children().size()); });
}

TEST_F(UnmanagedTestFixture, ResourcesManagedInStart) {
  dut_.RunInEnvironmentTypeContext(
      [](Environment& env) { env.usb_phy().set_watch_connection_status_changed_called(true); });

  zx::result start = dut_.StartDriverWithCustomStartArgs([](fdf::DriverStartArgs& args) {
    dwc3_config::Config cfg;
    cfg.enable_suspend() = false;
    args.config(cfg.ToVmo());
  });
  ASSERT_TRUE(start.is_ok());

  dut_.RunInEnvironmentTypeContext([](Environment& env) {
    EXPECT_TRUE(env.vreg().enabled());
    EXPECT_FALSE(env.reset().take_toggled());
    EXPECT_TRUE(env.clock().enabled());
  });

  dut_.runtime().RunUntilIdle();
  EXPECT_EQ(ZX_OK, WaitForPhy());

  EXPECT_EQ(ZX_OK, dut_.StopDriver().status_value());
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

TEST_F(ManagedTestFixture, TestInspectMetrics) {
  namespace fdescriptor = fuchsia_hardware_usb_descriptor;
  const uint8_t ep_num = UsbAddressToEpNum(0x02);

  // Dynamic cable connection triggers automatic core wake-up and soft reset.
  dut_.RunInEnvironmentTypeContext(
      [&](Environment& env) { TriggerConnectionPlugIn(env, fdescriptor::UsbSpeed::kSuper); });
  dut_.runtime().RunUntilIdle();

  auto dci_service = dut_.Connect<fuchsia_hardware_usb_dci::UsbDciService::Device>();
  ASSERT_TRUE(dci_service.is_ok())
      << "Failed to connect to UsbDciService: " << dci_service.status_string();
  fidl::WireSyncClient<fuchsia_hardware_usb_dci::UsbDci> dci{std::move(*dci_service)};

  fuchsia_hardware_usb_descriptor::wire::UsbEndpointDescriptor ep_desc{
      .b_length = sizeof(fuchsia_hardware_usb_descriptor::wire::UsbEndpointDescriptor),
      .b_descriptor_type = USB_DT_ENDPOINT,
      .b_endpoint_address = 0x02,  // EP 2 OUT
      .bm_attributes = USB_ENDPOINT_BULK,
      .w_max_packet_size = 1024,
      .b_interval = 0,
  };
  fuchsia_hardware_usb_descriptor::wire::UsbSsEpCompDescriptor ss_comp_desc{
      .b_length = sizeof(fuchsia_hardware_usb_descriptor::wire::UsbSsEpCompDescriptor),
      .b_descriptor_type = USB_DT_SS_EP_COMPANION,
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
    EXPECT_EQ(1u, uep.fifo.GetActiveCount());

    // Assign hardware Resource ID 1 to the active transfer to prevent teardown panics.
    TriggerEpTransferStarted(drv, ep_num, 1);

    pending_hierarchy =
        fpromise::run_single_threaded(inspect::ReadFromInspector(drv.inspector().inspector()))
            .take_value();
  });

  const auto* pending_dwc3 = pending_hierarchy.GetByPath({"dwc3"});
  ASSERT_NE(nullptr, pending_dwc3)
      << "Inspect root node 'dwc3' was not found in pending hierarchy!";

  std::string ep_node_name = std::format("endpoint-0x{:02x}", ep_num);

  const auto* pending_active_trbs =
      pending_dwc3->GetByPath({"endpoints", ep_node_name, "trb_fifo", "active_trbs"});
  ASSERT_NE(nullptr, pending_active_trbs) << "Active TRBs node was not found in pending hierarchy!";
  const auto* trb0 = pending_active_trbs->GetByPath({"0"});
  ASSERT_NE(nullptr, trb0) << "Pending TRB at index 0 was not found!";

  // Assert existence of TRB fields and check value of hardware_owned.
  const auto* ptr_low = trb0->node().get_property<inspect::UintPropertyValue>("ptr_low");
  ASSERT_NE(nullptr, ptr_low) << "Pending TRB field 'ptr_low' was not found!";
  const auto* ptr_high = trb0->node().get_property<inspect::UintPropertyValue>("ptr_high");
  ASSERT_NE(nullptr, ptr_high) << "Pending TRB field 'ptr_high' was not found!";
  const auto* status = trb0->node().get_property<inspect::UintPropertyValue>("status");
  ASSERT_NE(nullptr, status) << "Pending TRB field 'status' was not found!";
  const auto* control = trb0->node().get_property<inspect::UintPropertyValue>("control");
  ASSERT_NE(nullptr, control) << "Pending TRB field 'control' was not found!";

  const auto* hwo = trb0->node().get_property<inspect::BoolPropertyValue>("hardware_owned");
  ASSERT_NE(nullptr, hwo) << "Pending TRB field 'hardware_owned' was not found!";
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
  ASSERT_NE(nullptr, dwc3_node) << "Inspect root node 'dwc3' was not found in completed hierarchy!";

  const auto* time_start = dwc3_node->node().get_property<inspect::UintPropertyValue>("time_start");
  ASSERT_NE(nullptr, time_start) << "Root node property 'time_start' was not found!";

  const auto* history_node = dwc3_node->GetByPath({"event_history"});
  ASSERT_NE(nullptr, history_node) << "Inspect node 'event_history' was not found!";

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
  ASSERT_NE(nullptr, endpoints_node) << "Inspect node 'endpoints' was not found!";

  const auto* ep2_out = endpoints_node->GetByPath({ep_node_name});
  ASSERT_NE(nullptr, ep2_out) << "Endpoint node '" << ep_node_name
                              << "' was not found in completed hierarchy!";

  const auto* type = ep2_out->node().get_property<inspect::UintPropertyValue>("type");
  ASSERT_NE(nullptr, type) << "Endpoint 'type' property was not found!";
  EXPECT_EQ(static_cast<uint64_t>(USB_ENDPOINT_BULK), type->value());

  const auto* enabled = ep2_out->node().get_property<inspect::BoolPropertyValue>("enabled");
  ASSERT_NE(nullptr, enabled) << "Endpoint 'enabled' property was not found!";
  EXPECT_TRUE(enabled->value());

  const auto* transfers =
      ep2_out->node().get_property<inspect::UintPropertyValue>("total_transfers");
  ASSERT_NE(nullptr, transfers) << "Endpoint 'total_transfers' property was not found!";
  EXPECT_EQ(1u, transfers->value());

  const auto* bytes = ep2_out->node().get_property<inspect::UintPropertyValue>("total_bytes");
  ASSERT_NE(nullptr, bytes) << "Endpoint 'total_bytes' property was not found!";
  EXPECT_EQ(1024u, bytes->value());

  const auto* fifo_node = ep2_out->GetByPath({"trb_fifo"});
  ASSERT_NE(nullptr, fifo_node) << "TRB FIFO inspect node 'trb_fifo' was not found!";

  const auto* total_slots =
      fifo_node->node().get_property<inspect::UintPropertyValue>("total_slots");
  ASSERT_NE(nullptr, total_slots) << "FIFO property 'total_slots' was not found!";
  EXPECT_GT(total_slots->value(), 0u);

  const auto* active_trbs = fifo_node->GetByPath({"active_trbs"});
  EXPECT_EQ(nullptr, active_trbs);
}

typedef struct {
  // Full core_id + versioning information.
  uint32_t version_register;

  // True if the driver is expected to start.
  bool should_start;

  // True if the driver is expected to poll CmdAct on EndTransfer commands.
  bool poll_end_xfer;
} Param;

// clang-format off
const auto kCases = testing::Values(
    Param{0x00000000, false, false},
    Param{0xffffffff, false, false},
    Param{0x5500101a, false, false},
    Param{0x5532101a, false, false},
    Param{0x5533101a, true, false},
    Param{0x5533101a, true, false},
    Param{0x5533308a, true, false},
    Param{0x5533309a, true, false},
    Param{0x5533309b, true, false},
    Param{0x5533310a, true, true},  // Driver polls in version 3.10a+
    Param{0x5533310b, true, true},
    Param{0x5533311a, true, true},
    Param{0x55333110, true, true},
    Param{0x5533401a, true, true},
    Param{0x5534101a, false, false});  // 5534 is invalid core id.
// clang-format on

using Parameterized = TestFixture<false, testing::TestWithParam<Param>>;

TEST_P(Parameterized, TestHwVersion) {
  Param p{GetParam()};

  ver_number_ = p.version_register;

  zx::result start = dut_.StartDriverWithCustomStartArgs([](fdf::DriverStartArgs& args) {
    dwc3_config::Config cfg;
    cfg.enable_suspend() = false;
    args.config(cfg.ToVmo());
  });

  ASSERT_EQ(p.should_start, start.is_ok());

  dut_.runtime().RunUntilIdle();
  EXPECT_EQ(ZX_OK, WaitForPhy());

  if (p.should_start) {
    dut_.RunInDriverContext([&](Dwc3& drv) { EXPECT_EQ(p.poll_end_xfer, drv.poll_end_xfer()); });
    EXPECT_EQ(ZX_OK, dut_.StopDriver().status_value());
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

}  // namespace dwc3
