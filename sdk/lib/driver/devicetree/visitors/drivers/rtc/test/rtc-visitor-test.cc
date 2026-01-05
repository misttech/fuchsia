// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "../rtc-visitor.h"

#include <lib/driver/component/cpp/composite_node_spec.h>
#include <lib/driver/component/cpp/node_add_args.h>
#include <lib/driver/devicetree/testing/visitor-test-helper.h>
#include <lib/driver/devicetree/visitors/default/bind-property/bind-property.h>
#include <lib/driver/devicetree/visitors/registry.h>

#include <bind/fuchsia/cpp/bind.h>
#include <bind/fuchsia/hardware/hrtimer/cpp/bind.h>
#include <gtest/gtest.h>

namespace rtc_dt {

class RtcVisitorTester : public fdf_devicetree::testing::VisitorTestHelper<RtcVisitor> {
 public:
  RtcVisitorTester(std::string_view dtb_path)
      : fdf_devicetree::testing::VisitorTestHelper<RtcVisitor>(dtb_path, "RtcVisitorTester") {}
};

TEST(RtcVisitorTester, TestReferences) {
  fdf_devicetree::VisitorRegistry visitors;
  ASSERT_TRUE(
      visitors.RegisterVisitor(std::make_unique<fdf_devicetree::BindPropertyVisitor>()).is_ok());
  auto tester = std::make_unique<RtcVisitorTester>("/pkg/test-data/rtc.dtb");
  RtcVisitorTester* rtc_tester = tester.get();
  ASSERT_TRUE(visitors.RegisterVisitor(std::move(tester)).is_ok());

  ASSERT_EQ(ZX_OK, rtc_tester->manager()->Walk(visitors).status_value());
  ASSERT_TRUE(rtc_tester->DoPublish().is_ok());

  auto non_pbus_node_count =
      rtc_tester->env().SyncCall(&fdf_devicetree::testing::FakeEnvWrapper::non_pbus_node_size);
  ASSERT_EQ(non_pbus_node_count, 2u);

  auto pbus_node_count =
      rtc_tester->env().SyncCall(&fdf_devicetree::testing::FakeEnvWrapper::pbus_node_size);
  ASSERT_EQ(pbus_node_count, 0u);

  auto mgr_request =
      rtc_tester->env().SyncCall(&fdf_devicetree::testing::FakeEnvWrapper::mgr_requests_at, 0);
  EXPECT_EQ(mgr_request.name(), "test-device");
  const auto& parents = mgr_request.parents2();
  ASSERT_EQ(parents->size(), 2u);

  // Parent 0 - pdev, ignore
  // Parent 1 - RTC
  EXPECT_TRUE(fdf_devicetree::testing::CheckHasBindRules(
      {{
          fdf::MakeAcceptBindRule2(bind_fuchsia_hardware_hrtimer::SERVICE,
                                   bind_fuchsia_hardware_hrtimer::SERVICE_ZIRCONTRANSPORT),
      }},
      (*parents)[1].bind_rules(), false));

  EXPECT_TRUE(fdf_devicetree::testing::CheckHasProperties(
      {{
          fdf::MakeProperty2(bind_fuchsia_hardware_hrtimer::SERVICE,
                             bind_fuchsia_hardware_hrtimer::SERVICE_ZIRCONTRANSPORT),
      }},
      (*parents)[1].properties(), false));
}

}  // namespace rtc_dt
