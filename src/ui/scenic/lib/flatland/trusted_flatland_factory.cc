// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/flatland/trusted_flatland_factory.h"

namespace flatland {

TrustedFlatlandFactoryImpl::TrustedFlatlandFactoryImpl(
    std::shared_ptr<FlatlandManager> flatland_manager)
    : flatland_manager_(std::move(flatland_manager)) {}

void TrustedFlatlandFactoryImpl::CreateFlatland(CreateFlatlandRequest& request,
                                                CreateFlatlandCompleter::Sync& completer) {
  CreateFlatland(fidl::InterfaceRequest<fuchsia::ui::composition::Flatland>(
                     request.server_end().TakeChannel()),
                 std::move(request.config()));
  completer.Reply(fit::ok());
}

void TrustedFlatlandFactoryImpl::CreateFlatland(
    fidl::InterfaceRequest<fuchsia::ui::composition::Flatland> server_end,
    fuchsia_ui_composition::TrustedFlatlandConfig config) {
  flatland_manager_->CreateFlatland(std::move(server_end), std::move(config));
}

}  // namespace flatland