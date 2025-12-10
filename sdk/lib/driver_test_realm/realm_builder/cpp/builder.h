// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_TEST_REALM_REALM_BUILDER_CPP_BUILDER_H_
#define LIB_DRIVER_TEST_REALM_REALM_BUILDER_CPP_BUILDER_H_

#include <fidl/fuchsia.component.test/cpp/fidl.h>
#include <fidl/fuchsia.driver.development/cpp/fidl.h>
#include <fidl/fuchsia.driver.test/cpp/fidl.h>
#include <lib/sys/component/cpp/testing/realm_builder.h>

namespace driver_test_realm {

struct Options {
  std::optional<bool> using_subpackage;
  std::optional<std::tuple<component_testing::Ref, std::vector<fuchsia_component_test::Capability>>>
      driver_offers;
  std::optional<std::vector<fuchsia_component_test::Capability>> driver_exposes;
  std::vector<std::tuple<fuchsia_component_test::Capability, component_testing::Ref>>
      extra_realm_capabilities;
};

class OptionsBuilder {
 public:
  OptionsBuilder& using_subpackage(bool using_subpackage);
  OptionsBuilder& driver_offers(component_testing::Ref provider,
                                const std::vector<fuchsia_component_test::Capability>& offers);
  OptionsBuilder& driver_exposes(const std::vector<fuchsia_component_test::Capability>& exposes);
  OptionsBuilder& add_extra_realm_capability(fuchsia_component_test::Capability capability,
                                             component_testing::Ref provider);

  Options Build() const { return options_; }

 private:
  Options options_;
};

void Setup(component_testing::RealmBuilder& realm_builder, async_dispatcher_t* dispatcher,
           Options options, fuchsia_driver_test::RealmArgs args);

zx::result<> WaitForBootup(component_testing::RealmRoot& realm_root);
zx::result<fuchsia_driver_development::NodeInfo> WaitForNode(
    component_testing::RealmRoot& realm_root, std::string_view moniker);

}  // namespace driver_test_realm

#endif  // LIB_DRIVER_TEST_REALM_REALM_BUILDER_CPP_BUILDER_H_
