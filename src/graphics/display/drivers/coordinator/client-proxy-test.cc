// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/drivers/coordinator/client-proxy.h"

#include <fidl/fuchsia.hardware.display/cpp/fidl.h>
#include <fidl/fuchsia.hardware.display/cpp/wire.h>
#include <fidl/fuchsia.hardware.display/cpp/wire_test_base.h>
#include <lib/driver/testing/cpp/driver_runtime.h>
#include <lib/driver/testing/cpp/scoped_global_logger.h>
#include <lib/fdf/cpp/dispatcher.h>
#include <lib/fdf/dispatcher.h>
#include <lib/fidl/cpp/wire/client_base.h>
#include <lib/fidl/cpp/wire/wire_messaging.h>
#include <lib/sync/cpp/completion.h>
#include <zircon/errors.h>

#include <array>
#include <memory>
#include <optional>
#include <utility>

#include <fbl/auto_lock.h>
#include <gtest/gtest.h>

#include "src/graphics/display/drivers/coordinator/client-id.h"
#include "src/graphics/display/drivers/coordinator/client-priority.h"
#include "src/graphics/display/drivers/coordinator/controller.h"
#include "src/graphics/display/drivers/coordinator/engine-driver-client-fidl.h"
#include "src/graphics/display/lib/api-types/cpp/config-stamp.h"
#include "src/graphics/display/lib/api-types/cpp/display-id.h"
#include "src/graphics/display/lib/api-types/cpp/driver-config-stamp.h"
#include "src/lib/testing/predicates/status.h"

namespace display_coordinator {

namespace {

class MockCoordinatorListener
    : public fidl::WireServer<fuchsia_hardware_display::CoordinatorListener> {
 public:
  MockCoordinatorListener() = default;
  ~MockCoordinatorListener() = default;

  void OnDisplaysChanged(OnDisplaysChangedRequestView request,
                         OnDisplaysChangedCompleter::Sync& completer) override {
    latest_added_display_infos_ = std::vector(request->added.begin(), request->added.end());
    latest_removed_display_ids_ = {};
    for (fuchsia_hardware_display_types::wire::DisplayId fidl_id : request->removed) {
      latest_removed_display_ids_.push_back(display::DisplayId(fidl_id));
    }
  }

  void OnVsync(OnVsyncRequestView request, OnVsyncCompleter::Sync& completer) override {
    latest_vsync_timestamp_ = zx::time_monotonic(request->timestamp);
    latest_applied_config_stamp_ = display::ConfigStamp(request->applied_config_stamp);
  }

  void OnClientOwnershipChange(OnClientOwnershipChangeRequestView request,
                               OnClientOwnershipChangeCompleter::Sync& completer) override {
    client_has_ownership_ = request->has_ownership;
  }

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_hardware_display::CoordinatorListener> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  std::vector<fuchsia_hardware_display::wire::Info> latest_added_display_infos() const {
    return latest_added_display_infos_;
  }
  std::vector<display::DisplayId> latest_removed_display_ids() const {
    return latest_removed_display_ids_;
  }
  bool client_has_ownership() const { return client_has_ownership_; }
  zx::time_monotonic latest_vsync_timestamp() const { return latest_vsync_timestamp_; }
  display::ConfigStamp latest_applied_config_stamp() const { return latest_applied_config_stamp_; }

 private:
  std::vector<fuchsia_hardware_display::wire::Info> latest_added_display_infos_;
  std::vector<display::DisplayId> latest_removed_display_ids_;
  bool client_has_ownership_ = false;
  zx::time_monotonic latest_vsync_timestamp_ = zx::time_monotonic::infinite_past();
  display::ConfigStamp latest_applied_config_stamp_ = display::kInvalidConfigStamp;
};

class ClientProxyTest : public ::testing::Test {
 public:
  void SetUp() override {
    auto [engine_client_end, engine_server_end] =
        fdf::Endpoints<fuchsia_hardware_display_engine::Engine>::Create();
    std::unique_ptr<EngineDriverClient> engine_driver_client =
        std::make_unique<EngineDriverClientFidl>(std::move(engine_client_end));

    auto [listener_client_end, listener_server_end] =
        fidl::Endpoints<fuchsia_hardware_display::CoordinatorListener>::Create();
    listener_server_binding_.emplace(fidl::BindServer(driver_dispatcher_->async_dispatcher(),
                                                      std::move(listener_server_end),
                                                      &mock_coordinator_listener));

    auto [coordinator_client_end, coordinator_server_end] =
        fidl::Endpoints<fuchsia_hardware_display::Coordinator>::Create();

    controller_.emplace(std::move(engine_driver_client), driver_dispatcher_->borrow(),
                        engine_listener_dispatcher_->borrow());

    client_proxy_.emplace(&controller_.value(), ClientPriority::kPrimary, ClientId(1),
                          /*on_client_disconnected=*/[] {});
    ASSERT_OK(client_proxy_->InitForTesting(std::move(coordinator_server_end),
                                            std::move(listener_client_end)));
  }

  void TearDown() override {
    client_proxy_->TearDown();

    driver_runtime_.ShutdownAllDispatchers(/*dut_initial_dispatcher=*/nullptr);
  }

 protected:
  fdf_testing::ScopedGlobalLogger logger_;
  fdf_testing::DriverRuntime driver_runtime_;

  fdf::UnownedSynchronizedDispatcher driver_dispatcher_ = driver_runtime_.GetForegroundDispatcher();
  fdf::UnownedSynchronizedDispatcher engine_listener_dispatcher_ =
      driver_runtime_.StartBackgroundDispatcher();

  MockCoordinatorListener mock_coordinator_listener;
  std::optional<fidl::ServerBindingRef<fuchsia_hardware_display::CoordinatorListener>>
      listener_server_binding_;

  std::optional<Controller> controller_;
  std::optional<ClientProxy> client_proxy_;
};

TEST_F(ClientProxyTest, ClientVSyncDelivery) {
  constexpr display::DriverConfigStamp kDriverStampValue(1);
  constexpr display::ConfigStamp kClientStampValue(2);

  fbl::AutoLock lock(controller_->mtx());
  client_proxy_->UpdateConfigStampMapping({
      .driver_stamp = kDriverStampValue,
      .client_stamp = kClientStampValue,
  });

  client_proxy_->OnDisplayVsync(display::kInvalidDisplayId, 0, kDriverStampValue);

  driver_runtime_.RunUntilIdle();
  EXPECT_EQ(mock_coordinator_listener.latest_applied_config_stamp(), kClientStampValue);
}

TEST_F(ClientProxyTest, ClientVSyncPeerClosed) {
  listener_server_binding_->Close(ZX_OK);

  fbl::AutoLock lock(controller_->mtx());
  client_proxy_->OnDisplayVsync(display::kInvalidDisplayId, 0, display::kInvalidDriverConfigStamp);
}

TEST_F(ClientProxyTest, ClientMustDrainUntilThrottledPendingStamps) {
  constexpr size_t kNumPendingStamps = 5;
  constexpr std::array<uint64_t, kNumPendingStamps> kDriverStampValues = {1u, 2u, 3u, 4u, 5u};
  constexpr std::array<uint64_t, kNumPendingStamps> kClientStampValues = {2u, 3u, 4u, 5u, 6u};

  fbl::AutoLock lock(controller_->mtx());
  for (size_t i = 0; i < kNumPendingStamps; i++) {
    client_proxy_->UpdateConfigStampMapping({
        .driver_stamp = display::DriverConfigStamp(kDriverStampValues[i]),
        .client_stamp = display::ConfigStamp(kClientStampValues[i]),
    });
  }

  client_proxy_->OnDisplayVsync(display::kInvalidDisplayId, 0,
                                display::DriverConfigStamp(kDriverStampValues.back()));

  EXPECT_EQ(client_proxy_->pending_applied_config_stamps().size(), 1u);
  EXPECT_EQ(client_proxy_->pending_applied_config_stamps().front().driver_stamp,
            display::DriverConfigStamp(kDriverStampValues.back()));
}

}  // namespace

}  // namespace display_coordinator
