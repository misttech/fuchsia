// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/cobalt/bin/app/key_fetcher.h"

#include <lib/syslog/cpp/macros.h>

#include <chrono>
#include <cstdint>
#include <memory>
#include <optional>
#include <string>
#include <utility>

#include <gtest/gtest.h>

#include "src/cobalt/bin/app/utils.h"
#include "src/cobalt/bin/testing/fake_clock.h"
#include "src/lib/files/directory.h"
#include "src/lib/files/file.h"
#include "src/lib/files/path.h"
#include "src/lib/testing/loop_fixture/test_loop_fixture.h"
#include "third_party/cobalt/src/pb/cobalt_public_keys.pb.h"
#include "third_party/cobalt/src/pb/key.pb.h"

namespace cobalt {
namespace {

constexpr char kTestCacheDir[] = "/tmp/cobalt_key_fetcher_test";
constexpr char kTestPublicKeysUrl[] = "https://example.com/cobalt/public_keys.pb";
constexpr int64_t kInitialTimeSecs = 1000;
constexpr int64_t kExpiredTimeSecs = 500;
constexpr int64_t kValidExpirationSecs = 2000;
constexpr int64_t kUpdatedExpirationSecs = 3000;
constexpr int64_t kScheduledExpirationSecs = 4000;
constexpr uint32_t kHttpOk = 200;
constexpr uint32_t kHttpServiceUnavailable = 503;
constexpr uint32_t kOlderShufflerKeyIndex = 30;
constexpr uint32_t kOlderAnalyzerKeyIndex = 31;
constexpr uint32_t kBundledShufflerKeyIndex = 40;
constexpr uint32_t kBundledAnalyzerKeyIndex = 41;
constexpr uint32_t kCachedShufflerKeyIndex = 50;
constexpr uint32_t kCachedAnalyzerKeyIndex = 51;
constexpr uint32_t kFetchedShufflerKeyIndex = 60;
constexpr uint32_t kFetchedAnalyzerKeyIndex = 61;

class StubHTTPClient : public lib::HTTPClient {
 public:
  explicit StubHTTPClient(lib::StatusOr<lib::HTTPResponse> response)
      : next_response_(std::move(response)) {}

  void SetResponse(lib::StatusOr<lib::HTTPResponse> response) {
    next_response_ = std::move(response);
  }

  lib::StatusOr<lib::HTTPResponse> GetSync(
      lib::HTTPRequest request, std::chrono::steady_clock::time_point /*deadline*/) override {
    ++get_call_count_;
    last_request_url_ = std::move(request.url);
    if (!next_response_.ok()) {
      return next_response_.status();
    }
    return lib::HTTPResponse(next_response_->response, next_response_->status,
                             next_response_->http_code, next_response_->headers);
  }

  [[nodiscard]] int get_call_count() const { return get_call_count_; }
  [[nodiscard]] const std::string& last_request_url() const { return last_request_url_; }

 private:
  lib::StatusOr<lib::HTTPResponse> next_response_;
  int get_call_count_ = 0;
  std::string last_request_url_;
};

class KeyFetcherTest : public ::gtest::TestLoopFixture {
 protected:
  void SetUp() override {
    ASSERT_TRUE(files::DeletePath(kTestCacheDir, /*recursive=*/true));
    ASSERT_TRUE(files::CreateDirectory(kTestCacheDir));

    // TODO(https://fxbug.dev/539564173): Construct a test Tink keyset directly after
    // bundled public key files are removed from the package.
    CobaltEncryptionKey bundled_key;
    ASSERT_TRUE(
        bundled_key.ParseFromString(ReadPublicKeyPem("/pkg/data/keys/shuffler_devel_public")));
    valid_tink_keyset_bytes_ = bundled_key.serialized_key();
    ASSERT_FALSE(valid_tink_keyset_bytes_.empty());
  }

  [[nodiscard]] static KeyFetcher::ScheduleOptions ImmediateScheduleOptions() {
    return KeyFetcher::ScheduleOptions{
        .max_initial_jitter = zx::sec(0),
        .steady_state_interval = zx::hour(24),
        .steady_state_jitter = zx::hour(2),
        .poll_interval = zx::hour(1),
        .boot_clock = zx::clock::get_boot,
    };
  }

  [[nodiscard]] std::string BuildPublicKeysBytes(uint32_t shuffler_idx, uint32_t analyzer_idx,
                                                 int64_t expiration_secs) const {
    return BuildPublicKeysBytes(shuffler_idx, analyzer_idx, expiration_secs, expiration_secs);
  }

  [[nodiscard]] std::string BuildPublicKeysBytes(uint32_t shuffler_idx, uint32_t analyzer_idx,
                                                 int64_t shuffler_expiration_secs,
                                                 int64_t analyzer_expiration_secs) const {
    CobaltPublicKeys keys;
    keys.mutable_prod_shuffler_key()->set_public_tink_keyset(valid_tink_keyset_bytes_);
    keys.mutable_prod_shuffler_key()->set_key_index(shuffler_idx);
    keys.mutable_prod_shuffler_key()->set_expiration_timestamp_secs(shuffler_expiration_secs);

    keys.mutable_prod_analyzer_key()->set_public_tink_keyset(valid_tink_keyset_bytes_);
    keys.mutable_prod_analyzer_key()->set_key_index(analyzer_idx);
    keys.mutable_prod_analyzer_key()->set_expiration_timestamp_secs(analyzer_expiration_secs);

    std::string out;
    FX_CHECK(keys.SerializeToString(&out));
    return out;
  }

  [[nodiscard]] std::string BuildDevelPublicKeysBytes(uint32_t shuffler_idx, uint32_t analyzer_idx,
                                                      int64_t expiration_secs) const {
    CobaltPublicKeys keys;
    keys.mutable_dev_shuffler_key()->set_public_tink_keyset(valid_tink_keyset_bytes_);
    keys.mutable_dev_shuffler_key()->set_key_index(shuffler_idx);
    keys.mutable_dev_shuffler_key()->set_expiration_timestamp_secs(expiration_secs);

    keys.mutable_dev_analyzer_key()->set_public_tink_keyset(valid_tink_keyset_bytes_);
    keys.mutable_dev_analyzer_key()->set_key_index(analyzer_idx);
    keys.mutable_dev_analyzer_key()->set_expiration_timestamp_secs(expiration_secs);

    std::string out;
    FX_CHECK(keys.SerializeToString(&out));
    return out;
  }

 private:
  std::string valid_tink_keyset_bytes_;
};

TEST(KeyFetchStatusToStringTest, ReturnsExpectedNames) {
  EXPECT_STREQ(KeyFetchStatusToString(KeyFetchStatus::kUninitialized), "Uninitialized");
  EXPECT_STREQ(KeyFetchStatusToString(KeyFetchStatus::kSuccess), "Success");
  EXPECT_STREQ(KeyFetchStatusToString(KeyFetchStatus::kNetworkError), "NetworkError");
  EXPECT_STREQ(KeyFetchStatusToString(KeyFetchStatus::kParseError), "ParseError");
  EXPECT_STREQ(KeyFetchStatusToString(KeyFetchStatus::kInvalidTinkKeyset), "InvalidTinkKeyset");
  EXPECT_STREQ(KeyFetchStatusToString(KeyFetchStatus::kExpiredKey), "ExpiredKey");
  EXPECT_STREQ(KeyFetchStatusToString(static_cast<KeyFetchStatus>(99)), "Unknown");
}

TEST_F(KeyFetcherTest, LoadKeysFromDiskCacheMissingFileReturnsFalse) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(
      util::ParsedKey(util::EncryptedMessageMaker::MakeUnencrypted(), kBundledShufflerKeyIndex,
                      kBundledKeyExpirationSecs),
      &clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(
      util::ParsedKey(util::EncryptedMessageMaker::MakeUnencrypted(), kBundledAnalyzerKeyIndex,
                      kBundledKeyExpirationSecs),
      &clock);

  KeyFetcher fetcher(dispatcher(), std::make_unique<lib::EmptyHTTPClient>(), &shuffler_maker,
                     &analyzer_maker, &clock, system_data::Environment::PROD, kTestCacheDir,
                     kTestPublicKeysUrl);

  EXPECT_FALSE(fetcher.LoadKeysFromDiskCache());
  EXPECT_EQ(shuffler_maker.key_index(), kBundledShufflerKeyIndex);
  EXPECT_EQ(analyzer_maker.key_index(), kBundledAnalyzerKeyIndex);
  EXPECT_FALSE(shuffler_maker.IsExpired());
  EXPECT_FALSE(analyzer_maker.IsExpired());
}

TEST_F(KeyFetcherTest, LoadKeysFromDiskCacheEmptyFileDeletesFileAndReturnsFalse) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(
      util::ParsedKey(util::EncryptedMessageMaker::MakeUnencrypted(), kBundledShufflerKeyIndex,
                      kBundledKeyExpirationSecs),
      &clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(
      util::ParsedKey(util::EncryptedMessageMaker::MakeUnencrypted(), kBundledAnalyzerKeyIndex,
                      kBundledKeyExpirationSecs),
      &clock);

  KeyFetcher fetcher(dispatcher(), std::make_unique<lib::EmptyHTTPClient>(), &shuffler_maker,
                     &analyzer_maker, &clock, system_data::Environment::PROD, kTestCacheDir,
                     kTestPublicKeysUrl);

  ASSERT_TRUE(files::WriteFile(fetcher.cache_file_path(), ""));
  ASSERT_TRUE(files::IsFile(fetcher.cache_file_path()));

  EXPECT_FALSE(fetcher.LoadKeysFromDiskCache());
  EXPECT_FALSE(files::IsFile(fetcher.cache_file_path()));
  EXPECT_EQ(shuffler_maker.key_index(), kBundledShufflerKeyIndex);
  EXPECT_EQ(analyzer_maker.key_index(), kBundledAnalyzerKeyIndex);
  EXPECT_FALSE(shuffler_maker.IsExpired());
  EXPECT_FALSE(analyzer_maker.IsExpired());
}

TEST_F(KeyFetcherTest, LoadKeysFromDiskCacheCorruptFileDeletesFileAndReturnsFalse) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(
      util::ParsedKey(util::EncryptedMessageMaker::MakeUnencrypted(), kBundledShufflerKeyIndex,
                      kBundledKeyExpirationSecs),
      &clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(
      util::ParsedKey(util::EncryptedMessageMaker::MakeUnencrypted(), kBundledAnalyzerKeyIndex,
                      kBundledKeyExpirationSecs),
      &clock);

  KeyFetcher fetcher(dispatcher(), std::make_unique<lib::EmptyHTTPClient>(), &shuffler_maker,
                     &analyzer_maker, &clock, system_data::Environment::PROD, kTestCacheDir,
                     kTestPublicKeysUrl);

  ASSERT_TRUE(files::WriteFile(fetcher.cache_file_path(), "corrupt-bytes"));
  EXPECT_TRUE(files::IsFile(fetcher.cache_file_path()));
  EXPECT_FALSE(fetcher.LoadKeysFromDiskCache());
  EXPECT_FALSE(files::IsFile(fetcher.cache_file_path()));
  EXPECT_EQ(shuffler_maker.key_index(), kBundledShufflerKeyIndex);
  EXPECT_EQ(analyzer_maker.key_index(), kBundledAnalyzerKeyIndex);
  EXPECT_FALSE(shuffler_maker.IsExpired());
  EXPECT_FALSE(analyzer_maker.IsExpired());
}

TEST_F(KeyFetcherTest, LoadKeysFromDiskCacheExpiredFileReturnsFalse) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(
      util::ParsedKey(util::EncryptedMessageMaker::MakeUnencrypted(), kBundledShufflerKeyIndex,
                      kBundledKeyExpirationSecs),
      &clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(
      util::ParsedKey(util::EncryptedMessageMaker::MakeUnencrypted(), kBundledAnalyzerKeyIndex,
                      kBundledKeyExpirationSecs),
      &clock);

  KeyFetcher fetcher(dispatcher(), std::make_unique<lib::EmptyHTTPClient>(), &shuffler_maker,
                     &analyzer_maker, &clock, system_data::Environment::PROD, kTestCacheDir,
                     kTestPublicKeysUrl);

  const std::string expired_bytes =
      BuildPublicKeysBytes(kCachedShufflerKeyIndex, kCachedAnalyzerKeyIndex, kExpiredTimeSecs);
  ASSERT_TRUE(files::WriteFile(fetcher.cache_file_path(), expired_bytes));
  EXPECT_FALSE(fetcher.LoadKeysFromDiskCache());
  EXPECT_TRUE(files::IsFile(fetcher.cache_file_path()));
  EXPECT_EQ(shuffler_maker.key_index(), kCachedShufflerKeyIndex);
  EXPECT_EQ(analyzer_maker.key_index(), kCachedAnalyzerKeyIndex);
  EXPECT_TRUE(shuffler_maker.IsExpired());
  EXPECT_TRUE(analyzer_maker.IsExpired());
}

TEST_F(KeyFetcherTest, LoadKeysFromDiskCacheExpiredFilePersistsAcrossOfflineReboot) {
  FakeSystemClock clock(kInitialTimeSecs);
  {
    util::DynamicEncryptedMessageMaker shuffler_maker(
        util::ParsedKey(util::EncryptedMessageMaker::MakeUnencrypted(), kBundledShufflerKeyIndex,
                        kBundledKeyExpirationSecs),
        &clock);
    util::DynamicEncryptedMessageMaker analyzer_maker(
        util::ParsedKey(util::EncryptedMessageMaker::MakeUnencrypted(), kBundledAnalyzerKeyIndex,
                        kBundledKeyExpirationSecs),
        &clock);

    KeyFetcher first_boot_fetcher(
        dispatcher(), std::make_unique<lib::EmptyHTTPClient>(), &shuffler_maker, &analyzer_maker,
        &clock, system_data::Environment::PROD, kTestCacheDir, kTestPublicKeysUrl);
    ASSERT_TRUE(files::WriteFile(
        first_boot_fetcher.cache_file_path(),
        BuildPublicKeysBytes(kCachedShufflerKeyIndex, kCachedAnalyzerKeyIndex, kExpiredTimeSecs)));
    EXPECT_FALSE(first_boot_fetcher.LoadKeysFromDiskCache());
  }

  util::DynamicEncryptedMessageMaker second_boot_shuffler_maker(
      util::ParsedKey(util::EncryptedMessageMaker::MakeUnencrypted(), kBundledShufflerKeyIndex,
                      kBundledKeyExpirationSecs),
      &clock);
  util::DynamicEncryptedMessageMaker second_boot_analyzer_maker(
      util::ParsedKey(util::EncryptedMessageMaker::MakeUnencrypted(), kBundledAnalyzerKeyIndex,
                      kBundledKeyExpirationSecs),
      &clock);

  KeyFetcher second_boot_fetcher(dispatcher(), std::make_unique<lib::EmptyHTTPClient>(),
                                 &second_boot_shuffler_maker, &second_boot_analyzer_maker, &clock,
                                 system_data::Environment::PROD, kTestCacheDir, kTestPublicKeysUrl);
  EXPECT_FALSE(second_boot_fetcher.LoadKeysFromDiskCache());
  EXPECT_TRUE(files::IsFile(second_boot_fetcher.cache_file_path()));
  EXPECT_EQ(second_boot_shuffler_maker.key_index(), kCachedShufflerKeyIndex);
  EXPECT_EQ(second_boot_analyzer_maker.key_index(), kCachedAnalyzerKeyIndex);
  EXPECT_TRUE(second_boot_shuffler_maker.IsExpired());
  EXPECT_TRUE(second_boot_analyzer_maker.IsExpired());
}

TEST_F(KeyFetcherTest, LoadKeysFromDiskCacheValidFileUpdatesMakers) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(&clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(&clock);

  KeyFetcher fetcher(dispatcher(), std::make_unique<lib::EmptyHTTPClient>(), &shuffler_maker,
                     &analyzer_maker, &clock, system_data::Environment::PROD, kTestCacheDir,
                     kTestPublicKeysUrl);

  const std::string pb_bytes =
      BuildPublicKeysBytes(kCachedShufflerKeyIndex, kCachedAnalyzerKeyIndex, kValidExpirationSecs);
  ASSERT_TRUE(files::WriteFile(fetcher.cache_file_path(), pb_bytes));
  EXPECT_TRUE(fetcher.LoadKeysFromDiskCache());
  EXPECT_TRUE(shuffler_maker.IsInitialized());
  EXPECT_TRUE(analyzer_maker.IsInitialized());
  EXPECT_EQ(shuffler_maker.key_index(), kCachedShufflerKeyIndex);
  EXPECT_EQ(analyzer_maker.key_index(), kCachedAnalyzerKeyIndex);
}

TEST_F(KeyFetcherTest, LoadKeysFromDiskCacheEqualKeyIndexReplacesBundledKeys) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(
      util::ParsedKey(util::EncryptedMessageMaker::MakeUnencrypted(), kBundledShufflerKeyIndex,
                      kBundledKeyExpirationSecs),
      &clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(
      util::ParsedKey(util::EncryptedMessageMaker::MakeUnencrypted(), kBundledAnalyzerKeyIndex,
                      kBundledKeyExpirationSecs),
      &clock);

  KeyFetcher fetcher(dispatcher(), std::make_unique<lib::EmptyHTTPClient>(), &shuffler_maker,
                     &analyzer_maker, &clock, system_data::Environment::PROD, kTestCacheDir,
                     kTestPublicKeysUrl);

  ASSERT_TRUE(
      files::WriteFile(fetcher.cache_file_path(),
                       BuildPublicKeysBytes(kBundledShufflerKeyIndex, kBundledAnalyzerKeyIndex,
                                            kValidExpirationSecs)));
  EXPECT_TRUE(fetcher.LoadKeysFromDiskCache());
  EXPECT_EQ(shuffler_maker.key_index(), kBundledShufflerKeyIndex);
  EXPECT_EQ(analyzer_maker.key_index(), kBundledAnalyzerKeyIndex);
  EXPECT_EQ(shuffler_maker.expiration_timestamp_secs(), kValidExpirationSecs);
  EXPECT_EQ(analyzer_maker.expiration_timestamp_secs(), kValidExpirationSecs);
}

TEST_F(KeyFetcherTest, LoadKeysFromDiskCacheNewerKeyIndexReplacesBundledKeys) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(
      util::ParsedKey(util::EncryptedMessageMaker::MakeUnencrypted(), kBundledShufflerKeyIndex,
                      kBundledKeyExpirationSecs),
      &clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(
      util::ParsedKey(util::EncryptedMessageMaker::MakeUnencrypted(), kBundledAnalyzerKeyIndex,
                      kBundledKeyExpirationSecs),
      &clock);

  KeyFetcher fetcher(dispatcher(), std::make_unique<lib::EmptyHTTPClient>(), &shuffler_maker,
                     &analyzer_maker, &clock, system_data::Environment::PROD, kTestCacheDir,
                     kTestPublicKeysUrl);

  ASSERT_TRUE(
      files::WriteFile(fetcher.cache_file_path(),
                       BuildPublicKeysBytes(kCachedShufflerKeyIndex, kCachedAnalyzerKeyIndex,
                                            kUpdatedExpirationSecs)));
  EXPECT_TRUE(fetcher.LoadKeysFromDiskCache());
  EXPECT_EQ(shuffler_maker.key_index(), kCachedShufflerKeyIndex);
  EXPECT_EQ(analyzer_maker.key_index(), kCachedAnalyzerKeyIndex);
  EXPECT_EQ(shuffler_maker.expiration_timestamp_secs(), kUpdatedExpirationSecs);
  EXPECT_EQ(analyzer_maker.expiration_timestamp_secs(), kUpdatedExpirationSecs);
}

TEST_F(KeyFetcherTest, LoadKeysFromDiskCacheOlderKeyIndexThanBundledDeletesCacheAndReturnsFalse) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(
      util::ParsedKey(util::EncryptedMessageMaker::MakeUnencrypted(), kBundledShufflerKeyIndex,
                      kBundledKeyExpirationSecs),
      &clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(
      util::ParsedKey(util::EncryptedMessageMaker::MakeUnencrypted(), kBundledAnalyzerKeyIndex,
                      kBundledKeyExpirationSecs),
      &clock);

  KeyFetcher fetcher(dispatcher(), std::make_unique<lib::EmptyHTTPClient>(), &shuffler_maker,
                     &analyzer_maker, &clock, system_data::Environment::PROD, kTestCacheDir,
                     kTestPublicKeysUrl);

  const std::string stale_pb_bytes =
      BuildPublicKeysBytes(kOlderShufflerKeyIndex, kOlderAnalyzerKeyIndex, kValidExpirationSecs);
  ASSERT_TRUE(files::WriteFile(fetcher.cache_file_path(), stale_pb_bytes));
  EXPECT_FALSE(fetcher.LoadKeysFromDiskCache());
  EXPECT_FALSE(files::IsFile(fetcher.cache_file_path()));
  EXPECT_EQ(shuffler_maker.key_index(), kBundledShufflerKeyIndex);
  EXPECT_EQ(analyzer_maker.key_index(), kBundledAnalyzerKeyIndex);
}

TEST_F(KeyFetcherTest, ScheduledFetchDevelEnvironmentSelectsDevKeys) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(&clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(&clock);

  const std::string pb_bytes = BuildDevelPublicKeysBytes(
      kFetchedShufflerKeyIndex, kFetchedAnalyzerKeyIndex, kUpdatedExpirationSecs);
  auto stub_http_client =
      std::make_unique<StubHTTPClient>(lib::HTTPResponse(pb_bytes, Status::OkStatus(), kHttpOk));

  KeyFetcher fetcher(dispatcher(), std::move(stub_http_client), &shuffler_maker, &analyzer_maker,
                     &clock, system_data::Environment::DEVEL, kTestCacheDir, kTestPublicKeysUrl,
                     ImmediateScheduleOptions());

  fetcher.OnSystemClockBecomesAccurate();
  RunLoopUntilIdle();

  EXPECT_EQ(fetcher.last_fetch_status(), KeyFetchStatus::kSuccess);
  EXPECT_EQ(shuffler_maker.key_index(), kFetchedShufflerKeyIndex);
  EXPECT_EQ(analyzer_maker.key_index(), kFetchedAnalyzerKeyIndex);
}

TEST_F(KeyFetcherTest, ScheduledFetchClampsAsymmetricExpirationToMinimum) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(&clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(&clock);

  const std::string pb_bytes =
      BuildPublicKeysBytes(kFetchedShufflerKeyIndex, kFetchedAnalyzerKeyIndex, kValidExpirationSecs,
                           kUpdatedExpirationSecs);
  auto stub_http_client =
      std::make_unique<StubHTTPClient>(lib::HTTPResponse(pb_bytes, Status::OkStatus(), kHttpOk));

  KeyFetcher fetcher(dispatcher(), std::move(stub_http_client), &shuffler_maker, &analyzer_maker,
                     &clock, system_data::Environment::PROD, kTestCacheDir, kTestPublicKeysUrl,
                     ImmediateScheduleOptions());

  fetcher.OnSystemClockBecomesAccurate();
  RunLoopUntilIdle();

  EXPECT_EQ(fetcher.last_fetch_status(), KeyFetchStatus::kSuccess);
  EXPECT_EQ(shuffler_maker.expiration_timestamp_secs(), kValidExpirationSecs);
  EXPECT_EQ(analyzer_maker.expiration_timestamp_secs(), kValidExpirationSecs);
}

TEST_F(KeyFetcherTest, ScheduledFetchPersistsToDiskAndUpdatesMakers) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(&clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(&clock);

  const std::string pb_bytes = BuildPublicKeysBytes(
      kFetchedShufflerKeyIndex, kFetchedAnalyzerKeyIndex, kUpdatedExpirationSecs);
  auto stub_http_client =
      std::make_unique<StubHTTPClient>(lib::HTTPResponse(pb_bytes, Status::OkStatus(), kHttpOk));
  auto* stub_http_client_ptr = stub_http_client.get();

  KeyFetcher fetcher(dispatcher(), std::move(stub_http_client), &shuffler_maker, &analyzer_maker,
                     &clock, system_data::Environment::PROD, kTestCacheDir, kTestPublicKeysUrl,
                     ImmediateScheduleOptions());

  fetcher.OnSystemClockBecomesAccurate();
  RunLoopUntilIdle();

  EXPECT_EQ(fetcher.last_fetch_status(), KeyFetchStatus::kSuccess);
  EXPECT_EQ(stub_http_client_ptr->last_request_url(), kTestPublicKeysUrl);
  EXPECT_EQ(fetcher.consecutive_fetch_failures(), 0u);
  EXPECT_EQ(shuffler_maker.key_index(), kFetchedShufflerKeyIndex);
  EXPECT_EQ(analyzer_maker.key_index(), kFetchedAnalyzerKeyIndex);

  std::string cached_bytes;
  ASSERT_TRUE(files::ReadFileToString(fetcher.cache_file_path(), &cached_bytes));
  EXPECT_EQ(cached_bytes, pb_bytes);
}

TEST_F(KeyFetcherTest, LoadKeysFromDiskCacheDoesNotNotifyStatusObserver) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(&clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(&clock);

  KeyFetcher fetcher(dispatcher(), std::make_unique<lib::EmptyHTTPClient>(), &shuffler_maker,
                     &analyzer_maker, &clock, system_data::Environment::PROD, kTestCacheDir,
                     kTestPublicKeysUrl);

  bool observer_called = false;
  fetcher.SetStatusObserver([&](KeyFetchStatus /*status*/, uint32_t /*failures*/,
                                int64_t /*attempt_time_secs*/) { observer_called = true; });

  const std::string pb_bytes =
      BuildPublicKeysBytes(kCachedShufflerKeyIndex, kCachedAnalyzerKeyIndex, kValidExpirationSecs);
  ASSERT_TRUE(files::WriteFile(fetcher.cache_file_path(), pb_bytes));
  EXPECT_TRUE(fetcher.LoadKeysFromDiskCache());
  EXPECT_FALSE(observer_called);
  EXPECT_EQ(fetcher.last_fetch_status(), KeyFetchStatus::kUninitialized);
  EXPECT_EQ(fetcher.last_fetch_attempt_time_secs(), 0);
}

TEST_F(KeyFetcherTest, ScheduledFetchNotifiesStatusObserver) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(&clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(&clock);

  const std::string pb_bytes = BuildPublicKeysBytes(
      kFetchedShufflerKeyIndex, kFetchedAnalyzerKeyIndex, kUpdatedExpirationSecs);
  auto stub_http_client =
      std::make_unique<StubHTTPClient>(lib::HTTPResponse(pb_bytes, Status::OkStatus(), kHttpOk));

  KeyFetcher fetcher(dispatcher(), std::move(stub_http_client), &shuffler_maker, &analyzer_maker,
                     &clock, system_data::Environment::PROD, kTestCacheDir, kTestPublicKeysUrl,
                     ImmediateScheduleOptions());

  KeyFetchStatus observed_status = KeyFetchStatus::kUninitialized;
  std::optional<uint32_t> observed_failures;
  int64_t observed_time_secs = 0;
  uint32_t observed_shuffler_key_index = 0;
  uint32_t observed_analyzer_key_index = 0;
  fetcher.SetStatusObserver(
      [&](KeyFetchStatus status, uint32_t consecutive_failures, int64_t attempt_time_secs) {
        observed_status = status;
        observed_failures = consecutive_failures;
        observed_time_secs = attempt_time_secs;
        observed_shuffler_key_index = shuffler_maker.key_index();
        observed_analyzer_key_index = analyzer_maker.key_index();
      });

  fetcher.OnSystemClockBecomesAccurate();
  RunLoopUntilIdle();

  EXPECT_EQ(fetcher.last_fetch_attempt_time_secs(), kInitialTimeSecs);
  EXPECT_EQ(observed_status, KeyFetchStatus::kSuccess);
  EXPECT_EQ(observed_failures, 0u);
  EXPECT_EQ(observed_time_secs, kInitialTimeSecs);
  EXPECT_EQ(observed_shuffler_key_index, kFetchedShufflerKeyIndex);
  EXPECT_EQ(observed_analyzer_key_index, kFetchedAnalyzerKeyIndex);
}

TEST_F(KeyFetcherTest, ScheduledFetchFailureAndRecoveryNotifiesStatusObserver) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(&clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(&clock);

  auto stub_http_client = std::make_unique<StubHTTPClient>(
      lib::HTTPResponse("unavailable", Status::OkStatus(), kHttpServiceUnavailable));
  auto* stub_http_client_ptr = stub_http_client.get();

  zx::time_boot fake_boot_time{0};
  KeyFetcher::ScheduleOptions opts{
      .max_initial_jitter = zx::sec(0),
      .steady_state_interval = zx::hour(24),
      .steady_state_jitter = zx::sec(0),
      .poll_interval = zx::hour(1),
      .boot_clock = [&fake_boot_time] { return fake_boot_time; },
  };

  KeyFetcher fetcher(dispatcher(), std::move(stub_http_client), &shuffler_maker, &analyzer_maker,
                     &clock, system_data::Environment::PROD, kTestCacheDir, kTestPublicKeysUrl,
                     opts);

  KeyFetchStatus observed_status = KeyFetchStatus::kUninitialized;
  std::optional<uint32_t> observed_failures;
  int64_t observed_time_secs = 0;
  fetcher.SetStatusObserver(
      [&](KeyFetchStatus status, uint32_t consecutive_failures, int64_t attempt_time_secs) {
        observed_status = status;
        observed_failures = consecutive_failures;
        observed_time_secs = attempt_time_secs;
      });

  fetcher.OnSystemClockBecomesAccurate();
  RunLoopUntilIdle();
  EXPECT_EQ(observed_status, KeyFetchStatus::kNetworkError);
  EXPECT_EQ(observed_failures, 1u);
  EXPECT_EQ(observed_time_secs, kInitialTimeSecs);

  constexpr int64_t kSecondAttemptSecs = 1300;
  clock.SetNowSecs(kSecondAttemptSecs);
  fake_boot_time += zx::min(10);
  RunLoopFor(zx::min(10));
  EXPECT_EQ(observed_status, KeyFetchStatus::kNetworkError);
  EXPECT_EQ(observed_failures, 2u);
  EXPECT_EQ(observed_time_secs, kSecondAttemptSecs);

  constexpr int64_t kRecoveryAttemptSecs = 2500;
  clock.SetNowSecs(kRecoveryAttemptSecs);
  stub_http_client_ptr->SetResponse(
      lib::HTTPResponse(BuildPublicKeysBytes(kFetchedShufflerKeyIndex, kFetchedAnalyzerKeyIndex,
                                             kScheduledExpirationSecs),
                        Status::OkStatus(), kHttpOk));
  fake_boot_time += zx::min(20);
  RunLoopFor(zx::min(20));
  EXPECT_EQ(observed_status, KeyFetchStatus::kSuccess);
  EXPECT_EQ(observed_failures, 0u);
  EXPECT_EQ(observed_time_secs, kRecoveryAttemptSecs);
}

TEST_F(KeyFetcherTest, ScheduledFetchDiskWriteFailureStillAppliesKeysInMemory) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(&clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(&clock);

  const std::string pb_bytes = BuildPublicKeysBytes(
      kFetchedShufflerKeyIndex, kFetchedAnalyzerKeyIndex, kUpdatedExpirationSecs);
  auto stub_http_client =
      std::make_unique<StubHTTPClient>(lib::HTTPResponse(pb_bytes, Status::OkStatus(), kHttpOk));

  KeyFetcher fetcher(dispatcher(), std::move(stub_http_client), &shuffler_maker, &analyzer_maker,
                     &clock, system_data::Environment::PROD, kTestCacheDir, kTestPublicKeysUrl,
                     ImmediateScheduleOptions());

  // Make cache_file_path() a directory so files::WriteFile fails.
  ASSERT_TRUE(files::CreateDirectory(fetcher.cache_file_path()));

  fetcher.OnSystemClockBecomesAccurate();
  RunLoopUntilIdle();

  EXPECT_EQ(fetcher.last_fetch_status(), KeyFetchStatus::kSuccess);
  EXPECT_EQ(fetcher.consecutive_fetch_failures(), 0u);
  EXPECT_EQ(shuffler_maker.key_index(), kFetchedShufflerKeyIndex);
  EXPECT_EQ(analyzer_maker.key_index(), kFetchedAnalyzerKeyIndex);
}

TEST_F(KeyFetcherTest, ScheduledFetchTransportErrorReturnsNetworkError) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(&clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(&clock);

  auto stub_http_client =
      std::make_unique<StubHTTPClient>(Status(StatusCode::UNAVAILABLE, "connection failed"));

  KeyFetcher fetcher(dispatcher(), std::move(stub_http_client), &shuffler_maker, &analyzer_maker,
                     &clock, system_data::Environment::PROD, kTestCacheDir, kTestPublicKeysUrl,
                     ImmediateScheduleOptions());

  fetcher.OnSystemClockBecomesAccurate();
  RunLoopUntilIdle();

  EXPECT_EQ(fetcher.last_fetch_status(), KeyFetchStatus::kNetworkError);
  EXPECT_EQ(fetcher.consecutive_fetch_failures(), 1u);
}

TEST_F(KeyFetcherTest, ScheduledFetchResponseStatusErrorReturnsNetworkError) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(&clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(&clock);

  auto stub_http_client = std::make_unique<StubHTTPClient>(
      lib::HTTPResponse("body", Status(StatusCode::UNAVAILABLE, "connection failed"), kHttpOk));

  KeyFetcher fetcher(dispatcher(), std::move(stub_http_client), &shuffler_maker, &analyzer_maker,
                     &clock, system_data::Environment::PROD, kTestCacheDir, kTestPublicKeysUrl,
                     ImmediateScheduleOptions());

  fetcher.OnSystemClockBecomesAccurate();
  RunLoopUntilIdle();

  EXPECT_EQ(fetcher.last_fetch_status(), KeyFetchStatus::kNetworkError);
  EXPECT_EQ(fetcher.consecutive_fetch_failures(), 1u);
}

TEST_F(KeyFetcherTest, ScheduledFetchHttpErrorReturnsNetworkError) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(&clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(&clock);

  auto stub_http_client = std::make_unique<StubHTTPClient>(
      lib::HTTPResponse("unavailable", Status::OkStatus(), kHttpServiceUnavailable));

  KeyFetcher fetcher(dispatcher(), std::move(stub_http_client), &shuffler_maker, &analyzer_maker,
                     &clock, system_data::Environment::PROD, kTestCacheDir, kTestPublicKeysUrl,
                     ImmediateScheduleOptions());

  fetcher.OnSystemClockBecomesAccurate();
  RunLoopUntilIdle();

  EXPECT_EQ(fetcher.last_fetch_status(), KeyFetchStatus::kNetworkError);
  EXPECT_EQ(fetcher.consecutive_fetch_failures(), 1u);
}

TEST_F(KeyFetcherTest, ScheduledFetchEmptyResponseBodyReturnsNetworkError) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(&clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(&clock);

  auto stub_http_client =
      std::make_unique<StubHTTPClient>(lib::HTTPResponse("", Status::OkStatus(), kHttpOk));

  KeyFetcher fetcher(dispatcher(), std::move(stub_http_client), &shuffler_maker, &analyzer_maker,
                     &clock, system_data::Environment::PROD, kTestCacheDir, kTestPublicKeysUrl,
                     ImmediateScheduleOptions());

  fetcher.OnSystemClockBecomesAccurate();
  RunLoopUntilIdle();

  EXPECT_EQ(fetcher.last_fetch_status(), KeyFetchStatus::kNetworkError);
  EXPECT_EQ(fetcher.consecutive_fetch_failures(), 1u);
}

TEST_F(KeyFetcherTest, ScheduledFetchCorruptPayloadReturnsParseError) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(&clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(&clock);

  auto stub_http_client = std::make_unique<StubHTTPClient>(
      lib::HTTPResponse("not-a-proto", Status::OkStatus(), kHttpOk));

  KeyFetcher fetcher(dispatcher(), std::move(stub_http_client), &shuffler_maker, &analyzer_maker,
                     &clock, system_data::Environment::PROD, kTestCacheDir, kTestPublicKeysUrl,
                     ImmediateScheduleOptions());

  fetcher.OnSystemClockBecomesAccurate();
  RunLoopUntilIdle();

  EXPECT_EQ(fetcher.last_fetch_status(), KeyFetchStatus::kParseError);
  EXPECT_EQ(fetcher.consecutive_fetch_failures(), 1u);
}

TEST_F(KeyFetcherTest, ScheduledFetchInvalidTinkKeysetReturnsInvalidTinkKeyset) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(&clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(&clock);

  CobaltPublicKeys bad_tink_keys;
  bad_tink_keys.mutable_prod_shuffler_key()->set_public_tink_keyset("corrupt-tink-bytes");
  bad_tink_keys.mutable_prod_shuffler_key()->set_key_index(kFetchedShufflerKeyIndex);
  bad_tink_keys.mutable_prod_shuffler_key()->set_expiration_timestamp_secs(kValidExpirationSecs);
  bad_tink_keys.mutable_prod_analyzer_key()->set_public_tink_keyset("corrupt-tink-bytes");
  bad_tink_keys.mutable_prod_analyzer_key()->set_key_index(kFetchedAnalyzerKeyIndex);
  bad_tink_keys.mutable_prod_analyzer_key()->set_expiration_timestamp_secs(kValidExpirationSecs);

  auto stub_http_client = std::make_unique<StubHTTPClient>(
      lib::HTTPResponse(bad_tink_keys.SerializeAsString(), Status::OkStatus(), kHttpOk));

  KeyFetcher fetcher(dispatcher(), std::move(stub_http_client), &shuffler_maker, &analyzer_maker,
                     &clock, system_data::Environment::PROD, kTestCacheDir, kTestPublicKeysUrl,
                     ImmediateScheduleOptions());

  fetcher.OnSystemClockBecomesAccurate();
  RunLoopUntilIdle();

  EXPECT_EQ(fetcher.last_fetch_status(), KeyFetchStatus::kInvalidTinkKeyset);
  EXPECT_EQ(fetcher.consecutive_fetch_failures(), 1u);
}

TEST_F(KeyFetcherTest, ScheduledFetchExpiredKeyReturnsExpiredKey) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(&clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(&clock);

  auto stub_http_client = std::make_unique<StubHTTPClient>(lib::HTTPResponse(
      BuildPublicKeysBytes(kFetchedShufflerKeyIndex, kFetchedAnalyzerKeyIndex, kExpiredTimeSecs),
      Status::OkStatus(), kHttpOk));

  KeyFetcher fetcher(dispatcher(), std::move(stub_http_client), &shuffler_maker, &analyzer_maker,
                     &clock, system_data::Environment::PROD, kTestCacheDir, kTestPublicKeysUrl,
                     ImmediateScheduleOptions());

  fetcher.OnSystemClockBecomesAccurate();
  RunLoopUntilIdle();

  EXPECT_EQ(fetcher.last_fetch_status(), KeyFetchStatus::kExpiredKey);
  EXPECT_EQ(fetcher.consecutive_fetch_failures(), 1u);
}

TEST_F(KeyFetcherTest, ScheduledFetchFailurePreservesExistingActiveKeys) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(&clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(&clock);

  auto stub_http_client = std::make_unique<StubHTTPClient>(lib::HTTPResponse(
      BuildPublicKeysBytes(kFetchedShufflerKeyIndex, kFetchedAnalyzerKeyIndex, kExpiredTimeSecs),
      Status::OkStatus(), kHttpOk));
  KeyFetcher fetcher(dispatcher(), std::move(stub_http_client), &shuffler_maker, &analyzer_maker,
                     &clock, system_data::Environment::PROD, kTestCacheDir, kTestPublicKeysUrl,
                     ImmediateScheduleOptions());

  ASSERT_TRUE(
      files::WriteFile(fetcher.cache_file_path(),
                       BuildPublicKeysBytes(kCachedShufflerKeyIndex, kCachedAnalyzerKeyIndex,
                                            kValidExpirationSecs)));
  ASSERT_TRUE(fetcher.LoadKeysFromDiskCache());
  ASSERT_EQ(shuffler_maker.key_index(), kCachedShufflerKeyIndex);
  ASSERT_EQ(analyzer_maker.key_index(), kCachedAnalyzerKeyIndex);

  fetcher.OnSystemClockBecomesAccurate();
  RunLoopUntilIdle();

  EXPECT_EQ(fetcher.last_fetch_status(), KeyFetchStatus::kExpiredKey);
  EXPECT_EQ(shuffler_maker.key_index(), kCachedShufflerKeyIndex);
  EXPECT_EQ(analyzer_maker.key_index(), kCachedAnalyzerKeyIndex);
}

TEST_F(KeyFetcherTest, ScheduledFetchRejectsOlderKeyIndexThanActiveKeys) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(&clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(&clock);

  const std::string older_pb_bytes =
      BuildPublicKeysBytes(kOlderShufflerKeyIndex, kOlderAnalyzerKeyIndex, kUpdatedExpirationSecs);
  auto stub_http_client = std::make_unique<StubHTTPClient>(
      lib::HTTPResponse(older_pb_bytes, Status::OkStatus(), kHttpOk));
  KeyFetcher fetcher(dispatcher(), std::move(stub_http_client), &shuffler_maker, &analyzer_maker,
                     &clock, system_data::Environment::PROD, kTestCacheDir, kTestPublicKeysUrl,
                     ImmediateScheduleOptions());

  const std::string active_pb_bytes =
      BuildPublicKeysBytes(kCachedShufflerKeyIndex, kCachedAnalyzerKeyIndex, kValidExpirationSecs);
  ASSERT_TRUE(files::WriteFile(fetcher.cache_file_path(), active_pb_bytes));
  ASSERT_TRUE(fetcher.LoadKeysFromDiskCache());

  fetcher.OnSystemClockBecomesAccurate();
  RunLoopUntilIdle();

  EXPECT_EQ(fetcher.last_fetch_status(), KeyFetchStatus::kParseError);
  EXPECT_EQ(fetcher.consecutive_fetch_failures(), 1u);
  EXPECT_EQ(shuffler_maker.key_index(), kCachedShufflerKeyIndex);
  EXPECT_EQ(analyzer_maker.key_index(), kCachedAnalyzerKeyIndex);

  std::string cached_bytes;
  ASSERT_TRUE(files::ReadFileToString(fetcher.cache_file_path(), &cached_bytes));
  EXPECT_EQ(cached_bytes, active_pb_bytes);
}

TEST_F(KeyFetcherTest, ScheduledFetchRejectsOlderAnalyzerKeyIndexWhenShufflerKeyIndexIsNewer) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(&clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(&clock);

  const std::string asymmetric_rollback_pb_bytes = BuildPublicKeysBytes(
      kFetchedShufflerKeyIndex, kOlderAnalyzerKeyIndex, kUpdatedExpirationSecs);
  auto stub_http_client = std::make_unique<StubHTTPClient>(
      lib::HTTPResponse(asymmetric_rollback_pb_bytes, Status::OkStatus(), kHttpOk));
  KeyFetcher fetcher(dispatcher(), std::move(stub_http_client), &shuffler_maker, &analyzer_maker,
                     &clock, system_data::Environment::PROD, kTestCacheDir, kTestPublicKeysUrl,
                     ImmediateScheduleOptions());

  const std::string active_pb_bytes =
      BuildPublicKeysBytes(kCachedShufflerKeyIndex, kCachedAnalyzerKeyIndex, kValidExpirationSecs);
  ASSERT_TRUE(files::WriteFile(fetcher.cache_file_path(), active_pb_bytes));
  ASSERT_TRUE(fetcher.LoadKeysFromDiskCache());

  fetcher.OnSystemClockBecomesAccurate();
  RunLoopUntilIdle();

  EXPECT_EQ(fetcher.last_fetch_status(), KeyFetchStatus::kParseError);
  EXPECT_EQ(fetcher.consecutive_fetch_failures(), 1u);
  EXPECT_EQ(shuffler_maker.key_index(), kCachedShufflerKeyIndex);
  EXPECT_EQ(analyzer_maker.key_index(), kCachedAnalyzerKeyIndex);

  std::string cached_bytes;
  ASSERT_TRUE(files::ReadFileToString(fetcher.cache_file_path(), &cached_bytes));
  EXPECT_EQ(cached_bytes, active_pb_bytes);
}

TEST_F(KeyFetcherTest, OnSystemClockBecomesAccurateTriggersInitialFetch) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(&clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(&clock);

  auto stub_http_client = std::make_unique<StubHTTPClient>(
      lib::HTTPResponse(BuildPublicKeysBytes(kFetchedShufflerKeyIndex, kFetchedAnalyzerKeyIndex,
                                             kScheduledExpirationSecs),
                        Status::OkStatus(), kHttpOk));
  auto* stub_http_client_ptr = stub_http_client.get();

  KeyFetcher fetcher(dispatcher(), std::move(stub_http_client), &shuffler_maker, &analyzer_maker,
                     &clock, system_data::Environment::PROD, kTestCacheDir, kTestPublicKeysUrl,
                     ImmediateScheduleOptions());

  fetcher.OnSystemClockBecomesAccurate();
  RunLoopUntilIdle();

  EXPECT_EQ(stub_http_client_ptr->get_call_count(), 1);
  EXPECT_EQ(shuffler_maker.key_index(), kFetchedShufflerKeyIndex);
  EXPECT_EQ(analyzer_maker.key_index(), kFetchedAnalyzerKeyIndex);
}

TEST_F(KeyFetcherTest, OnSystemClockBecomesAccurateWithUninitializedMakersBypassesInitialJitter) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(&clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(&clock);

  auto stub_http_client = std::make_unique<StubHTTPClient>(
      lib::HTTPResponse(BuildPublicKeysBytes(kFetchedShufflerKeyIndex, kFetchedAnalyzerKeyIndex,
                                             kScheduledExpirationSecs),
                        Status::OkStatus(), kHttpOk));
  auto* stub_http_client_ptr = stub_http_client.get();

  KeyFetcher::ScheduleOptions opts = ImmediateScheduleOptions();
  opts.max_initial_jitter = zx::min(15);
  KeyFetcher fetcher(dispatcher(), std::move(stub_http_client), &shuffler_maker, &analyzer_maker,
                     &clock, system_data::Environment::PROD, kTestCacheDir, kTestPublicKeysUrl,
                     opts);

  fetcher.OnSystemClockBecomesAccurate();
  RunLoopUntilIdle();

  EXPECT_EQ(stub_http_client_ptr->get_call_count(), 1);
  EXPECT_EQ(shuffler_maker.key_index(), kFetchedShufflerKeyIndex);
  EXPECT_EQ(analyzer_maker.key_index(), kFetchedAnalyzerKeyIndex);
}

TEST_F(KeyFetcherTest, OnSystemClockBecomesAccurateWithExpiredCachedKeysBypassesInitialJitter) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(
      util::ParsedKey(util::EncryptedMessageMaker::MakeUnencrypted(), kBundledShufflerKeyIndex,
                      kBundledKeyExpirationSecs),
      &clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(
      util::ParsedKey(util::EncryptedMessageMaker::MakeUnencrypted(), kBundledAnalyzerKeyIndex,
                      kBundledKeyExpirationSecs),
      &clock);

  auto stub_http_client = std::make_unique<StubHTTPClient>(
      lib::HTTPResponse(BuildPublicKeysBytes(kFetchedShufflerKeyIndex, kFetchedAnalyzerKeyIndex,
                                             kScheduledExpirationSecs),
                        Status::OkStatus(), kHttpOk));
  auto* stub_http_client_ptr = stub_http_client.get();

  KeyFetcher::ScheduleOptions opts = ImmediateScheduleOptions();
  opts.max_initial_jitter = zx::min(15);
  KeyFetcher fetcher(dispatcher(), std::move(stub_http_client), &shuffler_maker, &analyzer_maker,
                     &clock, system_data::Environment::PROD, kTestCacheDir, kTestPublicKeysUrl,
                     opts);

  // Load keys at boot (when clock is at backstop time kInitialTimeSecs < kValidExpirationSecs).
  ASSERT_TRUE(
      files::WriteFile(fetcher.cache_file_path(),
                       BuildPublicKeysBytes(kCachedShufflerKeyIndex, kCachedAnalyzerKeyIndex,
                                            kValidExpirationSecs)));
  ASSERT_TRUE(fetcher.LoadKeysFromDiskCache());

  // Now the UTC clock jumps past kValidExpirationSecs and synchronizes.
  clock.SetNowSecs(kUpdatedExpirationSecs);
  fetcher.OnSystemClockBecomesAccurate();
  RunLoopUntilIdle();

  EXPECT_EQ(stub_http_client_ptr->get_call_count(), 1);
  EXPECT_EQ(shuffler_maker.key_index(), kFetchedShufflerKeyIndex);
  EXPECT_EQ(analyzer_maker.key_index(), kFetchedAnalyzerKeyIndex);
}

TEST_F(KeyFetcherTest, ScheduledPollingAppliesInitialAndSteadyStateJitter) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(
      util::ParsedKey(util::EncryptedMessageMaker::MakeUnencrypted(), kBundledShufflerKeyIndex,
                      kBundledKeyExpirationSecs),
      &clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(
      util::ParsedKey(util::EncryptedMessageMaker::MakeUnencrypted(), kBundledAnalyzerKeyIndex,
                      kBundledKeyExpirationSecs),
      &clock);

  auto stub_http_client = std::make_unique<StubHTTPClient>(
      lib::HTTPResponse(BuildPublicKeysBytes(kFetchedShufflerKeyIndex, kFetchedAnalyzerKeyIndex,
                                             kScheduledExpirationSecs),
                        Status::OkStatus(), kHttpOk));
  auto* stub_http_client_ptr = stub_http_client.get();

  zx::duration boot_offset{0};
  KeyFetcher::ScheduleOptions opts{
      .max_initial_jitter = zx::min(15),
      .steady_state_interval = zx::hour(24),
      .steady_state_jitter = zx::min(30),
      .poll_interval = zx::hour(1),
      .boot_clock = [this,
                     &boot_offset] { return zx::time_boot(test_loop().Now().get()) + boot_offset; },
  };

  KeyFetcher fetcher(dispatcher(), std::move(stub_http_client), &shuffler_maker, &analyzer_maker,
                     &clock, system_data::Environment::PROD, kTestCacheDir, kTestPublicKeysUrl,
                     opts);

  fetcher.OnSystemClockBecomesAccurate();
  // After 15 minutes (the upper bound of initial jitter), the initial fetch must have fired.
  RunLoopFor(zx::min(15));
  ASSERT_EQ(stub_http_client_ptr->get_call_count(), 1);

  // At 23h15m total boot time (before the 23h30m lower bound of 0 + 24h - 30m), the next fetch
  // has not fired.
  boot_offset += zx::hour(22);
  RunLoopFor(zx::hour(1));
  EXPECT_EQ(stub_http_client_ptr->get_call_count(), 1);

  // At 25h15m total boot time (past the 24h45m upper bound of 15m + 24h + 30m), the next fetch
  // must have fired.
  boot_offset += zx::hour(1);
  RunLoopFor(zx::hour(1));
  EXPECT_EQ(stub_http_client_ptr->get_call_count(), 2);
}

TEST_F(KeyFetcherTest, ScheduledFetchFailureRetriesWithBackoffAndRecovers) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(&clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(&clock);

  auto stub_http_client = std::make_unique<StubHTTPClient>(
      lib::HTTPResponse("unavailable", Status::OkStatus(), kHttpServiceUnavailable));
  auto* stub_http_client_ptr = stub_http_client.get();

  zx::time_boot fake_boot_time{0};
  KeyFetcher::ScheduleOptions opts{
      .max_initial_jitter = zx::sec(0),
      .steady_state_interval = zx::hour(24),
      .steady_state_jitter = zx::sec(0),
      .poll_interval = zx::hour(1),
      .boot_clock = [&fake_boot_time] { return fake_boot_time; },
  };

  KeyFetcher fetcher(dispatcher(), std::move(stub_http_client), &shuffler_maker, &analyzer_maker,
                     &clock, system_data::Environment::PROD, kTestCacheDir, kTestPublicKeysUrl,
                     opts);

  fetcher.OnSystemClockBecomesAccurate();
  RunLoopUntilIdle();
  EXPECT_EQ(stub_http_client_ptr->get_call_count(), 1);
  EXPECT_EQ(fetcher.last_fetch_status(), KeyFetchStatus::kNetworkError);
  EXPECT_EQ(fetcher.consecutive_fetch_failures(), 1u);

  // Failure 1 -> retry delay is in [5m, 10m].
  fake_boot_time += zx::min(4);
  RunLoopFor(zx::min(4));
  EXPECT_EQ(stub_http_client_ptr->get_call_count(), 1);
  fake_boot_time += zx::min(6);
  RunLoopFor(zx::min(6));
  EXPECT_EQ(stub_http_client_ptr->get_call_count(), 2);
  EXPECT_EQ(fetcher.consecutive_fetch_failures(), 2u);

  // Failure 2 -> retry delay is in [10m, 20m].
  fake_boot_time += zx::min(9);
  RunLoopFor(zx::min(9));
  EXPECT_EQ(stub_http_client_ptr->get_call_count(), 2);
  fake_boot_time += zx::min(11);
  RunLoopFor(zx::min(11));
  EXPECT_EQ(stub_http_client_ptr->get_call_count(), 3);
  EXPECT_EQ(fetcher.consecutive_fetch_failures(), 3u);

  // Failure 3 -> retry delay is in [20m, 40m].
  fake_boot_time += zx::min(19);
  RunLoopFor(zx::min(19));
  EXPECT_EQ(stub_http_client_ptr->get_call_count(), 3);
  fake_boot_time += zx::min(21);
  RunLoopFor(zx::min(21));
  EXPECT_EQ(stub_http_client_ptr->get_call_count(), 4);
  EXPECT_EQ(fetcher.consecutive_fetch_failures(), 4u);

  // Failure 4 -> retry delay is in [40m, 60m].
  fake_boot_time += zx::min(39);
  RunLoopFor(zx::min(39));
  EXPECT_EQ(stub_http_client_ptr->get_call_count(), 4);
  fake_boot_time += zx::min(21);
  RunLoopFor(zx::min(21));
  EXPECT_EQ(stub_http_client_ptr->get_call_count(), 5);
  EXPECT_EQ(fetcher.consecutive_fetch_failures(), 5u);

  // Failure >= 5 remains capped at 60 minutes and recovers on success.
  stub_http_client_ptr->SetResponse(
      lib::HTTPResponse(BuildPublicKeysBytes(kFetchedShufflerKeyIndex, kFetchedAnalyzerKeyIndex,
                                             kScheduledExpirationSecs),
                        Status::OkStatus(), kHttpOk));
  fake_boot_time += zx::min(59);
  RunLoopFor(zx::min(59));
  EXPECT_EQ(stub_http_client_ptr->get_call_count(), 5);
  fake_boot_time += zx::min(1);
  RunLoopFor(zx::min(1));

  EXPECT_EQ(stub_http_client_ptr->get_call_count(), 6);
  EXPECT_EQ(fetcher.last_fetch_status(), KeyFetchStatus::kSuccess);
  EXPECT_EQ(fetcher.consecutive_fetch_failures(), 0u);
  EXPECT_EQ(shuffler_maker.key_index(), kFetchedShufflerKeyIndex);
  EXPECT_EQ(analyzer_maker.key_index(), kFetchedAnalyzerKeyIndex);

  // Subsequent failure after recovery resets backoff to [5m, 10m].
  stub_http_client_ptr->SetResponse(
      lib::HTTPResponse("unavailable", Status::OkStatus(), kHttpServiceUnavailable));
  fake_boot_time += zx::hour(24);
  RunLoopFor(zx::hour(1));
  EXPECT_EQ(stub_http_client_ptr->get_call_count(), 7);
  EXPECT_EQ(fetcher.consecutive_fetch_failures(), 1u);

  fake_boot_time += zx::min(10);
  RunLoopFor(zx::min(10));
  EXPECT_EQ(stub_http_client_ptr->get_call_count(), 8);
  EXPECT_EQ(fetcher.consecutive_fetch_failures(), 2u);
}

TEST_F(KeyFetcherTest, SteadyStatePollingChunksIntervalsUntilBootDeadline) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(&clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(&clock);

  auto stub_http_client = std::make_unique<StubHTTPClient>(
      lib::HTTPResponse(BuildPublicKeysBytes(kCachedShufflerKeyIndex, kCachedAnalyzerKeyIndex,
                                             kScheduledExpirationSecs),
                        Status::OkStatus(), kHttpOk));
  auto* stub_http_client_ptr = stub_http_client.get();

  zx::time_boot fake_boot_time{0};
  KeyFetcher::ScheduleOptions opts{
      .max_initial_jitter = zx::sec(0),
      .steady_state_interval = zx::hour(24),
      .steady_state_jitter = zx::sec(0),
      .poll_interval = zx::hour(1),
      .boot_clock = [&fake_boot_time] { return fake_boot_time; },
  };

  KeyFetcher fetcher(dispatcher(), std::move(stub_http_client), &shuffler_maker, &analyzer_maker,
                     &clock, system_data::Environment::PROD, kTestCacheDir, kTestPublicKeysUrl,
                     opts);

  fetcher.OnSystemClockBecomesAccurate();
  RunLoopUntilIdle();
  ASSERT_EQ(stub_http_client_ptr->get_call_count(), 1);
  ASSERT_EQ(shuffler_maker.key_index(), kCachedShufflerKeyIndex);

  // After 1 hour of monotonic and boot time, the 24-hour deadline has not been reached.
  fake_boot_time += zx::hour(1);
  RunLoopFor(zx::hour(1));
  EXPECT_EQ(stub_http_client_ptr->get_call_count(), 1);

  // When boot time reaches the 24-hour deadline (e.g. including suspend time), the next poll
  // triggers a network fetch.
  stub_http_client_ptr->SetResponse(
      lib::HTTPResponse(BuildPublicKeysBytes(kFetchedShufflerKeyIndex, kFetchedAnalyzerKeyIndex,
                                             kScheduledExpirationSecs),
                        Status::OkStatus(), kHttpOk));
  fake_boot_time += zx::hour(23);
  RunLoopFor(zx::hour(1));
  EXPECT_EQ(stub_http_client_ptr->get_call_count(), 2);
  EXPECT_EQ(shuffler_maker.key_index(), kFetchedShufflerKeyIndex);
  EXPECT_EQ(analyzer_maker.key_index(), kFetchedAnalyzerKeyIndex);
}

TEST_F(KeyFetcherTest, SteadyStatePollingFetchesImmediatelyWhenActiveKeyExpiresEarly) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(&clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(&clock);

  auto stub_http_client = std::make_unique<StubHTTPClient>(lib::HTTPResponse(
      BuildPublicKeysBytes(kCachedShufflerKeyIndex, kCachedAnalyzerKeyIndex, kValidExpirationSecs),
      Status::OkStatus(), kHttpOk));
  auto* stub_http_client_ptr = stub_http_client.get();

  zx::time_boot fake_boot_time{0};
  KeyFetcher::ScheduleOptions opts{
      .max_initial_jitter = zx::sec(0),
      .steady_state_interval = zx::hour(24),
      .steady_state_jitter = zx::sec(0),
      .poll_interval = zx::hour(1),
      .boot_clock = [&fake_boot_time] { return fake_boot_time; },
  };

  KeyFetcher fetcher(dispatcher(), std::move(stub_http_client), &shuffler_maker, &analyzer_maker,
                     &clock, system_data::Environment::PROD, kTestCacheDir, kTestPublicKeysUrl,
                     opts);

  fetcher.OnSystemClockBecomesAccurate();
  RunLoopUntilIdle();
  ASSERT_EQ(stub_http_client_ptr->get_call_count(), 1);
  ASSERT_EQ(shuffler_maker.key_index(), kCachedShufflerKeyIndex);

  // Advance system clock past kValidExpirationSecs before the 24-hour boot deadline.
  clock.SetNowSecs(kUpdatedExpirationSecs);
  stub_http_client_ptr->SetResponse(
      lib::HTTPResponse(BuildPublicKeysBytes(kFetchedShufflerKeyIndex, kFetchedAnalyzerKeyIndex,
                                             kScheduledExpirationSecs),
                        Status::OkStatus(), kHttpOk));

  fake_boot_time += zx::hour(1);
  RunLoopFor(zx::hour(1));
  EXPECT_EQ(stub_http_client_ptr->get_call_count(), 2);
  EXPECT_EQ(shuffler_maker.key_index(), kFetchedShufflerKeyIndex);
  EXPECT_EQ(analyzer_maker.key_index(), kFetchedAnalyzerKeyIndex);
}

TEST_F(KeyFetcherTest, DestructorCancelsPendingScheduledFetchTask) {
  FakeSystemClock clock(kInitialTimeSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(
      util::ParsedKey(util::EncryptedMessageMaker::MakeUnencrypted(), kBundledShufflerKeyIndex,
                      kBundledKeyExpirationSecs),
      &clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(
      util::ParsedKey(util::EncryptedMessageMaker::MakeUnencrypted(), kBundledAnalyzerKeyIndex,
                      kBundledKeyExpirationSecs),
      &clock);

  {
    auto stub_http_client = std::make_unique<StubHTTPClient>(
        lib::HTTPResponse(BuildPublicKeysBytes(kFetchedShufflerKeyIndex, kFetchedAnalyzerKeyIndex,
                                               kScheduledExpirationSecs),
                          Status::OkStatus(), kHttpOk));
    KeyFetcher::ScheduleOptions opts = ImmediateScheduleOptions();
    opts.max_initial_jitter = zx::min(15);
    KeyFetcher fetcher(dispatcher(), std::move(stub_http_client), &shuffler_maker, &analyzer_maker,
                       &clock, system_data::Environment::PROD, kTestCacheDir, kTestPublicKeysUrl,
                       opts);
    fetcher.OnSystemClockBecomesAccurate();
  }

  RunLoopFor(zx::min(15));
  EXPECT_EQ(shuffler_maker.key_index(), kBundledShufflerKeyIndex);
  EXPECT_EQ(analyzer_maker.key_index(), kBundledAnalyzerKeyIndex);
}

}  // namespace
}  // namespace cobalt
