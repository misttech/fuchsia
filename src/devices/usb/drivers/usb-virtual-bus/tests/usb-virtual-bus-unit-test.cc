// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.usb.dci/cpp/test_base.h>
#include <fuchsia/hardware/usb/bus/cpp/banjo.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/sync/completion.h>

#include <gtest/gtest.h>

#include "src/devices/usb/drivers/usb-virtual-bus/usb-virtual-bus.h"

namespace usb_virtual_bus {

namespace fdci = fuchsia_hardware_usb_dci;
namespace fendpoint = fuchsia_hardware_usb_endpoint;
namespace frequest = fuchsia_hardware_usb_request;

class FakeUsbBus : public ddk::UsbBusInterfaceProtocol<FakeUsbBus> {
 public:
  zx_status_t UsbBusInterfaceAddDevice(uint32_t device_id, uint32_t hub_id, usb_speed_t speed) {
    return ZX_OK;
  }
  zx_status_t UsbBusInterfaceRemoveDevice(uint32_t device_id) { return ZX_OK; }
  zx_status_t UsbBusInterfaceResetPort(uint32_t hub_id, uint32_t port, bool enumerating) {
    return ZX_ERR_NOT_SUPPORTED;
  }
  zx_status_t UsbBusInterfaceReinitializeDevice(uint32_t device_id) { return ZX_ERR_NOT_SUPPORTED; }

  usb_bus_interface_protocol_t* get_proto() {
    proto_.ctx = this;
    proto_.ops = &usb_bus_interface_protocol_ops_;
    return &proto_;
  }

 private:
  usb_bus_interface_protocol_t proto_;
};

class EndpointHandler : public fidl::SyncEventHandler<fendpoint::Endpoint> {
 public:
  ~EndpointHandler() { EXPECT_TRUE(expected_on_completion_.empty()); }

  using ExpectedOnCompletionFnType =
      std::function<void(fidl::Event<fendpoint::Endpoint::OnCompletion>& event)>;

  void OnCompletion(fidl::Event<fendpoint::Endpoint::OnCompletion>& event) override {
    ASSERT_FALSE(expected_on_completion_.empty());
    expected_on_completion_.front()(event);
    expected_on_completion_.pop();
  }

  void ExpectOnCompletion(ExpectedOnCompletionFnType fn) {
    expected_on_completion_.emplace(std::move(fn));
  }

  // Helper functions for common expected OnCompletions
  static ExpectedOnCompletionFnType ExpectOnCompletionDirect(size_t data_size, bool validate_data) {
    return [data_size, validate_data](fidl::Event<fendpoint::Endpoint::OnCompletion>& event) {
      ASSERT_EQ(event.completion().size(), 1UL);
      ASSERT_EQ(event.completion()[0].status(), ZX_OK);
      ASSERT_EQ(event.completion()[0].transfer_size(), data_size);
      if (validate_data) {
        const auto& data = *event.completion()[0].request()->data();
        ASSERT_EQ(data.size(), 1UL);
        ASSERT_EQ(data[0].offset(), 0UL);
        ASSERT_EQ(data[0].size(), data_size);
        ASSERT_EQ(data[0].buffer()->Which(), frequest::Buffer::Tag::kData);
        for (size_t i = 0; i < data_size; i++) {
          ASSERT_EQ(data[0].buffer()->data().value()[i], static_cast<uint8_t>(i));
        }
      }
    };
  }

  static ExpectedOnCompletionFnType ExpectOnCompletionVmo(size_t data_size,
                                                          uint8_t* data_ptr = nullptr) {
    return [data_size, data_ptr](fidl::Event<fendpoint::Endpoint::OnCompletion>& event) {
      ASSERT_EQ(event.completion().size(), 1UL);
      ASSERT_EQ(event.completion()[0].status(), ZX_OK);
      ASSERT_EQ(event.completion()[0].transfer_size(), data_size);
      if (data_ptr) {
        const auto& data = *event.completion()[0].request()->data();
        ASSERT_EQ(data.size(), 1UL);
        ASSERT_EQ(data[0].offset(), 0UL);
        ASSERT_EQ(data[0].size(), data_size);
        ASSERT_EQ(data[0].buffer()->Which(), frequest::Buffer::Tag::kVmoId);
        ASSERT_EQ(data[0].buffer()->vmo_id().value(), 1UL);
        for (size_t i = 0; i < data_size; i++) {
          ASSERT_EQ(data_ptr[i], static_cast<uint8_t>(i));
        }
      }
    };
  }

 private:
  std::queue<ExpectedOnCompletionFnType> expected_on_completion_;
};

class FakeDci : public fidl::testing::TestBase<fdci::UsbDciInterface> {
 public:
  ~FakeDci() { EXPECT_EQ(expected_control_, 0UL); }

  // FakeEndpoint& endpoint() { return endpoint_; }
  // void WaitForUnbind() { sync_completion_wait(&unbind_sync_, ZX_TIME_INFINITE); }
  fidl::ClientEnd<fdci::UsbDciInterface> Connect() {
    auto [client_end, server_end] = fidl::Endpoints<fdci::UsbDciInterface>::Create();
    bindings_.AddBinding(fdf::Dispatcher::GetCurrent()->async_dispatcher(), std::move(server_end),
                         this, fidl::kIgnoreBindingClosure);
    return std::move(client_end);
  }

  void ExpectControl() { expected_control_++; }
  libsync::Completion& wait_for_control() { return wait_for_control_; }

 private:
  void Control(ControlRequest& request, ControlCompleter::Sync& completer) override {
    if (!expected_control_) {
      // Used for Disconnect and unbind tests. Store the completer.
      store_completer_.emplace(completer.ToAsync());
      wait_for_control_.Signal();
      return;
    }
    expected_control_--;

    std::vector<uint8_t> control_data(request.setup().w_length());
    for (size_t i = 0; i < request.setup().w_length(); i++) {
      control_data[i] = static_cast<uint8_t>(i);
    }
    completer.Reply(zx::ok(std::move(control_data)));
  }
  void SetConnected(SetConnectedRequest& request, SetConnectedCompleter::Sync& completer) override {
    store_completer_.reset();
    completer.Reply(zx::ok());
  }
  void handle_unknown_method(fidl::UnknownMethodMetadata<fdci::UsbDciInterface> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {
    ASSERT_FALSE(true);
  }
  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {}

  fidl::ServerBindingGroup<fdci::UsbDciInterface> bindings_;
  std::atomic_uint32_t expected_control_ = 0;
  std::optional<ControlCompleter::Async> store_completer_;
  libsync::Completion wait_for_control_;
};

class TestEnvironment : public fdf_testing::Environment {
 public:
  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override { return zx::ok(); }

  FakeDci dci_;
};

class UsbVirtualBusTestConfig final {
 public:
  using DriverType = UsbVirtualBus;
  using EnvironmentType = TestEnvironment;
};

class UsbVirtualBusTest : public testing::Test {
 protected:
  void SetUp() override {
    ASSERT_TRUE(driver_test_.StartDriver().is_ok());

    zx::result connect_result =
        driver_test().ConnectThroughDevfs<fuchsia_hardware_usb_virtual_bus::Bus>("usb-virtual-bus");
    EXPECT_TRUE(connect_result.is_ok());
    ASSERT_TRUE(connect_result->is_valid());
    virtual_bus_.Bind(std::move(*connect_result));
  }

  void TearDown() override { ASSERT_TRUE(driver_test_.StopDriver().is_ok()); }

  void EnableAndConnect() {
    Enable();
    Connect();
  }

  void Enable() {
    auto enable_result = virtual_bus()->Enable();
    ASSERT_TRUE(enable_result.ok());
    ASSERT_EQ(enable_result->status, ZX_OK);

    driver_test().RunInDriverContext([&](UsbVirtualBus& driver) {
      driver.SetBusInterface(fake_usb_bus_.get_proto());
      zx::result result = driver.SetDciInterface(
          driver_test().RunInEnvironmentTypeContext<fidl::ClientEnd<fdci::UsbDciInterface>>(
              [](TestEnvironment& env) { return env.dci_.Connect(); }));
      ASSERT_TRUE(result.is_ok());
    });
  }

  void Connect() {
    auto connect_result_fidl = virtual_bus()->Connect();
    ASSERT_TRUE(connect_result_fidl.ok());
    ASSERT_EQ(connect_result_fidl->status, ZX_OK);

    driver_test().RunInDriverContext([&](UsbVirtualBus& driver) { driver.FinishConnect(); });
  }

  template <typename Service, typename... Args>
  fidl::ClientEnd<fendpoint::Endpoint> ConnectToEndpoint(Args... args) {
    auto controler = driver_test().Connect<typename Service::Device>();
    EXPECT_TRUE(controler.is_ok());

    auto [client_end, server_end] = fidl::Endpoints<fendpoint::Endpoint>::Create();
    auto connect_result =
        fidl::WireCall(*controler)->ConnectToEndpoint(args..., std::move(server_end));
    EXPECT_TRUE(connect_result.ok());
    EXPECT_TRUE(connect_result->is_ok());
    return std::move(client_end);
  }

  // Registers one VMO at VMO ID = 1
  std::pair<uint8_t*, zx::vmo> RegisterVmo(fidl::SyncClient<fendpoint::Endpoint>& client,
                                           size_t data_size) {
    auto result = client->RegisterVmos(
        std::vector<fendpoint::VmoInfo>{{fendpoint::VmoInfo().id(1).size(data_size)}});
    EXPECT_TRUE(result.is_ok());
    EXPECT_EQ(result->vmos().size(), 1UL);
    EXPECT_EQ(result->vmos()[0].id(), 1UL);
    zx::vmo vmo = std::move(*result->vmos()[0].vmo());
    zx_vaddr_t mapped_addr;
    EXPECT_EQ(zx::vmar::root_self()->map(ZX_VM_PERM_READ | ZX_VM_PERM_WRITE, 0, vmo, 0, data_size,
                                         &mapped_addr),
              ZX_OK);
    return {reinterpret_cast<uint8_t*>(mapped_addr), std::move(vmo)};
  }

  fdf_testing::BackgroundDriverTest<UsbVirtualBusTestConfig>& driver_test() { return driver_test_; }
  fidl::WireSyncClient<fuchsia_hardware_usb_virtual_bus::Bus>& virtual_bus() {
    return virtual_bus_;
  }

 private:
  fdf_testing::BackgroundDriverTest<UsbVirtualBusTestConfig> driver_test_;

  fidl::WireSyncClient<fuchsia_hardware_usb_virtual_bus::Bus> virtual_bus_;
  FakeUsbBus fake_usb_bus_;
};

TEST_F(UsbVirtualBusTest, LifecycleTest) {
  // The driver should create a child node for the bus itself.
  driver_test().RunInNodeContext(
      [](fdf_testing::TestNode& node) { ASSERT_EQ(1UL, node.children().size()); });
}

TEST_F(UsbVirtualBusTest, EnableDisableTest) {
  auto enable_result = virtual_bus()->Enable();
  ASSERT_TRUE(enable_result.ok());
  ASSERT_EQ(enable_result->status, ZX_OK);

  // After enabling, there should be two additional children: host and device.
  // The total children will be 3 (bus, host, device).
  driver_test().RunInNodeContext(
      [](fdf_testing::TestNode& node) { ASSERT_EQ(3UL, node.children().size()); });

  auto disable_result = virtual_bus()->Disable();
  ASSERT_TRUE(disable_result.ok());
  ASSERT_EQ(disable_result->status, ZX_OK);

  // After disabling, the host and device children should eventually go away.
  while (driver_test().RunInNodeContext<size_t>(
             [](auto& node) { return node.children().size(); }) != 1) {
    zx::nanosleep(zx::deadline_after(zx::usec(30)));
  }
}

TEST_F(UsbVirtualBusTest, ReconnectTest) {
  EnableAndConnect();

  auto disconnect_result = virtual_bus()->Disconnect();
  ASSERT_TRUE(disconnect_result.ok());
  ASSERT_EQ(disconnect_result->status, ZX_OK);

  auto connect_result_fidl = virtual_bus()->Connect();
  ASSERT_TRUE(connect_result_fidl.ok());
  ASSERT_EQ(connect_result_fidl->status, ZX_OK);

  driver_test().RunInDriverContext([&](UsbVirtualBus& driver) { driver.FinishConnect(); });
}

TEST_F(UsbVirtualBusTest, BanjoControlRequestTest) {
  EnableAndConnect();

  size_t req_size;
  driver_test().RunInDriverContext(
      [&req_size](UsbVirtualBus& driver) { req_size = driver.host()->UsbHciGetRequestSize(); });

  usb_request_t* req;
  ASSERT_EQ(usb_request_alloc(&req, sizeof(usb_device_descriptor_t), 0, req_size), ZX_OK);
  // A standard GET_DESCRIPTOR request.
  usb_setup_t setup = {
      .bm_request_type = USB_DIR_IN | USB_TYPE_STANDARD | USB_RECIP_DEVICE,
      .b_request = USB_REQ_GET_DESCRIPTOR,
      .w_value = static_cast<uint16_t>(USB_DT_DEVICE << 8),
      .w_index = 0,
      .w_length = sizeof(usb_device_descriptor_t),
  };
  memcpy(&req->setup, &setup, sizeof(setup));
  req->header.length = sizeof(req->setup);

  libsync::Completion completion;
  usb_request_complete_callback_t complete = {
      .callback =
          [](void* ctx, usb_request_t* req) {
            ASSERT_EQ(req->response.status, ZX_OK);
            ASSERT_EQ(req->response.actual, sizeof(usb_device_descriptor_t));
            // Verify data
            uint8_t* data;
            ASSERT_EQ(usb_request_mmap(req, (void**)&data), ZX_OK);
            for (size_t i = 0; i < sizeof(usb_device_descriptor_t); i++) {
              ASSERT_EQ(data[i], static_cast<uint8_t>(i));
            }
            static_cast<libsync::Completion*>(ctx)->Signal();
          },
      .ctx = &completion,
  };

  driver_test().RunInEnvironmentTypeContext(
      [](TestEnvironment& env) { return env.dci_.ExpectControl(); });
  driver_test().RunInDriverContext([&req, &complete](UsbVirtualBus& driver) {
    driver.host()->UsbHciRequestQueue(req, &complete);
  });

  completion.Wait();
  usb_request_release(req);
}

TEST_F(UsbVirtualBusTest, BanjoOutRequestTest) {
  EnableAndConnect();

  size_t hci_req_size, dci_req_size;
  driver_test().RunInDriverContext([&hci_req_size](UsbVirtualBus& driver) {
    hci_req_size = driver.host()->UsbHciGetRequestSize();
  });
  driver_test().RunInDriverContext([&dci_req_size](UsbVirtualBus& driver) {
    dci_req_size = driver.device()->UsbDciGetRequestSize();
  });

  static constexpr size_t kDataSize = 256;
  const uint8_t kEpAddr = 1 | USB_DIR_OUT;
  usb_request_t *host_req, *dev_req;
  ASSERT_EQ(usb_request_alloc(&host_req, kDataSize, kEpAddr, hci_req_size), ZX_OK);
  ASSERT_EQ(usb_request_alloc(&dev_req, kDataSize, kEpAddr, dci_req_size), ZX_OK);

  // Fill host request with data
  uint8_t* data;
  ASSERT_EQ(usb_request_mmap(host_req, (void**)&data), ZX_OK);
  for (size_t i = 0; i < kDataSize; i++) {
    data[i] = static_cast<uint8_t>(i);
  }
  host_req->header.length = kDataSize;

  libsync::Completion host_completion;
  usb_request_complete_callback_t host_complete_cb = {
      .callback =
          [](void* ctx, usb_request_t* req) {
            ASSERT_EQ(req->response.status, ZX_OK);
            ASSERT_EQ(req->response.actual, kDataSize);
            static_cast<libsync::Completion*>(ctx)->Signal();
          },
      .ctx = &host_completion,
  };

  libsync::Completion dev_completion;
  usb_request_complete_callback_t dev_complete_cb = {
      .callback =
          [](void* ctx, usb_request_t* req) {
            ASSERT_EQ(req->response.status, ZX_OK);
            ASSERT_EQ(req->response.actual, kDataSize);
            // Verify data
            uint8_t* data;
            ASSERT_EQ(usb_request_mmap(req, (void**)&data), ZX_OK);
            for (size_t i = 0; i < kDataSize; i++) {
              ASSERT_EQ(data[i], static_cast<uint8_t>(i));
            }
            static_cast<libsync::Completion*>(ctx)->Signal();
          },
      .ctx = &dev_completion,
  };

  driver_test().RunInDriverContext(
      [&host_req, &dev_req, &dev_complete_cb, &host_complete_cb](UsbVirtualBus& driver) {
        // TODO: do we need a test that tests the other way around? Hci first then Dci
        // Queue the device request first, to receive the data.
        driver.device()->UsbDciRequestQueue(dev_req, &dev_complete_cb);

        // Queue the host request to send the data.
        driver.host()->UsbHciRequestQueue(host_req, &host_complete_cb);
      });

  host_completion.Wait();
  dev_completion.Wait();

  usb_request_release(host_req);
  usb_request_release(dev_req);
}

TEST_F(UsbVirtualBusTest, BanjoInRequestTest) {
  EnableAndConnect();

  size_t hci_req_size, dci_req_size;
  driver_test().RunInDriverContext([&hci_req_size](UsbVirtualBus& driver) {
    hci_req_size = driver.host()->UsbHciGetRequestSize();
  });
  driver_test().RunInDriverContext([&dci_req_size](UsbVirtualBus& driver) {
    dci_req_size = driver.device()->UsbDciGetRequestSize();
  });

  static constexpr size_t kDataSize = 256;
  const uint8_t kEpAddr = 2 | USB_DIR_OUT;
  usb_request_t *host_req, *dev_req;
  ASSERT_EQ(usb_request_alloc(&host_req, kDataSize, kEpAddr, hci_req_size), ZX_OK);
  ASSERT_EQ(usb_request_alloc(&dev_req, kDataSize, kEpAddr, dci_req_size), ZX_OK);

  // Fill host request with data
  uint8_t* data;
  ASSERT_EQ(usb_request_mmap(host_req, (void**)&data), ZX_OK);
  for (size_t i = 0; i < kDataSize; i++) {
    data[i] = static_cast<uint8_t>(i);
  }
  host_req->header.length = kDataSize;

  libsync::Completion host_completion;
  usb_request_complete_callback_t host_complete_cb = {
      .callback =
          [](void* ctx, usb_request_t* req) {
            ASSERT_EQ(req->response.status, ZX_OK);
            ASSERT_EQ(req->response.actual, kDataSize);
            // Verify data
            uint8_t* data;
            ASSERT_EQ(usb_request_mmap(req, (void**)&data), ZX_OK);
            for (size_t i = 0; i < kDataSize; i++) {
              ASSERT_EQ(data[i], static_cast<uint8_t>(i));
            }
            static_cast<libsync::Completion*>(ctx)->Signal();
          },
      .ctx = &host_completion,
  };

  libsync::Completion dev_completion;
  usb_request_complete_callback_t dev_complete_cb = {
      .callback =
          [](void* ctx, usb_request_t* req) {
            ASSERT_EQ(req->response.status, ZX_OK);
            ASSERT_EQ(req->response.actual, kDataSize);
            static_cast<libsync::Completion*>(ctx)->Signal();
          },
      .ctx = &dev_completion,
  };

  driver_test().RunInDriverContext(
      [&host_req, &dev_req, &dev_complete_cb, &host_complete_cb](UsbVirtualBus& driver) {
        // Queue the device request first, to receive the data.
        driver.device()->UsbDciRequestQueue(dev_req, &dev_complete_cb);

        // Queue the host request to send the data.
        driver.host()->UsbHciRequestQueue(host_req, &host_complete_cb);
      });

  host_completion.Wait();
  dev_completion.Wait();

  usb_request_release(host_req);
  usb_request_release(dev_req);
}

TEST_F(UsbVirtualBusTest, FidlControlRequestTest) {
  EnableAndConnect();

  fidl::SyncClient<fendpoint::Endpoint> ep_client(
      ConnectToEndpoint<fuchsia_hardware_usb_hci::UsbHciService>(0, static_cast<uint8_t>(0)));
  EndpointHandler event_handler;

  // Direct data transfer
  {
    std::vector<frequest::Request> requests;
    requests.emplace_back(std::move(frequest::Request().information(
        frequest::RequestInfo::WithControl(frequest::ControlRequestInfo().setup(
            fuchsia_hardware_usb_descriptor::UsbSetup()
                .bm_request_type(USB_DIR_IN | USB_TYPE_STANDARD | USB_RECIP_DEVICE)
                .b_request(USB_REQ_GET_DESCRIPTOR)
                .w_value(static_cast<uint16_t>(USB_DT_DEVICE << 8))
                .w_index(0)
                .w_length(sizeof(usb_device_descriptor_t)))))));

    driver_test().RunInEnvironmentTypeContext(
        [](TestEnvironment& env) { return env.dci_.ExpectControl(); });
    ASSERT_TRUE(ep_client->QueueRequests(std::move(requests)).is_ok());

    event_handler.ExpectOnCompletion(
        EndpointHandler::ExpectOnCompletionDirect(sizeof(usb_device_descriptor_t), true));
    ASSERT_TRUE(ep_client.HandleOneEvent(event_handler).ok());
  }

  // VMO data transfer
  {
    auto [data_ptr, vmo] = RegisterVmo(ep_client, sizeof(usb_device_descriptor_t));

    std::vector<frequest::Request> requests;
    std::vector<frequest::BufferRegion> buffer;
    buffer.emplace_back()
        .buffer(frequest::Buffer::WithVmoId(1))
        .offset(0)
        .size(sizeof(usb_device_descriptor_t));
    requests.emplace_back(std::move(
        frequest::Request()
            .information(frequest::RequestInfo::WithControl(frequest::ControlRequestInfo().setup(
                fuchsia_hardware_usb_descriptor::UsbSetup()
                    .bm_request_type(USB_DIR_IN | USB_TYPE_STANDARD | USB_RECIP_DEVICE)
                    .b_request(USB_REQ_GET_DESCRIPTOR)
                    .w_value(static_cast<uint16_t>(USB_DT_DEVICE << 8))
                    .w_index(0)
                    .w_length(sizeof(usb_device_descriptor_t)))))
            .data(std::move(buffer))));

    driver_test().RunInEnvironmentTypeContext(
        [](TestEnvironment& env) { return env.dci_.ExpectControl(); });
    ASSERT_TRUE(ep_client->QueueRequests(std::move(requests)).is_ok());

    event_handler.ExpectOnCompletion(
        EndpointHandler::ExpectOnCompletionVmo(sizeof(usb_device_descriptor_t), data_ptr));
    ASSERT_TRUE(ep_client.HandleOneEvent(event_handler).ok());
  }
}

TEST_F(UsbVirtualBusTest, FidlOutRequestTest) {
  EnableAndConnect();

  const uint8_t kEpAddr = 1 | USB_DIR_OUT;
  fidl::SyncClient<fendpoint::Endpoint> host_ep(
      ConnectToEndpoint<fuchsia_hardware_usb_hci::UsbHciService>(0, kEpAddr));
  EndpointHandler host_event_handler;
  fidl::SyncClient<fendpoint::Endpoint> device_ep(
      ConnectToEndpoint<fuchsia_hardware_usb_dci::UsbDciService>(kEpAddr));
  EndpointHandler device_event_handler;

  static constexpr size_t kDataSize = 256;
  auto [device_data, device_vmo] = RegisterVmo(device_ep, kDataSize);

  {
    std::vector<frequest::Request> requests;
    std::vector<frequest::BufferRegion> buffer;
    buffer.emplace_back().buffer(frequest::Buffer::WithVmoId(1)).offset(0).size(kDataSize);
    requests.emplace_back(
        std::move(frequest::Request()
                      .information(frequest::RequestInfo::WithBulk(frequest::BulkRequestInfo()))
                      .data(std::move(buffer))));

    ASSERT_TRUE(device_ep->QueueRequests(std::move(requests)).is_ok());
  }

  auto [host_data, host_vmo] = RegisterVmo(host_ep, kDataSize);
  for (size_t i = 0; i < kDataSize; i++) {
    static_cast<uint8_t*>(host_data)[i] = static_cast<uint8_t>(i);
  }

  {
    std::vector<frequest::Request> requests;
    std::vector<frequest::BufferRegion> buffer;
    buffer.emplace_back().buffer(frequest::Buffer::WithVmoId(1)).offset(0).size(kDataSize);
    requests.emplace_back(
        std::move(frequest::Request()
                      .information(frequest::RequestInfo::WithBulk(frequest::BulkRequestInfo()))
                      .data(std::move(buffer))));

    ASSERT_TRUE(host_ep->QueueRequests(std::move(requests)).is_ok());
  }

  host_event_handler.ExpectOnCompletion(EndpointHandler::ExpectOnCompletionVmo(kDataSize));
  ASSERT_TRUE(host_ep.HandleOneEvent(host_event_handler).ok());

  device_event_handler.ExpectOnCompletion(
      EndpointHandler::ExpectOnCompletionVmo(kDataSize, device_data));
  ASSERT_TRUE(device_ep.HandleOneEvent(device_event_handler).ok());
}

TEST_F(UsbVirtualBusTest, FidlInRequestTest) {
  EnableAndConnect();

  const uint8_t kEpAddr = 2 | USB_DIR_IN;
  fidl::SyncClient<fendpoint::Endpoint> host_ep(
      ConnectToEndpoint<fuchsia_hardware_usb_hci::UsbHciService>(0, kEpAddr));
  EndpointHandler host_event_handler;
  fidl::SyncClient<fendpoint::Endpoint> device_ep(
      ConnectToEndpoint<fuchsia_hardware_usb_dci::UsbDciService>(kEpAddr));
  EndpointHandler device_event_handler;

  static constexpr size_t kDataSize = 256;
  {
    std::vector<frequest::Request> requests;
    std::vector<frequest::BufferRegion> buffer;
    buffer.emplace_back()
        .buffer(frequest::Buffer::WithData(std::vector<uint8_t>(kDataSize)))
        .offset(0)
        .size(kDataSize);
    requests.emplace_back(
        std::move(frequest::Request()
                      .information(frequest::RequestInfo::WithBulk(frequest::BulkRequestInfo()))
                      .data(std::move(buffer))));

    ASSERT_TRUE(host_ep->QueueRequests(std::move(requests)).is_ok());
  }

  {
    std::vector<frequest::Request> requests;
    std::vector<frequest::BufferRegion> buffer;
    std::vector<uint8_t> data(kDataSize);
    for (size_t i = 0; i < kDataSize; i++) {
      data[i] = static_cast<uint8_t>(i);
    }
    buffer.emplace_back()
        .buffer(frequest::Buffer::WithData(std::move(data)))
        .offset(0)
        .size(kDataSize);
    requests.emplace_back(
        std::move(frequest::Request()
                      .information(frequest::RequestInfo::WithBulk(frequest::BulkRequestInfo()))
                      .data(std::move(buffer))));

    ASSERT_TRUE(device_ep->QueueRequests(std::move(requests)).is_ok());
  }

  host_event_handler.ExpectOnCompletion(EndpointHandler::ExpectOnCompletionDirect(kDataSize, true));
  ASSERT_TRUE(host_ep.HandleOneEvent(host_event_handler).ok());

  device_event_handler.ExpectOnCompletion(
      EndpointHandler::ExpectOnCompletionDirect(kDataSize, false));
  ASSERT_TRUE(device_ep.HandleOneEvent(device_event_handler).ok());
}

TEST_F(UsbVirtualBusTest, QueueControlRequestBeforeConnectTest) {
  auto enable_result = virtual_bus()->Enable();
  ASSERT_TRUE(enable_result.ok());
  ASSERT_EQ(enable_result->status, ZX_OK);

  fidl::SyncClient<fendpoint::Endpoint> ep_client(
      ConnectToEndpoint<fuchsia_hardware_usb_hci::UsbHciService>(0, static_cast<uint8_t>(0)));
  EndpointHandler event_handler;

  {
    std::vector<frequest::Request> requests;
    requests.emplace_back(std::move(frequest::Request().information(
        frequest::RequestInfo::WithControl(frequest::ControlRequestInfo().setup(
            fuchsia_hardware_usb_descriptor::UsbSetup()
                .bm_request_type(USB_DIR_IN | USB_TYPE_STANDARD | USB_RECIP_DEVICE)
                .b_request(USB_REQ_GET_DESCRIPTOR)
                .w_value(static_cast<uint16_t>(USB_DT_DEVICE << 8))
                .w_index(0)
                .w_length(sizeof(usb_device_descriptor_t)))))));

    ASSERT_TRUE(ep_client->QueueRequests(std::move(requests)).is_ok());
  }

  event_handler.ExpectOnCompletion([](fidl::Event<fendpoint::Endpoint::OnCompletion>& event) {
    ASSERT_EQ(event.completion().size(), 1UL);
    ASSERT_EQ(event.completion()[0].status(), ZX_ERR_IO_NOT_PRESENT);
    ASSERT_EQ(event.completion()[0].transfer_size(), 0);
  });
  ASSERT_TRUE(ep_client.HandleOneEvent(event_handler).ok());
}

TEST_F(UsbVirtualBusTest, QueueNormalRequestBeforeConnectedTest) {
  auto enable_result = virtual_bus()->Enable();
  ASSERT_TRUE(enable_result.ok());
  ASSERT_EQ(enable_result->status, ZX_OK);

  const uint8_t kEpAddr = 2 | USB_DIR_IN;
  fidl::SyncClient<fendpoint::Endpoint> ep_client(
      ConnectToEndpoint<fuchsia_hardware_usb_hci::UsbHciService>(0, kEpAddr));
  EndpointHandler event_handler;

  static constexpr size_t kDataSize = 256;
  {
    std::vector<frequest::Request> requests;
    std::vector<frequest::BufferRegion> buffer;
    buffer.emplace_back()
        .buffer(frequest::Buffer::WithData(std::vector<uint8_t>(kDataSize)))
        .offset(0)
        .size(kDataSize);
    requests.emplace_back(
        std::move(frequest::Request()
                      .information(frequest::RequestInfo::WithBulk(frequest::BulkRequestInfo()))
                      .data(std::move(buffer))));

    ASSERT_TRUE(ep_client->QueueRequests(std::move(requests)).is_ok());
  }

  event_handler.ExpectOnCompletion([](fidl::Event<fendpoint::Endpoint::OnCompletion>& event) {
    ASSERT_EQ(event.completion().size(), 1UL);
    ASSERT_EQ(event.completion()[0].status(), ZX_ERR_IO_NOT_PRESENT);
    ASSERT_EQ(event.completion()[0].transfer_size(), 0);
  });
  ASSERT_TRUE(ep_client.HandleOneEvent(event_handler).ok());
}

TEST_F(UsbVirtualBusTest, UnexpectedDisconnectDuringControlTest) {
  EnableAndConnect();

  fidl::SyncClient<fendpoint::Endpoint> ep_client(
      ConnectToEndpoint<fuchsia_hardware_usb_hci::UsbHciService>(0, static_cast<uint8_t>(0)));
  EndpointHandler event_handler;

  {
    std::vector<frequest::Request> requests;
    requests.emplace_back(std::move(frequest::Request().information(
        frequest::RequestInfo::WithControl(frequest::ControlRequestInfo().setup(
            fuchsia_hardware_usb_descriptor::UsbSetup()
                .bm_request_type(USB_DIR_IN | USB_TYPE_STANDARD | USB_RECIP_DEVICE)
                .b_request(USB_REQ_GET_DESCRIPTOR)
                .w_value(static_cast<uint16_t>(USB_DT_DEVICE << 8))
                .w_index(0)
                .w_length(sizeof(usb_device_descriptor_t)))))));

    ASSERT_TRUE(ep_client->QueueRequests(std::move(requests)).is_ok());
  }

  libsync::Completion* wait;
  driver_test().RunInEnvironmentTypeContext(
      [&wait](TestEnvironment& env) { wait = &env.dci_.wait_for_control(); });
  wait->Wait();

  auto result = virtual_bus()->Disconnect();
  ASSERT_TRUE(result.ok());
  ASSERT_EQ(result->status, ZX_OK);

  event_handler.ExpectOnCompletion([](fidl::Event<fendpoint::Endpoint::OnCompletion>& event) {
    ASSERT_EQ(event.completion().size(), 1UL);
    ASSERT_EQ(event.completion()[0].status(), ZX_ERR_IO_NOT_PRESENT);
    ASSERT_EQ(event.completion()[0].transfer_size(), 0);
  });
  ASSERT_TRUE(ep_client.HandleOneEvent(event_handler).ok());
}

TEST_F(UsbVirtualBusTest, UnexpectedDisconnectDuringHostNormalTest) {
  EnableAndConnect();

  const uint8_t kEpAddr = 2 | USB_DIR_IN;
  fidl::SyncClient<fendpoint::Endpoint> ep_client(
      ConnectToEndpoint<fuchsia_hardware_usb_hci::UsbHciService>(0, kEpAddr));
  EndpointHandler event_handler;

  static constexpr size_t kDataSize = 256;
  {
    std::vector<frequest::Request> requests;
    std::vector<frequest::BufferRegion> buffer;
    buffer.emplace_back()
        .buffer(frequest::Buffer::WithData(std::vector<uint8_t>(kDataSize)))
        .offset(0)
        .size(kDataSize);
    requests.emplace_back(
        std::move(frequest::Request()
                      .information(frequest::RequestInfo::WithBulk(frequest::BulkRequestInfo()))
                      .data(std::move(buffer))));

    ASSERT_TRUE(ep_client->QueueRequests(std::move(requests)).is_ok());
  }
  // Ensure that QueueRequests has finished.
  ep_client->GetInfo();

  auto result = virtual_bus()->Disconnect();
  ASSERT_TRUE(result.ok());
  ASSERT_EQ(result->status, ZX_OK);

  event_handler.ExpectOnCompletion([](fidl::Event<fendpoint::Endpoint::OnCompletion>& event) {
    ASSERT_EQ(event.completion().size(), 1UL);
    ASSERT_EQ(event.completion()[0].status(), ZX_ERR_IO_NOT_PRESENT);
    ASSERT_EQ(event.completion()[0].transfer_size(), 0);
  });
  ASSERT_TRUE(ep_client.HandleOneEvent(event_handler).ok());
}

TEST_F(UsbVirtualBusTest, UnexpectedDisconnectDuringDeviceNormalTest) {
  EnableAndConnect();

  const uint8_t kEpAddr = 2 | USB_DIR_IN;
  fidl::SyncClient<fendpoint::Endpoint> ep_client(
      ConnectToEndpoint<fuchsia_hardware_usb_dci::UsbDciService>(kEpAddr));
  EndpointHandler event_handler;

  static constexpr size_t kDataSize = 256;
  {
    std::vector<frequest::Request> requests;
    std::vector<frequest::BufferRegion> buffer;
    buffer.emplace_back()
        .buffer(frequest::Buffer::WithData(std::vector<uint8_t>(kDataSize)))
        .offset(0)
        .size(kDataSize);
    requests.emplace_back(
        std::move(frequest::Request()
                      .information(frequest::RequestInfo::WithBulk(frequest::BulkRequestInfo()))
                      .data(std::move(buffer))));

    ASSERT_TRUE(ep_client->QueueRequests(std::move(requests)).is_ok());
  }
  // Ensure that QueueRequests has finished.
  ep_client->GetInfo();

  auto result = virtual_bus()->Disconnect();
  ASSERT_TRUE(result.ok());
  ASSERT_EQ(result->status, ZX_OK);

  event_handler.ExpectOnCompletion([](fidl::Event<fendpoint::Endpoint::OnCompletion>& event) {
    ASSERT_EQ(event.completion().size(), 1UL);
    ASSERT_EQ(event.completion()[0].status(), ZX_ERR_IO_NOT_PRESENT);
    ASSERT_EQ(event.completion()[0].transfer_size(), 0);
  });
  ASSERT_TRUE(ep_client.HandleOneEvent(event_handler).ok());
}

}  // namespace usb_virtual_bus
