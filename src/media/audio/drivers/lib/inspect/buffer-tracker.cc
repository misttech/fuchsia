// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be found in the LICENSE file.

#include "src/media/audio/drivers/lib/inspect/buffer-tracker.h"

#include <lib/driver/logging/cpp/logger.h>

#include "src/media/audio/drivers/lib/inspect/recorder.h"

namespace audio {

BufferTracker::BufferTracker(inspect::Node node, std::optional<uint32_t> max_buffer_count,
                             std::optional<zx::duration> per_buffer_duration)
    : node_(std::move(node)),

      per_buffer_duration_(per_buffer_duration),
      max_buffer_count_(max_buffer_count) {
  avg_processing_time_us_ = node_.CreateLazyValues(
      kProcessingTimeAvgUsec, [this]() -> fpromise::promise<inspect::Inspector> {
        std::lock_guard<std::mutex> lock(mutex_);
        inspect::Inspector inspector;
        inspector.GetRoot().CreateUint(kProcessingTimeAvgUsec,
                                       total_buffers_processed_ == 0
                                           ? 0
                                           : total_processing_time_us_ / total_buffers_processed_,
                                       &inspector);
        return fpromise::make_ok_promise(inspector);
      });
  max_processing_time_us_ = node_.CreateUint(kProcessingTimeMaxUsec, 0);
  total_empty_buffer_duration_us_ = node_.CreateUint(kEmptyBufferCumulativeDurationUsec, 0);
  empty_buffer_episode_count_ = node_.CreateUint(kEmptyBufferEpisodeCount, 0);
  max_empty_buffer_duration_us_ = node_.CreateUint(kEmptyBufferDurationMaxUsec, 0);
  avg_outstanding_buffer_count_ = node_.CreateLazyValues(
      kCountOutstandingBuffersAvg, [this]() -> fpromise::promise<inspect::Inspector> {
        std::lock_guard<std::mutex> lock(mutex_);
        inspect::Inspector inspector;
        inspector.GetRoot().CreateUint(
            kCountOutstandingBuffersAvg,
            total_buffers_processed_ == 0
                ? 0
                : cumulative_outstanding_buffer_count_ / total_buffers_processed_,
            &inspector);
        return fpromise::make_ok_promise(inspector);
      });
  total_buffers_processed_count_ = node_.CreateUint(kCountBuffersProcessed, 0);
  if (max_buffer_count.has_value()) {
    total_full_buffer_duration_us_ = node_.CreateUint(kFullBufferCumulativeDurationUsec, 0);
    full_buffer_episode_count_ = node_.CreateUint(kFullBufferEpisodeCount, 0);
    max_full_buffer_duration_us_ = node_.CreateUint(kFullBufferMaxDurationUsec, 0);
  }
  if (per_buffer_duration_.has_value()) {
    total_buffers_processed_duration_us_ = node_.CreateLazyValues(
        kProcessingTimeCumulativeUsec, [this]() -> fpromise::promise<inspect::Inspector> {
          std::lock_guard<std::mutex> lock(mutex_);
          inspect::Inspector inspector;
          inspector.GetRoot().CreateUint(
              kProcessingTimeCumulativeUsec,
              total_buffers_processed_ * per_buffer_duration_->to_usecs(), &inspector);
          return fpromise::make_ok_promise(inspector);
        });
  }
}

void BufferTracker::RecordSubmission() {
  std::lock_guard<std::mutex> lock(mutex_);
  if (submission_times_.size() >= max_buffer_count_.value_or(UINT_MAX)) {
    FDF_LOG(ERROR, "Submission count (%lu) is more than or equal to max buffer count (%d).",
            submission_times_.size(), max_buffer_count_.value_or(UINT_MAX));
    return;
  }
  auto submission_time = zx::clock::get_monotonic();
  if (submission_times_.empty()) {
    if (empty_buffer_start_time_.get() != 0) {
      const zx::duration duration = submission_time - empty_buffer_start_time_;
      total_empty_buffer_duration_us_.Add(duration.to_usecs());
      empty_buffer_episode_count_.Add(1);
      if (duration > max_empty_buffer_duration_) {
        max_empty_buffer_duration_ = duration;
        max_empty_buffer_duration_us_.Set(duration.to_usecs());
      }
      empty_buffer_start_time_ = zx::time(0);
    }
  }
  submission_times_.push(submission_time);
  if (max_buffer_count_.has_value() && submission_times_.size() == max_buffer_count_.value()) {
    if (full_buffer_start_time_.get() == 0) {
      full_buffer_start_time_ = submission_time;
    }
  }
}

void BufferTracker::RecordCompletion() {
  std::lock_guard<std::mutex> lock(mutex_);
  auto completion_time = zx::clock::get_monotonic();
  if (submission_times_.empty()) {
    FDF_LOG(ERROR, "No buffers submitted to this tracker yet.");
    return;
  }
  if (max_buffer_count_.has_value() && submission_times_.size() == max_buffer_count_.value()) {
    if (full_buffer_start_time_.get() != 0) {
      const zx::duration duration = completion_time - full_buffer_start_time_;
      total_full_buffer_duration_us_->Add(duration.to_usecs());
      full_buffer_episode_count_->Add(1);
      if (duration > max_full_buffer_duration_) {
        max_full_buffer_duration_ = duration;
        max_full_buffer_duration_us_->Set(duration.to_usecs());
      }
      full_buffer_start_time_ = zx::time(0);
    }
  }
  cumulative_outstanding_buffer_count_ += submission_times_.size();
  zx::time submission_time = submission_times_.front();
  submission_times_.pop();
  if (submission_times_.empty()) {
    if (empty_buffer_start_time_.get() == 0) {
      empty_buffer_start_time_ = completion_time;
    }
  }

  zx::duration processing_time = completion_time - submission_time;
  total_processing_time_us_ += processing_time.to_usecs();
  total_buffers_processed_++;
  total_buffers_processed_count_.Set(total_buffers_processed_);

  if (processing_time > max_processing_time_) {
    max_processing_time_ = processing_time;
    max_processing_time_us_.Set(max_processing_time_.to_usecs());
  }
}

}  // namespace audio
