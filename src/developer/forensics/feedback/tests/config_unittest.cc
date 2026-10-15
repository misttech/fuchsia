// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/config.h"

#include <lib/inspect/testing/cpp/inspect.h>
#include <lib/syslog/cpp/macros.h>

#include <initializer_list>
#include <optional>
#include <string>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/developer/forensics/feedback/constants.h"
#include "src/developer/forensics/testing/unit_test_fixture.h"
#include "src/lib/files/scoped_temp_dir.h"
#include "third_party/rapidjson/include/rapidjson/document.h"
#include "third_party/rapidjson/include/rapidjson/stringbuffer.h"
#include "third_party/rapidjson/include/rapidjson/writer.h"

namespace forensics::feedback {
namespace {

using inspect::testing::BoolIs;
using inspect::testing::ChildrenMatch;
using inspect::testing::NameMatches;
using inspect::testing::NodeMatches;
using inspect::testing::PropertyList;
using inspect::testing::StringIs;
using inspect::testing::UintIs;
using testing::ElementsAreArray;
using testing::IsEmpty;
using testing::IsSupersetOf;

constexpr auto kConfigDisabled = CrashReportUploadPolicy::kDisabled;
constexpr auto kConfigEnabled = CrashReportUploadPolicy::kEnabled;
constexpr auto kConfigReadFromPrivacySettings = CrashReportUploadPolicy::kReadFromPrivacySettings;

auto BuildConfigMatcher(
    std::initializer_list<testing::Matcher<const inspect::PropertyValue&>> properties) {
  return ChildrenMatch(Contains(AllOf(
      NodeMatches(AllOf(NameMatches(kInspectConfigKey), PropertyList(IsSupersetOf(properties)))))));
}

class ConfigTest : public testing::Test {
 public:
  // Writes |config| to a file and returns the path of the config.
  std::string WriteConfig(const std::string& config) {
    std::string path;
    FX_CHECK(temp_dir_.NewTempFileWithData(config, &path));
    return path;
  }

 private:
  files::ScopedTempDir temp_dir_;
};

class SnapshotConfigTest : public ConfigTest {
 protected:
  std::optional<SnapshotConfig> ParseConfig(const std::string& config) {
    return GetSnapshotConfig(WriteConfig(config));
  }
};

class SnapshotExclusionConfigTest : public ConfigTest {
 protected:
  std::optional<SnapshotExclusionConfig> ParseConfig(const std::string& config) {
    return GetSnapshotExclusionConfig(WriteConfig(config));
  }
};

class TestConfig {
 public:
  explicit TestConfig(const char* raw_json) {
    json_.Parse(raw_json);
    FX_CHECK(!json_.HasParseError());
  }

  template <typename T>
  void Set(const char* key, T value) {
    FX_CHECK(json_.HasMember(key));
    json_[key] = rapidjson::Value(value);
  }

  void Set(const char* key, const char* value) {
    FX_CHECK(json_.HasMember(key));
    json_[key].SetString(value, json_.GetAllocator());
  }

  void AddMember(const char* key, const char* value) {
    FX_CHECK(!json_.HasMember(key));
    json_.AddMember(rapidjson::Value(key, json_.GetAllocator()),
                    rapidjson::Value(value, json_.GetAllocator()), json_.GetAllocator());
  }

  void EraseMember(const char* key) { FX_CHECK(json_.EraseMember(key)); }

  std::string ToString() const {
    rapidjson::StringBuffer buffer;
    rapidjson::Writer<rapidjson::StringBuffer> writer(buffer);
    json_.Accept(writer);
    return buffer.GetString();
  }

 private:
  rapidjson::Document json_;
};

TestConfig CreateDefaultConfig() {
  return TestConfig(R"({
    "persisted_logs_total_size_kib": 512,
    "report_persistence_max_cache_size_kib": 1,
    "report_persistence_max_tmp_size_kib": 1,
    "snapshot_persistence_max_cache_size_mib": 1,
    "snapshot_persistence_max_tmp_size_mib": 1,
    "spontaneous_reboot_reason": "spontaneous",
    "crash_report_upload_policy": "disabled",
    "daily_per_product_crash_report_quota": -1,
    "enable_data_redaction": false,
    "enable_hourly_snapshots": false,
    "enable_limit_inspect_data": false,
    "disk_endurance": "high",
    "remote_device_id_provider": false,
    "supports_user_initiated_poweroffs": false
  })");
}

class FeedbackConfigTest : public ConfigTest {
 protected:
  std::optional<FeedbackConfig> ParseConfig(const TestConfig& config) {
    return GetFeedbackConfig(WriteConfig(config.ToString()));
  }
};

using InspectConfigTest = UnitTestFixture;

TEST_F(FeedbackConfigTest, MissingCrashReportUploadPolicy) {
  TestConfig config = CreateDefaultConfig();
  config.EraseMember("crash_report_upload_policy");
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, MissingDailyPerProductCrashReportQuota) {
  TestConfig config = CreateDefaultConfig();
  config.EraseMember("daily_per_product_crash_report_quota");
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, MissingEnableDataRedaction) {
  TestConfig config = CreateDefaultConfig();
  config.EraseMember("enable_data_redaction");
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, MissingEnableHourlySnapshots) {
  TestConfig config = CreateDefaultConfig();
  config.EraseMember("enable_hourly_snapshots");
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, MissingEnableLimitInspectData) {
  TestConfig config = CreateDefaultConfig();
  config.EraseMember("enable_limit_inspect_data");
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, MissingDiskEndurance) {
  TestConfig config = CreateDefaultConfig();
  config.EraseMember("disk_endurance");
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, MissingRemoteDeviceIdProvider) {
  TestConfig config = CreateDefaultConfig();
  config.EraseMember("remote_device_id_provider");
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, MissingSupportsUserInitiatedPoweroffs) {
  TestConfig config = CreateDefaultConfig();
  config.EraseMember("supports_user_initiated_poweroffs");
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, CrashReportUploadPolicyDisabled) {
  TestConfig config = CreateDefaultConfig();
  config.Set("crash_report_upload_policy", "disabled");
  const std::optional<FeedbackConfig> feedback_config = ParseConfig(config);
  ASSERT_TRUE(feedback_config.has_value());
  EXPECT_EQ(feedback_config->build_type_config.crash_report_upload_policy,
            CrashReportUploadPolicy::kDisabled);
}

TEST_F(FeedbackConfigTest, CrashReportUploadPolicyEnabled) {
  TestConfig config = CreateDefaultConfig();
  config.Set("crash_report_upload_policy", "enabled");
  const std::optional<FeedbackConfig> feedback_config = ParseConfig(config);
  ASSERT_TRUE(feedback_config.has_value());
  EXPECT_EQ(feedback_config->build_type_config.crash_report_upload_policy,
            CrashReportUploadPolicy::kEnabled);
}

TEST_F(FeedbackConfigTest, CrashReportUploadPolicyReadFromPrivacySettings) {
  TestConfig config = CreateDefaultConfig();
  config.Set("crash_report_upload_policy", "read_from_privacy_settings");
  const std::optional<FeedbackConfig> feedback_config = ParseConfig(config);
  ASSERT_TRUE(feedback_config.has_value());
  EXPECT_EQ(feedback_config->build_type_config.crash_report_upload_policy,
            CrashReportUploadPolicy::kReadFromPrivacySettings);
}

TEST_F(FeedbackConfigTest, CrashReportUploadPolicyNotAllowedValue) {
  TestConfig config = CreateDefaultConfig();
  config.Set("crash_report_upload_policy", "not_allowed");
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, CrashReportUploadPolicyNotString) {
  TestConfig config = CreateDefaultConfig();
  config.Set("crash_report_upload_policy", 0);
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, DailyPerProductCrashReportQuotaNegative) {
  TestConfig config = CreateDefaultConfig();
  config.Set("daily_per_product_crash_report_quota", -1);
  const std::optional<FeedbackConfig> feedback_config = ParseConfig(config);
  ASSERT_TRUE(feedback_config.has_value());
  EXPECT_EQ(feedback_config->build_type_config.daily_per_product_crash_report_quota, std::nullopt);
}

TEST_F(FeedbackConfigTest, DailyPerProductCrashReportQuotaZero) {
  TestConfig config = CreateDefaultConfig();
  config.Set("daily_per_product_crash_report_quota", 0);
  const std::optional<FeedbackConfig> feedback_config = ParseConfig(config);
  ASSERT_TRUE(feedback_config.has_value());
  EXPECT_EQ(feedback_config->build_type_config.daily_per_product_crash_report_quota, std::nullopt);
}

TEST_F(FeedbackConfigTest, DailyPerProductCrashReportQuotaPositive) {
  TestConfig config = CreateDefaultConfig();
  config.Set("daily_per_product_crash_report_quota", 100);
  const std::optional<FeedbackConfig> feedback_config = ParseConfig(config);
  ASSERT_TRUE(feedback_config.has_value());
  EXPECT_EQ(feedback_config->build_type_config.daily_per_product_crash_report_quota, 100);
}

TEST_F(FeedbackConfigTest, DailyPerProductCrashReportQuotaNotNumber) {
  TestConfig config = CreateDefaultConfig();
  config.Set("daily_per_product_crash_report_quota", "");
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, EnableDataRedactionTrue) {
  TestConfig config = CreateDefaultConfig();
  config.Set("enable_data_redaction", true);
  const std::optional<FeedbackConfig> feedback_config = ParseConfig(config);
  ASSERT_TRUE(feedback_config.has_value());
  EXPECT_TRUE(feedback_config->build_type_config.enable_data_redaction);
}

TEST_F(FeedbackConfigTest, EnableDataRedactionFalse) {
  TestConfig config = CreateDefaultConfig();
  config.Set("enable_data_redaction", false);
  const std::optional<FeedbackConfig> feedback_config = ParseConfig(config);
  ASSERT_TRUE(feedback_config.has_value());
  EXPECT_FALSE(feedback_config->build_type_config.enable_data_redaction);
}

TEST_F(FeedbackConfigTest, EnableDataRedactionNotBoolean) {
  TestConfig config = CreateDefaultConfig();
  config.Set("enable_data_redaction", "");
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, EnableHourlySnapshotsTrue) {
  TestConfig config = CreateDefaultConfig();
  config.Set("enable_hourly_snapshots", true);
  const std::optional<FeedbackConfig> feedback_config = ParseConfig(config);
  ASSERT_TRUE(feedback_config.has_value());
  EXPECT_TRUE(feedback_config->build_type_config.enable_hourly_snapshots);
}

TEST_F(FeedbackConfigTest, EnableHourlySnapshotsFalse) {
  TestConfig config = CreateDefaultConfig();
  config.Set("enable_hourly_snapshots", false);
  const std::optional<FeedbackConfig> feedback_config = ParseConfig(config);
  ASSERT_TRUE(feedback_config.has_value());
  EXPECT_FALSE(feedback_config->build_type_config.enable_hourly_snapshots);
}

TEST_F(FeedbackConfigTest, EnableHourlySnapshotsNotBoolean) {
  TestConfig config = CreateDefaultConfig();
  config.Set("enable_hourly_snapshots", "");
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, EnableLimitInspectDataTrue) {
  TestConfig config = CreateDefaultConfig();
  config.Set("enable_limit_inspect_data", true);
  const std::optional<FeedbackConfig> feedback_config = ParseConfig(config);
  ASSERT_TRUE(feedback_config.has_value());
  EXPECT_TRUE(feedback_config->build_type_config.enable_limit_inspect_data);
}

TEST_F(FeedbackConfigTest, EnableLimitInspectDataFalse) {
  TestConfig config = CreateDefaultConfig();
  config.Set("enable_limit_inspect_data", false);
  const std::optional<FeedbackConfig> feedback_config = ParseConfig(config);
  ASSERT_TRUE(feedback_config.has_value());
  EXPECT_FALSE(feedback_config->build_type_config.enable_limit_inspect_data);
}

TEST_F(FeedbackConfigTest, EnableLimitInspectDataNotBoolean) {
  TestConfig config = CreateDefaultConfig();
  config.Set("enable_limit_inspect_data", "");
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, DiskEnduranceHigh) {
  TestConfig config = CreateDefaultConfig();
  config.Set("disk_endurance", "high");
  const std::optional<FeedbackConfig> feedback_config = ParseConfig(config);
  ASSERT_TRUE(feedback_config.has_value());
  EXPECT_EQ(feedback_config->disk_endurance, DiskEndurance::kHigh);
}

TEST_F(FeedbackConfigTest, DiskEnduranceLow) {
  TestConfig config = CreateDefaultConfig();
  config.Set("disk_endurance", "low");
  const std::optional<FeedbackConfig> feedback_config = ParseConfig(config);
  ASSERT_TRUE(feedback_config.has_value());
  EXPECT_EQ(feedback_config->disk_endurance, DiskEndurance::kLow);
}

TEST_F(FeedbackConfigTest, DiskEnduranceNotAllowedValue) {
  TestConfig config = CreateDefaultConfig();
  config.Set("disk_endurance", "not_allowed");
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, DiskEnduranceNotString) {
  TestConfig config = CreateDefaultConfig();
  config.Set("disk_endurance", 0);
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, RemoteDeviceIdProviderTrue) {
  TestConfig config = CreateDefaultConfig();
  config.Set("remote_device_id_provider", true);
  const std::optional<FeedbackConfig> feedback_config = ParseConfig(config);
  ASSERT_TRUE(feedback_config.has_value());
  EXPECT_TRUE(feedback_config->remote_device_id_provider);
}

TEST_F(FeedbackConfigTest, RemoteDeviceIdProviderFalse) {
  TestConfig config = CreateDefaultConfig();
  config.Set("remote_device_id_provider", false);
  const std::optional<FeedbackConfig> feedback_config = ParseConfig(config);
  ASSERT_TRUE(feedback_config.has_value());
  EXPECT_FALSE(feedback_config->remote_device_id_provider);
}

TEST_F(FeedbackConfigTest, RemoteDeviceIdProviderNotBoolean) {
  TestConfig config = CreateDefaultConfig();
  config.Set("remote_device_id_provider", "");
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, SupportsUserInitiatedPoweroffsTrue) {
  TestConfig config = CreateDefaultConfig();
  config.Set("supports_user_initiated_poweroffs", true);
  const std::optional<FeedbackConfig> feedback_config = ParseConfig(config);
  ASSERT_TRUE(feedback_config.has_value());
  EXPECT_TRUE(feedback_config->supports_user_initiated_poweroffs);
}

TEST_F(FeedbackConfigTest, SupportsUserInitiatedPoweroffsFalse) {
  TestConfig config = CreateDefaultConfig();
  config.Set("supports_user_initiated_poweroffs", false);
  const std::optional<FeedbackConfig> feedback_config = ParseConfig(config);
  ASSERT_TRUE(feedback_config.has_value());
  EXPECT_FALSE(feedback_config->supports_user_initiated_poweroffs);
}

TEST_F(FeedbackConfigTest, SupportsUserInitiatedPoweroffsNotBoolean) {
  TestConfig config = CreateDefaultConfig();
  config.Set("supports_user_initiated_poweroffs", "");
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(SnapshotConfigTest, MissingAnnotationAllowlist) {
  const std::optional<SnapshotConfig> config = ParseConfig(R"({
  "attachment_allowlist": []
})");

  EXPECT_FALSE(config.has_value());
}

TEST_F(SnapshotConfigTest, MissingAttachmentAllowlist) {
  const std::optional<SnapshotConfig> config = ParseConfig(R"({
  "annotation_allowlist": []
})");

  EXPECT_FALSE(config.has_value());
}

TEST_F(SnapshotConfigTest, SpuriousField) {
  const std::optional<SnapshotConfig> config = ParseConfig(R"({
  "annotation_allowlist": [],
  "attachment_allowlist": [],
  "spurious": ""
})");

  EXPECT_FALSE(config.has_value());
}

TEST_F(SnapshotConfigTest, AnnotationAllowlistNotArray) {
  const std::optional<SnapshotConfig> config = ParseConfig(R"({
  "annotation_allowlist": "",
  "attachment_allowlist": []
})");

  EXPECT_FALSE(config.has_value());
}

TEST_F(SnapshotConfigTest, AttachmentAllowlistNotArray) {
  const std::optional<SnapshotConfig> config = ParseConfig(R"({
  "annotation_allowlist": [],
  "attachment_allowlist": ""
})");

  EXPECT_FALSE(config.has_value());
}

TEST_F(SnapshotConfigTest, AnnotationAllowlistNotArrayOfStrings) {
  const std::optional<SnapshotConfig> config = ParseConfig(R"({
  "annotation_allowlist": [1],
  "attachment_allowlist": []
})");

  EXPECT_FALSE(config.has_value());
}

TEST_F(SnapshotConfigTest, AttachmentAllowlistNotArrayOfStrings) {
  const std::optional<SnapshotConfig> config = ParseConfig(R"({
  "annotation_allowlist": [],
  "attachment_allowlist": [1]
})");

  EXPECT_FALSE(config.has_value());
}

TEST_F(SnapshotConfigTest, AnnotationAllowlistDuplicateItems) {
  const std::optional<SnapshotConfig> config = ParseConfig(R"({
  "annotation_allowlist": ["a", "a"],
  "attachment_allowlist": []
})");

  EXPECT_FALSE(config.has_value());
}

TEST_F(SnapshotConfigTest, AttachmentAllowlistDuplicateItems) {
  const std::optional<SnapshotConfig> config = ParseConfig(R"({
  "annotation_allowlist": [],
  "attachment_allowlist": ["a", "a"]
})");

  EXPECT_FALSE(config.has_value());
}

TEST_F(SnapshotConfigTest, AnnotationAllowlistEmpty) {
  const std::optional<SnapshotConfig> config = ParseConfig(R"({
  "annotation_allowlist": [],
  "attachment_allowlist": ["a"]
})");

  ASSERT_TRUE(config.has_value());
  EXPECT_THAT(config->default_annotations, IsEmpty());
}

TEST_F(SnapshotConfigTest, AttachmentAllowlistEmpty) {
  const std::optional<SnapshotConfig> config = ParseConfig(R"({
  "annotation_allowlist": ["a"],
  "attachment_allowlist": []
})");

  ASSERT_TRUE(config.has_value());
  EXPECT_THAT(config->attachment_allowlist, IsEmpty());
}

TEST_F(SnapshotConfigTest, AnnotationAllowlistNonEmpty) {
  const std::optional<SnapshotConfig> config = ParseConfig(R"({
  "annotation_allowlist": ["a", "b"],
  "attachment_allowlist": []
})");

  ASSERT_TRUE(config.has_value());
  EXPECT_THAT(config->default_annotations, ElementsAreArray({"a", "b"}));
}

TEST_F(SnapshotConfigTest, AttachmentAllowlistNonEmpty) {
  const std::optional<SnapshotConfig> config = ParseConfig(R"({
  "annotation_allowlist": [],
  "attachment_allowlist": ["a", "b"]
})");

  ASSERT_TRUE(config.has_value());
  EXPECT_THAT(config->attachment_allowlist, ElementsAreArray({"a", "b"}));
}

TEST_F(SnapshotExclusionConfigTest, MissingExcludedAnnotations) {
  const std::optional<SnapshotExclusionConfig> config = ParseConfig(R"({})");

  ASSERT_TRUE(config.has_value());
  EXPECT_THAT(config->excluded_annotations, IsEmpty());
}

TEST_F(SnapshotExclusionConfigTest, SpuriousField) {
  const std::optional<SnapshotExclusionConfig> config = ParseConfig(R"({
    "excluded_annotations": [],
    "spurious": ""
})");

  EXPECT_FALSE(config.has_value());
}

TEST_F(SnapshotExclusionConfigTest, ExcludedAnnotationsNotArray) {
  const std::optional<SnapshotExclusionConfig> config = ParseConfig(R"({
    "excluded_annotations": ""
})");

  EXPECT_FALSE(config.has_value());
}

TEST_F(SnapshotExclusionConfigTest, ExcludedAnnotationsNotArrayOfStrings) {
  const std::optional<SnapshotExclusionConfig> config = ParseConfig(R"({
    "excluded_annotations": [1]
})");

  EXPECT_FALSE(config.has_value());
}

TEST_F(SnapshotExclusionConfigTest, ExcludedAnnotationsDuplicateItems) {
  const std::optional<SnapshotExclusionConfig> config = ParseConfig(R"({
    "excluded_annotations": ["a", "a"]
})");

  EXPECT_FALSE(config.has_value());
}

TEST_F(SnapshotExclusionConfigTest, ExcludedAnnotationsEmpty) {
  const std::optional<SnapshotExclusionConfig> config = ParseConfig(R"({
    "excluded_annotations": []
})");

  ASSERT_TRUE(config.has_value());
  EXPECT_THAT(config->excluded_annotations, IsEmpty());
}

TEST_F(SnapshotExclusionConfigTest, ExcludedAnnotationsNonEmpty) {
  const std::optional<SnapshotExclusionConfig> config = ParseConfig(R"({
    "excluded_annotations": ["a", "b"]
})");

  ASSERT_TRUE(config.has_value());
  EXPECT_THAT(config->excluded_annotations, ElementsAreArray({"a", "b"}));
}

TEST_F(FeedbackConfigTest, MissingConfigs) {
  EXPECT_FALSE(GetFeedbackConfig("/bad/path").has_value());
}

TEST_F(FeedbackConfigTest, MissingPersistedLogsTotalSizeKib) {
  TestConfig config = CreateDefaultConfig();
  config.EraseMember("persisted_logs_total_size_kib");
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, MissingReportPersistenceMaxCacheSizeKib) {
  TestConfig config = CreateDefaultConfig();
  config.EraseMember("report_persistence_max_cache_size_kib");
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, MissingReportPersistenceMaxTmpSizeKib) {
  TestConfig config = CreateDefaultConfig();
  config.EraseMember("report_persistence_max_tmp_size_kib");
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, MissingSnapshotPersistenceMaxCacheSizeMib) {
  TestConfig config = CreateDefaultConfig();
  config.EraseMember("snapshot_persistence_max_cache_size_mib");
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, MissingSnapshotPersistenceMaxTmpSizeMib) {
  TestConfig config = CreateDefaultConfig();
  config.EraseMember("snapshot_persistence_max_tmp_size_mib");
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, MissingSpontaneousRebootReason) {
  TestConfig config = CreateDefaultConfig();
  config.EraseMember("spontaneous_reboot_reason");
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, SpuriousField) {
  TestConfig config = CreateDefaultConfig();
  config.AddMember("spurious", "");
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, PersistedLogsTotalSizeKibPositive) {
  TestConfig config = CreateDefaultConfig();
  config.Set("persisted_logs_total_size_kib", 512);
  const std::optional<FeedbackConfig> feedback_config = ParseConfig(config);
  ASSERT_TRUE(feedback_config.has_value());
  EXPECT_EQ(feedback_config->persisted_logs_total_size, StorageSize::Kilobytes(512));
}

TEST_F(FeedbackConfigTest, PersistedLogsTotalSizeKibZero) {
  TestConfig config = CreateDefaultConfig();
  config.Set("persisted_logs_total_size_kib", 0);
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, PersistedLogsTotalSizeKibNegative) {
  TestConfig config = CreateDefaultConfig();
  config.Set("persisted_logs_total_size_kib", -1);
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, PersistedLogsTotalSizeKibNotNumber) {
  TestConfig config = CreateDefaultConfig();
  config.Set("persisted_logs_total_size_kib", "");
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, ReportPersistenceMaxCacheSizeMibPositive) {
  TestConfig config = CreateDefaultConfig();
  config.Set("report_persistence_max_cache_size_kib", 1);
  const std::optional<FeedbackConfig> feedback_config = ParseConfig(config);
  ASSERT_TRUE(feedback_config.has_value());
  EXPECT_EQ(feedback_config->report_persistence_max_cache_size, StorageSize::Kilobytes(1));
}

TEST_F(FeedbackConfigTest, ReportPersistenceMaxCacheSizeKibZero) {
  TestConfig config = CreateDefaultConfig();
  config.Set("report_persistence_max_cache_size_kib", 0);
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, ReportPersistenceMaxCacheSizeKibNegative) {
  TestConfig config = CreateDefaultConfig();
  config.Set("report_persistence_max_cache_size_kib", -1);
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, ReportPersistenceMaxCacheSizeKibNotNumber) {
  TestConfig config = CreateDefaultConfig();
  config.Set("report_persistence_max_cache_size_kib", "");
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, ReportPersistenceMaxTmpSizeMibPositive) {
  TestConfig config = CreateDefaultConfig();
  config.Set("report_persistence_max_tmp_size_kib", 1);
  const std::optional<FeedbackConfig> feedback_config = ParseConfig(config);
  ASSERT_TRUE(feedback_config.has_value());
  EXPECT_EQ(feedback_config->report_persistence_max_tmp_size, StorageSize::Kilobytes(1));
}

TEST_F(FeedbackConfigTest, ReportPersistenceMaxTmpSizeKibZero) {
  TestConfig config = CreateDefaultConfig();
  config.Set("report_persistence_max_tmp_size_kib", 0);
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, ReportPersistenceMaxTmpSizeKibNegative) {
  TestConfig config = CreateDefaultConfig();
  config.Set("report_persistence_max_tmp_size_kib", -1);
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, ReportPersistenceMaxTmpSizeKibNotNumber) {
  TestConfig config = CreateDefaultConfig();
  config.Set("report_persistence_max_tmp_size_kib", "");
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, SnapshotPersistenceMaxCacheSizeMibPositive) {
  TestConfig config = CreateDefaultConfig();
  config.Set("snapshot_persistence_max_cache_size_mib", 1);
  const std::optional<FeedbackConfig> feedback_config = ParseConfig(config);
  ASSERT_TRUE(feedback_config.has_value());
  EXPECT_EQ(feedback_config->snapshot_persistence_max_cache_size, StorageSize::Megabytes(1));
}

TEST_F(FeedbackConfigTest, SnapshotPersistenceMaxCacheSizeMibZero) {
  TestConfig config = CreateDefaultConfig();
  config.Set("snapshot_persistence_max_cache_size_mib", 0);
  const std::optional<FeedbackConfig> feedback_config = ParseConfig(config);
  ASSERT_TRUE(feedback_config.has_value());
  EXPECT_FALSE(feedback_config->snapshot_persistence_max_cache_size.has_value());
}

TEST_F(FeedbackConfigTest, SnapshotPersistenceMaxCacheSizeMibNegative) {
  TestConfig config = CreateDefaultConfig();
  config.Set("snapshot_persistence_max_cache_size_mib", -1);
  const std::optional<FeedbackConfig> feedback_config = ParseConfig(config);
  ASSERT_TRUE(feedback_config.has_value());
  EXPECT_FALSE(feedback_config->snapshot_persistence_max_cache_size.has_value());
}

TEST_F(FeedbackConfigTest, SnapshotPersistenceMaxCacheSizeMibNotNumber) {
  TestConfig config = CreateDefaultConfig();
  config.Set("snapshot_persistence_max_cache_size_mib", "");
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, SnapshotPersistenceMaxTmpSizeMibPositive) {
  TestConfig config = CreateDefaultConfig();
  config.Set("snapshot_persistence_max_tmp_size_mib", 1);
  const std::optional<FeedbackConfig> feedback_config = ParseConfig(config);
  ASSERT_TRUE(feedback_config.has_value());
  EXPECT_EQ(feedback_config->snapshot_persistence_max_tmp_size, StorageSize::Megabytes(1));
}

TEST_F(FeedbackConfigTest, SnapshotPersistenceMaxTmpSizeMibZero) {
  TestConfig config = CreateDefaultConfig();
  config.Set("snapshot_persistence_max_tmp_size_mib", 0);
  const std::optional<FeedbackConfig> feedback_config = ParseConfig(config);
  ASSERT_TRUE(feedback_config.has_value());
  EXPECT_FALSE(feedback_config->snapshot_persistence_max_tmp_size.has_value());
}

TEST_F(FeedbackConfigTest, SnapshotPersistenceMaxTmpSizeMibNegative) {
  TestConfig config = CreateDefaultConfig();
  config.Set("snapshot_persistence_max_tmp_size_mib", -1);
  const std::optional<FeedbackConfig> feedback_config = ParseConfig(config);
  ASSERT_TRUE(feedback_config.has_value());
  EXPECT_FALSE(feedback_config->snapshot_persistence_max_tmp_size.has_value());
}

TEST_F(FeedbackConfigTest, SnapshotPersistenceMaxTmpSizeMibNotNumber) {
  TestConfig config = CreateDefaultConfig();
  config.Set("snapshot_persistence_max_tmp_size_mib", "");
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, SpontaneousRebootReasonNotAllowedValue) {
  TestConfig config = CreateDefaultConfig();
  config.Set("spontaneous_reboot_reason", "not_allowed");
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, SpontaneousRebootReasonNotString) {
  TestConfig config = CreateDefaultConfig();
  config.Set("spontaneous_reboot_reason", 0);
  EXPECT_FALSE(ParseConfig(config).has_value());
}

TEST_F(FeedbackConfigTest, SpontaneousRebootReasonSpontaneous) {
  TestConfig config = CreateDefaultConfig();
  config.Set("spontaneous_reboot_reason", "spontaneous");
  const std::optional<FeedbackConfig> feedback_config = ParseConfig(config);
  ASSERT_TRUE(feedback_config.has_value());
  EXPECT_EQ(feedback_config->spontaneous_reboot_reason, SpontaneousRebootReason::kBriefPowerLoss);
}

TEST_F(FeedbackConfigTest, SpontaneousRebootReasonBriefPowerLoss) {
  TestConfig config = CreateDefaultConfig();
  config.Set("spontaneous_reboot_reason", "brief_power_loss");
  const std::optional<FeedbackConfig> feedback_config = ParseConfig(config);
  ASSERT_TRUE(feedback_config.has_value());
  EXPECT_EQ(feedback_config->spontaneous_reboot_reason, SpontaneousRebootReason::kBriefPowerLoss);
}

TEST_F(FeedbackConfigTest, SpontaneousRebootReasonHardReset) {
  TestConfig config = CreateDefaultConfig();
  config.Set("spontaneous_reboot_reason", "hard_reset");
  const std::optional<FeedbackConfig> feedback_config = ParseConfig(config);
  ASSERT_TRUE(feedback_config.has_value());
  EXPECT_EQ(feedback_config->spontaneous_reboot_reason, SpontaneousRebootReason::kHardReset);
}

TEST_F(InspectConfigTest, ExposeConfig_UploadDisabled) {
  ExposeConfig(InspectRoot(), FeedbackConfig{
                                  .build_type_config =
                                      BuildTypeConfig{
                                          .crash_report_upload_policy = kConfigDisabled,
                                      },
                              });

  EXPECT_THAT(
      InspectTree(),
      BuildConfigMatcher({StringIs(kCrashReportUploadPolicyKey, ToString(kConfigDisabled))}));
}

TEST_F(InspectConfigTest, ExposeConfig_UploadEnabled) {
  ExposeConfig(InspectRoot(), FeedbackConfig{
                                  .build_type_config =
                                      BuildTypeConfig{
                                          .crash_report_upload_policy = kConfigEnabled,
                                      },
                              });

  EXPECT_THAT(
      InspectTree(),
      BuildConfigMatcher({StringIs(kCrashReportUploadPolicyKey, ToString(kConfigEnabled))}));
}

TEST_F(InspectConfigTest, ExposeConfig_UploadReadFromPrivacySettings) {
  ExposeConfig(InspectRoot(),
               FeedbackConfig{
                   .build_type_config =
                       BuildTypeConfig{
                           .crash_report_upload_policy = kConfigReadFromPrivacySettings,
                       },
               });

  EXPECT_THAT(InspectTree(),
              BuildConfigMatcher({StringIs(kCrashReportUploadPolicyKey,
                                           ToString(kConfigReadFromPrivacySettings))}));
}

TEST_F(InspectConfigTest, ExposeConfig_DailyPerProductCrashReportQuotaNone) {
  ExposeConfig(InspectRoot(), FeedbackConfig{
                                  .build_type_config =
                                      BuildTypeConfig{
                                          .daily_per_product_crash_report_quota = std::nullopt,
                                      },
                              });

  EXPECT_THAT(InspectTree(),
              BuildConfigMatcher({StringIs(kDailyPerProductCrashReportQuotaKey, "none")}));
}

TEST_F(InspectConfigTest, ExposeConfig_DailyPerProductCrashReportQuotaPositive) {
  ExposeConfig(InspectRoot(), FeedbackConfig{
                                  .build_type_config =
                                      BuildTypeConfig{
                                          .daily_per_product_crash_report_quota = 1,
                                      },
                              });

  EXPECT_THAT(InspectTree(),
              BuildConfigMatcher({StringIs(kDailyPerProductCrashReportQuotaKey, "1")}));
}

TEST_F(InspectConfigTest, ExposeConfig_EnableDataRedactionFalse) {
  ExposeConfig(InspectRoot(), FeedbackConfig{
                                  .build_type_config =
                                      BuildTypeConfig{
                                          .enable_data_redaction = false,
                                      },
                              });

  EXPECT_THAT(InspectTree(), BuildConfigMatcher({BoolIs(kEnableDataRedactionKey, false)}));
}

TEST_F(InspectConfigTest, ExposeConfig_EnableDataRedactionTrue) {
  ExposeConfig(InspectRoot(), FeedbackConfig{
                                  .build_type_config =
                                      BuildTypeConfig{
                                          .enable_data_redaction = true,
                                      },
                              });

  EXPECT_THAT(InspectTree(), BuildConfigMatcher({BoolIs(kEnableDataRedactionKey, true)}));
}

TEST_F(InspectConfigTest, ExposeConfig_EnableHourlySnapshotsFalse) {
  ExposeConfig(InspectRoot(), FeedbackConfig{
                                  .build_type_config =
                                      BuildTypeConfig{
                                          .enable_hourly_snapshots = false,
                                      },
                              });

  EXPECT_THAT(InspectTree(), BuildConfigMatcher({BoolIs(kEnableHourlySnapshotsKey, false)}));
}

TEST_F(InspectConfigTest, ExposeConfig_EnableHourlySnapshotsTrue) {
  ExposeConfig(InspectRoot(), FeedbackConfig{
                                  .build_type_config =
                                      BuildTypeConfig{
                                          .enable_hourly_snapshots = true,
                                      },
                              });

  EXPECT_THAT(InspectTree(), BuildConfigMatcher({BoolIs(kEnableHourlySnapshotsKey, true)}));
}

TEST_F(InspectConfigTest, ExposeConfig_EnableLimitInspectDataFalse) {
  ExposeConfig(InspectRoot(), FeedbackConfig{
                                  .build_type_config =
                                      BuildTypeConfig{
                                          .enable_limit_inspect_data = false,
                                      },
                              });

  EXPECT_THAT(InspectTree(), BuildConfigMatcher({BoolIs(kEnableLimitInspectDataKey, false)}));
}

TEST_F(InspectConfigTest, ExposeConfig_EnableLimitInspectDataTrue) {
  ExposeConfig(InspectRoot(), FeedbackConfig{
                                  .build_type_config =
                                      BuildTypeConfig{
                                          .enable_limit_inspect_data = true,
                                      },
                              });

  EXPECT_THAT(InspectTree(), BuildConfigMatcher({BoolIs(kEnableLimitInspectDataKey, true)}));
}

TEST_F(InspectConfigTest, ExposeConfig_BuildTypeEnableAll) {
  ExposeConfig(InspectRoot(), FeedbackConfig{
                                  .build_type_config =
                                      BuildTypeConfig{
                                          .crash_report_upload_policy = kConfigEnabled,
                                          .daily_per_product_crash_report_quota = 1,
                                          .enable_data_redaction = true,
                                          .enable_hourly_snapshots = true,
                                          .enable_limit_inspect_data = true,
                                      },
                              });

  EXPECT_THAT(InspectTree(), BuildConfigMatcher({
                                 StringIs(kCrashReportUploadPolicyKey, ToString(kConfigEnabled)),
                                 StringIs(kDailyPerProductCrashReportQuotaKey, "1"),
                                 BoolIs(kEnableDataRedactionKey, true),
                                 BoolIs(kEnableHourlySnapshotsKey, true),
                                 BoolIs(kEnableLimitInspectDataKey, true),
                             }));
}

TEST_F(InspectConfigTest, ExposeConfig_SnapshotPersistenceMaxTmpSizeNone) {
  ExposeConfig(InspectRoot(), FeedbackConfig{
                                  .snapshot_persistence_max_tmp_size = std::nullopt,
                              });

  EXPECT_THAT(InspectTree(),
              BuildConfigMatcher({StringIs(kSnapshotPersistenceMaxTmpSizeKey, "none")}));
}

TEST_F(InspectConfigTest, ExposeConfig_SnapshotPersistenceMaxTmpSizePositive) {
  ExposeConfig(InspectRoot(), FeedbackConfig{
                                  .snapshot_persistence_max_tmp_size = StorageSize::Megabytes(1),
                              });

  EXPECT_THAT(InspectTree(),
              BuildConfigMatcher({StringIs(kSnapshotPersistenceMaxTmpSizeKey, "1")}));
}

TEST_F(InspectConfigTest, ExposeConfig_SnapshotPersistenceMaxCacheSizeNone) {
  ExposeConfig(InspectRoot(), FeedbackConfig{
                                  .snapshot_persistence_max_cache_size = std::nullopt,
                              });

  EXPECT_THAT(InspectTree(),
              BuildConfigMatcher({StringIs(kSnapshotPersistenceMaxCacheSizeKey, "none")}));
}

TEST_F(InspectConfigTest, ExposeConfig_SnapshotPersistenceMaxCacheSizePositive) {
  ExposeConfig(InspectRoot(), FeedbackConfig{
                                  .snapshot_persistence_max_cache_size = StorageSize::Megabytes(1),
                              });

  EXPECT_THAT(InspectTree(),
              BuildConfigMatcher({StringIs(kSnapshotPersistenceMaxCacheSizeKey, "1")}));
}

TEST_F(InspectConfigTest, ExposeConfig_DiskEnduranceHigh) {
  ExposeConfig(InspectRoot(), FeedbackConfig{
                                  .disk_endurance = DiskEndurance::kHigh,
                              });

  EXPECT_THAT(InspectTree(), BuildConfigMatcher({StringIs(kDiskEnduranceKey, "HIGH")}));
}

TEST_F(InspectConfigTest, ExposeConfig_DiskEnduranceLow) {
  ExposeConfig(InspectRoot(), FeedbackConfig{
                                  .disk_endurance = DiskEndurance::kLow,
                              });

  EXPECT_THAT(InspectTree(), BuildConfigMatcher({StringIs(kDiskEnduranceKey, "LOW")}));
}

TEST_F(InspectConfigTest, ExposeConfig_SupportsUserInitiatedPoweroffsFalse) {
  ExposeConfig(InspectRoot(), FeedbackConfig{
                                  .supports_user_initiated_poweroffs = false,
                              });

  EXPECT_THAT(InspectTree(),
              BuildConfigMatcher({BoolIs(kSupportsUserInitiatedPoweroffsKey, false)}));
}

TEST_F(InspectConfigTest, ExposeConfig_SupportsUserInitiatedPoweroffsTrue) {
  ExposeConfig(InspectRoot(), FeedbackConfig{
                                  .supports_user_initiated_poweroffs = true,
                              });

  EXPECT_THAT(InspectTree(),
              BuildConfigMatcher({BoolIs(kSupportsUserInitiatedPoweroffsKey, true)}));
}

TEST_F(InspectConfigTest, ExposeConfig_PersistedLogsTotalSize) {
  ExposeConfig(InspectRoot(), FeedbackConfig{
                                  .persisted_logs_total_size = StorageSize::Kilobytes(512),
                              });

  EXPECT_THAT(InspectTree(), BuildConfigMatcher({UintIs(kPersistedLogsTotalSizeKey, 512)}));
}

TEST_F(InspectConfigTest, ExposeConfig_FeedbackConfigEnableAll) {
  ExposeConfig(InspectRoot(), FeedbackConfig{
                                  .persisted_logs_total_size = StorageSize::Kilobytes(512),
                                  .snapshot_persistence_max_cache_size = StorageSize::Megabytes(1),
                                  .snapshot_persistence_max_tmp_size = StorageSize::Megabytes(1),
                              });

  EXPECT_THAT(InspectTree(), BuildConfigMatcher({
                                 UintIs(kPersistedLogsTotalSizeKey, 512),
                                 StringIs(kSnapshotPersistenceMaxTmpSizeKey, "1"),
                                 StringIs(kSnapshotPersistenceMaxCacheSizeKey, "1"),
                                 StringIs(kDiskEnduranceKey, "HIGH"),
                                 BoolIs(kSupportsUserInitiatedPoweroffsKey, false),
                             }));
}

TEST_F(InspectConfigTest, ExposeConfig_EnableAll) {
  ExposeConfig(InspectRoot(), FeedbackConfig{
                                  .persisted_logs_total_size = StorageSize::Kilobytes(512),
                                  .snapshot_persistence_max_cache_size = StorageSize::Megabytes(1),
                                  .snapshot_persistence_max_tmp_size = StorageSize::Megabytes(1),
                                  .disk_endurance = DiskEndurance::kLow,
                                  .supports_user_initiated_poweroffs = true,
                                  .build_type_config =
                                      BuildTypeConfig{
                                          .crash_report_upload_policy = kConfigEnabled,
                                          .daily_per_product_crash_report_quota = 1,
                                          .enable_data_redaction = true,
                                          .enable_hourly_snapshots = true,
                                          .enable_limit_inspect_data = true,
                                      },
                              });

  EXPECT_THAT(InspectTree(), BuildConfigMatcher({
                                 UintIs(kPersistedLogsTotalSizeKey, 512),
                                 StringIs(kCrashReportUploadPolicyKey, ToString(kConfigEnabled)),
                                 StringIs(kDailyPerProductCrashReportQuotaKey, "1"),
                                 StringIs(kDiskEnduranceKey, "LOW"),
                                 BoolIs(kEnableDataRedactionKey, true),
                                 BoolIs(kEnableHourlySnapshotsKey, true),
                                 BoolIs(kEnableLimitInspectDataKey, true),
                                 StringIs(kSnapshotPersistenceMaxTmpSizeKey, "1"),
                                 StringIs(kSnapshotPersistenceMaxCacheSizeKey, "1"),
                                 BoolIs(kSupportsUserInitiatedPoweroffsKey, true),
                             }));
}

}  // namespace
}  // namespace forensics::feedback
