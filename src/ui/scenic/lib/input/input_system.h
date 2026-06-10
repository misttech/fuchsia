// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_INPUT_INPUT_SYSTEM_H_
#define SRC_UI_SCENIC_LIB_INPUT_INPUT_SYSTEM_H_

#include <lib/async/dispatcher.h>
#include <lib/zx/channel.h>

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
  InputSystem(async_dispatcher_t* input_dispatcher,
              std::shared_ptr<view_tree::SnapshotHolder> snapshot_holder,
              inspect::Node& inspect_node, RequestFocusFunc request_focus,
              sys::ComponentContext* context = nullptr);
  ~InputSystem() = default;

#if !defined(FUCHSIA_DSO)
  void BindPointerinjectorRegistry(
      fidl::InterfaceRequest<fuchsia::ui::pointerinjector::Registry> request);
#else
  void BindPointerinjectorRegistry(zx::channel channel);
#endif
  void BindLocalHit(fidl::InterfaceRequest<fuchsia::ui::pointer::augment::LocalHit> request);
  void BindA11yPointerEventRegistry(
      fidl::InterfaceRequest<fuchsia::ui::input::accessibility::PointerEventRegistry> request);

  // Delegates to `touch_system_`.
  void RegisterTouchSource(
      fidl::InterfaceRequest<fuchsia::ui::pointer::TouchSource> touch_source_request,
      zx_koid_t client_view_ref_koid);

  // Delegates to `mouse_system_`.
  void RegisterMouseSource(
      fidl::InterfaceRequest<fuchsia::ui::pointer::MouseSource> mouse_source_request,
      zx_koid_t client_view_ref_koid);

#if !defined(FUCHSIA_DSO)
  // For tests.
  // TODO(https://fxbug.dev/42152433): Remove when integration tests are properly separated out.
  void RegisterPointerinjector(
      fuchsia::ui::pointerinjector::Config config,
      fidl::InterfaceRequest<fuchsia::ui::pointerinjector::Device> injector,
      fuchsia::ui::pointerinjector::Registry::RegisterCallback callback) {
    pointerinjector_registry_.Register(std::move(config), std::move(injector), std::move(callback));
  }
#endif

 private:
  HitTester hit_tester_;
  MouseSystem mouse_system_;
  TouchSystem touch_system_;
#if defined(FUCHSIA_DSO)
  ::scenic_impl::input_dso::PointerinjectorRegistry pointerinjector_registry_;
#else
  PointerinjectorRegistry pointerinjector_registry_;
#endif
};

}  // namespace scenic_impl::input

#endif  // SRC_UI_SCENIC_LIB_INPUT_INPUT_SYSTEM_H_
