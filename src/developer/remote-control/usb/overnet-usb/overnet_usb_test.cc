// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "overnet_usb.h"

#include <lib/ddk/metadata.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/inspect/testing/cpp/inspect.h>
#include <lib/sync/completion.h>
#include <zircon/compiler.h>

#include <algorithm>
#include <cstdint>
#include <memory>
#include <optional>
#include <string_view>
#include <utility>
#include <vector>

#include <gtest/gtest.h>
#include <usb-inspect/usb-inspect-test-helper.h>

#include "fbl/auto_lock.h"
#include "fbl/mutex.h"
#include "fidl/fuchsia.hardware.overnet/cpp/markers.h"
#include "fidl/fuchsia.hardware.usb.function/cpp/fidl.h"
#include "lib/driver/compat/cpp/device_server.h"
#include "lib/fidl/cpp/wire/channel.h"
#include "lib/fidl/cpp/wire/internal/transport_channel.h"
#include "src/devices/usb/lib/usb-endpoint/testing/fake-usb-endpoint-server.h"

// NOLINTBEGIN(misc-use-anonymous-namespace)
// NOLINTBEGIN(readability-convert-member-functions-to-static)
// NOLINTBEGIN(readability-container-data-pointer)

static constexpr uint8_t kBulkOutEndpoint = 1;
static constexpr uint8_t kBulkInEndpoint = 2;
static constexpr uint8_t kInterfaceNum = 1;

// A fake endpoint that allows for more complex behaviour in responding to completion requests
// by requiring that there be outstanding requests when you attempt to fulfill them.
class FakeEndpoint : public fake_usb_endpoint::FakeEndpoint {
 public:
  void Connect(async_dispatcher_t* dispatcher,
               fidl::ServerEnd<fuchsia_hardware_usb_endpoint::Endpoint> server) override {
    binding_ref_.emplace(fidl::BindServer(dispatcher, std::move(server), this));
  }

  // QueueRequests: adds requests to a queue, which will be replied to when RequestComplete() is
  // called.
  void QueueRequests(QueueRequestsRequest& request,
                     QueueRequestsCompleter::Sync& completer) override {
    FDF_LOG(DEBUG, "QueueRequests");
    fbl::AutoLock _(&lock_);
    // Add request to queue.
    requests_.insert(requests_.end(), std::make_move_iterator(request.req().begin()),
                     std::make_move_iterator(request.req().end()));
  }

  void CancelAll(CancelAllCompleter::Sync& completer) override {
    fbl::AutoLock _(&lock_);
    for (auto& request : requests_) {
      SendRequestComplete(fuchsia_hardware_usb_request::Request(std::move(request)),
                          ZX_ERR_IO_NOT_PRESENT, 0);
    }
    requests_.erase(requests_.begin(), requests_.end());
    completer.Reply(fit::ok());
  }

  // Returns the next waiting request. The caller is responsible for ensuring that
  // a request is waiting in the queue.
  fuchsia_hardware_usb_request::Request GetNextRequest() {
    fbl::AutoLock _(&lock_);
    EXPECT_GT(requests_.size(), 0u);
    auto next_request = fuchsia_hardware_usb_request::Request(std::move(requests_.front()));
    requests_.erase(requests_.begin());
    return next_request;
  }

  void SendRequestComplete(fuchsia_hardware_usb_request::Request request, zx_status_t status,
                           size_t actual) {
    auto completion = std::move(fuchsia_hardware_usb_endpoint::Completion()
                                    .request(std::move(request))
                                    .status(status)
                                    .transfer_size(actual));

    ASSERT_TRUE(binding_ref_);
    std::vector<fuchsia_hardware_usb_endpoint::Completion> completions;
    completions.emplace_back(std::move(completion));
    EXPECT_TRUE(fidl::SendEvent(*binding_ref_)->OnCompletion(std::move(completions)).is_ok());
  }

  // RegisterVmos: stores the vmo mapping
  void RegisterVmos(RegisterVmosRequest& request, RegisterVmosCompleter::Sync& completer) override {
    fbl::AutoLock lock(&lock_);
    std::vector<fuchsia_hardware_usb_endpoint::VmoHandle> ret;
    for (const auto& vmo_id : request.vmo_ids()) {
      zx::vmo vmo;
      auto status = zx::vmo::create(*vmo_id.size(), 0, &vmo);
      if (status != ZX_OK) {
        continue;
      }
      zx::vmo dup_vmo;
      EXPECT_EQ(ZX_OK, vmo.duplicate(ZX_RIGHT_SAME_RIGHTS, &dup_vmo));
      vmos_.emplace(*vmo_id.id(), std::move(dup_vmo));
      ret.emplace_back(std::move(
          fuchsia_hardware_usb_endpoint::VmoHandle().id(*vmo_id.id()).vmo(std::move(vmo))));
    }
    completer.Reply(std::move(ret));
  }
  // UnregisterVmos: stores the vmo mapping
  void UnregisterVmos(UnregisterVmosRequest& request,
                      UnregisterVmosCompleter::Sync& completer) override {
    fbl::AutoLock lock(&lock_);
    for (const auto& vmo_id : request.vmo_ids()) {
      vmos_.erase(vmo_id);
    }
    completer.Reply({{}, {}});
  }

  void WithVmo(uint64_t vmo_id, std::function<void(zx::vmo&)> cb) {
    fbl::AutoLock lock(&lock_);
    auto vmo = vmos_.find(vmo_id);
    EXPECT_NE(vmo, vmos_.end());
    cb(vmo->second);
  }

  size_t pending_request_count() {
    fbl::AutoLock _(&lock_);
    return requests_.size();
  }

 private:
  std::optional<fidl::ServerBindingRef<fuchsia_hardware_usb_endpoint::Endpoint>> binding_ref_;

  fbl::Mutex lock_;
  std::vector<fuchsia_hardware_usb_request::Request> requests_ __TA_GUARDED(lock_);
  std::unordered_map<uint64_t, zx::vmo> vmos_ __TA_GUARDED(lock_);
};

class TestCallback : public fidl::WireServer<fuchsia_hardware_overnet::Callback> {
 public:
  TestCallback(size_t expected_calls, std::function<void(zx::socket)> callback)
      : expected_calls_(expected_calls), callback_(std::move(callback)) {}
  ~TestCallback() {
    FDF_LOG(DEBUG, "Destroying TestCallback %zu==%zu", expected_calls_, actual_calls_);
    EXPECT_EQ(expected_calls_, actual_calls_);
  }
  void NewLink(::fuchsia_hardware_overnet::wire::CallbackNewLinkRequest* request,
               NewLinkCompleter::Sync& completer) override {
    actual_calls_++;
    FDF_LOG(DEBUG, "calling callback %zu", actual_calls_);
    callback_(std::move(request->socket));
    completer.Reply();
  }

 private:
  size_t expected_calls_;
  size_t actual_calls_ = 0;
  std::function<void(zx::socket)> callback_;

  DISALLOW_COPY_AND_ASSIGN_ALLOW_MOVE(TestCallback);
};

class FakeUsb
    : public fake_usb_endpoint::FakeUsbFidlProvider<fuchsia_hardware_usb_function::UsbFunction,
                                                    FakeEndpoint> {
 public:
  using Base = fake_usb_endpoint::FakeUsbFidlProvider<fuchsia_hardware_usb_function::UsbFunction,
                                                      FakeEndpoint>;
  using Base::Base;

  ~FakeUsb() {
    EXPECT_EQ(expect_configure_ep_.size(), 0u);
    EXPECT_EQ(expect_disable_ep_.size(), 0u);
  }

  void Configure(
      fidl::Request<fuchsia_hardware_usb_function::UsbFunction::Configure>& request,
      fidl::internal::NaturalCompleter<fuchsia_hardware_usb_function::UsbFunction::Configure>::Sync&
          completer) override {
    interface_ = std::move(request.iface());
    completer.Reply(fit::ok());
  }

  void AllocResources(
      fidl::Request<fuchsia_hardware_usb_function::UsbFunction::AllocResources>& request,
      fidl::internal::NaturalCompleter<
          fuchsia_hardware_usb_function::UsbFunction::AllocResources>::Sync& completer) override {
    fuchsia_hardware_usb_function::UsbFunctionAllocResourcesResponse response;
    ASSERT_EQ(request.endpoints().size(), 2u);
    ASSERT_EQ(request.interface_count(), 1u);
    ASSERT_EQ(request.strings().size(), 1u);
    response.interface_nums() = {kInterfaceNum};
    response.endpoint_addrs() = {kBulkOutEndpoint, kBulkInEndpoint};
    response.string_indices() = {1};
    for (size_t i = 0; i < 2; i++) {
      fidl::ServerEnd ep = std::move(request.endpoints()[i].endpoint());
      fake_endpoint(response.endpoint_addrs()[i]).Connect(dispatcher(), std::move(ep));
    }
    completer.Reply(fit::ok(std::move(response)));
  }

  void ConfigureEndpoint(
      fidl::Request<fuchsia_hardware_usb_function::UsbFunction::ConfigureEndpoint>& request,
      fidl::internal::NaturalCompleter<
          fuchsia_hardware_usb_function::UsbFunction::ConfigureEndpoint>::Sync& completer)
      override {
    completer.Reply(fit::ok());
    if (expect_configure_ep_.empty()) {
      ADD_FAILURE() << "received ConfigureEndpoint "
                    << static_cast<uint32_t>(request.endpoint_address()) << " without expectation";
      return;
    }
    EXPECT_EQ(expect_configure_ep_.front(), request.endpoint_address());
    expect_configure_ep_.pop();
  }

  void DisableEndpoint(
      fidl::Request<fuchsia_hardware_usb_function::UsbFunction::DisableEndpoint>& request,
      fidl::internal::NaturalCompleter<
          fuchsia_hardware_usb_function::UsbFunction::DisableEndpoint>::Sync& completer) override {
    completer.Reply(fit::ok());
    if (expect_disable_ep_.empty()) {
      ADD_FAILURE() << "received DisableEndpoint "
                    << static_cast<uint32_t>(request.endpoint_address()) << " without expectation";
      return;
    }
    EXPECT_EQ(expect_disable_ep_.front(), request.endpoint_address());
    expect_disable_ep_.pop();
  }

  void ExpectConfigureEndpoint(uint8_t endpoint_address) {
    expect_configure_ep_.push(endpoint_address);
  }

  void ExpectDisableEndpoint(uint8_t endpoint_address) {
    expect_disable_ep_.push(endpoint_address);
  }

  fidl::ClientEnd<fuchsia_hardware_usb_function::UsbFunctionInterface> TakeInterface() {
    return std::move(interface_);
  }

 private:
  std::queue<uint8_t> expect_configure_ep_;
  std::queue<uint8_t> expect_disable_ep_;
  fidl::ClientEnd<fuchsia_hardware_usb_function::UsbFunctionInterface> interface_;
};

class OvernetUsbEnvironment : public fdf_testing::Environment {
 public:
  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    dispatcher_ = fdf::Dispatcher::GetCurrent()->async_dispatcher();
    device_server_.Initialize("default");
    if (auto res = device_server_.Serve(dispatcher_, &to_driver_vfs); res != ZX_OK) {
      return zx::error(res);
    }
    fake_usb_ = std::make_unique<FakeUsb>(dispatcher_);
    fuchsia_hardware_usb_function::UsbFunctionService::InstanceHandler handler({
        .device = usb_function_bindings_.CreateHandler(
            fake_usb_.get(), fdf::Dispatcher::GetCurrent()->async_dispatcher(),
            fidl::kIgnoreBindingClosure),
    });
    auto result = to_driver_vfs.AddService<fuchsia_hardware_usb_function::UsbFunctionService>(
        std::move(handler));
    if (result.is_error()) {
      return result.take_error();
    }

    auto endpoints = fidl::Endpoints<fuchsia_hardware_overnet::Usb>::Create();

    return zx::ok();
  }

  async_dispatcher_t* dispatcher_;
  compat::DeviceServer device_server_;
  std::unique_ptr<FakeUsb> fake_usb_;
  fidl::ServerBindingGroup<fuchsia_hardware_usb_function::UsbFunction> usb_function_bindings_;
};

class OvernetUsbTestConfig final {
 public:
  using DriverType = OvernetUsb;
  using EnvironmentType = OvernetUsbEnvironment;
};

class OvernetUsbTest : public ::testing::Test {
 public:
  fuchsia_hardware_usb_request::Request WaitForRequestOn(uint8_t endpoint) {
    auto& runtime = driver_test().runtime();
    size_t request_count;
    fuchsia_hardware_usb_request::Request request;
    FDF_LOG(DEBUG, "Waiting for request on endpoint %d", endpoint);
    do {
      runtime.RunUntilIdle();
      driver_test().RunInEnvironmentTypeContext(
          [&request, &request_count, endpoint](OvernetUsbEnvironment& env) mutable {
            auto& ep = env.fake_usb_->fake_endpoint(endpoint);
            request_count = ep.pending_request_count();
            if (request_count > 0) {
              request = ep.GetNextRequest();
            }
          });
    } while (request_count == 0);

    return request;
  }
  // seems to be getting stuck waiting for the second buffer on the out endpoint, which really
  // shouldn't happen?
  bool SendTx(const uint8_t* tx, size_t size) {
    FDF_LOG(DEBUG, "SendTx(%zu)", size);

    auto request = WaitForRequestOn(kBulkOutEndpoint);
    FDF_LOG(DEBUG, "got request on out endpoint");
    driver_test().RunInEnvironmentTypeContext(
        [request = std::move(request), tx, size](OvernetUsbEnvironment& env) mutable {
          auto& out_ep = env.fake_usb_->fake_endpoint(kBulkOutEndpoint);
          auto& data = request.data();
          ASSERT_EQ(data->size(), 1u);
          auto& buffer = data->front().buffer();
          ASSERT_EQ(buffer->Which(), fuchsia_hardware_usb_request::Buffer::Tag::kVmoId);
          auto vmo_id = buffer->vmo_id().value();
          out_ep.WithVmo(vmo_id,
                         [tx, size](zx::vmo& vmo) { ASSERT_EQ(ZX_OK, vmo.write(tx, 0, size)); });
          data->at(0).size(size);
          out_ep.SendRequestComplete(std::move(request), ZX_OK, size);
        });
    return true;
  }

  std::optional<std::vector<uint8_t>> GetRx() {
    std::vector<uint8_t> ret;
    auto request = WaitForRequestOn(kBulkInEndpoint);
    driver_test().RunInEnvironmentTypeContext(
        [&ret, request = std::move(request)](OvernetUsbEnvironment& env) mutable {
          auto& in_ep = env.fake_usb_->fake_endpoint(kBulkInEndpoint);
          FDF_LOG(DEBUG, "Got request on in endpoint");
          auto& data = request.data();
          ASSERT_EQ(data->size(), 1u);
          auto& buffer = data->front().buffer();
          ASSERT_EQ(buffer->Which(), fuchsia_hardware_usb_request::Buffer::Tag::kVmoId);
          uint64_t vmo_id = buffer->vmo_id().value();
          ret.resize(data->front().size().value());
          size_t offset = data->front().offset().value_or(0);
          FDF_LOG(DEBUG, "reading %zu bytes from incoming vmo at offset %zu", ret.size(), offset);
          in_ep.WithVmo(vmo_id, [&ret, offset](zx::vmo& vmo) {
            ASSERT_EQ(ZX_OK, vmo.read(&*ret.begin(), offset, ret.size()));
          });
          in_ep.SendRequestComplete(std::move(request), ZX_OK, ret.size());
        });
    return std::optional(ret);
  }

  bool GetRxConcatExpect(const uint8_t* data, size_t len) {
    FDF_LOG(DEBUG, "GetRxConcatExpect(%zu)", len);
    while (len != 0) {
      auto got = GetRx();
      if (!got.has_value()) {
        FDF_LOG(ERROR, "No value returned from GetRx");
        return false;
      }
      if (got->size() > len) {
        FDF_LOG(ERROR, "returned size %zu was greater than expected %zu", got->size(), len);
        return false;
      }
      if (!std::equal(got->begin(), got->end(), data)) {
        FDF_LOG(ERROR, "returned data did not match expectation");
        return false;
      }
      len -= got->size();
      data += got->size();
    }
    return true;
  }

  bool SocketReadExpect(zx::socket* socket, const uint8_t* data, size_t len) {
    FDF_LOG(DEBUG, "SocketReadExpect(%zu)", len);
    std::vector<uint8_t> buf(len, 0);

    while (len > 0) {
      FDF_LOG(DEBUG, "reading loop iteration, need %zu bytes still", len);
      zx_signals_t pending;
      size_t actual;
      if (socket->wait_one(ZX_SOCKET_READABLE, zx::time::infinite(), &pending) != ZX_OK) {
        return false;
      }
      if ((pending & ZX_SOCKET_READABLE) == 0) {
        return false;
      }
      if (socket->read(0, buf.data(), len, &actual) != ZX_OK) {
        return false;
      }
      if (!std::equal(buf.begin(), buf.begin() + static_cast<ssize_t>(actual), data)) {
        return false;
      }
      len -= actual;
      data += actual;
    }
    return true;
  }

  bool SocketWriteAll(zx::socket* socket, const uint8_t* data, size_t len) {
    FDF_LOG(DEBUG, "SocketWriteAll(%zu)", len);
    while (len > 0) {
      zx_signals_t pending;
      size_t actual;
      zx_status_t res = socket->wait_one(ZX_SOCKET_WRITABLE, zx::time::infinite(), &pending);
      if (res != ZX_OK) {
        FDF_LOG(ERROR, "error while waiting on socket: %d", res);
        return false;
      }
      if ((pending & ZX_SOCKET_WRITABLE) == 0) {
        FDF_LOG(ERROR, "socket not writeable (%x)", pending);
        return false;
      }
      res = socket->write(0, data, len, &actual);
      if (res != ZX_OK) {
        FDF_LOG(ERROR, "error while writing to socket: %d", res);
        return false;
      }
      FDF_LOG(DEBUG, "wrote %zu bytes to socket", actual);
      len -= actual;
      data += actual;
    }
    return true;
  }

  void SetUp() override {
    ASSERT_TRUE(driver_test().StartDriver().is_ok());
    driver_test().RunInEnvironmentTypeContext([this](OvernetUsbEnvironment& env) {
      function_client_.Bind(env.fake_usb_->TakeInterface());
    });
    auto device = driver_test().Connect<fuchsia_hardware_overnet::UsbService::Device>();
    ASSERT_TRUE(device.is_ok());
    client_.Bind(std::move(device.value()));
    driver_test().RunInDriverContext([](OvernetUsb& driver) {
      EXPECT_EQ(driver.BulkInAddress(), kBulkInEndpoint);
      EXPECT_EQ(driver.BulkOutAddress(), kBulkOutEndpoint);
    });
  }

  void TearDown() override {
    FDF_LOG(DEBUG, "TearDown start");

    zx::result<> result = driver_test().StopDriver();
    ASSERT_TRUE(result.is_ok());
    FDF_LOG(DEBUG, "TearDown finished");
  }

  void ConfigureDevice() {
    ExpectConfigureEndpoints();
    fidl::Result result = function_client_->SetConfigured({{
        .configured = true,
        .speed = fuchsia_hardware_usb_descriptor::UsbSpeed::kHigh,
    }});
    ASSERT_TRUE(result.is_ok()) << result.error_value().FormatDescription();
  }

  void UnconfigureDevice() {
    FDF_LOG(DEBUG, "Unconfiguring device");
    ExpectDisableEndpoints();
    fidl::Result result = function_client_->SetConfigured({{
        .configured = false,
    }});
    ASSERT_TRUE(result.is_ok()) << result.error_value().FormatDescription();
  }

  void ResetWithSetInterface() {
    FDF_LOG(DEBUG, "Resetting device by calling SetInterface on it");
    ExpectConfigureEndpoints();
    fidl::Result result = function_client_->SetInterface({{
        .interface = kInterfaceNum,
        .alt_setting = 0,
    }});
    ASSERT_TRUE(result.is_ok()) << result.error_value().FormatDescription();
  }

  void ExpectConfigureEndpoints() {
    driver_test().RunInEnvironmentTypeContext([](OvernetUsbEnvironment& env) {
      env.fake_usb_->ExpectConfigureEndpoint(kBulkInEndpoint);
      env.fake_usb_->ExpectConfigureEndpoint(kBulkOutEndpoint);
    });
  }

  void ExpectDisableEndpoints() {
    driver_test().RunInEnvironmentTypeContext([](OvernetUsbEnvironment& env) {
      env.fake_usb_->ExpectDisableEndpoint(kBulkInEndpoint);
      env.fake_usb_->ExpectDisableEndpoint(kBulkOutEndpoint);
    });
  }

  std::unique_ptr<TestCallback> SetupCallback(size_t expected_calls,
                                              std::function<void(zx::socket)> callback) {
    auto callback_obj = SetTestCallback(expected_calls, std::move(callback));
    EXPECT_TRUE(callback_obj);
    driver_test().runtime().RunUntilIdle();
    return callback_obj;
  }

  std::unique_ptr<TestCallback> SetTestCallback(size_t expected_calls,
                                                std::function<void(zx::socket)> callback) const {
    auto dispatcher = fdf::Dispatcher::GetCurrent()->async_dispatcher();
    auto ret = std::make_unique<TestCallback>(expected_calls, callback);
    auto endpoints = fidl::CreateEndpoints<fuchsia_hardware_overnet::Callback>();
    if (!endpoints.is_ok()) {
      return nullptr;
    }
    fidl::BindServer(dispatcher, std::move(endpoints->server), ret.get());
    if (!client_->SetCallback(std::move(endpoints->client)).ok()) {
      return nullptr;
    }
    return ret;
  }

  fdf_testing::BackgroundDriverTest<OvernetUsbTestConfig>& driver_test() { return driver_test_; }
  fdf_testing::BackgroundDriverTest<OvernetUsbTestConfig> driver_test_;
  fidl::SyncClient<fuchsia_hardware_usb_function::UsbFunctionInterface> function_client_;
  fidl::WireSyncClient<fuchsia_hardware_overnet::Usb> client_;
};

TEST_F(OvernetUsbTest, Startup) { FDF_LOG(DEBUG, "startup"); }

TEST_F(OvernetUsbTest, ConfigureAndUnconfigure) {
  ConfigureDevice();
  UnconfigureDevice();
}

TEST_F(OvernetUsbTest, SocketGet) {
  ConfigureDevice();
  std::atomic_bool callback_called = false;
  auto callback = SetupCallback(1, [&callback_called](zx::socket sock) {
    FDF_LOG(DEBUG, "got socket");
    callback_called = true;
  });
  while (!callback_called) {
    driver_test().runtime().RunUntilIdle();
  }
  FDF_LOG(DEBUG, "Callback setup");
  UnconfigureDevice();
}

TEST_F(OvernetUsbTest, DataFromTarget) {
  ConfigureDevice();
  std::vector<zx::socket> sockets;
  auto callback =
      SetupCallback(1, [&sockets](zx::socket socket) { sockets.emplace_back(std::move(socket)); });
  while (sockets.size() < 1u) {
    driver_test().runtime().RunUntilIdle();
  }

  std::string_view test_data =
      "A basket of biscuits, a basket of mixed biscuits and a biscuit mixer.";
  ASSERT_TRUE(SocketWriteAll(&sockets[0], reinterpret_cast<const uint8_t*>(test_data.data()),
                             test_data.size()));

  ASSERT_TRUE(
      GetRxConcatExpect(reinterpret_cast<const uint8_t*>(test_data.data()), test_data.size()));

  std::string_view test_data_b = "Aluminum, linoleum, magnesium, petroleum.";
  ASSERT_TRUE(SocketWriteAll(&sockets[0], reinterpret_cast<const uint8_t*>(test_data_b.data()),
                             test_data_b.size()));
  ASSERT_TRUE(
      GetRxConcatExpect(reinterpret_cast<const uint8_t*>(test_data_b.data()), test_data_b.size()));
  UnconfigureDevice();
}

TEST_F(OvernetUsbTest, DataFromHost) {
  ConfigureDevice();
  std::vector<zx::socket> sockets;
  auto callback =
      SetupCallback(1, [&sockets](zx::socket socket) { sockets.emplace_back(std::move(socket)); });
  while (sockets.size() < 1u) {
    driver_test().runtime().RunUntilIdle();
  }

  std::string_view test_data =
      "A basket of biscuits, a basket of mixed biscuits and a biscuit mixer.";
  ASSERT_TRUE(SendTx(reinterpret_cast<const uint8_t*>(test_data.data()), test_data.size()));
  ASSERT_TRUE(SocketReadExpect(&sockets[0], reinterpret_cast<const uint8_t*>(test_data.data()),
                               test_data.size()));

  std::string_view test_data_b = "Aluminum, linoleum, magnesium, petroleum.";
  ASSERT_TRUE(SendTx(reinterpret_cast<const uint8_t*>(test_data_b.data()), test_data_b.size()));
  ASSERT_TRUE(SocketReadExpect(&sockets[0], reinterpret_cast<const uint8_t*>(test_data_b.data()),
                               test_data_b.size()));
  UnconfigureDevice();
}

TEST_F(OvernetUsbTest, Reset) {
  ConfigureDevice();
  std::vector<zx::socket> sockets;
  auto callback =
      SetupCallback(2, [&sockets](zx::socket socket) { sockets.emplace_back(std::move(socket)); });
  while (sockets.size() < 1u) {
    driver_test().runtime().RunUntilIdle();
  }

  std::string_view test_data =
      "A basket of biscuits, a basket of mixed biscuits and a biscuit mixer.";
  ASSERT_TRUE(SendTx(reinterpret_cast<const uint8_t*>(test_data.data()), test_data.size()));
  ASSERT_TRUE(SocketReadExpect(&sockets[0], reinterpret_cast<const uint8_t*>(test_data.data()),
                               test_data.size()));

  std::string_view test_data_b = "Aluminum, linoleum, magnesium, petroleum.";
  ASSERT_TRUE(SocketWriteAll(&sockets[0], reinterpret_cast<const uint8_t*>(test_data_b.data()),
                             test_data_b.size()));
  ASSERT_TRUE(
      GetRxConcatExpect(reinterpret_cast<const uint8_t*>(test_data_b.data()), test_data_b.size()));
  ResetWithSetInterface();
  // wait for the socket reset to work its way through and produce a new socket
  while (sockets.size() < 2u) {
    driver_test().runtime().RunUntilIdle();
  }

  zx_signals_t pending;
  sockets[0].wait_one(ZX_SOCKET_PEER_CLOSED, zx::time::infinite(), &pending);
  ASSERT_NE(pending & ZX_SOCKET_PEER_CLOSED, 0u);

  std::string_view test_data_c = "Around the rugged rocks the ragged rascals ran.";
  ASSERT_TRUE(SendTx(reinterpret_cast<const uint8_t*>(test_data_c.data()), test_data_c.size()));
  ASSERT_TRUE(SocketReadExpect(&sockets[1], reinterpret_cast<const uint8_t*>(test_data_c.data()),
                               test_data_c.size()));

  std::string_view test_data_d = "A proper copper coffee pot.";
  ASSERT_TRUE(SocketWriteAll(&sockets[1], reinterpret_cast<const uint8_t*>(test_data_d.data()),
                             test_data_d.size()));
  ASSERT_TRUE(
      GetRxConcatExpect(reinterpret_cast<const uint8_t*>(test_data_d.data()), test_data_d.size()));
  UnconfigureDevice();
}

TEST_F(OvernetUsbTest, ResetMoreData) {
  ConfigureDevice();
  std::vector<zx::socket> sockets;
  auto callback =
      SetupCallback(2, [&sockets](zx::socket socket) { sockets.emplace_back(std::move(socket)); });
  while (sockets.size() < 1) {
    driver_test().runtime().RunUntilIdle();
  }

  std::string_view test_data_a =
      "A basket of biscuits, a basket of mixed biscuits and a biscuit mixer.";
  std::string_view test_data_b = "Aluminum, linoleum, magnesium, petroleum.";
  std::string_view test_data_c = "Around the rugged rocks the ragged rascals ran.";
  std::string_view test_data_d = "A proper copper coffee pot.";

  for (int i = 0; i < 50; i++) {
    ASSERT_TRUE(SendTx(reinterpret_cast<const uint8_t*>(test_data_a.data()), test_data_a.size()));
    ASSERT_TRUE(SocketReadExpect(&sockets[0], reinterpret_cast<const uint8_t*>(test_data_a.data()),
                                 test_data_a.size()));

    ASSERT_TRUE(SocketWriteAll(&sockets[0], reinterpret_cast<const uint8_t*>(test_data_b.data()),
                               test_data_b.size()));
    ASSERT_TRUE(GetRxConcatExpect(reinterpret_cast<const uint8_t*>(test_data_b.data()),
                                  test_data_b.size()));

    ASSERT_TRUE(SendTx(reinterpret_cast<const uint8_t*>(test_data_c.data()), test_data_c.size()));
    ASSERT_TRUE(SocketReadExpect(&sockets[0], reinterpret_cast<const uint8_t*>(test_data_c.data()),
                                 test_data_c.size()));

    ASSERT_TRUE(SocketWriteAll(&sockets[0], reinterpret_cast<const uint8_t*>(test_data_d.data()),
                               test_data_d.size()));
    ASSERT_TRUE(GetRxConcatExpect(reinterpret_cast<const uint8_t*>(test_data_d.data()),
                                  test_data_d.size()));
  }
  ResetWithSetInterface();
  // wait for the socket reset to work its way through and produce a new socket
  while (sockets.size() < 2) {
    driver_test().runtime().RunUntilIdle();
  }

  zx_signals_t pending;
  sockets[0].wait_one(ZX_SOCKET_PEER_CLOSED, zx::time::infinite(), &pending);
  ASSERT_NE(pending & ZX_SOCKET_PEER_CLOSED, 0u);

  for (int i = 0; i < 50; i++) {
    ASSERT_TRUE(SendTx(reinterpret_cast<const uint8_t*>(test_data_a.data()), test_data_a.size()));
    ASSERT_TRUE(SocketReadExpect(&sockets[1], reinterpret_cast<const uint8_t*>(test_data_a.data()),
                                 test_data_a.size()));

    ASSERT_TRUE(SocketWriteAll(&sockets[1], reinterpret_cast<const uint8_t*>(test_data_b.data()),
                               test_data_b.size()));
    ASSERT_TRUE(GetRxConcatExpect(reinterpret_cast<const uint8_t*>(test_data_b.data()),
                                  test_data_b.size()));

    ASSERT_TRUE(SendTx(reinterpret_cast<const uint8_t*>(test_data_c.data()), test_data_c.size()));
    ASSERT_TRUE(SocketReadExpect(&sockets[1], reinterpret_cast<const uint8_t*>(test_data_c.data()),
                                 test_data_c.size()));

    ASSERT_TRUE(SocketWriteAll(&sockets[1], reinterpret_cast<const uint8_t*>(test_data_d.data()),
                               test_data_d.size()));
    ASSERT_TRUE(GetRxConcatExpect(reinterpret_cast<const uint8_t*>(test_data_d.data()),
                                  test_data_d.size()));
  }
  UnconfigureDevice();
}
TEST_F(OvernetUsbTest, Inspect) {
  ConfigureDevice();
  std::vector<zx::socket> sockets;
  auto callback =
      SetupCallback(1, [&sockets](zx::socket socket) { sockets.emplace_back(std::move(socket)); });
  while (sockets.size() < 1u) {
    driver_test().runtime().RunUntilIdle();
  }

  std::string_view host_to_device_data = "Host to Device (RX for driver)";
  std::string_view device_to_host_data = "Device to Host (TX for driver)";

  // 1. Host to Device (RX)
  ASSERT_TRUE(SendTx(reinterpret_cast<const uint8_t*>(host_to_device_data.data()),
                     host_to_device_data.size()));
  ASSERT_TRUE(SocketReadExpect(&sockets[0],
                               reinterpret_cast<const uint8_t*>(host_to_device_data.data()),
                               host_to_device_data.size()));

  // 2. Device to Host (TX)
  ASSERT_TRUE(SocketWriteAll(&sockets[0],
                             reinterpret_cast<const uint8_t*>(device_to_host_data.data()),
                             device_to_host_data.size()));
  ASSERT_TRUE(GetRxConcatExpect(reinterpret_cast<const uint8_t*>(device_to_host_data.data()),
                                device_to_host_data.size()));

  // 3. Wait for all in-flight USB TX requests to complete cleanly (eliminates async FIDL races!)
  bool has_pending_tx = true;
  while (has_pending_tx) {
    driver_test().RunInDriverContext(
        [&has_pending_tx](OvernetUsb& driver) { has_pending_tx = driver.HasPendingTxRequests(); });
    if (has_pending_tx) {
      zx::nanosleep(zx::deadline_after(zx::msec(1)));
    }
  }

  driver_test().RunInDriverContext([tx_size = device_to_host_data.size(),
                                    rx_size = host_to_device_data.size()](OvernetUsb& driver) {
    driver.GetThroughputTrackerForTesting().MeasureForTesting(zx::sec(1));

    auto hierarchy = usb_inspect::ReadHierarchyFromInspector(driver.inspector().inspector());

    auto* overnet_node = hierarchy.GetByPath({"overnet-usb"});
    ASSERT_TRUE(overnet_node != nullptr);

    auto* bulk_in = hierarchy.GetByPath({"overnet-usb", "bulk_in"});
    ASSERT_TRUE(bulk_in != nullptr);
    auto err_in = usb_inspect::VerifyEndpointInspect(bulk_in, tx_size, std::nullopt, 0,
                                                     std::nullopt, tx_size);
    EXPECT_TRUE(err_in.is_ok()) << err_in.error_value();

    auto* bulk_out = hierarchy.GetByPath({"overnet-usb", "bulk_out"});
    ASSERT_TRUE(bulk_out != nullptr);
    auto err_out = usb_inspect::VerifyEndpointInspect(bulk_out, std::nullopt, rx_size, std::nullopt,
                                                      8, rx_size);
    EXPECT_TRUE(err_out.is_ok()) << err_out.error_value();
  });

  UnconfigureDevice();
}

// NOLINTEND(readability-container-data-pointer)
// NOLINTEND(readability-convert-member-functions-to-static)
// NOLINTEND(misc-use-anonymous-namespace)
