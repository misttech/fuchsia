// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_POWER_STATE_RECORDER_CPP_ENUM_STATE_RECORDER_H_
#define LIB_POWER_STATE_RECORDER_CPP_ENUM_STATE_RECORDER_H_

#include <lib/inspect/cpp/bounded_list_node.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/trace-engine/types.h>
#include <lib/trace/event.h>
#include <lib/zx/clock.h>
#include <lib/zx/process.h>
#include <lib/zx/result.h>
#include <zircon/compiler.h>

#include <map>
#include <string>
#include <type_traits>
#include <unordered_map>

#include "lib/power/state_recorder/cpp/manager.h"

namespace power_observability {

// Using an anonymous namespace here results in an unneeded-internal-declaration warning.
namespace internal {

zx_koid_t GetPid() {
  static const zx_koid_t pid = []() {
    zx_info_handle_basic_t info;
    zx_status_t status =
        zx::process::self()->get_info(ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr, nullptr);
    ZX_ASSERT_MSG(status == ZX_OK, "Failed to retrieve PID");
    return info.koid;
  }();
  return pid;
}

}  // namespace internal

// Metadata for an enum state.
template <typename T>
struct EnumStateMetadata {
  static_assert(std::is_enum_v<T>, "T must be an enum type.");
  // Name of the state.
  // - Inspect: This name will be used for this state's Inspect node, recorded in
  //   a node named "power_observability_state_recorders" within the component inspector's root.
  // - Trace: Time series will be recorded on a *global* track with this name. If names
  //   collide, events for the colliding recorders will be placed on the same track.
  std::string name;

  // Mapping of state IDs to state names.
  std::map<T, std::string> states;

  // Category for trace events associated with this state. This must be a string literal due to
  // constraints of the tracing system.
  const char* trace_category_literal;
};

// Records state changes to Inspect and trace.
template <typename T>
class EnumStateRecorder final {
 public:
  static_assert(std::is_enum_v<T>, "T must be an enum type.");

  // Creates a new StateRecorder.
  //
  // Errors:
  //   - ZX_ERR_ALREADY_EXISTS: `metadata.name` is already in use by a StateRecorder exporting
  //     to the provided inspector.
  static zx::result<EnumStateRecorder<T>> Create(EnumStateMetadata<T> metadata, T initial_state,
                                                 size_t capacity, StateRecorderManager& manager);

  void Record(T state_enum);

  EnumStateRecorder(const EnumStateRecorder&) = delete;
  EnumStateRecorder& operator=(const EnumStateRecorder&) = delete;

  EnumStateRecorder& operator=(EnumStateRecorder&& other) noexcept {
    name_ = std::move(other.name_);
    trace_category_literal_ = other.trace_category_literal_;
    state_names_ = std::move(other.state_names_);
    root_node_ = std::move(other.root_node_);
    history_ = std::move(other.history_);
    trace_id_ = other.trace_id_;
    trace_name_ = std::move(other.trace_name_);
    trace_name_ref_ = other.trace_name_ref_;
    manager_ = other.manager_;

    other.moved_from_ = true;

    return *this;
  }

  EnumStateRecorder(EnumStateRecorder&& other) noexcept
      : name_(std::move(other.name_)),
        trace_category_literal_(other.trace_category_literal_),
        state_names_(std::move(other.state_names_)),
        root_node_(std::move(other.root_node_)),
        history_(std::move(other.history_)),
        trace_id_(other.trace_id_),
        trace_name_(std::move(other.trace_name_)),
        trace_name_ref_(other.trace_name_ref_),
        manager_(other.manager_) {
    other.moved_from_ = true;
  }

  ~EnumStateRecorder() {
    if (!moved_from_) {
      manager_->UnregisterName(name_);
    }
  }

 protected:
  EnumStateRecorder(EnumStateMetadata<T> metadata, T initial_state, size_t capacity,
                    StateRecorderManager& manager, inspect::Node root_node)
      : name_(metadata.name),
        trace_category_literal_(metadata.trace_category_literal),
        root_node_(std::move(root_node)),
        history_(root_node_.CreateChild("history"), capacity),
        trace_id_(TRACE_NONCE()),
        trace_name_(std::make_unique<std::string>(
            std::format("{} {} {}", name_, internal::GetPid(), trace_id_))),
        trace_name_ref_(trace_make_inline_string_ref(trace_name_->c_str(), trace_name_->length())),
        manager_(&manager) {
    root_node_.RecordChild("metadata", [&](inspect::Node& metadata_node) {
      metadata_node.RecordString("name", metadata.name);
      metadata_node.RecordString("type", "enum");
      metadata_node.RecordChild("states", [&](inspect::Node& states_node) {
        for (const auto& [state_enum, state_name] : metadata.states) {
          states_node.RecordUint(state_name, static_cast<std::underlying_type_t<T>>(state_enum));
        }
      });
    });

    for (const auto& [state_enum, state_name] : metadata.states) {
      state_names_.emplace(state_enum, state_name);
    }

    Record(initial_state);
  }

 private:
  // Stores a state name as both a std::string (for Inspect) and trace_string_ref_t (for trace);
  struct StateName {
    // Use a unique_ptr to guarantee address stability, so the ref in `trace_name` remains valid on
    // move.
    std::unique_ptr<std::string> inspect_name;
    trace_string_ref_t trace_name;

    explicit StateName(std::string name)
        : inspect_name(std::make_unique<std::string>(name)),
          trace_name(trace_make_inline_string_ref(inspect_name->c_str(), inspect_name->length())) {}
  };

  const StateName* GetStateName(T state_enum) const;

  std::string name_;
  const char* trace_category_literal_;

  std::unordered_map<T, StateName> state_names_;

  inspect::Node root_node_;
  inspect::BoundedListNode history_;

  trace_async_id_t trace_id_;

  // Store the name string in a unique_ptr to guarantee address stability, so the ref in
  // `trace_name_ref_` remains valid on move.
  //
  // Note that `trace_name_` appends to `name` to ensure uniqueness. As a result, the StateName
  // struct doesn't quite match this use case.
  std::unique_ptr<std::string> trace_name_;
  trace_string_ref_t trace_name_ref_;

  std::optional<const StateName*> current_state_name_;

  // Store a reference to the manager so we can unregister our name on destruction.
  StateRecorderManager* manager_;
  bool moved_from_ = false;
};

template <typename T>
zx::result<EnumStateRecorder<T>> EnumStateRecorder<T>::Create(EnumStateMetadata<T> metadata,
                                                              T initial_state, size_t capacity,
                                                              StateRecorderManager& manager) {
  auto result = manager.RegisterName(metadata.name);
  if (!result.is_ok()) {
    return result.take_error();
  }

  return zx::ok(
      EnumStateRecorder<T>(metadata, initial_state, capacity, manager, std::move(result.value())));
}

template <typename T>
const typename EnumStateRecorder<T>::StateName* EnumStateRecorder<T>::GetStateName(
    T state_enum) const {
  static const StateName UNKNOWN_STATE_NAME = StateName("<Unknown>");

  auto it = state_names_.find(state_enum);
  if (it != state_names_.end()) {
    return &it->second;
  }
  return &UNKNOWN_STATE_NAME;
}

template <typename T>
void EnumStateRecorder<T>::Record(T state_enum) {
  // Since our event names are not literals, we're using the trace function API rather than the
  // more common macro API.
  static trace_site_t trace_site_state;
  trace_string_ref_t trace_category_ref;
  trace_context_t* trace_context = trace_acquire_context_for_category_cached(
      trace_category_literal_, &trace_site_state, &trace_category_ref);
  trace_thread_ref_t thread_ref;

  if (unlikely(trace_context)) {
    trace_context_register_current_thread(trace_context, &thread_ref);
    if (current_state_name_.has_value()) {
      trace_context_write_async_end_event_record(
          trace_context, zx_ticks_get_boot(), &thread_ref, &trace_category_ref,
          &(current_state_name_.value()->trace_name), trace_id_, nullptr, 0);
    }
  }

  current_state_name_ = GetStateName(state_enum);

  if (unlikely(trace_context)) {
    // The instant is emitted before the duration event to establish the name of the track.
    trace_context_write_async_instant_event_record(trace_context, zx_ticks_get_boot(), &thread_ref,
                                                   &trace_category_ref, &trace_name_ref_, trace_id_,
                                                   nullptr, 0);

    trace_context_write_async_begin_event_record(
        trace_context, zx_ticks_get_boot(), &thread_ref, &trace_category_ref,
        &(current_state_name_.value()->trace_name), trace_id_, nullptr, 0);
    trace_release_context(trace_context);
  }

  auto timestamp = zx::clock::get_boot().get();
  history_.CreateEntry([&](inspect::Node& node) {
    node.RecordInt("@time", timestamp);
    node.RecordString("value", *current_state_name_.value()->inspect_name);
  });
}

}  // namespace power_observability

#endif  // LIB_POWER_STATE_RECORDER_CPP_ENUM_STATE_RECORDER_H_
