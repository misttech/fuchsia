// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_COBALT_BIN_APP_KEY_FETCHER_H_
#define SRC_COBALT_BIN_APP_KEY_FETCHER_H_

#include <lib/async/cpp/task.h>
#include <lib/async/dispatcher.h>
#include <lib/fit/function.h>
#include <lib/zx/clock.h>
#include <lib/zx/time.h>

#include <cstdint>
#include <functional>
#include <limits>
#include <memory>
#include <string>
#include <utility>

#include "src/lib/backoff/exponential_backoff.h"
#include "third_party/cobalt/src/lib/util/dynamic_encrypted_message_maker.h"
#include "third_party/cobalt/src/public/lib/clock_interfaces.h"
#include "third_party/cobalt/src/public/lib/http_client.h"
#include "third_party/cobalt/src/system_data/configuration_data.h"

namespace cobalt {

// Sentinel expiration timestamp for static keys bundled in the package, which do not expire.
// TODO(https://fxbug.dev/539564173): Remove bundled key fallback after dynamic key rotation is
// rolled out.
inline constexpr int64_t kBundledKeyExpirationSecs = std::numeric_limits<int64_t>::max();

enum class KeyFetchStatus : uint8_t {
  kUninitialized = 0,
  kSuccess = 1,
  kNetworkError = 2,
  kParseError = 3,
  kInvalidTinkKeyset = 4,
  kExpiredKey = 5,
};

[[nodiscard]] const char* KeyFetchStatusToString(KeyFetchStatus status);

class KeyFetcher {
 public:
  // Returns the current time on ZX_CLOCK_BOOT (which continues advancing while
  // the system is suspended).
  using BootClock = std::function<zx::time_boot()>;

  // Invoked on |dispatcher| after each scheduled network key fetch attempt
  // (after any newly fetched keys have been applied to the makers) with the
  // resulting status, current consecutive failure count, and the UTC Unix
  // timestamp in seconds when the fetch was attempted.
  using StatusObserver = fit::function<void(KeyFetchStatus, uint32_t, int64_t)>;

  struct ScheduleOptions {
    // Upper bound for the uniform random delay [0, max_initial_jitter] before
    // the initial network key fetch after the system clock becomes accurate.
    zx::duration max_initial_jitter;

    // Base interval between successful key fetches in steady state.
    zx::duration steady_state_interval;

    // Maximum uniform random jitter [-steady_state_jitter, +steady_state_jitter]
    // added to |steady_state_interval| after a successful key fetch.
    zx::duration steady_state_jitter;

    // Maximum delay for each individual async_dispatcher_t wakeup while waiting
    // for the next fetch deadline. Because async_dispatcher_t schedules delayed
    // tasks on ZX_CLOCK_MONOTONIC (which pauses during system suspend), whereas
    // key rotation deadlines are tracked against ZX_CLOCK_BOOT (which continues
    // advancing across suspend), KeyFetcher wakes up at least once per
    // |poll_interval| to check |boot_clock| against the target boot-time
    // deadline.
    zx::duration poll_interval;

    // Clock used to measure key rotation deadlines across system suspend.
    BootClock boot_clock;
  };

  // |dispatcher|, |http_client|, |shuffler_maker|, |analyzer_maker|, and
  // |system_clock| must be non-null. Unowned pointers must outlive the
  // KeyFetcher instance. All methods and scheduled HTTPClient::GetSync calls
  // execute on |dispatcher|'s thread.
  KeyFetcher(async_dispatcher_t* dispatcher, std::unique_ptr<lib::HTTPClient> http_client,
             util::DynamicEncryptedMessageMaker* shuffler_maker,
             util::DynamicEncryptedMessageMaker* analyzer_maker,
             util::SystemClockInterface* system_clock, system_data::Environment environment,
             std::string cache_dir, std::string public_keys_url);

  KeyFetcher(async_dispatcher_t* dispatcher, std::unique_ptr<lib::HTTPClient> http_client,
             util::DynamicEncryptedMessageMaker* shuffler_maker,
             util::DynamicEncryptedMessageMaker* analyzer_maker,
             util::SystemClockInterface* system_clock, system_data::Environment environment,
             std::string cache_dir, std::string public_keys_url, ScheduleOptions schedule_options);

  ~KeyFetcher();

  KeyFetcher(const KeyFetcher&) = delete;
  KeyFetcher& operator=(const KeyFetcher&) = delete;
  KeyFetcher(KeyFetcher&&) = delete;
  KeyFetcher& operator=(KeyFetcher&&) = delete;

  // Attempts to load cached public keys from <cache_dir>/public_keys.pb on boot.
  // Returns true if valid, unexpired keys were loaded into the DynamicEncryptedMessageMakers.
  // If the cached keyset is newer than or equal to the active keys but already expired, it is
  // applied to the makers (so they fail closed) and kept on disk until overwritten by a valid
  // network fetch before returning false.
  bool LoadKeysFromDiskCache();

  // Called after the Fuchsia system clock has synchronized with an external time source.
  // Schedules the initial network key fetch (immediately if active keys are uninitialized
  // or expired, or with uniform random jitter otherwise).
  void OnSystemClockBecomesAccurate();

  // Sets the optional observer invoked after each scheduled network fetch
  // attempt, replacing any previous observer. Earlier attempts are not
  // replayed. Captured pointers must outlive this KeyFetcher.
  void SetStatusObserver(StatusObserver observer) { status_observer_ = std::move(observer); }

  [[nodiscard]] uint32_t consecutive_fetch_failures() const { return consecutive_fetch_failures_; }
  [[nodiscard]] KeyFetchStatus last_fetch_status() const { return last_fetch_status_; }
  [[nodiscard]] int64_t last_fetch_attempt_time_secs() const {
    return last_fetch_attempt_time_secs_;
  }
  [[nodiscard]] const std::string& cache_file_path() const { return cache_file_path_; }

 private:
  static constexpr zx::duration kInitialRetryDelay = zx::min(5);
  static constexpr uint32_t kRetryBackoffFactor = 2u;
  static constexpr zx::duration kMaxRetryDelay = zx::hour(1);

  [[nodiscard]] bool IsOlderThanActiveKeys(const util::ParsedKeyset& keyset) const;
  [[nodiscard]] KeyFetchStatus FetchAndCachePublicKeys();
  void ApplyKeyset(util::ParsedKeyset keyset);
  void RunScheduledFetch();
  void ScheduleNextPoll(zx::duration delay);
  void CheckBootDeadlineAndFetch();
  [[nodiscard]] zx::duration ComputeInitialJitter() const;
  [[nodiscard]] zx::duration ComputeSteadyStateIntervalWithJitter() const;

  async_dispatcher_t* const dispatcher_;
  const std::unique_ptr<lib::HTTPClient> http_client_;
  util::DynamicEncryptedMessageMaker* const shuffler_maker_;
  util::DynamicEncryptedMessageMaker* const analyzer_maker_;
  util::SystemClockInterface* const system_clock_;
  const system_data::Environment environment_;
  const std::string cache_dir_;
  const std::string cache_file_path_;
  const std::string public_keys_url_;
  const ScheduleOptions schedule_options_;

  backoff::ExponentialBackoff retry_backoff_{kInitialRetryDelay, kRetryBackoffFactor,
                                             kMaxRetryDelay};
  uint32_t consecutive_fetch_failures_ = 0;
  KeyFetchStatus last_fetch_status_ = KeyFetchStatus::kUninitialized;
  int64_t last_fetch_attempt_time_secs_ = 0;
  StatusObserver status_observer_;
  zx::time_boot target_boot_deadline_{0};
  async::TaskClosureMethod<KeyFetcher, &KeyFetcher::CheckBootDeadlineAndFetch> scheduled_task_{
      this};
};

}  // namespace cobalt

#endif  // SRC_COBALT_BIN_APP_KEY_FETCHER_H_
