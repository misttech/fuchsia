// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be found in the LICENSE file.

#include "src/media/audio/drivers/lib/inspect/recorder.h"

#include <lib/inspect/cpp/vmo/types.h>
#include <lib/zx/clock.h>
#include <lib/zx/time.h>

namespace audio {

PowerTransition::PowerTransition(inspect::Node node, bool state, const zx::time& called_at,
                                 const zx::time& completed_at)
    : node_(std::move(node)) {
  state_ = node_.CreateBool(kPowerState, state);
  called_at_ = node_.CreateInt(kCalledAt, called_at.get());
  completed_at_ = node_.CreateInt(kEffectiveAt, completed_at.get());
}

DaiEntry::DaiEntry(inspect::Node node, uint64_t element_id) : node_(std::move(node)) {
  element_id_ = node_.CreateUint(kElementId, element_id);
}

ActiveChannelsCall::ActiveChannelsCall(inspect::Node node, uint64_t channel_mask,
                                       const zx::time& called_at, const zx::time& completed_at)
    : node_(std::move(node)) {
  channel_mask_ = node_.CreateUint(kChannelBitmask, channel_mask);
  called_at_ = node_.CreateInt(kCalledAt, called_at.get());
  completed_at_ = node_.CreateInt(kEffectiveAt, completed_at.get());
}

void TaskRecords::UpdateInt(inspect::IntProperty* property, inspect::Node* node,
                            std::string_view name, const int64_t value) {
  ZX_ASSERT_MSG(property != nullptr, "Invalid task node");
  if (*property) {
    property->Set(value);
  } else {
    ZX_ASSERT_MSG(node != nullptr, "Invalid task node");
    *property = node->CreateInt(name, value);
  }
}

void TaskRecords::RecordTaskMetrics(const Subtask::Metrics& metrics,
                                    std::optional<zx::duration> start_to_start,
                                    std::optional<zx::duration> end_to_end,
                                    std::optional<zx::duration> scheduling_delay) {
  if (task_times_entries_.size() == 0) {
    return;
  }
  auto& task_times = task_times_entries_[next_entry_index_];
  next_entry_index_ = (next_entry_index_ + 1) % max_entry_count_;

  if (!task_times.node || metrics.name != std::string_view(task_times.name)) {
    task_times = {};  // reset all inspect objects.
    task_times.node = node_.CreateChild(metrics.name);
    task_times.name = metrics.name;
  }

  if (start_to_start.has_value()) {
    UpdateInt(&task_times.start_to_start_us, &task_times.node, "start_to_start_us",
              start_to_start->to_usecs());
  }
  if (end_to_end.has_value()) {
    UpdateInt(&task_times.end_to_end_us, &task_times.node, "end_to_end_us", end_to_end->to_usecs());
  }
  if (scheduling_delay.has_value()) {
    UpdateInt(&task_times.scheduling_delay_us, &task_times.node, "scheduling_delay_us",
              scheduling_delay->to_usecs());
  }
  UpdateInt(&task_times.wall_time_us, &task_times.node, "wall_time_us",
            metrics.wall_time.to_usecs());

  if (metrics.got_detailed_thread_metrics) {
    UpdateInt(&task_times.cpu_time_us, &task_times.node, "cpu_time_us",
              metrics.cpu_time.to_usecs());
    UpdateInt(&task_times.queue_time_us, &task_times.node, "queue_time_us",
              metrics.queue_time.to_usecs());
    UpdateInt(&task_times.page_fault_time_us, &task_times.node, "page_fault_time_us",
              metrics.page_fault_time.to_usecs());
    UpdateInt(&task_times.kernel_lock_contention_time_us, &task_times.node,
              "kernel_lock_contention_time_us", metrics.kernel_lock_contention_time.to_usecs());
  }
}

AggregateRecords::AggregateRecords(inspect::Node& node)
    : min_task_records_(node.CreateChild("min_task_records"), 1),
      max_task_records_(node.CreateChild("max_task_records"), 1),
      sum_task_records_(node.CreateChild("sum_task_records"), 1),
      avg_task_records_(node.CreateChild("avg_task_records"), 1) {
  worst_underrun_frames_property_ = node.CreateUint("worst_underrun_frames", 0);
  worst_overrun_frames_property_ = node.CreateUint("worst_overrun_frames", 0);
  task_count_ = node.CreateUint("task_count", 0);
  task_underrun_count_ = node.CreateUint("task_underrun_count", 0);
  task_overrun_count_ = node.CreateUint("task_overrun_count", 0);
  dropped_transfer_count_ = node.CreateUint("dropped_transfer_count", 0);

  min_metrics_ = Subtask::Metrics{"min_metrics"};
  min_metrics_.wall_time = zx::duration::infinite();
  min_metrics_.cpu_time = zx::duration::infinite();
  min_metrics_.queue_time = zx::duration::infinite();
  min_metrics_.page_fault_time = zx::duration::infinite();
  min_metrics_.kernel_lock_contention_time = zx::duration::infinite();
  max_metrics_ = Subtask::Metrics{"max_metrics"};
  max_metrics_.wall_time = zx::duration::infinite_past();
  max_metrics_.cpu_time = zx::duration::infinite_past();
  max_metrics_.queue_time = zx::duration::infinite_past();
  max_metrics_.page_fault_time = zx::duration::infinite_past();
  max_metrics_.kernel_lock_contention_time = zx::duration::infinite_past();
  sum_metrics_ = Subtask::Metrics{"sum_metrics"};
  avg_metrics_ = Subtask::Metrics{"avg_metrics"};
}

void AggregateRecords::RecordTaskMetrics(const Subtask::Metrics& metrics,
                                         std::optional<zx::duration> start_to_start,
                                         std::optional<zx::duration> end_to_end) {
  total_task_count_++;
  task_count_.Set(total_task_count_);

  // Get the min and max values for every field, including start-to-start and end-to-end deltas.
  // If `metrics.got_detailed_thread_metrics` is false, we only get basic wall-clock durations.
  min_metrics_.wall_time = std::min(min_metrics_.wall_time, metrics.wall_time);
  max_metrics_.wall_time = std::max(max_metrics_.wall_time, metrics.wall_time);
  if (metrics.got_detailed_thread_metrics) {
    min_metrics_.cpu_time = std::min(min_metrics_.cpu_time, metrics.cpu_time);
    min_metrics_.queue_time = std::min(min_metrics_.queue_time, metrics.queue_time);
    min_metrics_.page_fault_time = std::min(min_metrics_.page_fault_time, metrics.page_fault_time);
    min_metrics_.kernel_lock_contention_time =
        std::min(min_metrics_.kernel_lock_contention_time, metrics.kernel_lock_contention_time);
    max_metrics_.cpu_time = std::max(max_metrics_.cpu_time, metrics.cpu_time);
    max_metrics_.queue_time = std::max(max_metrics_.queue_time, metrics.queue_time);
    max_metrics_.page_fault_time = std::max(max_metrics_.page_fault_time, metrics.page_fault_time);
    max_metrics_.kernel_lock_contention_time =
        std::max(max_metrics_.kernel_lock_contention_time, metrics.kernel_lock_contention_time);
  }
  sum_metrics_ += metrics;

  zx::duration avg_start_to_start = zx::duration(0);
  zx::duration avg_end_to_end = zx::duration(0);
  zx::duration avg_scheduling_delay = zx::duration(0);
  bool has_scheduling_delay = false;
  if (start_to_start.has_value()) {
    min_start_to_start_ = std::min(min_start_to_start_, start_to_start.value());
    max_start_to_start_ = std::max(max_start_to_start_, start_to_start.value());
    sum_start_to_start_ += start_to_start.value();
    total_start_to_start_count_++;
    avg_start_to_start = sum_start_to_start_ / total_start_to_start_count_;
    if (task_schedule_interval_.has_value()) {
      zx::duration schedule_delay = start_to_start.value() > task_schedule_interval_.value()
                                        ? start_to_start.value() - task_schedule_interval_.value()
                                        : zx::duration(0);
      min_schedule_delay_ = std::min(min_schedule_delay_, schedule_delay);
      max_schedule_delay_ = std::max(max_schedule_delay_, schedule_delay);
      sum_schedule_delay_ += schedule_delay;
      total_scheduling_delay_count_++;
      avg_scheduling_delay = sum_schedule_delay_ / total_scheduling_delay_count_;
      has_scheduling_delay = true;
    }
  }
  if (end_to_end.has_value()) {
    min_end_to_end_ = std::min(min_end_to_end_, end_to_end.value());
    max_end_to_end_ = std::max(max_end_to_end_, end_to_end.value());
    sum_end_to_end_ += end_to_end.value();
    total_end_to_end_count_++;
    avg_end_to_end = sum_end_to_end_ / total_end_to_end_count_;
  }

  min_task_records_.RecordTaskMetrics(
      min_metrics_, min_start_to_start_, min_end_to_end_,
      has_scheduling_delay ? std::make_optional(min_schedule_delay_) : std::nullopt);
  max_task_records_.RecordTaskMetrics(
      max_metrics_, max_start_to_start_, max_end_to_end_,
      has_scheduling_delay ? std::make_optional(max_schedule_delay_) : std::nullopt);
  sum_task_records_.RecordTaskMetrics(
      sum_metrics_, std::nullopt, std::nullopt,
      has_scheduling_delay ? std::make_optional(sum_schedule_delay_) : std::nullopt);

  avg_metrics_.wall_time = sum_metrics_.wall_time / total_task_count_;
  if (sum_metrics_.got_detailed_thread_metrics) {
    total_thread_metrics_count_++;
    avg_metrics_.cpu_time = sum_metrics_.cpu_time / total_thread_metrics_count_;
    avg_metrics_.queue_time = sum_metrics_.queue_time / total_thread_metrics_count_;
    avg_metrics_.page_fault_time = sum_metrics_.page_fault_time / total_thread_metrics_count_;
    avg_metrics_.kernel_lock_contention_time =
        sum_metrics_.kernel_lock_contention_time / total_thread_metrics_count_;
    avg_metrics_.got_detailed_thread_metrics = true;
  }

  avg_task_records_.RecordTaskMetrics(
      avg_metrics_, avg_start_to_start, avg_end_to_end,
      has_scheduling_delay ? std::make_optional(avg_scheduling_delay) : std::nullopt);
}

void AggregateRecords::SetupBufferTracker(inspect::Node& node, const std::string& name,
                                          std::optional<uint32_t> max_buffer_count,
                                          std::optional<zx::duration> per_buffer_duration) {
  buffer_tracker_.emplace(node.CreateChild(name), max_buffer_count, per_buffer_duration);
}

void AggregateRecords::RecordBufferSubmission() {
  if (buffer_tracker_) {
    buffer_tracker_->RecordSubmission();
  }
}
void AggregateRecords::RecordBufferCompletion() {
  if (buffer_tracker_) {
    buffer_tracker_->RecordCompletion();
  }
}

void AggregateRecords::SetTaskScheduleInterval(zx::duration interval) {
  task_schedule_interval_ = interval;
}

RunningInterval::RunningInterval(inspect::Node node, const zx::time& started_at,
                                 size_t startup_task_count, size_t final_task_count)
    : node_(std::move(node)),
      startup_tasks_to_save_(startup_task_count),
      final_tasks_to_save_(final_task_count),
      aggregate_records_(node_) {
  started_at_ = started_at;
  node_.RecordInt(kStartedAtUs, started_at.get() / 1000);

  if (startup_tasks_to_save_) {
    startup_task_records_ =
        TaskRecords(node_.CreateChild("startup_task_records"), startup_task_count);
  }
  if (final_tasks_to_save_) {
    final_task_records_ = TaskRecords(node_.CreateChild("final_task_records"), final_task_count);
  }
}

void RunningInterval::RecordStopTime(const zx::time& stopped_at) {
  stopped_at_ = stopped_at;
  node_.RecordInt(kStoppedAtUs, stopped_at.get() / 1000);
  zx::duration audio_duration = stopped_at_ - started_at_;
  node_.RecordInt(kAudioDuration, audio_duration.to_usecs());
}

void RunningInterval::RecordTaskMetrics(const Subtask::Metrics& metrics,
                                        std::optional<zx::duration> start_to_start,
                                        std::optional<zx::duration> end_to_end) {
  ++record_count_;
  if (record_count_ <= startup_tasks_to_save_ || final_tasks_to_save_ > 0) {
    std::optional<TaskRecords>& task_records =
        record_count_ <= startup_tasks_to_save_ ? startup_task_records_ : final_task_records_;
    if (task_records.has_value()) {
      task_records->RecordTaskMetrics(metrics, start_to_start, end_to_end);
    }
  }
  aggregate_records_.RecordTaskMetrics(metrics, start_to_start, end_to_end);
}

RingBufferRecorder::RingBufferRecorder(RingBufferSpecification* ring_buffer_spec,
                                       inspect::Node node, const zx::time& created_at)
    : ring_buffer_spec_(ring_buffer_spec), instance_node_(std::move(node)) {
  created_at_ = instance_node_.CreateInt(kCtorTime, created_at.get());
  running_intervals_root_ = instance_node_.CreateChild(kRunningIntervals);
}

void RingBufferRecorder::RecordDestructionTime(const zx::time& destroyed_at) {
  destroyed_at_ = instance_node_.CreateInt(kDtorTime, destroyed_at.get());
}

// This captures the current startup_task_save_count_ and final_task_save_count_ for this interval.
void RingBufferRecorder::RecordStartTime(const zx::time& started_at) {
  auto running_interval = std::make_unique<RunningInterval>(
      running_intervals_root_.CreateChild(std::to_string(running_intervals_.size())), started_at,
      startup_task_save_count_, final_task_save_count_);
  if (buffer_tracker_name_.has_value()) {
    running_interval->aggregate_records().SetupBufferTracker(
        running_interval->node(), *buffer_tracker_name_, buffer_tracker_max_count_,
        buffer_tracker_per_buffer_duration_);
  }
  if (task_schedule_interval_.has_value()) {
    running_interval->aggregate_records().SetTaskScheduleInterval(task_schedule_interval_.value());
  }
  running_intervals_.emplace_back(std::move(running_interval));
  prev_start_time_ = std::nullopt;
  prev_wall_time_ = std::nullopt;
}

void RingBufferRecorder::RecordStopTime(const zx::time& stopped_at) {
  // It's pointless for clients to call Stop before Start, but we shouldn't crash if they do.
  if (!running_intervals_.empty()) {
    running_intervals_.back()->RecordStopTime(stopped_at);
  }
}

// Set the values that are captured into a RunningInterval at creation (upon RecordStartTime call).
// Can be called multiple times; does not affect a currently-active RunningInterval.
void RingBufferRecorder::SaveStartupAndFinalTaskMetrics(size_t startup_task_save_count,
                                                        size_t final_task_save_count) {
  startup_task_save_count_ = std::min(startup_task_save_count, kMaxStartupTaskRecords);
  final_task_save_count_ = std::min(final_task_save_count, kMaxFinalTaskRecords);
}

void RingBufferRecorder::RecordTaskMetrics(const Subtask::Metrics& metrics) {
  std::optional<zx::duration> start_to_start, end_to_end;
  if (prev_start_time_.has_value()) {
    start_to_start = metrics.start_time - prev_start_time_.value();
    if (prev_wall_time_.has_value()) {
      end_to_end = start_to_start.value() + metrics.wall_time - prev_wall_time_.value();
    }
  }

  ring_buffer_spec_->aggregate_records().RecordTaskMetrics(metrics, start_to_start, end_to_end);
  if (!running_intervals_.empty()) {
    running_intervals_.back()->RecordTaskMetrics(metrics, start_to_start, end_to_end);
  }

  prev_start_time_ = metrics.start_time;
  prev_wall_time_ = metrics.wall_time;
}

void RingBufferRecorder::RecordTaskUnderrun(int64_t underrun_frames) {
  ring_buffer_spec_->aggregate_records().RecordTaskUnderrun(underrun_frames);
  if (!running_intervals_.empty()) {
    running_intervals_.back()->aggregate_records().RecordTaskUnderrun(underrun_frames);
  }
}

void RingBufferRecorder::RecordTaskOverrun(int64_t overrun_frames) {
  ring_buffer_spec_->aggregate_records().RecordTaskOverrun(overrun_frames);
  if (!running_intervals_.empty()) {
    running_intervals_.back()->aggregate_records().RecordTaskOverrun(overrun_frames);
  }
}

void RingBufferRecorder::RecordDroppedTransfer() {
  ring_buffer_spec_->aggregate_records().RecordDroppedTransfer();
  if (!running_intervals_.empty()) {
    running_intervals_.back()->aggregate_records().RecordDroppedTransfer();
  }
}

void RingBufferRecorder::RecordActiveChannelsCall(uint64_t active_channels_bitmask,
                                                  const zx::time& set_active_channels_called_at,
                                                  const zx::time& active_channels_time_complete) {
  if (!active_channels_calls_root_) {
    active_channels_calls_root_ = instance_node_.CreateChild(kSetActiveChannelsCalls);
  }

  ActiveChannelsCall active_channels_call{
      active_channels_calls_root_.CreateChild(std::to_string(active_channels_calls_.size())),
      active_channels_bitmask, set_active_channels_called_at, active_channels_time_complete};
  active_channels_calls_.emplace_back(std::move(active_channels_call));
}

void RingBufferRecorder::SetupBufferTracker(const std::string& name,
                                            std::optional<uint32_t> max_buffer_count,
                                            std::optional<zx::duration> per_buffer_duration) {
  buffer_tracker_name_ = name;
  buffer_tracker_max_count_ = max_buffer_count;
  buffer_tracker_per_buffer_duration_ = per_buffer_duration;

  ring_buffer_spec_->aggregate_records().SetupBufferTracker(ring_buffer_spec_->node(), name,
                                                            max_buffer_count, per_buffer_duration);
}

void RingBufferRecorder::RecordBufferSubmission() {
  ring_buffer_spec_->aggregate_records().RecordBufferSubmission();
  if (!running_intervals_.empty()) {
    running_intervals_.back()->aggregate_records().RecordBufferSubmission();
  }
}
void RingBufferRecorder::RecordBufferCompletion() {
  ring_buffer_spec_->aggregate_records().RecordBufferCompletion();
  if (!running_intervals_.empty()) {
    running_intervals_.back()->aggregate_records().RecordBufferCompletion();
  }
}

void RingBufferRecorder::SetTaskScheduleInterval(zx::duration interval) {
  task_schedule_interval_ = interval;
  ring_buffer_spec_->aggregate_records().SetTaskScheduleInterval(interval);
}

RingBufferSpecification::RingBufferSpecification(inspect::Node node, uint64_t element_id,
                                                 bool supports_active_channels, bool outgoing)
    : node_(std::move(node)), aggregate_records_(node_) {
  element_id_ = node_.CreateUint(kElementId, element_id);
  supports_active_channels_ = node_.CreateBool(kSupportsActiveChannels, supports_active_channels);
  outgoing_ = node_.CreateBool(kIsOutgoingStream, outgoing);

  ring_buffer_inspect_instances_.reserve(kMaxRingBufferInspectInstances);
}

Recorder::Recorder(inspect::Node& inspect_root) : inspect_root_(inspect_root) {
  ring_buffers_root_node_ = inspect_root_.CreateChild(kRingBuffers);
  dai_root_node_ = inspect_root_.CreateChild(kDAIs);
}

void Recorder::PopulatePowerNodes() {
  current_power_state_ = inspect_root_.CreateBool(kCurrentPowerState, true);
  power_transitions_node_ = inspect_root_.CreateChild(kPowerTransitions);
}

void Recorder::PopulateRingBuffer(const std::string& name, uint64_t element_id,
                                  bool supports_active_channels, bool outgoing) {
  auto ring_buffer_spec_node = ring_buffers_root_node_.CreateChild(name);
  RingBufferSpecification ring_buffer_spec{std::move(ring_buffer_spec_node), element_id,
                                           supports_active_channels, outgoing};
  ring_buffer_specs_.emplace(element_id, std::move(ring_buffer_spec));
}

void Recorder::PopulateDai(const std::string& name, uint64_t element_id) {
  auto dai_node = dai_root_node_.CreateChild(name);
  DaiEntry dai_entry{std::move(dai_node), element_id};
  dai_entries_.emplace_back(std::move(dai_entry));
}

void Recorder::RecordSocPowerUp(const zx::time& called_at, const zx::time& completed_at) {
  if (!current_power_state_) {
    PopulatePowerNodes();
  }

  current_power_state_.Set(true);
  power_transitions_.emplace_back(
      power_transitions_node_.CreateChild(std::to_string(power_transitions_.size())), true,
      called_at, completed_at);
}

void Recorder::RecordSocPowerDown(const zx::time& called_at, const zx::time& completed_at) {
  if (!current_power_state_) {
    PopulatePowerNodes();
  }

  current_power_state_.Set(false);
  power_transitions_.emplace_back(
      power_transitions_node_.CreateChild(std::to_string(power_transitions_.size())), false,
      called_at, completed_at);
}

}  // namespace audio
