// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.wlan.phyimpl/cpp/driver/wire.h>
#include <fidl/fuchsia.wlan.phyimpl/cpp/wire_types.h>
#include <fuchsia/wlan/common/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async/cpp/task.h>
#include <lib/async/default.h>
#include <lib/async_patterns/testing/cpp/dispatcher_bound.h>
#include <lib/component/incoming/cpp/service.h>
#include <lib/driver/incoming/cpp/namespace.h>
#include <lib/driver/outgoing/cpp/outgoing_directory.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/fdf/testing.h>
#include <lib/fidl/cpp/binding.h>
#include <lib/fidl/cpp/decoder.h>
#include <lib/fidl/cpp/message.h>
#include <lib/sync/cpp/completion.h>
#include <lib/sys/cpp/testing/component_context_provider.h>
#include <netinet/if_ether.h>
#include <zircon/errors.h>

#include <gtest/gtest.h>

#include "src/connectivity/wlan/drivers/wlanphy/device.h"
#include "src/devices/bin/driver_runtime/dispatcher.h"

namespace wlanphy {
namespace {

// This test class provides the fake upper layer(wlandevicemonitor) and lower layer(wlanphyimpl
// device) for running wlanphy device:
//    |
//    |                              +--------------------+
//    |                             /|  wlandevicemonitor |
//    |                            / +--------------------+
//    |                           /           /|\
//    |                          /             | <---- [Normal FIDL with protocol:
//    |                         /             \|/               fuchsia_wlan_device::Phy]
//    |             Both faked           +-----+-----+
//    |            in this test          |  wlanphy  |   <---- Test target
//    |        class(WlanDeviceTest)     |  device   |
//    |                         \        +-----+-----+
//    |                          \         /|\    |
//    |                           \         |     |
//    |  [FIDL transport with ------------->|     |<---- [Driver transport FIDL with protocol:
//    |  protocol:                  \       |     |          fuchsia_wlan_phyimpl::WlanPhyImpl]
//    |  fuchsia_wlan_phyimpl::      \      |    \|/
//    |  WlanPhyImplNotify]           \ +---------------+
//    |                                \| wlanphyimpl   |
//    |                                 |    device     |
//    |                                 +---------------+
//    |
class FakeWlanPhyImpl : public fdf::WireServer<fuchsia_wlan_phyimpl::WlanPhyImpl> {
 public:
  FakeWlanPhyImpl() {
    // Initialize struct to avoid random values.
    memset(static_cast<void*>(&create_iface_req_), 0, sizeof(create_iface_req_));
  }

  ~FakeWlanPhyImpl() {}

  void ServiceConnectHandler(fdf::ServerEnd<fuchsia_wlan_phyimpl::WlanPhyImpl> server_end) {
    fdf::BindServer(fdf_dispatcher_get_current_dispatcher(), std::move(server_end), this);
  }

  void Init(InitRequestView request, fdf::Arena& arena, InitCompleter::Sync& completer) override {
    if (!request->has_notify_client()) {
      completer.buffer(arena).ReplyError(ZX_ERR_BAD_HANDLE);
      return;
    }
    phyimpl_notify_client_.Bind(std::move(request->notify_client()));
    completer.buffer(arena).ReplySuccess();
    wait_for_notify_client_.Signal();
  }
  // Server end handler functions for fuchsia_wlan_phyimpl::WlanPhyImpl.
  void GetSupportedMacRoles(fdf::Arena& arena,
                            GetSupportedMacRolesCompleter::Sync& completer) override {
    std::vector<fuchsia_wlan_common::wire::WlanMacRole> supported_mac_roles_vec;
    supported_mac_roles_vec.push_back(kFakeMacRole);
    auto supported_mac_roles =
        fidl::VectorView<fuchsia_wlan_common::wire::WlanMacRole>::FromExternal(
            supported_mac_roles_vec);
    fidl::Arena fidl_arena;
    auto builder =
        fuchsia_wlan_phyimpl::wire::WlanPhyImplGetSupportedMacRolesResponse::Builder(fidl_arena);
    builder.supported_mac_roles(supported_mac_roles);
    completer.buffer(arena).ReplySuccess(builder.Build());
    test_completion_.Signal();
  }
  void CreateIface(CreateIfaceRequestView request, fdf::Arena& arena,
                   CreateIfaceCompleter::Sync& completer) override {
    has_init_sta_addr_ = false;
    if (request->has_init_sta_addr()) {
      create_iface_req_.init_sta_addr = request->init_sta_addr();
      has_init_sta_addr_ = true;
    }
    if (request->has_role()) {
      create_iface_req_.role = request->role();
    }

    fidl::Arena fidl_arena;
    auto builder = fuchsia_wlan_phyimpl::wire::WlanPhyImplCreateIfaceResponse::Builder(fidl_arena);
    builder.iface_id(kFakeIfaceId);
    completer.buffer(arena).ReplySuccess(builder.Build());
    test_completion_.Signal();
  }
  void DestroyIface(DestroyIfaceRequestView request, fdf::Arena& arena,
                    DestroyIfaceCompleter::Sync& completer) override {
    destroy_iface_id_ = request->iface_id();
    completer.buffer(arena).ReplySuccess();
    test_completion_.Signal();
  }
  void SetCountry(SetCountryRequestView request, fdf::Arena& arena,
                  SetCountryCompleter::Sync& completer) override {
    country_ = *request;
    completer.buffer(arena).ReplySuccess();
    test_completion_.Signal();
  }
  void ClearCountry(fdf::Arena& arena, ClearCountryCompleter::Sync& completer) override {
    completer.buffer(arena).ReplySuccess();
    test_completion_.Signal();
  }
  void GetCountry(fdf::Arena& arena, GetCountryCompleter::Sync& completer) override {
    auto country = fuchsia_wlan_phyimpl::wire::WlanPhyCountry::WithAlpha2(kAlpha2);
    completer.buffer(arena).ReplySuccess(country);
    test_completion_.Signal();
  }
  void SetPowerSaveMode(SetPowerSaveModeRequestView request, fdf::Arena& arena,
                        SetPowerSaveModeCompleter::Sync& completer) override {
    ps_mode_ = request->ps_mode();
    completer.buffer(arena).ReplySuccess();
    test_completion_.Signal();
  }
  void GetPowerSaveMode(fdf::Arena& arena, GetPowerSaveModeCompleter::Sync& completer) override {
    fidl::Arena fidl_arena;
    auto builder =
        fuchsia_wlan_phyimpl::wire::WlanPhyImplGetPowerSaveModeResponse::Builder(fidl_arena);
    builder.ps_mode(kFakePsMode);

    completer.buffer(arena).ReplySuccess(builder.Build());
    test_completion_.Signal();
  }
  void PowerDown(fdf::Arena& arena, PowerDownCompleter::Sync& completer) override {
    completer.buffer(arena).ReplySuccess();
    test_completion_.Signal();
  }
  void PowerUp(fdf::Arena& arena, PowerUpCompleter::Sync& completer) override {
    completer.buffer(arena).ReplySuccess();
    test_completion_.Signal();
  }
  void Reset(fdf::Arena& arena, ResetCompleter::Sync& completer) override {
    completer.buffer(arena).ReplySuccess();
    test_completion_.Signal();
  }
  void GetPowerState(fdf::Arena& arena, GetPowerStateCompleter::Sync& completer) override {
    fidl::Arena fidl_arena;
    auto builder =
        fuchsia_wlan_phyimpl::wire::WlanPhyImplGetPowerStateResponse::Builder(fidl_arena);
    builder.power_on(true);
    completer.buffer(arena).ReplySuccess(builder.Build());
    test_completion_.Signal();
  }
  void SetBtCoexistenceMode(SetBtCoexistenceModeRequestView request, fdf::Arena& arena,
                            SetBtCoexistenceModeCompleter::Sync& completer) override {
    bt_coex_mode_ = request->mode();
    completer.buffer(arena).ReplySuccess();
    test_completion_.Signal();
  }
  void SetTxPowerScenario(SetTxPowerScenarioRequestView request, fdf::Arena& arena,
                          SetTxPowerScenarioCompleter::Sync& completer) override {
    tx_power_scenario_ = request->scenario();
    completer.buffer(arena).ReplySuccess();
    test_completion_.Signal();
  }
  void ResetTxPowerScenario(fdf::Arena& arena,
                            ResetTxPowerScenarioCompleter::Sync& completer) override {
    tx_power_scenario_ = fuchsia_wlan_phyimpl::wire::TxPowerScenario::kDefault;
    completer.buffer(arena).ReplySuccess();
    test_completion_.Signal();
  }
  void GetTxPowerScenario(fdf::Arena& arena,
                          GetTxPowerScenarioCompleter::Sync& completer) override {
    completer.buffer(arena).ReplySuccess(tx_power_scenario_);
    test_completion_.Signal();
  }

  zx_status_t SendCriticalErrorEvent(fuchsia_wlan_phyimpl::CriticalErrorReason reason) {
    fidl::Arena fidl_arena;

    auto builder =
        fuchsia_wlan_phyimpl::wire::WlanPhyImplNotifyOnCriticalErrorRequest::Builder(fidl_arena);
    builder.reason_code(reason);
    auto result = phyimpl_notify_client_.buffer(fidl_arena)->OnCriticalError(builder.Build());
    if (!result.ok()) {
      return ZX_ERR_INTERNAL;
    }
    if (result->is_error()) {
      auto status = result->error_value();
      if (status == fuchsia_wlan_phyimpl::WlanPhyImplNotifyError::kShouldWait) {
        return ZX_ERR_SHOULD_WAIT;
      }
      return ZX_ERR_INTERNAL;
    }
    return ZX_OK;
  }

  zx_status_t SendCountryCodeEvent(std::array<uint8_t, 2> country_code) {
    fidl::Arena fidl_arena;

    auto builder = fuchsia_wlan_phyimpl::wire::WlanPhyImplNotifyOnCountryCodeChangeRequest::Builder(
        fidl_arena);
    fidl::Array<uint8_t, 2> fidl_country;
    std::copy(country_code.begin(), country_code.end(), fidl_country.begin());
    auto wire_country = fuchsia_wlan_phyimpl::wire::WlanPhyCountry::WithAlpha2(fidl_country);
    builder.phy_country(wire_country);
    auto result = phyimpl_notify_client_.buffer(fidl_arena)->OnCountryCodeChange(builder.Build());
    if (!result.ok()) {
      return ZX_ERR_INTERNAL;
    }
    if (result->is_error()) {
      auto status = result->error_value();
      if (status == fuchsia_wlan_phyimpl::WlanPhyImplNotifyError::kShouldWait) {
        return ZX_ERR_SHOULD_WAIT;
      }
      return ZX_ERR_INTERNAL;
    }
    return ZX_OK;
  }

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_wlan_phyimpl::WlanPhyImpl> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override {}

  void WaitForCompletion() { test_completion_.Wait(); }
  void WaitForNotifyClient() { wait_for_notify_client_.Wait(); }

  bool HasInitStaAddr() { return has_init_sta_addr_; }
  fuchsia_wlan_device::wire::CreateIfaceRequest& GetIfaceReq() { return create_iface_req_; }
  uint16_t GetDestroyIfaceId() { return destroy_iface_id_; }
  fuchsia_wlan_phyimpl::wire::WlanPhyCountry& GetCountryReq() { return country_; }
  fuchsia_wlan_common::wire::PowerSaveType GetPSType() { return ps_mode_; }
  fuchsia_wlan_phyimpl::wire::BtCoexistenceMode GetBtCoexMode() { return bt_coex_mode_; }
  // Record the create iface request data when fake phyimpl device gets it.
  fuchsia_wlan_device::wire::CreateIfaceRequest create_iface_req_;
  bool has_init_sta_addr_;

  // Record the destroy iface request data when fake phyimpl device gets it.
  uint16_t destroy_iface_id_;

  // Record the country data when fake phyimpl device gets it.
  fuchsia_wlan_phyimpl::wire::WlanPhyCountry country_;

  // Record the power save mode data when fake phyimpl device gets it.
  fuchsia_wlan_common::wire::PowerSaveType ps_mode_;

  // Record the bt coexistence mode data when fake phyimpl device gets it.
  fuchsia_wlan_phyimpl::wire::BtCoexistenceMode bt_coex_mode_;

  // Record the tx power scenario data when fake phyimpl device gets it.
  fuchsia_wlan_phyimpl::wire::TxPowerScenario tx_power_scenario_ =
      fuchsia_wlan_phyimpl::wire::TxPowerScenario::kDefault;

  static constexpr fuchsia_wlan_common::wire::WlanMacRole kFakeMacRole =
      fuchsia_wlan_common::wire::WlanMacRole::kAp;
  static constexpr uint16_t kFakeIfaceId = 1;
  static constexpr fidl::Array<uint8_t, fuchsia_wlan_phyimpl::wire::kWlanphyAlpha2Len> kAlpha2{'W',
                                                                                               'W'};
  static constexpr fuchsia_wlan_common::wire::PowerSaveType kFakePsMode =
      fuchsia_wlan_common::wire::PowerSaveType::kPsModePerformance;
  static constexpr ::fidl::Array<uint8_t, 6> kValidStaAddr = {1, 2, 3, 4, 5, 6};
  static constexpr ::fidl::Array<uint8_t, 6> kInvalidStaAddr = {0, 0, 0, 0, 0, 0};

  // The completion to synchronize the state in tests, because there are async FIDL calls.
  libsync::Completion test_completion_;
  libsync::Completion wait_for_notify_client_;

 protected:
  void* dummy_ctx_;

 private:
  fidl::WireSyncClient<fuchsia_wlan_phyimpl::WlanPhyImplNotify> phyimpl_notify_client_;
  fuchsia_wlan_phyimpl::WlanPhyImplNotifyError critical_event_err_;
};

class TestEnvironment : fdf_testing::Environment {
 public:
  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    auto wlanphyimpl = [this](fdf::ServerEnd<fuchsia_wlan_phyimpl::WlanPhyImpl> server_end) {
      fake_phyimpl_parent_.ServiceConnectHandler(std::move(server_end));
    };

    // Add the service contains WlanPhyImpl protocol to outgoing directory.
    fuchsia_wlan_phyimpl::Service::InstanceHandler wlanphyimpl_service_handler(
        {.wlan_phy_impl = wlanphyimpl});
    auto result = to_driver_vfs.AddService<fuchsia_wlan_phyimpl::Service>(
        std::move(wlanphyimpl_service_handler));
    EXPECT_TRUE(result.is_ok());

    return zx::ok();
  }

  FakeWlanPhyImpl fake_phyimpl_parent_;
};

class FixtureConfig final {
 public:
  using DriverType = wlanphy::Device;
  using EnvironmentType = TestEnvironment;
};

class WlanphyDeviceTest : public ::testing::Test {
 public:
  WlanphyDeviceTest() = default;
  void SetUp() override {
    zx::result<> result = driver_test().StartDriver();
    ASSERT_EQ(ZX_OK, result.status_value());

    auto connect_result =
        driver_test().ConnectThroughDevfs<fuchsia_wlan_device::Connector>("wlanphy");
    EXPECT_EQ(ZX_OK, connect_result.status_value());
    // Bind to the client end
    fidl::ClientEnd<fuchsia_wlan_device::Connector> client_end(std::move(connect_result.value()));
    phy_connector_.Bind(std::move(client_end));
    ASSERT_TRUE(phy_connector_.is_valid());
    // Create an endpoint
    auto endpoints_phy = fidl::Endpoints<fuchsia_wlan_device::Phy>::Create();
    // Send the server end to the driver
    auto conn_result = phy_connector_->Connect(std::move(endpoints_phy.server));
    ASSERT_TRUE(conn_result.ok());
    // Bind to the client end.
    client_phy_ = fidl::WireSyncClient<fuchsia_wlan_device::Phy>(std::move(endpoints_phy.client));
    ASSERT_TRUE(client_phy_.is_valid());
  }

  void TearDown() override {
    // Only PrepareStop() will be called in StopDriver(), Stop() won't be called.
    zx::result prepare_stop_result = driver_test().StopDriver();
    EXPECT_EQ(ZX_OK, prepare_stop_result.status_value());
  }
  void WaitForCommandCompletion() {
    FakeWlanPhyImpl* fake_wlan_phy = nullptr;
    driver_test().RunInEnvironmentTypeContext(
        [&](TestEnvironment& env) { fake_wlan_phy = &env.fake_phyimpl_parent_; });
    ASSERT_NE(fake_wlan_phy, nullptr);
    fake_wlan_phy->WaitForCompletion();
  }
  void WaitForNotifyClient() {
    FakeWlanPhyImpl* fake_wlan_phy = nullptr;
    driver_test().RunInEnvironmentTypeContext(
        [&](TestEnvironment& env) { fake_wlan_phy = &env.fake_phyimpl_parent_; });
    ASSERT_NE(fake_wlan_phy, nullptr);
    fake_wlan_phy->WaitForNotifyClient();
  }
  fdf_testing::BackgroundDriverTest<FixtureConfig>& driver_test() { return driver_test_; }
  // The FIDL client to communicate with wlanphy device.
  fidl::WireSyncClient<fuchsia_wlan_device::Phy> client_phy_;
  fidl::WireSyncClient<fuchsia_wlan_device::Connector> phy_connector_;
  fdf_testing::BackgroundDriverTest<FixtureConfig> driver_test_;
};

TEST_F(WlanphyDeviceTest, CreateIfaceTestNullAddr) {
  auto dummy_ends = fidl::CreateEndpoints<fuchsia_wlan_device::Phy>();
  auto dummy_channel = dummy_ends->server.TakeChannel();
  // All-zero MAC address in the request will should result in a false on has_init_sta_addr in
  // next level's FIDL request.
  fuchsia_wlan_device::wire::CreateIfaceRequest req = {
      .role = fuchsia_wlan_common::wire::WlanMacRole::kClient,
      .mlme_channel = std::move(dummy_channel),
      .init_sta_addr = FakeWlanPhyImpl::kInvalidStaAddr,
  };
  auto result = client_phy_->CreateIface(std::move(req));
  ASSERT_TRUE(result.ok());

  WaitForCommandCompletion();
  EXPECT_FALSE(driver_test().RunInEnvironmentTypeContext<bool>(
      [](TestEnvironment& env) { return env.fake_phyimpl_parent_.HasInitStaAddr(); }));
}

TEST_F(WlanphyDeviceTest, CreateIfaceTestValidAddr) {
  auto dummy_ends = fidl::CreateEndpoints<fuchsia_wlan_device::Phy>();
  auto dummy_channel = dummy_ends->server.TakeChannel();

  fuchsia_wlan_device::wire::CreateIfaceRequest req = {
      .role = fuchsia_wlan_common::wire::WlanMacRole::kClient,
      .mlme_channel = std::move(dummy_channel),
      .init_sta_addr = FakeWlanPhyImpl::kValidStaAddr,
  };

  auto result = client_phy_->CreateIface(std::move(req));
  ASSERT_TRUE(result.ok());
  WaitForCommandCompletion();
  EXPECT_TRUE(driver_test().RunInEnvironmentTypeContext<bool>(
      [](TestEnvironment& env) { return env.fake_phyimpl_parent_.HasInitStaAddr(); }));
  EXPECT_EQ(driver_test().RunInEnvironmentTypeContext<uint16_t>(
                [](TestEnvironment& env) { return env.fake_phyimpl_parent_.kFakeIfaceId; }),
            result->value()->iface_id);
}

TEST_F(WlanphyDeviceTest, CreateIfaceTestInvalidRole) {
  auto dummy_ends = fidl::CreateEndpoints<fuchsia_wlan_device::Phy>();
  auto dummy_channel = dummy_ends->server.TakeChannel();

  fuchsia_wlan_device::wire::CreateIfaceRequest req = {
      .role = static_cast<fuchsia_wlan_common::wire::WlanMacRole>(999),
      .mlme_channel = std::move(dummy_channel),
      .init_sta_addr = FakeWlanPhyImpl::kValidStaAddr,
  };

  auto result = client_phy_->CreateIface(std::move(req));
  ASSERT_TRUE(result.ok());
  ASSERT_TRUE(result->is_error());
  EXPECT_EQ(result->error_value(), ZX_ERR_INVALID_ARGS);
}

TEST_F(WlanphyDeviceTest, DestroyIface) {
  fuchsia_wlan_device::wire::DestroyIfaceRequest req = {
      .id = FakeWlanPhyImpl::kFakeIfaceId,
  };

  auto result = client_phy_->DestroyIface(std::move(req));
  ASSERT_TRUE(result.ok());
  WaitForCommandCompletion();
  EXPECT_EQ(driver_test().RunInEnvironmentTypeContext<uint16_t>(
                [](TestEnvironment& env) { return env.fake_phyimpl_parent_.kFakeIfaceId; }),
            req.id);
}

TEST_F(WlanphyDeviceTest, SetCountry) {
  fuchsia_wlan_device::wire::CountryCode country_code = {
      .alpha2 =
          {
              .data_ = {'U', 'S'},
          },
  };
  auto result = client_phy_->SetCountry(std::move(country_code));
  ASSERT_TRUE(result.ok());
  WaitForCommandCompletion();

  auto country =
      driver_test().RunInEnvironmentTypeContext<fuchsia_wlan_phyimpl::wire::WlanPhyCountry>(
          [](TestEnvironment& env) { return env.fake_phyimpl_parent_.GetCountryReq(); });
  EXPECT_EQ(0, memcmp(&country_code.alpha2.data()[0], &country.alpha2().data()[0],
                      fuchsia_wlan_phyimpl::wire::kWlanphyAlpha2Len));
}

TEST_F(WlanphyDeviceTest, GetCountry) {
  auto result = client_phy_->GetCountry();
  ASSERT_TRUE(result.ok());
  WaitForCommandCompletion();
  auto country_code =
      driver_test()
          .RunInEnvironmentTypeContext<
              fidl::Array<uint8_t, fuchsia_wlan_phyimpl::wire::kWlanphyAlpha2Len>>(
              [](TestEnvironment& env) { return env.fake_phyimpl_parent_.kAlpha2; });
  EXPECT_EQ(0, memcmp(&result->value()->resp.alpha2.data()[0], &country_code.data()[0],
                      fuchsia_wlan_phyimpl::wire::kWlanphyAlpha2Len));
}

TEST_F(WlanphyDeviceTest, ClearCountry) {
  auto result = client_phy_->ClearCountry();
  ASSERT_TRUE(result.ok());
  WaitForCommandCompletion();
}

TEST_F(WlanphyDeviceTest, SetPowerSaveMode) {
  fuchsia_wlan_common::wire::PowerSaveType ps_mode =
      fuchsia_wlan_common::wire::PowerSaveType::kPsModeLowPower;
  auto result = client_phy_->SetPowerSaveMode(std::move(ps_mode));
  ASSERT_TRUE(result.ok());
  WaitForCommandCompletion();
  EXPECT_EQ(driver_test().RunInEnvironmentTypeContext<fuchsia_wlan_common::wire::PowerSaveType>(
                [](TestEnvironment& env) { return env.fake_phyimpl_parent_.GetPSType(); }),
            ps_mode);
}

TEST_F(WlanphyDeviceTest, GetPowerSaveMode) {
  auto result = client_phy_->GetPowerSaveMode();
  ASSERT_TRUE(result.ok());
  WaitForCommandCompletion();
  EXPECT_EQ(driver_test().RunInEnvironmentTypeContext<fuchsia_wlan_common::wire::PowerSaveType>(
                [](TestEnvironment& env) { return env.fake_phyimpl_parent_.kFakePsMode; }),
            result->value()->resp);
}

TEST_F(WlanphyDeviceTest, PowerDown) {
  auto result = client_phy_->PowerDown();
  ASSERT_TRUE(result.ok());
  WaitForCommandCompletion();
}

TEST_F(WlanphyDeviceTest, PowerUp) {
  auto result = client_phy_->PowerUp();
  ASSERT_TRUE(result.ok());
  WaitForCommandCompletion();
}

TEST_F(WlanphyDeviceTest, Reset) {
  auto result = client_phy_->Reset();
  ASSERT_TRUE(result.ok());
  WaitForCommandCompletion();
}

TEST_F(WlanphyDeviceTest, GetPowerState) {
  auto result = client_phy_->GetPowerState();
  ASSERT_TRUE(result.ok());
  WaitForCommandCompletion();
  EXPECT_EQ(
      driver_test().RunInEnvironmentTypeContext<bool>([](TestEnvironment& env) { return true; }),
      result->value()->power_on);
}

TEST_F(WlanphyDeviceTest, SetBtCoexistenceMode) {
  auto result =
      client_phy_->SetBtCoexistenceMode(fuchsia_wlan_internal::wire::BtCoexistenceMode::kModeAuto);
  ASSERT_TRUE(result.ok());
  WaitForCommandCompletion();
  EXPECT_EQ(
      driver_test().RunInEnvironmentTypeContext<fuchsia_wlan_phyimpl::wire::BtCoexistenceMode>(
          [](TestEnvironment& env) { return env.fake_phyimpl_parent_.GetBtCoexMode(); }),
      fuchsia_wlan_phyimpl::wire::BtCoexistenceMode::kModeAuto);
}

using TxPowerScenarioParam = std::pair<fuchsia_wlan_internal::wire::TxPowerScenario,
                                       fuchsia_wlan_phyimpl::wire::TxPowerScenario>;

struct TxPowerScenarioTest : public WlanphyDeviceTest,
                             public testing::WithParamInterface<TxPowerScenarioParam> {
  static fuchsia_wlan_internal::wire::TxPowerScenario InternalTxPowerScenario() {
    return GetParam().first;
  }
  static fuchsia_wlan_phyimpl::wire::TxPowerScenario PhyImplTxPowerScenario() {
    return GetParam().second;
  }
};

TEST_P(TxPowerScenarioTest, SetTxPowerScenario) {
  auto result = client_phy_->SetTxPowerScenario(InternalTxPowerScenario());
  ASSERT_TRUE(result.ok());
  WaitForCommandCompletion();
  EXPECT_EQ(driver_test().RunInEnvironmentTypeContext<fuchsia_wlan_phyimpl::wire::TxPowerScenario>(
                [](TestEnvironment& env) { return env.fake_phyimpl_parent_.tx_power_scenario_; }),
            PhyImplTxPowerScenario());
}

TEST_P(TxPowerScenarioTest, ResetTxPowerScenario) {
  driver_test().RunInEnvironmentTypeContext([&](TestEnvironment& env) {
    env.fake_phyimpl_parent_.tx_power_scenario_ = PhyImplTxPowerScenario();
  });

  auto result = client_phy_->ResetTxPowerScenario();
  ASSERT_TRUE(result.ok());
  WaitForCommandCompletion();
  // No matter what scenario was set, the test reset implementation sets the scenario to default.
  EXPECT_EQ(driver_test().RunInEnvironmentTypeContext<fuchsia_wlan_phyimpl::wire::TxPowerScenario>(
                [](TestEnvironment& env) { return env.fake_phyimpl_parent_.tx_power_scenario_; }),
            fuchsia_wlan_phyimpl::wire::TxPowerScenario::kDefault);
}

TEST_P(TxPowerScenarioTest, GetTxPowerScenario) {
  driver_test().RunInEnvironmentTypeContext([&](TestEnvironment& env) {
    env.fake_phyimpl_parent_.tx_power_scenario_ = PhyImplTxPowerScenario();
  });

  auto result = client_phy_->GetTxPowerScenario();
  ASSERT_TRUE(result.ok());
  EXPECT_EQ(result.value()->scenario, InternalTxPowerScenario());
  WaitForCommandCompletion();
}

INSTANTIATE_TEST_SUITE_P(
    TxPowerScenarioTests, TxPowerScenarioTest,
    testing::Values(
        TxPowerScenarioParam(fuchsia_wlan_internal::wire::TxPowerScenario::kDefault,
                             fuchsia_wlan_phyimpl::wire::TxPowerScenario::kDefault),
        TxPowerScenarioParam(fuchsia_wlan_internal::wire::TxPowerScenario::kVoiceCall,
                             fuchsia_wlan_phyimpl::wire::TxPowerScenario::kVoiceCall),
        TxPowerScenarioParam(fuchsia_wlan_internal::wire::TxPowerScenario::kHeadCellOff,
                             fuchsia_wlan_phyimpl::wire::TxPowerScenario::kHeadCellOff),
        TxPowerScenarioParam(fuchsia_wlan_internal::wire::TxPowerScenario::kHeadCellOn,
                             fuchsia_wlan_phyimpl::wire::TxPowerScenario::kHeadCellOn),
        TxPowerScenarioParam(fuchsia_wlan_internal::wire::TxPowerScenario::kBodyCellOff,
                             fuchsia_wlan_phyimpl::wire::TxPowerScenario::kBodyCellOff),
        TxPowerScenarioParam(fuchsia_wlan_internal::wire::TxPowerScenario::kBodyCellOn,
                             fuchsia_wlan_phyimpl::wire::TxPowerScenario::kBodyCellOn),
        TxPowerScenarioParam(fuchsia_wlan_internal::wire::TxPowerScenario::kBodyBtActive,
                             fuchsia_wlan_phyimpl::wire::TxPowerScenario::kBodyBtActive)),
    [](const testing::TestParamInfo<TxPowerScenarioTest::ParamType>& info) {
      // Generate a suffix for the test name.
      switch (info.param.first) {
        case fuchsia_wlan_internal::wire::TxPowerScenario::kDefault:
          return "Default";
        case fuchsia_wlan_internal::wire::TxPowerScenario::kVoiceCall:
          return "VoiceCall";
        case fuchsia_wlan_internal::wire::TxPowerScenario::kHeadCellOff:
          return "HeadCellOff";
        case fuchsia_wlan_internal::wire::TxPowerScenario::kHeadCellOn:
          return "HeadCellOn";
        case fuchsia_wlan_internal::wire::TxPowerScenario::kBodyCellOff:
          return "BodyCellOff";
        case fuchsia_wlan_internal::wire::TxPowerScenario::kBodyCellOn:
          return "BodyCellOn";
        case fuchsia_wlan_internal::wire::TxPowerScenario::kBodyBtActive:
          return "BodyBtActive";
        default:
          // Make sure that each pair in the testing::Values list has a matching case statement.
          ZX_PANIC("Unhandled TX power scenario: %u", static_cast<uint32_t>(info.param.first));
      }
    });

// Send the critical error notification from the wlanphyimpl server (fake wlan driver)
// and expect it to arrive at the wlan.device.phy client via the wlanphy driver. Wait
// and retry until the driver responds with ZX_OK.
TEST_F(WlanphyDeviceTest, CriticalErrorNotifyWaitAndRetry) {
  class EventHandler final : public fidl::WireSyncEventHandler<fuchsia_wlan_device::Phy> {
   public:
    EventHandler() = default;

    void OnCriticalError(
        fidl::WireEvent<fuchsia_wlan_device::Phy::OnCriticalError>* event) override {
      ASSERT_EQ(event->reason_code, fuchsia_wlan_device::CriticalErrorReason::kFwCrash);
      msgs_received_++;
    }
    void OnCountryCodeChange(
        fidl::WireEvent<fuchsia_wlan_device::Phy::OnCountryCodeChange>* event) override {}

    uint8_t GetReceivedMsgCount() { return msgs_received_; }

   private:
    uint8_t msgs_received_ = 0;
  };

  EventHandler event_handler;
  // Wait until notify_client is ready.
  WaitForNotifyClient();

  zx_status_t status;
  while (true) {
    status = driver_test().RunInEnvironmentTypeContext<zx_status_t>([](TestEnvironment& env) {
      return env.fake_phyimpl_parent_.SendCriticalErrorEvent(
          fuchsia_wlan_phyimpl::CriticalErrorReason::kFwCrash);
    });
    if (status == ZX_OK) {
      auto result = client_phy_.HandleOneEvent(event_handler);
      ASSERT_TRUE(result.ok());
      ASSERT_EQ(event_handler.GetReceivedMsgCount(), 1);
      break;
    }
    // If it error'd out, it should be SHOULD_WAIT.
    ASSERT_EQ(status, ZX_ERR_SHOULD_WAIT);
  }
}

// Send the critical error notification from the wlanphyimpl server (fake wlan driver)
// and expect it to arrive at the wlan.device.phy client via the wlanphy driver. Send a
//
TEST_F(WlanphyDeviceTest, CriticalErrorNotify) {
  class EventHandler final : public fidl::WireSyncEventHandler<fuchsia_wlan_device::Phy> {
   public:
    EventHandler() = default;

    void OnCriticalError(
        fidl::WireEvent<fuchsia_wlan_device::Phy::OnCriticalError>* event) override {
      ASSERT_EQ(event->reason_code, fuchsia_wlan_device::CriticalErrorReason::kFwCrash);
      msgs_received_++;
    }
    void OnCountryCodeChange(
        fidl::WireEvent<fuchsia_wlan_device::Phy::OnCountryCodeChange>* event) override {}

    uint8_t GetReceivedMsgCount() { return msgs_received_; }

   private:
    uint8_t msgs_received_ = 0;
  };

  EventHandler event_handler;
  // Wait until notify_client is ready.
  WaitForNotifyClient();
  // Send a message via the client_phy
  auto pwr_result = client_phy_->GetPowerState();
  ASSERT_TRUE(pwr_result.ok());
  WaitForCommandCompletion();

  auto status = driver_test().RunInEnvironmentTypeContext<zx_status_t>([](TestEnvironment& env) {
    return env.fake_phyimpl_parent_.SendCriticalErrorEvent(
        fuchsia_wlan_phyimpl::CriticalErrorReason::kFwCrash);
  });
  ASSERT_EQ(status, ZX_OK);
  auto result = client_phy_.HandleOneEvent(event_handler);
  ASSERT_TRUE(result.ok());
  ASSERT_EQ(event_handler.GetReceivedMsgCount(), 1);
}

TEST_F(WlanphyDeviceTest, CountryCodeNotify) {
  class EventHandler final : public fidl::WireSyncEventHandler<fuchsia_wlan_device::Phy> {
   public:
    EventHandler() = default;

    void OnCriticalError(
        fidl::WireEvent<fuchsia_wlan_device::Phy::OnCriticalError>* event) override {}
    void OnCountryCodeChange(
        fidl::WireEvent<fuchsia_wlan_device::Phy::OnCountryCodeChange>* event) override {
      memcpy(received_ccode_.data(), event->ind.alpha2.data(), received_ccode_.size());
      msgs_received_++;
    }

    uint8_t GetReceivedMsgCount() { return msgs_received_; }
    std::array<uint8_t, 2> GetReceivedCountryCode() { return received_ccode_; }

   private:
    uint8_t msgs_received_ = 0;
    std::array<uint8_t, 2> received_ccode_ = {};
  };

  EventHandler event_handler;
  // Wait until notify_client is ready.
  WaitForNotifyClient();
  // Send a message via the client_phy
  auto pwr_result = client_phy_->GetPowerState();
  ASSERT_TRUE(pwr_result.ok());
  WaitForCommandCompletion();
  std::array<uint8_t, 2> country_code = {'U', 'S'};

  auto status = driver_test().RunInEnvironmentTypeContext<zx_status_t>([&](TestEnvironment& env) {
    return env.fake_phyimpl_parent_.SendCountryCodeEvent(country_code);
  });
  ASSERT_EQ(status, ZX_OK);
  auto result = client_phy_.HandleOneEvent(event_handler);
  ASSERT_TRUE(result.ok());
  ASSERT_EQ(event_handler.GetReceivedMsgCount(), 1);
  ASSERT_EQ(country_code, event_handler.GetReceivedCountryCode());
}

}  // namespace
}  // namespace wlanphy
