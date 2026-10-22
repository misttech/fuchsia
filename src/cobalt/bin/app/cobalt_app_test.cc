// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/cobalt/bin/app/cobalt_app.h"

#include <fuchsia/metrics/cpp/fidl.h>
#include <fuchsia/process/lifecycle/cpp/fidl.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/inspect/testing/cpp/inspect.h>
#include <lib/sys/cpp/testing/component_context_provider.h>

#include <gmock/gmock.h>
#include <gtest/gtest.h>
#include <sdk/lib/sys/cpp/testing/service_directory_provider.h>

#include "src/cobalt/bin/app/diagnostics_impl.h"
#include "src/cobalt/bin/app/testapp_metrics_registry.cb.h"
#include "src/cobalt/bin/testing/fake_clock.h"
#include "src/cobalt/bin/testing/fake_http_loader.h"
#include "src/lib/cobalt/cpp/metric_event_builder.h"
#include "src/lib/files/directory.h"
#include "src/lib/files/file.h"
#include "src/lib/files/path.h"
#include "src/lib/testing/loop_fixture/test_loop_fixture.h"
#include "src/public/cobalt_config.h"
#include "third_party/cobalt/src/public/testing/fake_cobalt_service.h"

namespace cobalt {
namespace {

const char kTestDir[] = "/tmp/cobalt_app_test";
constexpr char kTestPublicKeysUrl[] = "https://example.com/cobalt/public_keys.pb";

bool WriteFile(const std::string& file, const std::string& to_write) {
  return files::WriteFile(files::JoinPath(kTestDir, file), to_write.c_str(), to_write.length());
}

}  // namespace

class CreateCobaltConfigTest : public gtest::TestLoopFixture {
 public:
  CreateCobaltConfigTest() : ::gtest::TestLoopFixture(), clock_(dispatcher(), inspect::Node()) {}

 protected:
  void SetUp() override {
    ASSERT_TRUE(files::DeletePath(kTestDir, true));
    ASSERT_TRUE(files::CreateDirectory(kTestDir));

    loader_ = std::make_unique<FakeHTTPLoader>(dispatcher());
    service_directory_provider_.AddService(loader_->GetHandler());
  }

  CobaltConfig CreateCobaltConfig(
      const std::string& metrics_registry_path, const FuchsiaConfigurationData& configuration_data,
      std::chrono::seconds target_interval, std::chrono::seconds min_interval,
      std::chrono::seconds initial_interval, float jitter, size_t event_aggregator_backfill_days,
      bool use_memory_observation_store, size_t max_bytes_per_observation_store,
      const std::string& product_name, const std::string& board_name, const std::string& version) {
    return CobaltApp::CreateCobaltConfig(
        dispatcher(), metrics_registry_path, configuration_data, &clock_,
        [this] {
          fuchsia::net::http::LoaderSyncPtr loader_sync;
          service_directory_provider_.service_directory()->Connect(loader_sync.NewRequest());
          return loader_sync;
        },
        UploadScheduleConfig{target_interval, min_interval, initial_interval, jitter},
        event_aggregator_backfill_days, /*test_dont_backfill_empty_reports=*/false,
        use_memory_observation_store, max_bytes_per_observation_store,
        cobalt::kDefaultStorageQuotas, product_name, board_name, version,
        std::make_unique<DiagnosticsImpl>(inspect::Node()));
  }

  std::unique_ptr<KeyFetcher> ConfigureKeyRotation(TargetPipelineInterface* target_pipeline,
                                                   util::SystemClockInterface* encryption_clock) {
    return CobaltApp::ConfigureKeyRotation(dispatcher(), target_pipeline,
                                           std::make_unique<lib::EmptyHTTPClient>(),
                                           encryption_clock, kTestDir, kTestPublicKeysUrl);
  }

  sys::testing::ServiceDirectoryProvider service_directory_provider_;
  std::unique_ptr<FakeHTTPLoader> loader_;

  sys::testing::ComponentContextProvider context_provider_;
  FuchsiaSystemClock clock_;
};

TEST_F(CreateCobaltConfigTest, Devel) {
  ASSERT_TRUE(WriteFile("cobalt_environment", "DEVEL"));
  ASSERT_TRUE(WriteFile("config.json", "{\"release_stage\": \"DEBUG\"}"));
  FuchsiaConfigurationData configuration_data(kTestDir, kTestDir);
  CobaltConfig config =
      CreateCobaltConfig("/pkg/data/testapp_metrics_registry.pb", configuration_data,
                         std::chrono::seconds(1), std::chrono::seconds(2), std::chrono::seconds(3),
                         0, 4, true, 1048000, "core", "x64", "0.1.2");
  EXPECT_EQ(config.target_pipeline->environment(), system_data::Environment::DEVEL);
  EXPECT_EQ(config.release_stage, ReleaseStage::DEBUG);
}

TEST_F(CreateCobaltConfigTest, Local) {
  ASSERT_TRUE(WriteFile("cobalt_environment", "LOCAL"));
  ASSERT_TRUE(WriteFile("config.json", "{\"release_stage\": \"DEBUG\"}"));
  FuchsiaConfigurationData configuration_data(kTestDir, kTestDir);
  CobaltConfig config =
      CreateCobaltConfig("/pkg/data/testapp_metrics_registry.pb", configuration_data,
                         std::chrono::seconds(1), std::chrono::seconds(2), std::chrono::seconds(3),
                         0, 4, true, 1048000, "core", "x64", "0.1.2");
  EXPECT_EQ(config.target_pipeline->environment(), system_data::Environment::LOCAL);
  EXPECT_EQ(config.release_stage, ReleaseStage::DEBUG);
}

TEST_F(CreateCobaltConfigTest, GA) {
  ASSERT_TRUE(WriteFile("cobalt_environment", "DEVEL"));
  ASSERT_TRUE(WriteFile("config.json", "{\"release_stage\": \"GA\"}"));
  FuchsiaConfigurationData configuration_data(kTestDir, kTestDir);
  CobaltConfig config =
      CreateCobaltConfig("/pkg/data/testapp_metrics_registry.pb", configuration_data,
                         std::chrono::seconds(1), std::chrono::seconds(2), std::chrono::seconds(3),
                         0, 4, true, 1048000, "core", "x64", "0.1.2");
  EXPECT_EQ(config.target_pipeline->environment(), system_data::Environment::DEVEL);
  EXPECT_EQ(config.release_stage, ReleaseStage::GA);
}

TEST_F(CreateCobaltConfigTest, ConfigFields) {
  ASSERT_TRUE(WriteFile("cobalt_environment", "DEVEL"));
  ASSERT_TRUE(WriteFile("config.json", "{\"release_stage\": \"GA\"}"));
  float test_jitter = .3;
  FuchsiaConfigurationData configuration_data(kTestDir, kTestDir);
  CobaltConfig config =
      CreateCobaltConfig("/pkg/data/testapp_metrics_registry.pb", configuration_data,
                         std::chrono::seconds(1), std::chrono::seconds(2), std::chrono::seconds(3),
                         test_jitter, 4, true, 1048000, "core", "x64", "0.1.2");
  EXPECT_EQ(config.upload_schedule_cfg.jitter, test_jitter);
  EXPECT_TRUE(config.defer_observation_encryption);
}

TEST_F(CreateCobaltConfigTest, BuildTypeUser) {
  ASSERT_TRUE(WriteFile("cobalt_environment", "DEVEL"));
  ASSERT_TRUE(WriteFile("type", "user\n"));
  FuchsiaConfigurationData configuration_data(kTestDir, kTestDir, kTestDir);
  CobaltConfig config =
      CreateCobaltConfig("/pkg/data/testapp_metrics_registry.pb", configuration_data,
                         std::chrono::seconds(1), std::chrono::seconds(2), std::chrono::seconds(3),
                         0, 4, true, 1048000, "core", "x64", "0.1.2");
  EXPECT_EQ(config.build_type, SystemProfile::USER);
}

TEST_F(CreateCobaltConfigTest, UnknownBuildType) {
  ASSERT_TRUE(WriteFile("cobalt_environment", "DEVEL"));
  FuchsiaConfigurationData configuration_data(kTestDir, kTestDir, kTestDir);
  CobaltConfig config =
      CreateCobaltConfig("/pkg/data/testapp_metrics_registry.pb", configuration_data,
                         std::chrono::seconds(1), std::chrono::seconds(2), std::chrono::seconds(3),
                         0, 4, true, 1048000, "core", "x64", "0.1.2");
  EXPECT_EQ(config.build_type, SystemProfile::UNKNOWN_TYPE);
}

TEST_F(CreateCobaltConfigTest, InvalidBuildType) {
  ASSERT_TRUE(WriteFile("cobalt_environment", "DEVEL"));
  ASSERT_TRUE(WriteFile("type", "invalid"));
  FuchsiaConfigurationData configuration_data(kTestDir, kTestDir, kTestDir);
  CobaltConfig config =
      CreateCobaltConfig("/pkg/data/testapp_metrics_registry.pb", configuration_data,
                         std::chrono::seconds(1), std::chrono::seconds(2), std::chrono::seconds(3),
                         0, 4, true, 1048000, "core", "x64", "0.1.2");
  EXPECT_EQ(config.build_type, SystemProfile::OTHER_TYPE);
}

TEST_F(CreateCobaltConfigTest, ConfigureKeyRotationNullTargetPipelineReturnsNull) {
  util::SystemClock encryption_clock;
  EXPECT_EQ(ConfigureKeyRotation(/*target_pipeline=*/nullptr, &encryption_clock), nullptr);
}

TEST_F(CreateCobaltConfigTest, ConfigureKeyRotationLocalReturnsNull) {
  ASSERT_TRUE(WriteFile("cobalt_environment", "LOCAL"));
  ASSERT_TRUE(WriteFile("config.json", "{\"release_stage\": \"DEBUG\"}"));
  FuchsiaConfigurationData configuration_data(kTestDir, kTestDir);
  CobaltConfig config =
      CreateCobaltConfig("/pkg/data/testapp_metrics_registry.pb", configuration_data,
                         std::chrono::seconds(1), std::chrono::seconds(2), std::chrono::seconds(3),
                         0, 4, true, 1048000, "core", "x64", "0.1.2");
  util::SystemClock encryption_clock;
  std::unique_ptr<KeyFetcher> key_fetcher =
      ConfigureKeyRotation(config.target_pipeline.get(), &encryption_clock);
  EXPECT_EQ(key_fetcher, nullptr);
  EXPECT_EQ(config.target_pipeline->TakeShufflerEncryptedMessageMaker(), nullptr);
  EXPECT_EQ(config.target_pipeline->TakeAnalyzerEncryptedMessageMaker(), nullptr);
}

TEST_F(CreateCobaltConfigTest, ConfigureKeyRotationDevelAttachesDynamicMakers) {
  ASSERT_TRUE(WriteFile("cobalt_environment", "DEVEL"));
  ASSERT_TRUE(WriteFile("config.json", "{\"release_stage\": \"DEBUG\"}"));
  FuchsiaConfigurationData configuration_data(kTestDir, kTestDir);
  util::SystemClock encryption_clock;
  CobaltConfig config =
      CreateCobaltConfig("/pkg/data/testapp_metrics_registry.pb", configuration_data,
                         std::chrono::seconds(1), std::chrono::seconds(2), std::chrono::seconds(3),
                         0, 4, true, 1048000, "core", "x64", "0.1.2");
  std::unique_ptr<util::EncryptedMessageMaker> shuffler_maker;
  std::unique_ptr<util::EncryptedMessageMaker> analyzer_maker;
  std::unique_ptr<KeyFetcher> key_fetcher =
      ConfigureKeyRotation(config.target_pipeline.get(), &encryption_clock);
  ASSERT_NE(key_fetcher, nullptr);
  shuffler_maker = config.target_pipeline->TakeShufflerEncryptedMessageMaker();
  analyzer_maker = config.target_pipeline->TakeAnalyzerEncryptedMessageMaker();
  ASSERT_NE(shuffler_maker, nullptr);
  ASSERT_NE(analyzer_maker, nullptr);

  Observation obs;
  obs.set_random_id("devel-bundled-check");
  EncryptedMessage encrypted;
  EXPECT_TRUE(analyzer_maker->Encrypt(obs, &encrypted));
  EXPECT_GT(encrypted.key_index(), 0u);
}

TEST_F(CreateCobaltConfigTest, ConfigureKeyRotationInvalidBundledKeyLeavesMakersUninitialized) {
  util::SystemClock encryption_clock;
  TargetPipeline target_pipeline(system_data::Environment::PROD, "not-a-valid-key-proto",
                                 /*analyzer_encryption_key_bytes=*/"",
                                 std::make_unique<lib::EmptyHTTPClient>(),
                                 /*clearcut_max_retries=*/1);
  std::unique_ptr<util::EncryptedMessageMaker> shuffler_maker;
  std::unique_ptr<util::EncryptedMessageMaker> analyzer_maker;
  std::unique_ptr<KeyFetcher> key_fetcher =
      ConfigureKeyRotation(&target_pipeline, &encryption_clock);
  ASSERT_NE(key_fetcher, nullptr);
  shuffler_maker = target_pipeline.TakeShufflerEncryptedMessageMaker();
  analyzer_maker = target_pipeline.TakeAnalyzerEncryptedMessageMaker();
  ASSERT_NE(shuffler_maker, nullptr);
  ASSERT_NE(analyzer_maker, nullptr);

  Observation obs;
  obs.set_random_id("uninitialized-fallback-check");
  EncryptedMessage encrypted;
  EXPECT_FALSE(analyzer_maker->Encrypt(obs, &encrypted));
}

using inspect::testing::ChildrenMatch;
using inspect::testing::NameMatches;
using inspect::testing::NodeMatches;
using ::testing::UnorderedElementsAre;

class CobaltAppTest : public gtest::TestLoopFixture {
 public:
  CobaltAppTest()
      : ::gtest::TestLoopFixture(),
        clock_(new FakeFuchsiaSystemClock(dispatcher())),
        fake_service_(new testing::FakeCobaltService()),
        shuffler_maker_(&test_encryption_clock_),
        analyzer_maker_(&test_encryption_clock_),
        key_fetcher_(new KeyFetcher(dispatcher(), std::make_unique<lib::EmptyHTTPClient>(),
                                    &shuffler_maker_, &analyzer_maker_, &test_encryption_clock_,
                                    system_data::Environment::DEVEL, kTestDir, kTestPublicKeysUrl,
                                    KeyFetcher::ScheduleOptions{
                                        .max_initial_jitter = zx::sec(0),
                                        .steady_state_interval = zx::hour(24),
                                        .steady_state_jitter = zx::hour(2),
                                        .poll_interval = zx::hour(1),
                                        .boot_clock = zx::clock::get_boot,
                                    })),
        cobalt_app_(
            context_provider_.TakeContext(), dispatcher(), lifecycle_.NewRequest(dispatcher()),
            []() { /* Stub shutdown callback */ }, inspector_.GetRoot().CreateChild("cobalt_app"),
            inspect::Node(), std::unique_ptr<testing::FakeCobaltService>(fake_service_),
            std::unique_ptr<FakeFuchsiaSystemClock>(clock_), true,
            /*test_dont_backfill_empty_reports=*/false, false,
            std::make_unique<util::SystemClock>(), std::unique_ptr<KeyFetcher>(key_fetcher_)) {}

 protected:
  void SetUp() override {
    ASSERT_TRUE(files::DeletePath(kTestDir, /*recursive=*/true));
    ASSERT_TRUE(files::CreateDirectory(kTestDir));
    context_provider_.ConnectToPublicService(metric_event_logger_factory_.NewRequest());
    ASSERT_NE(metric_event_logger_factory_.get(), nullptr);
  }

  // Call the MetricEventLoggerFactory to create a Logger connection.
  //
  // If experiments are provided, uses CreateMetricEventLoggerWithExperiments
  // instead.
  fuchsia::metrics::MetricEventLoggerPtr GetMetricEventLogger(
      int project_id = testapp_registry::kProjectId, std::vector<uint32_t> experiment_ids = {}) {
    fpromise::result<void, fuchsia::metrics::Error> result;
    fuchsia::metrics::ProjectSpec project;
    project.set_customer_id(1);
    project.set_project_id(project_id);
    fuchsia::metrics::MetricEventLoggerPtr logger;
    if (experiment_ids.empty()) {
      metric_event_logger_factory_->CreateMetricEventLogger(
          std::move(project), logger.NewRequest(),
          [&](auto result_) { result = std::move(result_); });
    } else {
      metric_event_logger_factory_->CreateMetricEventLoggerWithExperiments(
          std::move(project), std::move(experiment_ids), logger.NewRequest(),
          [&](auto result_) { result = std::move(result_); });
    }
    RunLoopUntilIdle();
    EXPECT_TRUE(result.is_ok());
    EXPECT_NE(logger.get(), nullptr);
    return logger;
  }

  fuchsia::cobalt::Controller* GetCobaltController() { return cobalt_app_.controller_impl_.get(); }

  fuchsia::process::lifecycle::LifecyclePtr lifecycle_;
  sys::testing::ComponentContextProvider context_provider_;
  FakeFuchsiaSystemClock* clock_;
  testing::FakeCobaltService* fake_service_;
  util::SystemClock test_encryption_clock_;
  util::DynamicEncryptedMessageMaker shuffler_maker_;
  util::DynamicEncryptedMessageMaker analyzer_maker_;
  KeyFetcher* key_fetcher_;
  inspect::Inspector inspector_;
  CobaltApp cobalt_app_;
  fuchsia::metrics::MetricEventLoggerFactoryPtr metric_event_logger_factory_;
};

TEST_F(CobaltAppTest, SystemClockIsAccurate) {
  bool callback_invoked = false;
  GetCobaltController()->ListenForInitialized([&callback_invoked]() { callback_invoked = true; });
  EXPECT_FALSE(callback_invoked);
  EXPECT_EQ(key_fetcher_->last_fetch_status(), KeyFetchStatus::kUninitialized);

  // Give the clock time to become accurate.
  RunLoopUntilIdle();
  EXPECT_EQ(fake_service_->system_clock_is_accurate(), true);
  EXPECT_TRUE(callback_invoked);
  EXPECT_EQ(key_fetcher_->last_fetch_status(), KeyFetchStatus::kNetworkError);
}

TEST_F(CobaltAppTest, InspectData) {
  fpromise::result<inspect::Hierarchy> result = inspect::ReadFromVmo(inspector_.DuplicateVmo());
  ASSERT_TRUE(result.is_ok());
  EXPECT_THAT(
      result.take_value(),
      AllOf(NodeMatches(NameMatches("root")),
            ChildrenMatch(UnorderedElementsAre(AllOf(
                NodeMatches(NameMatches("cobalt_app")),
                ChildrenMatch(UnorderedElementsAre(NodeMatches(NameMatches("system_data")))))))));
}

TEST_F(CobaltAppTest, CreateMetricEventLogger) {
  logger::testing::FakeLogger* fake_logger = fake_service_->last_logger_created();
  EXPECT_EQ(fake_logger, nullptr);
  fuchsia::metrics::MetricEventLoggerPtr logger = GetMetricEventLogger();
  fake_logger = fake_service_->last_logger_created();
  EXPECT_NE(fake_logger, nullptr);
  EXPECT_EQ(fake_logger->call_count(), 0);
}

TEST_F(CobaltAppTest, CreateMetricEventLoggerWithExperiments) {
  logger::testing::FakeLogger* fake_logger = fake_service_->last_logger_created();
  EXPECT_EQ(fake_logger, nullptr);
  std::vector<uint32_t> test_experiments = {123456789, 987654321};
  fuchsia::metrics::MetricEventLoggerPtr logger =
      GetMetricEventLogger(testapp_registry::kProjectId, test_experiments);
  fake_logger = fake_service_->last_logger_created();
  EXPECT_NE(fake_logger, nullptr);
  EXPECT_EQ(fake_logger->call_count(), 0);
}

TEST_F(CobaltAppTest, CreateMetricEventLoggerNoValidLogger) {
  // Make sure that the CobaltService returns nullptr for the next call to NewLogger().
  fake_service_->FailNextNewLogger();

  fpromise::result<void, fuchsia::metrics::Error> result;
  fuchsia::metrics::ProjectSpec project;
  project.set_customer_id(1);
  project.set_project_id(987654321);
  fuchsia::metrics::MetricEventLoggerPtr logger;
  metric_event_logger_factory_->CreateMetricEventLogger(
      std::move(project), logger.NewRequest(), [&](auto result_) { result = std::move(result_); });
  RunLoopUntilIdle();
  EXPECT_TRUE(result.is_error());
  EXPECT_EQ(result.error(), fuchsia::metrics::Error::INVALID_ARGUMENTS);
}

TEST_F(CobaltAppTest, LogOccurrence) {
  fuchsia::metrics::MetricEventLoggerPtr logger = GetMetricEventLogger();
  logger::testing::FakeLogger* fake_logger = fake_service_->last_logger_created();
  ASSERT_NE(fake_logger, nullptr);
  EXPECT_EQ(fake_logger->call_count(), 0);

  fpromise::result<void, fuchsia::metrics::Error> result;
  logger->LogOccurrence(testapp_registry::kErrorOccurredNewMetricId, /*count=*/1,
                        /*event_codes=*/{2}, [&](auto result_) { result = std::move(result_); });
  RunLoopUntilIdle();
  ASSERT_TRUE(result.is_ok());
  EXPECT_EQ(fake_logger->call_count(), 1);
  auto event = fake_logger->last_event_logged();
  EXPECT_EQ(event.metric_id(), testapp_registry::kErrorOccurredNewMetricId);
  EXPECT_EQ(event.occurrence_event().count(), 1);
  EXPECT_THAT(event.occurrence_event().event_code(), ::testing::ElementsAre(2));
}

TEST_F(CobaltAppTest, LogInteger) {
  fuchsia::metrics::MetricEventLoggerPtr logger = GetMetricEventLogger();
  logger::testing::FakeLogger* fake_logger = fake_service_->last_logger_created();
  ASSERT_NE(fake_logger, nullptr);
  EXPECT_EQ(fake_logger->call_count(), 0);

  fpromise::result<void, fuchsia::metrics::Error> result;
  logger->LogInteger(testapp_registry::kUpdateDurationNewMetricId, /*value=*/-42,
                     /*event_codes=*/{3}, [&](auto result_) { result = std::move(result_); });
  RunLoopUntilIdle();
  ASSERT_TRUE(result.is_ok());
  EXPECT_EQ(fake_logger->call_count(), 1);
  auto event = fake_logger->last_event_logged();
  EXPECT_EQ(event.metric_id(), testapp_registry::kUpdateDurationNewMetricId);
  EXPECT_EQ(event.integer_event().value(), -42);
  EXPECT_THAT(event.integer_event().event_code(), ::testing::ElementsAre(3));
}

TEST_F(CobaltAppTest, LogIntegerHistogram) {
  fuchsia::metrics::MetricEventLoggerPtr logger = GetMetricEventLogger();
  logger::testing::FakeLogger* fake_logger = fake_service_->last_logger_created();
  ASSERT_NE(fake_logger, nullptr);
  EXPECT_EQ(fake_logger->call_count(), 0);

  fpromise::result<void, fuchsia::metrics::Error> result;
  std::vector<fuchsia::metrics::HistogramBucket> histogram;
  fuchsia::metrics::HistogramBucket entry;
  entry.index = 1;
  entry.count = 42;
  histogram.push_back(entry);
  logger->LogIntegerHistogram(testapp_registry::kBandwidthUsageNewMetricId, std::move(histogram),
                              /*event_codes=*/{4, 5},
                              [&](auto result_) { result = std::move(result_); });
  RunLoopUntilIdle();
  ASSERT_TRUE(result.is_ok());
  EXPECT_EQ(fake_logger->call_count(), 1);
  auto event = fake_logger->last_event_logged();
  EXPECT_EQ(event.metric_id(), testapp_registry::kBandwidthUsageNewMetricId);
  EXPECT_EQ(event.integer_histogram_event().buckets().size(), 1);
  EXPECT_EQ(event.integer_histogram_event().buckets(0).index(), 1);
  EXPECT_EQ(event.integer_histogram_event().buckets(0).count(), 42);
  EXPECT_THAT(event.integer_histogram_event().event_code(), ::testing::ElementsAre(4, 5));
}

TEST_F(CobaltAppTest, LogString) {
  fuchsia::metrics::MetricEventLoggerPtr logger = GetMetricEventLogger();
  logger::testing::FakeLogger* fake_logger = fake_service_->last_logger_created();
  ASSERT_NE(fake_logger, nullptr);
  EXPECT_EQ(fake_logger->call_count(), 0);

  fpromise::result<void, fuchsia::metrics::Error> result;
  logger->LogString(testapp_registry::kErrorOccurredComponentsMetricId,
                    /*string_value=*/"component", /*event_codes=*/{3},
                    [&](auto result_) { result = std::move(result_); });
  RunLoopUntilIdle();
  ASSERT_TRUE(result.is_ok());
  EXPECT_EQ(fake_logger->call_count(), 1);
  auto event = fake_logger->last_event_logged();
  EXPECT_EQ(event.metric_id(), testapp_registry::kErrorOccurredComponentsMetricId);
  EXPECT_EQ(event.string_event().string_value(), "component");
  EXPECT_THAT(event.string_event().event_code(), ::testing::ElementsAre(3));
}

TEST_F(CobaltAppTest, LogMetricEvents) {
  FX_LOGS(INFO) << "A logging statement";
  fuchsia::metrics::MetricEventLoggerPtr logger = GetMetricEventLogger();
  logger::testing::FakeLogger* fake_logger = fake_service_->last_logger_created();
  ASSERT_NE(fake_logger, nullptr);
  EXPECT_EQ(fake_logger->call_count(), 0);

  fpromise::result<void, fuchsia::metrics::Error> result;
  std::vector<fuchsia::metrics::MetricEvent> events;
  events.push_back(MetricEventBuilder(testapp_registry::kErrorOccurredNewMetricId)
                       .with_event_code(2)
                       .as_occurrence(1));
  events.push_back(MetricEventBuilder(testapp_registry::kErrorOccurredNewMetricId)
                       .with_event_code(2)
                       .as_occurrence(2));
  events.push_back(MetricEventBuilder(testapp_registry::kErrorOccurredNewMetricId)
                       .with_event_code(2)
                       .as_occurrence(3));
  events.push_back(MetricEventBuilder(testapp_registry::kErrorOccurredNewMetricId)
                       .with_event_code(2)
                       .as_occurrence(4));
  events.push_back(MetricEventBuilder(testapp_registry::kErrorOccurredNewMetricId)
                       .with_event_code(2)
                       .as_occurrence(5));
  logger->LogMetricEvents(std::move(events), [&](auto result_) { result = std::move(result_); });
  RunLoopUntilIdle();
  ASSERT_TRUE(result.is_ok());
  EXPECT_EQ(fake_logger->call_count(), 5);
  auto event = fake_logger->last_event_logged();
  EXPECT_EQ(event.metric_id(), testapp_registry::kErrorOccurredNewMetricId);
  EXPECT_EQ(event.occurrence_event().count(), 5);
  EXPECT_THAT(event.occurrence_event().event_code(), ::testing::ElementsAre(2));
}

TEST_F(CobaltAppTest, ShutDown) {
  EXPECT_EQ(fake_service_->is_shut_down(), false);

  fuchsia::metrics::MetricEventLoggerPtr metric_logger = GetMetricEventLogger();
  EXPECT_TRUE(metric_logger.is_bound());

  lifecycle_->Stop();
  RunLoopUntilIdle();
  EXPECT_EQ(fake_service_->is_shut_down(), true);
  EXPECT_FALSE(metric_logger.is_bound());
  EXPECT_FALSE(lifecycle_.is_bound());
}

TEST_F(CobaltAppTest, NoNewLoggersAfterShutDown) {
  lifecycle_->Stop();
  RunLoopUntilIdle();

  fuchsia::metrics::MetricEventLoggerPtr metric_logger = nullptr;
  fuchsia::metrics::MetricEventLoggerFactory_CreateMetricEventLogger_Result metrics_result;
  fuchsia::metrics::ProjectSpec project;
  project.set_customer_id(1);
  project.set_project_id(testapp_registry::kProjectId);

  metric_event_logger_factory_->CreateMetricEventLogger(
      std::move(project), metric_logger.NewRequest(),
      [&](fuchsia::metrics::MetricEventLoggerFactory_CreateMetricEventLogger_Result result) {
        metrics_result = std::move(result);
      });
  RunLoopUntilIdle();
  EXPECT_TRUE(metrics_result.is_err());
  EXPECT_EQ(metrics_result.err(), fuchsia::metrics::Error::SHUT_DOWN);
  EXPECT_FALSE(metric_logger.is_bound());
}

}  // namespace cobalt
