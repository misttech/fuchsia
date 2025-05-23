// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/media/audio/lib/clock/logging.h"

#include <lib/syslog/cpp/macros.h>

#include <atomic>
#include <iomanip>

namespace media_audio {

namespace {
// Whether to enable LogClockAdjustment. If false, then LogClockAdjustment is a no-op.
constexpr bool kLogClockAdjustment = true;
// Within LogClockAdjustment(), log if position error >= kLogClockAdjustmentPositionErrorThreshold,
// or if clock-rate-change >= kLogClockAdjustmentRateChangeThresholdPpm, or if it has been
// kLogClockAdjustmentStride calls since the last time we logged.
constexpr int64_t kLogClockAdjustmentStride = 1009;  // prime, to avoid periodicity
constexpr zx::duration kLogClockAdjustmentPositionErrorThreshold = zx::nsec(500);
constexpr int64_t kLogClockAdjustmentRateChangeThresholdPpm = 500;
// Should we always log "high error" clock adjustments even if the clock rate is unchanged?
constexpr bool kLogClockRateUnchanged = false;
// Within LogClockAdjustment, whether to include PID coefficients in the log.
constexpr bool kLogClockAdjustmentWithPidCoefficients = false;
}  // namespace

void LogClockAdjustment(const Clock& clock, std::optional<int32_t> last_rate_ppm,
                        int32_t next_rate_ppm, zx::duration pos_error,
                        const ::media::audio::clock::PidControl& pid) {
  if constexpr (!kLogClockAdjustment) {
    return;
  }

  static std::atomic<int64_t> log_count(0);

  // If absolute error or rate change is large enough, then log now but reset our stride.
  bool big_error =
      std::abs(pos_error.to_nsecs()) >= kLogClockAdjustmentPositionErrorThreshold.to_nsecs();
  bool big_rate_change = last_rate_ppm.has_value() && std::abs(*last_rate_ppm - next_rate_ppm) >=
                                                          kLogClockAdjustmentRateChangeThresholdPpm;
  // ...but don't force this if "high error but no rate change", since this indicates that a clock
  // is pegged to the max/min rate (and will continue to be until the PID's "I" term catches up).
  //
  // Override this (for max transparency but verbose logging) by setting kLogClockRateUnchanged.
  bool rate_changed =
      (last_rate_ppm.has_value() && *last_rate_ppm != next_rate_ppm) || kLogClockRateUnchanged;

  if (big_rate_change || (big_error && rate_changed)) {
    log_count.store(0);
  }

  if (log_count.fetch_add(1) % kLogClockAdjustmentStride == 0) {
    std::stringstream os;
    os << (&clock) << " " << clock.name();
    if (!last_rate_ppm) {
      os << " set to (ppm)               " << std::setw(5) << next_rate_ppm;
    } else if (next_rate_ppm != *last_rate_ppm) {
      os << " change from (ppm) " << std::setw(5) << *last_rate_ppm << " to " << std::setw(5)
         << next_rate_ppm;
    } else {
      os << " adjust_ppm remains (ppm)   " << std::setw(5) << *last_rate_ppm;
    }
    if constexpr (kLogClockAdjustmentWithPidCoefficients) {
      os << "; PID " << pid;
    }
    os << "; src_pos_err " << pos_error.to_nsecs() << " ns";
    FX_LOGS(INFO) << os.str();
  }
}

}  // namespace media_audio
