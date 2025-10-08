// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/tests/utils/scenic_ctf_test_base.h"

namespace integration_tests {

void ScenicCtfTest::SetUp() {
  {
    context_ = sys::ComponentContext::Create();

    // Connect to realm factory.
    {
      auto result = fidl::CreateEndpoints<fuchsia_ui_test_context::ScenicRealmFactory>();
      ASSERT_TRUE(result.is_ok());
      auto& [client_end, server_end] = result.value();

      const std::string& service_path =
          fuchsia_ui_test_context::ScenicRealmFactory::kDiscoverableName;
      ASSERT_EQ(context_->svc()->Connect(service_path, server_end.TakeChannel()), ZX_OK);

      realm_factory_ =
          fidl::SyncClient<fuchsia_ui_test_context::ScenicRealmFactory>(std::move(client_end));
    }

    auto result = fidl::CreateEndpoints<fuchsia_testing_harness::RealmProxy>();
    ASSERT_TRUE(result.is_ok());
    auto& [client_end, server_end] = result.value();

    realm_proxy_ = fidl::SyncClient<fuchsia_testing_harness::RealmProxy>(std::move(client_end));

    fuchsia_ui_test_context::ScenicRealmFactoryCreateRealmRequest req;
    req.realm_server() = std::move(server_end);
    req.display_rotation() = DisplayRotation();
    req.renderer() = Renderer();
    req.display_composition() = DisplayComposition();
    if (DisplayDimensions().height() != 0 && DisplayDimensions().width() != 0) {
      req.display_dimensions() = DisplayDimensions();
    }
    if (DisplayRefreshRateMillihertz() != 0) {
      req.display_refresh_rate_millihertz() = DisplayRefreshRateMillihertz();
    }
    if (DisplayMaxLayerCount() != 0) {
      req.display_max_layer_count() = DisplayMaxLayerCount();
    }

    ASSERT_TRUE(realm_factory_->CreateRealm(std::move(req)).is_ok());
  }
}

const std::shared_ptr<sys::ServiceDirectory>& ScenicCtfTest::LocalServiceDirectory() const {
  return context_->svc();
}

uint64_t ScenicCtfTest::DisplayRotation() const { return 0; }

fuchsia_ui_test_context::RendererType ScenicCtfTest::Renderer() const {
  return fuchsia_ui_test_context::RendererType::kVulkan;
}

fuchsia_math::SizeU ScenicCtfTest::DisplayDimensions() const { return fuchsia_math::SizeU(0, 0); }

uint32_t ScenicCtfTest::DisplayRefreshRateMillihertz() const { return 0; }

uint32_t ScenicCtfTest::DisplayMaxLayerCount() const { return 0; }

bool ScenicCtfTest::DisplayComposition() const { return true; }

void ScenicCtfHlcppTest::SetUp() {
  {
    context_ = sys::ComponentContext::Create();
    ASSERT_EQ(context_->svc()->Connect(realm_factory_.NewRequest()), ZX_OK);

    fuchsia::ui::test::context::ScenicRealmFactoryCreateRealmRequest req;
    fuchsia::ui::test::context::ScenicRealmFactory_CreateRealm_Result res;

    req.set_realm_server(realm_proxy_.NewRequest());
    req.set_display_rotation(DisplayRotation());
    req.set_renderer(Renderer());
    req.set_display_composition(DisplayComposition());
    if (DisplayDimensions().height != 0 && DisplayDimensions().width != 0) {
      req.set_display_dimensions(DisplayDimensions());
    }
    if (DisplayRefreshRateMillihertz() != 0) {
      req.set_display_refresh_rate_millihertz(DisplayRefreshRateMillihertz());
    }
    if (DisplayMaxLayerCount() != 0) {
      req.set_display_max_layer_count(DisplayMaxLayerCount());
    }

    ASSERT_EQ(realm_factory_->CreateRealm(std::move(req), &res), ZX_OK);
  }
}

const std::shared_ptr<sys::ServiceDirectory>& ScenicCtfHlcppTest::LocalServiceDirectory() const {
  return context_->svc();
}

uint64_t ScenicCtfHlcppTest::DisplayRotation() const { return 0; }

fuchsia::ui::test::context::RendererType ScenicCtfHlcppTest::Renderer() const {
  return fuchsia::ui::test::context::RendererType::VULKAN;
}

fuchsia::math::SizeU ScenicCtfHlcppTest::DisplayDimensions() const {
  return {.width = 0, .height = 0};
}

uint32_t ScenicCtfHlcppTest::DisplayRefreshRateMillihertz() const { return 0; }

uint32_t ScenicCtfHlcppTest::DisplayMaxLayerCount() const { return 0; }

bool ScenicCtfHlcppTest::DisplayComposition() const { return true; }

}  // namespace integration_tests
