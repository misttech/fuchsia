// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fuchsia/testing/harness/cpp/fidl.h>
#include <fuchsia/ui/composition/cpp/fidl.h>
#include <fuchsia/ui/display/singleton/cpp/fidl.h>
#include <fuchsia/ui/test/context/cpp/fidl.h>
#include <lib/async/cpp/task.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/channel.h>

#include <zxtest/zxtest.h>

#include "src/ui/scenic/tests/utils/scenic_ctf_test_base.h"

namespace integration_tests {

namespace {

using fth_RealmProxySyncPtr = fuchsia::testing::harness::RealmProxySyncPtr;
using fuc_FlatlandDisplay = fuchsia::ui::composition::FlatlandDisplay;
using fuds_Metrics = fuchsia::ui::display::singleton::Metrics;
using fuds_Info = fuchsia::ui::display::singleton::Info;
using fuds_InfoSyncPtr = fuchsia::ui::display::singleton::InfoSyncPtr;
using futc_ScenicRealmFactorySyncPtr = fuchsia::ui::test::context::ScenicRealmFactorySyncPtr;

// Max timeout in failure cases.
// Set this as low as you can that still works across all test platforms.
constexpr zx::duration kTimeout = zx::min(5);

struct DisplayConfig {
  fuchsia::math::SizeU dimensions;
  uint32_t refresh_rate_millihertz;
};

class SingletonDisplayIntegrationTest : public zxtest::Test,
                                        public ui_testing::LoggingEventLoop,
                                        public zxtest::WithParamInterface<DisplayConfig> {
 public:
  SingletonDisplayIntegrationTest() = default;

  void SetUp() {
    zxtest::Test::SetUp();
    {
      context_ = sys::ComponentContext::Create();
      ASSERT_EQ(context_->svc()->Connect(realm_factory_.NewRequest()), ZX_OK);

      fuchsia::ui::test::context::ScenicRealmFactoryCreateRealmRequest req;
      fuchsia::ui::test::context::ScenicRealmFactory_CreateRealm_Result res;

      req.set_realm_server(realm_proxy_.NewRequest());
      req.set_display_rotation(0);
      req.set_renderer(fuchsia::ui::test::context::RendererType::NULL_);
      req.set_display_composition(true);
      if (GetDisplayDimensions().height != 0 && GetDisplayDimensions().width != 0) {
        req.set_display_dimensions(GetDisplayDimensions());
      }
      if (GetDisplayRefreshRateMillihertz() != 0) {
        req.set_display_refresh_rate_millihertz(GetDisplayRefreshRateMillihertz());
      }

      ASSERT_EQ(realm_factory_->CreateRealm(std::move(req), &res), ZX_OK);
    }

    // Post a "just in case" quit task, if the test hangs.
    async::PostDelayedTask(
        dispatcher(),
        [] { FX_LOGS(FATAL) << "\n\n>> Test did not complete in time, terminating.  <<\n\n"; },
        kTimeout);

    singleton_display_ = ConnectSyncIntoRealm<fuds_Info>();
  }

  fuchsia::math::SizeU GetDisplayDimensions() const { return GetParam().dimensions; }
  uint32_t GetDisplayRefreshRateMillihertz() const { return GetParam().refresh_rate_millihertz; }

  /// Connect to the FIDL protocol which served from the realm proxy use default served path if no
  /// name passed in.
  template <typename Interface>
  fidl::SynchronousInterfacePtr<Interface> ConnectSyncIntoRealm(
      const std::string& service_path = Interface::Name_) {
    fidl::SynchronousInterfacePtr<Interface> ptr;

    fuchsia::testing::harness::RealmProxy_ConnectToNamedProtocol_Result result;
    if (realm_proxy_->ConnectToNamedProtocol(service_path, ptr.NewRequest().TakeChannel(),
                                             &result) != ZX_OK) {
      std::cerr << "ConnectToNamedProtocol(" << service_path << ", " << Interface::Name_
                << ") failed." << std::endl;
      std::abort();
    }
    return std::move(ptr);
  }

 protected:
  fuds_InfoSyncPtr singleton_display_;
  futc_ScenicRealmFactorySyncPtr realm_factory_;
  fth_RealmProxySyncPtr realm_proxy_;
  std::unique_ptr<sys::ComponentContext> context_;
};

TEST_P(SingletonDisplayIntegrationTest, GetMetrics) {
  fuds_Metrics metrics;
  ASSERT_EQ(ZX_OK, singleton_display_->GetMetrics(&metrics));

  ASSERT_TRUE(metrics.has_extent_in_px());
  ASSERT_TRUE(metrics.has_extent_in_mm());
  ASSERT_TRUE(metrics.has_recommended_device_pixel_ratio());

  EXPECT_EQ(GetDisplayDimensions().width, metrics.extent_in_px().width);
  EXPECT_EQ(GetDisplayDimensions().height, metrics.extent_in_px().height);
  EXPECT_EQ(160, metrics.extent_in_mm().width);
  EXPECT_EQ(90, metrics.extent_in_mm().height);
  EXPECT_EQ(1.f, metrics.recommended_device_pixel_ratio().x);
  EXPECT_EQ(1.f, metrics.recommended_device_pixel_ratio().y);
  EXPECT_EQ(GetDisplayRefreshRateMillihertz(), metrics.maximum_refresh_rate_in_millihertz());
}

TEST_P(SingletonDisplayIntegrationTest, DevicePixelRatioChange) {
  auto flatland_display = ConnectSyncIntoRealm<fuc_FlatlandDisplay>();
  const float kDPRx = 1.25f;
  const float kDPRy = 1.25f;
  flatland_display->SetDevicePixelRatio({kDPRx, kDPRy});

  // FlatlandDisplay lives on a Flatland thread and SingletonDisplay lives on the main thread, so
  // the update may not be sequential.
  RunLoopUntil([this, kDPRx, kDPRy] {
    fuds_Metrics metrics;
    EXPECT_EQ(ZX_OK, singleton_display_->GetMetrics(&metrics));
    return metrics.has_recommended_device_pixel_ratio() &&
           kDPRx == metrics.recommended_device_pixel_ratio().x &&
           kDPRy == metrics.recommended_device_pixel_ratio().y;
  });
}

constexpr DisplayConfig kSherlockDisplayConfig = {
    .dimensions = {.width = 1280, .height = 800},
    .refresh_rate_millihertz = 60000,
};
constexpr DisplayConfig kAstroDisplayConfig = {
    .dimensions = {.width = 1024, .height = 600},
    .refresh_rate_millihertz = 60000,
};
constexpr DisplayConfig kAstroLowRefreshRateDisplayConfig = {
    .dimensions = {.width = 1024, .height = 600},
    .refresh_rate_millihertz = 30000,
};

INSTANTIATE_TEST_SUITE_P(Panel, SingletonDisplayIntegrationTest,
                         zxtest::Values(kAstroDisplayConfig, kSherlockDisplayConfig,
                                        kAstroLowRefreshRateDisplayConfig));

}  // namespace

}  // namespace integration_tests
