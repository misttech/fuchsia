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

InputManager::InputManager(async_dispatcher_t* input_dispatcher, sys::ComponentContext* context,
                           inspect::Node& parent_node, bool use_auto_focus)
    : input_dispatcher_(input_dispatcher),
      use_auto_focus_(use_auto_focus),
      focus_manager_(input_dispatcher, snapshot_holder_, parent_node.CreateChild("FocusManager")),
      observer_registry_(geometry_provider_),
      scoped_observer_registry_(geometry_provider_),
      input_(input_dispatcher, context, snapshot_holder_, parent_node,
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
  FX_DCHECK(context);
  // Served on the input thread.  Note that we don't explicitly publish `InputSystem` or its
  // sub-components; these are "view bound protocols" connected to via e.g. the Flatland API, not
  // routed by Fuchsia's component manager.
  focus_manager_.Publish(*context);

  // These are served on the main thread, not for any good reason: they just haven't been moved to
  // the input thread yet.
  utils::CheckIsOnMainThread();
  view_ref_installed_impl_.Publish(context);
  observer_registry_.Publish(context);
  scoped_observer_registry_.Publish(context);
}

void InputManager::RegisterViewFocuser(fidl::ServerEnd<fuchsia_ui_views::Focuser> focuser,
                                       zx_koid_t view_ref_koid) {
  async::PostTask(input_dispatcher_, [this, focuser = std::move(focuser), view_ref_koid]() mutable {
    focus_manager_.RegisterViewFocuser(view_ref_koid, fidl::NaturalToHLCPP(std::move(focuser)));
  });
}

void InputManager::RegisterViewRefFocused(fidl::ServerEnd<fuchsia_ui_views::ViewRefFocused> vrf,
                                          zx_koid_t view_ref_koid) {
  async::PostTask(input_dispatcher_, [this, vrf = std::move(vrf), view_ref_koid]() mutable {
    focus_manager_.RegisterViewRefFocused(view_ref_koid, fidl::NaturalToHLCPP(std::move(vrf)));
  });
}

void InputManager::RegisterTouchSource(
    fidl::ServerEnd<fuchsia_ui_pointer::TouchSource> touch_source, zx_koid_t view_ref_koid) {
  async::PostTask(
      input_dispatcher_, [this, touch_source = std::move(touch_source), view_ref_koid]() mutable {
        input_.RegisterTouchSource(fidl::NaturalToHLCPP(std::move(touch_source)), view_ref_koid);
      });
}

void InputManager::RegisterMouseSource(
    fidl::ServerEnd<fuchsia_ui_pointer::MouseSource> mouse_source, zx_koid_t view_ref_koid) {
  async::PostTask(
      input_dispatcher_, [this, mouse_source = std::move(mouse_source), view_ref_koid]() mutable {
        input_.RegisterMouseSource(fidl::NaturalToHLCPP(std::move(mouse_source)), view_ref_koid);
      });
}

void InputManager::OnNewViewTreeSnapshot(std::shared_ptr<const view_tree::Snapshot> snapshot) {
  utils::CheckIsOnMainThread();

  // There are dependencies between subsystems; `snapshot_holder_` is shared between them to
  // guarantee that they all work with a consistent view tree snapshot.
  snapshot_holder_->SetSnapshot(snapshot);

  // Poke FocusManager to eagerly respond to the new snapshot; it might have outstanding hanging
  // gets that otherwise wouldn't notice.
  focus_manager_.OnNewViewTreeSnapshot();

  // Keep both of these on the main thread for now.  If a FIDL client observes a change that results
  // in another message being sent to a FIDL protocol served on the input thread, we already updated
  // the snapshot visible to the input thread above in `snapshot_holder_->SetSnapshot(snapshot)`,
  // so there is no race whereby the input thread could use a stale snapshot.
  view_ref_installed_impl_.OnNewViewTreeSnapshot(snapshot);
  geometry_provider_.OnNewViewTreeSnapshot(std::move(snapshot));
}

}  // namespace scenic_impl::input
