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
  avg_processing_time_node_ = node_.CreateLazyValues(
      kProcessingTimeAvgUsec, [this]() -> fpromise::promise<inspect::Inspector> {
        std::lock_guard<std::mutex> lock(mutex_);
        inspect::Inspector inspector;
        if (total_buffers_processed_count_ > 0) {
          inspector.GetRoot().CreateUint(
              kProcessingTimeAvgUsec,
              total_processing_duration_us_ / total_buffers_processed_count_, &inspector);
        }
        return fpromise::make_ok_promise(inspector);
      });
  max_processing_time_us_prop_ = node_.CreateUint(kProcessingTimeMaxUsec, 0);
  empty_buffer_episode_count_prop_ = node_.CreateUint(kEmptyBufferEpisodeCount, 0);
  avg_outstanding_buffer_count_node_ = node_.CreateLazyValues(
      kCountOutstandingBuffersAvg, [this]() -> fpromise::promise<inspect::Inspector> {
        std::lock_guard<std::mutex> lock(mutex_);
        inspect::Inspector inspector;
        if (total_buffers_processed_count_ > 0) {
          inspector.GetRoot().CreateDouble(
              kCountOutstandingBuffersAvg,
              static_cast<double>(cumulative_outstanding_buffer_count_) /
                  static_cast<double>(total_buffers_processed_count_),
              &inspector);
        }
        return fpromise::make_ok_promise(inspector);
      });
  minmax_outstanding_buffer_counts_ = node_.CreateLazyValues(
      kCountOutstandingBuffersMax, [this]() -> fpromise::promise<inspect::Inspector> {
        std::lock_guard<std::mutex> lock(mutex_);
        inspect::Inspector inspector;
        if (started_monitoring_min_max_buffers_) {
          inspector.GetRoot().CreateUint(kCountOutstandingBuffersMin,
                                         outstanding_buffers_count_min_, &inspector);
          inspector.GetRoot().CreateUint(kCountOutstandingBuffersMax,
                                         outstanding_buffers_count_max_, &inspector);
        }
        return fpromise::make_ok_promise(inspector);
      });
  total_buffers_processed_count_prop_ = node_.CreateUint(kCountBuffersProcessed, 0);
  if (max_buffer_count.has_value()) {
    full_buffer_episode_count_prop_ = node_.CreateUint(kFullBufferEpisodeCount, 0);
  }
  if (per_buffer_duration_.has_value()) {
    total_buffers_processed_duration_node_ = node_.CreateLazyValues(
        kProcessingTimeCumulativeUsec, [this]() -> fpromise::promise<inspect::Inspector> {
          std::lock_guard<std::mutex> lock(mutex_);
          inspect::Inspector inspector;
          if (total_buffers_processed_count_ > 0) {
            inspector.GetRoot().CreateUint(
                kProcessingTimeCumulativeUsec,
                total_buffers_processed_count_ * per_buffer_duration_->to_nsecs() / 1000,
                &inspector);
          }
          return fpromise::make_ok_promise(inspector);
        });
  }
}

void BufferTracker::RecordSubmission() {
  std::lock_guard<std::mutex> lock(mutex_);
  if (submission_times_.size() >= max_buffer_count_.value_or(UINT_MAX)) {
    fdf::warn("RecordSubmission: active count ({}) cannot equal/exceed max_buffer_count_ ({})",
              submission_times_.size(), max_buffer_count_.value_or(UINT_MAX));
    return;
  }
  auto submission_time = zx::clock::get_monotonic();
  if (submission_times_.empty()) {
    if (empty_buffer_start_time_.get() != 0) {
      const zx::duration duration = submission_time - empty_buffer_start_time_;
      if (!total_empty_buffer_duration_us_prop_) {
        total_empty_buffer_duration_us_prop_ =
            node_.CreateUint(kEmptyBufferDurationCumulativeUsec, 0);
        max_empty_buffer_duration_us_prop_ = node_.CreateUint(kEmptyBufferDurationMaxUsec, 0);
      }
      total_empty_buffer_duration_us_prop_.Add(duration.to_usecs());
      empty_buffer_episode_count_prop_.Add(1);
      if (duration > max_empty_buffer_duration_) {
        max_empty_buffer_duration_ = duration;
        max_empty_buffer_duration_us_prop_.Set(duration.to_usecs());
      }
      empty_buffer_start_time_ = zx::time(0);
    }
  }
  submission_times_.push(submission_time);
  // Don't track min/max outstanding buffers when initially "priming" or "draining" the pipeline.
  if (currently_monitoring_min_max_buffers_) {
    outstanding_buffers_count_max_ =
        std::max(outstanding_buffers_count_max_, submission_times_.size());
    // Start... or StopMonitoringOutstandingBufferCount could be called at any time.
    // For full accuracy, account for the outstanding buffer count immediately prior to submission.
    outstanding_buffers_count_min_ =
        std::min(outstanding_buffers_count_min_, submission_times_.size() - 1);
  }

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
    fdf::warn("RecordCompletion: no active buffers to complete.");
    return;
  }
  if (max_buffer_count_.has_value() && submission_times_.size() == max_buffer_count_.value()) {
    if (full_buffer_start_time_.get() != 0) {
      const zx::duration duration = completion_time - full_buffer_start_time_;
      if (!total_full_buffer_duration_us_prop_) {
        total_full_buffer_duration_us_prop_ =
            node_.CreateUint(kFullBufferDurationCumulativeUsec, 0);
        max_full_buffer_duration_us_prop_ = node_.CreateUint(kFullBufferDurationMaxUsec, 0);
      }

      total_full_buffer_duration_us_prop_->Add(duration.to_usecs());
      full_buffer_episode_count_prop_->Add(1);
      if (duration > max_full_buffer_duration_) {
        max_full_buffer_duration_ = duration;
        max_full_buffer_duration_us_prop_->Set(duration.to_usecs());
      }
      full_buffer_start_time_ = zx::time(0);
    }
  }
  cumulative_outstanding_buffer_count_ += submission_times_.size();
  zx::time submission_time = submission_times_.front();
  submission_times_.pop();

  // Don't track min/max outstanding buffers when "draining" the pipeline.
  if (currently_monitoring_min_max_buffers_) {
    outstanding_buffers_count_min_ =
        std::min(outstanding_buffers_count_min_, submission_times_.size());
    // Start... or StopMonitoringOutstandingBufferCount could be called at any time.
    // For full accuracy, account for the outstanding buffer count immediately prior to completion.
    outstanding_buffers_count_max_ =
        std::max(outstanding_buffers_count_max_, submission_times_.size() + 1);
  }

  if (submission_times_.empty()) {
    if (empty_buffer_start_time_.get() == 0) {
      empty_buffer_start_time_ = completion_time;
    }
  }

  total_buffers_processed_count_++;
  total_buffers_processed_count_prop_.Set(total_buffers_processed_count_);
  zx::duration processing_duration = completion_time - submission_time;
  total_processing_duration_us_ += processing_duration.to_usecs();
  if (processing_duration > max_processing_duration_) {
    max_processing_duration_ = processing_duration;
    max_processing_time_us_prop_.Set(max_processing_duration_.to_usecs());
  }
}

}  // namespace audio
