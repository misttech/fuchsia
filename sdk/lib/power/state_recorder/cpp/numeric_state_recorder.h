// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_POWER_STATE_RECORDER_CPP_NUMERIC_STATE_RECORDER_H_
#define LIB_POWER_STATE_RECORDER_CPP_NUMERIC_STATE_RECORDER_H_

#include <lib/inspect/component/cpp/component.h>
#include <lib/inspect/cpp/bounded_list_node.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/power/state_recorder/cpp/manager.h>
#include <lib/trace-engine/context.h>
#include <lib/trace-engine/types.h>
#include <lib/trace/event.h>
#include <lib/zx/clock.h>
#include <lib/zx/process.h>
#include <lib/zx/result.h>
#include <zircon/compiler.h>

#include <algorithm>
#include <optional>
#include <string>
#include <type_traits>

namespace power_observability {

// Decimal prefixes that can be used with most Units.
enum class DecimalPrefix {
  Nano,
  Micro,
  Milli,
  Centi,
  Deci,
  Kilo,
  Mega,
  Giga,
};

// Measurement units that can be used with NumericStateRecorder. Construct using the public
// factory functions.
class Units {
 public:
  static Units Amps(std::optional<DecimalPrefix> prefix = std::nullopt) {
    return Units(BaseUnit::Amps, prefix);
  }
  static Units Hertz(std::optional<DecimalPrefix> prefix = std::nullopt) {
    return Units(BaseUnit::Hertz, prefix);
  }
  static Units Joules(std::optional<DecimalPrefix> prefix = std::nullopt) {
    return Units(BaseUnit::Joules, prefix);
  }
  static Units Watts(std::optional<DecimalPrefix> prefix = std::nullopt) {
    return Units(BaseUnit::Watts, prefix);
  }
  static Units Volts(std::optional<DecimalPrefix> prefix = std::nullopt) {
    return Units(BaseUnit::Volts, prefix);
  }
  static Units Celsius(std::optional<DecimalPrefix> prefix = std::nullopt) {
    return Units(BaseUnit::Celsius, prefix);
  }
  static Units Number(std::optional<DecimalPrefix> prefix = std::nullopt) {
    return Units(BaseUnit::Number, prefix);
  }
  static Units Percent() { return Units(BaseUnit::Percent, std::nullopt); }

  std::string ToString() const;

 private:
  enum class BaseUnit {
    Amps,
    Hertz,
    Joules,
    Watts,
    Volts,
    Celsius,
    Number,
    Percent,
  };
  static std::string ToString(BaseUnit base);

  Units(BaseUnit base, std::optional<DecimalPrefix> prefix) : base_(base), prefix_(prefix) {}

  BaseUnit base_;
  std::optional<DecimalPrefix> prefix_;
};

// The concepts below, combined with a few natural types, specify the numeric types that can be
// used with NumericStateRecorder and how they are recorded to trace and Inspect.
//
// | Concept        | Trace type | Inspect type |
// |----------------|------------|--------------|
// | WidensToUint32 | uint32_t   | uint64_t     |
// | uint64_t       | uint64_t   | uint64_t     |
// | WidensToInt32  | int32_t    | int64_t      |
// | int64_t        | int64_t    | int64_t      |
// | WidensToDouble | double     | double       |
template <typename T>
concept WidensToUint32 =
    std::is_same_v<T, uint8_t> || std::is_same_v<T, uint16_t> || std::is_same_v<T, uint32_t>;

template <typename T>
concept WidensToUint64 = WidensToUint32<T> || std::is_same_v<T, uint64_t>;

template <typename T>
concept WidensToInt32 =
    std::is_same_v<T, int8_t> || std::is_same_v<T, int16_t> || std::is_same_v<T, int32_t>;

template <typename T>
concept WidensToInt64 = WidensToInt32<T> || std::is_same_v<T, int64_t>;

template <typename T>
concept WidensToDouble = std::is_same_v<T, float> || std::is_same_v<T, double>;

template <typename T>
concept IsRecordableNumericType = WidensToUint64<T> || WidensToInt64<T> || WidensToDouble<T>;

// Metadata for a numeric state.
template <typename T>
  requires IsRecordableNumericType<T>
struct NumericStateMetadata {
  std::string name;
  Units units;
  std::optional<std::pair<T, T>> range;  // Inclusive range, [min, max]
  const char* trace_category_literal;
};

// Records time series data of a numeric-valued state.
template <typename T>
  requires IsRecordableNumericType<T>
class NumericStateRecorder final {
 public:
  static zx::result<NumericStateRecorder<T>> Create(NumericStateMetadata<T> metadata,
                                                    T initial_state, size_t capacity,
                                                    StateRecorderManager& manager);

  void Record(T value);

  NumericStateRecorder(const NumericStateRecorder&) = delete;
  NumericStateRecorder& operator=(const NumericStateRecorder&) = delete;

  NumericStateRecorder& operator=(NumericStateRecorder&& other) noexcept {
    name_ = std::move(other.name_);
    trace_category_literal_ = other.trace_category_literal_;
    root_node_ = std::move(other.root_node_);
    history_ = std::move(other.history_);
    trace_id_ = other.trace_id_;
    trace_name_ref_ = other.trace_name_ref_;
    manager_ = other.manager_;
    moved_from_ = other.moved_from_;
    other.moved_from_ = true;
    return *this;
  }

  NumericStateRecorder(NumericStateRecorder&& other) noexcept
      : name_(std::move(other.name_)),
        trace_category_literal_(other.trace_category_literal_),
        root_node_(std::move(other.root_node_)),
        history_(std::move(other.history_)),
        trace_id_(other.trace_id_),
        trace_name_ref_(other.trace_name_ref_),
        manager_(other.manager_),
        moved_from_(other.moved_from_) {
    other.moved_from_ = true;
  }

  ~NumericStateRecorder() {
    if (!moved_from_) {
      manager_->UnregisterName(*name_);
    }
  }

 private:
  NumericStateRecorder(NumericStateMetadata<T> metadata, T initial_state, size_t capacity,
                       StateRecorderManager& manager, inspect::Node root_node)
      : name_(std::make_unique<std::string>(metadata.name)),
        trace_category_literal_(metadata.trace_category_literal),
        root_node_(std::move(root_node)),
        history_(root_node_.CreateChild("history"), capacity),
        trace_id_(TRACE_NONCE()),
        trace_name_ref_(trace_make_inline_string_ref(name_->c_str(), name_->length())),
        manager_(&manager) {
    root_node_.RecordChild("metadata", [&](inspect::Node& metadata_node) {
      metadata_node.RecordString("name", *name_);
      metadata_node.RecordString("type", "numeric");
      metadata_node.RecordString("units", metadata.units.ToString());
      if (metadata.range.has_value()) {
        metadata_node.RecordChild("range", [&](inspect::Node& range_node) {
          if constexpr (WidensToUint64<T>) {
            range_node.RecordUint("min_inc", static_cast<uint64_t>(metadata.range->first));
            range_node.RecordUint("max_inc", static_cast<uint64_t>(metadata.range->second));
          } else if constexpr (WidensToInt64<T>) {
            range_node.RecordInt("min_inc", static_cast<int64_t>(metadata.range->first));
            range_node.RecordInt("max_inc", static_cast<int64_t>(metadata.range->second));
          } else if constexpr (WidensToDouble<T>) {
            range_node.RecordDouble("min_inc", static_cast<double>(metadata.range->first));
            range_node.RecordDouble("max_inc", static_cast<double>(metadata.range->second));
          } else {
            static_assert(!IsRecordableNumericType<T>, "Unsupported type");
          }
        });
      }
    });

    Record(initial_state);
  }

  std::unique_ptr<std::string> name_;  // Use unique_ptr for address stability with trace_name_ref_
  const char* trace_category_literal_;
  inspect::Node root_node_;
  inspect::BoundedListNode history_;
  trace_async_id_t trace_id_;
  trace_string_ref_t trace_name_ref_;
  StateRecorderManager* manager_;
  bool moved_from_ = false;
};

template <typename T>
  requires IsRecordableNumericType<T>
zx::result<NumericStateRecorder<T>> NumericStateRecorder<T>::Create(
    NumericStateMetadata<T> metadata, T initial_state, size_t capacity,
    StateRecorderManager& manager) {
  auto result = manager.RegisterName(metadata.name);
  if (!result.is_ok()) {
    return result.take_error();
  }
  return zx::ok(NumericStateRecorder<T>(metadata, initial_state, capacity, manager,
                                        std::move(result.value())));
}

template <typename T>
  requires IsRecordableNumericType<T>
void NumericStateRecorder<T>::Record(T value) {
  auto timestamp = zx::clock::get_boot().get();
  history_.CreateEntry([&](inspect::Node& node) {
    node.RecordInt("@time", timestamp);
    if constexpr (WidensToUint64<T>) {
      node.RecordUint("value", static_cast<uint64_t>(value));
    } else if constexpr (WidensToInt64<T>) {
      node.RecordInt("value", static_cast<int64_t>(value));
    } else if constexpr (WidensToDouble<T>) {
      node.RecordDouble("value", static_cast<double>(value));
    } else {
      static_assert(!IsRecordableNumericType<T>, "Unsupported type");
    }
  });

  static trace_site_t trace_site_state;
  trace_string_ref_t category_ref;
  trace_context_t* context = trace_acquire_context_for_category_cached(
      trace_category_literal_, &trace_site_state, &category_ref);

  if (unlikely(context)) {
    trace_thread_ref_t thread_ref;
    trace_context_register_current_thread(context, &thread_ref);

    trace_arg_t arg;
    if constexpr (WidensToUint32<T>) {
      arg = trace_make_arg(trace_make_inline_c_string_ref("value"),
                           trace_make_uint32_arg_value(static_cast<uint32_t>(value)));
    } else if constexpr (std::is_same_v<T, uint64_t>) {
      arg = trace_make_arg(trace_make_inline_c_string_ref("value"),
                           trace_make_uint64_arg_value(value));
    } else if constexpr (WidensToInt32<T>) {
      arg = trace_make_arg(trace_make_inline_c_string_ref("value"),
                           trace_make_int32_arg_value(static_cast<int32_t>(value)));
    } else if constexpr (std::is_same_v<T, int64_t>) {
      arg = trace_make_arg(trace_make_inline_c_string_ref("value"),
                           trace_make_int64_arg_value(value));
    } else if constexpr (WidensToDouble<T>) {
      arg = trace_make_arg(trace_make_inline_c_string_ref("value"),
                           trace_make_double_arg_value(static_cast<double>(value)));
    } else {
      static_assert(!IsRecordableNumericType<T>, "Unsupported type");
    }

    trace_context_write_counter_event_record(context, zx_ticks_get_boot(), &thread_ref,
                                             &category_ref, &trace_name_ref_, trace_id_, &arg, 1);
    trace_release_context(context);
  }
}

}  // namespace power_observability

#endif  // LIB_POWER_STATE_RECORDER_CPP_NUMERIC_STATE_RECORDER_H_
