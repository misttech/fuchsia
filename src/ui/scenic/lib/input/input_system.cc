// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/input/input_system.h"

#include <lib/fdf/cpp/channel.h>
#include <lib/fdf/token.h>
#include <lib/syslog/cpp/macros.h>

#include "src/ui/scenic/lib/utils/check_is_on_thread.h"

#if defined(FUCHSIA_DSO)
#include <fidl/fuchsia.ui.pointerinjector.dso/cpp/driver/wire.h>
#endif

namespace scenic_impl::input {

InputSystem::InputSystem(async_dispatcher_t *input_dispatcher, sys::ComponentContext *context,
                         std::shared_ptr<view_tree::SnapshotHolder> snapshot_holder,
                         inspect::Node &inspect_node, RequestFocusFunc request_focus)
    : hit_tester_(inspect_node),
      mouse_system_(context, snapshot_holder, hit_tester_, std::move(request_focus)),
      touch_system_(input_dispatcher, context, snapshot_holder, hit_tester_, inspect_node),
      pointerinjector_registry_(
          input_dispatcher, context, snapshot_holder,
          /*inject_touch_exclusive=*/
          [&touch_system = touch_system_](InternalTouchEvent event, StreamId stream_id) {
            touch_system.InjectTouchEventExclusive(std::move(event), stream_id);
          },
          /*inject_touch_hit_tested=*/
          [&touch_system = touch_system_](InternalTouchEvent event, StreamId stream_id) {
            touch_system.InjectTouchEventHitTested(std::move(event), stream_id);
          },
          /*inject_mouse_exclusive=*/
          [&mouse_system = mouse_system_](InternalMouseEvent event, StreamId stream_id) {
            mouse_system.InjectMouseEventExclusive(std::move(event), stream_id);
          },
          /*inject_mouse_hit_tested=*/
          [&mouse_system = mouse_system_](InternalMouseEvent event, StreamId stream_id) {
            mouse_system.InjectMouseEventHitTested(std::move(event), stream_id);
          },
          // Explicit call necessary to cancel mouse stream, because mouse stream itself does not
          // track phase.
          /*cancel_mouse_stream=*/
          [&mouse_system = mouse_system_](StreamId stream_id) {
            mouse_system.CancelMouseStream(stream_id);
          },
          inspect_node.CreateChild("PointerinjectorRegistry"))
#if defined(FUCHSIA_DSO)
      ,
      pointerinjector_registry_dso_(
          input_dispatcher, snapshot_holder,
          /*inject_touch_exclusive=*/
          [&touch_system = touch_system_](InternalTouchEvent event, StreamId stream_id) {
            touch_system.InjectTouchEventExclusive(std::move(event), stream_id);
          },
          /*inject_touch_hit_tested=*/
          [&touch_system = touch_system_](InternalTouchEvent event, StreamId stream_id) {
            touch_system.InjectTouchEventHitTested(std::move(event), stream_id);
          },
          inspect_node.CreateChild("PointerinjectorRegistryDso")) {
  FX_DCHECK(input_dispatcher);
  context->outgoing()->AddPublicService(
      [this](zx::channel zx_channel, async_dispatcher_t *unused_dispatcher) mutable {
        zx_handle_t handle;
        zx_status_t s = fdf_token_receive(zx_channel.release(), &handle);
        if (s != ZX_OK) {
          FX_LOGS(WARNING) << "FDF token failed cast to channel on "
                           << fuchsia_ui_pointerinjector::Registry::kDiscoverableName;
          return;
        }
        pointerinjector_registry_dso_.Bind(fdf::Channel(handle));
      },
      fuchsia_ui_pointerinjector_dso::Registry::kDiscoverableName);
}
#else
{
}
#endif

void InputSystem::RegisterTouchSource(
    fidl::InterfaceRequest<fuchsia::ui::pointer::TouchSource> touch_source_request,
    zx_koid_t client_view_ref_koid) {
  utils::CheckIsOnInputThread();
  touch_system_.RegisterTouchSource(std::move(touch_source_request), client_view_ref_koid);
}

void InputSystem::RegisterMouseSource(
    fidl::InterfaceRequest<fuchsia::ui::pointer::MouseSource> mouse_source_request,
    zx_koid_t client_view_ref_koid) {
  utils::CheckIsOnInputThread();
  mouse_system_.RegisterMouseSource(std::move(mouse_source_request), client_view_ref_koid);
}

}  // namespace scenic_impl::input
