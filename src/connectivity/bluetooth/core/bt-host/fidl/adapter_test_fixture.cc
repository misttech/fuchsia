// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "adapter_test_fixture.h"

namespace bthost::testing {

using bt::testing::FakeController;
using TestingBase = bt::testing::ControllerTest<bt::testing::FakeController>;

void AdapterTestFixture::SetUp() {
  FakeController::Settings settings;
  settings.ApplyDualModeDefaults();
  SetUp(settings);
}

void AdapterTestFixture::SetUp(FakeController::Settings settings,
                               pw::bluetooth::Controller::FeaturesBits features) {
  TestingBase::Initialize(features, /*initialize_transport=*/false);

  auto l2cap = std::make_unique<bt::l2cap::testing::FakeL2cap>(pw_dispatcher());
  l2cap_ = l2cap.get();
  gatt_ = std::make_unique<bt::gatt::testing::FakeLayer>(pw_dispatcher());
  bt::gap::Adapter::Config config = {
      .legacy_pairing_enabled = false,
  };
  adapter_ = bt::gap::Adapter::Create(pw_dispatcher(), transport()->GetWeakPtr(),
                                      gatt_->GetWeakPtr(), config, std::move(l2cap));

  test_device()->set_settings(settings);

  bool success = false;
  adapter_->Initialize([&](bool result) { success = result; }, [] {});
  RunLoopUntilIdle();
  ASSERT_TRUE(success);
  ASSERT_TRUE(adapter_->le());
  ASSERT_TRUE(adapter_->bredr());
}

void AdapterTestFixture::TearDown() {
  // Drain all scheduled tasks.
  RunLoopUntilIdle();

  // Cleanly shut down the stack.
  l2cap_ = nullptr;
  adapter_ = nullptr;
  RunLoopUntilIdle();

  gatt_ = nullptr;
}

}  // namespace bthost::testing
