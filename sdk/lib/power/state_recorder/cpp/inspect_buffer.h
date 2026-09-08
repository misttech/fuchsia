// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_POWER_STATE_RECORDER_CPP_INSPECT_BUFFER_H_
#define LIB_POWER_STATE_RECORDER_CPP_INSPECT_BUFFER_H_

#include <lib/fit/function.h>
#include <lib/inspect/component/cpp/component.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/power/state_recorder/cpp/concepts.h>
#include <lib/zx/clock.h>
#include <lib/zx/time.h>
#include <zircon/syscalls.h>

#include <cstdint>
#include <type_traits>
#include <vector>

namespace power_observability::internal {

inline int64_t to_msecs(zx::time_boot timestamp) {
  return zx::duration(timestamp.to_timespec()).to_msecs();
}

// The logical data points stored in a TimestampedBuffer.
template <typename ValueType>
struct DataPoint {
  int64_t timestamp_ns;
  ValueType value;
};

// Simple wrapper around std::vector<ValueType> for use with TimestampedBuffer.
template <typename ValueType>
class ValueBuffer {
 public:
  explicit ValueBuffer(size_t size) : buffer_(size) {}
  ValueType Get(size_t index) const { return buffer_[index]; }
  void Set(size_t index, ValueType value) { buffer_[index] = value; }
  void Reset() { std::ranges::fill(buffer_, static_cast<ValueType>(0)); }

 private:
  std::vector<ValueType> buffer_;
};

// Stores bits in an underlying byte array, for use with TimestampedBuffer<bool>.
class BitBuffer {
 public:
  explicit BitBuffer(size_t size) : buffer_((size + 7) / 8) {}
  bool Get(size_t index) const {
    size_t byte_idx = index >> 3;
    size_t bit_idx = index & 0x7;
    return (buffer_[byte_idx] >> bit_idx) & 1;
  }
  void Set(size_t index, bool value) {
    size_t byte_idx = index >> 3;
    size_t bit_idx = index & 0x7;
    if (value) {
      buffer_[byte_idx] |= (1 << bit_idx);
    } else {
      buffer_[byte_idx] &= ~(1 << bit_idx);
    }
  }
  void Reset() { std::ranges::fill(buffer_, 0); }

 private:
  std::vector<uint8_t> buffer_;
};

// Define is_bool_enum_v such that std::underlying_type_t is neer evaluated for a non-enum type.
template <typename T>
inline constexpr bool is_bool_enum_v = false;

template <typename T>
  requires std::is_enum_v<T>
inline constexpr bool is_bool_enum_v<T> = std::is_same_v<std::underlying_type_t<T>, bool>;

// Used to track buffer resets due to timestamp delta over/underflow.
struct ResetInfo {
  size_t count;
  int64_t last_reset_ns;
};

// Manages circular buffers for timestamp deltas and associated data.
template <typename ValueType>
  requires IsRecordableValueType<ValueType>
class TimestampedBuffer {
 public:
  // If ValueType is bool or an enum type with bool underlying type, use BitBuffer. Otherwise, use
  // ValueBuffer<ValueType>.
  using BufferType =
      std::conditional_t<std::is_same_v<ValueType, bool> || is_bool_enum_v<ValueType>, BitBuffer,
                         ValueBuffer<ValueType>>;

  explicit TimestampedBuffer(size_t size)
      : delta_ms_buffer_(size), data_buffer_(BufferType(size)), buffer_size_(size) {
    reset_info_.last_reset_ns = zx::msec(to_msecs(zx::clock::get_boot())).get();
  }

  // Delete move and copy constructors until we have a good reason to use them.
  TimestampedBuffer(TimestampedBuffer&& other) = delete;
  TimestampedBuffer& operator=(TimestampedBuffer&& other) = delete;
  TimestampedBuffer(const TimestampedBuffer& other) = delete;
  TimestampedBuffer& operator=(const TimestampedBuffer& other) = delete;

  // Adds an entry to the buffer, at millisecond timestamp `current_ms`.
  //
  // If the millisecond delta between the new timestamp and the most recent timestamp cannot be
  // represented as an int32_t (approximately 24.9 days), the buffer will be reset, retaining only
  // the supplied entry.
  void AddEntry(ValueType data, int64_t current_ms) {
    int32_t delta = 0;
    if (!initialized_) {
      // Initialize the buffer on the first call.
      base_ts_ms_ = current_ms;
      last_ts_ms_ = current_ms;
      initialized_ = true;
    } else {
      int64_t int64_delta = current_ms - last_ts_ms_;
      if (std::numeric_limits<int32_t>::min() <= int64_delta &&
          int64_delta <= std::numeric_limits<int32_t>::max()) {
        delta = static_cast<int32_t>(int64_delta);
      } else {
        // The timestamp delta overflowed or underflowed. Reset the buffer to its initialized state.
        std::ranges::fill(delta_ms_buffer_, 0);
        data_buffer_.Reset();
        base_ts_ms_ = current_ms;
        last_ts_ms_ = current_ms;
        write_idx_ = 0;
        count_ = 0;

        reset_info_.count += 1;
        reset_info_.last_reset_ns = zx::msec(current_ms).to_nsecs();
      }
    }
    // Update base time on wrap-around (before overwriting)
    if (count_ == buffer_size_) {
      base_ts_ms_ += delta_ms_buffer_[write_idx_];
    }
    // Store the new delta and its corresponding data
    delta_ms_buffer_[write_idx_] = delta;
    data_buffer_.Set(write_idx_, data);
    // Update state for the next entry
    last_ts_ms_ = current_ms;
    write_idx_ = (write_idx_ + 1) % buffer_size_;
    if (count_ < buffer_size_) {
      count_++;
    }
  }

  // Reconstructs each DataPoint stored in the buffer and runs `callback` on it.
  void ForEachDataPoint(fit::function<void(const DataPoint<ValueType>&)> callback) const {
    if (!initialized_ || count_ == 0) {
      return;
    }

    zx::duration current_ts = zx::msec(base_ts_ms_);
    size_t read_idx = (count_ == buffer_size_) ? write_idx_ : 0;

    for (size_t i = 0; i < count_; ++i) {
      current_ts += zx::msec(delta_ms_buffer_[read_idx]);
      ValueType value = data_buffer_.Get(read_idx);
      callback({.timestamp_ns = current_ts.to_nsecs(), .value = value});
      read_idx = (read_idx + 1) % buffer_size_;
    }
  }

  const ResetInfo& GetResetInfo() const { return reset_info_; }
  size_t GetCount() const { return count_; }
  size_t GetCapacity() const { return buffer_size_; }

 private:
  std::vector<int32_t> delta_ms_buffer_;
  BufferType data_buffer_;
  size_t buffer_size_;
  int64_t base_ts_ms_ = 0;
  int64_t last_ts_ms_ = 0;
  size_t write_idx_ = 0;
  size_t count_ = 0;
  ResetInfo reset_info_ = {.count = 0, .last_reset_ns = 0};
  bool initialized_ = false;
};

inline constexpr size_t kShardCapacity = 200;

template <typename T>
struct InspectArrayTypeTraits;

template <typename T>
  requires WidensToUint64<T>
struct InspectArrayTypeTraits<T> {
  using ArrayProperty = inspect::UintArray;
  using ValueType = uint64_t;
  static ArrayProperty Create(inspect::Node& node, std::string_view name, size_t slots) {
    return node.CreateUintArray(name, slots);
  }
};

template <typename T>
  requires WidensToInt64<T>
struct InspectArrayTypeTraits<T> {
  using ArrayProperty = inspect::IntArray;
  using ValueType = int64_t;
  static ArrayProperty Create(inspect::Node& node, std::string_view name, size_t slots) {
    return node.CreateIntArray(name, slots);
  }
};

template <typename T>
  requires WidensToDouble<T>
struct InspectArrayTypeTraits<T> {
  using ArrayProperty = inspect::DoubleArray;
  using ValueType = double;
  static ArrayProperty Create(inspect::Node& node, std::string_view name, size_t slots) {
    return node.CreateDoubleArray(name, slots);
  }
};

template <typename T>
  requires std::is_enum_v<T>
struct InspectArrayTypeTraits<T> : InspectArrayTypeTraits<std::underlying_type_t<T>> {};

template <typename T>
struct InspectShard {
  inspect::Node node;
  inspect::IntArray times;
  typename InspectArrayTypeTraits<T>::ArrayProperty values;
};

// Fixed-capacity sharded circular buffer in Inspect for eager mode.
template <typename T>
  requires IsRecordableValueType<T>
class EagerShardedBuffer {
 public:
  EagerShardedBuffer(inspect::Node& parent_node, size_t capacity)
      : capacity_(capacity),
        history_node_(parent_node.CreateChild("history")),
        current_index_(history_node_.CreateUint("current_index", 0)),
        current_size_(history_node_.CreateUint("current_size", 0)),
        shards_node_(history_node_.CreateChild("shards")) {
    size_t active_capacity = std::max<size_t>(capacity, 1);
    size_t total_shards = (active_capacity + kShardCapacity - 1) / kShardCapacity;
    shards_.reserve(total_shards);

    for (size_t s = 0; s < total_shards; ++s) {
      size_t start_idx = s * kShardCapacity;
      size_t end_idx = std::min(start_idx + kShardCapacity, capacity);
      size_t shard_size = std::max<size_t>(end_idx - start_idx, 1);

      auto shard_node = shards_node_.CreateChild(std::to_string(s));
      auto times = shard_node.CreateIntArray("times", shard_size);
      auto values = InspectArrayTypeTraits<T>::Create(shard_node, "values", shard_size);

      shards_.push_back(InspectShard<T>{
          .node = std::move(shard_node),
          .times = std::move(times),
          .values = std::move(values),
      });
    }
  }

  EagerShardedBuffer(EagerShardedBuffer&&) = default;
  EagerShardedBuffer& operator=(EagerShardedBuffer&&) = default;
  EagerShardedBuffer(const EagerShardedBuffer&) = delete;
  EagerShardedBuffer& operator=(const EagerShardedBuffer&) = delete;

  void Record(int64_t timestamp_ns, T value) {
    if (capacity_ == 0) {
      return;
    }
    size_t shard_idx = index_tracker_ / kShardCapacity;
    size_t slot_idx = index_tracker_ % kShardCapacity;

    auto& shard = shards_[shard_idx];
    shard.times.Set(slot_idx, timestamp_ns);
    using ValueType = typename InspectArrayTypeTraits<T>::ValueType;
    shard.values.Set(slot_idx, static_cast<ValueType>(value));

    index_tracker_ = (index_tracker_ + 1) % capacity_;
    size_tracker_ = std::min(size_tracker_ + 1, capacity_);

    current_index_.Set(index_tracker_);
    current_size_.Set(size_tracker_);
  }

 private:
  size_t capacity_;
  inspect::Node history_node_;
  inspect::UintProperty current_index_;
  inspect::UintProperty current_size_;
  inspect::Node shards_node_;
  std::vector<InspectShard<T>> shards_;
  size_t index_tracker_ = 0;
  size_t size_tracker_ = 0;
};

// Records data in an underlying TimestampedBuffer to a lazy node in sharded circular buffer format.
template <typename T>
  requires IsRecordableValueType<T>
class LazyInspectRecorderBase {
 public:
  // This class cannot be safely moved because the lazy node callback references `this`.
  LazyInspectRecorderBase(LazyInspectRecorderBase&& other) = delete;
  LazyInspectRecorderBase& operator=(LazyInspectRecorderBase&& other) = delete;

  LazyInspectRecorderBase(const LazyInspectRecorderBase& other) = delete;
  LazyInspectRecorderBase& operator=(const LazyInspectRecorderBase& other) = delete;

  virtual ~LazyInspectRecorderBase() {}

  void AddEntry(T data, int64_t timestamp_ms) { buffer_.AddEntry(data, timestamp_ms); }

 protected:
  LazyInspectRecorderBase(size_t capacity, inspect::Node& parent_node)
      : buffer_(capacity),
        history_node_(parent_node.CreateLazyNode(
            "history", fit::bind_member<&LazyInspectRecorderBase<T>::TimeSeriesToInspect>(this))),
        reset_info_node_(parent_node.CreateLazyNode(
            "reset_info",
            fit::bind_member<&LazyInspectRecorderBase<T>::ResetInfoToInspect>(this))) {}

 private:
  fpromise::promise<inspect::Inspector> ResetInfoToInspect() const {
    inspect::Inspector inspector;
    auto& root = inspector.GetRoot();
    auto& reset_info = buffer_.GetResetInfo();
    root.RecordUint("count", reset_info.count);
    root.RecordInt("last_reset_ns", reset_info.last_reset_ns);
    return fpromise::make_ok_promise(std::move(inspector));
  }

  fpromise::promise<inspect::Inspector> TimeSeriesToInspect() const {
    std::vector<DataPoint<T>> items;
    items.reserve(buffer_.GetCount());
    buffer_.ForEachDataPoint([&](const DataPoint<T>& data_point) { items.push_back(data_point); });

    inspect::Inspector inspector;
    auto& root = inspector.GetRoot();
    size_t size = items.size();

    root.RecordUint("current_index", 0);
    root.RecordUint("current_size", size);

    auto shards_node = root.CreateChild("shards");
    size_t active_capacity = std::max<size_t>(size, 1);
    size_t num_shards = (active_capacity + kShardCapacity - 1) / kShardCapacity;

    for (size_t s = 0; s < num_shards; ++s) {
      size_t shard_start_idx = s * kShardCapacity;
      size_t shard_end_idx = std::min(shard_start_idx + kShardCapacity, size);
      // Since s < num_shards, shard_start_idx <= size. So shard_end_idx >= shard_start_idx.
      size_t shard_size = shard_end_idx - shard_start_idx;
      size_t allocated_size = std::max<size_t>(shard_size, 1);

      auto shard_node = shards_node.CreateChild(std::to_string(s));
      auto times_prop = shard_node.CreateIntArray("times", allocated_size);
      auto values_prop = InspectArrayTypeTraits<T>::Create(shard_node, "values", allocated_size);

      using ValueType = typename InspectArrayTypeTraits<T>::ValueType;
      for (size_t i = 0; i < shard_size; ++i) {
        const auto& item = items[shard_start_idx + i];
        times_prop.Set(i, item.timestamp_ns);
        values_prop.Set(i, static_cast<ValueType>(item.value));
      }

      inspector.emplace(std::move(times_prop));
      inspector.emplace(std::move(values_prop));
      inspector.emplace(std::move(shard_node));
    }

    inspector.emplace(std::move(shards_node));
    return fpromise::make_ok_promise(std::move(inspector));
  }

  TimestampedBuffer<T> buffer_;
  inspect::LazyNode history_node_;
  inspect::LazyNode reset_info_node_;
};

}  // namespace power_observability::internal

#endif  // LIB_POWER_STATE_RECORDER_CPP_INSPECT_BUFFER_H_
