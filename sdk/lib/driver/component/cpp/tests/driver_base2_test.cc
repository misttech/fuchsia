// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async/cpp/task.h>
#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/driver/testing/cpp/driver_runtime.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/driver/testing/cpp/minimal_compat_environment.h>
#include <lib/inspect/cpp/reader.h>

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

class TestPowerArgsDriver2 : public fdf::DriverBase2 {
 public:
  TestPowerArgsDriver2() : fdf::DriverBase2("test_power_args_driver2") {}

  static DriverRegistration GetDriverRegistration() {
    return FUCHSIA_DRIVER_REGISTRATION_V1(
        fdf_internal::DriverServer2<TestPowerArgsDriver2>::initialize,
        fdf_internal::DriverServer2<TestPowerArgsDriver2>::destroy);
  }

  zx::result<> Start(fdf::DriverContext context) override {
    has_power_args_ = context.has_power_args();
    runner_ = context.take_power_element_runner();
    second_runner_ = context.take_power_element_runner();
    lessor_ = context.take_power_element_lessor();
    second_lessor_ = context.take_power_element_lessor();
    token_ = context.power_element_token();
    return zx::ok();
  }

  bool has_power_args() const { return has_power_args_; }
  const std::optional<fidl::ServerEnd<fuchsia_power_broker::ElementRunner>>& runner() const {
    return runner_;
  }
  const std::optional<fidl::ServerEnd<fuchsia_power_broker::ElementRunner>>& second_runner() const {
    return second_runner_;
  }
  const std::optional<fidl::ClientEnd<fuchsia_power_broker::Lessor>>& lessor() const {
    return lessor_;
  }
  const std::optional<fidl::ClientEnd<fuchsia_power_broker::Lessor>>& second_lessor() const {
    return second_lessor_;
  }
  const std::optional<fuchsia_power_broker::DependencyToken>& token() const { return token_; }

 private:
  bool has_power_args_ = false;
  std::optional<fidl::ServerEnd<fuchsia_power_broker::ElementRunner>> runner_;
  std::optional<fidl::ServerEnd<fuchsia_power_broker::ElementRunner>> second_runner_;
  std::optional<fidl::ClientEnd<fuchsia_power_broker::Lessor>> lessor_;
  std::optional<fidl::ClientEnd<fuchsia_power_broker::Lessor>> second_lessor_;
  std::optional<fuchsia_power_broker::DependencyToken> token_;
};

struct PowerArgsFixtureConfig final {
  using DriverType = TestPowerArgsDriver2;
  using EnvironmentType = fdf_testing::MinimalCompatEnvironment;
};

TEST(DriverBase2Test, PowerElementArgsAbsent) {
  fdf_testing::ForegroundDriverTest<PowerArgsFixtureConfig> driver_test;
  ASSERT_EQ(ZX_OK, driver_test.StartDriver().status_value());
  EXPECT_FALSE(driver_test.driver()->has_power_args());
  EXPECT_FALSE(driver_test.driver()->runner().has_value());
  EXPECT_FALSE(driver_test.driver()->lessor().has_value());
  EXPECT_FALSE(driver_test.driver()->token().has_value());
  ASSERT_EQ(ZX_OK, driver_test.StopDriver().status_value());
}

TEST(DriverBase2Test, PowerElementArgsTokenOnly) {
  fdf_testing::ForegroundDriverTest<PowerArgsFixtureConfig> driver_test;
  ASSERT_EQ(ZX_OK, driver_test
                       .StartDriverWithCustomStartArgs([](fdf::DriverStartArgs& args) {
                         zx::event token;
                         ASSERT_EQ(ZX_OK, zx::event::create(0, &token));
                         fuchsia_driver_framework::PowerElementArgs power_args;
                         power_args.token(std::move(token));
                         args.power_element_args(std::move(power_args));
                       })
                       .status_value());
  EXPECT_TRUE(driver_test.driver()->has_power_args());
  EXPECT_FALSE(driver_test.driver()->runner().has_value());
  EXPECT_FALSE(driver_test.driver()->lessor().has_value());
  ASSERT_TRUE(driver_test.driver()->token().has_value());
  EXPECT_TRUE(driver_test.driver()->token()->is_valid());
  ASSERT_EQ(ZX_OK, driver_test.StopDriver().status_value());
}

TEST(DriverBase2Test, PowerElementArgsRunnerAndLessorOnly) {
  fdf_testing::ForegroundDriverTest<PowerArgsFixtureConfig> driver_test;
  ASSERT_EQ(ZX_OK, driver_test
                       .StartDriverWithCustomStartArgs([](fdf::DriverStartArgs& args) {
                         auto [runner_client, runner_server] =
                             fidl::Endpoints<fuchsia_power_broker::ElementRunner>::Create();
                         auto [lessor_client, lessor_server] =
                             fidl::Endpoints<fuchsia_power_broker::Lessor>::Create();
                         fuchsia_driver_framework::PowerElementArgs power_args;
                         power_args.runner_server(std::move(runner_server));
                         power_args.lessor_client(std::move(lessor_client));
                         args.power_element_args(std::move(power_args));
                       })
                       .status_value());
  EXPECT_TRUE(driver_test.driver()->has_power_args());
  ASSERT_TRUE(driver_test.driver()->runner().has_value());
  EXPECT_TRUE(driver_test.driver()->runner()->is_valid());
  EXPECT_FALSE(driver_test.driver()->second_runner().has_value());
  ASSERT_TRUE(driver_test.driver()->lessor().has_value());
  EXPECT_TRUE(driver_test.driver()->lessor()->is_valid());
  EXPECT_FALSE(driver_test.driver()->second_lessor().has_value());
  EXPECT_FALSE(driver_test.driver()->token().has_value());
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

// The DriverServer2 serving the most recently initialized InspectHealthCheckDriver2. Tests use it
// to bind a second Driver channel so they can exercise the real Driver.HealthCheck dispatch path
// (including Inspect mirroring) while fdf_testing::ForegroundDriverTest manages the lifecycle.
void* g_inspect_hc_driver_server = nullptr;

class InspectHealthCheckDriver2 : public fdf::DriverBase2 {
 public:
  using Server = fdf_internal::DriverServer2<InspectHealthCheckDriver2>;

  InspectHealthCheckDriver2() : fdf::DriverBase2("inspect_hc_driver2") {}

  static DriverRegistration GetDriverRegistration() {
    return FUCHSIA_DRIVER_REGISTRATION_V1(Initialize, Destroy);
  }

  static void* Initialize(fdf_handle_t server_handle) {
    g_inspect_hc_driver_server = Server::initialize(server_handle);
    return g_inspect_hc_driver_server;
  }

  static void Destroy(void* token) {
    g_inspect_hc_driver_server = nullptr;
    Server::destroy(token);
  }

  zx::result<> Start(fdf::DriverContext context) override {
    component_inspector_.emplace(context.CreateInspector(this, inspector_));
    return zx::ok();
  }

  // PING replies OK, DEEP replies a detailed UNHEALTHY response, and FAST replies an error.
  void HealthCheck(fuchsia_driver_framework::HealthCheckRequest request,
                   HealthCheckCompleter completer) override {
    switch (request) {
      case fuchsia_driver_framework::HealthCheckRequest::kPing:
        completer(fit::ok(fuchsia_driver_framework::HealthCheckResponse{}));
        return;
      case fuchsia_driver_framework::HealthCheckRequest::kDeep:
        completer(fit::ok(fuchsia_driver_framework::HealthCheckResponse{{
            .diagnostics =
                std::vector<fuchsia_driver_framework::HealthDiagnosticItem>{
                    {{.key = "pipe_state",
                      .value = fuchsia_driver_framework::HealthDiagnosticValue::WithStringValue(
                          "stalled")}},
                    {{.key = "drift_ns",
                      .value = fuchsia_driver_framework::HealthDiagnosticValue::WithIntValue(-42)}},
                    {{.key = "vsync_count",
                      .value =
                          fuchsia_driver_framework::HealthDiagnosticValue::WithUintValue(100u)}},
                    {{.key = "panel_powered",
                      .value =
                          fuchsia_driver_framework::HealthDiagnosticValue::WithBoolValue(true)}},
                    {{.key = "last_vsync",
                      .value = fuchsia_driver_framework::HealthDiagnosticValue::WithTimestamp(
                          zx::time_boot(123456789))}},
                },
            .conditions =
                std::vector<fuchsia_driver_framework::HealthCondition>{
                    {{
                        .status = fuchsia_driver_framework::HealthStatus::kUnhealthy,
                        .reason_code = "VSYNC_STALLED",
                        .message = "No VSYNC interrupt in 500ms",
                        .diagnostics =
                            std::vector<fuchsia_driver_framework::HealthDiagnosticItem>{
                                {{.key = "stall_ms",
                                  .value = fuchsia_driver_framework::HealthDiagnosticValue::
                                      WithUintValue(500u)}},
                            },
                        .first_observed = zx::time_boot(1000),
                        .last_observed = zx::time_boot(2000),
                        .occurrence_count = 3,
                    }},
                    {{
                        .status = fuchsia_driver_framework::HealthStatus::kDegraded,
                        .reason_code = "TE_JITTER",
                    }},
                },
        }}));
        return;
      default:
        completer(fit::error(fuchsia_driver_framework::HealthCheckError::kInternalError));
        return;
    }
  }

  const inspect::Inspector& inspector() const { return inspector_; }

 private:
  inspect::Inspector inspector_;
  std::optional<inspect::ComponentInspector> component_inspector_;
};

struct InspectHealthCheckFixtureConfig final {
  using DriverType = InspectHealthCheckDriver2;
  using EnvironmentType = fdf_testing::MinimalCompatEnvironment;
};

TEST(DriverBase2Test, HealthCheckMirroredToInspect) {
  fdf_testing::ForegroundDriverTest<InspectHealthCheckFixtureConfig> driver_test;
  ASSERT_EQ(ZX_OK, driver_test.StartDriver().status_value());

  auto* server = static_cast<InspectHealthCheckDriver2::Server*>(g_inspect_hc_driver_server);
  ASSERT_NE(nullptr, server);
  auto [client_end, server_end] = fdf::Endpoints<fuchsia_driver_framework::Driver>::Create();
  fdf::ServerBindingRef<fuchsia_driver_framework::Driver> binding =
      fdf::BindServer(fdf::Dispatcher::GetCurrent()->get(), std::move(server_end), server);
  fdf::WireClient<fuchsia_driver_framework::Driver> client(std::move(client_end),
                                                           fdf::Dispatcher::GetCurrent()->get());

  // Sends Driver.HealthCheck at `level` and returns whether the driver replied successfully.
  auto run_health_check = [&](fuchsia_driver_framework::wire::HealthCheckRequest level) {
    fdf::Arena arena('TEST');
    std::optional<bool> reply_ok;
    client.buffer(arena)->HealthCheck(level).Then(
        [&reply_ok](fdf::WireUnownedResult<fuchsia_driver_framework::Driver::HealthCheck>& result) {
          ASSERT_TRUE(result.ok());
          reply_ok = result->is_ok();
        });
    driver_test.runtime().RunUntilIdle();
    return reply_ok;
  };

  auto read_health_check_node = [&]() {
    auto hierarchy = inspect::ReadFromVmo(driver_test.driver()->inspector().DuplicateVmo());
    EXPECT_TRUE(hierarchy.is_ok());
    return std::move(hierarchy.value());
  };

  // Returns the string property `name` of the "driver_health_check" node, if present.
  auto read_string = [&](const std::string& name) -> std::optional<std::string> {
    auto hierarchy = read_health_check_node();
    const auto* hc_node = hierarchy.GetByPath({"driver_health_check"});
    if (hc_node == nullptr) {
      return std::nullopt;
    }
    const auto* prop = hc_node->node().get_property<inspect::StringPropertyValue>(name);
    if (prop == nullptr) {
      return std::nullopt;
    }
    return prop->value();
  };

  // 1. An error reply records status="ERROR", the error, and the level.
  EXPECT_EQ(std::optional(false),
            run_health_check(fuchsia_driver_framework::wire::HealthCheckRequest::kFast));
  EXPECT_EQ("ERROR", read_string("status"));
  EXPECT_EQ("INTERNAL_ERROR", read_string("error"));
  EXPECT_EQ("FAST", read_string("level"));

  // 2. A successful PING replaces the node with status="OK" and the level.
  EXPECT_EQ(std::optional(true),
            run_health_check(fuchsia_driver_framework::wire::HealthCheckRequest::kPing));
  EXPECT_EQ("OK", read_string("status"));
  EXPECT_EQ("PING", read_string("level"));
  EXPECT_EQ(std::nullopt, read_string("error"));

  // 3. A detailed reply replaces the node with status, whole-check diagnostics, and one child per
  //    condition under "conditions".
  EXPECT_EQ(std::optional(true),
            run_health_check(fuchsia_driver_framework::wire::HealthCheckRequest::kDeep));
  auto check_detailed_node = [&]() {
    auto hierarchy = read_health_check_node();
    const auto* hc_node = hierarchy.GetByPath({"driver_health_check"});
    ASSERT_NE(nullptr, hc_node);
    EXPECT_EQ("UNHEALTHY",
              hc_node->node().get_property<inspect::StringPropertyValue>("status")->value());

    const auto* diag_node = hierarchy.GetByPath({"driver_health_check", "diagnostics"});
    ASSERT_NE(nullptr, diag_node);
    EXPECT_EQ("stalled",
              diag_node->node().get_property<inspect::StringPropertyValue>("pipe_state")->value());
    EXPECT_EQ(-42, diag_node->node().get_property<inspect::IntPropertyValue>("drift_ns")->value());
    EXPECT_EQ(100u,
              diag_node->node().get_property<inspect::UintPropertyValue>("vsync_count")->value());
    EXPECT_EQ(true,
              diag_node->node().get_property<inspect::BoolPropertyValue>("panel_powered")->value());
    // Timestamp diagnostic values are recorded as int properties.
    EXPECT_EQ(123456789,
              diag_node->node().get_property<inspect::IntPropertyValue>("last_vsync")->value());

    const auto* vsync_node =
        hierarchy.GetByPath({"driver_health_check", "conditions", "VSYNC_STALLED"});
    ASSERT_NE(nullptr, vsync_node);
    EXPECT_EQ("UNHEALTHY",
              vsync_node->node().get_property<inspect::StringPropertyValue>("status")->value());
    EXPECT_EQ("No VSYNC interrupt in 500ms",
              vsync_node->node().get_property<inspect::StringPropertyValue>("message")->value());
    EXPECT_EQ(
        1000,
        vsync_node->node().get_property<inspect::IntPropertyValue>("first_observed")->value());
    EXPECT_EQ(2000,
              vsync_node->node().get_property<inspect::IntPropertyValue>("last_observed")->value());
    EXPECT_EQ(
        3u,
        vsync_node->node().get_property<inspect::UintPropertyValue>("occurrence_count")->value());
    const auto* vsync_diag =
        hierarchy.GetByPath({"driver_health_check", "conditions", "VSYNC_STALLED", "diagnostics"});
    ASSERT_NE(nullptr, vsync_diag);
    EXPECT_EQ(500u,
              vsync_diag->node().get_property<inspect::UintPropertyValue>("stall_ms")->value());

    const auto* jitter_node =
        hierarchy.GetByPath({"driver_health_check", "conditions", "TE_JITTER"});
    ASSERT_NE(nullptr, jitter_node);
    EXPECT_EQ("DEGRADED",
              jitter_node->node().get_property<inspect::StringPropertyValue>("status")->value());
    EXPECT_EQ(nullptr, jitter_node->node().get_property<inspect::StringPropertyValue>("message"));
    EXPECT_EQ(nullptr,
              jitter_node->node().get_property<inspect::IntPropertyValue>("first_observed"));
  };
  check_detailed_node();
  EXPECT_EQ("DEEP", read_string("level"));

  // 4. A later error reply replaces the recorded response.
  EXPECT_EQ(std::optional(false),
            run_health_check(fuchsia_driver_framework::wire::HealthCheckRequest::kFast));
  EXPECT_EQ("ERROR", read_string("status"));
  EXPECT_EQ("INTERNAL_ERROR", read_string("error"));
  EXPECT_EQ(nullptr, read_health_check_node().GetByPath({"driver_health_check", "conditions"}));
  EXPECT_EQ(nullptr, read_health_check_node().GetByPath({"driver_health_check", "diagnostics"}));

  client = {};
  binding.Unbind();
  driver_test.runtime().RunUntilIdle();
  ASSERT_EQ(ZX_OK, driver_test.StopDriver().status_value());
}

class HealthAlertEventHandler
    : public fdf::WireAsyncEventHandler<fuchsia_driver_framework::Driver> {
 public:
  void OnHealthAlert(
      fidl::WireEvent<fuchsia_driver_framework::Driver::OnHealthAlert>* event) override {
    alerts.push_back(fidl::ToNatural(event->alert));
  }

  void handle_unknown_event(
      fidl::UnknownEventMetadata<fuchsia_driver_framework::Driver> metadata) override {}

  std::vector<fuchsia_driver_framework::HealthAlert> alerts;
};

TEST(DriverBase2Test, ReportHealthAlertEmitsEventOverDriverChannel) {
  fdf_testing::DriverRuntime runtime;
  auto [client_end, server_end] = fdf::Endpoints<fuchsia_driver_framework::Driver>::Create();
  void* token =
      fdf_internal::DriverServer2<TestDriver2>::initialize(server_end.TakeChannel().release());
  ASSERT_NE(nullptr, token);

  HealthAlertEventHandler event_handler;
  fdf::WireClient<fuchsia_driver_framework::Driver> client(
      std::move(client_end), fdf::Dispatcher::GetCurrent()->get(), &event_handler);

  auto* server = static_cast<fdf_internal::DriverServer2<TestDriver2>*>(token);
  auto* driver = static_cast<TestDriver2*>(server->GetDriverBaseImpl());
  ASSERT_NE(nullptr, driver);

  driver->ReportHealthAlert(fuchsia_driver_framework::HealthAlert::WithSelfFault(
      fuchsia_driver_framework::HealthCheckResponse{{
          .conditions = {{{{
              .status = fuchsia_driver_framework::HealthStatus::kUnhealthy,
              .reason_code = "DMA_RING_STALLED",
              .message = "TX ring 0 made no progress for 500ms",
          }}}},
      }}));
  driver->ReportHealthAlert(fuchsia_driver_framework::HealthAlert::WithUpstreamNeighborFault(
      fuchsia_driver_framework::HealthCheckResponse{{
          .conditions = {{{{
              .status = fuchsia_driver_framework::HealthStatus::kDegraded,
              .reason_code = "UPSTREAM_CLOCK_MISSING",
          }}}},
      }}));

  runtime.RunUntilIdle();
  ASSERT_EQ(2u, event_handler.alerts.size());
  ASSERT_EQ(fuchsia_driver_framework::HealthAlert::Tag::kSelfFault,
            event_handler.alerts[0].Which());
  ASSERT_TRUE(event_handler.alerts[0].self_fault()->conditions().has_value());
  ASSERT_EQ(1u, event_handler.alerts[0].self_fault()->conditions()->size());
  EXPECT_EQ(fuchsia_driver_framework::HealthStatus::kUnhealthy,
            event_handler.alerts[0].self_fault()->conditions()->at(0).status());
  EXPECT_EQ("DMA_RING_STALLED",
            event_handler.alerts[0].self_fault()->conditions()->at(0).reason_code());

  ASSERT_EQ(fuchsia_driver_framework::HealthAlert::Tag::kUpstreamNeighborFault,
            event_handler.alerts[1].Which());
  ASSERT_TRUE(event_handler.alerts[1].upstream_neighbor_fault()->conditions().has_value());
  ASSERT_EQ(1u, event_handler.alerts[1].upstream_neighbor_fault()->conditions()->size());
  EXPECT_EQ("UPSTREAM_CLOCK_MISSING",
            event_handler.alerts[1].upstream_neighbor_fault()->conditions()->at(0).reason_code());

  // After Stop clears the alert sink, ReportHealthAlert is a safe no-op.
  fdf::Arena arena('STOP');
  ASSERT_TRUE(client.buffer(arena)->Stop().ok());
  runtime.RunUntilIdle();
  driver->ReportHealthAlert(fuchsia_driver_framework::HealthAlert::WithDownstreamNeighborFault(
      fuchsia_driver_framework::HealthCheckResponse{}));
  runtime.RunUntilIdle();
  EXPECT_EQ(2u, event_handler.alerts.size());

  client = {};
  runtime.RunUntilIdle();
  fdf_internal::DriverServer2<TestDriver2>::destroy(token);
}

TEST(DriverBase2Test, ReportHealthAlertSelfFaultMirroredToInspect) {
  fdf_testing::ForegroundDriverTest<InspectHealthCheckFixtureConfig> driver_test;
  ASSERT_EQ(ZX_OK, driver_test.StartDriver().status_value());

  // Reporting a neighbor fault does not overwrite the driver's own health_check Inspect node.
  driver_test.driver()->ReportHealthAlert(
      fuchsia_driver_framework::HealthAlert::WithUpstreamNeighborFault(
          fuchsia_driver_framework::HealthCheckResponse{{
              .conditions = {{{{
                  .status = fuchsia_driver_framework::HealthStatus::kDegraded,
                  .reason_code = "NEIGHBOR_TIMEOUT",
              }}}},
          }}));
  {
    auto hierarchy = inspect::ReadFromVmo(driver_test.driver()->inspector().DuplicateVmo());
    ASSERT_TRUE(hierarchy.is_ok());
    EXPECT_EQ(nullptr, hierarchy.value().GetByPath({"driver_health_check"}));
  }

  // Reporting kSelfFault mirrors the fault details to root/driver_health_check in Inspect.
  driver_test.driver()->ReportHealthAlert(fuchsia_driver_framework::HealthAlert::WithSelfFault(
      fuchsia_driver_framework::HealthCheckResponse{{
          .conditions = {{{{
              .status = fuchsia_driver_framework::HealthStatus::kUnhealthy,
              .reason_code = "SELF_LINK_LOST",
              .message = "PCIe link down",
          }}}},
      }}));
  {
    auto hierarchy = inspect::ReadFromVmo(driver_test.driver()->inspector().DuplicateVmo());
    ASSERT_TRUE(hierarchy.is_ok());
    const auto* hc_node = hierarchy.value().GetByPath({"driver_health_check"});
    ASSERT_NE(nullptr, hc_node);
    EXPECT_EQ("UNHEALTHY",
              hc_node->node().get_property<inspect::StringPropertyValue>("status")->value());
    EXPECT_EQ(nullptr, hc_node->node().get_property<inspect::StringPropertyValue>("level"));
    const auto* condition_node =
        hierarchy.value().GetByPath({"driver_health_check", "conditions", "SELF_LINK_LOST"});
    ASSERT_NE(nullptr, condition_node);
    EXPECT_EQ("UNHEALTHY",
              condition_node->node().get_property<inspect::StringPropertyValue>("status")->value());
    EXPECT_EQ(
        "PCIe link down",
        condition_node->node().get_property<inspect::StringPropertyValue>("message")->value());
  }

  ASSERT_EQ(ZX_OK, driver_test.StopDriver().status_value());
}

}  // namespace
