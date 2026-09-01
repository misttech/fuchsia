// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "usb-cdc-function.h"

#include <endian.h>
#include <fidl/fuchsia.boot.metadata/cpp/fidl.h>
#include <fidl/fuchsia.hardware.network.driver/cpp/driver/fidl.h>
#include <fidl/fuchsia.hardware.network.driver/cpp/driver/wire_test_base.h>
#include <fidl/fuchsia.hardware.network.driver/cpp/fidl.h>
#include <fidl/fuchsia.hardware.network/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.endpoint/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.function/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async/cpp/task.h>
#include <lib/async/default.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/driver/compat/cpp/device_server.h>
#include <lib/driver/metadata/cpp/metadata_server.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/fit/defer.h>
#include <lib/inspect/testing/cpp/inspect.h>
#include <lib/sync/cpp/completion.h>

#include <gmock/gmock.h>
#include <gtest/gtest.h>
#include <usb-inspect/usb-inspect-test-helper.h>

#include "src/devices/usb/lib/usb-endpoint/testing/fake-usb-endpoint-server.h"
#include "src/lib/testing/predicates/status.h"

namespace usb_cdc_function {
namespace {

constexpr uint8_t kBulkOutEp = 1;
constexpr uint8_t kBulkInEp = 2;
constexpr uint8_t kIntrEp = 3;
constexpr uint8_t kCommInterface = 0;
constexpr uint8_t kDataInterface = 1;

class FakeNetworkDeviceIfc : public fidl::testing::WireTestBase<fnetdev::NetworkDeviceIfc> {
 public:
  FakeNetworkDeviceIfc() = default;

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    ADD_FAILURE() << "FakeNetworkDeviceIfc not implemented: " << name;
    if (completer.is_reply_needed()) {
      completer.Close(ZX_ERR_NOT_SUPPORTED);
    }
  }

  void AddPort(fnetdev::wire::NetworkDeviceIfcAddPortRequest* request, fdf::Arena& arena,
               AddPortCompleter::Sync& completer) override {
    fit::function<void()> callback;
    {
      std::lock_guard<std::mutex> guard(lock_);
      port_id_ = request->id;
      port_ = std::move(request->port);
      callback = on_add_port_.share();
    }
    completer.ToAsync().buffer(arena).Reply(ZX_OK);
    if (callback) {
      callback();
    }
  }

  void CompleteRx(fnetdev::wire::NetworkDeviceIfcCompleteRxRequest* request, fdf::Arena& arena,
                  CompleteRxCompleter::Sync& completer) override {
    fit::function<void()> callback;
    {
      std::lock_guard<std::mutex> guard(lock_);
      for (const auto& result : request->rx) {
        completed_rx_.push(fidl::ToNatural(result));
      }
      callback = on_complete_rx_.share();
    }
    if (callback) {
      callback();
    }
  }

  void CompleteTx(fnetdev::wire::NetworkDeviceIfcCompleteTxRequest* request, fdf::Arena& arena,
                  CompleteTxCompleter::Sync& completer) override {
    fit::function<void()> callback;
    {
      std::lock_guard<std::mutex> guard(lock_);
      for (const auto& result : request->tx) {
        completed_tx_.push(result);
      }
      callback = on_complete_tx_.share();
    }
    if (callback) {
      callback();
    }
  }

  void PortStatusChanged(fnetdev::wire::NetworkDeviceIfcPortStatusChangedRequest* request,
                         fdf::Arena& arena, PortStatusChangedCompleter::Sync& completer) override {}

  bool HasPort() {
    std::lock_guard<std::mutex> guard(lock_);
    return port_.is_valid();
  }

  fdf::ClientEnd<fnetdev::NetworkPort> TakePort() {
    std::lock_guard<std::mutex> guard(lock_);
    return std::move(port_);
  }

  void set_on_add_port(fit::function<void()> callback) {
    std::lock_guard<std::mutex> guard(lock_);
    on_add_port_ = std::move(callback);
  }
  void set_on_complete_tx(fit::function<void()> callback) {
    std::lock_guard<std::mutex> guard(lock_);
    on_complete_tx_ = std::move(callback);
  }
  void set_on_complete_rx(fit::function<void()> callback) {
    std::lock_guard<std::mutex> guard(lock_);
    on_complete_rx_ = std::move(callback);
  }

  std::optional<fnetdev::wire::TxResult> PopCompleteTx() {
    std::lock_guard<std::mutex> guard(lock_);
    if (completed_tx_.empty()) {
      return std::nullopt;
    }
    auto tx = std::move(completed_tx_.front());
    completed_tx_.pop();
    return tx;
  }
  std::optional<fnetdev::RxBuffer> PopCompleteRx() {
    std::lock_guard<std::mutex> guard(lock_);
    if (completed_rx_.empty()) {
      return std::nullopt;
    }
    auto rx = std::move(completed_rx_.front());
    completed_rx_.pop();
    return rx;
  }

 private:
  std::mutex lock_;
  uint8_t port_id_;
  fdf::ClientEnd<fnetdev::NetworkPort> port_;
  fit::function<void()> on_add_port_;
  fit::function<void()> on_complete_tx_;
  fit::function<void()> on_complete_rx_;
  std::queue<fnetdev::wire::TxResult> completed_tx_;
  std::queue<fnetdev::RxBuffer> completed_rx_;
};

class DelayedCancelEndpoint : public fake_usb_endpoint::FakeEndpoint {
 public:
  void CancelAll(CancelAllCompleter::Sync& completer) override {
    {
      std::lock_guard<std::mutex> guard(lock_);
      cancel_all_called_ = true;
      if (hold_cancel_) {
        EXPECT_FALSE(delayed_cancel_.has_value())
            << "Concurrent CancelAll called while still held!";
        delayed_cancel_ = completer.ToAsync();
        return;
      }
    }
    DoCancelAll(completer.ToAsync());
  }

  void ReleaseCancelAll() {
    std::optional<CancelAllCompleter::Async> completer;
    {
      std::lock_guard<std::mutex> guard(lock_);
      completer = std::exchange(delayed_cancel_, std::nullopt);
    }
    if (completer.has_value()) {
      DoCancelAll(std::move(*completer));
    }
  }

  void set_hold_cancel(bool hold) {
    std::lock_guard<std::mutex> guard(lock_);
    hold_cancel_ = hold;
  }
  bool has_delayed_cancel() {
    std::lock_guard<std::mutex> guard(lock_);
    return delayed_cancel_.has_value();
  }

  bool has_pending_requests() {
    std::lock_guard<std::mutex> guard(lock_);
    return !requests_.empty();
  }

  bool cancel_all_called() {
    std::lock_guard<std::mutex> guard(lock_);
    return cancel_all_called_;
  }
  void reset_cancel_all_called() {
    std::lock_guard<std::mutex> guard(lock_);
    cancel_all_called_ = false;
  }

 private:
  void DoCancelAll(CancelAllCompleter::Async completer) {
    std::vector<fuchsia_hardware_usb_endpoint::Completion> completions;
    std::optional<fidl::ServerBindingRef<fuchsia_hardware_usb_endpoint::Endpoint>> local_binding;
    {
      std::lock_guard<std::mutex> guard(lock_);
      completions = CancelAllLocked(/*is_disable=*/false);
      local_binding = binding_ref_;
    }
    if (!completions.empty() && local_binding) {
      (void)fidl::SendEvent(*local_binding)->OnCompletion(std::move(completions));
    }
    completer.Reply(fit::ok());
  }

  bool hold_cancel_ = false;
  bool cancel_all_called_ = false;
  std::optional<CancelAllCompleter::Async> delayed_cancel_;
};

class FakeUsbFunction
    : public fake_usb_endpoint::FakeUsbFidlProvider<fuchsia_hardware_usb_function::UsbFunction,
                                                    DelayedCancelEndpoint> {
 public:
  using Base = fake_usb_endpoint::FakeUsbFidlProvider<fuchsia_hardware_usb_function::UsbFunction,
                                                      DelayedCancelEndpoint>;
  using Base::Base;

  void Configure(
      fidl::Request<fuchsia_hardware_usb_function::UsbFunction::Configure>& request,
      fidl::internal::NaturalCompleter<fuchsia_hardware_usb_function::UsbFunction::Configure>::Sync&
          completer) override {
    fit::callback<void()> callback;
    {
      std::lock_guard<std::mutex> guard(lock_);
      interface_ = std::move(request.iface());
      callback = std::move(on_configure_);
    }
    completer.Reply(fit::ok());
    if (callback) {
      std::move(callback)();
    }
  }

  void Deconfigure(
      fidl::internal::NaturalCompleter<
          fuchsia_hardware_usb_function::UsbFunction::Deconfigure>::Sync& completer) override {
    {
      std::lock_guard<std::mutex> guard(lock_);
      if (hold_deconfigure_) {
        EXPECT_FALSE(delayed_deconfigure_completer_.has_value())
            << "Concurrent Deconfigure detected while still held!";
        delayed_deconfigure_completer_ = completer.ToAsync();
        return;
      }
    }
    completer.Reply(fit::ok());
  }

  void ReleaseDelayedDeconfigure() {
    std::optional<fidl::internal::NaturalCompleter<
        fuchsia_hardware_usb_function::UsbFunction::Deconfigure>::Async>
        completer;
    {
      std::lock_guard<std::mutex> guard(lock_);
      completer = std::exchange(delayed_deconfigure_completer_, std::nullopt);
    }
    if (completer.has_value()) {
      completer->Reply(fit::ok());
    }
  }

  bool has_delayed_deconfigure() {
    std::lock_guard<std::mutex> guard(lock_);
    return delayed_deconfigure_completer_.has_value();
  }
  void set_hold_deconfigure(bool hold) {
    std::lock_guard<std::mutex> guard(lock_);
    hold_deconfigure_ = hold;
  }

  void AllocResources(
      fidl::Request<fuchsia_hardware_usb_function::UsbFunction::AllocResources>& request,
      fidl::internal::NaturalCompleter<
          fuchsia_hardware_usb_function::UsbFunction::AllocResources>::Sync& completer) override {
    fuchsia_hardware_usb_function::UsbFunctionAllocResourcesResponse response;
    ASSERT_EQ(request.endpoints().size(), 3u);
    ASSERT_EQ(request.interface_count(), 2u);
    ASSERT_EQ(request.strings().size(), 1u);
    response.interface_nums() = {kCommInterface, kDataInterface};
    response.endpoint_addrs() = {kIntrEp, kBulkInEp, kBulkOutEp};
    response.string_indices() = {1};
    for (size_t i = 0; i < 3; i++) {
      fidl::ServerEnd ep = std::move(request.endpoints()[i].endpoint());
      fake_endpoint(response.endpoint_addrs()[i]).Connect(dispatcher(), std::move(ep));
    }
    completer.Reply(fit::ok(std::move(response)));
  }

  fidl::ClientEnd<fuchsia_hardware_usb_function::UsbFunctionInterface> TakeInterface() {
    std::lock_guard<std::mutex> guard(lock_);
    return std::move(interface_);
  }

  void DisableEndpoint(
      fidl::Request<fuchsia_hardware_usb_function::UsbFunction::DisableEndpoint>& request,
      fidl::internal::NaturalCompleter<
          fuchsia_hardware_usb_function::UsbFunction::DisableEndpoint>::Sync& completer) override {
    std::optional<zx_status_t> error;
    bool verify = false;
    {
      std::lock_guard<std::mutex> guard(lock_);
      error = disable_endpoint_error_;
      verify = verify_lifecycle_order_;
    }

    if (error.has_value()) {
      completer.Reply(fit::error(error.value()));
      return;
    }

    uint8_t ep_addr = request.endpoint_address();
    auto& fake_ep = fake_endpoint(ep_addr);

    if (verify) {
      // Verify lifecycle ordering.
      EXPECT_FALSE(fake_ep.has_pending_requests())
          << "DisableEndpoint called while requests are still pending on endpoint!";
      EXPECT_FALSE(fake_ep.has_delayed_cancel())
          << "DisableEndpoint called while CancelAll is still held!";
    }

    fake_ep.DisableAndCancelAll();
    completer.Reply(fit::ok());
  }

  void set_verify_lifecycle_order(bool verify) {
    std::lock_guard<std::mutex> guard(lock_);
    verify_lifecycle_order_ = verify;
  }
  void set_disable_endpoint_error(std::optional<zx_status_t> error) {
    std::lock_guard<std::mutex> guard(lock_);
    disable_endpoint_error_ = error;
  }

  void set_on_configure(fit::callback<void()> callback) {
    std::lock_guard<std::mutex> guard(lock_);
    on_configure_ = std::move(callback);
  }

 private:
  std::mutex lock_;
  fidl::ClientEnd<fuchsia_hardware_usb_function::UsbFunctionInterface> interface_;
  fit::callback<void()> on_configure_;
  bool verify_lifecycle_order_ = false;
  bool hold_deconfigure_ = false;
  std::optional<fidl::internal::NaturalCompleter<
      fuchsia_hardware_usb_function::UsbFunction::Deconfigure>::Async>
      delayed_deconfigure_completer_;
  std::optional<zx_status_t> disable_endpoint_error_;
};

class Environment : public fdf_testing::Environment {
 public:
  void CloseUsbFunctionServer() { usb_function_bindings_.CloseAll(ZX_ERR_PEER_CLOSED); }

  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    async_dispatcher_t* dispatcher = fdf::Dispatcher::GetCurrent()->async_dispatcher();

    device_server_.Initialize("default", std::nullopt);
    zx_status_t status = device_server_.Serve(dispatcher, &to_driver_vfs);
    if (status != ZX_OK) {
      return zx::error(status);
    }

    fuchsia_hardware_usb_function::UsbFunctionService::InstanceHandler handler({
        .device = usb_function_bindings_.CreateHandler(&fake_usb_fidl_, dispatcher,
                                                       fidl::kIgnoreBindingClosure),
    });

    if (mac_address_.has_value()) {
      if (zx::result result =
              metadata_server_.Serve(to_driver_vfs, dispatcher, mac_address_.value());
          result.is_error()) {
        return result.take_error();
      }
    }

    if (zx::result result =
            to_driver_vfs.AddService<fuchsia_hardware_usb_function::UsbFunctionService>(
                std::move(handler));
        result.is_error()) {
      return result.take_error();
    }

    return zx::ok();
  }

  using FakeUsbFidl = FakeUsbFunction;

  compat::DeviceServer device_server_;
  FakeUsbFunction fake_usb_fidl_{fdf::Dispatcher::GetCurrent()->async_dispatcher()};
  fidl::ServerBindingGroup<fuchsia_hardware_usb_function::UsbFunction> usb_function_bindings_;
  fdf_metadata::MetadataServer<fuchsia_boot_metadata::MacAddressMetadata> metadata_server_;
  FakeNetworkDeviceIfc fake_ifc_;
  std::optional<fuchsia_boot_metadata::MacAddressMetadata> mac_address_;
};

class UsbCdcTestConfig final {
 public:
  using DriverType = UsbCdcFunction;
  using EnvironmentType = Environment;
};

class UsbCdcTest : public ::testing::Test {
 public:
  static constexpr fdf_arena_tag_t kArenaTag = 'TEST';
  static constexpr std::array<uint8_t, 6> kTestMac = {0, 1, 2, 3, 4, 5};

  virtual void ConfigureMetadata(Environment& env) {
    env.mac_address_ =
        fuchsia_boot_metadata::MacAddressMetadata{{.mac_address = {{{.octets = kTestMac}}}}};
  }

  void SetUp() override {
    auto endpoints = fdf::CreateEndpoints<fnetdev::NetworkDeviceIfc>();
    auto port_ready = std::make_shared<libsync::Completion>();
    auto function_configured = std::make_shared<libsync::Completion>();
    driver_test_.RunInEnvironmentTypeContext([this, server = std::move(endpoints->server),
                                              port_ready,
                                              function_configured](Environment& env) mutable {
      ConfigureMetadata(env);
      fdf::BindServer(fdf::Dispatcher::GetCurrent()->get(), std::move(server), &env.fake_ifc_);
      env.fake_ifc_.set_on_add_port([port_ready]() { port_ready->Signal(); });
      env.fake_usb_fidl_.set_on_configure(
          [function_configured]() { function_configured->Signal(); });
    });

    auto start_result = driver_test_.StartDriver();
    if (start_result.is_error()) {
      driver_stopped_ = true;
    }
    if (!expect_start_success_) {
      ASSERT_TRUE(start_result.is_error());
      return;
    }
    ASSERT_OK(start_result.status_value());

    // Connect to the driver
    auto connect_result = driver_test_.Connect<fnetdev::Service::NetworkDeviceImpl>();
    ASSERT_OK(connect_result.status_value());

    fdf::Arena arena(kArenaTag);
    net_impl_client_.Bind(std::move(connect_result.value()));
    auto init_result = net_impl_client_.buffer(arena)->Init(std::move(endpoints->client));
    ASSERT_OK(init_result.status());
    ASSERT_OK(init_result->s);

    ASSERT_OK(port_ready->Wait(zx::sec(5)));
    ASSERT_OK(function_configured->Wait(zx::sec(5)));

    driver_test_.RunInEnvironmentTypeContext([this](Environment& env) {
      EXPECT_TRUE(env.fake_ifc_.HasPort());
      net_port_client_.Bind(env.fake_ifc_.TakePort());
      function_client_.Bind(env.fake_usb_fidl_.TakeInterface());
    });
    driver_test_.RunInDriverContext([](UsbCdcFunction& driver) {
      EXPECT_EQ(driver.InterruptAddress(), kIntrEp);
      EXPECT_EQ(driver.BulkInAddress(), kBulkInEp);
      EXPECT_EQ(driver.BulkOutAddress(), kBulkOutEp);
    });
  }

  void TearDown() override {
    driver_test_.RunInEnvironmentTypeContext([](Environment& env) {
      env.fake_ifc_.set_on_complete_rx(nullptr);
      env.fake_ifc_.set_on_complete_tx(nullptr);

      // Deterministically reset persistent mock holds to prevent cross-test leakage!
      for (uint8_t ep : {kBulkOutEp, kIntrEp, kBulkInEp}) {
        auto& fake_ep = env.fake_usb_fidl_.fake_endpoint(ep);
        fake_ep.set_hold_cancel(false);
        fake_ep.ReleaseCancelAll();
        fake_ep.reset_cancel_all_called();
      }
      env.fake_usb_fidl_.set_hold_deconfigure(false);
      env.fake_usb_fidl_.set_disable_endpoint_error(std::nullopt);
      env.fake_usb_fidl_.set_verify_lifecycle_order(false);
      env.fake_usb_fidl_.ReleaseDelayedDeconfigure();
    });
    if (!driver_stopped_) {
      ASSERT_OK(driver_test_.StopDriver().status_value());
      driver_stopped_ = true;
    }
  }

  static constexpr uint8_t kVmoId = 1;

  void StartNetworkDevice() {
    fdf::Arena arena(kArenaTag);
    auto start_result = net_impl_client_.buffer(arena)->Start();
    ASSERT_OK(start_result.status());
    ASSERT_OK(start_result->s);
  }

  void ExecuteMockNetworkTransaction(uint32_t buffer_id, size_t length, zx_status_t expected_status,
                                     fit::function<void()> middle_action = nullptr) {
    fdf::Arena arena(kArenaTag);
    auto tx_completed = std::make_shared<libsync::Completion>();
    driver_test_.RunInEnvironmentTypeContext([&](Environment& env) {
      env.fake_ifc_.set_on_complete_tx([tx_completed]() { tx_completed->Signal(); });
    });
    auto cleanup = fit::defer([&]() {
      driver_test_.RunInEnvironmentTypeContext(
          [&](Environment& env) { env.fake_ifc_.set_on_complete_tx(nullptr); });
    });

    fnetdev::wire::BufferRegion region = {
        .vmo = kVmoId,
        .offset = 0,
        .length = length,
    };
    fnetdev::wire::TxBuffer tx_buffer{
        .id = buffer_id,
        .data = fidl::VectorView<fnetdev::wire::BufferRegion>::FromExternal(&region, 1),
    };
    ASSERT_OK(net_impl_client_.buffer(arena)
                  ->QueueTx(fidl::VectorView<fnetdev::wire::TxBuffer>::FromExternal(&tx_buffer, 1))
                  .status());

    if (middle_action) {
      EXPECT_STATUS(tx_completed->Wait(zx::time::infinite_past()), ZX_ERR_TIMED_OUT);
      middle_action();
    }

    ASSERT_OK(tx_completed->Wait(zx::sec(5)));
    driver_test_.RunInEnvironmentTypeContext([&](Environment& env) {
      std::optional complete = env.fake_ifc_.PopCompleteTx();
      ASSERT_TRUE(complete.has_value());
      EXPECT_EQ(complete->id, buffer_id);
      EXPECT_EQ(complete->status, expected_status);
    });
  }

  void SetConfiguredAndEnable() {
    ASSERT_TRUE(function_client_.is_valid());
    {
      fidl::Result result = function_client_->SetConfigured({{
          .configured = true,
          .speed = fuchsia_hardware_usb_descriptor::UsbSpeed::kHigh,
      }});
      ASSERT_TRUE(result.is_ok()) << result.error_value().FormatDescription();
    }
    {
      fidl::Result result = function_client_->SetInterface({{
          .interface = kDataInterface,
          .alt_setting = 1,
      }});
      ASSERT_TRUE(result.is_ok()) << result.error_value().FormatDescription();
    }
  }

 protected:
  fdf_testing::BackgroundDriverTest<UsbCdcTestConfig> driver_test_;
  fdf::WireSyncClient<fnetdev::NetworkDeviceImpl> net_impl_client_;
  fdf::WireSyncClient<fnetdev::NetworkPort> net_port_client_;
  fidl::SyncClient<fuchsia_hardware_usb_function::UsbFunctionInterface> function_client_;
  bool driver_stopped_ = false;
  bool expect_start_success_ = true;
};

// Validates that GetInfo returns the expected TX and RX buffer queue depths for the driver.
TEST_F(UsbCdcTest, GetInfo) {
  fdf::Arena arena(kArenaTag);
  auto result = net_impl_client_.buffer(arena)->GetInfo();
  ASSERT_OK(result.status());
  EXPECT_EQ(result->info.tx_depth(), UsbCdcFunction::kTxDepth);
  EXPECT_EQ(result->info.rx_depth(), UsbCdcFunction::kRxDepth);
}

// Validates that GetMac retrieves the provisioned Ethernet MAC address from boot metadata.
TEST_F(UsbCdcTest, GetMac) {
  StartNetworkDevice();

  fdf::Arena arena(kArenaTag);
  auto result = net_port_client_.buffer(arena)->GetMac();
  ASSERT_OK(result.status());
  ASSERT_TRUE(result->mac_ifc.is_valid());

  fdf::WireSyncClient<fnetdev::MacAddr> mac_client;
  mac_client.Bind(std::move(result->mac_ifc));

  auto mac_result = mac_client.buffer(arena)->GetAddress();
  ASSERT_OK(mac_result.status());

  // TODO(https://fxbug.dev/476474119): The driver is currently flipping MAC
  // addresses, we should always use the local mac address provided by the
  // metadata and offer the other one to the host.
  std::array<uint8_t, 6> expected_mac = kTestMac;
  expected_mac[5] ^= 0x01;
  EXPECT_THAT(mac_result->mac.octets, testing::ElementsAreArray(expected_mac));
}

// Validates that transmit packet requests are immediately rejected when the driver is offline.
TEST_F(UsbCdcTest, TxFailsIfOffline) {
  StartNetworkDevice();

  constexpr uint32_t kBufferId = 100;

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(4096, 0, &vmo));
  uint8_t data[] = {0xAA, 0xBB, 0xCC, 0xDD};
  ASSERT_OK(vmo.write(data, 0, sizeof(data)));
  fdf::Arena arena(kArenaTag);
  auto prepare_result = net_impl_client_.buffer(arena)->PrepareVmo(kVmoId, std::move(vmo));
  ASSERT_OK(prepare_result.status());
  ASSERT_OK(prepare_result->s);

  ASSERT_NO_FATAL_FAILURE(ExecuteMockNetworkTransaction(kBufferId, sizeof(data), ZX_ERR_BAD_STATE));
}

// Validates end-to-end transmit data packet transfer from network device to the USB bulk IN
// endpoint.
TEST_F(UsbCdcTest, QueueTx) {
  StartNetworkDevice();
  ASSERT_NO_FATAL_FAILURE(SetConfiguredAndEnable());

  constexpr uint32_t kBufferId = 100;

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(4096, 0, &vmo));
  uint8_t data[] = {0xAA, 0xBB, 0xCC, 0xDD};
  ASSERT_OK(vmo.write(data, 0, sizeof(data)));
  fdf::Arena arena(kArenaTag);
  auto prepare_result = net_impl_client_.buffer(arena)->PrepareVmo(kVmoId, std::move(vmo));
  ASSERT_OK(prepare_result.status());
  ASSERT_OK(prepare_result->s);

  auto trigger_usb = [&]() {
    driver_test_.RunInEnvironmentTypeContext([](Environment& env) {
      env.fake_usb_fidl_.fake_endpoint(kBulkInEp).RequestComplete(ZX_OK, sizeof(data));
    });
  };
  ASSERT_NO_FATAL_FAILURE(
      ExecuteMockNetworkTransaction(kBufferId, sizeof(data), ZX_OK, trigger_usb));
}

// Validates end-to-end receive data packet transfer from the USB bulk OUT endpoint to network
// buffers.
TEST_F(UsbCdcTest, Rx) {
  StartNetworkDevice();
  ASSERT_NO_FATAL_FAILURE(SetConfiguredAndEnable());

  auto rx_completed = std::make_shared<libsync::Completion>();
  driver_test_.RunInEnvironmentTypeContext([rx_completed](Environment& env) {
    env.fake_ifc_.set_on_complete_rx([rx_completed]() { rx_completed->Signal(); });
  });

  constexpr size_t kDataSize1 = 54;
  constexpr size_t kDataSize2 = 250;
  constexpr uint8_t kBufferId1 = 201;
  constexpr uint8_t kBufferId2 = 202;

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(4096, 0, &vmo));
  fdf::Arena arena(kArenaTag);
  auto prepare_result = net_impl_client_.buffer(arena)->PrepareVmo(kVmoId, std::move(vmo));
  ASSERT_OK(prepare_result.status());
  ASSERT_OK(prepare_result->s);

  {
    fnetdev::wire::RxSpaceBuffer buffer = {
        .id = kBufferId1,
        .region = {.vmo = kVmoId, .offset = 0, .length = 2048},
    };

    ASSERT_OK(
        net_impl_client_.buffer(arena)
            ->QueueRxSpace(fidl::VectorView<fnetdev::wire::RxSpaceBuffer>::FromExternal(&buffer, 1))
            .status());
  }

  EXPECT_STATUS(rx_completed->Wait(zx::time::infinite_past()), ZX_ERR_TIMED_OUT);

  // Complete 2 requests, but we only have one rx space already queued.
  // The pending request can be completed later.
  driver_test_.RunInEnvironmentTypeContext([](Environment& env) {
    env.fake_usb_fidl_.fake_endpoint(kBulkOutEp).RequestComplete(ZX_OK, kDataSize1);
    env.fake_usb_fidl_.fake_endpoint(kBulkOutEp).RequestComplete(ZX_OK, kDataSize2);
  });

  ASSERT_OK(rx_completed->Wait(zx::sec(5)));

  driver_test_.RunInEnvironmentTypeContext([&](Environment& env) {
    auto buffer = env.fake_ifc_.PopCompleteRx();
    ASSERT_TRUE(buffer.has_value());
    EXPECT_EQ(buffer->meta().port(), UsbCdcFunction::kPortId);
    EXPECT_EQ(buffer->meta().frame_type(), fuchsia_hardware_network::wire::FrameType::kEthernet);
    ASSERT_EQ(buffer->data().size(), 1u);
    auto& data = buffer->data()[0];
    EXPECT_EQ(data.offset(), 0u);
    EXPECT_EQ(data.length(), kDataSize1);
    EXPECT_EQ(data.id(), kBufferId1);
  });

  rx_completed->Reset();

  {
    fnetdev::wire::RxSpaceBuffer buffer = {
        .id = kBufferId2,
        .region = {.vmo = kVmoId, .offset = 0, .length = 2048},
    };

    ASSERT_OK(
        net_impl_client_.buffer(arena)
            ->QueueRxSpace(fidl::VectorView<fnetdev::wire::RxSpaceBuffer>::FromExternal(&buffer, 1))
            .status());
  }

  ASSERT_OK(rx_completed->Wait(zx::sec(5)));

  driver_test_.RunInEnvironmentTypeContext([&](Environment& env) {
    auto buffer = env.fake_ifc_.PopCompleteRx();
    ASSERT_TRUE(buffer.has_value());
    EXPECT_EQ(buffer->meta().port(), UsbCdcFunction::kPortId);
    EXPECT_EQ(buffer->meta().frame_type(), fuchsia_hardware_network::wire::FrameType::kEthernet);
    ASSERT_EQ(buffer->data().size(), 1u);
    auto& data = buffer->data()[0];
    EXPECT_EQ(data.offset(), 0u);
    EXPECT_EQ(data.length(), kDataSize2);
    EXPECT_EQ(data.id(), kBufferId2);
  });
}

// Validates that stopping the network device drains and cancels all active in-flight transactions.
TEST_F(UsbCdcTest, Stop) {
  StartNetworkDevice();
  ASSERT_NO_FATAL_FAILURE(SetConfiguredAndEnable());

  constexpr uint8_t kRxBufferId = 2;
  constexpr uint8_t kTxBufferId = 3;

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(4096, 0, &vmo));
  fdf::Arena arena(kArenaTag);
  auto prepare_result = net_impl_client_.buffer(arena)->PrepareVmo(kVmoId, std::move(vmo));
  ASSERT_OK(prepare_result.status());
  ASSERT_OK(prepare_result->s);

  fnetdev::wire::RxSpaceBuffer rx_buffer = {
      .id = kRxBufferId,
      .region = {.vmo = kVmoId, .offset = 0, .length = 2048},
  };

  ASSERT_OK(net_impl_client_.buffer(arena)
                ->QueueRxSpace(
                    fidl::VectorView<fnetdev::wire::RxSpaceBuffer>::FromExternal(&rx_buffer, 1))
                .status());

  auto rx_completed = std::make_shared<libsync::Completion>();
  driver_test_.RunInEnvironmentTypeContext([rx_completed](Environment& env) {
    env.fake_ifc_.set_on_complete_rx([rx_completed]() { rx_completed->Signal(); });
  });

  auto trigger_stop = [&]() {
    auto result = net_impl_client_.buffer(arena)->Stop();
    ASSERT_OK(result.status());
  };
  ASSERT_NO_FATAL_FAILURE(
      ExecuteMockNetworkTransaction(kTxBufferId, 2048, ZX_ERR_CANCELED, trigger_stop));

  ASSERT_OK(rx_completed->Wait(zx::sec(5)));

  // All in flight transactions should complete on stop.
  driver_test_.RunInEnvironmentTypeContext([&](Environment& env) {
    auto rx = env.fake_ifc_.PopCompleteRx();
    ASSERT_TRUE(rx.has_value());
    ASSERT_EQ(rx->data().size(), 1u);
    EXPECT_EQ(rx->data()[0].id(), kRxBufferId);
  });
}

// Validates that Inspect tracks online state, throughput metrics, and TX/RX byte counters.
TEST_F(UsbCdcTest, Inspect) {
  StartNetworkDevice();
  ASSERT_NO_FATAL_FAILURE(SetConfiguredAndEnable());

  constexpr uint32_t kTxBufferId = 100;
  constexpr uint8_t kRxBufferId = 201;
  constexpr size_t kTxDataSize = 4;
  constexpr size_t kRxDataSize = 54;

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(4096, 0, &vmo));
  uint8_t tx_data[] = {0xAA, 0xBB, 0xCC, 0xDD};
  ASSERT_OK(vmo.write(tx_data, 0, sizeof(tx_data)));

  fdf::Arena arena(kArenaTag);
  auto prepare_result = net_impl_client_.buffer(arena)->PrepareVmo(kVmoId, std::move(vmo));
  ASSERT_OK(prepare_result.status());
  ASSERT_OK(prepare_result->s);

  // 1. Queue TX
  fnetdev::wire::BufferRegion tx_region = {.vmo = kVmoId, .offset = 0, .length = kTxDataSize};
  fnetdev::wire::TxBuffer tx_buffer = {
      .id = kTxBufferId,
      .data = fidl::VectorView<fnetdev::wire::BufferRegion>::FromExternal(&tx_region, 1),
  };

  auto tx_completed = std::make_shared<libsync::Completion>();
  driver_test_.RunInEnvironmentTypeContext([tx_completed](Environment& env) {
    env.fake_ifc_.set_on_complete_tx([tx_completed]() { tx_completed->Signal(); });
  });
  ASSERT_OK(net_impl_client_.buffer(arena)
                ->QueueTx(fidl::VectorView<fnetdev::wire::TxBuffer>::FromExternal(&tx_buffer, 1))
                .status());

  driver_test_.RunInEnvironmentTypeContext([](Environment& env) {
    env.fake_usb_fidl_.fake_endpoint(kBulkInEp).RequestComplete(ZX_OK, kTxDataSize);
  });
  ASSERT_OK(tx_completed->Wait(zx::sec(5)));

  // 2. Queue RX Space and Receive
  auto rx_completed = std::make_shared<libsync::Completion>();
  driver_test_.RunInEnvironmentTypeContext([rx_completed](Environment& env) {
    env.fake_ifc_.set_on_complete_rx([rx_completed]() { rx_completed->Signal(); });
  });

  fnetdev::wire::RxSpaceBuffer rx_space = {
      .id = kRxBufferId,
      .region = {.vmo = kVmoId, .offset = 1024, .length = 2048},
  };
  ASSERT_OK(
      net_impl_client_.buffer(arena)
          ->QueueRxSpace(fidl::VectorView<fnetdev::wire::RxSpaceBuffer>::FromExternal(&rx_space, 1))
          .status());

  driver_test_.RunInEnvironmentTypeContext([](Environment& env) {
    env.fake_usb_fidl_.fake_endpoint(kBulkOutEp).RequestComplete(ZX_OK, kRxDataSize);
  });
  ASSERT_OK(rx_completed->Wait(zx::sec(5)));

  // 3. Trigger throughput and verify
  driver_test_.RunInDriverContext([kTxDataSize, kRxDataSize](UsbCdcFunction& driver) {
    driver.GetThroughputTrackerForTesting().MeasureForTesting(zx::sec(1));

    auto hierarchy = usb_inspect::ReadHierarchyFromInspector(driver.inspector().inspector());

    auto* cdc_node = hierarchy.GetByPath({"usb-cdc-function"});
    ASSERT_TRUE(cdc_node != nullptr);

    const auto* online_prop = cdc_node->node().get_property<inspect::BoolPropertyValue>("online");
    ASSERT_TRUE(online_prop != nullptr);
    EXPECT_TRUE(online_prop->value());

    auto* bulk_in = hierarchy.GetByPath({"usb-cdc-function", "bulk_in"});

    ASSERT_TRUE(bulk_in != nullptr);
    auto err_in = usb_inspect::VerifyEndpointInspect(bulk_in, kTxDataSize, std::nullopt, 0,
                                                     std::nullopt, kTxDataSize);
    EXPECT_TRUE(err_in.is_ok()) << err_in.error_value();

    auto* bulk_out = hierarchy.GetByPath({"usb-cdc-function", "bulk_out"});
    ASSERT_TRUE(bulk_out != nullptr);
    auto err_out = usb_inspect::VerifyEndpointInspect(
        bulk_out, std::nullopt, kRxDataSize, std::nullopt, UsbCdcFunction::kRxDepth, kRxDataSize);
    EXPECT_TRUE(err_out.is_ok()) << err_out.error_value();
  });
}

// Validates CDC control requests (packet filters, clear halt) and interrupt status notifications.
TEST_F(UsbCdcTest, ControlAndNotifications) {
  ASSERT_NO_FATAL_FAILURE(SetConfiguredAndEnable());

  // 1. Send Class-Interface request to set ethernet packet filter (acknowledged/ZX_OK)
  {
    fuchsia_hardware_usb_descriptor::UsbSetup setup{{
        .bm_request_type = USB_DIR_OUT | USB_TYPE_CLASS | USB_RECIP_INTERFACE,
        .b_request = USB_CDC_SET_ETHERNET_PACKET_FILTER,
        .w_value = 0,
        .w_index = 0,
        .w_length = 0,
    }};
    auto result = function_client_->Control({setup, {}});
    ASSERT_TRUE(result.is_ok());
  }

  // 2. Send Standard-Endpoint request to clear halt feature (acknowledged/ZX_OK)
  {
    fuchsia_hardware_usb_descriptor::UsbSetup setup{{
        .bm_request_type = USB_DIR_OUT | USB_TYPE_STANDARD | USB_RECIP_ENDPOINT,
        .b_request = USB_REQ_CLEAR_FEATURE,
        .w_value = USB_ENDPOINT_HALT,
        .w_index = 0,
        .w_length = 0,
    }};
    auto result = function_client_->Control({setup, {}});
    ASSERT_TRUE(result.is_ok());
  }

  // 3. Send unsupported/invalid request (fails with ZX_ERR_NOT_SUPPORTED)
  {
    fuchsia_hardware_usb_descriptor::UsbSetup setup{{
        .bm_request_type = USB_DIR_OUT | USB_TYPE_STANDARD | USB_RECIP_DEVICE,
        .b_request = USB_REQ_SET_ADDRESS,
        .w_value = 5,
        .w_index = 0,
        .w_length = 0,
    }};
    auto result = function_client_->Control({setup, {}});
    ASSERT_TRUE(result.is_error());
    ASSERT_TRUE(result.error_value().is_domain_error());
    ASSERT_EQ(result.error_value().domain_error(), ZX_ERR_NOT_SUPPORTED);
  }

  // 4. Verify Interrupt Notifications are correctly queued on kIntrEp.
  // There should be exactly 4 notifications:
  // - 2 during SetConfigured(true) in SetUp() (offline, speed = 0).
  // - 2 during EnablePort(true) in SetUp() (online, speed = 100M/1G).
  ASSERT_TRUE(driver_test_.runtime().RunWithTimeoutOrUntil(
      [&]() {
        size_t pending = 0;
        driver_test_.RunInEnvironmentTypeContext([&](Environment& env) {
          pending = env.fake_usb_fidl_.fake_endpoint(kIntrEp).pending_request_count();
        });
        return pending == 4u;
      },
      zx::sec(5)));

  driver_test_.RunInEnvironmentTypeContext([&](Environment& env) {
    auto& fake_ep = env.fake_usb_fidl_.fake_endpoint(kIntrEp);

    // Notification 1: Network Connection (Offline)
    {
      auto data_res = fake_ep.ReadPendingRequestData();
      ASSERT_OK(data_res.status_value());
      ASSERT_GE(data_res->size(), sizeof(usb_cdc_notification_t));
      usb_cdc_notification_t notif;
      std::memcpy(&notif, data_res->data(), sizeof(notif));
      EXPECT_EQ(notif.bmRequestType, USB_DIR_IN | USB_TYPE_CLASS | USB_RECIP_INTERFACE);
      EXPECT_EQ(notif.bNotification, USB_CDC_NC_NETWORK_CONNECTION);
      EXPECT_EQ(le16toh(notif.wValue), 0);  // Offline
      EXPECT_EQ(le16toh(notif.wLength), 0);
      fake_ep.RequestComplete(ZX_OK, sizeof(usb_cdc_notification_t));
    }

    // Notification 2: Speed Change (Offline / 0 bps)
    {
      auto data_res = fake_ep.ReadPendingRequestData();
      ASSERT_OK(data_res.status_value());
      ASSERT_GE(data_res->size(), sizeof(usb_cdc_speed_change_notification_t));
      usb_cdc_speed_change_notification_t notif;
      std::memcpy(&notif, data_res->data(), sizeof(notif));
      EXPECT_EQ(notif.notification.bmRequestType,
                USB_DIR_IN | USB_TYPE_CLASS | USB_RECIP_INTERFACE);
      EXPECT_EQ(notif.notification.bNotification, USB_CDC_NC_CONNECTION_SPEED_CHANGE);
      EXPECT_EQ(le16toh(notif.notification.wLength), 2 * sizeof(uint32_t));
      EXPECT_EQ(le32toh(notif.downlink_br), 0u);
      EXPECT_EQ(le32toh(notif.uplink_br), 0u);
      fake_ep.RequestComplete(ZX_OK, sizeof(usb_cdc_speed_change_notification_t));
    }

    // Notification 3: Network Connection (Online)
    {
      auto data_res = fake_ep.ReadPendingRequestData();
      ASSERT_OK(data_res.status_value());
      ASSERT_GE(data_res->size(), sizeof(usb_cdc_notification_t));
      usb_cdc_notification_t notif;
      std::memcpy(&notif, data_res->data(), sizeof(notif));
      EXPECT_EQ(notif.bmRequestType, USB_DIR_IN | USB_TYPE_CLASS | USB_RECIP_INTERFACE);
      EXPECT_EQ(notif.bNotification, USB_CDC_NC_NETWORK_CONNECTION);
      EXPECT_EQ(le16toh(notif.wValue), 1);  // Online
      EXPECT_EQ(le16toh(notif.wLength), 0);
      fake_ep.RequestComplete(ZX_OK, sizeof(usb_cdc_notification_t));
    }

    // Notification 4: Speed Change (Online / 100 Mbps)
    {
      auto data_res = fake_ep.ReadPendingRequestData();
      ASSERT_OK(data_res.status_value());
      ASSERT_GE(data_res->size(), sizeof(usb_cdc_speed_change_notification_t));
      usb_cdc_speed_change_notification_t notif;
      std::memcpy(&notif, data_res->data(), sizeof(notif));
      EXPECT_EQ(notif.notification.bmRequestType,
                USB_DIR_IN | USB_TYPE_CLASS | USB_RECIP_INTERFACE);
      EXPECT_EQ(notif.notification.bNotification, USB_CDC_NC_CONNECTION_SPEED_CHANGE);
      EXPECT_EQ(le16toh(notif.notification.wLength), 2 * sizeof(uint32_t));
      EXPECT_EQ(le32toh(notif.downlink_br), 100 * 1000 * 1000u);
      EXPECT_EQ(le32toh(notif.uplink_br), 100 * 1000 * 1000u);
      fake_ep.RequestComplete(ZX_OK, sizeof(usb_cdc_speed_change_notification_t));
    }
  });

  // Let the driver process all 4 completions and return requests to its pool.
  ASSERT_TRUE(driver_test_.runtime().RunWithTimeoutOrUntil(
      [&]() {
        bool full = false;
        driver_test_.RunInDriverContext(
            [&](UsbCdcFunction& driver) { full = driver.IntrEpRequestsFull(); });
        return full;
      },
      zx::sec(5)));
}

class UsbCdcNoMetadataTest : public UsbCdcTest {
 protected:
  void ConfigureMetadata(Environment& env) override {
    // Do nothing to simulate metadata not found!
  }
};

// Validates that the driver falls back to generating a random locally-administered MAC address
// when boot metadata is missing.
TEST_F(UsbCdcNoMetadataTest, FallbackToRandomMacAddress) {
  // The driver should successfully start and generate a random MAC address.
  // We can verify that the generated MAC address has the local-assignment bit (0x02) set!
  driver_test_.RunInDriverContext([](UsbCdcFunction& driver) {
    std::array<uint8_t, 6> mac = driver.mac_addr();
    EXPECT_EQ(mac[0] & 0x02, 0x02);
  });
}

class UsbCdcInvalidMetadataTest : public UsbCdcTest {
 protected:
  void ConfigureMetadata(Environment& env) override {
    expect_start_success_ = false;
    // Set invalid metadata that is missing the mac_address field.
    fuchsia_boot_metadata::MacAddressMetadata metadata;
    env.mac_address_ = metadata;
  }
};

// Validates that driver startup fails cleanly when corrupted or incomplete MAC metadata is
// provided.
TEST_F(UsbCdcInvalidMetadataTest, FailsToStart) {
  // Driver fails to start in SetUp() because expect_start_success_ is false and
  // StartDriver() returns a failure, which is verified by SetUp().
}

}  // namespace
}  // namespace usb_cdc_function
