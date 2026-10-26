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
#include <string.h>
#include <zircon/status.h>
#include <zircon/syscalls-next.h>

#include <optional>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

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
    // No-op override for testing, which only records the options it was called with and returns
    // the report set by `set_next_report`, or an empty report.
    last_suspend_options_ = options;
    if (next_status_ != ZX_OK) {
      return zx::error(std::exchange(next_status_, ZX_OK));
    }
    WakeSourceReport report = next_report_.value_or(WakeSourceReport{
        .actual_entry_count = 0,
    });
    next_report_.reset();
    return zx::ok(report);
  }

  // The options passed to the most recent `SystemSuspendEnter` call, if any.
  std::optional<uint64_t> last_suspend_options() const { return last_suspend_options_; }

  // Sets the report returned by the next `SystemSuspendEnter` call.
  void set_next_report(const WakeSourceReport& report) { next_report_ = report; }

  // Sets the error returned by the next `SystemSuspendEnter` call.
  void set_next_status(zx_status_t status) { next_status_ = status; }

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
  std::optional<WakeSourceReport> next_report_;
  zx_status_t next_status_ = ZX_OK;
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

  // Sets the report the driver's next `SystemSuspendEnter` call returns.
  void SetNextReport(const WakeSourceReport& report) {
    driver_test().RunInDriverContext(
        [&report](GenericSuspendTest& driver) { driver.set_next_report(report); });
  }

  // Sets the error the driver's next `SystemSuspendEnter` call returns.
  void SetNextStatus(zx_status_t status) {
    driver_test().RunInDriverContext(
        [status](GenericSuspendTest& driver) { driver.set_next_status(status); });
  }

  // Issues a Suspend request for state 0, expects it to succeed, and returns the wake vectors.
  std::vector<uint64_t> SuspendAndGetWakeVectors() {
    fidl::Arena arena;
    auto request = fuchsia_hardware_power_suspend::wire::SuspenderSuspendRequest::Builder(arena)
                       .state_index(0)
                       .Build();
    auto result = client()->Suspend(request);
    if (!result.ok()) {
      ADD_FAILURE() << "Suspend transport error: " << result.status_string();
      return {};
    }
    if (result->is_error()) {
      ADD_FAILURE() << "Suspend failed: " << zx_status_get_string(result->error_value());
      return {};
    }
    if (!result->value()->has_reason() || !result->value()->reason().has_wake_vectors()) {
      ADD_FAILURE() << "Missing wake vectors";
      return {};
    }
    const auto& wake_vectors = result->value()->reason().wake_vectors();
    return std::vector<uint64_t>(wake_vectors.begin(), wake_vectors.end());
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

namespace {

// Returns a wake source report entry with the given values.
zx_wake_source_report_entry_t MakeEntry(zx_koid_t koid, std::string_view name,
                                        zx_instant_boot_t initial_signal_time,
                                        zx_instant_boot_t last_signal_time, uint32_t flags) {
  zx_wake_source_report_entry_t entry{
      .koid = koid,
      .initial_signal_time = initial_signal_time,
      .last_signal_time = last_signal_time,
      .last_ack_time = ZX_TIME_INFINITE,
      .signal_count = 1,
      .flags = flags,
  };
  name.copy(entry.name, sizeof(entry.name) - 1);
  return entry;
}

// Expects `node` to describe the wake source report entry `expected`.
void ExpectWakeSourceInspect(const inspect::Hierarchy& node,
                             const zx_wake_source_report_entry_t& expected) {
  const auto* koid = node.node().get_property<inspect::UintPropertyValue>("koid");
  ASSERT_NE(koid, nullptr);
  EXPECT_EQ(koid->value(), expected.koid);
  const auto* name = node.node().get_property<inspect::StringPropertyValue>("name");
  ASSERT_NE(name, nullptr);
  EXPECT_EQ(name->value(),
            std::string_view(expected.name, strnlen(expected.name, sizeof(expected.name))));
  const auto* initial_signal_time =
      node.node().get_property<inspect::IntPropertyValue>("initial_signal_time_ns");
  ASSERT_NE(initial_signal_time, nullptr);
  EXPECT_EQ(initial_signal_time->value(), expected.initial_signal_time);
  const auto* last_signal_time =
      node.node().get_property<inspect::IntPropertyValue>("last_signal_time_ns");
  ASSERT_NE(last_signal_time, nullptr);
  EXPECT_EQ(last_signal_time->value(), expected.last_signal_time);
  const auto* last_ack_time =
      node.node().get_property<inspect::IntPropertyValue>("last_ack_time_ns");
  ASSERT_NE(last_ack_time, nullptr);
  EXPECT_EQ(last_ack_time->value(), expected.last_ack_time);
  const auto* signal_count = node.node().get_property<inspect::UintPropertyValue>("signal_count");
  ASSERT_NE(signal_count, nullptr);
  EXPECT_EQ(signal_count->value(), expected.signal_count);
  const auto* flags = node.node().get_property<inspect::UintPropertyValue>("flags");
  ASSERT_NE(flags, nullptr);
  EXPECT_EQ(flags->value(), expected.flags);
}

}  // namespace

TEST_F(GenericSuspendTestFixture, SuspendRecordsWakeSources) {
  WakeSourceReport report{
      .header =
          {
              .report_time = 3000,
              .suspend_start_time = 1000,
              .total_wake_sources = 5,
              .unreported_wake_report_entries = 0,
          },
      .actual_entry_count = 2,
  };
  report.entries[0] = MakeEntry(11, "irq-a", 2000, 2500, ZX_SYSTEM_WAKE_REPORT_ENTRY_FLAG_SIGNALED);
  report.entries[1] = MakeEntry(12, "irq-b", 500, 600,
                                ZX_SYSTEM_WAKE_REPORT_ENTRY_FLAG_SIGNALED |
                                    ZX_SYSTEM_WAKE_REPORT_ENTRY_FLAG_PREVIOUSLY_REPORTED);
  // The kernel does not guarantee that names are NUL-terminated.
  memset(report.entries[1].name, 'x', sizeof(report.entries[1].name));
  SetNextReport(report);

  EXPECT_EQ(SuspendAndGetWakeVectors(), (std::vector<uint64_t>{11, 12}));

  inspect::Hierarchy hierarchy = ReadInspect();
  const inspect::Hierarchy* last_wake_report = hierarchy.GetByPath({"last_wake_report"});
  ASSERT_NE(last_wake_report, nullptr);
  const auto* report_time =
      last_wake_report->node().get_property<inspect::IntPropertyValue>("report_time_ns");
  ASSERT_NE(report_time, nullptr);
  EXPECT_EQ(report_time->value(), 3000);
  const auto* suspend_start_time =
      last_wake_report->node().get_property<inspect::IntPropertyValue>("suspend_start_time_ns");
  ASSERT_NE(suspend_start_time, nullptr);
  EXPECT_EQ(suspend_start_time->value(), 1000);
  const auto* entry_count =
      last_wake_report->node().get_property<inspect::UintPropertyValue>("entry_count");
  ASSERT_NE(entry_count, nullptr);
  EXPECT_EQ(entry_count->value(), 2u);

  const inspect::Hierarchy* wake_sources = last_wake_report->GetByPath({"wake_sources"});
  ASSERT_NE(wake_sources, nullptr);
  EXPECT_EQ(wake_sources->children().size(), 2u);
  const inspect::Hierarchy* entry0 = wake_sources->GetByPath({"0"});
  ASSERT_NE(entry0, nullptr);
  ASSERT_NO_FATAL_FAILURE(ExpectWakeSourceInspect(*entry0, report.entries[0]));
  const inspect::Hierarchy* entry1 = wake_sources->GetByPath({"1"});
  ASSERT_NE(entry1, nullptr);
  ASSERT_NO_FATAL_FAILURE(ExpectWakeSourceInspect(*entry1, report.entries[1]));
  const auto* name1 = entry1->node().get_property<inspect::StringPropertyValue>("name");
  ASSERT_NE(name1, nullptr);
  EXPECT_EQ(name1->value(), std::string(ZX_MAX_NAME_LEN, 'x'));

  // A subsequent resume replaces the previous report.
  ASSERT_NO_FATAL_FAILURE(Suspend());
  hierarchy = ReadInspect();
  last_wake_report = hierarchy.GetByPath({"last_wake_report"});
  ASSERT_NE(last_wake_report, nullptr);
  wake_sources = last_wake_report->GetByPath({"wake_sources"});
  ASSERT_NE(wake_sources, nullptr);
  EXPECT_TRUE(wake_sources->children().empty());
}

TEST_F(GenericSuspendTestFixture, SuspendLimitsWakeSourcesInInspect) {
  constexpr uint32_t kEntryCount = 20;
  WakeSourceReport report{
      .header =
          {
              .report_time = 3000,
              .suspend_start_time = 1000,
              .total_wake_sources = kEntryCount,
              .unreported_wake_report_entries = 0,
          },
      .actual_entry_count = kEntryCount,
  };
  std::vector<uint64_t> expected_koids;
  for (uint32_t i = 0; i < kEntryCount; i++) {
    report.entries[i] = MakeEntry(100 + i, "irq", 2000, 2000, 0);
    expected_koids.push_back(100 + i);
  }
  SetNextReport(report);

  // All entries are returned over FIDL.
  EXPECT_EQ(SuspendAndGetWakeVectors(), expected_koids);

  // Inspect records the count of all entries, but details for only the first 16.
  inspect::Hierarchy hierarchy = ReadInspect();
  const inspect::Hierarchy* last_wake_report = hierarchy.GetByPath({"last_wake_report"});
  ASSERT_NE(last_wake_report, nullptr);
  const auto* entry_count =
      last_wake_report->node().get_property<inspect::UintPropertyValue>("entry_count");
  ASSERT_NE(entry_count, nullptr);
  EXPECT_EQ(entry_count->value(), kEntryCount);
  const inspect::Hierarchy* wake_sources = last_wake_report->GetByPath({"wake_sources"});
  ASSERT_NE(wake_sources, nullptr);
  EXPECT_EQ(wake_sources->children().size(), 16u);
}

TEST_F(GenericSuspendTestFixture, FailedSuspendKeepsLastWakeReport) {
  WakeSourceReport report{
      .header =
          {
              .report_time = 3000,
              .suspend_start_time = 1000,
              .total_wake_sources = 1,
              .unreported_wake_report_entries = 0,
          },
      .actual_entry_count = 1,
  };
  report.entries[0] = MakeEntry(11, "irq-a", 2000, 2000, 0);
  SetNextReport(report);
  ASSERT_NO_FATAL_FAILURE(Suspend());

  SetNextStatus(ZX_ERR_BAD_STATE);
  fidl::Arena arena;
  auto request = fuchsia_hardware_power_suspend::wire::SuspenderSuspendRequest::Builder(arena)
                     .state_index(0)
                     .Build();
  auto result = client()->Suspend(request);
  ASSERT_TRUE(result.ok());
  ASSERT_TRUE(result->is_error());
  EXPECT_EQ(result->error_value(), ZX_ERR_BAD_STATE);

  // The report from the last successful resume is kept.
  inspect::Hierarchy hierarchy = ReadInspect();
  const inspect::Hierarchy* entry0 = hierarchy.GetByPath({"last_wake_report", "wake_sources", "0"});
  ASSERT_NE(entry0, nullptr);
  ASSERT_NO_FATAL_FAILURE(ExpectWakeSourceInspect(*entry0, report.entries[0]));
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
