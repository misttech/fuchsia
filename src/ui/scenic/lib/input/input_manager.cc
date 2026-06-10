// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/input/input_manager.h"

#include <fidl/fuchsia.ui.pointer/cpp/hlcpp_conversion.h>
#include <fidl/fuchsia.ui.views/cpp/hlcpp_conversion.h>
#include <lib/fidl/cpp/hlcpp_conversion.h>
#include <lib/inspect/cpp/inspect.h>

#include "src/ui/scenic/lib/utils/check_is_on_thread.h"

namespace scenic_impl::input {

InputManager::InputManager(async_dispatcher_t* input_dispatcher,
                           std::shared_ptr<view_tree::SnapshotHolder> snapshot_holder,
                           inspect::Node parent_node, bool use_auto_focus)
    : inspect_node_(std::move(parent_node)),
      use_auto_focus_(use_auto_focus),
      geometry_provider_(snapshot_holder),
      focus_manager_(input_dispatcher, snapshot_holder, inspect_node_.CreateChild("FocusManager")),
      observer_registry_(geometry_provider_),
      scoped_observer_registry_(geometry_provider_),
      view_ref_installed_impl_(snapshot_holder),
      input_(input_dispatcher, snapshot_holder, inspect_node_,
             [this](zx_koid_t koid, const view_tree::Snapshot& snapshot) {
               if (!use_auto_focus_)
                 return;

               const auto& focus_chain = focus_manager_.GetFocusChain(snapshot);
               if (!focus_chain.empty()) {
                 const zx_koid_t requestor = focus_chain[0];
                 const zx_koid_t request = koid != ZX_KOID_INVALID ? koid : requestor;
                 focus_manager_.RequestFocus(requestor, request, snapshot);
               }
             }) {
  // Constructed and executed entirely on the dedicated input thread.
  // Note that we don't explicitly publish `InputSystem` or its sub-components; these are
  // "view bound protocols" connected to via e.g. the Flatland API, not routed by Fuchsia's
  // component manager.
  utils::CheckIsOnInputThread();
}

void InputManager::RegisterViewFocuser(fidl::ServerEnd<fuchsia_ui_views::Focuser> focuser,
                                       zx_koid_t view_ref_koid) {
  focus_manager_.RegisterViewFocuser(view_ref_koid, fidl::NaturalToHLCPP(std::move(focuser)));
}

void InputManager::RegisterViewRefFocused(fidl::ServerEnd<fuchsia_ui_views::ViewRefFocused> vrf,
                                          zx_koid_t view_ref_koid) {
  focus_manager_.RegisterViewRefFocused(view_ref_koid, fidl::NaturalToHLCPP(std::move(vrf)));
}

void InputManager::RegisterTouchSource(
    fidl::ServerEnd<fuchsia_ui_pointer::TouchSource> touch_source, zx_koid_t view_ref_koid) {
  input_.RegisterTouchSource(fidl::NaturalToHLCPP(std::move(touch_source)), view_ref_koid);
}

void InputManager::RegisterMouseSource(
    fidl::ServerEnd<fuchsia_ui_pointer::MouseSource> mouse_source, zx_koid_t view_ref_koid) {
  input_.RegisterMouseSource(fidl::NaturalToHLCPP(std::move(mouse_source)), view_ref_koid);
}

void InputManager::OnNewViewTreeSnapshot() {
  // All of these run on the dedicated input thread. Because they are updated synchronously
  // here on the input thread, there is no risk of race conditions or thread synchronization
  // overhead when a FIDL client observes a change and sends a subsequent message to another
  // input or focus protocol served on this thread.
  utils::CheckIsOnInputThread();

  // Poke FocusManager to eagerly respond to the new snapshot; it might have outstanding hanging
  // gets that otherwise wouldn't notice.
  focus_manager_.OnNewViewTreeSnapshot();
  view_ref_installed_impl_.OnNewViewTreeSnapshot();
  geometry_provider_.OnNewViewTreeSnapshot();
}

void InputManager::BindFocusChainListenerRegistry(
    fidl::InterfaceRequest<fuchsia::ui::focus::FocusChainListenerRegistry> request) {
  focus_manager_.Bind(std::move(request));
}

void InputManager::BindViewRefInstalled(
    fidl::InterfaceRequest<fuchsia::ui::views::ViewRefInstalled> request) {
  view_ref_installed_impl_.Bind(std::move(request));
}

void InputManager::BindObserverRegistry(
    fidl::InterfaceRequest<fuchsia::ui::observation::test::Registry> request) {
  observer_registry_.Bind(std::move(request));
}

void InputManager::BindScopedObserverRegistry(
    fidl::InterfaceRequest<fuchsia::ui::observation::scope::Registry> request) {
  scoped_observer_registry_.Bind(std::move(request));
}

#if !defined(FUCHSIA_DSO)
void InputManager::BindPointerinjectorRegistry(
    fidl::InterfaceRequest<fuchsia::ui::pointerinjector::Registry> request) {
  input_.BindPointerinjectorRegistry(std::move(request));
}
#else
void InputManager::BindPointerinjectorRegistry(zx::channel channel) {
  input_.BindPointerinjectorRegistry(std::move(channel));
}
#endif

void InputManager::BindLocalHit(
    fidl::InterfaceRequest<fuchsia::ui::pointer::augment::LocalHit> request) {
  input_.BindLocalHit(std::move(request));
}

void InputManager::BindA11yPointerEventRegistry(
    fidl::InterfaceRequest<fuchsia::ui::input::accessibility::PointerEventRegistry> request) {
  input_.BindA11yPointerEventRegistry(std::move(request));
}

}  // namespace scenic_impl::input
