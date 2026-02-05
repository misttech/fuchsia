// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/tests/utils/scenic_ctf_test_base.h"

namespace integration_tests {

void ScenicCtfTest::SetFlatlandDisplayContent(fuchsia_ui_views::ViewportCreationToken token) {
  return ScenicCtfTestEnvironment::GetGlobalTestEnvironment()->SetFlatlandDisplayContent(
      std::move(token));
}

const std::shared_ptr<sys::ServiceDirectory>& ScenicCtfTest::LocalServiceDirectory() const {
  return ScenicCtfTestEnvironment::GetGlobalTestEnvironment()->LocalServiceDirectory();
}

uint64_t ScenicCtfTest::GetDisplayRotation() const {
  return ScenicCtfTestEnvironment::GetGlobalTestEnvironment()->GetDisplayRotation();
}

fuchsia_math::SizeU ScenicCtfTest::GetDisplayDimensions() const {
  return ScenicCtfTestEnvironment::GetGlobalTestEnvironment()->GetDisplayDimensions();
}

uint32_t ScenicCtfTest::GetDisplayRefreshRateMillihertz() const {
  return ScenicCtfTestEnvironment::GetGlobalTestEnvironment()->GetDisplayRefreshRateMillihertz();
}

uint32_t ScenicCtfTest::GetDisplayMaxLayerCount() const {
  return ScenicCtfTestEnvironment::GetGlobalTestEnvironment()->GetDisplayMaxLayerCount();
}

bool ScenicCtfTest::UseDisplayComposition() const {
  return ScenicCtfTestEnvironment::GetGlobalTestEnvironment()->UseDisplayComposition();
}

void ScenicCtfHlcppTest::SetUp() {
  {
    context_ = sys::ComponentContext::Create();
    ASSERT_EQ(context_->svc()->Connect(realm_factory_.NewRequest()), ZX_OK);

    fuchsia::ui::test::context::ScenicRealmFactoryCreateRealmRequest req;
    fuchsia::ui::test::context::ScenicRealmFactory_CreateRealm_Result res;

    req.set_realm_server(realm_proxy_.NewRequest());
    req.set_display_rotation(GetDisplayRotation());
    req.set_renderer(renderer_type_);
    req.set_display_composition(UseDisplayComposition());
    if (GetDisplayDimensions().height != 0 && GetDisplayDimensions().width != 0) {
      req.set_display_dimensions(GetDisplayDimensions());
    }
    if (GetDisplayRefreshRateMillihertz() != 0) {
      req.set_display_refresh_rate_millihertz(GetDisplayRefreshRateMillihertz());
    }
    if (GetDisplayMaxLayerCount() != 0) {
      req.set_display_max_layer_count(GetDisplayMaxLayerCount());
    }

    ASSERT_EQ(realm_factory_->CreateRealm(std::move(req), &res), ZX_OK);
  }
}

const std::shared_ptr<sys::ServiceDirectory>& ScenicCtfHlcppTest::LocalServiceDirectory() const {
  return context_->svc();
}

uint64_t ScenicCtfHlcppTest::GetDisplayRotation() const { return 0; }

fuchsia::math::SizeU ScenicCtfHlcppTest::GetDisplayDimensions() const {
  return {.width = 0, .height = 0};
}

uint32_t ScenicCtfHlcppTest::GetDisplayRefreshRateMillihertz() const { return 0; }

uint32_t ScenicCtfHlcppTest::GetDisplayMaxLayerCount() const { return 0; }

bool ScenicCtfHlcppTest::UseDisplayComposition() const { return true; }

}  // namespace integration_tests
