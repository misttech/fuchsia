// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/usb-peripheral/usb-peripheral-test-harness.h"

namespace usb_peripheral::test {
namespace {

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
  comp.Wait();
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
    ASSERT_EQ(peripheral.SnapshotState(), UsbPeripheral::DeviceState::kPeripheralReady);
  });

  // Call CheckAndStartController again.
  // This is to check for a very rare race condition where the function register calls occur in
  // parallel and the peripheral has already started the controller.
  // It's hard to simulate this race condition, so we just call CheckAndStartController again
  // to be safe.
  // This race condition would go away once we move to a single dispatcher.
  this->dut().RunInDriverContext([&](UsbPeripheral& peripheral) {
    // This should do nothing and return ZX_OK because state is not kWaitForFunctionBind.
    ASSERT_OK(peripheral.CheckAndStartController());
  });

  // Verify state is still kPeripheralReady.
  this->dut().RunInDriverContext([&](UsbPeripheral& peripheral) {
    ASSERT_EQ(peripheral.SnapshotState(), UsbPeripheral::DeviceState::kPeripheralReady);
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
      [](UsbPeripheralTestEnvironment& env) { env.dci().fail_start_ = true; });

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
      [](UsbPeripheralTestEnvironment& env) { env.dci().fail_stop_ = true; });

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
    env.dci().fail_stop_ = false;
    env.dci().set_controller_started(false);
  });
}

}  // namespace
}  // namespace usb_peripheral::test
