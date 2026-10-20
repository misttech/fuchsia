// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async/cpp/task.h>
#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/driver/testing/cpp/driver_runtime.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/driver/testing/cpp/minimal_compat_environment.h>

#include <gtest/gtest.h>

namespace {

class TestDriver2 : public fdf::DriverBase2 {
 public:
  TestDriver2() : fdf::DriverBase2("test_driver2") { dispatcher_in_constructor_ = dispatcher(); }

  zx::result<> Start(fdf::DriverContext context) override { return zx::ok(); }

  async_dispatcher_t* dispatcher_in_constructor() const { return dispatcher_in_constructor_; }

 private:
  async_dispatcher_t* dispatcher_in_constructor_ = nullptr;
};

TEST(DriverBase2Test, DispatcherInConstructor) {
  fdf_testing::DriverRuntime runtime;

  EXPECT_NE(nullptr, fdf_dispatcher_get_current_dispatcher());

  TestDriver2 driver;
  EXPECT_NE(nullptr, driver.dispatcher_in_constructor());
  EXPECT_EQ(fdf_dispatcher_get_async_dispatcher(fdf_dispatcher_get_current_dispatcher()),
            driver.dispatcher_in_constructor());
}

#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
class TestProvideResourceDriver2 : public fdf::DriverBase2 {
 public:
  TestProvideResourceDriver2() : fdf::DriverBase2("test_driver2") {}

  static DriverRegistration GetDriverRegistration() {
    return FUCHSIA_DRIVER_REGISTRATION_V1(
        fdf_internal::DriverServer2<TestProvideResourceDriver2>::initialize,
        fdf_internal::DriverServer2<TestProvideResourceDriver2>::destroy);
  }

  zx::result<> Start(fdf::DriverContext context) override { return zx::ok(); }

  void CreateResourceSync() {
    auto result = ProvideResource("my_resource", {}, {});
    ASSERT_EQ(ZX_OK, result.status_value());
  }
};

struct ProvideResourceFixtureConfig final {
  using DriverType = TestProvideResourceDriver2;
  using EnvironmentType = fdf_testing::MinimalCompatEnvironment;
};

TEST(DriverBase2Test, ProvideResource) {
  fdf_testing::ForegroundDriverTest<ProvideResourceFixtureConfig> driver_test;
  ASSERT_EQ(ZX_OK, driver_test.StartDriver().status_value());
  driver_test.driver()->CreateResourceSync();
  ASSERT_EQ(ZX_OK, driver_test.StopDriver().status_value());
}
#endif

TEST(DriverBase2Test, DefaultHealthCheckRepliesByLevel) {
  fdf_testing::DriverRuntime runtime;
  auto [client_end, server_end] = fdf::Endpoints<fuchsia_driver_framework::Driver>::Create();
  void* token =
      fdf_internal::DriverServer2<TestDriver2>::initialize(server_end.TakeChannel().release());
  ASSERT_NE(nullptr, token);

  fdf::WireClient<fuchsia_driver_framework::Driver> client(std::move(client_end),
                                                           fdf::Dispatcher::GetCurrent()->get());
  fdf::Arena arena('TEST');
  bool ping_called = false;
  client.buffer(arena)
      ->HealthCheck(fuchsia_driver_framework::wire::HealthCheckRequest::kPing)
      .Then([&ping_called](
                fdf::WireUnownedResult<fuchsia_driver_framework::Driver::HealthCheck>& result) {
        ASSERT_TRUE(result.ok());
        ASSERT_TRUE(result->is_ok());
        EXPECT_FALSE(result->value()->has_conditions());
        ping_called = true;
      });
  runtime.RunUntilIdle();
  EXPECT_TRUE(ping_called);

  bool fast_called = false;
  client.buffer(arena)
      ->HealthCheck(fuchsia_driver_framework::wire::HealthCheckRequest::kFast)
      .Then([&fast_called](
                fdf::WireUnownedResult<fuchsia_driver_framework::Driver::HealthCheck>& result) {
        ASSERT_TRUE(result.ok());
        ASSERT_TRUE(result->is_error());
        EXPECT_EQ(fuchsia_driver_framework::wire::HealthCheckError::kNotSupported,
                  result->error_value());
        fast_called = true;
      });
  runtime.RunUntilIdle();
  EXPECT_TRUE(fast_called);

  bool deep_called = false;
  client.buffer(arena)
      ->HealthCheck(fuchsia_driver_framework::wire::HealthCheckRequest::kDeep)
      .Then([&deep_called](
                fdf::WireUnownedResult<fuchsia_driver_framework::Driver::HealthCheck>& result) {
        ASSERT_TRUE(result.ok());
        ASSERT_TRUE(result->is_error());
        EXPECT_EQ(fuchsia_driver_framework::wire::HealthCheckError::kNotSupported,
                  result->error_value());
        deep_called = true;
      });
  runtime.RunUntilIdle();
  EXPECT_TRUE(deep_called);

  client = {};
  runtime.RunUntilIdle();
  fdf_internal::DriverServer2<TestDriver2>::destroy(token);
}

class CustomHealthCheckDriver2 : public fdf::DriverBase2 {
 public:
  CustomHealthCheckDriver2() : fdf::DriverBase2("custom_hc_driver2") {}

  zx::result<> Start(fdf::DriverContext context) override { return zx::ok(); }

  void HealthCheck(fuchsia_driver_framework::HealthCheckRequest request,
                   HealthCheckCompleter completer) override {
    if (request == fuchsia_driver_framework::HealthCheckRequest::kDeep) {
      // Intentionally drop completer without invoking it to test HealthCheckReplyGuard.
      return;
    }
    // Complete asynchronously on the driver dispatcher after HealthCheck returns.
    async::PostTask(dispatcher(), [completer = std::move(completer)]() mutable {
      completer(fit::ok(fuchsia_driver_framework::HealthCheckResponse{{
          .conditions = {{{{
              .status = fuchsia_driver_framework::HealthStatus::kUnhealthy,
              .reason_code = "ASYNC_DSI_UNLOCKED",
          }}}},
      }}));
    });
  }
};

TEST(DriverBase2Test, CustomHealthCheckAndDroppedCompleterGuard) {
  fdf_testing::DriverRuntime runtime;
  auto [client_end, server_end] = fdf::Endpoints<fuchsia_driver_framework::Driver>::Create();
  void* token = fdf_internal::DriverServer2<CustomHealthCheckDriver2>::initialize(
      server_end.TakeChannel().release());
  ASSERT_NE(nullptr, token);

  fdf::WireClient<fuchsia_driver_framework::Driver> client(std::move(client_end),
                                                           fdf::Dispatcher::GetCurrent()->get());
  fdf::Arena arena('TEST');

  bool fast_called = false;
  client.buffer(arena)
      ->HealthCheck(fuchsia_driver_framework::wire::HealthCheckRequest::kFast)
      .Then([&fast_called](
                fdf::WireUnownedResult<fuchsia_driver_framework::Driver::HealthCheck>& result) {
        ASSERT_TRUE(result.ok());
        ASSERT_TRUE(result->is_ok());
        ASSERT_TRUE(result->value()->has_conditions());
        ASSERT_EQ(1u, result->value()->conditions().size());
        EXPECT_EQ(fuchsia_driver_framework::wire::HealthStatus::kUnhealthy,
                  result->value()->conditions()[0].status());
        EXPECT_EQ("ASYNC_DSI_UNLOCKED", result->value()->conditions()[0].reason_code().get());
        fast_called = true;
      });
  runtime.RunUntilIdle();
  EXPECT_TRUE(fast_called);

  bool deep_called = false;
  client.buffer(arena)
      ->HealthCheck(fuchsia_driver_framework::wire::HealthCheckRequest::kDeep)
      .Then([&deep_called](
                fdf::WireUnownedResult<fuchsia_driver_framework::Driver::HealthCheck>& result) {
        ASSERT_TRUE(result.ok());
        ASSERT_TRUE(result->is_error());
        EXPECT_EQ(fuchsia_driver_framework::wire::HealthCheckError::kInternalError,
                  result->error_value());
        deep_called = true;
      });
  runtime.RunUntilIdle();
  EXPECT_TRUE(deep_called);

  client = {};
  runtime.RunUntilIdle();
  fdf_internal::DriverServer2<CustomHealthCheckDriver2>::destroy(token);
}

}  // namespace
