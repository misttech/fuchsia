// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_TEST_REALM_REALM_BUILDER_CPP_BUILDER_H_
#define LIB_DRIVER_TEST_REALM_REALM_BUILDER_CPP_BUILDER_H_

#include <fidl/fuchsia.component.test/cpp/fidl.h>
#include <fidl/fuchsia.driver.test/cpp/fidl.h>
#include <lib/sys/component/cpp/testing/realm_builder.h>

namespace driver_test_realm {

struct Options {
  std::optional<component_testing::Ref> dtr_offers_provider;
  std::optional<component_testing::Ref> boot_items_to_tunnel;
  std::optional<component_testing::Ref> trace_provider;
};

class OptionsBuilder {
 public:
  OptionsBuilder& set_dtr_offers_provider(component_testing::Ref provider);
  OptionsBuilder& set_boot_items_to_tunnel(component_testing::Ref items);
  OptionsBuilder& set_trace_provider(component_testing::Ref provider);
  Options Build() const { return options_; }

 private:
  Options options_;
};

void Setup(component_testing::RealmBuilder& realm_builder, async_dispatcher_t* dispatcher,
           fuchsia_driver_test::RealmArgs args, Options options = {});

zx::result<> WaitForBootup(component_testing::RealmRoot& realm_root);

}  // namespace driver_test_realm

#endif  // LIB_DRIVER_TEST_REALM_REALM_BUILDER_CPP_BUILDER_H_
