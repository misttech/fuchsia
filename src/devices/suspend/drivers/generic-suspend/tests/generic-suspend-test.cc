// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "../generic-suspend.h"

#include <fidl/fuchsia.hardware.power.suspend/cpp/markers.h>
#include <fidl/fuchsia.kernel/cpp/wire.h>
#include <lib/driver/compat/cpp/device_server.h>
#include <lib/driver/fake-platform-device/cpp/fake-pdev.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/inspect/cpp/hierarchy.h>
#include <lib/inspect/cpp/reader.h>
#include <zircon/syscalls-next.h>

#include <optional>

#include <gtest/gtest.h>

#include "lib/zx/handle.h"
#include "lib/zx/resource.h"
#include "lib/zx/vmo.h"
#include "src/devices/suspend/drivers/generic-suspend/generic_suspend_config.h"

namespace suspend {

class GenericSuspendTest : public GenericSuspend {
 public:
  GenericSuspendTest() : GenericSuspend() {
    zx_status_t result = zx::vmo::create(1, 0, &fake_resource_);
    ZX_ASSERT(result == ZX_OK);
  }

  zx::result<zx::resource> GetCpuResource(fdf::Namespace& incoming) override {
    zx::vmo dupe;
    zx_status_t st = fake_resource_.duplicate(ZX_RIGHT_SAME_RIGHTS, &dupe);
    if (st != ZX_OK) {
      return zx::error(st);
    }

    // Client is now the owner.
    zx::handle result(dupe.release());

    return zx::ok(std::move(result));
  }

  zx::result<WakeSourceReport> SystemSuspendEnter(uint64_t options) override {
    // No-op override for testing, which only records the options it was called with.
    last_suspend_options_ = options;
    WakeSourceReport empty{
        .actual_entry_count = 0,
    };
    return zx::ok(empty);
  }

  // The options passed to the most recent `SystemSuspendEnter` call, if any.
  std::optional<uint64_t> last_suspend_options() const { return last_suspend_options_; }

  using GenericSuspend::inspector;

  static DriverRegistration GetDriverRegistration() {
    // Use a custom DriverRegistration to create the DUT. Without this, the non-test implementation
    // will be used by default.
    return FUCHSIA_DRIVER_REGISTRATION_V1(
        fdf_internal::DriverServer2<GenericSuspendTest>::initialize,
        fdf_internal::DriverServer2<GenericSuspendTest>::destroy);
  }

 private:
  // We just need any kernel handle here.
  zx::vmo fake_resource_;

  std::optional<uint64_t> last_suspend_options_;
};

class TestEnvironmentWrapper : public fdf_testing::Environment {
 public:
  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    pdev_.SetConfig(fdf_fake::FakePDev::Config{});

    auto pdev_result = to_driver_vfs.AddService<fuchsia_hardware_platform_device::Service>(
        pdev_.GetInstanceHandler(fdf::Dispatcher::GetCurrent()->async_dispatcher()));
    if (pdev_result.is_error()) {
      return pdev_result.take_error();
    }

    compat_server_.Initialize("default");
    zx_status_t status =
        compat_server_.Serve(fdf::Dispatcher::GetCurrent()->async_dispatcher(), &to_driver_vfs);
    return zx::make_result(status);
  }

 private:
  fdf_fake::FakePDev pdev_;
  compat::DeviceServer compat_server_;
};

class GenericSuspendTestConfiguration {
 public:
  using DriverType = GenericSuspendTest;
  using EnvironmentType = TestEnvironmentWrapper;
};

class GenericSuspendTestFixture : public ::testing::Test {
 public:
  void SetUp() override { ASSERT_NO_FATAL_FAILURE(StartDriver(generic_suspend_config::Config{})); }

  // Starts the driver with the given structured config and connects to its Suspender protocol.
  void StartDriver(generic_suspend_config::Config config) {
    zx::result<> result = driver_test().StartDriverWithCustomStartArgs(
        [&config](fdf::DriverStartArgs& start_args) { start_args.config(config.ToVmo()); });
    ASSERT_EQ(ZX_OK, result.status_value());
    zx::result connect_result =
        driver_test().Connect<fuchsia_hardware_power_suspend::SuspendService::Suspender>();
    EXPECT_EQ(ZX_OK, connect_result.status_value());
    client_.Bind(std::move(connect_result.value()));
  }
  void TearDown() override {
    zx::result<> result = driver_test().StopDriver();
    ASSERT_EQ(ZX_OK, result.status_value());
  }
  fidl::WireSyncClient<fuchsia_hardware_power_suspend::Suspender>& client() { return client_; }

  // Issues a Suspend request for state 0 and expects it to succeed.
  void Suspend() {
    fidl::Arena arena;
    auto request = fuchsia_hardware_power_suspend::wire::SuspenderSuspendRequest::Builder(arena)
                       .state_index(0)
                       .Build();
    auto result = client()->Suspend(request);
    ASSERT_TRUE(result.ok());
    ASSERT_TRUE(result->is_ok());
  }

  // The options the driver passed to the most recent `SystemSuspendEnter` call, if any.
  std::optional<uint64_t> LastSuspendOptions() {
    return driver_test().RunInDriverContext<std::optional<uint64_t>>(
        [](GenericSuspendTest& driver) { return driver.last_suspend_options(); });
  }

  // Reads the driver's inspect hierarchy.
  inspect::Hierarchy ReadInspect() {
    zx::vmo vmo = driver_test().RunInDriverContext<zx::vmo>(
        [](GenericSuspendTest& driver) { return driver.inspector().DuplicateVmo(); });
    fpromise::result<inspect::Hierarchy> hierarchy = inspect::ReadFromVmo(vmo);
    ZX_ASSERT(hierarchy.is_ok());
    return hierarchy.take_value();
  }

  // Expects the `use_deep_suspend` inspect root property to be present and equal `expected`.
  void ExpectUseDeepSuspendInspect(bool expected) {
    inspect::Hierarchy hierarchy = ReadInspect();
    const auto* use_deep_suspend =
        hierarchy.node().get_property<inspect::BoolPropertyValue>("use_deep_suspend");
    ASSERT_NE(use_deep_suspend, nullptr);
    EXPECT_EQ(use_deep_suspend->value(), expected);
  }

  fdf_testing::BackgroundDriverTest<GenericSuspendTestConfiguration>& driver_test() {
    return driver_test_;
  }

 private:
  fdf_testing::BackgroundDriverTest<GenericSuspendTestConfiguration> driver_test_;
  fidl::WireSyncClient<fuchsia_hardware_power_suspend::Suspender> client_;
};

TEST_F(GenericSuspendTestFixture, TrivialGetSuspendStates) {
  auto result = client()->GetSuspendStates();

  ASSERT_TRUE(result.ok());

  // The protocol mandates that at least one suspend state is returned.
  ASSERT_TRUE(result.value()->has_suspend_states());
  EXPECT_GT(result.value()->suspend_states().size(), 0ul);
}

TEST_F(GenericSuspendTestFixture, TrivialSuspend) {
  ASSERT_NO_FATAL_FAILURE(Suspend());

  // Deep suspend is disabled by default, so no suspend options are passed.
  std::optional<uint64_t> options = LastSuspendOptions();
  ASSERT_TRUE(options.has_value());
  EXPECT_EQ(options.value(), 0u);
  ExpectUseDeepSuspendInspect(false);
}

class GenericSuspendDeepSuspendTestFixture : public GenericSuspendTestFixture {
 public:
  void SetUp() override {
    generic_suspend_config::Config config;
    config.use_deep_suspend() = true;
    ASSERT_NO_FATAL_FAILURE(StartDriver(std::move(config)));
  }
};

TEST_F(GenericSuspendDeepSuspendTestFixture, SuspendWithDeepSuspendPassesDeepOption) {
  ASSERT_NO_FATAL_FAILURE(Suspend());

  std::optional<uint64_t> options = LastSuspendOptions();
  ASSERT_TRUE(options.has_value());
  // ZX_SYSTEM_SUSPEND_OPTION_DEEP may not be combined with any other option.
  EXPECT_EQ(options.value(), ZX_SYSTEM_SUSPEND_OPTION_DEEP);
  ExpectUseDeepSuspendInspect(true);
}

}  // namespace suspend
