// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/usb-peripheral/usb-peripheral.h"

#include <fidl/fuchsia.hardware.usb.dci/cpp/wire.h>
#include <fidl/fuchsia.hardware.usb.descriptor/cpp/wire.h>
#include <fidl/fuchsia.hardware.usb.function/cpp/wire_test_base.h>
#include <lib/async_patterns/testing/cpp/dispatcher_bound.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/driver/testing/cpp/driver_runtime.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/inspect/cpp/reader.h>
#include <lib/sync/cpp/completion.h>
#include <lib/zx/clock.h>
#include <lib/zx/interrupt.h>
#include <zircon/errors.h>
#include <zircon/syscalls.h>

#include <cstdio>
#include <cstring>
#include <map>
#include <memory>
#include <optional>
#include <vector>

#include <gtest/gtest.h>
#include <sdk/lib/inspect/testing/cpp/inspect.h>
#include <usb-inspect/usb-inspect-test-helper.h>
#include <usb/peripheral.h>
#include <usb/request-cpp.h>
#include <usb/usb.h>

#include "src/devices/usb/drivers/usb-peripheral/usb-function.h"
#include "src/lib/testing/predicates/status.h"
#include "usb/descriptors.h"

namespace fdci = fuchsia_hardware_usb_dci;
namespace fdescriptor = fuchsia_hardware_usb_descriptor;
namespace fendpoint = fuchsia_hardware_usb_endpoint;
namespace ffunction = fuchsia_hardware_usb_function;
namespace fperipheral = fuchsia_hardware_usb_peripheral;

namespace usb_peripheral {

inline std::ostream& operator<<(std::ostream& os, const UsbPeripheral::DeviceState& state) {
  return os << std::format("{}", state);
}

}  // namespace usb_peripheral

namespace usb_peripheral::test {
namespace {

using inspect::testing::BoolIs;
using inspect::testing::NameMatches;
using inspect::testing::NodeMatches;
using inspect::testing::PropertyList;
using inspect::testing::StringIs;
using inspect::testing::UintIs;
using ::testing::AllOf;
using ::testing::Contains;

class FakeDevice : public fidl::WireServer<fdci::UsbDci> {
 public:
  FakeDevice() = default;

  fdci::UsbDciService::InstanceHandler GetHandler() {
    return fdci::UsbDciService::InstanceHandler(
        {.device = bindings_.CreateHandler(this, fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                                           fidl::kIgnoreBindingClosure)});
  }

  // fdci::UsbDci protocol.
  void ConnectToEndpoint(ConnectToEndpointRequestView req,
                         ConnectToEndpointCompleter::Sync& completer) override {
    endpoints_[req->ep_addr] = std::move(req->ep);
    completer.ReplySuccess();
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
    if (fail_configure_) {
      completer.ReplyError(ZX_ERR_IO_NOT_PRESENT);
      return;
    }
    configured_endpoints_.push_back(req->ep_descriptor);
    configured_endpoints_ss_companion_.push_back(req->ss_comp_descriptor);
    completer.ReplySuccess();
  }

  void DisableEndpoint(DisableEndpointRequestView req,
                       DisableEndpointCompleter::Sync& completer) override {
    if (fail_disable_) {
      completer.ReplyError(ZX_ERR_IO_NOT_PRESENT);
      return;
    }
    disabled_endpoints_.push_back(req->ep_address);
    completer.ReplySuccess();
  }

  void EndpointSetStall(EndpointSetStallRequestView req,
                        EndpointSetStallCompleter::Sync& completer) override {
    if (fail_stall_) {
      completer.ReplyError(ZX_ERR_IO_NOT_PRESENT);
    } else {
      set_stalls_.push_back(req->ep_address);
      completer.ReplySuccess();
    }
  }

  void EndpointClearStall(EndpointClearStallRequestView req,
                          EndpointClearStallCompleter::Sync& completer) override {
    if (fail_stall_) {
      completer.ReplyError(ZX_ERR_IO_NOT_PRESENT);
    } else {
      clear_stalls_.push_back(req->ep_address);
      completer.ReplySuccess();
    }
  }

  void CancelAll(CancelAllRequestView req, CancelAllCompleter::Sync& completer) override {
    completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
  }

  void GetHardwareInfo(GetHardwareInfoCompleter::Sync& completer) override {
    completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
  }

  void AllocEndpoint(AllocEndpointRequestView req,
                     AllocEndpointCompleter::Sync& completer) override {
    completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
  }

  void FreeEndpoint(FreeEndpointRequestView req, FreeEndpointCompleter::Sync& completer) override {
    completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<fdci::UsbDci> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {}

  fidl::ClientEnd<fdci::UsbDciInterface> TakeClient() {
    auto client = std::move(client_);
    EXPECT_TRUE(client.has_value());
    return std::move(client.value());
  }

  libsync::Completion& set_interface_called() { return set_interface_called_; }

  bool controller_started() const { return controller_started_; }

  void set_stop_completion(libsync::Completion* stop_completion) {
    stop_completion_ = stop_completion;
  }

  fidl::ServerEnd<fendpoint::Endpoint> TakeEndpoint(uint8_t addr) {
    auto it = endpoints_.find(addr);
    if (it == endpoints_.end()) {
      return {};
    }
    auto ep = std::move(it->second);
    endpoints_.erase(it);
    return ep;
  }

  bool fail_stall_ = false;
  std::vector<uint8_t> set_stalls_;
  std::vector<uint8_t> clear_stalls_;

  bool fail_configure_ = false;
  std::vector<fdescriptor::wire::UsbEndpointDescriptor> configured_endpoints_;
  std::vector<fdescriptor::wire::UsbSsEpCompDescriptor> configured_endpoints_ss_companion_;

  bool fail_disable_ = false;
  std::vector<uint8_t> disabled_endpoints_;

 private:
  bool controller_started_ = false;
  libsync::Completion set_interface_called_;
  libsync::Completion* stop_completion_ = nullptr;
  fidl::ServerBindingGroup<fdci::UsbDci> bindings_;
  std::optional<fidl::ClientEnd<fdci::UsbDciInterface>> client_;
  std::map<uint8_t, fidl::ServerEnd<fendpoint::Endpoint>> endpoints_;
};

class FakeUsbFunction : public fidl::testing::WireTestBase<ffunction::UsbFunctionInterface>,
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
    configured_history_.push_back(req->configured);
    if (on_set_configured_) {
      on_set_configured_();
    }
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

  void WaitUntilUnbound() {
    unbound_completion_.Wait();
    unbound_completion_.Reset();
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<ffunction::UsbFunctionInterface> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {}

  void Bind(fdf::UnownedSynchronizedDispatcher dispatcher,
            fidl::ServerEnd<ffunction::UsbFunctionInterface> server_end) {
    dispatcher_ = std::move(dispatcher);
    binding_.emplace(fidl::BindServer(
        dispatcher_.value()->async_dispatcher(), std::move(server_end), shared_from_this(),
        [](FakeUsbFunction* impl, fidl::UnbindInfo info,
           fidl::ServerEnd<ffunction::UsbFunctionInterface> server_end) {
          impl->unbound_completion_.Signal();
        }));
  }

  void Unbind() { binding_->Unbind(); }

  bool control_called() const { return control_called_; }
  uint8_t control_req() const { return control_req_; }
  bool set_configured_called() const { return set_configured_called_; }
  void clear_set_configured_called() { set_configured_called_ = false; }
  bool configured() const { return configured_; }
  const std::vector<bool>& configured_history() const { return configured_history_; }
  bool set_interface_called() const { return set_interface_called_; }

  void set_on_set_configured(fit::function<void()> cb) { on_set_configured_ = std::move(cb); }
  uint8_t interface() const { return interface_; }
  uint8_t alt_setting() const { return alt_setting_; }

  fdf::UnownedSynchronizedDispatcher& dispatcher() {
    ZX_ASSERT(dispatcher_.has_value());
    return dispatcher_.value();
  }

 private:
  libsync::Completion call_completed_;
  libsync::Completion unbound_completion_;

  bool control_called_ = false;
  uint8_t control_req_ = 0;

  bool set_configured_called_ = false;
  bool configured_ = false;
  std::vector<bool> configured_history_;
  fit::function<void()> on_set_configured_;

  bool set_interface_called_ = false;
  uint8_t interface_ = 0;
  uint8_t alt_setting_ = 0;

  std::optional<fdf::UnownedSynchronizedDispatcher> dispatcher_;
  std::optional<fidl::ServerBindingRef<ffunction::UsbFunctionInterface>> binding_;
};

class FakeEvents : public fidl::WireServer<fperipheral::Events> {
 public:
  FakeEvents() = default;
  ~FakeEvents() { Unbind(); }

  void FunctionRegistered(FunctionRegisteredCompleter::Sync& completer) override {
    completer.Reply();
  }
  void FunctionsCleared(FunctionsClearedCompleter::Sync& completer) override {
    cleared_called_ = true;
  }

  void WaitUntilCleared(fdf_testing::DriverRuntime& runtime) {
    runtime.RunUntil([&]() { return cleared_called_; });
    cleared_called_ = false;
  }

  void Bind(fidl::ServerEnd<fperipheral::Events> server_end) {
    binding_.emplace(fidl::BindServer(fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                                      std::move(server_end), this));
  }

  void Unbind() {
    if (binding_) {
      binding_->Unbind();
      binding_.reset();
    }
  }

 private:
  bool cleared_called_ = false;
  std::optional<fidl::ServerBindingRef<fperipheral::Events>> binding_;
};

class UsbPeripheralTestEnvironment : public fdf_testing::Environment {
 public:
  void Init(std::string_view serial_number) {
    serial_number_ = fuchsia_boot_metadata::SerialNumberMetadata{{.serial_number{serial_number}}};
  }

  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    if (serial_number_.has_value()) {
      if (zx::result result = serial_number_metadata_server_.Serve(
              to_driver_vfs, fdf::Dispatcher::GetCurrent()->async_dispatcher(),
              serial_number_.value());
          result.is_error()) {
        return result.take_error();
      }
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
  std::optional<fuchsia_boot_metadata::SerialNumberMetadata> serial_number_;
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
    if (config.functions().empty()) {
      ExpectState(UsbPeripheral::DeviceState::kNoConfiguration);
    } else {
      ExpectState(UsbPeripheral::DeviceState::kWaitForFunctionBind);
    }
    started_driver_ = true;
    driver_test_.runtime().RunUntil([&]() {
      return driver_test_.RunInEnvironmentTypeContext<bool>([](UsbPeripheralTestEnvironment& env) {
        return env.dci().set_interface_called().signaled();
      });
    });

    dci_.Bind(driver_test_.RunInEnvironmentTypeContext<fidl::ClientEnd<fdci::UsbDciInterface>>(
        [](auto& env) { return env.TakeDciClient(); }));
  }

  void WaitForChildNode(const std::string& name) {
    bool found = false;
    dut().runtime().RunUntil([&]() {
      dut().RunInNodeContext([&](fdf_testing::TestNode& root) {
        auto it = root.children().find(std::string(UsbPeripheral::kChildNodeName));
        if (it != root.children().end()) {
          auto& peripheral_node = it->second;
          auto func_it = peripheral_node.children().find(name);
          if (func_it != peripheral_node.children().end()) {
            found = true;
          }
        }
      });
      return found;
    });
  }

  void SimulateFunctionUnbind(const std::vector<std::string>& function_names) {
    for (const auto& name : function_names) {
      WaitForChildNode(name);
    }

    this->dut().RunInNodeContext([&](fdf_testing::TestNode& root) {
      auto it = root.children().find(std::string(UsbPeripheral::kChildNodeName));
      ASSERT_NE(it, root.children().end());
      auto& peripheral_node = it->second;

      for (const auto& name : function_names) {
        auto it_func = peripheral_node.children().find(name);
        ASSERT_NE(it_func, peripheral_node.children().end());
        // Dropping the returned Node channel triggers an unbind.
        (void)it_func->second.CreateNodeChannel();
      }
    });
  }

  void ExpectChildNodeCount(size_t expected_count) {
    this->dut().RunInNodeContext([&](fdf_testing::TestNode& root) {
      auto it = root.children().find(std::string(UsbPeripheral::kChildNodeName));
      ASSERT_NE(it, root.children().end());
      auto& peripheral_node = it->second;
      EXPECT_EQ(peripheral_node.children().size(), expected_count);
    });
  }

  void WaitUntilChildNodeCount(size_t expected_count) {
    dut().runtime().RunUntilIdle();
    dut().runtime().RunUntil([&]() {
      std::optional<size_t> actual_count;
      dut().RunInNodeContext([&](fdf_testing::TestNode& root) {
        auto it = root.children().find(std::string(UsbPeripheral::kChildNodeName));
        if (it != root.children().end()) {
          actual_count = it->second.children().size();
        }
      });
      return actual_count == expected_count;
    });
  }

  void ExpectControllerStarted(bool expected) {
    this->dut().RunInEnvironmentTypeContext([expected](UsbPeripheralTestEnvironment& env) {
      if (expected) {
        EXPECT_TRUE(env.dci().controller_started());
      } else {
        EXPECT_FALSE(env.dci().controller_started());
      }
    });
  }

  void TearDown() override {
    if (started_driver_) {
      // StopDriver will call PrepareStop, which should stop the controller.
      ASSERT_OK(driver_test_.StopDriver());

      ExpectControllerStarted(false);
    }
  }

  void RegisterFakeEvents(std::shared_ptr<FakeEvents> fake_events) {
    auto [client_end, server_end] = fidl::Endpoints<fperipheral::Events>::Create();
    fake_events->Bind(std::move(server_end));
    auto client = Client();
    ASSERT_TRUE(client->SetStateChangeListener(std::move(client_end)).ok());
  }

 protected:
  static constexpr std::string_view kSerialNumber = "Test serial number";

  fidl::WireSyncClient<fdci::UsbDciInterface>& dci() { return dci_; }

  void ExpectState(UsbPeripheral::DeviceState state) {
    this->dut().RunInDriverContext([state](UsbPeripheral& peripheral) {
      EXPECT_EQ(peripheral.SnapshotState(), state);

      auto hierarchy = usb_inspect::ReadHierarchyFromInspector(peripheral.inspector());
      auto* node = hierarchy.GetByPath({"usb-peripheral", "dci_metrics"});
      ASSERT_NE(node, nullptr);
      EXPECT_THAT(*node,
                  NodeMatches(PropertyList(Contains(StringIs("state", std::format("{}", state))))));
    });
  }

  void WaitUntilState(UsbPeripheral::DeviceState state) {
    dut().runtime().RunUntil([&]() {
      bool matched = false;
      dut().RunInDriverContext(
          [&](UsbPeripheral& peripheral) { matched = (peripheral.SnapshotState() == state); });
      return matched;
    });
  }

  fdf_testing::BackgroundDriverTest<UsbPeripheralTestConfig>& dut() { return driver_test_; }

  fidl::WireSyncClient<fperipheral::Device> Client() {
    auto client_end = driver_test_.Connect<fperipheral::Service::Device>();
    ZX_ASSERT_MSG(client_end.is_ok(), "Failed to connect to peripheral service: %s",
                  client_end.status_string());
    return fidl::WireSyncClient<fperipheral::Device>{std::move(client_end.value())};
  }

  zx::result<fidl::WireSyncClient<ffunction::UsbFunction>> ConnectFunction(
      std::string name = "function-000") {
    ExpectState(UsbPeripheral::DeviceState::kWaitForFunctionBind);
    zx::result result = dut().template Connect<ffunction::UsbFunctionService::Device>(name);
    if (result.is_error()) {
      return result.take_error();
    }
    ExpectState(UsbPeripheral::DeviceState::kWaitForFunctionBind);
    return zx::ok(fidl::WireSyncClient<ffunction::UsbFunction>(std::move(result.value())));
  }

  zx::result<fidl::WireSyncClient<fperipheral::Device>> ConnectPeripheral() {
    zx::result result = dut().template Connect<fperipheral::Service::Device>();
    if (result.is_error()) {
      return result.take_error();
    }
    return zx::ok(fidl::WireSyncClient<fperipheral::Device>(std::move(result.value())));
  }

  zx::result<std::tuple<std::shared_ptr<FakeUsbFunction>,
                        fidl::ClientEnd<ffunction::UsbFunctionInterface>>>
  BindFakeFunction() {
    ExpectState(UsbPeripheral::DeviceState::kWaitForFunctionBind);
    zx::result endpoints = fidl::CreateEndpoints<ffunction::UsbFunctionInterface>();
    if (endpoints.is_error()) {
      return endpoints.take_error();
    }
    auto fake_function = std::make_shared<FakeUsbFunction>();
    fake_function->Bind(dut().runtime().StartBackgroundDispatcher(), std::move(endpoints->server));
    ExpectState(UsbPeripheral::DeviceState::kWaitForFunctionBind);
    return zx::ok(std::make_tuple(fake_function, std::move(endpoints->client)));
  }

 private:
  fidl::WireSyncClient<fdci::UsbDciInterface> dci_;
  fdf_testing::BackgroundDriverTest<UsbPeripheralTestConfig> driver_test_;
  bool started_driver_ = false;
};

class ManagedUsbPeripheralTest : public UsbPeripheralHarness<true> {
 public:
  usb_peripheral_config::Config GetDriverConfig() override {
    return usb_peripheral_config::Config{};
  }
};
using UnmanagedUsbPeripheralTest = UsbPeripheralHarness<false>;

template <bool manage_lifetime>
class PeripheralReadyTestBase : public UsbPeripheralHarness<manage_lifetime> {
 public:
  struct FunctionClients {
    std::vector<fidl::WireSyncClient<ffunction::UsbFunction>> clients;
    std::vector<std::shared_ptr<FakeUsbFunction>> fakes;
  };

  static fperipheral::wire::DeviceDescriptor CreateTestDeviceDescriptor() {
    fperipheral::wire::DeviceDescriptor device_desc = {};
    device_desc.bcd_usb = 0x0200;
    device_desc.b_device_class = 0;
    device_desc.b_device_sub_class = 0;
    device_desc.b_device_protocol = 0;
    device_desc.b_max_packet_size0 = 64;
    device_desc.id_vendor = 0x18D1;
    device_desc.id_product = 0xA4A2;
    device_desc.bcd_device = 0x0100;
    device_desc.manufacturer = "Google";
    device_desc.product = "Fuchsia";
    device_desc.serial = "123456";
    device_desc.b_num_configurations = 1;
    return device_desc;
  }

  static fidl::VectorView<fidl::VectorView<fperipheral::wire::FunctionDescriptor>>
  CreateTestFunctionDescriptors(fidl::AnyArena& arena) {
    fperipheral::wire::FunctionDescriptor func_desc = {
        .interface_class = 0xFF,
        .interface_subclass = 0,
        .interface_protocol = 0,
    };
    fidl::VectorView<fperipheral::wire::FunctionDescriptor> functions(arena, 1);
    functions[0] = func_desc;
    fidl::VectorView<fidl::VectorView<fperipheral::wire::FunctionDescriptor>> configs(arena, 1);
    configs[0] = functions;
    return configs;
  }

  zx::result<std::vector<uint8_t>> CreateTestRawFunctionDescriptors(
      fidl::WireSyncClient<ffunction::UsbFunction>& function_client) {
    uint8_t interface_num = 0xFF;
    uint8_t ep_out = 0xFF;
    uint8_t ep_in = 0xFF;

    ffunction::wire::EndpointResource endpoints[2];
    zx::result ep_ends1 = fidl::CreateEndpoints<fendpoint::Endpoint>();
    if (ep_ends1.is_error()) {
      return ep_ends1.take_error();
    }
    endpoints[0].direction = fdescriptor::wire::EndpointDirection::kOut;
    endpoints[0].endpoint = std::move(ep_ends1->server);

    zx::result ep_ends2 = fidl::CreateEndpoints<fendpoint::Endpoint>();
    if (ep_ends2.is_error()) {
      return ep_ends2.take_error();
    }
    endpoints[1].direction = fdescriptor::wire::EndpointDirection::kIn;
    endpoints[1].endpoint = std::move(ep_ends2->server);

    fidl::WireResult res = function_client->AllocResources(
        1, fidl::VectorView<ffunction::wire::EndpointResource>::FromExternal(endpoints, 2), {});
    if (!res.ok()) {
      return zx::error(res.status());
    }
    if (res->is_error()) {
      return zx::error(res->error_value());
    }
    interface_num = res->value()->interface_nums[0];
    ep_out = res->value()->endpoint_addrs[0];
    ep_in = res->value()->endpoint_addrs[1];

    std::vector<uint8_t> descriptors = {
        0x09, 0x04, interface_num, 0x00, 0x02, 0xFF, 0x00, 0x00, 0x00,  // Interface
    };
    descriptors.insert(descriptors.end(),
                       {
                           0x07, 0x05, ep_out, 0x02, 0x40, 0x00, 0x00  // Bulk Out
                       });
    descriptors.insert(descriptors.end(), {
                                              0x07, 0x05, ep_in, 0x02, 0x40, 0x00, 0x00  // Bulk In
                                          });

    return zx::ok(descriptors);
  }

  zx::result<FunctionClients> TransitionToPeripheralReady(uint8_t num_functions = 1) {
    FunctionClients result;
    for (uint8_t i = 0; i < num_functions; i++) {
      char name[16];
      snprintf(name, sizeof(name), "function-%03d", i);
      this->dut().runtime().RunUntilIdle();
      this->WaitForChildNode(name);
      auto function_client = this->ConnectFunction(name);
      if (function_client.is_error()) {
        return function_client.take_error();
      }
      auto bind_res = this->BindFakeFunction();
      if (bind_res.is_error()) {
        return bind_res.take_error();
      }

      auto desc_res = CreateTestRawFunctionDescriptors(function_client.value());
      if (desc_res.is_error()) {
        return desc_res.take_error();
      }
      auto configure_res = function_client.value()->Configure(
          fidl::VectorView<uint8_t>::FromExternal(desc_res.value()),
          std::move(std::get<1>(bind_res.value())));
      if (!configure_res.ok()) {
        return zx::error(configure_res.status());
      }
      if (configure_res->is_error()) {
        return zx::error(configure_res->error_value());
      }
      result.clients.push_back(std::move(function_client.value()));
      result.fakes.push_back(std::get<0>(bind_res.value()));
      this->dut().runtime().RunUntilIdle();
    }
    this->ExpectState(UsbPeripheral::DeviceState::kPeripheralReady);
    this->ExpectControllerStarted(true);
    return zx::ok(std::move(result));
  }
};

using UnmanagedUsbPeripheralReadyTest = PeripheralReadyTestBase<false>;

class UsbPeripheralReadyTest : public PeripheralReadyTestBase<true> {
 public:
  void SetUp() override {
    PeripheralReadyTestBase<true>::SetUp();
    auto res = TransitionToPeripheralReady();
    ASSERT_OK(res);
    function_clients_ = std::move(res.value());
  }

  usb_peripheral_config::Config GetDriverConfig() override {
    usb_peripheral_config::Config config;
    config.functions() = {"test"};
    return config;
  }

  typename PeripheralReadyTestBase<true>::FunctionClients function_clients_;
};

class UsbPeripheralFunctionTest : public ManagedUsbPeripheralTest {
 public:
  usb_peripheral_config::Config GetDriverConfig() override {
    usb_peripheral_config::Config config;
    config.functions() = {"test"};
    return config;
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
  fidl::WireSyncClient<ffunction::UsbFunction> function_client =
      std::move(function_client_result.value());

  zx::result fake_function_result = BindFakeFunction();
  ASSERT_OK(fake_function_result);
  auto [fake_function, fake_function_endpoint] = std::move(fake_function_result.value());

  fidl::WireResult alloc_res = function_client->AllocResources(1, {}, {});
  ASSERT_TRUE(alloc_res.ok()) << alloc_res.status_string();
  ASSERT_TRUE(alloc_res->is_ok()) << zx_status_get_string(alloc_res->error_value());
  uint8_t interface_num = alloc_res->value()->interface_nums[0];

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
  ExpectControllerStarted(true);

  ExpectState(UsbPeripheral::DeviceState::kPeripheralReady);

  ASSERT_OK(dci()->SetConnected(true).status());

  ExpectState(UsbPeripheral::DeviceState::kHostConnected);

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

// Test that a repeated SetConfiguration request for the same configuration ID
// forces the function driver to transition through an unconfigured (false)
// state. In compliance with the USB 2.0 specification (section 9.1.1.5), this
// unconfigured transition ensures that all endpoints and interface state are
// reset to default values.
TEST_F(UsbPeripheralFunctionTest, RepeatedSetConfigurationResetsFunction) {
  zx::result function_client_result = ConnectFunction();
  ASSERT_OK(function_client_result);
  fidl::WireSyncClient<ffunction::UsbFunction> function_client =
      std::move(function_client_result.value());

  zx::result fake_function_result = BindFakeFunction();
  ASSERT_OK(fake_function_result);
  auto [fake_function, fake_function_endpoint] = std::move(fake_function_result.value());

  fidl::WireResult alloc_res = function_client->AllocResources(1, {}, {});
  ASSERT_TRUE(alloc_res.ok()) << alloc_res.status_string();
  ASSERT_TRUE(alloc_res->is_ok()) << zx_status_get_string(alloc_res->error_value());
  uint8_t interface_num = alloc_res->value()->interface_nums[0];

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
  ExpectControllerStarted(true);

  ExpectState(UsbPeripheral::DeviceState::kPeripheralReady);

  ASSERT_OK(dci()->SetConnected(true).status());

  ExpectState(UsbPeripheral::DeviceState::kHostConnected);

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

  EXPECT_TRUE(fake_function->set_configured_called());
  EXPECT_TRUE(fake_function->configured());
  ASSERT_EQ(fake_function->configured_history().size(), 1u);
  EXPECT_TRUE(fake_function->configured_history()[0]);

  // Test repeated SetConfiguration request causes unconfigure/reconfigure transition.
  fake_function->clear_set_configured_called();
  fidl::WireUnownedResult config_res2 =
      dci().buffer(arena)->Control(setup, fidl::VectorView<uint8_t>::FromExternal(unused));
  EXPECT_TRUE(config_res2.ok()) << config_res2.FormatDescription();
  ASSERT_OK(config_res2.value());

  EXPECT_TRUE(fake_function->set_configured_called());
  EXPECT_TRUE(fake_function->configured());
  ASSERT_EQ(fake_function->configured_history().size(), 3u);
  EXPECT_FALSE(fake_function->configured_history()[1]);
  EXPECT_TRUE(fake_function->configured_history()[2]);
}

TEST_F(UsbPeripheralFunctionTest, ClearFunctionsWaitsForTeardown) {
  zx::result peripheral_client_result = ConnectPeripheral();
  ASSERT_OK(peripheral_client_result);
  auto peripheral_client = std::move(peripheral_client_result.value());

  zx::result endpoints = fidl::CreateEndpoints<fperipheral::Events>();
  ASSERT_OK(endpoints);

  FakeEvents fake_events;
  fake_events.Bind(std::move(endpoints->server));

  auto set_listener_res = peripheral_client->SetStateChangeListener(std::move(endpoints->client));
  ASSERT_TRUE(set_listener_res.ok()) << set_listener_res.FormatDescription();

  // Add a function so there is something to clear.
  zx::result function_client_result = ConnectFunction();
  ASSERT_OK(function_client_result);
  fidl::WireSyncClient<ffunction::UsbFunction> function_client =
      std::move(function_client_result.value());

  zx::result fake_function_result = BindFakeFunction();
  ASSERT_OK(fake_function_result);
  auto [fake_function, fake_function_endpoint] = std::move(fake_function_result.value());

  fidl::WireResult alloc_res = function_client->AllocResources(1, {}, {});
  ASSERT_TRUE(alloc_res.ok()) << alloc_res.FormatDescription();
  ASSERT_OK(alloc_res.value());
  uint8_t interface_num = alloc_res->value()->interface_nums[0];

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

  // Clear functions and wait for event.
  auto clear_res = peripheral_client->ClearFunctions();
  ASSERT_TRUE(clear_res.ok()) << clear_res.FormatDescription();

  // Wait for async teardown to complete.
  this->dut().runtime().RunUntilIdle();

  // DCI should be stopped.
  ExpectControllerStarted(false);

  // 2. State should be back to kNoConfiguration.
  ExpectState(UsbPeripheral::DeviceState::kNoConfiguration);

  fake_events.WaitUntilCleared(this->dut().runtime());
  fake_events.Unbind();
}

TEST_F(UsbPeripheralFunctionTest, ConfigureFailsIfInterfaceNotAllocated) {
  ExpectControllerStarted(false);

  zx::result function_client_result = ConnectFunction();
  ASSERT_OK(function_client_result);
  fidl::WireSyncClient<ffunction::UsbFunction> function_client =
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
  fidl::WireSyncClient<ffunction::UsbFunction> function_client =
      std::move(function_client_result.value());

  zx::result fake_function_result = BindFakeFunction();
  ASSERT_OK(fake_function_result);
  auto [fake_function, fake_function_endpoint] = std::move(fake_function_result.value());

  fidl::WireResult alloc_res = function_client->AllocResources(1, {}, {});
  ASSERT_TRUE(alloc_res.ok()) << alloc_res.status_string();
  ASSERT_TRUE(alloc_res->is_ok()) << zx_status_get_string(alloc_res->error_value());
  uint8_t interface_num = alloc_res->value()->interface_nums[0];

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

  // Second call with a new endpoint should fail with ZX_ERR_ALREADY_BOUND.
  // The driver is already in kPeripheralReady because the first function was configured.
  ExpectState(UsbPeripheral::DeviceState::kPeripheralReady);
  zx::result endpoints = fidl::CreateEndpoints<ffunction::UsbFunctionInterface>();
  ASSERT_OK(endpoints);
  auto second_fake = std::make_shared<FakeUsbFunction>();
  second_fake->Bind(dut().runtime().StartBackgroundDispatcher(), std::move(endpoints->server));
  auto second_fake_endpoint = std::move(endpoints->client);
  ExpectState(UsbPeripheral::DeviceState::kPeripheralReady);

  fidl::WireResult second_configure_res = function_client->Configure(
      fidl::VectorView<uint8_t>::FromExternal(descriptors.data(), descriptors.size()),
      std::move(second_fake_endpoint));

  ASSERT_TRUE(second_configure_res.ok()) << second_configure_res.FormatDescription();
  EXPECT_STATUS(second_configure_res.value(), ZX_ERR_ALREADY_BOUND);
}

TEST_F(UsbPeripheralFunctionTest, DeconfigureAllowsReconfigure) {
  zx::result function_client_result = ConnectFunction();
  ASSERT_OK(function_client_result);
  fidl::WireSyncClient<ffunction::UsbFunction> function_client =
      std::move(function_client_result.value());

  zx::result fake_function_result = BindFakeFunction();
  ASSERT_OK(fake_function_result);
  auto [fake_function, fake_function_endpoint] = std::move(fake_function_result.value());

  fidl::WireResult alloc_res = function_client->AllocResources(1, {}, {});
  ASSERT_TRUE(alloc_res.ok()) << alloc_res.status_string();
  ASSERT_TRUE(alloc_res->is_ok()) << zx_status_get_string(alloc_res->error_value());
  uint8_t interface_num = alloc_res->value()->interface_nums[0];

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

  // First Configure
  {
    fidl::WireResult result = function_client->Configure(
        fidl::VectorView<uint8_t>::FromExternal(descriptors.data(), descriptors.size()),
        std::move(fake_function_endpoint));
    ASSERT_TRUE(result.ok()) << result.FormatDescription();
    ASSERT_OK(result.value());
  }
  ExpectState(UsbPeripheral::DeviceState::kPeripheralReady);

  // Deconfigure
  {
    fidl::WireResult result = function_client->Deconfigure();
    ASSERT_TRUE(result.ok()) << result.FormatDescription();
    ASSERT_OK(result.value());
  }
  fake_function->WaitUntilUnbound();
  ExpectState(UsbPeripheral::DeviceState::kWaitForFunctionBind);

  ExpectControllerStarted(false);

  // Now Configure should succeed again with a new endpoint
  fake_function_result = BindFakeFunction();
  ASSERT_OK(fake_function_result);
  auto [new_fake_function, new_fake_function_endpoint] = std::move(fake_function_result.value());

  {
    fidl::WireResult result = function_client->Configure(
        fidl::VectorView<uint8_t>::FromExternal(descriptors.data(), descriptors.size()),
        std::move(new_fake_function_endpoint));
    ASSERT_TRUE(result.ok()) << result.FormatDescription();
    ASSERT_OK(result.value());
  }
  ExpectState(UsbPeripheral::DeviceState::kPeripheralReady);

  ExpectControllerStarted(true);

  // Verify new fake function is called.
  {
    ASSERT_OK(dci()->SetConnected(true).status());
    fdescriptor::wire::UsbSetup setup;
    setup.bm_request_type = USB_DIR_OUT | USB_RECIP_DEVICE | USB_TYPE_STANDARD;
    setup.b_request = USB_REQ_SET_CONFIGURATION;
    setup.w_value = 1;  // Configuration 1
    setup.w_index = interface_num;
    setup.w_length = 0;
    fidl::Arena arena;
    std::vector<uint8_t> unused;
    fidl::WireResult config_res =
        dci()->Control(setup, fidl::VectorView<uint8_t>::FromExternal(unused));
    EXPECT_TRUE(config_res.ok()) << config_res.FormatDescription();
    ASSERT_OK(config_res.value());
  }

  new_fake_function->WaitUntilCalled();
  EXPECT_TRUE(new_fake_function->set_configured_called());
  EXPECT_TRUE(new_fake_function->configured());
}

TEST_F(UsbPeripheralFunctionTest, ControllerStoppedOnFunctionClose) {
  zx::result function_client_result = ConnectFunction();
  ASSERT_OK(function_client_result);
  fidl::WireSyncClient<ffunction::UsbFunction> function_client =
      std::move(function_client_result.value());

  zx::result fake_function_result = BindFakeFunction();
  ASSERT_OK(fake_function_result);
  auto [fake_function, fake_function_endpoint] = std::move(fake_function_result.value());

  fidl::WireResult alloc_res = function_client->AllocResources(1, {}, {});
  ASSERT_TRUE(alloc_res.ok()) << alloc_res.status_string();
  ASSERT_TRUE(alloc_res->is_ok()) << zx_status_get_string(alloc_res->error_value());
  uint8_t interface_num = alloc_res->value()->interface_nums[0];

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
  ExpectControllerStarted(true);

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

TEST_F(UsbPeripheralFunctionTest, AllocResources) {
  zx::result function_client_result = ConnectFunction();
  ASSERT_OK(function_client_result);
  fidl::WireSyncClient<ffunction::UsbFunction> function_client =
      std::move(function_client_result.value());

  fidl::Endpoints<fendpoint::Endpoint> ep_endpoints1 =
      fidl::Endpoints<fendpoint::Endpoint>::Create();
  fidl::Endpoints<fendpoint::Endpoint> ep_endpoints2 =
      fidl::Endpoints<fendpoint::Endpoint>::Create();

  zx_info_handle_basic_t info1, info2;
  ASSERT_OK(ep_endpoints1.server.channel().get_info(ZX_INFO_HANDLE_BASIC, &info1, sizeof(info1),
                                                    nullptr, nullptr));
  ASSERT_OK(ep_endpoints2.server.channel().get_info(ZX_INFO_HANDLE_BASIC, &info2, sizeof(info2),
                                                    nullptr, nullptr));

  fidl::Arena arena;
  auto endpoints = fidl::VectorView<ffunction::wire::EndpointResource>(arena, 2);
  endpoints[0].direction = fdescriptor::wire::EndpointDirection::kIn;
  endpoints[0].endpoint = std::move(ep_endpoints1.server);
  endpoints[1].direction = fdescriptor::wire::EndpointDirection::kOut;
  endpoints[1].endpoint = std::move(ep_endpoints2.server);

  auto strings = fidl::VectorView<fidl::StringView>(arena, 2);
  strings[0] = fidl::StringView(arena, "string1");
  strings[1] = fidl::StringView(arena, "string2");

  fidl::WireResult res = function_client->AllocResources(1, endpoints, strings);

  ASSERT_TRUE(res.ok()) << res.FormatDescription();
  ASSERT_TRUE(res->is_ok()) << zx_status_get_string(res->error_value());

  auto* response = res->value();
  ASSERT_EQ(response->interface_nums.size(), 1u);
  ASSERT_EQ(response->endpoint_addrs.size(), 2u);
  ASSERT_EQ(response->string_indices.size(), 2u);

  uint8_t ep1_addr = response->endpoint_addrs[0];
  uint8_t ep2_addr = response->endpoint_addrs[1];

  // Verify endpoints connected to DCI.
  dut().RunInEnvironmentTypeContext([&](UsbPeripheralTestEnvironment& env) {
    auto dci_ep1 = env.dci().TakeEndpoint(ep1_addr);
    auto dci_ep2 = env.dci().TakeEndpoint(ep2_addr);

    ASSERT_TRUE(dci_ep1.is_valid());
    ASSERT_TRUE(dci_ep2.is_valid());

    zx_info_handle_basic_t dci_info1, dci_info2;
    ASSERT_OK(dci_ep1.channel().get_info(ZX_INFO_HANDLE_BASIC, &dci_info1, sizeof(dci_info1),
                                         nullptr, nullptr));
    ASSERT_OK(dci_ep2.channel().get_info(ZX_INFO_HANDLE_BASIC, &dci_info2, sizeof(dci_info2),
                                         nullptr, nullptr));

    EXPECT_EQ(info1.koid, dci_info1.koid);
    EXPECT_EQ(info2.koid, dci_info2.koid);
  });

  zx::result fake_function_result = BindFakeFunction();
  ASSERT_OK(fake_function_result);
  auto [fake_function, fake_function_endpoint] = std::move(fake_function_result.value());

  struct {
    usb_interface_descriptor_t intf;
    usb_endpoint_descriptor_t ep1;
    usb_endpoint_descriptor_t ep2;
  } __PACKED combined_descriptors = {
      .intf =
          {
              .b_length = sizeof(usb_interface_descriptor_t),
              .b_descriptor_type = USB_DT_INTERFACE,
              .b_interface_number = response->interface_nums[0],
              .b_num_endpoints = 2,
              .b_interface_class = 8,
              .b_interface_sub_class = 6,
              .b_interface_protocol = 80,
              .i_interface = response->string_indices[0],
          },
      .ep1 =
          {
              .b_length = sizeof(usb_endpoint_descriptor_t),
              .b_descriptor_type = USB_DT_ENDPOINT,
              .b_endpoint_address = ep1_addr,
              .bm_attributes = USB_ENDPOINT_BULK,
              .w_max_packet_size = 512,
          },
      .ep2 =
          {
              .b_length = sizeof(usb_endpoint_descriptor_t),
              .b_descriptor_type = USB_DT_ENDPOINT,
              .b_endpoint_address = ep2_addr,
              .bm_attributes = USB_ENDPOINT_BULK,
              .w_max_packet_size = 512,
          },
  };

  std::vector<uint8_t> descriptors_vec(sizeof(combined_descriptors));
  memcpy(descriptors_vec.data(), &combined_descriptors, sizeof(combined_descriptors));

  fidl::WireResult configure_res = function_client->Configure(
      fidl::VectorView<uint8_t>::FromExternal(descriptors_vec.data(), descriptors_vec.size()),
      std::move(fake_function_endpoint));

  ASSERT_TRUE(configure_res.ok()) << configure_res.status_string();
  ASSERT_TRUE(configure_res->is_ok());
}

TEST_F(UsbPeripheralFunctionTest, ResourceCleanupOnClose) {
  zx::result function_client_result = ConnectFunction();
  ASSERT_OK(function_client_result);
  auto function_client = std::move(function_client_result.value());

  fidl::Arena arena;
  auto endpoints = fidl::VectorView<ffunction::wire::EndpointResource>(arena, 1);
  endpoints[0].direction = fdescriptor::wire::EndpointDirection::kIn;
  auto ep_endpoints = fidl::Endpoints<fendpoint::Endpoint>::Create();
  endpoints[0].endpoint = std::move(ep_endpoints.server);

  auto strings = fidl::VectorView<fidl::StringView>(arena, 1);
  strings[0] = fidl::StringView(arena, "cleanup_test_string");

  fidl::WireResult res = function_client->AllocResources(1, endpoints, strings);
  ASSERT_TRUE(res.ok()) << res.FormatDescription();
  ASSERT_TRUE(res->is_ok()) << zx_status_get_string(res->error_value());

  // Verify resources are allocated.
  UsbPeripheral::ResourceAllocations allocations;
  dut().RunInDriverContext(
      [&](UsbPeripheral& peripheral) { allocations = peripheral.GetResourceAllocations(0); });
  ASSERT_EQ(allocations.interface_nums.size(), 1u);
  ASSERT_EQ(allocations.endpoint_addrs.size(), 1u);
  ASSERT_EQ(allocations.string_indices.size(), 1u);

  // Close the FIDL connection.
  function_client = {};

  // Verify resources are cleared.
  dut().runtime().RunUntil([&]() {
    dut().RunInDriverContext(
        [&](UsbPeripheral& peripheral) { allocations = peripheral.GetResourceAllocations(0); });
    return allocations.interface_nums.empty() && allocations.endpoint_addrs.empty() &&
           allocations.string_indices.empty();
  });
}

TEST_F(UsbPeripheralFunctionTest, AllocResourcesRollback) {
  zx::result function_client_result = ConnectFunction();
  ASSERT_OK(function_client_result);
  auto function_client = std::move(function_client_result.value());

  fidl::Arena arena;

  // 1. Initial success allocation to have a baseline of "used" resources.
  {
    auto endpoints = fidl::VectorView<ffunction::wire::EndpointResource>(arena, 1);
    endpoints[0].direction = fdescriptor::wire::EndpointDirection::kIn;
    auto ep_endpoints = fidl::Endpoints<fendpoint::Endpoint>::Create();
    endpoints[0].endpoint = std::move(ep_endpoints.server);

    auto strings = fidl::VectorView<fidl::StringView>(arena, 1);
    strings[0] = fidl::StringView(arena, "initial_string");

    fidl::WireResult res = function_client->AllocResources(1, endpoints, strings);
    ASSERT_TRUE(res.ok()) << res.FormatDescription();
    ASSERT_TRUE(res.value().is_ok()) << zx_status_get_string(res.value().error_value());
  }

  UsbPeripheral::ResourceAllocations initial;
  dut().RunInDriverContext(
      [&](UsbPeripheral& peripheral) { initial = peripheral.GetResourceAllocations(0); });
  ASSERT_EQ(initial.interface_nums.size(), 1u);
  ASSERT_EQ(initial.endpoint_addrs.size(), 1u);
  ASSERT_EQ(initial.string_indices.size(), 1u);

  // 2. Perform a request that should succeed for strings and endpoints, but
  //    fails for interfaces. We already have 1 interface. Requesting
  //    UsbPeripheral::MAX_INTERFACES more should fail.
  {
    auto endpoints = fidl::VectorView<ffunction::wire::EndpointResource>(arena, 1);
    endpoints[0].direction = fdescriptor::wire::EndpointDirection::kOut;
    auto ep_endpoints = fidl::Endpoints<fendpoint::Endpoint>::Create();
    endpoints[0].endpoint = std::move(ep_endpoints.server);

    auto strings = fidl::VectorView<fidl::StringView>(arena, 1);
    strings[0] = fidl::StringView(arena, "should_rollback");

    fidl::WireResult res =
        function_client->AllocResources(UsbPeripheral::kMaxInterfaces, endpoints, strings);
    ASSERT_TRUE(res.ok()) << res.FormatDescription();
    EXPECT_STATUS(res.value().error_value(), ZX_ERR_NO_RESOURCES);
  }

  // Verify only initial resources remain.
  UsbPeripheral::ResourceAllocations allocations;
  dut().RunInDriverContext(
      [&](UsbPeripheral& peripheral) { allocations = peripheral.GetResourceAllocations(0); });
  EXPECT_EQ(allocations.interface_nums, initial.interface_nums);
  EXPECT_EQ(allocations.endpoint_addrs, initial.endpoint_addrs);
  EXPECT_EQ(allocations.string_indices, initial.string_indices);

  // 3. Perform a request that should succeed for interfaces and endpoints, but
  //    fails for strings. Global strings (3) + Initial function strings taken.
  //    Requesting enough to exceed UsbPeripheral::MAX_STRINGS should fail.
  {
    auto endpoints = fidl::VectorView<ffunction::wire::EndpointResource>(arena, 1);
    endpoints[0].direction = fdescriptor::wire::EndpointDirection::kOut;
    auto ep_endpoints = fidl::Endpoints<fendpoint::Endpoint>::Create();
    endpoints[0].endpoint = std::move(ep_endpoints.server);

    std::vector<fidl::StringView> strings_vec(UsbPeripheral::kMaxStrings,
                                              fidl::StringView(arena, "too_many"));

    fidl::WireResult res = function_client->AllocResources(
        1, endpoints,
        fidl::VectorView<fidl::StringView>::FromExternal(strings_vec.data(), strings_vec.size()));
    ASSERT_TRUE(res.ok()) << res.FormatDescription();
    EXPECT_STATUS(res.value().error_value(), ZX_ERR_NO_RESOURCES);
  }

  // Verify only initial resources remain.
  dut().RunInDriverContext(
      [&](UsbPeripheral& peripheral) { allocations = peripheral.GetResourceAllocations(0); });
  EXPECT_EQ(allocations.interface_nums, initial.interface_nums);
  EXPECT_EQ(allocations.endpoint_addrs, initial.endpoint_addrs);
  EXPECT_EQ(allocations.string_indices, initial.string_indices);

  // 4. Perform a request that should succeed for strings and interfaces, but
  //    fails for endpoints. Initial function IN endpoint (1) taken. Total IN
  //    endpoints available: UsbPeripheral::IN_EP_END -
  //    UsbPeripheral::IN_EP_START + 1.
  {
    size_t total_in_eps = UsbPeripheral::kInEpEnd - UsbPeripheral::kInEpStart + 1;
    auto endpoints = fidl::VectorView<ffunction::wire::EndpointResource>(arena, total_in_eps);
    for (size_t i = 0; i < total_in_eps; i++) {
      endpoints[i].direction = fdescriptor::wire::EndpointDirection::kIn;
      auto ep_endpoints = fidl::Endpoints<fendpoint::Endpoint>::Create();
      endpoints[i].endpoint = std::move(ep_endpoints.server);
    }

    auto strings = fidl::VectorView<fidl::StringView>(arena, 1);
    strings[0] = fidl::StringView(arena, "should_rollback");

    fidl::WireResult res = function_client->AllocResources(1, endpoints, strings);
    ASSERT_TRUE(res.ok()) << res.FormatDescription();
    EXPECT_STATUS(res.value().error_value(), ZX_ERR_NO_RESOURCES);
  }

  // Verify only initial resources remain.
  dut().RunInDriverContext(
      [&](UsbPeripheral& peripheral) { allocations = peripheral.GetResourceAllocations(0); });
  EXPECT_EQ(allocations.interface_nums, initial.interface_nums);
  EXPECT_EQ(allocations.endpoint_addrs, initial.endpoint_addrs);
  EXPECT_EQ(allocations.string_indices, initial.string_indices);
}

TEST_F(UsbPeripheralFunctionTest, EndpointSetStall) {
  zx::result function_client_result = ConnectFunction();
  ASSERT_OK(function_client_result);
  fidl::WireSyncClient<ffunction::UsbFunction> function_client =
      std::move(function_client_result.value());

  fidl::Arena arena;
  auto endpoints = fidl::VectorView<ffunction::wire::EndpointResource>(arena, 1);
  endpoints[0].direction = fdescriptor::wire::EndpointDirection::kIn;
  auto ep_endpoints = fidl::Endpoints<fendpoint::Endpoint>::Create();
  endpoints[0].endpoint = std::move(ep_endpoints.server);

  fidl::WireResult alloc_res = function_client->AllocResources(0, endpoints, {});
  ASSERT_TRUE(alloc_res.ok()) << alloc_res.status_string();
  ASSERT_OK(alloc_res.value());

  uint8_t ep_addr = alloc_res->value()->endpoint_addrs[0];

  // Test setting a stall on an allocated endpoint.
  auto res = function_client->EndpointSetStall(ep_addr);
  ASSERT_TRUE(res.ok()) << res.FormatDescription();
  ASSERT_OK(res.value());

  dut().RunInEnvironmentTypeContext([ep_addr](UsbPeripheralTestEnvironment& env) {
    EXPECT_EQ(env.dci().set_stalls_.size(), 1u);
    EXPECT_EQ(env.dci().set_stalls_[0], ep_addr);
  });

  // Test an unknown/failing endpoint stall by toggling `fail_stall_` in our mock.
  dut().RunInEnvironmentTypeContext(
      [](UsbPeripheralTestEnvironment& env) { env.dci().fail_stall_ = true; });

  auto res2 = function_client->EndpointSetStall(ep_addr);
  ASSERT_TRUE(res2.ok()) << res2.FormatDescription();
  EXPECT_STATUS(res2.value(), ZX_ERR_IO_NOT_PRESENT);

  // Test setting a stall on an unallocated endpoint.
  uint8_t unallocated_ep_addr = (ep_addr == 0x81) ? 0x82 : 0x81;
  auto res3 = function_client->EndpointSetStall(unallocated_ep_addr);
  ASSERT_TRUE(res3.ok()) << res3.FormatDescription();
  EXPECT_STATUS(res3.value(), ZX_ERR_NOT_FOUND);
}

TEST_F(UsbPeripheralFunctionTest, EndpointClearStall) {
  zx::result function_client_result = ConnectFunction();
  ASSERT_OK(function_client_result);
  fidl::WireSyncClient<ffunction::UsbFunction> function_client =
      std::move(function_client_result.value());

  fidl::Arena arena;
  auto endpoints = fidl::VectorView<ffunction::wire::EndpointResource>(arena, 1);
  endpoints[0].direction = fdescriptor::wire::EndpointDirection::kIn;
  auto ep_endpoints = fidl::Endpoints<fendpoint::Endpoint>::Create();
  endpoints[0].endpoint = std::move(ep_endpoints.server);

  fidl::WireResult alloc_res =
      function_client->AllocResources(0, endpoints, fidl::VectorView<fidl::StringView>());
  ASSERT_TRUE(alloc_res.ok()) << alloc_res.status_string();
  ASSERT_OK(alloc_res.value());

  uint8_t ep_addr = alloc_res->value()->endpoint_addrs[0];

  // Test clearing a stall on an allocated endpoint.
  auto res = function_client->EndpointClearStall(ep_addr);
  ASSERT_TRUE(res.ok()) << res.FormatDescription();
  ASSERT_OK(res.value());

  dut().RunInEnvironmentTypeContext([ep_addr](UsbPeripheralTestEnvironment& env) {
    EXPECT_EQ(env.dci().clear_stalls_.size(), 1u);
    EXPECT_EQ(env.dci().clear_stalls_[0], ep_addr);
  });

  // Test an unknown/failing endpoint stall by toggling `fail_stall_` in our mock.
  dut().RunInEnvironmentTypeContext(
      [](UsbPeripheralTestEnvironment& env) { env.dci().fail_stall_ = true; });

  auto res2 = function_client->EndpointClearStall(ep_addr);
  ASSERT_TRUE(res2.ok()) << res2.FormatDescription();
  EXPECT_STATUS(res2.value(), ZX_ERR_IO_NOT_PRESENT);

  // Test clearing a stall on an unallocated endpoint.
  uint8_t unallocated_ep_addr = (ep_addr == 0x81) ? 0x82 : 0x81;
  auto res3 = function_client->EndpointClearStall(unallocated_ep_addr);
  ASSERT_TRUE(res3.ok()) << res3.FormatDescription();
  EXPECT_STATUS(res3.value(), ZX_ERR_NOT_FOUND);
}

class UsbPeripheralFunctionConfigureEndpointTest : public UsbPeripheralFunctionTest,
                                                   public testing::WithParamInterface<bool> {};

TEST_P(UsbPeripheralFunctionConfigureEndpointTest, ConfigureEndpoint) {
  zx::result function_client_result = ConnectFunction();
  ASSERT_OK(function_client_result);
  fidl::WireSyncClient<ffunction::UsbFunction> function_client =
      std::move(function_client_result.value());

  fidl::Arena arena;
  auto endpoints = fidl::VectorView<ffunction::wire::EndpointResource>(arena, 1);
  endpoints[0].direction = fdescriptor::wire::EndpointDirection::kIn;
  auto ep_endpoints = fidl::Endpoints<fendpoint::Endpoint>::Create();
  endpoints[0].endpoint = std::move(ep_endpoints.server);

  fidl::WireResult alloc_res =
      function_client->AllocResources(0, endpoints, fidl::VectorView<fidl::StringView>());
  ASSERT_TRUE(alloc_res.ok()) << alloc_res.status_string();
  ASSERT_OK(alloc_res.value());

  uint8_t ep_addr = alloc_res->value()->endpoint_addrs[0];

  ffunction::wire::EndpointDescriptor desc = {
      .bm_attributes = 1,
      .w_max_packet_size = 2,
      .b_interval = 3,
  };

  auto config_builder = ffunction::wire::EndpointConfiguration::Builder(arena);
  config_builder.descriptor(desc);

  bool with_ss_companion = GetParam();
  ffunction::wire::SuperSpeedEndpointCompanionDescriptor ss_desc;
  if (with_ss_companion) {
    ss_desc = {
        .b_max_burst = 5,
        .bm_attributes = 4,
        .w_bytes_per_interval = 6,
    };
    config_builder.super_speed_companion(ss_desc);
  }

  ffunction::wire::EndpointConfiguration config = config_builder.Build();

  auto res = function_client->ConfigureEndpoint(ep_addr, config);
  ASSERT_TRUE(res.ok()) << res.FormatDescription();
  ASSERT_OK(res.value());

  dut().RunInEnvironmentTypeContext([ep_addr, &desc, with_ss_companion,
                                     &ss_desc](UsbPeripheralTestEnvironment& env) {
    EXPECT_EQ(env.dci().configured_endpoints_.size(), 1u);
    EXPECT_EQ(env.dci().configured_endpoints_[0].b_endpoint_address, ep_addr);
    EXPECT_EQ(env.dci().configured_endpoints_[0].w_max_packet_size, desc.w_max_packet_size);
    EXPECT_EQ(env.dci().configured_endpoints_[0].bm_attributes, desc.bm_attributes);
    EXPECT_EQ(env.dci().configured_endpoints_[0].b_interval, desc.b_interval);
    if (with_ss_companion) {
      EXPECT_EQ(env.dci().configured_endpoints_ss_companion_[0].b_max_burst, ss_desc.b_max_burst);
      EXPECT_EQ(env.dci().configured_endpoints_ss_companion_[0].bm_attributes,
                ss_desc.bm_attributes);
      EXPECT_EQ(env.dci().configured_endpoints_ss_companion_[0].w_bytes_per_interval,
                ss_desc.w_bytes_per_interval);
    } else {
      EXPECT_EQ(env.dci().configured_endpoints_ss_companion_[0].b_max_burst, 0);
      EXPECT_EQ(env.dci().configured_endpoints_ss_companion_[0].bm_attributes, 0);
      EXPECT_EQ(env.dci().configured_endpoints_ss_companion_[0].w_bytes_per_interval, 0u);
    }
  });

  // Test unknown endpoint configuration.
  uint8_t unallocated_ep_addr = (ep_addr == 0x81) ? 0x82 : 0x81;
  auto res2 = function_client->ConfigureEndpoint(unallocated_ep_addr, config);
  ASSERT_TRUE(res2.ok()) << res2.FormatDescription();
  EXPECT_STATUS(res2.value(), ZX_ERR_NOT_FOUND);

  // Test failing configuration from DCI.
  dut().RunInEnvironmentTypeContext(
      [](UsbPeripheralTestEnvironment& env) { env.dci().fail_configure_ = true; });
  auto res3 = function_client->ConfigureEndpoint(ep_addr, config);
  ASSERT_TRUE(res3.ok()) << res3.FormatDescription();
  EXPECT_STATUS(res3.value(), ZX_ERR_IO_NOT_PRESENT);
}

INSTANTIATE_TEST_SUITE_P(UsbPeripheralFunctionConfigureEndpointTest,
                         UsbPeripheralFunctionConfigureEndpointTest, testing::Bool());

TEST_F(UsbPeripheralFunctionTest, DisableEndpoint) {
  zx::result function_client_result = ConnectFunction();
  ASSERT_OK(function_client_result);
  fidl::WireSyncClient<ffunction::UsbFunction> function_client =
      std::move(function_client_result.value());

  fidl::Arena arena;
  auto endpoints = fidl::VectorView<ffunction::wire::EndpointResource>(arena, 1);
  endpoints[0].direction = fdescriptor::wire::EndpointDirection::kIn;
  auto ep_endpoints = fidl::Endpoints<fendpoint::Endpoint>::Create();
  endpoints[0].endpoint = std::move(ep_endpoints.server);

  fidl::WireResult alloc_res =
      function_client->AllocResources(0, endpoints, fidl::VectorView<fidl::StringView>());
  ASSERT_TRUE(alloc_res.ok()) << alloc_res.status_string();
  ASSERT_OK(alloc_res.value());

  uint8_t ep_addr = alloc_res->value()->endpoint_addrs[0];

  // Test disabling an allocated endpoint.
  auto res = function_client->DisableEndpoint(ep_addr);
  ASSERT_TRUE(res.ok()) << res.FormatDescription();
  ASSERT_TRUE(res->is_ok()) << zx_status_get_string(res->error_value());

  dut().RunInEnvironmentTypeContext([ep_addr](UsbPeripheralTestEnvironment& env) {
    EXPECT_EQ(env.dci().disabled_endpoints_.size(), 1u);
    EXPECT_EQ(env.dci().disabled_endpoints_[0], ep_addr);
  });

  // Test unknown endpoint disable
  uint8_t unallocated_ep_addr = (ep_addr == 0x81) ? 0x82 : 0x81;
  auto res2 = function_client->DisableEndpoint(unallocated_ep_addr);
  ASSERT_TRUE(res2.ok()) << res2.FormatDescription();
  EXPECT_STATUS(res2.value(), ZX_ERR_NOT_FOUND);

  // Test failing disable from DCI
  dut().RunInEnvironmentTypeContext(
      [](UsbPeripheralTestEnvironment& env) { env.dci().fail_disable_ = true; });
  auto res3 = function_client->DisableEndpoint(ep_addr);
  ASSERT_TRUE(res3.ok()) << res3.FormatDescription();
  EXPECT_STATUS(res3.value(), ZX_ERR_IO_NOT_PRESENT);
}

TEST_F(UsbPeripheralFunctionTest, ConfigureEndpointDuringSetConfigured) {
  zx::result function_client_result = ConnectFunction();
  ASSERT_OK(function_client_result);
  fidl::WireSyncClient<ffunction::UsbFunction> function_client =
      std::move(function_client_result.value());

  zx::result fake_function_result = BindFakeFunction();
  ASSERT_OK(fake_function_result);
  auto [fake_function, fake_function_endpoint] = std::move(fake_function_result.value());

  fidl::Arena arena;
  auto endpoints = fidl::VectorView<ffunction::wire::EndpointResource>(arena, 1);
  endpoints[0].direction = fdescriptor::wire::EndpointDirection::kIn;
  auto ep_endpoints = fidl::Endpoints<fendpoint::Endpoint>::Create();
  endpoints[0].endpoint = std::move(ep_endpoints.server);

  fidl::WireResult alloc_res =
      function_client->AllocResources(1, endpoints, fidl::VectorView<fidl::StringView>());
  ASSERT_TRUE(alloc_res.ok());
  ASSERT_OK(alloc_res.value());

  uint8_t ep_addr = alloc_res->value()->endpoint_addrs[0];
  uint8_t interface_num = alloc_res->value()->interface_nums[0];

  std::vector<uint8_t> descriptors;
  usb_interface_descriptor_t intf_desc = {
      .b_length = sizeof(usb_interface_descriptor_t),
      .b_descriptor_type = USB_DT_INTERFACE,
      .b_interface_number = interface_num,
      .b_alternate_setting = 0,
      .b_num_endpoints = 0,
  };
  descriptors.resize(sizeof(intf_desc));
  memcpy(descriptors.data(), &intf_desc, sizeof(intf_desc));

  fidl::WireResult configure_res = function_client->Configure(
      fidl::VectorView<uint8_t>::FromExternal(descriptors.data(), descriptors.size()),
      std::move(fake_function_endpoint));
  ASSERT_TRUE(configure_res.ok()) << configure_res.FormatDescription();
  ASSERT_OK(configure_res.value());

  ExpectControllerStarted(true);
  ASSERT_OK(dci()->SetConnected(true).status());

  libsync::Completion configure_endpoint_completed;

  // Set the callback that simulates the condition of calling back into the
  // function before responding to set configured.
  fake_function->set_on_set_configured([&]() {
    fidl::Arena arena;
    ffunction::wire::EndpointDescriptor desc;
    desc.bm_attributes = 2;
    desc.w_max_packet_size = 512;
    desc.b_interval = 0;
    auto config_builder = ffunction::wire::EndpointConfiguration::Builder(arena);
    config_builder.descriptor(desc);

    auto res = function_client->ConfigureEndpoint(ep_addr, config_builder.Build());
    ASSERT_TRUE(res.ok()) << res.FormatDescription();
    ASSERT_OK(res.value());

    configure_endpoint_completed.Signal();
  });

  // Trigger SetConfigured by sending standard endpoint request SetConfiguration = 1
  fdescriptor::wire::UsbSetup setup;
  setup.bm_request_type = USB_DIR_OUT | USB_RECIP_DEVICE | USB_TYPE_STANDARD;
  setup.b_request = USB_REQ_SET_CONFIGURATION;
  setup.w_value = 1;
  setup.w_index = 0;
  setup.w_length = 0;

  std::vector<uint8_t> unused;
  fidl::WireUnownedResult config_res =
      dci().buffer(arena)->Control(setup, fidl::VectorView<uint8_t>::FromExternal(unused));
  ASSERT_TRUE(config_res.ok()) << config_res.FormatDescription();
  ASSERT_OK(config_res.value());

  fake_function->WaitUntilCalled();
  EXPECT_TRUE(fake_function->set_configured_called());
  EXPECT_TRUE(fake_function->configured());

  configure_endpoint_completed.Wait();

  dut().RunInEnvironmentTypeContext([ep_addr](UsbPeripheralTestEnvironment& env) {
    EXPECT_EQ(env.dci().configured_endpoints_.size(), 1u);
    EXPECT_EQ(env.dci().configured_endpoints_[0].b_endpoint_address, ep_addr);
  });
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

}  // namespace
}  // namespace usb_peripheral::test
