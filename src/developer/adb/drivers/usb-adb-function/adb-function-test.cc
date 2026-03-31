// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "adb-function.h"

#include <fidl/fuchsia.hardware.adb/cpp/fidl.h>
#include <lib/async-loop/default.h>
#include <lib/async/default.h>
#include <lib/driver/outgoing/cpp/outgoing_directory.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/sync/completion.h>

#include <vector>

#include <usb/usb-request.h>
#include <zxtest/zxtest.h>

#include "src/devices/usb/lib/usb-endpoint/testing/fake-usb-endpoint-server.h"

namespace usb_adb_function {

static constexpr uint32_t kBulkOutEp = 1;
static constexpr uint32_t kBulkInEp = 2;

class AdbFakeUsb
    : public fake_usb_endpoint::FakeUsbFidlProvider<fuchsia_hardware_usb_function::UsbFunction,
                                                    fake_usb_endpoint::FakeEndpoint> {
 public:
  using Base = fake_usb_endpoint::FakeUsbFidlProvider<fuchsia_hardware_usb_function::UsbFunction,
                                                      fake_usb_endpoint::FakeEndpoint>;
  AdbFakeUsb(async_dispatcher_t* dispatcher) : Base(dispatcher), dispatcher_(dispatcher) {}

  void AllocResources(
      fidl::Request<fuchsia_hardware_usb_function::UsbFunction::AllocResources>& request,
      fidl::internal::NaturalCompleter<
          fuchsia_hardware_usb_function::UsbFunction::AllocResources>::Sync& completer) override {
    fuchsia_hardware_usb_function::UsbFunctionAllocResourcesResponse response;
    EXPECT_EQ(request.endpoints().size(), 2);
    EXPECT_EQ(request.interface_count(), 1u);
    EXPECT_EQ(request.strings().size(), 0u);
    response.interface_nums() = {0};
    response.endpoint_addrs() = {kBulkOutEp, kBulkInEp};
    response.string_indices() = {};

    for (size_t i = 0; i < request.endpoints().size(); i++) {
      uint8_t addr = response.endpoint_addrs()[i];
      fake_endpoint(addr).Connect(dispatcher_, std::move(request.endpoints()[i].endpoint()));
    }

    completer.Reply(fit::ok(std::move(response)));
  }

  void Configure(
      fidl::Request<fuchsia_hardware_usb_function::UsbFunction::Configure>& request,
      fidl::internal::NaturalCompleter<fuchsia_hardware_usb_function::UsbFunction::Configure>::Sync&
          completer) override {
    iface_client_ = std::move(request.iface());
    completer.Reply(fit::ok());
    if (on_configured_) {
      on_configured_();
    }
  }

  void Deconfigure(
      fidl::internal::NaturalCompleter<
          fuchsia_hardware_usb_function::UsbFunction::Deconfigure>::Sync& completer) override {
    completer.Reply(fit::ok());
    if (on_deconfigured_) {
      on_deconfigured_();
    }
  }

  fidl::ClientEnd<fuchsia_hardware_usb_function::UsbFunctionInterface> TakeIfaceClient() {
    return std::move(iface_client_);
  }

  void set_on_configured(fit::callback<void()> on_configured) {
    on_configured_ = std::move(on_configured);
  }

  void set_on_deconfigured(fit::callback<void()> on_deconfigured) {
    on_deconfigured_ = std::move(on_deconfigured);
  }

 private:
  async_dispatcher_t* dispatcher_;
  fidl::ClientEnd<fuchsia_hardware_usb_function::UsbFunctionInterface> iface_client_;
  fit::callback<void()> on_configured_;
  fit::callback<void()> on_deconfigured_;
};

class UsbAdbEnvironment : public fdf_testing::Environment {
 public:
  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    async_dispatcher_t* dispatcher = fdf::Dispatcher::GetCurrent()->async_dispatcher();
    fuchsia_hardware_usb_function::UsbFunctionService::InstanceHandler handler({
        .device = usb_function_bindings_.CreateHandler(&fake_dev_, dispatcher,
                                                       fidl::kIgnoreBindingClosure),
    });
    EXPECT_OK(to_driver_vfs.AddService<fuchsia_hardware_usb_function::UsbFunctionService>(
        std::move(handler)));

    return zx::ok();
  }

  void CancelAllUsbRxRequests() {
    for (size_t i = 0; i < kBulkRxCount; i++) {
      fake_dev_.fake_endpoint(kBulkOutEp).RequestComplete(ZX_ERR_CANCELED, 0);
    }
  }

  AdbFakeUsb fake_dev_ = AdbFakeUsb(fdf::Dispatcher::GetCurrent()->async_dispatcher());
  fidl::ServerBindingGroup<fuchsia_hardware_usb_function::UsbFunction> usb_function_bindings_;
};

class UsbAdbTestConfig final {
 public:
  using DriverType = UsbAdbDevice;
  using EnvironmentType = UsbAdbEnvironment;
};

class UsbAdbTest : public zxtest::Test {
 public:
  fidl::WireSyncClient<fadb::UsbAdbImpl> NormalStartAdb() {
    auto [client_end, server_end] = fidl::Endpoints<fadb::UsbAdbImpl>::Create();
    EXPECT_OK(client_->StartAdb(std::move(server_end)));
    WaitConfigured();
    EnableUsb();

    return fidl::WireSyncClient<fadb::UsbAdbImpl>(std::move(client_end));
  }

  void NormalStopDriver() {
    CancelAllUsbRxRequestsOnDeconfigure();
    EXPECT_OK(driver_test_.StopDriver());
  }

  void SetUp() override {
    // Expect calls from UsbAdbDevice initialization
    libsync::Completion configured;
    driver_test_.RunInEnvironmentTypeContext([&](UsbAdbEnvironment& env) {
      env.fake_dev_.set_on_configured([&]() { configured.Signal(); });
    });

    ASSERT_OK(driver_test_.StartDriver().status_value());
    auto device = driver_test_.Connect<fadb::Service::Adb>();
    EXPECT_OK(device.status_value());
    client_.Bind(std::move(device.value()));
    configured.Wait();
    driver_test_.RunInEnvironmentTypeContext([&](UsbAdbEnvironment& env) {
      iface_client_.Bind(env.fake_dev_.TakeIfaceClient());
      env.fake_dev_.set_on_configured(nullptr);
    });
    driver_test_.RunInDriverContext([&](UsbAdbDevice& dev) {
      EXPECT_EQ(dev.bulk_out_addr(), kBulkOutEp);
      EXPECT_EQ(dev.bulk_in_addr(), kBulkInEp);
    });
  }

  // Call SetConfigured of usb adb to bring the interface online.
  void EnableUsb() {
    ASSERT_TRUE(iface_client_.is_valid());
    fidl::Result result = iface_client_->SetConfigured({{
        .configured = true,
        .speed = fuchsia_hardware_usb_descriptor::UsbSpeed::kFull,
    }});
    EXPECT_TRUE(result.is_ok(), "%s", result.error_value().FormatDescription().c_str());
  }

  zx_status_t WaitFunctionClosed() {
    EXPECT_TRUE(iface_client_.is_valid());
    fidl::ClientEnd client_end = iface_client_.TakeClientEnd();
    return client_end.channel().wait_one(ZX_CHANNEL_PEER_CLOSED, zx::time::infinite(), nullptr);
  }

  void WaitConfigured() {
    if (iface_client_.is_valid()) {
      return;
    }
    fidl::ClientEnd<fuchsia_hardware_usb_function::UsbFunctionInterface> iface_client;
    libsync::Completion configured;
    for (;;) {
      driver_test_.RunInEnvironmentTypeContext([&](UsbAdbEnvironment& env) {
        iface_client = env.fake_dev_.TakeIfaceClient();
        if (iface_client.is_valid()) {
          env.fake_dev_.set_on_configured(nullptr);
          return;
        }
        env.fake_dev_.set_on_configured([&]() { configured.Signal(); });
      });
      if (iface_client.is_valid()) {
        iface_client_.Bind(std::move(iface_client));
        return;
      }
      configured.Wait();
    }
  }

  void SendTestData(fidl::WireSyncClient<fadb::UsbAdbImpl>& usb_impl, size_t size) {
    uint8_t test_data[size];

    driver_test_.RunInEnvironmentTypeContext([&](UsbAdbEnvironment& env) {
      for (uint32_t i = 0; i < sizeof(test_data) / kVmoDataSize; i++) {
        env.fake_dev_.fake_endpoint(kBulkInEp).RequestComplete(ZX_OK, kVmoDataSize);
      }
      if (sizeof(test_data) % kVmoDataSize) {
        env.fake_dev_.fake_endpoint(kBulkInEp).RequestComplete(ZX_OK,
                                                               sizeof(test_data) % kVmoDataSize);
      }
    });

    auto result =
        usb_impl->QueueTx(fidl::VectorView<uint8_t>::FromExternal(test_data, sizeof(test_data)));
    ASSERT_TRUE(result.ok());
    ASSERT_TRUE(result->is_ok());

    driver_test_.RunInEnvironmentTypeContext([&](UsbAdbEnvironment& env) {
      EXPECT_EQ(env.fake_dev_.fake_endpoint(kBulkInEp).pending_request_count(), 0);
    });
  }

  void ExpectReceiveData(size_t size) {
    // Invoke request completion on bulk out endpoint.
    driver_test_.RunInEnvironmentTypeContext([&](UsbAdbEnvironment& env) {
      env.fake_dev_.fake_endpoint(kBulkOutEp).RequestComplete(ZX_OK, size);
    });
  }

  void CancelAllUsbRxRequestsOnDeconfigure() {
    driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
      env.fake_dev_.set_on_deconfigured([&]() { env.CancelAllUsbRxRequests(); });
    });
  }

  fdf_testing::BackgroundDriverTest<UsbAdbTestConfig> driver_test_;
  fidl::WireSyncClient<fadb::Device> client_;
  fidl::SyncClient<fuchsia_hardware_usb_function::UsbFunctionInterface> iface_client_;
};

class EventHandler : public fidl::WireSyncEventHandler<fadb::UsbAdbImpl> {
 public:
  ~EventHandler() { EXPECT_TRUE(expected_statuses_.empty()); }

  void OnStatusChanged(fidl::WireEvent<fadb::UsbAdbImpl::OnStatusChanged>* event) override {
    ASSERT_FALSE(expected_statuses_.empty());
    EXPECT_EQ(event->status, expected_statuses_.front());
    expected_statuses_.pop();
  }

  std::queue<fadb::StatusFlags> expected_statuses_;
};

TEST_F(UsbAdbTest, StopBeforeUsbStartsUp) { EXPECT_OK(driver_test_.StopDriver()); }

TEST_F(UsbAdbTest, StartStop) {
  auto [client_end, server_end] = fidl::Endpoints<fadb::UsbAdbImpl>::Create();
  EXPECT_OK(client_->StartAdb(std::move(server_end)));
  auto usb_impl = fidl::WireSyncClient<fadb::UsbAdbImpl>(std::move(client_end));

  EventHandler handler;

  // TODO(https://fxbug.dev/398918059): Enable this assertion when
  // HandleOneEvent supports a deadline.
  //
  // We don't expect an "online" event until after USB comes up.
  // EXPECT_EQ(usb_impl.HandleOneEvent(handler, zx::deadline_after(zx::msec(1))).status(),
  //           ZX_ERR_TIMED_OUT);

  EnableUsb();

  // Now we should get the event.
  handler.expected_statuses_.push(fadb::StatusFlags::kOnline);
  EXPECT_OK(usb_impl.HandleOneEvent(handler));

  libsync::Completion stop_requested;
  driver_test_.RunInEnvironmentTypeContext([&](UsbAdbEnvironment& env) {
    env.fake_dev_.set_on_deconfigured([&]() { stop_requested.Signal(); });
  });

  // Request a USB reset.
  libsync::Completion stop_finished;
  std::thread t([&]() {
    EXPECT_OK(client_->StopAdb());
    stop_finished.Signal();
  });

  // TODO(https://fxbug.dev/398918059): Enable this assertion when
  // HandleOneEvent supports a deadline.
  //
  // We don't expect an "offline" event or for StopAdb to complete until USB is shut down.
  // EXPECT_EQ(usb_impl.HandleOneEvent(handler, zx::deadline_after(zx::msec(1))).status(),
  //           ZX_ERR_TIMED_OUT);
  EXPECT_EQ(stop_finished.Wait(zx::deadline_after(zx::msec(1))), ZX_ERR_TIMED_OUT);

  // We call CancelAllUsbRxRequests only _after_ the driver calls SetInterface
  // in order to avoid a race condition where we cancel a request, only to have
  // the driver process the cancellation and send it back out again before
  // `StopAdb()` gets processed.
  stop_requested.Wait();
  driver_test_.RunInEnvironmentTypeContext(
      [&](UsbAdbEnvironment& env) { env.CancelAllUsbRxRequests(); });
  ASSERT_OK(WaitFunctionClosed());

  handler.expected_statuses_.emplace(0);
  EXPECT_OK(usb_impl.HandleOneEvent(handler));
  EXPECT_EQ(usb_impl.HandleOneEvent(handler).status(), ZX_ERR_PEER_CLOSED);

  stop_finished.Wait();
  t.join();

  EXPECT_OK(driver_test_.StopDriver());
}

TEST_F(UsbAdbTest, StopDriverWhileConnected) {
  auto usb_impl = NormalStartAdb();

  EventHandler handler;
  handler.expected_statuses_.emplace(fadb::StatusFlags::kOnline);
  EXPECT_OK(usb_impl.HandleOneEvent(handler));

  CancelAllUsbRxRequestsOnDeconfigure();
  EXPECT_OK(driver_test_.StopDriver());

  handler.expected_statuses_.emplace(0);
  EXPECT_OK(usb_impl.HandleOneEvent(handler));
}

TEST_F(UsbAdbTest, UsbStackRequestsStop) {
  auto usb_impl = NormalStartAdb();

  EventHandler handler;
  handler.expected_statuses_.emplace(fadb::StatusFlags::kOnline);
  EXPECT_OK(usb_impl.HandleOneEvent(handler));

  fidl::Result result = iface_client_->SetConfigured({{
      .configured = false,
  }});
  ASSERT_TRUE(result.is_ok(), "%s", result.error_value().FormatDescription().c_str());
  driver_test_.RunInEnvironmentTypeContext(
      [](UsbAdbEnvironment& env) { env.CancelAllUsbRxRequests(); });

  handler.expected_statuses_.emplace(0);
  EXPECT_OK(usb_impl.HandleOneEvent(handler));
  EXPECT_OK(driver_test_.StopDriver());
}

TEST_F(UsbAdbTest, StartStopStartStop) {
  {
    EventHandler handler;
    auto usb_impl = NormalStartAdb();
    handler.expected_statuses_.push(fadb::StatusFlags::kOnline);
    EXPECT_OK(usb_impl.HandleOneEvent(handler));

    CancelAllUsbRxRequestsOnDeconfigure();
    EXPECT_OK(client_->StopAdb());

    handler.expected_statuses_.emplace(0);
    EXPECT_OK(usb_impl.HandleOneEvent(handler));
    EXPECT_EQ(usb_impl.HandleOneEvent(handler).status(), ZX_ERR_PEER_CLOSED);
    ASSERT_OK(WaitFunctionClosed());
  }

  {
    EventHandler handler;
    auto usb_impl = NormalStartAdb();
    handler.expected_statuses_.push(fadb::StatusFlags::kOnline);
    EXPECT_OK(usb_impl.HandleOneEvent(handler));

    CancelAllUsbRxRequestsOnDeconfigure();
    EXPECT_OK(client_->StopAdb());

    handler.expected_statuses_.emplace(0);
    EXPECT_OK(usb_impl.HandleOneEvent(handler));
    EXPECT_EQ(usb_impl.HandleOneEvent(handler).status(), ZX_ERR_PEER_CLOSED);
    ASSERT_OK(WaitFunctionClosed());
  }

  EXPECT_OK(driver_test_.StopDriver());
}

TEST_F(UsbAdbTest, StartAdbAfterUsbConnectionEstablished) {
  EnableUsb();

  auto [client_end, server_end] = fidl::Endpoints<fadb::UsbAdbImpl>::Create();
  EXPECT_OK(client_->StartAdb(std::move(server_end)));

  auto usb_impl = fidl::WireSyncClient<fadb::UsbAdbImpl>(std::move(client_end));

  // We should get kOnline immediately, because we're already connected.
  EventHandler handler;
  handler.expected_statuses_.push(fadb::StatusFlags::kOnline);
  EXPECT_OK(usb_impl.HandleOneEvent(handler));

  ASSERT_NO_FATAL_FAILURE(NormalStopDriver());
}

TEST_F(UsbAdbTest, SendAdbMessage) {
  auto usb_impl = NormalStartAdb();

  // Sending data that fits within a single VMO request
  ASSERT_NO_FATAL_FAILURE(SendTestData(usb_impl, kVmoDataSize - 2));
  // Sending data that is exactly fills up a single VMO request
  ASSERT_NO_FATAL_FAILURE(SendTestData(usb_impl, kVmoDataSize));
  // Sending data that exceeds a single VMO request
  ASSERT_NO_FATAL_FAILURE(SendTestData(usb_impl, kVmoDataSize + 2));
  // Sending data that exceeds kBulkTxRxCount VMO requests (the last packet should be stored in
  // queue)
  ASSERT_NO_FATAL_FAILURE(SendTestData(usb_impl, kVmoDataSize * kBulkTxCount + 2));
  // Sending data that exceeds kBulkTxRxCount + 1 VMO requests (probably unneeded test, but added
  // for good measure.)
  ASSERT_NO_FATAL_FAILURE(SendTestData(usb_impl, kVmoDataSize * (kBulkTxCount + 1) + 2));

  ASSERT_NO_FATAL_FAILURE(NormalStopDriver());
}

TEST_F(UsbAdbTest, RecvAdbMessage) {
  constexpr uint32_t kReceiveSize = kVmoDataSize - 2;
  auto usb_impl = NormalStartAdb();

  // Queue a receive request before the data is available. The request will not get an immediate
  // reply. Data fits within a single VMO request.

  std::thread t([&]() {
    auto response = usb_impl->Receive();
    ASSERT_OK(response.status());
    ASSERT_EQ(response.value().value()->data.size(), kReceiveSize);
  });

  // Wait to make it so (most likely) the `Receive` request arrives first. This is
  // just a test coverage thing - it won't flake if the `ExpectReceiveData`
  // happens first.
  zx::nanosleep(zx::deadline_after(zx::msec(1)));

  ASSERT_NO_FATAL_FAILURE(ExpectReceiveData(kReceiveSize));
  t.join();

  ASSERT_NO_FATAL_FAILURE(NormalStopDriver());
}

}  // namespace usb_adb_function
