// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "driver.h"

#include <fidl/fuchsia.power.battery/cpp/wire.h>
#include <lib/driver/testing/cpp/driver_test.h>

#include <gtest/gtest.h>
#include <sdk/lib/syslog/cpp/macros.h>

#include "src/lib/testing/predicates/status.h"

namespace fake_battery::testing {

namespace fbattery = fuchsia_power_battery;

class FakeBatteryDriverTestEnvironment : public fdf_testing::Environment {
 public:
  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override { return zx::ok(); }
};

class FixtureConfig final {
 public:
  using DriverType = Driver;
  using EnvironmentType = FakeBatteryDriverTestEnvironment;
};

class FakeBatteryDriverTest : public ::testing::Test {
 public:
  void SetUp() override {
    ASSERT_OK(driver_test_.StartDriver());
    zx::result connect_result = driver_test().Connect<fbattery::InfoService::Device>();
    EXPECT_EQ(ZX_OK, connect_result.status_value());
    battery_info_provider_ = std::move(connect_result.value());
  }

  void TearDown() override {
    ASSERT_OK(driver_test_.StopDriver());
    driver_test().ShutdownAndDestroyDriver();
  }

 protected:
  fidl::ClientEnd<fbattery::BatteryInfoProvider>& GetBatteryInfoProviderClient() {
    return battery_info_provider_;
  }
  fdf_testing::BackgroundDriverTest<FixtureConfig>& driver_test() { return driver_test_; }

 private:
  fidl::ClientEnd<fbattery::BatteryInfoProvider> battery_info_provider_;
  fdf_testing::BackgroundDriverTest<FixtureConfig> driver_test_;
};

TEST_F(FakeBatteryDriverTest, CanGetInfo) {
  {
    auto result = fidl::WireCall(GetBatteryInfoProviderClient())->GetBatteryInfo();
    ASSERT_EQ(result.status(), ZX_OK);
    const auto& info = result.value().info;
    ASSERT_EQ(info.status(), fuchsia_power_battery::BatteryStatus::kOk);
    ASSERT_EQ(info.time_remaining().Which(),
              fuchsia_power_battery::wire::TimeRemaining::Tag::kFullCharge);
    ASSERT_EQ(info.time_remaining().full_charge(), zx::sec(59).to_nsecs());
  }
}

class ForegroundFakeBatteryDriverTest : public ::testing::Test {
 public:
  void SetUp() override {
    ASSERT_OK(driver_test_.StartDriver());
    zx::result connect_result = driver_test().Connect<fbattery::InfoService::Device>();
    EXPECT_EQ(ZX_OK, connect_result.status_value());
    battery_info_provider_ = std::move(connect_result.value());
  }

  void TearDown() override {
    ASSERT_OK(driver_test_.StopDriver());
    driver_test().ShutdownAndDestroyDriver();
  }

 protected:
  fdf_testing::ForegroundDriverTest<FixtureConfig>& driver_test() { return driver_test_; }
  fidl::ClientEnd<fbattery::BatteryInfoProvider>& GetBatteryInfoProviderClient() {
    return battery_info_provider_;
  }

 private:
  fdf_testing::ForegroundDriverTest<FixtureConfig> driver_test_;
  fidl::ClientEnd<fbattery::BatteryInfoProvider> battery_info_provider_;
};

// Picking ForegroundDriverTest to test Watch. Otherwise we have to use a control fidl to make sure
// the driver receives the reply for OnChangeBatteryInfo, or we have to ignore ZX_ERR_CANCELED.
TEST_F(ForegroundFakeBatteryDriverTest, CanWatch) {
  class FakeBatteryInfoWatcher : public fidl::Server<fbattery::BatteryInfoWatcher> {
   public:
    void Bind(fidl::ServerEnd<fbattery::BatteryInfoWatcher> server_end,
              fdf_testing::DriverRuntime* runtime) {
      bindings_.AddBinding(runtime->GetForegroundDispatcher()->async_dispatcher(),
                           std::move(server_end), this, fidl::kIgnoreBindingClosure);
      test_runtime_ = runtime;
    }

    void OnChangeBatteryInfo(OnChangeBatteryInfoRequest& request,
                             OnChangeBatteryInfoCompleter::Sync& completer) override {
      EXPECT_EQ(request.info().charge_status(), fbattery::ChargeStatus::kCharging);
      EXPECT_EQ(request.info().charge_source(), fbattery::ChargeSource::kAcAdapter);
      EXPECT_EQ(request.info().present_voltage_mv(), 4752);
      EXPECT_EQ(request.info().present_current_ma(), 250);
      EXPECT_EQ(request.info().health(), fbattery::HealthStatus::kGood);
      completer.Reply();
      EXPECT_TRUE(test_runtime_);
      completed_ = true;
    }

    bool Completed() const { return completed_; }

   private:
    fidl::ServerBindingGroup<fbattery::BatteryInfoWatcher> bindings_;
    fdf_testing::DriverRuntime* test_runtime_;
    bool completed_ = false;

  } fake_watcher;
  {
    auto& battery_info_provider = GetBatteryInfoProviderClient();
    auto [client_end, server_end] = fidl::Endpoints<fbattery::BatteryInfoWatcher>::Create();
    fake_watcher.Bind(std::move(server_end), &driver_test().runtime());
    auto result = fidl::WireCall(battery_info_provider)->Watch(std::move(client_end));
    ASSERT_TRUE(result.ok());

    // Wait for the driver to receive and handle Watch message.
    driver_test().runtime().RunUntilIdle();
    driver_test().runtime().RunUntil([&fake_watcher]() { return fake_watcher.Completed(); });
    // Wait for the driver to receive and handle the reply from the fake watcher.
    driver_test().runtime().RunUntilIdle();
  }
}

}  // namespace fake_battery::testing
