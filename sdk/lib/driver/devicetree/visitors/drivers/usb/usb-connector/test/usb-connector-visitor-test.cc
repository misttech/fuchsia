// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "../usb-connector-visitor.h"

#include <lib/driver/component/cpp/composite_node_spec.h>
#include <lib/driver/component/cpp/node_add_args.h>
#include <lib/driver/devicetree/testing/visitor-test-helper.h>
#include <lib/driver/devicetree/visitors/default/bind-property/bind-property.h>
#include <lib/driver/devicetree/visitors/registry.h>

#include <bind/fuchsia/cpp/bind.h>
#include <gtest/gtest.h>

namespace usb_connector_visitor_dt {

class UsbConnectorVisitorTester
    : public fdf_devicetree::testing::VisitorTestHelper<UsbConnectorVisitor> {
 public:
  explicit UsbConnectorVisitorTester(std::string_view dtb_path)
      : fdf_devicetree::testing::VisitorTestHelper<UsbConnectorVisitor>(
            dtb_path, "UsbConnectorVisitorTester") {}
};

// Verifies that a consumer referencing a single USB connector without explicit
// `usb-connector-output-name` or `usb-connector-names` properties receives default
// `"usb-connector"` bind rules and properties.
TEST(UsbConnectorVisitorTester, TestSingleConnectorWithDefaults) {
  fdf_devicetree::VisitorRegistry visitors;
  ASSERT_TRUE(
      visitors.RegisterVisitor(std::make_unique<fdf_devicetree::BindPropertyVisitor>()).is_ok());
  auto tester = std::make_unique<UsbConnectorVisitorTester>("/pkg/test-data/usb-connector.dtb");
  UsbConnectorVisitorTester* connector_tester = tester.get();
  ASSERT_TRUE(visitors.RegisterVisitor(std::move(tester)).is_ok());

  ASSERT_EQ(ZX_OK, connector_tester->manager()->Walk(visitors).status_value());
  ASSERT_TRUE(connector_tester->DoPublish().is_ok());

  auto specs = connector_tester->GetCompositeNodeSpecs("default-connector-device");
  ASSERT_EQ(specs.size(), 1u);
  const auto& parents = specs[0].parents2();
  ASSERT_EQ(parents->size(), 2u);

  // Parent 0 is pdev. Parent 1 defaults both output and parent name to "usb-connector".
  EXPECT_TRUE(fdf_devicetree::testing::CheckHasBindRules(
      {{
          fdf::MakeAcceptBindRule(bind_fuchsia::SERVICE, "fuchsia.hardware.usb.connector.Service"),
          fdf::MakeAcceptBindRule(bind_fuchsia::NAME, "usb-connector"),
      }},
      (*parents)[1].bind_rules(), false));

  EXPECT_TRUE(fdf_devicetree::testing::CheckHasProperties(
      {{
          fdf::MakeProperty2(bind_fuchsia::SERVICE, "fuchsia.hardware.usb.connector.Service"),
          fdf::MakeProperty2(bind_fuchsia::NAME, "usb-connector"),
      }},
      (*parents)[1].properties(), false));
}

// Verifies that a consumer referencing a provider with `usb-connector-output-name` and
// `usb-connector-names` binds using the provider's output name in its bind rules and the consumer's
// connector name in its properties.
TEST(UsbConnectorVisitorTester, TestNamedConnector) {
  fdf_devicetree::VisitorRegistry visitors;
  ASSERT_TRUE(
      visitors.RegisterVisitor(std::make_unique<fdf_devicetree::BindPropertyVisitor>()).is_ok());
  auto tester = std::make_unique<UsbConnectorVisitorTester>("/pkg/test-data/usb-connector.dtb");
  UsbConnectorVisitorTester* connector_tester = tester.get();
  ASSERT_TRUE(visitors.RegisterVisitor(std::move(tester)).is_ok());

  ASSERT_EQ(ZX_OK, connector_tester->manager()->Walk(visitors).status_value());
  ASSERT_TRUE(connector_tester->DoPublish().is_ok());

  auto specs = connector_tester->GetCompositeNodeSpecs("named-connector-device");
  ASSERT_EQ(specs.size(), 1u);
  const auto& parents = specs[0].parents2();
  ASSERT_EQ(parents->size(), 2u);

  // Parent 1's rule comes from the provider's `usb-connector-output-name`, its property from the
  // consumer's `usb-connector-names`.
  EXPECT_TRUE(fdf_devicetree::testing::CheckHasBindRules(
      {{
          fdf::MakeAcceptBindRule(bind_fuchsia::SERVICE, "fuchsia.hardware.usb.connector.Service"),
          fdf::MakeAcceptBindRule(bind_fuchsia::NAME, "typec0"),
      }},
      (*parents)[1].bind_rules(), false));

  EXPECT_TRUE(fdf_devicetree::testing::CheckHasProperties(
      {{
          fdf::MakeProperty2(bind_fuchsia::SERVICE, "fuchsia.hardware.usb.connector.Service"),
          fdf::MakeProperty2(bind_fuchsia::NAME, "custom-connector"),
      }},
      (*parents)[1].properties(), false));
}

// Verifies that a consumer referencing multiple USB connectors in `usb-connectors` and
// `usb-connector-names` receives a composite parent specification for each connector in order.
TEST(UsbConnectorVisitorTester, TestMultipleConnectors) {
  fdf_devicetree::VisitorRegistry visitors;
  ASSERT_TRUE(
      visitors.RegisterVisitor(std::make_unique<fdf_devicetree::BindPropertyVisitor>()).is_ok());
  auto tester = std::make_unique<UsbConnectorVisitorTester>("/pkg/test-data/usb-connector.dtb");
  UsbConnectorVisitorTester* connector_tester = tester.get();
  ASSERT_TRUE(visitors.RegisterVisitor(std::move(tester)).is_ok());

  ASSERT_EQ(ZX_OK, connector_tester->manager()->Walk(visitors).status_value());
  ASSERT_TRUE(connector_tester->DoPublish().is_ok());

  auto specs = connector_tester->GetCompositeNodeSpecs("multi-connector-device");
  ASSERT_EQ(specs.size(), 1u);
  const auto& parents = specs[0].parents2();
  ASSERT_EQ(parents->size(), 3u);

  const std::pair<const char*, const char*> kExpected[] = {
      {"typec0", "port0"},
      {"typec1", "port1"},
  };
  for (size_t i = 0; i < std::size(kExpected); i++) {
    const auto& [output_name, parent_name] = kExpected[i];
    EXPECT_TRUE(fdf_devicetree::testing::CheckHasBindRules(
        {{
            fdf::MakeAcceptBindRule(bind_fuchsia::SERVICE,
                                    "fuchsia.hardware.usb.connector.Service"),
            fdf::MakeAcceptBindRule(bind_fuchsia::NAME, output_name),
        }},
        (*parents)[i + 1].bind_rules(), false));

    EXPECT_TRUE(fdf_devicetree::testing::CheckHasProperties(
        {{
            fdf::MakeProperty2(bind_fuchsia::SERVICE, "fuchsia.hardware.usb.connector.Service"),
            fdf::MakeProperty2(bind_fuchsia::NAME, parent_name),
        }},
        (*parents)[i + 1].properties(), false));
  }
}

}  // namespace usb_connector_visitor_dt
