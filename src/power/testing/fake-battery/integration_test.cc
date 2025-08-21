// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.driver.test/cpp/fidl.h>
#include <fidl/fuchsia.power.battery/cpp/fidl.h>
#include <lib/component/incoming/cpp/directory.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/component/incoming/cpp/service.h>
#include <lib/component/incoming/cpp/service_member_watcher.h>
#include <lib/device-watcher/cpp/device-watcher.h>
#include <lib/driver_test_realm/realm_builder/cpp/lib.h>
#include <lib/sys/component/cpp/testing/realm_builder.h>
#include <lib/sys/component/cpp/testing/realm_builder_types.h>
#include <lib/syslog/cpp/macros.h>

#include <bind/fuchsia/platform/cpp/bind.h>

#include "src/lib/testing/loop_fixture/test_loop_fixture.h"

class FakeBatteryRealmTest : public gtest::TestLoopFixture {
 public:
 protected:
  void SetUp() override {
    TestLoopFixture::SetUp();

    // Create and build the realm.
    auto realm_builder = component_testing::RealmBuilder::Create();
    driver_test_realm::Setup(realm_builder);

    std::vector<fuchsia_component_test::Capability> exposes;
    exposes.emplace_back(fuchsia_component_test::Capability::WithService(
        fuchsia_component_test::Service{{.name = fuchsia_power_battery::InfoService::Name}}));
    driver_test_realm::AddDtrExposes(realm_builder, exposes);
    realm_ = realm_builder.Build(dispatcher());

    // Start DriverTestRealm.
    zx::result dtr = realm_->component().Connect<fuchsia_driver_test::Realm>();
    fuchsia_driver_test::RealmArgs args{{
        .root_driver = "fuchsia-boot:///platform-bus#meta/platform-bus.cm",
        .dtr_exposes = exposes,
        .software_devices = std::vector{fuchsia_driver_test::SoftwareDevice(
            "fake-battery", bind_fuchsia_platform::BIND_PLATFORM_DEV_DID_FAKE_BATTERY)},
    }};
    fidl::Result result = fidl::Call(*dtr)->Start(std::move(args));
    ASSERT_TRUE(result.is_ok()) << result.error_value();
  }

  component_testing::RealmRoot& Realm() { return *realm_; }

 private:
  std::optional<component_testing::RealmRoot> realm_;
};

TEST_F(FakeBatteryRealmTest, DriversExist) {
  fidl::UnownedClientEnd<fuchsia_io::Directory> exposed{
      Realm().component().exposed().unowned_channel()};

  fidl::ClientEnd<fuchsia_io::Directory> svc_root(
      Realm().component().CloneExposedDir().TakeChannel());
  component::SyncServiceMemberWatcher<fuchsia_power_battery::InfoService::Device> watcher(
      svc_root.borrow());

  zx::result result1 = watcher.GetNextInstance(false);
  ASSERT_TRUE(result1.is_ok());

  auto client_end = std::move(result1.value());
  fidl::WireSyncClient client(std::move(client_end));

  // Send a FIDL request.
  fidl::WireResult result2 = client->GetBatteryInfo();
  ASSERT_EQ(ZX_OK, result2.status());
  const auto& info = result2.value().info;
  ASSERT_EQ(info.charge_source(), fuchsia_power_battery::ChargeSource::kAcAdapter);
}
