// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/input/pointerinjector_registry.h"

#include <lib/syslog/cpp/macros.h>
#include <lib/trace/event.h>

#include "src/ui/scenic/lib/input/mouse_injector.h"
#include "src/ui/scenic/lib/input/touch_injector.h"
#include "src/ui/scenic/lib/utils/check_is_on_thread.h"
#include "src/ui/scenic/lib/utils/helpers.h"
#include "src/ui/scenic/lib/utils/math.h"

namespace scenic_impl::input {

using fuchsia::ui::pointerinjector::DeviceType;
using fuchsia::ui::pointerinjector::DispatchPolicy;

namespace {

bool IsValidConfig(const fuchsia::ui::pointerinjector::Config& config) {
  if (!config.has_device_id() || !config.has_device_type() || !config.has_context() ||
      !config.has_target() || !config.has_viewport() || !config.has_dispatch_policy()) {
    FX_LOGS(ERROR) << "InjectorRegistry::Register : Argument |config| is incomplete.";
    return false;
  }

  const auto device_type = config.device_type();
  if (device_type != DeviceType::TOUCH && device_type != DeviceType::MOUSE) {
    FX_LOGS(ERROR) << "InjectorRegistry::Register : Unknown DeviceType.";
    return false;
  }

  const auto dispatch_policy = config.dispatch_policy();
  if (device_type == DeviceType::MOUSE) {
    if (dispatch_policy != DispatchPolicy::EXCLUSIVE_TARGET &&
        dispatch_policy != DispatchPolicy::MOUSE_HOVER_AND_LATCH_IN_TARGET) {
      FX_LOGS(ERROR)
          << "InjectorRegistry::Register : DeviceType::MOUSE with mismatched dispatch policy.";
      return false;
    }
  } else if (device_type == DeviceType::TOUCH) {
    if (dispatch_policy != DispatchPolicy::EXCLUSIVE_TARGET &&
        dispatch_policy != DispatchPolicy::TOP_HIT_AND_ANCESTORS_IN_TARGET) {
      FX_LOGS(ERROR)
          << "InjectorRegistry::Register : DeviceType::TOUCH with mismatched dispatch policy.";
      return false;
    }
  } else {
    FX_NOTREACHED();
  }

  if (!config.context().is_view() || !config.target().is_view()) {
    FX_LOGS(ERROR) << "InjectorRegistry::Register : Argument |config.context| or |config.target| "
                      "is not a view. Only views are supported.";
    return false;
  }

  if (Injector::IsValidViewport(config.viewport()) != ZX_OK) {
    // Errors printed in IsValidViewport. Just return result here.
    return false;
  }

  return true;
}

}  // namespace

// LINT.IfChange
PointerinjectorRegistry::PointerinjectorRegistry(
    async_dispatcher_t* input_dispatcher,
    std::shared_ptr<view_tree::SnapshotHolder> snapshot_holder,
    TouchInjectFunc inject_touch_exclusive, TouchInjectFunc inject_touch_hit_tested,
    MouseInjectFunc inject_mouse_exclusive, MouseInjectFunc inject_mouse_hit_tested,
    fit::function<void(StreamId stream_id)> cancel_mouse_stream, inspect::Node inspect_node)
    : inject_touch_exclusive_(std::move(inject_touch_exclusive)),
      inject_touch_hit_tested_(std::move(inject_touch_hit_tested)),
      inject_mouse_exclusive_(std::move(inject_mouse_exclusive)),
      inject_mouse_hit_tested_(std::move(inject_mouse_hit_tested)),
      cancel_mouse_stream_(std::move(cancel_mouse_stream)),
      snapshot_holder_(std::move(snapshot_holder)),
      inspect_node_(std::move(inspect_node)) {}

void PointerinjectorRegistry::Bind(
    fidl::InterfaceRequest<fuchsia::ui::pointerinjector::Registry> request) {
  injector_registry_.AddBinding(this, std::move(request));
}

void PointerinjectorRegistry::Register(
    fuchsia::ui::pointerinjector::Config config,
    fidl::InterfaceRequest<fuchsia::ui::pointerinjector::Device> injector,
    RegisterCallback callback) {
  TRACE_DURATION("input", "PointerinjectorRegistry::Register");

  if (!IsValidConfig(config)) {
    // Errors printed inside IsValidConfig. Just return here.
    injector.Close(ZX_ERR_INVALID_ARGS);
    return;
  }

  // Check connectivity here, since injector doesn't have access to it.
  const zx_koid_t context_koid = utils::ExtractKoid(config.context().view());
  const zx_koid_t target_koid = utils::ExtractKoid(config.target().view());
  if (context_koid == ZX_KOID_INVALID || target_koid == ZX_KOID_INVALID) {
    FX_LOGS(ERROR) << "InjectorRegistry::Register : Argument |config.context| or |config.target| "
                      "was invalid.";
    injector.Close(ZX_ERR_INVALID_ARGS);
    return;
  }
  auto snapshot = snapshot_holder_->GetSnapshot();

  if (!snapshot->IsDescendant(target_koid, context_koid)) {
    FX_LOGS(ERROR) << "InjectorRegistry::Register : Argument |config.context| must be connected to "
                      "the Scene, and |config.target| must be a descendant of |config.context|";
    injector.Close(ZX_ERR_BAD_STATE);
    return;
  }

  const InjectorId id = ++last_injector_id_;
  InjectorSettings settings{.dispatch_policy = config.dispatch_policy(),
                            .device_id = config.device_id(),
                            .device_type = config.device_type(),
                            .context_koid = context_koid,
                            .target_koid = target_koid};
  Viewport viewport{
      .extents = {config.viewport().extents()},
      .context_from_viewport_transform =
          utils::ColumnMajorMat3ArrayToMat4(config.viewport().viewport_to_context_transform()),
  };

  fit::function<void()> on_channel_closed = [this, id] { injectors_.erase(id); };

  if (settings.device_type == fuchsia::ui::pointerinjector::DeviceType::TOUCH) {
    const auto [_, success] = injectors_.emplace(
        id,
        std::make_unique<TouchInjector>(
            snapshot_holder_,
            inspect_node_.CreateChild(inspect_node_.UniqueName("touch-injector-")),
            std::move(settings), std::move(viewport), std::move(injector),
            /*inject=*/
            [&inject_func = settings.dispatch_policy ==
                                    fuchsia::ui::pointerinjector::DispatchPolicy::EXCLUSIVE_TARGET
                                ? inject_touch_exclusive_
                                : inject_touch_hit_tested_](
                InternalTouchEvent event, StreamId stream_id, const view_tree::Snapshot& snapshot) {
              TRACE_DURATION("input", "TouchInjector::inject_");
              inject_func(std::move(event), stream_id, snapshot);
            },
            std::move(on_channel_closed)));
    FX_CHECK(success) << "Injector already exists.";
  } else if (settings.device_type == fuchsia::ui::pointerinjector::DeviceType::MOUSE) {
    if (config.has_buttons()) {
      settings.button_identifiers = config.buttons();
    }
    if (config.has_scroll_v_range()) {
      settings.scroll_v_range = config.scroll_v_range();
    }
    if (config.has_scroll_h_range()) {
      settings.scroll_h_range = config.scroll_h_range();
    }
    const auto [_, success] = injectors_.emplace(
        id,
        std::make_unique<MouseInjector>(
            snapshot_holder_,
            inspect_node_.CreateChild(inspect_node_.UniqueName("mouse-injector-")),
            std::move(settings), std::move(viewport), std::move(injector),
            /*inject=*/
            [&inject_func = settings.dispatch_policy ==
                                    fuchsia::ui::pointerinjector::DispatchPolicy::EXCLUSIVE_TARGET
                                ? inject_mouse_exclusive_
                                : inject_mouse_hit_tested_](
                InternalMouseEvent event, StreamId stream_id, const view_tree::Snapshot& snapshot) {
              TRACE_DURATION("input", "MouseInjector::inject_");
              inject_func(std::move(event), stream_id, snapshot);
            },
            /*cancel_stream=*/[this](StreamId stream_id) { cancel_mouse_stream_(stream_id); },
            /*on_channel_closed=*/std::move(on_channel_closed)));
    FX_CHECK(success) << "Injector already exists.";
  } else {
    FX_NOTREACHED();
  }

  callback();
}
// LINT.ThenChange(//src/ui/scenic/lib/input/dso/pointerinjector_registry.cc)

}  // namespace scenic_impl::input
