// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/cobalt/bin/app/key_fetcher.h"

#include <lib/syslog/cpp/macros.h>
#include <zircon/syscalls.h>

#include <algorithm>
#include <chrono>
#include <utility>

#include "src/lib/files/directory.h"
#include "src/lib/files/file.h"
#include "src/lib/files/path.h"
#include "third_party/cobalt/src/lib/util/datetime_util.h"

namespace cobalt {
namespace {

constexpr char kPublicKeysFilename[] = "public_keys.pb";
constexpr auto kHttpFetchTimeout = std::chrono::seconds(30);
constexpr int64_t kHttpOk = 200;

bool IsKeysetExpired(const util::ParsedKeyset& keyset, int64_t now_secs) {
  return now_secs >= keyset.shuffler_key().expiration_timestamp_secs() ||
         now_secs >= keyset.analyzer_key().expiration_timestamp_secs();
}

zx::duration RandomDuration(zx::duration min_inclusive, zx::duration max_inclusive) {
  if (max_inclusive <= min_inclusive) {
    return min_inclusive;
  }
  uint64_t rand_val = 0;
  zx_cprng_draw(&rand_val, sizeof(rand_val));
  const int64_t min_nsecs = min_inclusive.to_nsecs();
  const int64_t max_nsecs = max_inclusive.to_nsecs();
  const uint64_t span = static_cast<uint64_t>(max_nsecs) - static_cast<uint64_t>(min_nsecs) + 1;
  const uint64_t offset = (span == 0) ? rand_val : (rand_val % span);
  return zx::nsec(static_cast<int64_t>(static_cast<uint64_t>(min_nsecs) + offset));
}

}  // namespace

const char* KeyFetchStatusToString(KeyFetchStatus status) {
  switch (status) {
    case KeyFetchStatus::kUninitialized:
      return "Uninitialized";
    case KeyFetchStatus::kSuccess:
      return "Success";
    case KeyFetchStatus::kNetworkError:
      return "NetworkError";
    case KeyFetchStatus::kParseError:
      return "ParseError";
    case KeyFetchStatus::kInvalidTinkKeyset:
      return "InvalidTinkKeyset";
    case KeyFetchStatus::kExpiredKey:
      return "ExpiredKey";
  }
  return "Unknown";
}

KeyFetcher::KeyFetcher(async_dispatcher_t* dispatcher, std::unique_ptr<lib::HTTPClient> http_client,
                       util::DynamicEncryptedMessageMaker* shuffler_maker,
                       util::DynamicEncryptedMessageMaker* analyzer_maker,
                       util::SystemClockInterface* system_clock,
                       system_data::Environment environment, std::string cache_dir,
                       std::string public_keys_url)
    : KeyFetcher(dispatcher, std::move(http_client), shuffler_maker, analyzer_maker, system_clock,
                 environment, std::move(cache_dir), std::move(public_keys_url),
                 ScheduleOptions{
                     .max_initial_jitter = zx::sec(60),
                     .steady_state_interval = zx::hour(24),
                     .steady_state_jitter = zx::hour(2),
                     .poll_interval = zx::hour(1),
                     .boot_clock = zx::clock::get_boot,
                 }) {}

KeyFetcher::KeyFetcher(async_dispatcher_t* dispatcher, std::unique_ptr<lib::HTTPClient> http_client,
                       util::DynamicEncryptedMessageMaker* shuffler_maker,
                       util::DynamicEncryptedMessageMaker* analyzer_maker,
                       util::SystemClockInterface* system_clock,
                       system_data::Environment environment, std::string cache_dir,
                       std::string public_keys_url, ScheduleOptions schedule_options)
    : dispatcher_(dispatcher),
      http_client_(std::move(http_client)),
      shuffler_maker_(shuffler_maker),
      analyzer_maker_(analyzer_maker),
      system_clock_(system_clock),
      environment_(environment),
      cache_dir_(std::move(cache_dir)),
      cache_file_path_(files::JoinPath(cache_dir_, kPublicKeysFilename)),
      public_keys_url_(std::move(public_keys_url)),
      schedule_options_(std::move(schedule_options)) {
  FX_CHECK(dispatcher_ != nullptr);
  FX_CHECK(http_client_ != nullptr);
  FX_CHECK(shuffler_maker_ != nullptr);
  FX_CHECK(analyzer_maker_ != nullptr);
  FX_CHECK(system_clock_ != nullptr);
  FX_CHECK(schedule_options_.boot_clock != nullptr);
  FX_CHECK(schedule_options_.max_initial_jitter >= zx::sec(0));
  FX_CHECK(schedule_options_.poll_interval > zx::sec(0));
  FX_CHECK(schedule_options_.steady_state_interval > zx::sec(0));
  FX_CHECK(schedule_options_.steady_state_jitter >= zx::sec(0));
  FX_CHECK(schedule_options_.steady_state_jitter < schedule_options_.steady_state_interval);
}

KeyFetcher::~KeyFetcher() { scheduled_task_.Cancel(); }

bool KeyFetcher::IsOlderThanActiveKeys(const util::ParsedKeyset& keyset) const {
  return (shuffler_maker_->IsInitialized() &&
          keyset.shuffler_key().key_index() < shuffler_maker_->key_index()) ||
         (analyzer_maker_->IsInitialized() &&
          keyset.analyzer_key().key_index() < analyzer_maker_->key_index());
}

bool KeyFetcher::LoadKeysFromDiskCache() {
  if (!files::IsFile(cache_file_path_)) {
    return false;
  }

  std::string pb_bytes;
  if (!files::ReadFileToString(cache_file_path_, &pb_bytes) || pb_bytes.empty()) {
    FX_LOGS(WARNING) << "KeyFetcher: Cached key file is empty or unreadable; deleting "
                     << cache_file_path_;
    files::DeletePath(cache_file_path_, /*recursive=*/false);
    return false;
  }

  lib::statusor::StatusOr<util::ParsedKeyset> parsed =
      util::ParsePublicKeys(pb_bytes, environment_);
  if (!parsed.ok()) {
    FX_LOGS(WARNING) << "KeyFetcher: Corrupt cached key file (" << parsed.status().error_message()
                     << "); deleting " << cache_file_path_;
    files::DeletePath(cache_file_path_, /*recursive=*/false);
    return false;
  }

  if (IsOlderThanActiveKeys(*parsed)) {
    FX_LOGS(WARNING) << "KeyFetcher: Cached public keys at " << cache_file_path_
                     << " are older than bundled keys; deleting " << cache_file_path_;
    files::DeletePath(cache_file_path_, /*recursive=*/false);
    return false;
  }

  const bool is_expired = IsKeysetExpired(*parsed, util::ToUnixSeconds(system_clock_->now()));
  // Apply the cached keyset even if it has expired so that any non-expiring bundled
  // fallback keys are replaced with the newer (now-expired) cached keys. This ensures
  // the makers fail closed (deferring observation encryption) and that
  // OnSystemClockBecomesAccurate() sees IsExpired() == true and fetches immediately.
  // Keep the expired cache file on disk until a valid network fetch overwrites it so a
  // subsequent offline reboot does not revert to the older non-expiring bundled keys.
  ApplyKeyset(std::move(*parsed));
  if (is_expired) {
    FX_LOGS(WARNING) << "KeyFetcher: Cached public keys at " << cache_file_path_
                     << " have expired.";
    return false;
  }

  return true;
}

void KeyFetcher::OnSystemClockBecomesAccurate() {
  zx::duration initial_jitter = ComputeInitialJitter();
  const bool is_expired = shuffler_maker_->IsExpired() || analyzer_maker_->IsExpired();
  const bool is_uninitialized =
      !shuffler_maker_->IsInitialized() || !analyzer_maker_->IsInitialized();
  if (is_expired) {
    FX_LOGS(WARNING)
        << "KeyFetcher: Active keys are expired after system clock sync; fetching immediately.";
    initial_jitter = zx::sec(0);
  } else if (is_uninitialized) {
    FX_LOGS(WARNING)
        << "KeyFetcher: Active keys are uninitialized on system clock sync; fetching immediately.";
    initial_jitter = zx::sec(0);
  }
  target_boot_deadline_ = schedule_options_.boot_clock() + initial_jitter;
  ScheduleNextPoll(std::min(initial_jitter, schedule_options_.poll_interval));
}

KeyFetchStatus KeyFetcher::FetchAndCachePublicKeys() {
  const auto deadline = std::chrono::steady_clock::now() + kHttpFetchTimeout;
  lib::statusor::StatusOr<lib::HTTPResponse> response =
      http_client_->GetSync(lib::HTTPRequest(public_keys_url_), deadline);
  if (!response.ok() || !response->status.ok() || response->http_code != kHttpOk ||
      response->response.empty()) {
    return KeyFetchStatus::kNetworkError;
  }

  const std::string& pb_bytes = response->response;
  lib::statusor::StatusOr<util::ParsedKeyset> parsed =
      util::ParsePublicKeys(pb_bytes, environment_);
  if (!parsed.ok()) {
    return (parsed.status().error_code() == StatusCode::FAILED_PRECONDITION)
               ? KeyFetchStatus::kInvalidTinkKeyset
               : KeyFetchStatus::kParseError;
  }

  if (IsOlderThanActiveKeys(*parsed)) {
    FX_LOGS(ERROR) << "KeyFetcher: Fetched public keys are older than active keys.";
    return KeyFetchStatus::kParseError;
  }

  if (IsKeysetExpired(*parsed, util::ToUnixSeconds(system_clock_->now()))) {
    FX_LOGS(ERROR) << "KeyFetcher: Fetched public keys are already expired.";
    return KeyFetchStatus::kExpiredKey;
  }

  if (!files::CreateDirectory(cache_dir_) || !files::WriteFile(cache_file_path_, pb_bytes)) {
    FX_LOGS(ERROR) << "KeyFetcher: Failed to write public keys to " << cache_file_path_;
  }

  ApplyKeyset(std::move(*parsed));
  return KeyFetchStatus::kSuccess;
}

void KeyFetcher::ApplyKeyset(util::ParsedKeyset keyset) {
  // Clamp both keys to the earlier expiration timestamp so they expire together and
  // neither maker remains usable while the other is expired.
  // TODO(https://fxbug.dev/539564173): Pass keyset.TakeShufflerKey() and
  // keyset.TakeAnalyzerKey() directly after https://fxrev.dev/1887274 rolls into
  // //third_party/cobalt.
  const int64_t effective_expiration_secs =
      std::min(keyset.shuffler_key().expiration_timestamp_secs(),
               keyset.analyzer_key().expiration_timestamp_secs());
  util::ParsedKey shuffler_key = keyset.TakeShufflerKey();
  util::ParsedKey analyzer_key = keyset.TakeAnalyzerKey();
  const uint32_t shuffler_index = shuffler_key.key_index();
  const uint32_t analyzer_index = analyzer_key.key_index();
  shuffler_maker_->UpdateKey(util::ParsedKey(std::move(shuffler_key).TakeMaker(), shuffler_index,
                                             effective_expiration_secs));
  analyzer_maker_->UpdateKey(util::ParsedKey(std::move(analyzer_key).TakeMaker(), analyzer_index,
                                             effective_expiration_secs));
}

void KeyFetcher::RunScheduledFetch() {
  last_fetch_attempt_time_secs_ = util::ToUnixSeconds(system_clock_->now());
  last_fetch_status_ = FetchAndCachePublicKeys();
  zx::duration next_delay;
  if (last_fetch_status_ == KeyFetchStatus::kSuccess) {
    consecutive_fetch_failures_ = 0;
    retry_backoff_.Reset();
    next_delay = ComputeSteadyStateIntervalWithJitter();
  } else {
    ++consecutive_fetch_failures_;
    next_delay = retry_backoff_.GetNext();
  }
  if (status_observer_) {
    status_observer_(last_fetch_status_, consecutive_fetch_failures_,
                     last_fetch_attempt_time_secs_);
  }
  target_boot_deadline_ = schedule_options_.boot_clock() + next_delay;
  ScheduleNextPoll(std::min(next_delay, schedule_options_.poll_interval));
}

void KeyFetcher::ScheduleNextPoll(zx::duration delay) {
  scheduled_task_.Cancel();
  const zx_status_t status = scheduled_task_.PostDelayed(dispatcher_, delay);
  if (status != ZX_OK && status != ZX_ERR_BAD_STATE) {
    FX_PLOGS(ERROR, status) << "KeyFetcher: Failed to post scheduled poll task";
  }
}

void KeyFetcher::CheckBootDeadlineAndFetch() {
  const zx::time_boot now_boot = schedule_options_.boot_clock();
  // Only trigger an immediate fetch on active key expiration when not already in
  // failure backoff; once a fetch fails, wait for the backoff deadline in
  // target_boot_deadline_ rather than retrying on every poll wakeup.
  const bool active_key_expired = consecutive_fetch_failures_ == 0 &&
                                  (shuffler_maker_->IsExpired() || analyzer_maker_->IsExpired());
  if (now_boot >= target_boot_deadline_ || active_key_expired) {
    RunScheduledFetch();
    return;
  }
  const zx::duration remaining = target_boot_deadline_ - now_boot;
  ScheduleNextPoll(std::min(remaining, schedule_options_.poll_interval));
}

zx::duration KeyFetcher::ComputeInitialJitter() const {
  if (schedule_options_.max_initial_jitter <= zx::sec(0)) {
    return zx::sec(0);
  }
  return RandomDuration(zx::sec(0), schedule_options_.max_initial_jitter);
}

zx::duration KeyFetcher::ComputeSteadyStateIntervalWithJitter() const {
  if (schedule_options_.steady_state_jitter <= zx::sec(0)) {
    return schedule_options_.steady_state_interval;
  }
  return schedule_options_.steady_state_interval +
         RandomDuration(-schedule_options_.steady_state_jitter,
                        schedule_options_.steady_state_jitter);
}

}  // namespace cobalt
