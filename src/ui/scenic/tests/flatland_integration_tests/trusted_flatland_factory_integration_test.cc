// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fuchsia/ui/composition/cpp/fidl.h>
#include <lib/syslog/cpp/macros.h>

#include <zxtest/zxtest.h>

#include "src/ui/scenic/tests/utils/blocking_present.h"
#include "src/ui/scenic/tests/utils/scenic_ctf_test_base.h"

namespace integration_tests {

namespace {

struct TestParameters {
  std::string test_name;
  fuchsia::ui::composition::TrustedFlatlandConfig config;

  // Constructor for gtest value-parameterized tests.
  TestParameters(std::string name, fuchsia::ui::composition::TrustedFlatlandConfig cfg)
      : test_name(std::move(name)), config(std::move(cfg)) {}

  // Copy constructor for gtest value-parameterized tests.
  // A user-defined copy constructor is needed because TrustedFlatlandConfig is not copyable.
  TestParameters(const TestParameters& other)
      : test_name(other.test_name), config(fidl::Clone(other.config)) {}
};

fuchsia::ui::composition::TrustedFlatlandConfig ScheduleAsapConfig() {
  fuchsia::ui::composition::TrustedFlatlandConfig config;
  config.set_schedule_asap(true);
  return config;
}

fuchsia::ui::composition::TrustedFlatlandConfig DirectAcquireFencesConfig() {
  fuchsia::ui::composition::TrustedFlatlandConfig config;
  config.set_pass_acquire_fences(true);
  return config;
}

fuchsia::ui::composition::TrustedFlatlandConfig AllOptionsConfig() {
  fuchsia::ui::composition::TrustedFlatlandConfig config;
  config.set_schedule_asap(true);
  config.set_pass_acquire_fences(true);
  return config;
}

}  // namespace

// Test fixture that sets up an environment with a Scenic we can connect to.
class TrustedFlatlandFactoryIntegrationTest : public ScenicCtfTest,
                                              public zxtest::WithParamInterface<TestParameters> {
 protected:
  void SetUp() override {
    ScenicCtfTest::SetUp();
    factory_ = ConnectSyncIntoRealm<fuchsia::ui::composition::TrustedFlatlandFactory>();
  }

  fuchsia::ui::composition::TrustedFlatlandFactorySyncPtr factory_ = nullptr;
};

TEST_P(TrustedFlatlandFactoryIntegrationTest, CreateFlatland) {
  auto param = GetParam();
  fuchsia::ui::composition::FlatlandPtr flatland;

  fuchsia::ui::composition::TrustedFlatlandFactory_CreateFlatland_Result result;
  ASSERT_OK(factory_->CreateFlatland(flatland.NewRequest(), std::move(param.config), &result));
  ASSERT_FALSE(result.is_err());

  // Do a trivial operation to ensure the Flatland channel is usable.
  flatland->Clear();
  BlockingPresent(this, flatland);
}

INSTANTIATE_TEST_SUITE_P(
    TrustedFlatlandFactoryIntegrationTestWithParams, TrustedFlatlandFactoryIntegrationTest,
    zxtest::Values(TestParameters("DefaultConfig", {}),
                   TestParameters("ScheduleAsap", ScheduleAsapConfig()),
                   TestParameters("DirectAcquireFences", DirectAcquireFencesConfig()),
                   TestParameters("AllOptions", AllOptionsConfig())),
    [](const zxtest::TestParamInfo<TestParameters>& info) { return info.param.test_name; });

}  // namespace integration_tests
