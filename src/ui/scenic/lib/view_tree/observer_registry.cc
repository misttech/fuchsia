// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "observer_registry.h"

#include <lib/syslog/cpp/log_level.h>
#include <lib/syslog/cpp/log_settings.h>
#include <lib/syslog/cpp/macros.h>

#include "src/ui/scenic/lib/utils/check_is_on_thread.h"

namespace view_tree {

Registry::Registry(view_tree::GeometryProvider& geometry_provider)
    : geometry_provider_(geometry_provider) {}

void Registry::RegisterGlobalViewTreeWatcher(
    fidl::InterfaceRequest<fuchsia::ui::observation::geometry::ViewTreeWatcher> request,
    Registry::RegisterGlobalViewTreeWatcherCallback callback) {
  utils::CheckIsOnInputThread();
  geometry_provider_.RegisterGlobalViewTreeWatcher(std::move(request));

  callback();
}

void Registry::Bind(fidl::InterfaceRequest<fuchsia::ui::observation::test::Registry> request) {
  utils::CheckIsOnInputThread();
  bindings_.AddBinding(this, std::move(request));
}
}  // namespace view_tree
