// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_INPUT_INPUT_SYSTEM_H_
#define SRC_UI_SCENIC_LIB_INPUT_INPUT_SYSTEM_H_

#include <lib/async/dispatcher.h>

#include <optional>

#include "src/ui/scenic/lib/input/mouse_system.h"
#include "src/ui/scenic/lib/input/pointerinjector_registry.h"
#include "src/ui/scenic/lib/input/touch_system.h"
#include "src/ui/scenic/lib/view_tree/snapshot_types.h"
#if defined(FUCHSIA_DSO)
#include "src/ui/scenic/lib/input/dso/pointerinjector_registry.h"  // nogncheck
#endif

namespace scenic_impl::input {

// Tracks and coordinates input APIs.
class InputSystem {
 public:
  InputSystem(sys::ComponentContext* context, inspect::Node& inspect_node,
              RequestFocusFunc request_focus, async_dispatcher_t* dispatcher);
  ~InputSystem() = default;

  void OnNewViewTreeSnapshot(std::shared_ptr<const view_tree::Snapshot> snapshot) {
    pointerinjector_registry_.OnNewViewTreeSnapshot(snapshot);
#if defined(FUCHSIA_DSO)
    pointerinjector_registry_dso_.OnNewViewTreeSnapshot(snapshot);
#endif
    touch_system_.SetViewTreeSnapshot(snapshot);
    mouse_system_.SetViewTreeSnapshot(snapshot);
  }

  void RegisterTouchSource(
      fidl::InterfaceRequest<fuchsia::ui::pointer::TouchSource> touch_source_request,
      zx_koid_t client_view_ref_koid) {
    touch_system_.RegisterTouchSource(std::move(touch_source_request), client_view_ref_koid);
  }

  void RegisterMouseSource(
      fidl::InterfaceRequest<fuchsia::ui::pointer::MouseSource> mouse_source_request,
      zx_koid_t client_view_ref_koid) {
    mouse_system_.RegisterMouseSource(std::move(mouse_source_request), client_view_ref_koid);
  }

  // For tests.
  // TODO(https://fxbug.dev/42152433): Remove when integration tests are properly separated out.
  void RegisterPointerinjector(
      fuchsia::ui::pointerinjector::Config config,
      fidl::InterfaceRequest<fuchsia::ui::pointerinjector::Device> injector,
      fuchsia::ui::pointerinjector::Registry::RegisterCallback callback) {
    pointerinjector_registry_.Register(std::move(config), std::move(injector), std::move(callback));
  }

  // Accessor for tests.
  // TODO(https://fxbug.dev/42152433): Remove when integration tests are properly separated out.
  scenic_impl::input::TouchSystem& touch_system() { return touch_system_; }

 private:
  const RequestFocusFunc request_focus_;
  HitTester hit_tester_;
  MouseSystem mouse_system_;
  TouchSystem touch_system_;
  PointerinjectorRegistry pointerinjector_registry_;
#if defined(FUCHSIA_DSO)
  ::scenic_impl::input_dso::PointerinjectorRegistry pointerinjector_registry_dso_;
#endif
};

}  // namespace scenic_impl::input

#endif  // SRC_UI_SCENIC_LIB_INPUT_INPUT_SYSTEM_H_
