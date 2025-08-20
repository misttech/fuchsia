// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async_patterns/testing/cpp/dispatcher_bound.h>
#include <lib/driver/testing/cpp/driver_runtime.h>
#include <lib/driver/testing/cpp/internal/driver_lifecycle.h>
#include <lib/driver/testing/cpp/internal/test_environment.h>
#include <lib/driver/testing/cpp/test_node.h>
#include <lib/syslog/structured_backend/fuchsia_syslog.h>

#include <gtest/gtest.h>

#include "src/graphics/display/lib/driver-framework-migration-utils/logging/testing/dfv2-driver-with-logging.h"
#include "src/lib/diagnostics/fake-log-sink/cpp/fake_log_sink.h"
#include "src/lib/testing/predicates/status.h"

namespace display {

namespace {

using ::testing::Values;

// WARNING: Don't use this test as a template for new tests as it uses the old driver testing
// library.
class DriverLoggingTest : public ::testing::TestWithParam<fuchsia_logging::RawLogSeverity> {
 public:
  void SetUp() override {
    // Create start args
    node_server_.emplace("root");
    zx::result start_args = node_server_->CreateStartArgsAndServe();
    EXPECT_OK(start_args);

    // Start the test environment
    test_environment_.emplace();
    test_environment_.SyncCall([this, server = std::move(start_args->incoming_directory_server)](
                                   fdf_testing::internal::TestEnvironment* env) mutable {
      ASSERT_OK(env->AddLogSink([this](fidl::ServerEnd<fuchsia_logger::LogSink> server_end) {
        ASSERT_FALSE(log_sink_);
        log_sink_.emplace(GetParam(), std::move(server_end));
      }));

      zx::result result = env->Initialize(std::move(server));
      EXPECT_OK(result);
    });

    // Start driver
    zx::result start_result =
        runtime_.RunToCompletion(driver_.Start(std::move(start_args->start_args)));
    EXPECT_OK(start_result);
  }

  void TearDown() override {
    zx::result prepare_stop_result = runtime_.RunToCompletion(driver_.PrepareStop());
    EXPECT_OK(prepare_stop_result);

    test_environment_.reset();
    node_server_.reset();

    runtime_.ShutdownAllDispatchers(fdf::Dispatcher::GetCurrent()->get());
  }

  fdf_testing::internal::DriverUnderTest<testing::Dfv2DriverWithLogging>& driver() {
    return driver_;
  }

 private:
  // Attaches a foreground dispatcher for us automatically.
  fdf_testing::DriverRuntime runtime_;

  std::optional<fuchsia_logging::FakeLogSink> log_sink_;

  // We have to use a separate dispatcher to handle the log sink connection because we wait for the
  // interest synchronously when constructing the driver.
  fdf::SynchronizedDispatcher test_environment_dispatcher_;
  async_patterns::TestDispatcherBound<fdf_testing::internal::TestEnvironment> test_environment_{
      runtime_.StartBackgroundDispatcher()->async_dispatcher()};

  // These will use the foreground dispatcher.
  std::optional<fdf_testing::TestNode> node_server_;
  fdf_testing::internal::DriverUnderTest<testing::Dfv2DriverWithLogging> driver_;
};

TEST_P(DriverLoggingTest, MinimumLogLevel) {
  EXPECT_EQ(driver()->LogTrace(), GetParam() <= FUCHSIA_LOG_TRACE);
  EXPECT_EQ(driver()->LogDebug(), GetParam() <= FUCHSIA_LOG_DEBUG);
  EXPECT_EQ(driver()->LogInfo(), GetParam() <= FUCHSIA_LOG_INFO);
  EXPECT_EQ(driver()->LogWarning(), GetParam() <= FUCHSIA_LOG_WARNING);
  EXPECT_EQ(driver()->LogError(), GetParam() <= FUCHSIA_LOG_ERROR);
}

INSTANTIATE_TEST_SUITE_P(, DriverLoggingTest,
                         Values(FUCHSIA_LOG_TRACE, FUCHSIA_LOG_DEBUG, FUCHSIA_LOG_INFO,
                                FUCHSIA_LOG_WARNING, FUCHSIA_LOG_ERROR));

}  // namespace

}  // namespace display
