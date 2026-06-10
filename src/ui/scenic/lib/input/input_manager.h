// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_INPUT_INPUT_MANAGER_H_
#define SRC_UI_SCENIC_LIB_INPUT_INPUT_MANAGER_H_

#include <fidl/fuchsia.ui.pointer/cpp/fidl.h>
#include <fidl/fuchsia.ui.views/cpp/fidl.h>
#include <lib/zx/channel.h>

#include "src/ui/scenic/lib/focus/focus_manager.h"
#include "src/ui/scenic/lib/input/constants.h"
#include "src/ui/scenic/lib/input/input_system.h"
#include "src/ui/scenic/lib/view_tree/geometry_provider.h"
#include "src/ui/scenic/lib/view_tree/observer_registry.h"
#include "src/ui/scenic/lib/view_tree/scoped_observer_registry.h"
#include "src/ui/scenic/lib/view_tree/view_ref_installed_impl.h"

namespace scenic_impl::input {

// Encapsulates all input-related subsystems.
//
class InputManager {
 public:
  explicit InputManager(async_dispatcher_t* input_dispatcher,
                        std::shared_ptr<view_tree::SnapshotHolder> snapshot_holder,
                        inspect::Node parent_node, bool use_auto_focus);
  ~InputManager() = default;

  // Facades for registering view-bound FIDL protocol endpoints.
  //
  // "View-bound" protocols (unlike standard component-scoped services routed via .cml files)
  // are scoped strictly to a single graphical View in the scene graph (identified by its ViewRef).
  // Their lifecycle, permissions, event delivery, and coordinate spaces are all tied to that
  // View's active presence in the ViewTree.
  //
  // These methods receive incoming FIDL server endpoints from clients, associate them with
  // a specific view ref KOID, and safely dispatch their registration and message-handling
  // onto the input dispatcher thread.
  //
  // Typically called by higher-level, session-managing systems (e.g. Flatland) when a client
  // creates a view and requests connection to these view-bound input or focus protocols.

  // Registers a Focuser server endpoint to allow a client to request focus changes on the
  // behalf of its view.
  void RegisterViewFocuser(fidl::ServerEnd<fuchsia_ui_views::Focuser> focuser,
                           zx_koid_t view_ref_koid);

  // Registers a ViewRefFocused listener to notify a client when its view gains or loses focus.
  void RegisterViewRefFocused(fidl::ServerEnd<fuchsia_ui_views::ViewRefFocused> vrf,
                              zx_koid_t view_ref_koid);

  // Registers a TouchSource server endpoint to deliver touch events targeted to the view.
  void RegisterTouchSource(fidl::ServerEnd<fuchsia_ui_pointer::TouchSource> touch_source,
                           zx_koid_t view_ref_koid);

  // Registers a MouseSource server endpoint to deliver mouse events targeted to the view.
  void RegisterMouseSource(fidl::ServerEnd<fuchsia_ui_pointer::MouseSource> mouse_source,
                           zx_koid_t view_ref_koid);

  // Dispatches a newly generated, consistent scene graph snapshot to all input subsystems.
  void OnNewViewTreeSnapshot();

  // Binds FIDL protocol endpoints to the input thread.
  void BindFocusChainListenerRegistry(
      fidl::InterfaceRequest<fuchsia::ui::focus::FocusChainListenerRegistry> request);
  void BindViewRefInstalled(fidl::InterfaceRequest<fuchsia::ui::views::ViewRefInstalled> request);
  void BindObserverRegistry(
      fidl::InterfaceRequest<fuchsia::ui::observation::test::Registry> request);
  void BindScopedObserverRegistry(
      fidl::InterfaceRequest<fuchsia::ui::observation::scope::Registry> request);
#if !defined(FUCHSIA_DSO)
  void BindPointerinjectorRegistry(
      fidl::InterfaceRequest<fuchsia::ui::pointerinjector::Registry> request);
#else
  void BindPointerinjectorRegistry(zx::channel channel);
#endif
  void BindLocalHit(fidl::InterfaceRequest<fuchsia::ui::pointer::augment::LocalHit> request);
  void BindA11yPointerEventRegistry(
      fidl::InterfaceRequest<fuchsia::ui::input::accessibility::PointerEventRegistry> request);

 private:
  inspect::Node inspect_node_;
  const bool use_auto_focus_;

  view_tree::GeometryProvider geometry_provider_;
  focus::FocusManager focus_manager_;
  view_tree::Registry observer_registry_;
  view_tree::ScopedRegistry scoped_observer_registry_;
  view_tree::ViewRefInstalledImpl view_ref_installed_impl_;
  InputSystem input_;
};

}  // namespace scenic_impl::input

#endif  // SRC_UI_SCENIC_LIB_INPUT_INPUT_MANAGER_H_
