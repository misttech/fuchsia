// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.display/cpp/fidl.h>
#include <lib/fidl/cpp/wire/channel.h>
#include <lib/fidl/cpp/wire/status.h>
#include <zircon/types.h>

#include <utility>

#include <gtest/gtest.h>

#include "src/graphics/display/drivers/fake/fake-display-device-config.h"
#include "src/graphics/display/lib/api-types/cpp/engine-info.h"
#include "src/graphics/display/testing/fake-coordinator-connector/service.h"
#include "src/lib/testing/loop_fixture/test_loop_fixture.h"

namespace {

class FakeDisplayCoordinatorConnectorTest : public gtest::TestLoopFixture {
 public:
  FakeDisplayCoordinatorConnectorTest() = default;
  ~FakeDisplayCoordinatorConnectorTest() override = default;

  void SetUp() override {
    TestLoopFixture::SetUp();

    constexpr fake_display::FakeDisplayDeviceConfig kFakeDisplayDeviceConfig = {
        .engine_info = display::EngineInfo({
            .max_layer_count = 1,
            .max_connected_display_count = 1,
            .is_capture_supported = true,
        }),
        .periodic_vsync = false,
    };
    coordinator_connector_ = std::make_unique<display::FakeDisplayCoordinatorConnector>(
        dispatcher(), kFakeDisplayDeviceConfig);

    auto [client_end, server_end] = fidl::Endpoints<fuchsia_hardware_display::Provider>::Create();

    fidl::BindServer(dispatcher(), std::move(server_end), coordinator_connector_.get());
    provider_client_ = fidl::Client(std::move(client_end), dispatcher());
  }

  void TearDown() override {
    RunLoopUntilIdle();
    coordinator_connector_.reset();
  }

 protected:
  fidl::Client<fuchsia_hardware_display::Provider> provider_client_;
  std::unique_ptr<display::FakeDisplayCoordinatorConnector> coordinator_connector_;
};

TEST_F(FakeDisplayCoordinatorConnectorTest, OpenPrimaryAndVirtconConnections) {
  std::optional<
      fidl::Result<fuchsia_hardware_display::Provider::OpenCoordinatorWithListenerForPrimary>>
      open_primary_result;
  std::optional<
      fidl::Result<fuchsia_hardware_display::Provider::OpenCoordinatorWithListenerForVirtcon>>
      open_virtcon_result;

  fidl::Endpoints<fuchsia_hardware_display::Coordinator> coordinator_primary =
      fidl::Endpoints<fuchsia_hardware_display::Coordinator>::Create();
  fidl::Endpoints<fuchsia_hardware_display::CoordinatorListener> primary_listener =
      fidl::Endpoints<fuchsia_hardware_display::CoordinatorListener>::Create();
  provider_client_
      ->OpenCoordinatorWithListenerForPrimary({{
          .coordinator = std::move(coordinator_primary.server),
          .coordinator_listener = std::move(primary_listener.client),
      }})
      .Then(
          [&](fidl::Result<
              fuchsia_hardware_display::Provider::OpenCoordinatorWithListenerForPrimary>& result) {
            open_primary_result.emplace(std::move(result));
          });

  fidl::Endpoints<fuchsia_hardware_display::Coordinator> coordinator_virtcon =
      fidl::Endpoints<fuchsia_hardware_display::Coordinator>::Create();
  fidl::Endpoints<fuchsia_hardware_display::CoordinatorListener> virtcon_listener =
      fidl::Endpoints<fuchsia_hardware_display::CoordinatorListener>::Create();
  provider_client_
      ->OpenCoordinatorWithListenerForVirtcon({{
          .coordinator = std::move(coordinator_virtcon.server),
          .coordinator_listener = std::move(virtcon_listener.client),
      }})
      .Then(
          [&](fidl::Result<
              fuchsia_hardware_display::Provider::OpenCoordinatorWithListenerForVirtcon>& result) {
            open_virtcon_result.emplace(std::move(result));
          });

  RunLoopUntilIdle();

  ASSERT_TRUE(open_primary_result.has_value());
  EXPECT_TRUE(open_primary_result->is_ok()) << open_primary_result->error_value();

  ASSERT_TRUE(open_virtcon_result.has_value());
  EXPECT_TRUE(open_virtcon_result->is_ok()) << open_virtcon_result->error_value();
}

}  // namespace
