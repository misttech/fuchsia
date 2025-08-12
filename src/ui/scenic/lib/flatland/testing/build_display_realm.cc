// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/flatland/testing/build_display_realm.h"

#include <fidl/fuchsia.hardware.display/cpp/fidl.h>
#include <fidl/fuchsia.sysmem2/cpp/fidl.h>
#include <lib/async/dispatcher.h>
#include <lib/sys/component/cpp/testing/realm_builder.h>

namespace flatland::testing {

component_testing::RealmRoot BuildDisplayRealm(async_dispatcher_t* dispatcher) {
  component_testing::RealmBuilder builder = component_testing::RealmBuilder::Create();
  static constexpr std::string_view kCoordinatorConnectorChildName =
      "display-coordinator-connector";
  builder.AddChild(std::string(kCoordinatorConnectorChildName),
                   "#meta/display-coordinator-connector.cm");

  // Route capabilities from the test to the child component.
  builder.AddRoute(component_testing::Route{
      .capabilities = {component_testing::Service{.name = "fuchsia.hardware.display.Service"},
                       component_testing::Protocol{.name = "fuchsia.tracing.provider.Registry"}},
      .source = component_testing::ParentRef(),
      .targets = {component_testing::ChildRef{.name = kCoordinatorConnectorChildName}},
  });

  // Route capabilities from the child component to the test.
  builder.AddRoute(component_testing::Route{
      .capabilities = {component_testing::Service{.name = "fuchsia.hardware.display.Service"}},
      .source = component_testing::ChildRef{.name = kCoordinatorConnectorChildName},
      .targets = {component_testing::ParentRef()},
  });
  return builder.Build(dispatcher);
}

component_testing::RealmRoot BuildFakeDisplayRealm(async_dispatcher_t* dispatcher) {
  component_testing::RealmBuilder builder = component_testing::RealmBuilder::Create();
  static constexpr std::string_view kCoordinatorConnectorChildName =
      "display-coordinator-connector";
  builder.AddChild(std::string(kCoordinatorConnectorChildName),
                   "#meta/display-coordinator-connector.cm");

  // Route capabilities from the test to the child component.
  builder.AddRoute(component_testing::Route{
      .capabilities = {component_testing::Protocol{.name = "fuchsia.sysmem2.Allocator"},
                       component_testing::Protocol{.name = "fuchsia.tracing.provider.Registry"}},
      .source = component_testing::ParentRef(),
      .targets = {component_testing::ChildRef{.name = kCoordinatorConnectorChildName}},
  });

  // Route capabilities from the child component to the test.
  builder.AddRoute(component_testing::Route{
      .capabilities = {component_testing::Service{.name = "fuchsia.hardware.display.Service"}},
      .source = component_testing::ChildRef{.name = kCoordinatorConnectorChildName},
      .targets = {component_testing::ParentRef()},
  });
  return builder.Build(dispatcher);
}

}  // namespace flatland::testing
