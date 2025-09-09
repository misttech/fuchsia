// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be found in the LICENSE file.

#ifndef SRC_MEDIA_AUDIO_DRIVERS_LIB_INSPECT_RECORDER_H_
#define SRC_MEDIA_AUDIO_DRIVERS_LIB_INSPECT_RECORDER_H_

#include <lib/inspect/cpp/vmo/types.h>
#include <lib/zx/clock.h>
#include <lib/zx/time.h>

#include <format>

namespace audio {

//
// Recorder class and subclasses
// The Recorder class is responsible for creating and updating Inspect -- so we don't need to
// implement this functionality in classes dedicated to other functions.
//

// Use StringReferences to save space in the Inspect VMO.
static constexpr std::string_view kCurrentPowerState = "current_power_state";

static constexpr std::string_view kPowerTransitions = "power_transitions";
static constexpr std::string_view kCalledAt = "called_at";
static constexpr std::string_view kEffectiveAt = "effective_at";
static constexpr std::string_view kPowerState = "power_state";

static constexpr std::string_view kRingBuffers = "RingBuffers";
static constexpr std::string_view kElementId = "element_id";
static constexpr std::string_view kSupportsActiveChannels = "supports_active_channels";
static constexpr std::string_view kIsOutgoingStream = "is_outgoing_stream";
static constexpr std::string_view kCtorTime = "ctor_time";
static constexpr std::string_view kDtorTime = "dtor_time";
static constexpr std::string_view kRunningIntervals = "running_intervals";
static constexpr std::string_view kStartedAt = "started_at";
static constexpr std::string_view kStoppedAt = "stopped_at";
static constexpr std::string_view kSetActiveChannelsCalls = "SetActiveChannels_calls";
static constexpr std::string_view kChannelBitmask = "channel_bitmask";

static constexpr std::string_view kDAIs = "DAIs";

// Represents a single power transition.
class PowerTransition {
 public:
  PowerTransition(inspect::Node node, bool state, const zx::time& called_at,
                  const zx::time& completed_at);

 private:
  inspect::Node node_;
  inspect::BoolProperty state_;
  inspect::IntProperty called_at_;
  inspect::IntProperty completed_at_;
};

// Represents the specification (unchanging information) of a Dai element.
class DaiEntry {
 public:
  DaiEntry(inspect::Node node, uint64_t element_id);
  inspect::Node& node() { return node_; }

 private:
  inspect::Node node_;
  inspect::UintProperty element_id_;
};

// Represents a call to SetActiveChannels.
class ActiveChannelsCall {
 public:
  ActiveChannelsCall(inspect::Node node, uint64_t channel_mask, const zx::time& called_at,
                     const zx::time& completed_at);

 private:
  inspect::Node node_;
  inspect::UintProperty channel_mask_;
  inspect::IntProperty called_at_;
  inspect::IntProperty completed_at_;
};

class TaskRecords {
 public:
  enum class Type {
    Startup,
    Final,
    Min,
    Max,
    Sum,
  };

  TaskRecords(inspect::Node node, Type type) : node_(std::move(node)), type_(type) {}

  void RecordTaskRecords(std::string_view name, int64_t start_to_start_us, int64_t end_to_end_us,
                         int64_t wall_time_us, int64_t cpu_time_us, int64_t queue_time_us,
                         int64_t page_fault_time_us, int64_t kernel_lock_contention_time_us) {
    auto& task_times = task_times_entries_.emplace_back();
    task_times.node = node_.CreateChild(name);
    if (type_ != Type::Sum) {
      task_times.start_to_start_us =
          task_times.node.CreateInt("start_to_start_us", start_to_start_us);
      task_times.end_to_end_us = task_times.node.CreateInt("end_to_end_us", end_to_end_us);
    }
    task_times.wall_time_us = task_times.node.CreateInt("wall_time_us", wall_time_us);
    task_times.cpu_time_us = task_times.node.CreateInt("cpu_time_us", cpu_time_us);
    task_times.queue_time_us = task_times.node.CreateInt("queue_time_us", queue_time_us);
    task_times.page_fault_time_us =
        task_times.node.CreateInt("page_fault_time_us", page_fault_time_us);
    task_times.kernel_lock_contention_time_us =
        task_times.node.CreateInt("kernel_lock_contention_time_us", kernel_lock_contention_time_us);
  }

 private:
  struct TaskTimes {
    inspect::Node node;
    inspect::IntProperty start_to_start_us;
    inspect::IntProperty end_to_end_us;
    inspect::IntProperty wall_time_us;
    inspect::IntProperty cpu_time_us;
    inspect::IntProperty queue_time_us;
    inspect::IntProperty page_fault_time_us;
    inspect::IntProperty kernel_lock_contention_time_us;
  };

  inspect::Node node_;
  const Type type_;
  std::vector<TaskTimes> task_times_entries_;
};

// Represents an interval during which a RingBuffer instance is started.
class RunningInterval {
 public:
  RunningInterval(inspect::Node node, const zx::time& started_at);

  void RecordStopTime(const zx::time& stopped_at);

  TaskRecords& CreateTaskRecords(TaskRecords::Type type, std::string_view name) {
    std::optional<TaskRecords>* task_records;
    switch (type) {
      case TaskRecords::Type::Startup:
        task_records = &startup_task_records_;
        break;
      case TaskRecords::Type::Final:
        task_records = &final_task_records_;
        break;
      case TaskRecords::Type::Min:
        task_records = &min_task_records_;
        break;
      case TaskRecords::Type::Max:
        task_records = &max_task_records_;
        break;
      case TaskRecords::Type::Sum:
        task_records = &sum_task_records_;
        break;
    }
    return task_records->emplace(node_.CreateChild(name), type);
  }

 private:
  inspect::Node node_;
  inspect::IntProperty started_at_;
  inspect::IntProperty stopped_at_;
  std::optional<TaskRecords> startup_task_records_;
  std::optional<TaskRecords> final_task_records_;
  std::optional<TaskRecords> min_task_records_;
  std::optional<TaskRecords> max_task_records_;
  std::optional<TaskRecords> sum_task_records_;
};

// One of the primary classes used by an outside class.
// Records info about a ring buffer instance, such as lifetime, start/stop, SetActiveChannels.
class RingBufferRecorder {
 public:
  RingBufferRecorder(inspect::Node node, const zx::time& created_at);

  void RecordDestructionTime(const zx::time& destroyed_at);

  void RecordStartTime(const zx::time& started_at);
  void RecordStopTime(const zx::time& stopped_at);

  TaskRecords* CreateTaskRecords(TaskRecords::Type type, std::string_view name) {
    if (!running_intervals_.empty()) {
      return &running_intervals_.rbegin()->CreateTaskRecords(type, name);
    }
    return nullptr;
  }

  void RecordActiveChannelsCall(uint64_t active_channels_bitmask, const zx::time& called_at,
                                const zx::time& completed_at);

 private:
  inspect::Node instance_node_;
  inspect::IntProperty created_at_;
  inspect::IntProperty destroyed_at_;

  inspect::Node active_channels_calls_root_;
  std::vector<ActiveChannelsCall> active_channels_calls_;

  inspect::Node running_intervals_root_;
  std::vector<RunningInterval> running_intervals_;
};

// Represents the specification (unchanging information) of a RingBuffer element.
class RingBufferSpecification {
 public:
  RingBufferSpecification(inspect::Node node, uint64_t element_id, bool supports_active_channels,
                          bool outgoing);

  RingBufferRecorder& CreateRingBufferInspectInstance(const zx::time& created_at) {
    RingBufferRecorder instance(
        node_.CreateChild(std::format("instance {}", ring_buffer_instance_count_++)), created_at);

    if (ring_buffer_instance_count_ <= kMaxRingBufferInspectInstances) {
      return ring_buffer_inspect_instances_.emplace_back(std::move(instance));
    }

    // Retain (kMaxRingBufferInspectInstances) most recent instances.
    RingBufferRecorder& existing_slot =
        ring_buffer_inspect_instances_[(ring_buffer_instance_count_ - 1) %
                                       kMaxRingBufferInspectInstances];
    existing_slot = std::move(instance);
    return existing_slot;
  }

 private:
  static constexpr size_t kMaxRingBufferInspectInstances = 20;

  inspect::Node node_;
  inspect::UintProperty element_id_;
  inspect::BoolProperty supports_active_channels_;
  inspect::BoolProperty outgoing_;
  std::vector<RingBufferRecorder> ring_buffer_inspect_instances_;
  size_t ring_buffer_instance_count_ = 0;
};

// One of the primary classes used by an outside class.
// Records info about a device, such as lifetime, power transitions, and Dai/RingBuffer elements.
class Recorder final {
 public:
  explicit Recorder(inspect::Node& inspect_root);

  void PopulateRingBuffer(const std::string& name, uint64_t element_id,
                          bool supports_active_channels, bool outgoing);
  void PopulateDai(const std::string& name, uint64_t element_id);

  void RecordSocPowerUp(const zx::time& called_at, const zx::time& completed_at);
  void RecordSocPowerDown(const zx::time& called_at, const zx::time& completed_at);

  RingBufferRecorder& CreateRingBufferInstance(uint64_t element_id, const zx::time& created_at) {
    auto it = ring_buffer_specs_.find(element_id);
    ZX_ASSERT_MSG(it != ring_buffer_specs_.end(), "No ring buffer with id %lu.", element_id);
    RingBufferSpecification& ring_buffer_spec = it->second;

    return ring_buffer_spec.CreateRingBufferInspectInstance(created_at);
  }

 private:
  void PopulatePowerNodes();

  inspect::Node& inspect_root_;
  inspect::BoolProperty current_power_state_;

  inspect::Node power_transitions_node_;
  std::vector<PowerTransition> power_transitions_;

  inspect::Node ring_buffers_root_node_;
  std::unordered_map<uint64_t, RingBufferSpecification> ring_buffer_specs_;

  inspect::Node dai_root_node_;
  std::vector<DaiEntry> dai_entries_;
};

}  // namespace audio

#endif  // SRC_MEDIA_AUDIO_DRIVERS_LIB_INSPECT_RECORDER_H_
