// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/drivers/coordinator/client-proxy.h"

#include <fidl/fuchsia.hardware.display/cpp/wire.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/fit/function.h>
#include <lib/inspect/cpp/vmo/types.h>
#include <lib/sync/completion.h>
#include <lib/zx/result.h>
#include <zircon/compiler.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <algorithm>
#include <span>

#include <fbl/alloc_checker.h>
#include <fbl/string_printf.h>

#include "src/graphics/display/drivers/coordinator/client-id.h"
#include "src/graphics/display/drivers/coordinator/client-priority.h"
#include "src/graphics/display/drivers/coordinator/client-vsync-queue.h"
#include "src/graphics/display/drivers/coordinator/client.h"
#include "src/graphics/display/drivers/coordinator/post-display-task.h"
#include "src/graphics/display/lib/api-types/cpp/config-stamp.h"
#include "src/graphics/display/lib/api-types/cpp/display-id.h"
#include "src/graphics/display/lib/api-types/cpp/driver-config-stamp.h"
#include "src/graphics/display/lib/api-types/cpp/vsync-ack-cookie.h"
#include "src/graphics/display/lib/driver-utils/post-task.h"

namespace display_coordinator {

void ClientProxy::SetOwnership(bool is_owner) {
  ZX_DEBUG_ASSERT(controller_.IsRunningOnDriverDispatcher());

  is_owner_property_.Set(is_owner);
  handler_.SetOwnership(is_owner);
}

void ClientProxy::OnDisplaysChanged(std::span<const display::DisplayId> added_display_ids,
                                    std::span<const display::DisplayId> removed_display_ids) {
  ZX_DEBUG_ASSERT(controller_.IsRunningOnDriverDispatcher());
  handler_.OnDisplaysChanged(added_display_ids, removed_display_ids);
}

void ClientProxy::ReapplySpecialConfigs() {
  ZX_DEBUG_ASSERT(controller_.IsRunningOnDriverDispatcher());

  zx::result<> result = controller_.engine_driver_client()->SetMinimumRgb(handler_.GetMinimumRgb());
  if (!result.is_ok()) {
    fdf::error("Failed to reapply minimum RGB value: {}", result);
  }
}

void ClientProxy::ReapplyConfig() {
  ZX_DEBUG_ASSERT(controller_.IsRunningOnDriverDispatcher());
  handler_.ReapplyConfig();
}

void ClientProxy::OnCaptureComplete() {
  ZX_DEBUG_ASSERT(controller_.IsRunningOnDriverDispatcher());

  if (enable_capture_) {
    handler_.CaptureCompleted();
  }
  enable_capture_ = false;
}

void ClientProxy::AcknowledgeVsync(display::VsyncAckCookie ack_cookie) {
  ZX_DEBUG_ASSERT(controller_.IsRunningOnDriverDispatcher());

  if (!vsync_queue_.Acknowledge(ack_cookie)) {
    fdf::error("Client passed incorrect VSync ack cookie: {}", ack_cookie.value());
  }
  DrainVsyncQueue();
}

void ClientProxy::OnDisplayVsync(display::DisplayId display_id, zx_instant_mono_t timestamp,
                                 display::DriverConfigStamp driver_config_stamp) {
  ZX_DEBUG_ASSERT(controller_.IsRunningOnDriverDispatcher());

  display::ConfigStamp client_stamp = {};
  auto it =
      std::find_if(pending_displayed_config_stamps_.begin(), pending_displayed_config_stamps_.end(),
                   [driver_config_stamp](const ConfigStampPair& stamp) {
                     return stamp.driver_stamp >= driver_config_stamp;
                   });

  if (it == pending_displayed_config_stamps_.end() || it->driver_stamp != driver_config_stamp) {
    client_stamp = display::kInvalidConfigStamp;
  } else {
    client_stamp = it->client_stamp;
    pending_displayed_config_stamps_.erase(pending_displayed_config_stamps_.begin(), it);
  }

  vsync_queue_.Push(ClientVsyncQueue::Message{.display_id = display_id,
                                              .timestamp = zx::time_monotonic(timestamp),
                                              .config_stamp = client_stamp});
  DrainVsyncQueue();
}

void ClientProxy::DrainVsyncQueue() {
  ZX_DEBUG_ASSERT(controller_.IsRunningOnDriverDispatcher());

  vsync_queue_.DrainUntilThrottled([&](const ClientVsyncQueue::Message& message,
                                       display::VsyncAckCookie ack_cookie) {
    handler_.NotifyVsync(message.display_id, message.timestamp, message.config_stamp, ack_cookie);
  });
}

void ClientProxy::EnableCapture(bool enable) {
  ZX_DEBUG_ASSERT(controller_.IsRunningOnDriverDispatcher());
  enable_capture_ = enable;
}

void ClientProxy::OnClientDead() {
  ZX_DEBUG_ASSERT(controller_.IsRunningOnDriverDispatcher());

  // Deletes `this`.
  controller_.OnClientDead(this);
}

void ClientProxy::UpdateConfigStampMapping(ConfigStampPair stamps) {
  ZX_DEBUG_ASSERT(controller_.IsRunningOnDriverDispatcher());
  ZX_DEBUG_ASSERT(pending_displayed_config_stamps_.empty() ||
                  pending_displayed_config_stamps_.back().driver_stamp < stamps.driver_stamp);

  pending_displayed_config_stamps_.push_back({
      .driver_stamp = stamps.driver_stamp,
      .client_stamp = stamps.client_stamp,
  });
}

void ClientProxy::TearDown() {
  ZX_DEBUG_ASSERT(controller_.IsRunningOnDriverDispatcher());
  handler_.TearDown(ZX_ERR_CONNECTION_ABORTED);
}

zx_status_t ClientProxy::Init(
    inspect::Node client_node,
    fidl::ServerEnd<fuchsia_hardware_display::Coordinator> coordinator_server_end,
    fidl::ClientEnd<fuchsia_hardware_display::CoordinatorListener>
        coordinator_listener_client_end) {
  ZX_DEBUG_ASSERT(controller_.IsRunningOnDriverDispatcher());

  node_ = std::move(client_node);
  node_.RecordString("priority", DebugStringFromClientPriority(handler_.priority()));
  is_owner_property_ = node_.CreateBool("is_owner", false);

  fidl::OnUnboundFn<Client> unbound_callback =
      [this](Client* client, fidl::UnbindInfo info,
             fidl::ServerEnd<fuchsia_hardware_display::Coordinator> ch) {
        ZX_DEBUG_ASSERT(controller_.IsRunningOnDriverDispatcher());

        // Make sure we `TearDown()` so that no further tasks are scheduled on
        // the driver dispatcher.
        client->TearDown(ZX_OK);

        // The client has died. Notify the proxy, which will free the Client
        // instance.
        OnClientDead();
      };

  handler_.Bind(std::move(coordinator_server_end), std::move(coordinator_listener_client_end),
                std::move(unbound_callback));
  return ZX_OK;
}

zx::result<> ClientProxy::InitForTesting(
    fidl::ServerEnd<fuchsia_hardware_display::Coordinator> coordinator_server_end,
    fidl::ClientEnd<fuchsia_hardware_display::CoordinatorListener>
        coordinator_listener_client_end) {
  ZX_DEBUG_ASSERT(controller_.IsRunningOnDriverDispatcher());

  // `ClientProxy` created by tests may not have a full-fledged display engine.
  // The production client teardown logic doesn't work here so we replace it with a no-op unbound
  // callback instead.
  fidl::OnUnboundFn<Client> unbound_callback =
      [](Client*, fidl::UnbindInfo, fidl::ServerEnd<fuchsia_hardware_display::Coordinator>) {};
  handler_.Bind(std::move(coordinator_server_end), std::move(coordinator_listener_client_end),
                std::move(unbound_callback));
  return zx::ok();
}

ClientProxy::ClientProxy(Controller* controller, ClientPriority client_priority, ClientId client_id)
    : controller_(*controller), handler_(&controller_, this, client_priority, client_id) {
  ZX_DEBUG_ASSERT(controller != nullptr);
}

ClientProxy::~ClientProxy() = default;

}  // namespace display_coordinator
