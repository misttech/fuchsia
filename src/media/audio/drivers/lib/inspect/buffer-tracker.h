// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be found in the LICENSE file.

#ifndef SRC_MEDIA_AUDIO_DRIVERS_LIB_INSPECT_BUFFER_TRACKER_H_
#define SRC_MEDIA_AUDIO_DRIVERS_LIB_INSPECT_BUFFER_TRACKER_H_

#include <lib/inspect/cpp/inspect.h>
#include <lib/inspect/cpp/vmo/types.h>
#include <lib/zx/clock.h>
#include <lib/zx/time.h>
#include <zircon/compiler.h>

#include <format>
#include <mutex>
#include <queue>

namespace audio {
// Represents all statistics for a named buffer type.
class BufferTracker {
 public:
  BufferTracker(inspect::Node node, std::optional<uint32_t> max_buffer_count,
                std::optional<zx::duration> per_buffer_duration = std::nullopt);

  void RecordSubmission();
  void RecordCompletion();

 private:
  inspect::Node node_;

  // Processing time metrics.
  inspect::LazyNode avg_processing_time_us_;
  inspect::UintProperty max_processing_time_us_;
  uint64_t total_processing_time_us_ __TA_GUARDED(mutex_) = 0;
  zx::duration max_processing_time_ __TA_GUARDED(mutex_) = zx::duration::infinite_past();

  // Empty buffer metrics.
  inspect::UintProperty total_empty_buffer_duration_us_;
  inspect::UintProperty empty_buffer_episode_count_;
  inspect::UintProperty max_empty_buffer_duration_us_;
  zx::duration max_empty_buffer_duration_ __TA_GUARDED(mutex_) = zx::duration::infinite_past();

  // Full buffer metrics.
  std::optional<inspect::UintProperty> total_full_buffer_duration_us_;
  std::optional<inspect::UintProperty> full_buffer_episode_count_;
  std::optional<inspect::UintProperty> max_full_buffer_duration_us_;
  zx::duration max_full_buffer_duration_ __TA_GUARDED(mutex_) = zx::duration::infinite_past();

  // Outstanding buffer metrics.
  inspect::LazyNode avg_outstanding_buffer_count_;
  inspect::UintProperty total_buffers_processed_count_;
  uint64_t cumulative_outstanding_buffer_count_ __TA_GUARDED(mutex_) = 0;
  uint64_t total_buffers_processed_ __TA_GUARDED(mutex_) = 0;
  std::optional<inspect::LazyNode> total_buffers_processed_duration_us_;
  std::optional<zx::duration> per_buffer_duration_;

  std::queue<zx::time> submission_times_ __TA_GUARDED(mutex_);
  zx::time empty_buffer_start_time_ __TA_GUARDED(mutex_) = zx::time(0);
  zx::time full_buffer_start_time_ __TA_GUARDED(mutex_) = zx::time(0);
  std::optional<uint32_t> max_buffer_count_;
  std::mutex mutex_;
};

}  // namespace audio

#endif  // SRC_MEDIA_AUDIO_DRIVERS_LIB_INSPECT_BUFFER_TRACKER_H_
