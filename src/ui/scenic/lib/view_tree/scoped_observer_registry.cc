// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "scoped_observer_registry.h"

#include <lib/syslog/cpp/macros.h>

#include "src/ui/scenic/lib/utils/check_is_on_thread.h"

namespace view_tree {

ScopedRegistry::ScopedRegistry(view_tree::GeometryProvider& geometry_provider)
    : geometry_provider_(geometry_provider) {}

void ScopedRegistry::RegisterScopedViewTreeWatcher(
    zx_koid_t context_view,
    fidl::InterfaceRequest<fuchsia::ui::observation::geometry::ViewTreeWatcher> request,
    ScopedRegistry::RegisterScopedViewTreeWatcherCallback callback) {
  utils::CheckIsOnInputThread();
  geometry_provider_.Register(std::move(request), context_view);

  callback();
}

void ScopedRegistry::Bind(
    fidl::InterfaceRequest<fuchsia::ui::observation::scope::Registry> request) {
  utils::CheckIsOnInputThread();
  bindings_.AddBinding(this, std::move(request));
}
}  // namespace view_tree
