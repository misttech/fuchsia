// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "usb-cdc-function.h"

#include <fidl/fuchsia.boot.metadata/cpp/fidl.h>
#include <fidl/fuchsia.hardware.network.driver/cpp/driver/fidl.h>
#include <fidl/fuchsia.hardware.network.driver/cpp/driver/wire_test_base.h>
#include <fidl/fuchsia.hardware.network.driver/cpp/fidl.h>
#include <fidl/fuchsia.hardware.network/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.endpoint/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.function/cpp/fidl.h>
#include <fuchsia/hardware/usb/function/cpp/banjo-mock.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async/cpp/task.h>
#include <lib/async/default.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/driver/compat/cpp/banjo_server.h>
#include <lib/driver/compat/cpp/device_server.h>
#include <lib/driver/metadata/cpp/metadata_server.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/sync/cpp/completion.h>

#include <zxtest/zxtest.h>

#include "src/devices/usb/lib/usb-endpoint/testing/fake-usb-endpoint-server.h"

bool operator==(const usb_request_complete_callback_t& lhs,
                const usb_request_complete_callback_t& rhs) {
  // Comparison of these struct is not useful. Return true always.
  return true;
}

bool operator==(const usb_ss_ep_comp_descriptor_t& lhs, const usb_ss_ep_comp_descriptor_t& rhs) {
  // Comparison of these struct is not useful. Return true always.
  return true;
}

bool operator==(const usb_endpoint_descriptor_t& lhs, const usb_endpoint_descriptor_t& rhs) {
  // Comparison of these struct is not useful. Return true always.
  return true;
}

bool operator==(const usb_request_t& lhs, const usb_request_t& rhs) {
  // Only comparing endpoint address. Use ExpectCallWithMatcher for more specific
  // comparisons.
  return lhs.header.ep_address == rhs.header.ep_address;
}

bool operator==(const usb_function_interface_protocol_t& lhs,
                const usb_function_interface_protocol_t& rhs) {
  // Comparison of these struct is not useful. Return true always.
  return true;
}

namespace usb_cdc_function {
namespace {

constexpr uint32_t kBulkOutEp = 1;
constexpr uint32_t kBulkInEp = 2;
constexpr uint32_t kIntrEp = 3;
constexpr uint8_t kCommInterface = 0;
constexpr uint8_t kDataInterface = 1;

class MockUsbFunction : public ddk::MockUsbFunction {
 public:
  compat::DeviceServer::BanjoConfig GetBanjoConfig() {
    compat::DeviceServer::BanjoConfig config{.default_proto_id = ZX_PROTOCOL_USB_FUNCTION};
    config.callbacks[ZX_PROTOCOL_USB_FUNCTION] = banjo_server_.callback();
    return config;
  }

  zx_status_t UsbFunctionSetInterface(const usb_function_interface_protocol_t* interface) override {
    interface_ = {interface};
    return ddk::MockUsbFunction::UsbFunctionSetInterface(interface);
  }

  // Knock out of expectations because the driver calls this with a nullptr,
  // which trips the generated mock.
  zx_status_t UsbFunctionConfigEp(const usb_endpoint_descriptor_t* ep_desc,
                                  const usb_ss_ep_comp_descriptor_t* ss_comp_desc) override {
    return ZX_OK;
  }

  ddk::UsbFunctionInterfaceProtocolClient& interface() { return interface_; }

 private:
  compat::BanjoServer banjo_server_{ZX_PROTOCOL_USB_FUNCTION, this, GetProto()->ops};
  ddk::UsbFunctionInterfaceProtocolClient interface_;
};

class FakeNetworkDeviceIfc : public fidl::testing::WireTestBase<fnetdev::NetworkDeviceIfc> {
 public:
  FakeNetworkDeviceIfc() = default;

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    ADD_FAILURE("FakeNetworkDeviceIfc not implemented: %s", name.c_str());
    if (completer.is_reply_needed()) {
      completer.Close(ZX_ERR_NOT_SUPPORTED);
    }
  }

  void AddPort(fnetdev::wire::NetworkDeviceIfcAddPortRequest* request, fdf::Arena& arena,
               AddPortCompleter::Sync& completer) override {
    port_id_ = request->id;
    port_ = std::move(request->port);
    completer.ToAsync().buffer(arena).Reply(ZX_OK);
    if (on_add_port_) {
      on_add_port_();
    }
  }

  void CompleteRx(fnetdev::wire::NetworkDeviceIfcCompleteRxRequest* request, fdf::Arena& arena,
                  CompleteRxCompleter::Sync& completer) override {
    for (auto& result : request->rx) {
      completed_rx_.push(fidl::ToNatural(result));
    }
    if (on_complete_rx_) {
      on_complete_rx_();
    }
  }

  void CompleteTx(fnetdev::wire::NetworkDeviceIfcCompleteTxRequest* request, fdf::Arena& arena,
                  CompleteTxCompleter::Sync& completer) override {
    for (const auto& result : request->tx) {
      completed_tx_.push(result);
    }
    if (on_complete_tx_)
      on_complete_tx_();
  }

  void PortStatusChanged(fnetdev::wire::NetworkDeviceIfcPortStatusChangedRequest* request,
                         fdf::Arena& arena, PortStatusChangedCompleter::Sync& completer) override {}

  bool HasPort() { return port_.is_valid(); }

  fdf::ClientEnd<fnetdev::NetworkPort> TakePort() { return std::move(port_); }

  void set_on_add_port(fit::function<void()> callback) { on_add_port_ = std::move(callback); }
  void set_on_complete_tx(fit::function<void()> callback) { on_complete_tx_ = std::move(callback); }
  void set_on_complete_rx(fit::function<void()> callback) { on_complete_rx_ = std::move(callback); }

  std::optional<fnetdev::wire::TxResult> PopCompleteTx() {
    if (completed_tx_.empty()) {
      return std::nullopt;
    }
    auto tx = completed_tx_.front();
    completed_tx_.pop();
    return tx;
  }
  std::optional<fnetdev::RxBuffer> PopCompleteRx() {
    if (completed_rx_.empty()) {
      return std::nullopt;
    }
    auto rx = completed_rx_.front();
    completed_rx_.pop();
    return rx;
  }

 private:
  uint8_t port_id_;
  fdf::ClientEnd<fnetdev::NetworkPort> port_;
  fit::function<void()> on_add_port_;
  fit::function<void()> on_complete_tx_;
  fit::function<void()> on_complete_rx_;
  std::queue<fnetdev::wire::TxResult> completed_tx_;
  std::queue<fnetdev::RxBuffer> completed_rx_;
};

class Environment : public fdf_testing::Environment {
 public:
  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    async_dispatcher_t* dispatcher = fdf::Dispatcher::GetCurrent()->async_dispatcher();

    device_server_.Initialize("default", std::nullopt, mock_usb_.GetBanjoConfig());
    zx_status_t status = device_server_.Serve(dispatcher, &to_driver_vfs);
    if (status != ZX_OK) {
      return zx::error(status);
    }

    fuchsia_hardware_usb_function::UsbFunctionService::InstanceHandler handler({
        .device = usb_function_bindings_.CreateHandler(&fake_usb_fidl_, dispatcher,
                                                       fidl::kIgnoreBindingClosure),
    });

    if (zx::result result = metadata_server_.Serve(
            to_driver_vfs, fdf::Dispatcher::GetCurrent()->async_dispatcher());
        result.is_error()) {
      return result.take_error();
    }

    if (zx::result result =
            to_driver_vfs.AddService<fuchsia_hardware_usb_function::UsbFunctionService>(
                std::move(handler));
        result.is_error()) {
      return result.take_error();
    }

    return zx::ok();
  }

  using FakeUsbFidl =
      fake_usb_endpoint::FakeUsbFidlProvider<fuchsia_hardware_usb_function::UsbFunction,
                                             fake_usb_endpoint::FakeEndpoint>;

  compat::DeviceServer device_server_;
  MockUsbFunction mock_usb_;
  FakeUsbFidl fake_usb_fidl_{fdf::Dispatcher::GetCurrent()->async_dispatcher()};
  fidl::ServerBindingGroup<fuchsia_hardware_usb_function::UsbFunction> usb_function_bindings_;
  fdf_metadata::MetadataServer<fuchsia_boot_metadata::MacAddressMetadata> metadata_server_;
  FakeNetworkDeviceIfc fake_ifc_;
};

class UsbCdcTestConfig final {
 public:
  using DriverType = UsbCdcFunction;
  using EnvironmentType = Environment;
};

class UsbCdcTest : public zxtest::Test {
 public:
  static constexpr fdf_arena_tag_t kArenaTag = 'TEST';
  static constexpr std::array<uint8_t, 6> kTestMac = {0, 1, 2, 3, 4, 5};

  void SetUp() override {
    auto endpoints = fdf::CreateEndpoints<fnetdev::NetworkDeviceIfc>();
    sync_completion_t port_ready;
    driver_test_.RunInEnvironmentTypeContext(
        [server = std::move(endpoints->server), &port_ready](Environment& env) mutable {
          EXPECT_OK(env.metadata_server_.SetMetadata({{.mac_address = {{{.octets = kTestMac}}}}}));
          env.mock_usb_.ExpectAllocInterface(ZX_OK, kCommInterface);  // comm
          env.mock_usb_.ExpectAllocInterface(ZX_OK, kDataInterface);  // data
          env.mock_usb_.ExpectAllocEp(ZX_OK, USB_DIR_OUT, kBulkOutEp);
          env.mock_usb_.ExpectAllocEp(ZX_OK, USB_DIR_IN, kBulkInEp);
          env.mock_usb_.ExpectAllocEp(ZX_OK, USB_DIR_IN, kIntrEp);
          env.mock_usb_.ExpectAllocStringDesc(ZX_OK, "000102030405", 1);
          env.fake_usb_fidl_.ExpectConnectToEndpoint(kBulkOutEp);
          env.fake_usb_fidl_.ExpectConnectToEndpoint(kBulkInEp);
          env.fake_usb_fidl_.ExpectConnectToEndpoint(kIntrEp);
          env.mock_usb_.ExpectSetInterface(ZX_OK, {});
          fdf::BindServer(fdf::Dispatcher::GetCurrent()->get(), std::move(server), &env.fake_ifc_);
          env.fake_ifc_.set_on_add_port([&port_ready]() { sync_completion_signal(&port_ready); });
        });

    ASSERT_OK(driver_test_.StartDriver().status_value());

    // Connect to the driver
    auto connect_result = driver_test_.Connect<fnetdev::Service::NetworkDeviceImpl>();
    ASSERT_OK(connect_result.status_value());

    fdf::Arena arena(kArenaTag);
    net_impl_client_.Bind(std::move(connect_result.value()));
    auto init_result = net_impl_client_.buffer(arena)->Init(std::move(endpoints->client));
    ASSERT_OK(init_result.status());
    ASSERT_OK(init_result->s);

    sync_completion_wait(&port_ready, ZX_TIME_INFINITE);

    driver_test_.RunInEnvironmentTypeContext([this](Environment& env) {
      EXPECT_TRUE(env.fake_ifc_.HasPort());
      net_port_client_.Bind(env.fake_ifc_.TakePort());
    });
  }

  void TearDown() override {
    driver_test_.RunInEnvironmentTypeContext([](Environment& env) {
      env.mock_usb_.ExpectDisableEp(ZX_OK, kBulkOutEp);
      env.mock_usb_.ExpectDisableEp(ZX_OK, kBulkInEp);
      env.mock_usb_.ExpectDisableEp(ZX_OK, kIntrEp);
      env.mock_usb_.ExpectSetInterface(ZX_OK, {});
    });

    ASSERT_OK(driver_test_.StopDriver().status_value());
    driver_test_.RunInEnvironmentTypeContext(
        [](Environment& env) { env.mock_usb_.VerifyAndClear(); });
  }

  void StartNetworkDevice() {
    fdf::Arena arena(kArenaTag);
    auto start_result = net_impl_client_.buffer(arena)->Start();
    ASSERT_OK(start_result.status());
    ASSERT_OK(start_result->s);
  }

  void SetConfiguredAndEnable() {
    driver_test_.RunInEnvironmentTypeContext([](Environment& env) {
      // Starting will cause 2 notifications in the interrupt ep.
      env.fake_usb_fidl_.fake_endpoint(kIntrEp).RequestComplete(ZX_OK, 0);
      env.fake_usb_fidl_.fake_endpoint(kIntrEp).RequestComplete(ZX_OK, 0);
      ASSERT_TRUE(env.mock_usb_.interface().is_valid());
      ASSERT_OK(env.mock_usb_.interface().SetConfigured(true, USB_SPEED_HIGH));
      ASSERT_OK(env.mock_usb_.interface().SetInterface(kDataInterface, 1));
    });
  }

 protected:
  fdf_testing::BackgroundDriverTest<UsbCdcTestConfig> driver_test_;
  fdf::WireSyncClient<fnetdev::NetworkDeviceImpl> net_impl_client_;
  fdf::WireSyncClient<fnetdev::NetworkPort> net_port_client_;
};

TEST_F(UsbCdcTest, GetInfo) {
  fdf::Arena arena(kArenaTag);
  auto result = net_impl_client_.buffer(arena)->GetInfo();
  ASSERT_OK(result.status());
  EXPECT_EQ(result->info.tx_depth(), UsbCdcFunction::kTxDepth);
  EXPECT_EQ(result->info.rx_depth(), UsbCdcFunction::kRxDepth);
}

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
  EXPECT_BYTES_EQ(mac_result->mac.octets.data(), expected_mac.data(), expected_mac.size());
}

TEST_F(UsbCdcTest, TxFailsIfOffline) {
  StartNetworkDevice();

  constexpr uint8_t kVmoId = 1;
  constexpr uint32_t kBufferId = 100;

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(4096, 0, &vmo));
  uint8_t data[] = {0xAA, 0xBB, 0xCC, 0xDD};
  ASSERT_OK(vmo.write(data, 0, sizeof(data)));
  fdf::Arena arena(kArenaTag);
  auto prepare_result = net_impl_client_.buffer(arena)->PrepareVmo(kVmoId, std::move(vmo));
  ASSERT_OK(prepare_result.status());
  ASSERT_OK(prepare_result->s);

  fnetdev::wire::BufferRegion region = {.vmo = kVmoId, .offset = 0, .length = sizeof(data)};
  fnetdev::wire::TxBuffer buffer = {
      .id = kBufferId,
      .data = fidl::VectorView<fnetdev::wire::BufferRegion>::FromExternal(&region, 1),
  };

  sync_completion_t tx_completed;
  driver_test_.RunInEnvironmentTypeContext([&](Environment& env) {
    env.fake_ifc_.set_on_complete_tx([&]() { sync_completion_signal(&tx_completed); });
  });
  ASSERT_OK(net_impl_client_.buffer(arena)
                ->QueueTx(fidl::VectorView<fnetdev::wire::TxBuffer>::FromExternal(&buffer, 1))
                .status());

  sync_completion_wait(&tx_completed, ZX_TIME_INFINITE);

  driver_test_.RunInEnvironmentTypeContext([&](Environment& env) {
    auto buffer = env.fake_ifc_.PopCompleteTx();
    ASSERT_TRUE(buffer.has_value());
    EXPECT_EQ(buffer->id, kBufferId);
    EXPECT_STATUS(buffer->status, ZX_ERR_BAD_STATE);
  });
}

TEST_F(UsbCdcTest, QueueTx) {
  StartNetworkDevice();
  ASSERT_NO_FATAL_FAILURE(SetConfiguredAndEnable());

  constexpr uint8_t kVmoId = 1;
  constexpr uint32_t kBufferId = 100;

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(4096, 0, &vmo));
  uint8_t data[] = {0xAA, 0xBB, 0xCC, 0xDD};
  ASSERT_OK(vmo.write(data, 0, sizeof(data)));
  fdf::Arena arena(kArenaTag);
  auto prepare_result = net_impl_client_.buffer(arena)->PrepareVmo(kVmoId, std::move(vmo));
  ASSERT_OK(prepare_result.status());
  ASSERT_OK(prepare_result->s);

  fnetdev::wire::BufferRegion region = {.vmo = kVmoId, .offset = 0, .length = sizeof(data)};
  fnetdev::wire::TxBuffer buffer = {
      .id = kBufferId,
      .data = fidl::VectorView<fnetdev::wire::BufferRegion>::FromExternal(&region, 1),
  };

  sync_completion_t tx_completed;
  driver_test_.RunInEnvironmentTypeContext([&](Environment& env) {
    env.fake_ifc_.set_on_complete_tx([&]() { sync_completion_signal(&tx_completed); });
  });
  ASSERT_OK(net_impl_client_.buffer(arena)
                ->QueueTx(fidl::VectorView<fnetdev::wire::TxBuffer>::FromExternal(&buffer, 1))
                .status());

  EXPECT_STATUS(sync_completion_wait(&tx_completed, ZX_TIME_INFINITE_PAST), ZX_ERR_TIMED_OUT);
  driver_test_.RunInEnvironmentTypeContext([](Environment& env) {
    env.fake_usb_fidl_.fake_endpoint(kBulkInEp).RequestComplete(ZX_OK, sizeof(data));
  });

  sync_completion_wait(&tx_completed, ZX_TIME_INFINITE);

  driver_test_.RunInEnvironmentTypeContext([&](Environment& env) {
    auto buffer = env.fake_ifc_.PopCompleteTx();
    ASSERT_TRUE(buffer.has_value());
    EXPECT_EQ(buffer->id, kBufferId);
    EXPECT_OK(buffer->status);
  });
}

TEST_F(UsbCdcTest, Rx) {
  StartNetworkDevice();
  ASSERT_NO_FATAL_FAILURE(SetConfiguredAndEnable());

  sync_completion_t rx_completed;
  driver_test_.RunInEnvironmentTypeContext([&](Environment& env) {
    env.fake_ifc_.set_on_complete_rx([&]() { sync_completion_signal(&rx_completed); });
  });

  constexpr uint8_t kVmoId = 1;
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

  EXPECT_STATUS(sync_completion_wait(&rx_completed, ZX_TIME_INFINITE_PAST), ZX_ERR_TIMED_OUT);

  // Complete 2 requests, but we only have one rx space already queued.
  // The pending request can be completed later.
  driver_test_.RunInEnvironmentTypeContext([](Environment& env) {
    env.fake_usb_fidl_.fake_endpoint(kBulkOutEp).RequestComplete(ZX_OK, kDataSize1);
    env.fake_usb_fidl_.fake_endpoint(kBulkOutEp).RequestComplete(ZX_OK, kDataSize2);
  });

  sync_completion_wait(&rx_completed, ZX_TIME_INFINITE);

  driver_test_.RunInEnvironmentTypeContext([&](Environment& env) {
    auto buffer = env.fake_ifc_.PopCompleteRx();
    ASSERT_TRUE(buffer.has_value());
    EXPECT_EQ(buffer->meta().port(), UsbCdcFunction::kPortId);
    EXPECT_EQ(buffer->meta().frame_type(), fuchsia_hardware_network::wire::FrameType::kEthernet);
    ASSERT_EQ(buffer->data().size(), 1u);
    auto& data = buffer->data()[0];
    EXPECT_EQ(data.offset(), 0);
    EXPECT_EQ(data.length(), kDataSize1);
    EXPECT_EQ(data.id(), kBufferId1);
  });

  sync_completion_reset(&rx_completed);

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

  sync_completion_wait(&rx_completed, ZX_TIME_INFINITE);

  driver_test_.RunInEnvironmentTypeContext([&](Environment& env) {
    auto buffer = env.fake_ifc_.PopCompleteRx();
    ASSERT_TRUE(buffer.has_value());
    EXPECT_EQ(buffer->meta().port(), UsbCdcFunction::kPortId);
    EXPECT_EQ(buffer->meta().frame_type(), fuchsia_hardware_network::wire::FrameType::kEthernet);
    ASSERT_EQ(buffer->data().size(), 1u);
    auto& data = buffer->data()[0];
    EXPECT_EQ(data.offset(), 0);
    EXPECT_EQ(data.length(), kDataSize2);
    EXPECT_EQ(data.id(), kBufferId2);
  });
}

TEST_F(UsbCdcTest, Stop) {
  StartNetworkDevice();
  ASSERT_NO_FATAL_FAILURE(SetConfiguredAndEnable());

  constexpr uint8_t kVmoId = 1;
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

  sync_completion_t rx_completed, tx_completed;
  driver_test_.RunInEnvironmentTypeContext([&](Environment& env) {
    env.fake_ifc_.set_on_complete_rx([&]() { sync_completion_signal(&rx_completed); });
    env.fake_ifc_.set_on_complete_tx([&]() { sync_completion_signal(&tx_completed); });
  });

  fnetdev::wire::BufferRegion region = {.vmo = kVmoId, .offset = 0, .length = 2048};
  fnetdev::wire::TxBuffer tx_buffer = {
      .id = kTxBufferId,
      .data = fidl::VectorView<fnetdev::wire::BufferRegion>::FromExternal(&region, 1),
  };

  ASSERT_OK(net_impl_client_.buffer(arena)
                ->QueueTx(fidl::VectorView<fnetdev::wire::TxBuffer>::FromExternal(&tx_buffer, 1))
                .status());

  auto result = net_impl_client_.buffer(arena)->Stop();
  ASSERT_OK(result.status());

  sync_completion_wait(&rx_completed, ZX_TIME_INFINITE);
  sync_completion_wait(&tx_completed, ZX_TIME_INFINITE);

  // All in flight transactions should complete on stop.
  driver_test_.RunInEnvironmentTypeContext([&](Environment& env) {
    auto rx = env.fake_ifc_.PopCompleteRx();
    ASSERT_TRUE(rx.has_value());
    ASSERT_EQ(rx->data().size(), 1u);
    EXPECT_EQ(rx->data()[0].id(), kRxBufferId);
    auto tx = env.fake_ifc_.PopCompleteTx();
    ASSERT_TRUE(tx.has_value());
    EXPECT_EQ(tx->id, kTxBufferId);
  });
}

TEST_F(UsbCdcTest, TeardownWithPendingRxCompletion) {
  StartNetworkDevice();
  ASSERT_NO_FATAL_FAILURE(SetConfiguredAndEnable());
  driver_test_.RunInEnvironmentTypeContext([](Environment& env) {
    env.fake_usb_fidl_.fake_endpoint(kBulkOutEp).RequestComplete(ZX_OK, 123);
  });

  bool ready = false;
  while (!ready) {
    driver_test_.RunInDriverContext([&ready](UsbCdcFunction& driver) {
      // Check that we're testing for the right thing.
      ready = driver.HasPendingRxCompletions();
    });
    driver_test_.runtime().RunUntilIdle();
  }

  // Bulk of verification happens on test teardown as part of stopping the
  // driver.
}

}  // namespace
}  // namespace usb_cdc_function
