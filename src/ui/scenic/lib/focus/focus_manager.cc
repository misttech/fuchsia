// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/focus/focus_manager.h"

#include <lib/async/cpp/task.h>
#include <lib/syslog/cpp/macros.h>

#include "src/ui/scenic/lib/utils/check_is_on_thread.h"
#include "src/ui/scenic/lib/utils/helpers.h"

namespace focus {

namespace {
std::string ToString(const std::vector<zx_koid_t>& chain) {
  std::string value;
  for (zx_koid_t koid : chain) {
    value += std::to_string(koid);
    value += ", ";
  }
  return value;
}
zx_koid_t FocusKoidOf(const std::vector<zx_koid_t>& chain) {
  if (chain.empty()) {
    return ZX_KOID_INVALID;
  }
  return chain.back();
}
}  // namespace

FocusManager::FocusManager(async_dispatcher_t* input_dispatcher,
                           std::shared_ptr<view_tree::SnapshotHolder> snapshot_holder,
                           inspect::Node inspect_node)
    : input_dispatcher_(input_dispatcher),
      snapshot_holder_(std::move(snapshot_holder)),
      view_focuser_registry_(
          /*request_focus*/
          [this](zx_koid_t requestor, zx_koid_t request) {
            auto snapshot_ref = snapshot_holder_->GetSnapshot();
            return RequestFocus(requestor, request, *snapshot_ref) == FocusChangeStatus::kAccept;
          },
          /*set_auto_focus*/
          [this](zx_koid_t requestor, zx_koid_t request) {
            auto snapshot_ref = snapshot_holder_->GetSnapshot();
            SetAutoFocus(requestor, request, *snapshot_ref);
          }),
      inspect_node_(std::move(inspect_node)) {
  FX_DCHECK(input_dispatcher_);

  // Track the focus chain in inspect.
  lazy_ = inspect_node_.CreateLazyValues("values", [this] {
    inspect::Inspector inspector;

    auto array = inspector.GetRoot().CreateUintArray("focus_chain", focus_chain_.size());
    for (size_t i = 0; i < focus_chain_.size(); i++) {
      array.Set(i, focus_chain_[i]);
    }
    inspector.emplace(std::move(array));

    return fpromise::make_ok_promise(std::move(inspector));
  });
}

void FocusManager::Publish(sys::ComponentContext& component_context) {
  component_context.outgoing()->AddPublicService<FocusChainListenerRegistry>(
      focus_chain_listener_registry_.GetHandler(this, input_dispatcher_));
}

void FocusManager::OnNewViewTreeSnapshot() {
  // Post a task to eagerly update the focus state.  It's possible that this task will race with
  // e.g. a FIDL request served on the input thread.  That's OK; `EnsureValidFocus()` is designed to
  // be idempotent and not do work twice for the same view tree snapshot.
  //
  // Posting the task eagerly has two benefits:
  // - *necessary* for hanging gets which wouldn't otherwise be be notified.
  // - latency reduction for a FIDL request that arrives after `EnsureValidFocus()` has already been
  //   run for the current snapshot.
  async::PostTask(input_dispatcher_, [this]() {
    TRACE_DURATION("input", "FocusManager::OnNewViewTreeSnapshot");
    auto snapshot = snapshot_holder_->GetSnapshot();
    EnsureValidFocus(*snapshot);
  });
}

FocusChangeStatus FocusManager::RequestFocus(zx_koid_t requestor, zx_koid_t request,
                                             const view_tree::Snapshot& snapshot) {
  TRACE_DURATION("input", "FocusManager::RequestFocus");
  EnsureValidFocus(snapshot);

  // Invalid requestor.
  if (!snapshot.view_tree.contains(requestor)) {
    return FocusChangeStatus::kErrorRequestorInvalid;
  }

  // Invalid request.
  if (!snapshot.view_tree.contains(request)) {
    return FocusChangeStatus::kErrorRequestInvalid;
  }

  // Transfer policy: requestor must be authorized.
  if (std::find(focus_chain_.begin(), focus_chain_.end(), requestor) == focus_chain_.end()) {
    return FocusChangeStatus::kErrorRequestorNotAuthorized;
  }

  // Transfer policy: requestor must be ancestor of request
  if (!snapshot.IsDescendant(/*descendant_koid*/ request, /*ancestor_koid*/ requestor) &&
      request != requestor) {
    return FocusChangeStatus::kErrorRequestorNotRequestAncestor;
  }

  // Transfer policy: request must be focusable
  if (!snapshot.view_tree.at(request).is_focusable) {
    return FocusChangeStatus::kErrorRequestCannotReceiveFocus;
  }

  // It's a valid request for a change to focus chain.
  SetFocus(request, snapshot);
  FX_DCHECK(focus_chain_.at(0) == snapshot.root);
  return FocusChangeStatus::kAccept;
}

FocusChangeStatus FocusManager::RequestFocusForTest(zx_koid_t requestor, zx_koid_t request) {
  auto snapshot_ref = snapshot_holder_->GetSnapshot();
  return RequestFocus(requestor, request, *snapshot_ref);
}

const std::vector<zx_koid_t>& FocusManager::GetFocusChainForTest() {
  auto snapshot_ref = snapshot_holder_->GetSnapshot();
  return GetFocusChain(*snapshot_ref);
}

void FocusManager::EnsureValidFocus(const view_tree::Snapshot& snapshot) {
  TRACE_DURATION("input", "FocusManager::EnsureValidFocus");
  utils::CheckIsOnInputThread();

  // This should be guaranteed by the properties of `view_tree::SnapshotRef`: nobody can hang onto
  // an older ref while a new one is checked out, because only one ref can be checked out at a time.
  FX_DCHECK(snapshot.sequence_number >= last_seen_sequence_number_);

  if (snapshot.sequence_number > last_seen_sequence_number_) {
    last_seen_sequence_number_ = snapshot.sequence_number;
    // TODO(https://fxbug.dev/42156009): This has linear cost. Look at making it cheaper.
    // ViewRefFocused clients should be registered before RepairFocus() so that they can be notified
    // about the new root getting focus.
    view_ref_focused_registry_.UpdateRegisteredViews(snapshot);
    RepairFocus(snapshot);
  }
}

void FocusManager::Register(
    fidl::InterfaceHandle<fuchsia::ui::focus::FocusChainListener> focus_chain_listener) {
  TRACE_DURATION("input", "FocusManager::Register");
  utils::CheckIsOnInputThread();

  // Retrieve snapshot and ensure the focus chain is valid for it *before* we add the new listener,
  // so that we don't dispatch the focus chain to it twice.
  auto snapshot_ref = snapshot_holder_->GetSnapshot();
  EnsureValidFocus(*snapshot_ref);

  fuchsia::ui::focus::FocusChainListenerPtr new_listener;
  new_listener.Bind(std::move(focus_chain_listener), input_dispatcher_);

  // Now emplace the new listener.
  const uint64_t id = next_focus_chain_listener_id_++;
  new_listener.set_error_handler([this, id](zx_status_t) { focus_chain_listeners_.erase(id); });
  const auto [it, success] = focus_chain_listeners_.emplace(id, std::move(new_listener));
  FX_DCHECK(success);

  // Dispatch current chain to this new listener.
  DispatchFocusChainTo(it->second, *snapshot_ref);
}

void FocusManager::RegisterViewRefFocused(
    zx_koid_t koid, fidl::InterfaceRequest<fuchsia::ui::views::ViewRefFocused> vrf) {
  TRACE_DURATION("gfx", "FocusManager::RegisterViewRefFocused");
  utils::CheckIsOnInputThread();
  view_ref_focused_registry_.Register(koid, std::move(vrf));
}

void FocusManager::RegisterViewFocuser(
    zx_koid_t koid, fidl::InterfaceRequest<fuchsia::ui::views::Focuser> focuser) {
  TRACE_DURATION("gfx", "FocusManager::RegisterViewFocuser");
  utils::CheckIsOnInputThread();
  view_focuser_registry_.Register(koid, std::move(focuser));
}

void FocusManager::DispatchFocusChainTo(const fuchsia::ui::focus::FocusChainListenerPtr& listener,
                                        const view_tree::Snapshot& snapshot) const {
  listener->OnFocusChange(CloneFocusChain(snapshot), [] { /* No flow control yet. */ });
}

void FocusManager::DispatchFocusChain(const view_tree::Snapshot& snapshot) const {
  for (auto& [_, listener] : focus_chain_listeners_) {
    DispatchFocusChainTo(listener, snapshot);
  }
}

void FocusManager::DispatchFocusEvents(zx_koid_t old_focus, zx_koid_t new_focus) {
  if (old_focus == new_focus)
    return;

  // Send over fuchsia.ui.views.ViewRefFocused.
  view_ref_focused_registry_.UpdateFocus(old_focus, new_focus);
}

void FocusManager::SetAutoFocus(zx_koid_t requestor, zx_koid_t target,
                                const view_tree::Snapshot& snapshot) {
  TRACE_DURATION("gfx", "FocusManager::SetAutoFocus");
  EnsureValidFocus(snapshot);

  if (target != ZX_KOID_INVALID) {
    auto_focus_targets_[requestor] = target;
  } else {
    auto_focus_targets_.erase(requestor);
  }

  // Move focus to the currently focused View to see if auto focus causes any changes.
  if (!focus_chain_.empty()) {
    SetFocus(focus_chain_.back(), snapshot);
  }
}

void FocusManager::SetAutoFocusForTest(zx_koid_t requestor, zx_koid_t target) {
  auto snapshot_ref = snapshot_holder_->GetSnapshot();
  SetAutoFocus(requestor, target, *snapshot_ref);
}

zx_koid_t FocusManager::FindNextAutoFocusTarget(zx_koid_t koid,
                                                const view_tree::Snapshot& snapshot) const {
  const auto it = auto_focus_targets_.find(koid);
  if (it != auto_focus_targets_.end() && snapshot.view_tree.contains(it->second)) {
    koid = it->second;
    while (koid != snapshot.root && !snapshot.view_tree.at(koid).is_focusable) {
      koid = snapshot.view_tree.at(koid).parent;
    }
  }
  return koid;
}

zx_koid_t FocusManager::ResolveAutoFocus(zx_koid_t koid,
                                         const view_tree::Snapshot& snapshot) const {
  // Iterate through auto focus targets until we find a stable point (i.e. where
  // FindNextAutoFocusTarget(koid) == koid).
  zx_koid_t auto_focus_result = FindNextAutoFocusTarget(koid, snapshot);
  while (auto_focus_result != koid && auto_focus_result != snapshot.root) {
    koid = auto_focus_result;
    auto_focus_result = FindNextAutoFocusTarget(koid, snapshot);
  }
  return auto_focus_result;
}

fuchsia::ui::views::ViewRef FocusManager::CloneViewRefOf(zx_koid_t koid,
                                                         const view_tree::Snapshot& snapshot) {
  FX_DCHECK(snapshot.view_tree.contains(koid))
      << "all views in the focus chain must exist in the view tree";
  fuchsia::ui::views::ViewRef clone;
  const auto& view_node = snapshot.view_tree.at(koid);
  clone.reference = utils::CopyZxHandle(view_node.view_ref->eventpair());

  return clone;
}

fuchsia::ui::focus::FocusChain FocusManager::CloneFocusChain(
    const view_tree::Snapshot& snapshot) const {
  fuchsia::ui::focus::FocusChain full_copy{};
  for (const zx_koid_t koid : focus_chain_) {
    full_copy.mutable_focus_chain()->push_back(CloneViewRefOf(koid, snapshot));
  }
  return full_copy;
}

void FocusManager::RepairFocus(const view_tree::Snapshot& snapshot) {
  // Old root no longer valid -> move focus to new root.
  if (focus_chain_.empty() || snapshot.root != focus_chain_.front()) {
    SetFocus(snapshot.root, snapshot);
    return;
  }

  // Even if the focus chain isn't invalid we still want to call SetFocus() on the currently focused
  // View since it may have a newly valid auto focus target.
  zx_koid_t focus_target = focus_chain_.back();

  // See if there's any place where the old focus chain breaks a parent-child relationship, and
  // truncate from there.
  // Note: Start at i = 1 so we can compare with i - 1.
  for (size_t child_index = 1; child_index < focus_chain_.size(); ++child_index) {
    const zx_koid_t child = focus_chain_.at(child_index);
    const zx_koid_t parent = focus_chain_.at(child_index - 1);
    if (!snapshot.view_tree.contains(child) || snapshot.view_tree.at(child).parent != parent) {
      focus_target = parent;
      break;
    }
  }

  // Find first focusable parent ancestor starting from |focus_target|.
  while (focus_target != snapshot.root && !snapshot.view_tree.at(focus_target).is_focusable) {
    focus_target = snapshot.view_tree.at(focus_target).parent;
  }
  SetFocus(focus_target, snapshot);
}

void FocusManager::SetFocus(zx_koid_t koid, const view_tree::Snapshot& snapshot) {
  FX_DCHECK(koid != ZX_KOID_INVALID || koid == snapshot.root);
  if (koid != ZX_KOID_INVALID) {
    FX_DCHECK(snapshot.view_tree.contains(koid));
    FX_DCHECK(snapshot.view_tree.at(koid).is_focusable);
  }

  koid = ResolveAutoFocus(koid, snapshot);

  std::vector<zx_koid_t> new_focus_chain;

  // Regenerate chain.
  while (koid != ZX_KOID_INVALID) {
    new_focus_chain.emplace_back(koid);
    koid = snapshot.view_tree.at(koid).parent;
  }
  std::reverse(new_focus_chain.begin(), new_focus_chain.end());

  SetFocusChain(std::move(new_focus_chain), snapshot);
}

void FocusManager::SetFocusChain(std::vector<zx_koid_t> update,
                                 const view_tree::Snapshot& snapshot) {
  if (update != focus_chain_) {
    FX_LOGS(DEBUG) << "Focus chain update: " << ToString(update);
    const zx_koid_t old_focus = FocusKoidOf(focus_chain_);
    const zx_koid_t new_focus = FocusKoidOf(update);

    focus_chain_ = std::move(update);

    DispatchFocusChain(snapshot);
    DispatchFocusEvents(old_focus, new_focus);
  }
}

}  // namespace focus
