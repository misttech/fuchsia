// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/fit/defer.h>

#include <atomic>

#include "src/devices/usb/drivers/usb-peripheral/usb-peripheral-test-harness.h"

namespace usb_peripheral::test {
namespace {

class UsbPeripheralTestHelper {
 public:
  static size_t active_functions_count(const UsbPeripheral& peripheral) { return 0; }
  static void FunctionCleared(UsbPeripheral& peripheral, size_t function_index,
                              uint64_t config_generation) {
    // TODO: Pass config_generation once the driver backend supports generation fencing.
    peripheral.FunctionCleared(function_index);
  }
};

TEST_F(ManagedUsbPeripheralTest, AddsCorrectSerialNumberMetadata) {
  fdescriptor::wire::UsbSetup setup;
  setup.w_length = 256;
  setup.w_value = 0x3 | (USB_DT_STRING << 8);
  setup.bm_request_type = USB_DIR_IN | USB_RECIP_DEVICE | USB_TYPE_STANDARD;
  setup.b_request = USB_REQ_GET_DESCRIPTOR;

  fidl::Arena arena;
  std::vector<uint8_t> unused;
  auto result =
      dci().buffer(arena)->Control(setup, fidl::VectorView<uint8_t>::FromExternal(unused));

  ASSERT_TRUE(result->is_ok());

  auto& serial = result.value()->read;

  EXPECT_EQ(serial[0], (kSerialNumber.size() + 1) * 2);
  EXPECT_EQ(serial[1], USB_DT_STRING);
  for (size_t i = 0; i < kSerialNumber.size(); i++) {
    EXPECT_EQ(serial[2 + (i * 2)], kSerialNumber[i]);
  }
}

TEST_F(ManagedUsbPeripheralTest, WorksWithVendorSpecificCommandWhenConfigurationIsZero) {
  fdescriptor::wire::UsbSetup setup;
  setup.w_length = 256;
  setup.w_value = 0x3 | (USB_DT_STRING << 8);
  setup.bm_request_type = USB_DIR_IN | USB_RECIP_DEVICE | USB_TYPE_VENDOR;
  setup.b_request = USB_REQ_GET_DESCRIPTOR;

  fidl::Arena arena;
  std::vector<uint8_t> unused;
  auto result =
      dci().buffer(arena)->Control(setup, fidl::VectorView<uint8_t>::FromExternal(unused));
  ASSERT_TRUE(result->is_error());
  ASSERT_EQ(ZX_ERR_BAD_STATE, result->error_value());
}

TEST_F(ManagedUsbPeripheralTest, BosDescriptorVersionHandling) {
  fdescriptor::wire::UsbSetup setup;
  setup.w_length = sizeof(usb_bos_descriptor_t);
  setup.w_value = USB_DT_BOS << 8;
  setup.bm_request_type = USB_DIR_IN | USB_RECIP_DEVICE | USB_TYPE_STANDARD;
  setup.b_request = USB_REQ_GET_DESCRIPTOR;

  // 1. USB 2.0 (0x0200) without BOS capabilities must stall GET_DESCRIPTOR(BOS).
  this->dut().RunInDriverContext(
      [](UsbPeripheral& driver) { driver.SetBcdUsbForTesting(USB_2_0); });
  {
    fidl::Arena arena;
    std::vector<uint8_t> unused;
    auto result =
        dci().buffer(arena)->Control(setup, fidl::VectorView<uint8_t>::FromExternal(unused));
    ASSERT_TRUE(result.ok()) << result.FormatDescription();
    ASSERT_TRUE(result->is_error());
    EXPECT_EQ(ZX_ERR_NOT_SUPPORTED, result->error_value());
  }

  // 2. USB 2.0.1 (0x0201) transition point supports BOS descriptors.
  this->dut().RunInDriverContext(
      [](UsbPeripheral& driver) { driver.SetBcdUsbForTesting(USB_2_0_1); });
  {
    fidl::Arena arena;
    std::vector<uint8_t> unused;
    auto result =
        dci().buffer(arena)->Control(setup, fidl::VectorView<uint8_t>::FromExternal(unused));
    ASSERT_TRUE(result.ok()) << result.FormatDescription();
    ASSERT_TRUE(result->is_ok());
  }

  // 3. USB 2.1 (0x0210) supports BOS descriptors.
  this->dut().RunInDriverContext(
      [](UsbPeripheral& driver) { driver.SetBcdUsbForTesting(USB_2_1); });
  {
    fidl::Arena arena;
    std::vector<uint8_t> unused;
    auto result =
        dci().buffer(arena)->Control(setup, fidl::VectorView<uint8_t>::FromExternal(unused));
    ASSERT_TRUE(result.ok()) << result.FormatDescription();
    ASSERT_TRUE(result->is_ok());
  }

  // 4. USB 3.1 (0x0310) SuperSpeed supports BOS descriptors.
  this->dut().RunInDriverContext(
      [](UsbPeripheral& driver) { driver.SetBcdUsbForTesting(USB_3_1); });
  {
    fidl::Arena arena;
    std::vector<uint8_t> unused;
    auto result =
        dci().buffer(arena)->Control(setup, fidl::VectorView<uint8_t>::FromExternal(unused));
    ASSERT_TRUE(result.ok()) << result.FormatDescription();
    ASSERT_TRUE(result->is_ok());
  }

  // 5. Non-zero descriptor index or w_index must stall even on USB 2.1+ devices.
  this->dut().RunInDriverContext(
      [](UsbPeripheral& driver) { driver.SetBcdUsbForTesting(USB_2_1); });
  {
    fidl::Arena arena;
    std::vector<uint8_t> unused;

    // Non-zero descriptor index ((wValue & 0xFF) != 0).
    setup.w_value = (USB_DT_BOS << 8) | 1;
    setup.w_index = 0;
    auto res_index =
        dci().buffer(arena)->Control(setup, fidl::VectorView<uint8_t>::FromExternal(unused));
    ASSERT_TRUE(res_index.ok()) << res_index.FormatDescription();
    ASSERT_TRUE(res_index->is_error());
    EXPECT_EQ(ZX_ERR_NOT_SUPPORTED, res_index->error_value());

    // Non-zero w_index (w_index != 0).
    setup.w_value = USB_DT_BOS << 8;
    setup.w_index = 1;
    auto res_windex =
        dci().buffer(arena)->Control(setup, fidl::VectorView<uint8_t>::FromExternal(unused));
    ASSERT_TRUE(res_windex.ok()) << res_windex.FormatDescription();
    ASSERT_TRUE(res_windex->is_error());
    EXPECT_EQ(ZX_ERR_NOT_SUPPORTED, res_windex->error_value());
  }
}

TEST_F(UsbPeripheralReadyTest, InspectMetrics) {
  // Initial state should be PeripheralReady.
  {
    inspect::Hierarchy hierarchy;
    this->dut().RunInDriverContext([&](UsbPeripheral& driver) {
      hierarchy = usb_inspect::ReadHierarchyFromInspector(driver.inspector());
    });

    auto* dci_metrics = hierarchy.GetByPath({"usb-peripheral", "dci_metrics"});
    ASSERT_NE(dci_metrics, nullptr);
    EXPECT_THAT(*dci_metrics,
                NodeMatches(AllOf(NameMatches("dci_metrics"),
                                  PropertyList(Contains(StringIs("state", "kPeripheralReady"))))));

    auto* function_node = hierarchy.GetByPath({"usb-peripheral", "function-000"});
    ASSERT_NE(function_node, nullptr);
    EXPECT_THAT(*function_node, NodeMatches(AllOf(NameMatches("function-000"),
                                                  PropertyList(Contains(UintIs("index", 0))))));

    auto* interface_node = hierarchy.GetByPath({"usb-peripheral", "function-000", "interface-000"});
    ASSERT_NE(interface_node, nullptr);
    EXPECT_THAT(*interface_node,
                NodeMatches(AllOf(
                    NameMatches("interface-000"),
                    PropertyList(::testing::UnorderedElementsAre(
                        UintIs("interface_number", ::testing::_), UintIs("alternate_setting", 0),
                        UintIs("num_endpoints", 2), UintIs("interface_class", 255),
                        UintIs("interface_subclass", 0), UintIs("interface_protocol", 0))))));

    auto& ep_children = interface_node->children();
    ASSERT_EQ(ep_children.size(), 2u);
    EXPECT_THAT(ep_children[0],
                NodeMatches(AllOf(PropertyList(Contains(UintIs("attributes", 2))),
                                  PropertyList(Contains(UintIs("max_packet_size", 64))))));
  }

  // Connect host.
  auto connected_res = this->dci()->SetConnected(true);
  ASSERT_TRUE(connected_res.ok());
  ExpectState(UsbPeripheral::DeviceState::kHostConnected);

  // Check Inspect again.
  {
    inspect::Hierarchy hierarchy;
    this->dut().RunInDriverContext([&](UsbPeripheral& driver) {
      hierarchy = usb_inspect::ReadHierarchyFromInspector(driver.inspector());
    });

    auto* dci_metrics = hierarchy.GetByPath({"usb-peripheral", "dci_metrics"});
    ASSERT_NE(dci_metrics, nullptr);
    EXPECT_THAT(*dci_metrics,
                NodeMatches(AllOf(NameMatches("dci_metrics"),
                                  PropertyList(Contains(StringIs("state", "kHostConnected"))))));

    // Check dci_metrics.
    EXPECT_THAT(*dci_metrics,
                NodeMatches(AllOf(NameMatches("dci_metrics"),
                                  PropertyList(Contains(BoolIs("connected", true))))));
  }

  // Disconnect host.
  auto disconnected_res = this->dci()->SetConnected(false);
  ASSERT_TRUE(disconnected_res.ok());
  ExpectState(UsbPeripheral::DeviceState::kPeripheralReady);

  // Check Inspect again.
  {
    inspect::Hierarchy hierarchy;
    this->dut().RunInDriverContext([&](UsbPeripheral& driver) {
      hierarchy = usb_inspect::ReadHierarchyFromInspector(driver.inspector());
    });

    auto* dci_metrics = hierarchy.GetByPath({"usb-peripheral", "dci_metrics"});
    ASSERT_NE(dci_metrics, nullptr);
    EXPECT_THAT(*dci_metrics,
                NodeMatches(AllOf(NameMatches("dci_metrics"),
                                  PropertyList(Contains(StringIs("state", "kPeripheralReady"))))));

    EXPECT_THAT(*dci_metrics,
                NodeMatches(AllOf(NameMatches("dci_metrics"),
                                  PropertyList(Contains(BoolIs("connected", false))))));
  }
}

TEST_F(UsbPeripheralReadyTest, HostConnectionToggle) {
  for (int i = 0; i < 10; ++i) {
    // Connect host.
    auto connected_res = this->dci()->SetConnected(true);
    ASSERT_TRUE(connected_res.ok());
    ExpectState(UsbPeripheral::DeviceState::kHostConnected);

    // Disconnect host.
    auto disconnected_res = this->dci()->SetConnected(false);
    ASSERT_TRUE(disconnected_res.ok());
    ExpectState(UsbPeripheral::DeviceState::kPeripheralReady);
  }
}

TEST_F(UsbPeripheralReadyTest, HostDisconnectResetsConfiguration) {
  // Connect host.
  auto connected_res = this->dci()->SetConnected(true);
  ASSERT_TRUE(connected_res.ok());
  ExpectState(UsbPeripheral::DeviceState::kHostConnected);

  // Set configuration to 1.
  {
    fdescriptor::wire::UsbSetup setup = {
        .bm_request_type = USB_DIR_OUT | USB_TYPE_STANDARD | USB_RECIP_DEVICE,
        .b_request = USB_REQ_SET_CONFIGURATION,
        .w_value = 1,
        .w_index = 0,
        .w_length = 0,
    };
    auto res = dci()->Control(setup, fidl::VectorView<uint8_t>());
    ASSERT_TRUE(res.ok()) << res.FormatDescription();
    ASSERT_TRUE(res->is_ok());
  }

  // Get configuration should be 1.
  {
    fdescriptor::wire::UsbSetup setup = {
        .bm_request_type = USB_DIR_IN | USB_TYPE_STANDARD | USB_RECIP_DEVICE,
        .b_request = USB_REQ_GET_CONFIGURATION,
        .w_value = 0,
        .w_index = 0,
        .w_length = 1,
    };
    auto res = dci()->Control(setup, fidl::VectorView<uint8_t>());
    ASSERT_TRUE(res.ok()) << res.FormatDescription();
    ASSERT_TRUE(res->is_ok());
    ASSERT_EQ(res->value()->read.size(), 1u);
    EXPECT_EQ(res->value()->read[0], 1);
  }

  // Disconnect host.
  auto disconnected_res = this->dci()->SetConnected(false);
  ASSERT_TRUE(disconnected_res.ok());
  ExpectState(UsbPeripheral::DeviceState::kPeripheralReady);

  // Get configuration should be 0.
  {
    fdescriptor::wire::UsbSetup setup = {
        .bm_request_type = USB_DIR_IN | USB_TYPE_STANDARD | USB_RECIP_DEVICE,
        .b_request = USB_REQ_GET_CONFIGURATION,
        .w_value = 0,
        .w_index = 0,
        .w_length = 1,
    };
    auto res = dci()->Control(setup, fidl::VectorView<uint8_t>());
    ASSERT_TRUE(res.ok()) << res.FormatDescription();
    ASSERT_TRUE(res->is_ok());
    ASSERT_EQ(res->value()->read.size(), 1u);
    EXPECT_EQ(res->value()->read[0], 0);
  }
}

TEST_F(UsbPeripheralReadyTest, DisconnectHostWhenAlreadyPeripheralReady) {
  ExpectState(UsbPeripheral::DeviceState::kPeripheralReady);
  FakeUsbFunction& fake = *function_clients_.fakes[0];
  EXPECT_FALSE(fake.set_configured_called());
  EXPECT_FALSE(fake.configured());

  {
    auto disconnected_res = this->dci()->SetConnected(false);
    ASSERT_TRUE(disconnected_res.ok());
  }

  ExpectState(UsbPeripheral::DeviceState::kPeripheralReady);

  fake.WaitUntilCalled();
  EXPECT_TRUE(fake.set_configured_called());
  EXPECT_FALSE(fake.configured());

  fake.clear_set_configured_called();
  {
    auto disconnected_res = this->dci()->SetConnected(false);
    ASSERT_TRUE(disconnected_res.ok());
  }
  ExpectState(UsbPeripheral::DeviceState::kPeripheralReady);

  // Not called again. Run in the fake's dispatcher so we know it would've
  // processed any call made as part of processing SetConnected.
  libsync::Completion comp;
  async::PostTask(fake.dispatcher()->async_dispatcher(), [&]() {
    EXPECT_FALSE(fake.set_configured_called());
    EXPECT_FALSE(fake.configured());
    comp.Signal();
  });
  ASSERT_OK(comp.Wait(zx::sec(5)));
}

TEST_F(UnmanagedUsbPeripheralTest, ClearFunctionsWhenNoneAdded) {
  StartDriverWithConfig(usb_peripheral_config::Config{});

  auto client = this->Client();

  zx::result endpoints = fidl::CreateEndpoints<fperipheral::Events>();
  ASSERT_OK(endpoints);

  FakeEvents fake_events;
  fake_events.Bind(std::move(endpoints->server));

  auto set_listener_res = client->SetStateChangeListener(std::move(endpoints->client));
  ASSERT_TRUE(set_listener_res.ok()) << set_listener_res.FormatDescription();

  // Clear functions - should work immediately.
  auto clear_res = client->ClearFunctions();
  ASSERT_TRUE(clear_res.ok()) << clear_res.FormatDescription();

  fake_events.WaitUntilCleared(this->dut().runtime());
  fake_events.Unbind();

  // Verify Inspect state is kNoConfiguration.
  ExpectState(UsbPeripheral::DeviceState::kNoConfiguration);
}

TEST_F(UnmanagedUsbPeripheralTest, GetConfiguration) {
  StartDriverWithConfig(usb_peripheral_config::Config{});

  auto client = this->Client();

  // Test GetConfiguration before configuration is set returns ZX_ERR_BAD_STATE.
  {
    auto res = client->GetConfiguration();
    ASSERT_TRUE(res.ok()) << res.FormatDescription();
    ASSERT_TRUE(res->is_error());
    EXPECT_EQ(res->error_value(), ZX_ERR_BAD_STATE);
  }

  // Set device descriptor and configuration.
  {
    fperipheral::wire::DeviceDescriptor dev_desc = {
        .bcd_usb = 0x0200,
        .b_device_class = 0,
        .b_device_sub_class = 0,
        .b_device_protocol = 0,
        .b_max_packet_size0 = 64,
        .id_vendor = 0x18d1,
        .id_product = 0xa001,
        .bcd_device = 0x0100,
        .manufacturer = fidl::StringView::FromExternal("Google"),
        .product = fidl::StringView::FromExternal("Fuchsia"),
        .serial = fidl::StringView::FromExternal("12345678"),
        .b_num_configurations = 1,
    };

    fidl::Arena arena;
    fidl::VectorView<fperipheral::wire::FunctionDescriptor> functions(arena, 1);
    functions[0] = fperipheral::wire::FunctionDescriptor{
        .interface_class = 0xff,
        .interface_subclass = 0x00,
        .interface_protocol = 0x00,
    };
    fidl::VectorView<fidl::VectorView<fperipheral::wire::FunctionDescriptor>> configs(arena, 1);
    configs[0] = functions;

    auto set_desc_res = client->SetConfiguration(dev_desc, configs);
    ASSERT_TRUE(set_desc_res.ok()) << set_desc_res.FormatDescription();
    ASSERT_TRUE(set_desc_res->is_ok());
  }

  // Verify GetConfiguration returns the configured descriptors and strings.
  {
    auto res = client->GetConfiguration();
    ASSERT_TRUE(res.ok()) << res.FormatDescription();
    ASSERT_TRUE(res->is_ok());
    const auto& dev_desc = res->value()->device_desc;
    EXPECT_EQ(dev_desc.id_vendor, 0x18d1);
    EXPECT_EQ(dev_desc.id_product, 0xa001);
    EXPECT_EQ(std::string_view(dev_desc.manufacturer.data(), dev_desc.manufacturer.size()),
              "Google");
    EXPECT_EQ(std::string_view(dev_desc.product.data(), dev_desc.product.size()), "Fuchsia");
    EXPECT_EQ(std::string_view(dev_desc.serial.data(), dev_desc.serial.size()), "12345678");
  }
}

TEST_F(UnmanagedUsbPeripheralTest, ClearFunctionsDoubleCallIsNoOp) {
  StartDriverWithConfig(usb_peripheral_config::Config{});

  auto client = this->Client();

  zx::result endpoints = fidl::CreateEndpoints<fperipheral::Events>();
  ASSERT_OK(endpoints);

  FakeEvents fake_events;
  fake_events.Bind(std::move(endpoints->server));

  auto set_listener_res = client->SetStateChangeListener(std::move(endpoints->client));
  ASSERT_TRUE(set_listener_res.ok()) << set_listener_res.FormatDescription();

  // Clear functions once.
  auto clear_res = client->ClearFunctions();
  ASSERT_TRUE(clear_res.ok()) << clear_res.FormatDescription();
  fake_events.WaitUntilCleared(this->dut().runtime());

  // Clear functions twice - should succeed and be a no-op since no functions are bound.
  auto clear_res2 = client->ClearFunctions();
  ASSERT_TRUE(clear_res2.ok()) << clear_res2.FormatDescription();

  fake_events.Unbind();
  ExpectState(UsbPeripheral::DeviceState::kNoConfiguration);
}

TEST_F(UnmanagedUsbPeripheralTest, KbootFunctionsOverrideFunctions) {
  usb_peripheral_config::Config config;
  config.functions() = {"ums"};
  config.kboot_functions() = "cdc,adb";
  StartDriverWithConfig(config);

  fdescriptor::wire::UsbSetup setup;
  setup.w_length = sizeof(usb_device_descriptor_t);
  setup.bm_request_type = USB_DIR_IN | USB_RECIP_DEVICE | USB_TYPE_STANDARD;
  setup.b_request = USB_REQ_GET_DESCRIPTOR;
  setup.w_value = USB_DT_DEVICE << 8;
  setup.w_index = 0;
  setup.w_length = sizeof(usb_device_descriptor_t);

  fidl::Arena arena;
  std::vector<uint8_t> unused;
  fidl::WireUnownedResult result =
      dci().buffer(arena)->Control(setup, fidl::VectorView<uint8_t>::FromExternal(unused));

  ASSERT_TRUE(result.ok());
  ASSERT_FALSE(result->is_error());
  ASSERT_EQ(sizeof(usb_device_descriptor_t), result->value()->read.size());

  usb_device_descriptor_t desc;
  std::memcpy(&desc, result->value()->read.data(), sizeof(usb_device_descriptor_t));

  // Determined by config.kboot_functions() above.
  ASSERT_EQ(GOOGLE_USB_CDC_AND_ADB_PID, desc.id_product);
}

TEST_F(UnmanagedUsbPeripheralTest, ClearFunctionsWhenAlreadyUnbound) {
  usb_peripheral_config::Config config;
  config.functions() = {"test", "cdc"};
  StartDriverWithConfig(config);

  auto client = Client();

  zx::result endpoints = fidl::CreateEndpoints<fperipheral::Events>();
  ASSERT_OK(endpoints);

  FakeEvents fake_events;
  fake_events.Bind(std::move(endpoints->server));

  auto set_listener_res = client->SetStateChangeListener(std::move(endpoints->client));
  ASSERT_TRUE(set_listener_res.ok()) << set_listener_res.FormatDescription();

  // Trigger framework-level removal for BOTH function-000 and function-001.
  SimulateFunctionUnbind({"function-000", "function-001"});

  // Process unbind handlers.
  this->dut().runtime().RunUntilIdle();

  // The peripheral driver should detect the spontaneous unbind and drop to kWaitForFunctionBind.
  ExpectState(UsbPeripheral::DeviceState::kWaitForFunctionBind);
  ExpectControllerStarted(false);

  // Verify that the framework has actually removed the nodes.
  WaitUntilChildNodeCount(0);

  // Call ClearFunctions. Since they are already unbound, this will return immediately.
  auto clear_res = client->ClearFunctions();
  ASSERT_TRUE(clear_res.ok()) << clear_res.FormatDescription();
  ExpectState(UsbPeripheral::DeviceState::kNoConfiguration);
  ExpectControllerStarted(false);

  // Verify that the node count remains zero.
  ExpectChildNodeCount(0);

  fake_events.WaitUntilCleared(this->dut().runtime());
  fake_events.Unbind();

  ExpectState(UsbPeripheral::DeviceState::kNoConfiguration);
}

TEST_F(UnmanagedUsbPeripheralTest, ClearFunctionsDuringBind) {
  usb_peripheral_config::Config config;
  config.functions() = {"test"};
  StartDriverWithConfig(config);

  auto client = Client();

  // We are now in kWaitForFunctionBind (as child devices were added).
  ExpectState(UsbPeripheral::DeviceState::kWaitForFunctionBind);

  // Call ClearFunctions before any function driver has registered.
  auto clear_res = client->ClearFunctions();
  ASSERT_TRUE(clear_res.ok()) << clear_res.FormatDescription();

  // Process the unbinds.
  this->dut().runtime().RunUntilIdle();

  // Should reach kNoConfiguration.
  ExpectState(UsbPeripheral::DeviceState::kNoConfiguration);
}

TEST_F(UnmanagedUsbPeripheralReadyTest, FunctionInterfaceClosedButNodeBound) {
  usb_peripheral_config::Config config;
  config.functions() = {"test"};
  StartDriverWithConfig(config);

  auto function_clients = TransitionToPeripheralReady();
  ASSERT_OK(function_clients);

  // Simulate a function driver closing its interface.
  {
    function_clients.value().clients[0] = {};
  }

  // The peripheral driver should detect this and drop back to kWaitForFunctionBind.
  WaitUntilState(UsbPeripheral::DeviceState::kWaitForFunctionBind);
}

TEST_F(UnmanagedUsbPeripheralReadyTest, FunctionInterfaceClosedInHostConnectedState) {
  usb_peripheral_config::Config config;
  config.functions() = {"test"};
  StartDriverWithConfig(config);

  auto function_clients = TransitionToPeripheralReady();
  ASSERT_OK(function_clients);

  // Connect host using the DCI interface.
  auto connected_res = this->dci()->SetConnected(true);
  ASSERT_TRUE(connected_res.ok()) << connected_res.FormatDescription();
  ExpectState(UsbPeripheral::DeviceState::kHostConnected);

  // Simulate a function driver closing its interface.
  {
    function_clients.value().clients[0] = {};
  }

  // The peripheral driver should detect this, take the peripheral offline,
  // and drop back to kWaitForFunctionBind (even if host was connected).
  WaitUntilState(UsbPeripheral::DeviceState::kWaitForFunctionBind);

  // Check that the controller was stopped.
  ExpectControllerStarted(false);
}

TEST_F(UnmanagedUsbPeripheralReadyTest, FunctionUnbindDuringTeardown) {
  usb_peripheral_config::Config config;
  config.functions() = {"test", "cdc"};
  StartDriverWithConfig(config);

  auto client = Client();

  auto function_clients = TransitionToPeripheralReady(2);
  ASSERT_OK(function_clients);

  ExpectState(UsbPeripheral::DeviceState::kPeripheralReady);

  // Trigger an unsolicited unbind for one function.
  SimulateFunctionUnbind({"function-000"});

  // Start ClearFunctions.
  auto clear_res = client->ClearFunctions();
  ASSERT_TRUE(clear_res.ok()) << clear_res.FormatDescription();

  WaitUntilState(UsbPeripheral::DeviceState::kNoConfiguration);
}

TEST_F(UnmanagedUsbPeripheralReadyTest, FaultyFunctionInterfaceReset) {
  usb_peripheral_config::Config config;
  config.functions() = {"test"};
  StartDriverWithConfig(config);

  auto function_clients = TransitionToPeripheralReady();
  ASSERT_OK(function_clients);

  // Faulty function driver closes its interface but doesn't unbind the node.
  {
    function_clients.value().clients[0] = {};
  }

  // The peripheral driver should detect this and drop back to kWaitForFunctionBind.
  WaitUntilState(UsbPeripheral::DeviceState::kWaitForFunctionBind);

  // Restore again by re-configuring.
  {
    auto peripheral_client = ConnectPeripheral();
    ASSERT_OK(peripheral_client);

    auto clear_res = peripheral_client.value()->ClearFunctions();
    ASSERT_TRUE(clear_res.ok()) << clear_res.FormatDescription();

    fperipheral::wire::DeviceDescriptor device_desc = CreateTestDeviceDescriptor();

    fidl::Arena arena;
    auto configs = CreateTestFunctionDescriptors(arena);

    auto set_config_res = peripheral_client.value()->SetConfiguration(device_desc, configs);
    ASSERT_TRUE(set_config_res.ok()) << set_config_res.FormatDescription();
    ASSERT_TRUE(set_config_res->is_ok()) << zx_status_get_string(set_config_res->error_value());

    auto function_clients = TransitionToPeripheralReady();
    ASSERT_OK(function_clients);
  }
}

TEST_F(UnmanagedUsbPeripheralTest, PartialFunctionRegistration) {
  usb_peripheral_config::Config config;
  config.functions() = {"test", "cdc"};
  StartDriverWithConfig(config);

  // We should be in kWaitForFunctionBind because only 0/2 functions are registered.
  ExpectState(UsbPeripheral::DeviceState::kWaitForFunctionBind);

  // Bind one function.
  zx::result function_client = ConnectFunction();
  ASSERT_OK(function_client);

  // Still kWaitForFunctionBind (1/2 registered).
  ExpectState(UsbPeripheral::DeviceState::kWaitForFunctionBind);

  // Unbind that one function by closing the client.
  {
    function_client.value() = {};
  }

  // We can't wait for FunctionsCleared here because one function is still registered (virtually).
  // But we can wait for the state to be stable.
  this->dut().runtime().RunUntilIdle();
  ExpectState(UsbPeripheral::DeviceState::kWaitForFunctionBind);
}

TEST_F(UnmanagedUsbPeripheralTest, PrepareStopTransitionFromNoConfiguration) {
  // Driver started without config reaches kNoConfiguration.
  StartDriverWithConfig({});
  // TearDown will call StopDriver which will in turn call PrepareStop and verify controller is
  // stopped.
}

TEST_F(UnmanagedUsbPeripheralTest, PrepareStopTransitionFromWaitForFunctionBind) {
  usb_peripheral_config::Config config;
  config.functions() = {"test"};
  StartDriverWithConfig(config);
  this->dut().runtime().RunUntilIdle();
  ExpectState(UsbPeripheral::DeviceState::kWaitForFunctionBind);

  // TearDown will call StopDriver which will in turn call PrepareStop and verify controller is
  // stopped.
}

TEST_F(UnmanagedUsbPeripheralTest, ClearFunctionsFromNoConfiguration) {
  StartDriverWithConfig(usb_peripheral_config::Config{});
  auto client = Client();
  auto clear_res = client->ClearFunctions();
  ASSERT_TRUE(clear_res.ok()) << clear_res.FormatDescription();

  this->dut().runtime().RunUntilIdle();
  ExpectState(UsbPeripheral::DeviceState::kNoConfiguration);
}

TEST_F(UnmanagedUsbPeripheralTest, ClearFunctionsFromWaitForFunctionBind) {
  usb_peripheral_config::Config config;
  config.functions() = {"test"};
  StartDriverWithConfig(config);
  this->dut().runtime().RunUntilIdle();
  ExpectState(UsbPeripheral::DeviceState::kWaitForFunctionBind);

  auto client = Client();
  auto clear_res = client->ClearFunctions();
  ASSERT_TRUE(clear_res.ok()) << clear_res.FormatDescription();

  this->dut().runtime().RunUntilIdle();
  ExpectState(UsbPeripheral::DeviceState::kNoConfiguration);

  ExpectControllerStarted(false);
}

TEST_F(UnmanagedUsbPeripheralReadyTest, ClearFunctionsFromPeripheralReady) {
  usb_peripheral_config::Config config;
  config.functions() = {"test"};
  StartDriverWithConfig(config);
  auto function_clients = TransitionToPeripheralReady();
  ASSERT_OK(function_clients);
  ExpectState(UsbPeripheral::DeviceState::kPeripheralReady);

  auto client = Client();
  auto clear_res = client->ClearFunctions();
  ASSERT_TRUE(clear_res.ok()) << clear_res.FormatDescription();

  this->dut().runtime().RunUntilIdle();
  ExpectState(UsbPeripheral::DeviceState::kNoConfiguration);

  ExpectControllerStarted(false);
}

TEST_F(UnmanagedUsbPeripheralReadyTest, ClearFunctionsFromHostConnected) {
  usb_peripheral_config::Config config;
  config.functions() = {"test"};
  StartDriverWithConfig(config);
  auto function_clients = TransitionToPeripheralReady();
  ASSERT_OK(function_clients);
  ExpectState(UsbPeripheral::DeviceState::kPeripheralReady);

  auto connected_res = this->dci()->SetConnected(true);
  ASSERT_TRUE(connected_res.ok()) << connected_res.FormatDescription();
  ExpectState(UsbPeripheral::DeviceState::kHostConnected);

  auto client = Client();
  auto clear_res = client->ClearFunctions();
  ASSERT_TRUE(clear_res.ok()) << clear_res.FormatDescription();

  this->dut().runtime().RunUntilIdle();
  ExpectState(UsbPeripheral::DeviceState::kNoConfiguration);

  ExpectControllerStarted(false);
}

TEST_F(UnmanagedUsbPeripheralReadyTest, PrepareStopTransitionFromPeripheralReady) {
  usb_peripheral_config::Config config;
  config.functions() = {"test"};
  StartDriverWithConfig(config);
  this->dut().runtime().RunUntilIdle();
  auto function_clients = TransitionToPeripheralReady();
  ASSERT_OK(function_clients);
  ExpectState(UsbPeripheral::DeviceState::kPeripheralReady);

  // TearDown will call StopDriver which will in turn call PrepareStop and verify controller is
  // stopped.
}

TEST_F(UnmanagedUsbPeripheralReadyTest, PrepareStopTransitionFromHostConnected) {
  usb_peripheral_config::Config config;
  config.functions() = {"test"};
  StartDriverWithConfig(config);
  this->dut().runtime().RunUntilIdle();
  auto function_clients = TransitionToPeripheralReady();
  ASSERT_OK(function_clients);
  ExpectState(UsbPeripheral::DeviceState::kPeripheralReady);

  auto connected_res = this->dci()->SetConnected(true);
  ASSERT_TRUE(connected_res.ok()) << connected_res.FormatDescription();
  ExpectState(UsbPeripheral::DeviceState::kHostConnected);

  // TearDown will call StopDriver which will in turn call PrepareStop and verify controller is
  // stopped.
}

TEST_F(UnmanagedUsbPeripheralTest, PartialFunctionNodeUnbind) {
  usb_peripheral_config::Config config;
  config.functions() = {"test", "cdc"};
  StartDriverWithConfig(config);

  // We should be in kWaitForFunctionBind because only 0/2 functions are registered.
  ExpectState(UsbPeripheral::DeviceState::kWaitForFunctionBind);

  // Bind one function (by connecting its interface). State remains kWaitForFunctionBind.
  {
    zx::result function_client = ConnectFunction();
    ASSERT_OK(function_client);
  }
  ExpectState(UsbPeripheral::DeviceState::kWaitForFunctionBind);

  // Simulate one function provider (the child node) unbinding entirely.
  // This is different from just closing the UsbFunction interface.
  SimulateFunctionUnbind({"function-001"});

  // State should stay kWaitForFunctionBind.
  this->dut().runtime().RunUntilIdle();
  ExpectState(UsbPeripheral::DeviceState::kWaitForFunctionBind);
}

TEST_F(UnmanagedUsbPeripheralReadyTest, ClearFunctionsAfterPartialFunctionUnbind) {
  usb_peripheral_config::Config config;
  config.functions() = {"test", "cdc"};
  StartDriverWithConfig(config);

  auto function_clients = TransitionToPeripheralReady(2);
  ASSERT_OK(function_clients);

  auto client = Client();

  // Simulate one function provider (the child node) unbinding.
  SimulateFunctionUnbind({"function-000"});

  // State should drop to kWaitForFunctionBind.
  WaitUntilState(UsbPeripheral::DeviceState::kWaitForFunctionBind);

  // Call ClearFunctions.
  auto clear_res = client->ClearFunctions();
  ASSERT_TRUE(clear_res.ok()) << clear_res.FormatDescription();

  WaitUntilState(UsbPeripheral::DeviceState::kNoConfiguration);
}

TEST_F(UnmanagedUsbPeripheralReadyTest, FunctionNodeUnbindInHostConnectedState) {
  usb_peripheral_config::Config config;
  config.functions() = {"test"};
  StartDriverWithConfig(config);

  auto function_clients = TransitionToPeripheralReady();
  ASSERT_OK(function_clients);

  // Connect host.
  auto connected_res = this->dci()->SetConnected(true);
  ASSERT_TRUE(connected_res.ok()) << connected_res.FormatDescription();
  ExpectState(UsbPeripheral::DeviceState::kHostConnected);

  // Simulate function node unbind.
  SimulateFunctionUnbind({"function-000"});

  // Peripheral should go offline and wait for functions.
  WaitUntilState(UsbPeripheral::DeviceState::kWaitForFunctionBind);

  // Controller should be stopped.
  ExpectControllerStarted(false);
}

TEST_F(UnmanagedUsbPeripheralReadyTest, CoordinatedCompositeTeardown) {
  // 1. Setup a multi-function composite device profile matching real watch configurations
  usb_peripheral_config::Config config;
  config.functions() = {"cdc", "test"};
  StartDriverWithConfig(config);

  auto function_clients = TransitionToPeripheralReady(2);
  ASSERT_OK(function_clients);

  // Connect host to transition to Host Connected state
  auto connected_res = this->dci()->SetConnected(true);
  ASSERT_TRUE(connected_res.ok()) << connected_res.FormatDescription();
  ExpectState(UsbPeripheral::DeviceState::kHostConnected);

  // 2. Simulate unsolicited unbind on the composite device (both function-000 and function-001)
  SimulateFunctionUnbind({"function-000", "function-001"});

  // 3. STATE AND TOPOLOGY VERIFICATION:
  // The parent must command RequestRemoval() on both nodes to sweep the layout cleanly.
  WaitUntilState(UsbPeripheral::DeviceState::kWaitForFunctionBind);
  ExpectControllerStarted(false);
  // Let mock framework process the node unbind events asynchronously on the loop.
  dut().runtime().RunUntilIdle();
}

TEST_F(UnmanagedUsbPeripheralReadyTest, FaultyFunctionNodeUnbindReset) {
  usb_peripheral_config::Config config;
  config.functions() = {"test"};
  StartDriverWithConfig(config);

  auto function_clients = TransitionToPeripheralReady();
  ASSERT_OK(function_clients);

  auto fake_events = std::make_shared<FakeEvents>();
  this->RegisterFakeEvents(fake_events);

  // Simulate repeated node unbinds.
  for (int i = 0; i < 10; i++) {
    SimulateFunctionUnbind({"function-000"});

    WaitUntilState(UsbPeripheral::DeviceState::kWaitForFunctionBind);
    ExpectControllerStarted(false);
    WaitUntilChildNodeCount(0);

    // Restore again by re-configuring.
    {
      auto peripheral_client = ConnectPeripheral();
      ASSERT_OK(peripheral_client);

      auto clear_res = peripheral_client.value()->ClearFunctions();
      ASSERT_TRUE(clear_res.ok()) << clear_res.FormatDescription();

      fperipheral::wire::DeviceDescriptor device_desc = CreateTestDeviceDescriptor();

      fidl::Arena arena;
      auto configs = CreateTestFunctionDescriptors(arena);

      auto set_config_res = peripheral_client.value()->SetConfiguration(device_desc, configs);
      ASSERT_TRUE(set_config_res.ok()) << set_config_res.FormatDescription();
      ASSERT_TRUE(set_config_res->is_ok()) << zx_status_get_string(set_config_res->error_value());

      auto function_clients = TransitionToPeripheralReady();
      ASSERT_OK(function_clients);
    }
  }

  fake_events->WaitUntilCleared(this->dut().runtime());
}

TEST_F(UnmanagedUsbPeripheralReadyTest, CheckAndStartControllerGuard) {
  usb_peripheral_config::Config config;
  config.functions() = {"test"};
  StartDriverWithConfig(config);

  auto function_clients = TransitionToPeripheralReady();
  ASSERT_OK(function_clients);

  // Verify initial state.
  this->dut().RunInDriverContext([&](UsbPeripheral& peripheral) {
    EXPECT_EQ(peripheral.SnapshotState(), UsbPeripheral::DeviceState::kPeripheralReady);
  });

  // Call CheckAndStartController again.
  // This is to check for a very rare race condition where the function register calls occur in
  // parallel and the peripheral has already started the controller.
  // It's hard to simulate this race condition, so we just call CheckAndStartController again
  // to be safe.
  // This race condition would go away once we move to a single dispatcher.
  this->dut().RunInDriverContext([&](UsbPeripheral& peripheral) {
    // This should do nothing and return ZX_OK because state is not kWaitForFunctionBind.
    EXPECT_OK(peripheral.CheckAndStartController());
  });

  // Verify state is still kPeripheralReady.
  this->dut().RunInDriverContext([&](UsbPeripheral& peripheral) {
    EXPECT_EQ(peripheral.SnapshotState(), UsbPeripheral::DeviceState::kPeripheralReady);
  });
}

TEST_F(UnmanagedUsbPeripheralTest, UnconfiguredRequestTests) {
  StartDriverWithConfig(usb_peripheral_config::Config{});

  // 1. Test Control request with USB_RECIP_INTERFACE before configuration.
  {
    fdescriptor::wire::UsbSetup setup = {
        .bm_request_type = USB_DIR_IN | USB_TYPE_STANDARD | USB_RECIP_INTERFACE,
        .b_request = USB_REQ_GET_STATUS,
        .w_value = 0,
        .w_index = 0,
        .w_length = 2,
    };

    auto res = dci()->Control(setup, fidl::VectorView<uint8_t>());
    ASSERT_TRUE(res.ok()) << res.FormatDescription();
    ASSERT_TRUE(res->is_error());
    EXPECT_EQ(res->error_value(), ZX_ERR_BAD_STATE);
  }

  // 1b. Test GET_STATUS for USB_RECIP_DEVICE before configuration.
  {
    fdescriptor::wire::UsbSetup setup = {
        .bm_request_type = USB_DIR_IN | USB_TYPE_STANDARD | USB_RECIP_DEVICE,
        .b_request = USB_REQ_GET_STATUS,
        .w_value = 0,
        .w_index = 0,
        .w_length = 2,
    };

    auto res = dci()->Control(setup, fidl::VectorView<uint8_t>());
    ASSERT_TRUE(res.ok()) << res.FormatDescription();
    ASSERT_TRUE(res->is_ok());
    ASSERT_EQ(res->value()->read.size(), 2u);
    EXPECT_EQ(res->value()->read[0] & (1 << USB_DEVICE_SELF_POWERED), 1 << USB_DEVICE_SELF_POWERED);
    EXPECT_EQ(res->value()->read[1], 0);
  }

  // 1c. Test GET_STATUS for USB_RECIP_ENDPOINT (EP0 w_index=0) before configuration.
  {
    fdescriptor::wire::UsbSetup setup = {
        .bm_request_type = USB_DIR_IN | USB_TYPE_STANDARD | USB_RECIP_ENDPOINT,
        .b_request = USB_REQ_GET_STATUS,
        .w_value = 0,
        .w_index = 0,
        .w_length = 2,
    };

    auto res = dci()->Control(setup, fidl::VectorView<uint8_t>());
    ASSERT_TRUE(res.ok()) << res.FormatDescription();
    ASSERT_TRUE(res->is_ok());
    ASSERT_EQ(res->value()->read.size(), 2u);
    EXPECT_EQ(res->value()->read[0], 0);
    EXPECT_EQ(res->value()->read[1], 0);
  }

  // 1d. Test GET_STATUS for USB_RECIP_ENDPOINT (non-zero w_index=1) before configuration.
  {
    fdescriptor::wire::UsbSetup setup = {
        .bm_request_type = USB_DIR_IN | USB_TYPE_STANDARD | USB_RECIP_ENDPOINT,
        .b_request = USB_REQ_GET_STATUS,
        .w_value = 0,
        .w_index = 1,
        .w_length = 2,
    };

    auto res = dci()->Control(setup, fidl::VectorView<uint8_t>());
    ASSERT_TRUE(res.ok()) << res.FormatDescription();
    ASSERT_TRUE(res->is_error());
    EXPECT_EQ(res->error_value(), ZX_ERR_BAD_STATE);
  }

  // 1e. Test SET_FEATURE(USB_ENDPOINT_HALT) on non-zero w_index=1 before configuration.
  {
    fdescriptor::wire::UsbSetup setup = {
        .bm_request_type = USB_DIR_OUT | USB_TYPE_STANDARD | USB_RECIP_ENDPOINT,
        .b_request = USB_REQ_SET_FEATURE,
        .w_value = USB_ENDPOINT_HALT,
        .w_index = 1,
        .w_length = 0,
    };

    auto res = dci()->Control(setup, fidl::VectorView<uint8_t>());
    ASSERT_TRUE(res.ok()) << res.FormatDescription();
    ASSERT_TRUE(res->is_error());
    EXPECT_EQ(res->error_value(), ZX_ERR_BAD_STATE);
  }

  // 1f. Test CLEAR_FEATURE(USB_ENDPOINT_HALT) on non-zero w_index=1 before configuration.
  {
    fdescriptor::wire::UsbSetup setup = {
        .bm_request_type = USB_DIR_OUT | USB_TYPE_STANDARD | USB_RECIP_ENDPOINT,
        .b_request = USB_REQ_CLEAR_FEATURE,
        .w_value = USB_ENDPOINT_HALT,
        .w_index = 1,
        .w_length = 0,
    };

    auto res = dci()->Control(setup, fidl::VectorView<uint8_t>());
    ASSERT_TRUE(res.ok()) << res.FormatDescription();
    ASSERT_TRUE(res->is_error());
    EXPECT_EQ(res->error_value(), ZX_ERR_BAD_STATE);
  }

  // 2. Test SetInterface (via CommonControl) before configuration.
  {
    fdescriptor::wire::UsbSetup setup = {
        .bm_request_type = USB_DIR_OUT | USB_TYPE_STANDARD | USB_RECIP_INTERFACE,
        .b_request = USB_REQ_SET_INTERFACE,
        .w_value = 1,  // alt setting
        .w_index = 0,  // interface
        .w_length = 0,
    };

    auto res = dci()->Control(setup, fidl::VectorView<uint8_t>());
    ASSERT_TRUE(res.ok()) << res.FormatDescription();
    ASSERT_TRUE(res->is_error());
    EXPECT_EQ(res->error_value(), ZX_ERR_BAD_STATE);
  }

  // 3. Test SetConfiguration with invalid index (1) when 0 configs exist.
  {
    fdescriptor::wire::UsbSetup setup = {
        .bm_request_type = USB_DIR_OUT | USB_TYPE_STANDARD | USB_RECIP_DEVICE,
        .b_request = USB_REQ_SET_CONFIGURATION,
        .w_value = 1,  // config 1
        .w_index = 0,
        .w_length = 0,
    };

    auto res = dci()->Control(setup, fidl::VectorView<uint8_t>());
    ASSERT_TRUE(res.ok()) << res.FormatDescription();
    ASSERT_TRUE(res->is_error());
    EXPECT_EQ(res->error_value(), ZX_ERR_INVALID_ARGS);
  }

  // 4. Test SetConfiguration with 0 (unconfigure) when 0 configs exist.
  {
    fdescriptor::wire::UsbSetup setup = {
        .bm_request_type = USB_DIR_OUT | USB_TYPE_STANDARD | USB_RECIP_DEVICE,
        .b_request = USB_REQ_SET_CONFIGURATION,
        .w_value = 0,  // config 0
        .w_index = 0,
        .w_length = 0,
    };

    auto res = dci()->Control(setup, fidl::VectorView<uint8_t>());
    ASSERT_TRUE(res.ok()) << res.FormatDescription();
    ASSERT_TRUE(res->is_ok());
  }

  // 5. Test GET_DESCRIPTOR(USB_DT_BOS) behavior across USB versions on unconfigured device.
  {
    fdescriptor::wire::UsbSetup setup = {
        .bm_request_type = USB_DIR_IN | USB_TYPE_STANDARD | USB_RECIP_DEVICE,
        .b_request = USB_REQ_GET_DESCRIPTOR,
        .w_value = USB_DT_BOS << 8,
        .w_index = 0,
        .w_length = sizeof(usb_bos_descriptor_t),
    };

    // Stalls for USB 2.0.
    this->dut().RunInDriverContext(
        [](UsbPeripheral& driver) { driver.SetBcdUsbForTesting(USB_2_0); });
    auto res_2_0 = dci()->Control(setup, fidl::VectorView<uint8_t>());
    ASSERT_TRUE(res_2_0.ok()) << res_2_0.FormatDescription();
    ASSERT_TRUE(res_2_0->is_error());
    EXPECT_EQ(res_2_0->error_value(), ZX_ERR_NOT_SUPPORTED);

    // Succeeds for USB 2.1.
    this->dut().RunInDriverContext(
        [](UsbPeripheral& driver) { driver.SetBcdUsbForTesting(USB_2_1); });
    auto res_2_1 = dci()->Control(setup, fidl::VectorView<uint8_t>());
    ASSERT_TRUE(res_2_1.ok()) << res_2_1.FormatDescription();
    ASSERT_TRUE(res_2_1->is_ok());

    // Succeeds for USB 3.1.
    this->dut().RunInDriverContext(
        [](UsbPeripheral& driver) { driver.SetBcdUsbForTesting(USB_3_1); });
    auto res_3_1 = dci()->Control(setup, fidl::VectorView<uint8_t>());
    ASSERT_TRUE(res_3_1.ok()) << res_3_1.FormatDescription();
    ASSERT_TRUE(res_3_1->is_ok());

    // Non-zero descriptor index ((wValue & 0xFF) != 0) must stall.
    setup.w_value = (USB_DT_BOS << 8) | 1;
    setup.w_index = 0;
    auto res_inv_idx = dci()->Control(setup, fidl::VectorView<uint8_t>());
    ASSERT_TRUE(res_inv_idx.ok()) << res_inv_idx.FormatDescription();
    ASSERT_TRUE(res_inv_idx->is_error());
    EXPECT_EQ(res_inv_idx->error_value(), ZX_ERR_NOT_SUPPORTED);

    // Non-zero w_index must stall.
    setup.w_value = USB_DT_BOS << 8;
    setup.w_index = 1;
    auto res_inv_widx = dci()->Control(setup, fidl::VectorView<uint8_t>());
    ASSERT_TRUE(res_inv_widx.ok()) << res_inv_widx.FormatDescription();
    ASSERT_TRUE(res_inv_widx->is_error());
    EXPECT_EQ(res_inv_widx->error_value(), ZX_ERR_NOT_SUPPORTED);
  }
}

TEST_F(UnmanagedUsbPeripheralReadyTest, InvalidConfigurationTest) {
  usb_peripheral_config::Config config;
  config.functions() = {"test"};
  StartDriverWithConfig(config);

  auto function_clients = TransitionToPeripheralReady();
  ASSERT_OK(function_clients);

  // Test SetConfiguration with invalid index (2) when 1 config exists.
  {
    fdescriptor::wire::UsbSetup setup = {
        .bm_request_type = USB_DIR_OUT | USB_TYPE_STANDARD | USB_RECIP_DEVICE,
        .b_request = USB_REQ_SET_CONFIGURATION,
        .w_value = 2,  // config 2 (invalid)
        .w_index = 0,
        .w_length = 0,
    };

    auto res = dci()->Control(setup, fidl::VectorView<uint8_t>());
    ASSERT_TRUE(res.ok()) << res.FormatDescription();
    ASSERT_TRUE(res->is_error());
    EXPECT_EQ(res->error_value(), ZX_ERR_INVALID_ARGS);
  }
}

TEST_F(UnmanagedUsbPeripheralReadyTest, ConfiguredGetStatusTests) {
  usb_peripheral_config::Config config;
  config.functions() = {"test"};
  StartDriverWithConfig(config);

  auto function_clients = TransitionToPeripheralReady();
  ASSERT_OK(function_clients);

  // Transition to configured state (configuration 1).
  {
    fdescriptor::wire::UsbSetup setup = {
        .bm_request_type = USB_DIR_OUT | USB_TYPE_STANDARD | USB_RECIP_DEVICE,
        .b_request = USB_REQ_SET_CONFIGURATION,
        .w_value = 1,
        .w_index = 0,
        .w_length = 0,
    };
    auto res = dci()->Control(setup, fidl::VectorView<uint8_t>());
    ASSERT_TRUE(res.ok()) << res.FormatDescription();
    ASSERT_TRUE(res->is_ok());
  }

  // 1. Test GET_STATUS for USB_RECIP_DEVICE in configured state.
  {
    fdescriptor::wire::UsbSetup setup = {
        .bm_request_type = USB_DIR_IN | USB_TYPE_STANDARD | USB_RECIP_DEVICE,
        .b_request = USB_REQ_GET_STATUS,
        .w_value = 0,
        .w_index = 0,
        .w_length = 2,
    };
    auto res = dci()->Control(setup, fidl::VectorView<uint8_t>());
    ASSERT_TRUE(res.ok()) << res.FormatDescription();
    ASSERT_TRUE(res->is_ok());
    ASSERT_EQ(res->value()->read.size(), 2u);
    EXPECT_EQ(res->value()->read[0] & (1 << USB_DEVICE_SELF_POWERED), 1 << USB_DEVICE_SELF_POWERED);
    EXPECT_EQ(res->value()->read[1], 0);
  }

  // 2. Test GET_STATUS for USB_RECIP_INTERFACE (valid interface 0) in configured state.
  {
    fdescriptor::wire::UsbSetup setup = {
        .bm_request_type = USB_DIR_IN | USB_TYPE_STANDARD | USB_RECIP_INTERFACE,
        .b_request = USB_REQ_GET_STATUS,
        .w_value = 0,
        .w_index = 0,  // interface 0
        .w_length = 2,
    };
    auto res = dci()->Control(setup, fidl::VectorView<uint8_t>());
    ASSERT_TRUE(res.ok()) << res.FormatDescription();
    ASSERT_TRUE(res->is_ok());
    ASSERT_EQ(res->value()->read.size(), 2u);
    EXPECT_EQ(res->value()->read[0], 0);
    EXPECT_EQ(res->value()->read[1], 0);
  }

  // 3. Test GET_STATUS for USB_RECIP_INTERFACE (invalid interface 5) in configured state.
  {
    fdescriptor::wire::UsbSetup setup = {
        .bm_request_type = USB_DIR_IN | USB_TYPE_STANDARD | USB_RECIP_INTERFACE,
        .b_request = USB_REQ_GET_STATUS,
        .w_value = 0,
        .w_index = 5,  // out of range
        .w_length = 2,
    };
    auto res = dci()->Control(setup, fidl::VectorView<uint8_t>());
    ASSERT_TRUE(res.ok()) << res.FormatDescription();
    ASSERT_TRUE(res->is_error());
    EXPECT_EQ(res->error_value(), ZX_ERR_OUT_OF_RANGE);
  }

  // 4. Test GET_STATUS for USB_RECIP_ENDPOINT (EP0 control endpoint) in configured state.
  {
    fdescriptor::wire::UsbSetup setup = {
        .bm_request_type = USB_DIR_IN | USB_TYPE_STANDARD | USB_RECIP_ENDPOINT,
        .b_request = USB_REQ_GET_STATUS,
        .w_value = 0,
        .w_index = 0,  // EP0
        .w_length = 2,
    };
    auto res = dci()->Control(setup, fidl::VectorView<uint8_t>());
    ASSERT_TRUE(res.ok()) << res.FormatDescription();
    ASSERT_TRUE(res->is_ok());
    ASSERT_EQ(res->value()->read.size(), 2u);
    EXPECT_EQ(res->value()->read[0], 0);
    EXPECT_EQ(res->value()->read[1], 0);
  }

  // 5. Test GET_STATUS for USB_RECIP_ENDPOINT (valid EP 0x81) initially not halted.
  {
    fdescriptor::wire::UsbSetup setup = {
        .bm_request_type = USB_DIR_IN | USB_TYPE_STANDARD | USB_RECIP_ENDPOINT,
        .b_request = USB_REQ_GET_STATUS,
        .w_value = 0,
        .w_index = 0x81,
        .w_length = 2,
    };
    auto res = dci()->Control(setup, fidl::VectorView<uint8_t>());
    ASSERT_TRUE(res.ok()) << res.FormatDescription();
    ASSERT_TRUE(res->is_ok());
    ASSERT_EQ(res->value()->read.size(), 2u);
    EXPECT_EQ(res->value()->read[0], 0);
    EXPECT_EQ(res->value()->read[1], 0);
  }

  // 6. Test SET_FEATURE(USB_ENDPOINT_HALT) on EP 0x81 and verify GET_STATUS reflects stall.
  {
    fdescriptor::wire::UsbSetup set_halt = {
        .bm_request_type = USB_DIR_OUT | USB_TYPE_STANDARD | USB_RECIP_ENDPOINT,
        .b_request = USB_REQ_SET_FEATURE,
        .w_value = USB_ENDPOINT_HALT,
        .w_index = 0x81,
        .w_length = 0,
    };
    auto res_set = dci()->Control(set_halt, fidl::VectorView<uint8_t>());
    ASSERT_TRUE(res_set.ok()) << res_set.FormatDescription();
    ASSERT_TRUE(res_set->is_ok());

    fdescriptor::wire::UsbSetup get_status = {
        .bm_request_type = USB_DIR_IN | USB_TYPE_STANDARD | USB_RECIP_ENDPOINT,
        .b_request = USB_REQ_GET_STATUS,
        .w_value = 0,
        .w_index = 0x81,
        .w_length = 2,
    };
    auto res_get = dci()->Control(get_status, fidl::VectorView<uint8_t>());
    ASSERT_TRUE(res_get.ok()) << res_get.FormatDescription();
    ASSERT_TRUE(res_get->is_ok());
    ASSERT_EQ(res_get->value()->read.size(), 2u);
    EXPECT_EQ(res_get->value()->read[0], 1);  // halted
    EXPECT_EQ(res_get->value()->read[1], 0);

    // Clear feature back to not halted.
    fdescriptor::wire::UsbSetup clear_halt = {
        .bm_request_type = USB_DIR_OUT | USB_TYPE_STANDARD | USB_RECIP_ENDPOINT,
        .b_request = USB_REQ_CLEAR_FEATURE,
        .w_value = USB_ENDPOINT_HALT,
        .w_index = 0x81,
        .w_length = 0,
    };
    auto res_clear = dci()->Control(clear_halt, fidl::VectorView<uint8_t>());
    ASSERT_TRUE(res_clear.ok()) << res_clear.FormatDescription();
    ASSERT_TRUE(res_clear->is_ok());

    auto res_get2 = dci()->Control(get_status, fidl::VectorView<uint8_t>());
    ASSERT_TRUE(res_get2.ok()) << res_get2.FormatDescription();
    ASSERT_TRUE(res_get2->is_ok());
    ASSERT_EQ(res_get2->value()->read.size(), 2u);
    EXPECT_EQ(res_get2->value()->read[0], 0);  // cleared
    EXPECT_EQ(res_get2->value()->read[1], 0);
  }
}

TEST_F(UnmanagedUsbPeripheralReadyTest, StartControllerFailsFromDci) {
  usb_peripheral_config::Config config;
  config.functions() = {"test"};
  StartDriverWithConfig(config);

  // Tell DCI mock to fail StartController.
  this->dut().RunInEnvironmentTypeContext(
      [](UsbPeripheralTestEnvironment& env) { env.dci().fail_start_.store(true); });

  auto res = TransitionToPeripheralReady();
  ASSERT_TRUE(res.is_error());
  EXPECT_STATUS(res.error_value(), ZX_ERR_IO_NOT_PRESENT);

  // Verify state stays at kWaitForFunctionBind since configuration failed to start the controller.
  ExpectState(UsbPeripheral::DeviceState::kWaitForFunctionBind);
  ExpectControllerStarted(false);
}

TEST_F(UnmanagedUsbPeripheralReadyTest, StopControllerFailsFromDci) {
  usb_peripheral_config::Config config;
  config.functions() = {"test"};
  StartDriverWithConfig(config);

  // Successfully configure and transition to peripheral ready first.
  auto res = TransitionToPeripheralReady();
  ASSERT_OK(res);

  // Tell DCI mock to fail StopController.
  this->dut().RunInEnvironmentTypeContext(
      [](UsbPeripheralTestEnvironment& env) { env.dci().fail_stop_.store(true); });

  // Call ClearFunctions. Even though StopController fails, it should still succeed
  // and teardown the functions to kNoConfiguration state.
  auto client = this->Client();

  zx::result endpoints = fidl::CreateEndpoints<fperipheral::Events>();
  ASSERT_OK(endpoints);

  FakeEvents fake_events;
  fake_events.Bind(std::move(endpoints->server));

  auto set_listener_res = client->SetStateChangeListener(std::move(endpoints->client));
  ASSERT_TRUE(set_listener_res.ok()) << set_listener_res.FormatDescription();

  auto clear_res = client->ClearFunctions();
  ASSERT_TRUE(clear_res.ok()) << clear_res.FormatDescription();

  fake_events.WaitUntilCleared(this->dut().runtime());
  fake_events.Unbind();

  // Verify we successfully cleaned up and reached kNoConfiguration.
  ExpectState(UsbPeripheral::DeviceState::kNoConfiguration);
  // DCI stop failed, so from fake DCI's point of view, it is still started.
  ExpectControllerStarted(true);

  // Reset failure flag and manually set controller_started to false to allow clean teardown in
  // TearDown()
  this->dut().RunInEnvironmentTypeContext([](UsbPeripheralTestEnvironment& env) {
    env.dci().fail_stop_.store(false);
    env.dci().set_controller_started(false);
  });
}

TEST_F(UsbPeripheralReadyTest, DISABLED_ActiveFunctionsCountTracking) {
  // Initially in kPeripheralReady with 1 function from SetUp().
  size_t active_count = dut().RunInDriverContext<size_t>([](UsbPeripheral& peripheral) {
    return UsbPeripheralTestHelper::active_functions_count(peripheral);
  });
  EXPECT_EQ(1u, active_count);

  // Close function channel.
  function_clients_.clients.clear();
  function_clients_.fakes.clear();
  dut().runtime().RunUntilIdle();

  active_count = dut().RunInDriverContext<size_t>([](UsbPeripheral& peripheral) {
    return UsbPeripheralTestHelper::active_functions_count(peripheral);
  });
  EXPECT_EQ(0u, active_count);
}

TEST_F(UnmanagedUsbPeripheralReadyTest,
       DISABLED_LateFunctionClearedCallbackIgnoredByGenerationFence) {
  usb_peripheral_config::Config config;
  config.functions() = {"test"};
  StartDriverWithConfig(config);

  // Generation 0 Setup (initial configuration)
  auto function_clients_res = TransitionToPeripheralReady();
  ASSERT_OK(function_clients_res);

  // Verify that active functions count is 1 for Generation 0.
  size_t active_count = dut().RunInDriverContext<size_t>([](UsbPeripheral& peripheral) {
    return UsbPeripheralTestHelper::active_functions_count(peripheral);
  });
  EXPECT_EQ(1u, active_count);

  auto peripheral_client = ConnectPeripheral();
  ASSERT_OK(peripheral_client);

  // Clear functions to trigger teardown.
  auto clear_res = peripheral_client.value()->ClearFunctions();
  ASSERT_TRUE(clear_res.ok()) << clear_res.FormatDescription();
  dut().runtime().RunUntilIdle();

  // Generation 1 Setup: Re-configure the device to increment config_generation to 1.
  fperipheral::wire::DeviceDescriptor device_desc = CreateTestDeviceDescriptor();
  fidl::Arena arena;
  auto configs = CreateTestFunctionDescriptors(arena);
  auto set_config_res = peripheral_client.value()->SetConfiguration(device_desc, configs);
  ASSERT_TRUE(set_config_res.ok());

  auto function_clients_gen1 = TransitionToPeripheralReady();
  ASSERT_OK(function_clients_gen1);

  // Verify that active functions count is 1 for Generation 1.
  active_count = dut().RunInDriverContext<size_t>([](UsbPeripheral& peripheral) {
    return UsbPeripheralTestHelper::active_functions_count(peripheral);
  });
  EXPECT_EQ(1u, active_count);

  // Direct injection of a stale FunctionCleared callback stamped with Generation 0.
  dut().RunInDriverContext([](UsbPeripheral& peripheral) {
    // Generation 0 is stale (the current generation is 1).
    UsbPeripheralTestHelper::FunctionCleared(peripheral, 0, 0);
  });

  dut().runtime().RunUntilIdle();

  // Assertion: The driver must securely ignore the stale callback.
  active_count = dut().RunInDriverContext<size_t>([](UsbPeripheral& peripheral) {
    return UsbPeripheralTestHelper::active_functions_count(peripheral);
  });
  EXPECT_EQ(1u, active_count);

  // Ensure we didn't accidentally tear down or regress state.
  ExpectState(UsbPeripheral::DeviceState::kPeripheralReady);
}

TEST_F(UnmanagedUsbPeripheralReadyTest, DISABLED_HostDisconnectPowerCutRaceTrap) {
  usb_peripheral_config::Config config;
  config.functions() = {"test"};
  StartDriverWithConfig(config);

  auto function_clients_res = TransitionToPeripheralReady();
  ASSERT_OK(function_clients_res);
  auto function_clients =
      std::make_shared<FunctionClients>(std::move(function_clients_res.value()));

  // The mock function driver in TransitionToPeripheralReady allocates OUT endpoint 1 and IN
  // endpoint 0x81.
  uint8_t ep_addr_out = 1;
  uint8_t ep_addr_in = 0x81;

  // Establish active connection.
  auto connected_res = this->dci()->SetConnected(true);
  ASSERT_TRUE(connected_res.ok());
  ExpectState(UsbPeripheral::DeviceState::kHostConnected);

  std::optional<FakeUsbFunction::SetConfiguredCompleterAsync> deferred_completer;
  auto completer_cleanup = fit::defer([&]() {
    if (deferred_completer.has_value()) {
      deferred_completer->ReplySuccess();
      deferred_completer.reset();
    }
  });
  auto callback_cleanup =
      fit::defer([&]() { function_clients->fakes[0]->set_on_set_configured_async(nullptr); });
  libsync::Completion unconfigure_invoked;

  // Intercept SetConfigured(false) on fake function driver and trigger endpoint disable.
  function_clients->fakes[0]->set_on_set_configured_async(
      [&](bool configured, FakeUsbFunction::SetConfiguredCompleterAsync completer) {
        if (!configured) {
          auto res_out = function_clients->clients[0]->DisableEndpoint(ep_addr_out);
          EXPECT_TRUE(res_out.ok());
          if (res_out.ok()) {
            EXPECT_TRUE(res_out->is_ok())
                << "DisableEndpoint OUT failed: " << zx_status_get_string(res_out->error_value());
          }
          auto res_in = function_clients->clients[0]->DisableEndpoint(ep_addr_in);
          EXPECT_TRUE(res_in.ok());
          if (res_in.ok()) {
            EXPECT_TRUE(res_in->is_ok())
                << "DisableEndpoint IN failed: " << zx_status_get_string(res_in->error_value());
          }
          deferred_completer.emplace(std::move(completer));
          unconfigure_invoked.Signal();
        } else {
          completer.ReplySuccess();
        }
      });

  // Trigger host disconnect.
  auto disconnected_res = this->dci()->SetConnected(false);
  ASSERT_TRUE(disconnected_res.ok());

  // Wait for the unconfiguration callback to be invoked and disable endpoints.
  ASSERT_OK(unconfigure_invoked.Wait(zx::sec(5)));

  // Allow any pending dispatcher tasks to settle.
  this->dut().runtime().RunUntilIdle();

  // Verify that endpoints were disabled on DCI.
  std::vector<uint8_t> disabled_endpoints;
  this->dut().RunInEnvironmentTypeContext([&](UsbPeripheralTestEnvironment& env) {
    disabled_endpoints = env.dci().disabled_endpoints();
  });
  EXPECT_EQ(disabled_endpoints.size(), 2u);

  // Assertion: The peripheral state MUST remain kHostConnected while unconfigure is in-flight,
  // preventing power cuts before endpoints are disabled.
  ExpectState(UsbPeripheral::DeviceState::kHostConnected);

  // Complete the unconfigure operation.
  ASSERT_TRUE(deferred_completer.has_value());
  completer_cleanup.call();

  // Let the async promise join chain finish.
  this->dut().runtime().RunUntilIdle();

  // Verify the driver successfully transitioned back to kPeripheralReady after async teardown.
  WaitUntilState(UsbPeripheral::DeviceState::kPeripheralReady);
}

TEST_F(UnmanagedUsbPeripheralReadyTest, DISABLED_AsynchronousUnconfigureTeardownCompleter) {
  usb_peripheral_config::Config config;
  config.functions() = {"test"};
  StartDriverWithConfig(config);

  auto function_clients_res = TransitionToPeripheralReady();
  ASSERT_OK(function_clients_res);
  auto fake_function = function_clients_res.value().fakes[0];

  // Simulate active VBUS cable plug to transition state to kHostConnected.
  ASSERT_OK(dci()->SetConnected(true).status());
  ExpectState(UsbPeripheral::DeviceState::kHostConnected);

  // Defer SetConfigured(false) completion to simulate slow network flushes.
  std::optional<FakeUsbFunction::SetConfiguredCompleterAsync> deferred_completer;
  auto completer_cleanup = fit::defer([&]() {
    if (deferred_completer.has_value()) {
      deferred_completer->ReplySuccess();
      deferred_completer.reset();
    }
  });
  auto callback_cleanup =
      fit::defer([&]() { fake_function->set_on_set_configured_async(nullptr); });
  fake_function->set_on_set_configured_async(
      [&](bool configured, FakeUsbFunction::SetConfiguredCompleterAsync completer) {
        if (!configured) {
          deferred_completer.emplace(std::move(completer));
        } else {
          completer.ReplySuccess();
        }
      });

  auto peripheral_client = ConnectPeripheral();
  ASSERT_OK(peripheral_client);

  std::atomic<bool> clear_finished = false;
  // Clear functions asynchronously on a background thread. This is necessary because
  // ClearFunctions() is a synchronous FIDL call that blocks waiting for SetConfigured(false)
  // to complete, which is held open by deferred_completer. If called on the main test thread,
  // the test would deadlock before it could assert the in-flight state or reply to the completer.
  auto clear_promise = std::async(std::launch::async, [&]() {
    auto clear_res = peripheral_client.value()->ClearFunctions();
    EXPECT_TRUE(clear_res.ok()) << clear_res.FormatDescription();
    clear_finished.store(true);
  });

  // Allow the dispatcher to process the initial ClearFunctions task.
  dut().runtime().RunUntilIdle();

  // Block until the background thread fully finishes the SetConfigured(false) callback.
  fake_function->WaitUntilCalled();

  // Assert that ClearFunctions remains blocked and the hardware controller is kept live
  // while the SetConfigured(false) completer is outstanding.
  EXPECT_FALSE(clear_finished.load());
  dut().RunInEnvironmentTypeContext(
      [](UsbPeripheralTestEnvironment& env) { EXPECT_TRUE(env.dci().controller_started()); });

  // Signal logical teardown completion.
  ASSERT_TRUE(deferred_completer.has_value());
  completer_cleanup.call();

  // Process the completion reply and execute hardware shutdown.
  dut().runtime().RunUntilIdle();
  dut().runtime().RunUntil([&]() {
    return dut().RunInEnvironmentTypeContext<bool>(
        [](UsbPeripheralTestEnvironment& env) { return !env.dci().controller_started(); });
  });

  // Assert that ClearFunctions completed and the hardware controller stopped.
  clear_promise.get();
  EXPECT_TRUE(clear_finished);
  dut().RunInEnvironmentTypeContext(
      [](UsbPeripheralTestEnvironment& env) { EXPECT_FALSE(env.dci().controller_started()); });
}

TEST_F(UnmanagedUsbPeripheralReadyTest, DISABLED_HostReconnectDuringAsyncUnconfigure) {
  usb_peripheral_config::Config config;
  config.functions() = {"test"};
  StartDriverWithConfig(config);

  auto function_clients_res = TransitionToPeripheralReady();
  ASSERT_OK(function_clients_res);
  auto fake_function = function_clients_res.value().fakes[0];

  // Simulate active VBUS cable plug to transition state to kHostConnected.
  ASSERT_OK(dci()->SetConnected(true).status());
  ExpectState(UsbPeripheral::DeviceState::kHostConnected);

  // Defer SetConfigured(false) completion to simulate delayed mock teardown.
  std::optional<FakeUsbFunction::SetConfiguredCompleterAsync> deferred_completer;
  auto completer_cleanup = fit::defer([&]() {
    if (deferred_completer.has_value()) {
      deferred_completer->ReplySuccess();
      deferred_completer.reset();
    }
  });
  auto callback_cleanup =
      fit::defer([&]() { fake_function->set_on_set_configured_async(nullptr); });
  fake_function->set_on_set_configured_async(
      [&](bool configured, FakeUsbFunction::SetConfiguredCompleterAsync completer) {
        if (!configured) {
          deferred_completer.emplace(std::move(completer));
        } else {
          completer.ReplySuccess();
        }
      });

  // Trigger a disconnect via DCI to begin the async teardown.
  ASSERT_OK(dci()->SetConnected(false).status());

  // Allow the dispatcher to process the SetConnected(false) task and wait for
  // SetConfigured(false) to arrive at the fake function.
  dut().runtime().RunUntilIdle();
  fake_function->WaitUntilCalled();

  // Assertion: The state should still be kHostConnected because the unconfigure
  // promise is currently blocked waiting for the completer.
  ExpectState(UsbPeripheral::DeviceState::kHostConnected);

  // Before resolving the completer, simulate a rapid cable bounce by reconnecting.
  ASSERT_OK(dci()->SetConnected(true).status());
  dut().runtime().RunUntilIdle();

  // Now resolve the captured completer to allow the async promise join task to finish.
  ASSERT_TRUE(deferred_completer.has_value());
  completer_cleanup.call();

  // Run the dispatcher until idle to flush the promise join chain.
  dut().runtime().RunUntilIdle();

  // Final Assertion: Verify that the state machine safely caught the reconnection.
  ExpectState(UsbPeripheral::DeviceState::kHostConnected);
}

}  // namespace
}  // namespace usb_peripheral::test
