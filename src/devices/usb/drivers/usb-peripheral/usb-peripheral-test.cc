// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/usb-peripheral/usb-peripheral.h"

#include <fidl/fuchsia.hardware.usb.dci/cpp/wire.h>
#include <fidl/fuchsia.hardware.usb.descriptor/cpp/wire.h>
#include <fidl/fuchsia.hardware.usb.function/cpp/wire_test_base.h>
#include <fuchsia/hardware/usb/dci/c/banjo.h>
#include <fuchsia/hardware/usb/dci/cpp/banjo.h>
#include <lib/async_patterns/testing/cpp/dispatcher_bound.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/driver/testing/cpp/driver_runtime.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/sync/cpp/completion.h>
#include <lib/zx/clock.h>
#include <lib/zx/interrupt.h>
#include <zircon/errors.h>
#include <zircon/syscalls.h>

#include <cstdio>
#include <cstring>
#include <memory>
#include <vector>

#include <gtest/gtest.h>
#include <usb/peripheral.h>
#include <usb/usb.h>

#include "src/devices/usb/drivers/usb-peripheral/usb-function.h"
#include "src/lib/testing/predicates/status.h"
#include "usb/descriptors.h"

namespace fdci = fuchsia_hardware_usb_dci;
namespace fdescriptor = fuchsia_hardware_usb_descriptor;

namespace usb_peripheral::test {
namespace {

class FakeDevice : public ddk::UsbDciProtocol<FakeDevice>, public fidl::WireServer<fdci::UsbDci> {
 public:
  FakeDevice() : proto_({&usb_dci_protocol_ops_, this}) {}

  fdci::UsbDciService::InstanceHandler GetHandler() {
    return fdci::UsbDciService::InstanceHandler(
        {.device = bindings_.CreateHandler(this, fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                                           fidl::kIgnoreBindingClosure)});
  }

  // USB DCI protocol implementation (No longer used).
  void UsbDciRequestQueue(usb_request_t* req, const usb_request_complete_callback_t* cb) {}
  zx_status_t UsbDciSetInterface(const usb_dci_interface_protocol_t* interface) {
    return ZX_ERR_NOT_SUPPORTED;
  }
  zx_status_t UsbDciConfigEp(const usb_endpoint_descriptor_t* ep_desc,
                             const usb_ss_ep_comp_descriptor_t* ss_comp_desc) {
    return ZX_ERR_NOT_SUPPORTED;
  }
  zx_status_t UsbDciDisableEp(uint8_t ep_address) { return ZX_ERR_NOT_SUPPORTED; }
  zx_status_t UsbDciEpSetStall(uint8_t ep_address) { return ZX_ERR_NOT_SUPPORTED; }
  zx_status_t UsbDciEpClearStall(uint8_t ep_address) { return ZX_ERR_NOT_SUPPORTED; }
  size_t UsbDciGetRequestSize() { return sizeof(usb_request_t); }

  zx_status_t UsbDciCancelAll(uint8_t ep_address) { return ZX_OK; }

  // fuchsia_hardware_usb_dci::UsbDci protocol.
  void ConnectToEndpoint(ConnectToEndpointRequestView req,
                         ConnectToEndpointCompleter::Sync& completer) override {
    completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
  }

  void SetInterface(SetInterfaceRequestView req, SetInterfaceCompleter::Sync& completer) override {
    fidl::Arena arena;
    client_.emplace(std::move(req->interface));
    completer.buffer(arena).ReplySuccess();
    set_interface_called_.Signal();
  }

  void StartController(StartControllerCompleter::Sync& completer) override {
    controller_started_ = true;
    completer.ReplySuccess();
  }

  void StopController(StopControllerCompleter::Sync& completer) override {
    controller_started_ = false;
    completer.ReplySuccess();
    if (stop_completion_) {
      stop_completion_->Signal();
    }
  }

  void ConfigureEndpoint(ConfigureEndpointRequestView req,
                         ConfigureEndpointCompleter::Sync& completer) override {
    completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
  }

  void DisableEndpoint(DisableEndpointRequestView req,
                       DisableEndpointCompleter::Sync& completer) override {
    completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
  }

  void EndpointSetStall(EndpointSetStallRequestView req,
                        EndpointSetStallCompleter::Sync& completer) override {
    completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
  }

  void EndpointClearStall(EndpointClearStallRequestView req,
                          EndpointClearStallCompleter::Sync& completer) override {
    completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
  }

  void CancelAll(CancelAllRequestView req, CancelAllCompleter::Sync& completer) override {
    completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_hardware_usb_dci::UsbDci> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {}

  usb_dci_protocol_t* proto() { return &proto_; }

  fidl::ClientEnd<fdci::UsbDciInterface> TakeClient() {
    set_interface_called_.Wait();
    auto client = std::move(client_);
    EXPECT_TRUE(client.has_value());
    return std::move(client.value());
  }

  compat::DeviceServer::BanjoConfig GetBanjoConfig() {
    compat::DeviceServer::BanjoConfig config{ZX_PROTOCOL_USB_DCI};
    config.callbacks[ZX_PROTOCOL_USB_DCI] = banjo_server_.callback();
    return config;
  }

  bool controller_started() const { return controller_started_; }

  void set_stop_completion(libsync::Completion* stop_completion) {
    stop_completion_ = stop_completion;
  }

 private:
  usb_dci_protocol_t proto_;
  bool controller_started_ = false;
  libsync::Completion set_interface_called_;
  libsync::Completion* stop_completion_ = nullptr;
  fidl::ServerBindingGroup<fdci::UsbDci> bindings_;
  std::optional<fidl::ClientEnd<fdci::UsbDciInterface>> client_;
  compat::BanjoServer banjo_server_{ZX_PROTOCOL_USB_DCI, this, &usb_dci_protocol_ops_};
};

class FakeUsbFunction
    : public fidl::testing::WireTestBase<fuchsia_hardware_usb_function::UsbFunctionInterface>,
      public std::enable_shared_from_this<FakeUsbFunction> {
 public:
  void Control(ControlRequestView req, ControlCompleter::Sync& completer) override {
    control_called_ = true;
    control_req_ = req->setup.b_request;
    fidl::Arena arena;
    std::vector<uint8_t> read_data = {1, 2, 3};
    completer.buffer(arena).ReplySuccess(fidl::VectorView<uint8_t>::FromExternal(read_data));
    call_completed_.Signal();
  }

  void SetConfigured(SetConfiguredRequestView req,
                     SetConfiguredCompleter::Sync& completer) override {
    set_configured_called_ = true;
    configured_ = req->configured;
    completer.ReplySuccess();
    call_completed_.Signal();
  }

  void SetInterface(SetInterfaceRequestView req, SetInterfaceCompleter::Sync& completer) override {
    set_interface_called_ = true;
    interface_ = req->interface;
    alt_setting_ = req->alt_setting;
    completer.ReplySuccess();
    call_completed_.Signal();
  }

  void NotImplemented_(const std::string& name, ::fidl::CompleterBase& completer) override {
    ADD_FAILURE() << "Not implemented: " << name;
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  void WaitUntilCalled() {
    call_completed_.Wait();
    call_completed_.Reset();
  }

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_hardware_usb_function::UsbFunctionInterface> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override {}

  void Bind(async_dispatcher_t* dispatcher,
            fidl::ServerEnd<fuchsia_hardware_usb_function::UsbFunctionInterface> server_end) {
    binding_.emplace(fidl::BindServer(dispatcher, std::move(server_end), shared_from_this()));
  }

  void Unbind() { binding_->Unbind(); }

  bool control_called() const { return control_called_; }
  uint8_t control_req() const { return control_req_; }
  bool set_configured_called() const { return set_configured_called_; }
  bool configured() const { return configured_; }
  bool set_interface_called() const { return set_interface_called_; }
  uint8_t interface() const { return interface_; }
  uint8_t alt_setting() const { return alt_setting_; }

 private:
  libsync::Completion call_completed_;

  bool control_called_ = false;
  uint8_t control_req_ = 0;

  bool set_configured_called_ = false;
  bool configured_ = false;

  bool set_interface_called_ = false;
  uint8_t interface_ = 0;
  uint8_t alt_setting_ = 0;

  std::optional<fidl::ServerBindingRef<fuchsia_hardware_usb_function::UsbFunctionInterface>>
      binding_;
};

class UsbPeripheralTestEnvironment : public fdf_testing::Environment {
 public:
  void Init(std::string_view serial_number) {
    fuchsia_boot_metadata::SerialNumberMetadata metadata{{.serial_number{serial_number}}};
    ASSERT_OK(serial_number_metadata_server_.SetMetadata(metadata));

    device_server_.Initialize("default", std::nullopt, dci_.GetBanjoConfig());
  }

  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    if (zx_status_t status =
            device_server_.Serve(fdf::Dispatcher::GetCurrent()->async_dispatcher(), &to_driver_vfs);
        status != ZX_OK) {
      return zx::error(status);
    }

    if (zx::result result = serial_number_metadata_server_.Serve(
            to_driver_vfs, fdf::Dispatcher::GetCurrent()->async_dispatcher());
        result.is_error()) {
      return result.take_error();
    }

    if (zx::result result = to_driver_vfs.AddService<fdci::UsbDciService>(dci_.GetHandler());
        result.is_error()) {
      return result.take_error();
    }

    return zx::ok();
  }

  fidl::ClientEnd<fdci::UsbDciInterface> TakeDciClient() { return dci_.TakeClient(); }

  FakeDevice& dci() { return dci_; }

 private:
  FakeDevice dci_;
  fdf_metadata::MetadataServer<fuchsia_boot_metadata::SerialNumberMetadata>
      serial_number_metadata_server_;
  compat::DeviceServer device_server_;
};

class UsbPeripheralTestConfig {
 public:
  using DriverType = UsbPeripheral;
  using EnvironmentType = UsbPeripheralTestEnvironment;
};

template <bool manage_lifetime>
class UsbPeripheralHarness : public ::testing::Test {
 public:
  void SetUp() override {
    driver_test_.RunInEnvironmentTypeContext([&](auto& env) { env.Init(kSerialNumber); });

    if constexpr (manage_lifetime) {
      StartDriverWithConfig(GetDriverConfig());
    }
  }

  virtual usb_peripheral_config::Config GetDriverConfig() {
    return usb_peripheral_config::Config{};
  }

  void StartDriverWithConfig(const usb_peripheral_config::Config& config) {
    ASSERT_OK(driver_test_.StartDriverWithCustomStartArgs(
        [&](auto& start_args) { start_args.config().emplace(config.ToVmo()); }));
    started_driver_ = true;
    dci_.Bind(driver_test_.RunInEnvironmentTypeContext<fidl::ClientEnd<fdci::UsbDciInterface>>(
        [](auto& env) { return env.TakeDciClient(); }));
  }

  void TearDown() override {
    if (started_driver_) {
      ASSERT_OK(driver_test_.StopDriver());
    }
  }

 protected:
  static constexpr std::string_view kSerialNumber = "Test serial number";

  fidl::WireSyncClient<fdci::UsbDciInterface>& dci() { return dci_; }
  fdf_testing::BackgroundDriverTest<UsbPeripheralTestConfig>& dut() { return driver_test_; }

 private:
  bool started_driver_;
  fidl::WireSyncClient<fdci::UsbDciInterface> dci_;
  fdf_testing::BackgroundDriverTest<UsbPeripheralTestConfig> driver_test_;
};

using UnmanagedUsbPeripheralTest = UsbPeripheralHarness<false>;
using ManagedUsbPeripheralTest = UsbPeripheralHarness<true>;

class UsbPeripheralFunctionTest : public ManagedUsbPeripheralTest {
 public:
  usb_peripheral_config::Config GetDriverConfig() override {
    usb_peripheral_config::Config config;
    config.functions() = {"test"};
    return config;
  }

  zx::result<fidl::WireSyncClient<fuchsia_hardware_usb_function::UsbFunction>> ConnectFunction() {
    zx::result result =
        dut().Connect<fuchsia_hardware_usb_function::UsbFunctionService::Device>("function-000");
    if (result.is_error()) {
      return result.take_error();
    }
    return zx::ok(fidl::WireSyncClient<fuchsia_hardware_usb_function::UsbFunction>(
        std::move(result.value())));
  }

  zx::result<std::tuple<std::shared_ptr<FakeUsbFunction>,
                        fidl::ClientEnd<fuchsia_hardware_usb_function::UsbFunctionInterface>>>
  BindFakeFunction() {
    zx::result endpoints =
        fidl::CreateEndpoints<fuchsia_hardware_usb_function::UsbFunctionInterface>();
    if (endpoints.is_error()) {
      return endpoints.take_error();
    }
    auto fake_function = std::make_shared<FakeUsbFunction>();
    fake_function->Bind(dut().runtime().StartBackgroundDispatcher()->async_dispatcher(),
                        std::move(endpoints->server));
    return zx::ok(std::make_tuple(fake_function, std::move(endpoints->client)));
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
  for (size_t i = 0; i < sizeof(kSerialNumber) - 1; i++) {
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

TEST_F(UsbPeripheralFunctionTest, ConfigureAndRouteFidlCalls) {
  zx::result function_client_result = ConnectFunction();
  ASSERT_OK(function_client_result);
  fidl::WireSyncClient<fuchsia_hardware_usb_function::UsbFunction> function_client =
      std::move(function_client_result.value());

  zx::result fake_function_result = BindFakeFunction();
  ASSERT_OK(fake_function_result);
  auto [fake_function, fake_function_endpoint] = std::move(fake_function_result.value());

  zx_status_t alloc_status;
  uint8_t interface_num = 0;
  // TODO(https://fxbug.dev/439593030): Replace with FIDL call once that's available.
  dut().RunInDriverContext(
      [&](UsbPeripheral& driver) { alloc_status = driver.AllocInterface(0, &interface_num); });
  ASSERT_OK(alloc_status);

  // Valid descriptors for ValidateFunction (we pass UMS's descriptors)
  usb_interface_descriptor_t intf_desc = {
      .b_length = sizeof(usb_interface_descriptor_t),
      .b_descriptor_type = USB_DT_INTERFACE,
      .b_interface_number = interface_num,
      .b_alternate_setting = 0,
      .b_num_endpoints = 0,
      .b_interface_class = 8,
      .b_interface_sub_class = 6,
      .b_interface_protocol = 80,
      .i_interface = 0,
  };

  std::vector<uint8_t> descriptors(sizeof(intf_desc));
  memcpy(descriptors.data(), &intf_desc, sizeof(intf_desc));

  fidl::WireResult configure_res = function_client->Configure(
      fidl::VectorView<uint8_t>::FromExternal(descriptors.data(), descriptors.size()),
      std::move(fake_function_endpoint));

  ASSERT_TRUE(configure_res.ok()) << configure_res.status_string();
  ASSERT_TRUE(configure_res->is_ok());

  // Controller starts when all functions are registered.
  dut().RunInEnvironmentTypeContext(
      [](UsbPeripheralTestEnvironment& env) { EXPECT_TRUE(env.dci().controller_started()); });
  ASSERT_OK(dci()->SetConnected(true).status());

  fidl::Arena arena;
  std::vector<uint8_t> unused;

  // Test SetConfigured via standard endpoint request
  fdescriptor::wire::UsbSetup setup;
  setup.bm_request_type = USB_DIR_OUT | USB_RECIP_DEVICE | USB_TYPE_STANDARD;
  setup.b_request = USB_REQ_SET_CONFIGURATION;
  setup.w_value = 1;  // Configuration 1
  setup.w_index = interface_num;
  setup.w_length = 0;

  fidl::WireUnownedResult config_res =
      dci().buffer(arena)->Control(setup, fidl::VectorView<uint8_t>::FromExternal(unused));
  EXPECT_TRUE(config_res.ok()) << config_res.FormatDescription();
  ASSERT_OK(config_res.value());

  fake_function->WaitUntilCalled();
  EXPECT_TRUE(fake_function->set_configured_called());
  EXPECT_TRUE(fake_function->configured());

  // Test Control via provided endpoint request.
  setup.bm_request_type = USB_DIR_IN;
  setup.b_request = 0xAA;
  setup.w_value = 0x01;
  setup.w_index = 0x02;
  setup.w_length = 3;
  fidl::WireUnownedResult control_res =
      dci().buffer(arena)->Control(setup, fidl::VectorView<uint8_t>::FromExternal(unused));

  EXPECT_TRUE(control_res.ok()) << control_res.FormatDescription();
  ASSERT_OK(control_res.value());

  fake_function->WaitUntilCalled();
  EXPECT_TRUE(fake_function->control_called());
  EXPECT_EQ(0xAA, fake_function->control_req());

  // Test SetInterface via standard endpoint request
  setup.bm_request_type = USB_DIR_OUT | USB_RECIP_INTERFACE | USB_TYPE_STANDARD;
  setup.b_request = USB_REQ_SET_INTERFACE;
  setup.w_value = 1;  // Alt setting 1
  setup.w_index = interface_num;
  setup.w_length = 0;

  fidl::WireUnownedResult intf_res =
      dci().buffer(arena)->Control(setup, fidl::VectorView<uint8_t>::FromExternal(unused));
  ASSERT_TRUE(intf_res->is_ok()) << intf_res.FormatDescription();
  ASSERT_OK(intf_res.value());
  fake_function->WaitUntilCalled();

  EXPECT_TRUE(fake_function->set_interface_called());
  EXPECT_EQ(interface_num, fake_function->interface());
  EXPECT_EQ(1, fake_function->alt_setting());
}

TEST_F(UsbPeripheralFunctionTest, ConfigureFailsIfInterfaceNotAllocated) {
  dut().RunInEnvironmentTypeContext(
      [](UsbPeripheralTestEnvironment& env) { EXPECT_FALSE(env.dci().controller_started()); });

  zx::result function_client_result = ConnectFunction();
  ASSERT_OK(function_client_result);
  fidl::WireSyncClient<fuchsia_hardware_usb_function::UsbFunction> function_client =
      std::move(function_client_result.value());

  zx::result fake_function_result = BindFakeFunction();
  ASSERT_OK(fake_function_result);
  auto [fake_function, fake_function_endpoint] = std::move(fake_function_result.value());

  // We use an interface number that hasn't been allocated (0, by default in descriptors).
  usb_interface_descriptor_t intf_desc = {
      .b_length = sizeof(usb_interface_descriptor_t),
      .b_descriptor_type = USB_DT_INTERFACE,
      .b_interface_number = 0,
      .b_alternate_setting = 0,
      .b_num_endpoints = 0,
      .b_interface_class = 8,
      .b_interface_sub_class = 6,
      .b_interface_protocol = 80,
      .i_interface = 0,
  };

  std::vector<uint8_t> descriptors(sizeof(intf_desc));
  memcpy(descriptors.data(), &intf_desc, sizeof(intf_desc));

  fidl::WireResult configure_res = function_client->Configure(
      fidl::VectorView<uint8_t>::FromExternal(descriptors.data(), descriptors.size()),
      std::move(fake_function_endpoint));

  ASSERT_TRUE(configure_res.ok()) << configure_res.FormatDescription();
  EXPECT_STATUS(configure_res.value(), ZX_ERR_INVALID_ARGS);
}

TEST_F(UsbPeripheralFunctionTest, ConfigureFailsIfAlreadyBound) {
  zx::result function_client_result = ConnectFunction();
  ASSERT_OK(function_client_result);
  fidl::WireSyncClient<fuchsia_hardware_usb_function::UsbFunction> function_client =
      std::move(function_client_result.value());

  zx::result fake_function_result = BindFakeFunction();
  ASSERT_OK(fake_function_result);
  auto [fake_function, fake_function_endpoint] = std::move(fake_function_result.value());

  zx_status_t alloc_status;
  uint8_t interface_num = 0;
  dut().RunInDriverContext(
      [&](UsbPeripheral& driver) { alloc_status = driver.AllocInterface(0, &interface_num); });
  ASSERT_OK(alloc_status);

  usb_interface_descriptor_t intf_desc = {
      .b_length = sizeof(usb_interface_descriptor_t),
      .b_descriptor_type = USB_DT_INTERFACE,
      .b_interface_number = interface_num,
      .b_alternate_setting = 0,
      .b_num_endpoints = 0,
      .b_interface_class = 8,
      .b_interface_sub_class = 6,
      .b_interface_protocol = 80,
      .i_interface = 0,
  };

  std::vector<uint8_t> descriptors(sizeof(intf_desc));
  memcpy(descriptors.data(), &intf_desc, sizeof(intf_desc));

  // First call should succeed
  fidl::WireResult configure_res = function_client->Configure(
      fidl::VectorView<uint8_t>::FromExternal(descriptors.data(), descriptors.size()),
      std::move(fake_function_endpoint));
  ASSERT_TRUE(configure_res.ok()) << configure_res.FormatDescription();
  ASSERT_OK(configure_res.value());

  // Second call with a new endpoint should fail with ZX_ERR_ALREADY_BOUND
  zx::result second_fake_result = BindFakeFunction();
  ASSERT_OK(second_fake_result);
  auto [second_fake, second_fake_endpoint] = std::move(second_fake_result.value());

  fidl::WireResult second_configure_res = function_client->Configure(
      fidl::VectorView<uint8_t>::FromExternal(descriptors.data(), descriptors.size()),
      std::move(second_fake_endpoint));

  ASSERT_TRUE(second_configure_res.ok()) << second_configure_res.FormatDescription();
  EXPECT_STATUS(second_configure_res.value(), ZX_ERR_ALREADY_BOUND);
}

TEST_F(UsbPeripheralFunctionTest, ControllerStoppedOnFunctionClose) {
  zx::result function_client_result = ConnectFunction();
  ASSERT_OK(function_client_result);
  fidl::WireSyncClient<fuchsia_hardware_usb_function::UsbFunction> function_client =
      std::move(function_client_result.value());

  zx::result fake_function_result = BindFakeFunction();
  ASSERT_OK(fake_function_result);
  auto [fake_function, fake_function_endpoint] = std::move(fake_function_result.value());

  zx_status_t alloc_status;
  uint8_t interface_num = 0;
  dut().RunInDriverContext(
      [&](UsbPeripheral& driver) { alloc_status = driver.AllocInterface(0, &interface_num); });
  ASSERT_OK(alloc_status);

  usb_interface_descriptor_t intf_desc = {
      .b_length = sizeof(usb_interface_descriptor_t),
      .b_descriptor_type = USB_DT_INTERFACE,
      .b_interface_number = interface_num,
      .b_alternate_setting = 0,
      .b_num_endpoints = 0,
      .b_interface_class = 8,
      .b_interface_sub_class = 6,
      .b_interface_protocol = 80,
      .i_interface = 0,
  };

  std::vector<uint8_t> descriptors(sizeof(intf_desc));
  memcpy(descriptors.data(), &intf_desc, sizeof(intf_desc));

  fidl::WireResult configure_res = function_client->Configure(
      fidl::VectorView<uint8_t>::FromExternal(descriptors.data(), descriptors.size()),
      std::move(fake_function_endpoint));
  ASSERT_TRUE(configure_res.ok()) << configure_res.FormatDescription();
  ASSERT_OK(configure_res.value());

  // Controller starts when all functions are registered.
  dut().RunInEnvironmentTypeContext(
      [](UsbPeripheralTestEnvironment& env) { EXPECT_TRUE(env.dci().controller_started()); });

  // Set the stop completion before unbinding.
  libsync::Completion stop_completion;
  dut().RunInEnvironmentTypeContext(
      [&](UsbPeripheralTestEnvironment& env) { env.dci().set_stop_completion(&stop_completion); });

  // Close the fake function endpoint.
  fake_function->Unbind();

  // Wait for the controller to stop.
  stop_completion.Wait();

  dut().RunInEnvironmentTypeContext([](UsbPeripheralTestEnvironment& env) {
    env.dci().set_stop_completion(nullptr);
    EXPECT_FALSE(env.dci().controller_started());
  });
}

}  // namespace
}  // namespace usb_peripheral::test
