// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/drivers/coordinator/client-set.h"

#include <fidl/fuchsia.hardware.display/cpp/wire.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/inspect/cpp/vmo/types.h>
#include <lib/zx/result.h>
#include <lib/zx/time.h>
#include <zircon/status.h>
#include <zircon/time.h>

#include <memory>
#include <span>

#include <fbl/string_printf.h>

#include "src/graphics/display/drivers/coordinator/client-priority.h"
#include "src/graphics/display/drivers/coordinator/client.h"
#include "src/graphics/display/drivers/coordinator/controller.h"
#include "src/graphics/display/lib/api-types/cpp/display-id.h"
#include "src/graphics/display/lib/api-types/cpp/driver-config-stamp.h"

namespace display_coordinator {

ClientSet::ClientSet(inspect::Node root_node) : root_node_(std::move(root_node)) {}

ClientSet::~ClientSet() = default;

void ClientSet::DispatchOnDisplaysChanged(std::span<const display::DisplayId> added_display_ids,
                                          std::span<const display::DisplayId> removed_display_ids) {
  if (virtcon_client_ready_) {
    ZX_DEBUG_ASSERT(virtcon_client_ != nullptr);
    virtcon_client_->OnDisplaysChanged(added_display_ids, removed_display_ids);
  }
  if (primary_client_ready_) {
    ZX_DEBUG_ASSERT(primary_client_ != nullptr);
    primary_client_->OnDisplaysChanged(added_display_ids, removed_display_ids);
  }
}

void ClientSet::DispatchOnDisplayVsync(display::DisplayId display_id, zx::time_monotonic timestamp,
                                       display::DriverConfigStamp vsync_config_stamp,
                                       ClientPriority client_priority) {
  ZX_DEBUG_ASSERT(display_id != display::kInvalidDisplayId);
  ZX_DEBUG_ASSERT(vsync_config_stamp != display::kInvalidDriverConfigStamp);

  zx_instant_mono_t fidl_timestamp = timestamp.get();
  switch (client_priority) {
    case ClientPriority::kPrimary:
      primary_client_->OnDisplayVsync(display_id, fidl_timestamp, vsync_config_stamp);
      break;
    case ClientPriority::kVirtcon:
      virtcon_client_->OnDisplayVsync(display_id, fidl_timestamp, vsync_config_stamp);
      break;
  }
}

void ClientSet::DispatchOnCaptureComplete() {
  if (virtcon_client_ready_) {
    ZX_DEBUG_ASSERT(virtcon_client_ != nullptr);
    virtcon_client_->OnCaptureComplete();
  }
  if (primary_client_ready_) {
    ZX_DEBUG_ASSERT(primary_client_ != nullptr);
    primary_client_->OnCaptureComplete();
  }
}

void ClientSet::SetVirtconMode(fuchsia_hardware_display::wire::VirtconMode virtcon_mode) {
  virtcon_mode_ = virtcon_mode;
  HandleClientOwnershipChanges();
}

namespace {

void PrintChannelKoids(ClientPriority client_priority, const zx::channel& channel) {
  zx_info_handle_basic_t info{};
  size_t actual, avail;
  zx_status_t status = channel.get_info(ZX_INFO_HANDLE_BASIC, &info, sizeof(info), &actual, &avail);
  if (status != ZX_OK || info.type != ZX_OBJ_TYPE_CHANNEL) {
    fdf::error("Failed to get koids for handle type {}: {}", info.type, status);
    return;
  }
  ZX_DEBUG_ASSERT(actual == avail);
  fdf::info("Client connecting at priority {} - FIDL client end: 0x{:x} server end: 0x{:x}",
            DebugStringFromClientPriority(client_priority), info.related_koid, info.koid);
}

}  // namespace

zx::result<ClientId> ClientSet::ConnectClient(
    Controller* controller, ClientPriority client_priority,
    fidl::ServerEnd<fuchsia_hardware_display::Coordinator> coordinator_server_end,
    fidl::ClientEnd<fuchsia_hardware_display::CoordinatorListener>
        coordinator_listener_client_end) {
  ZX_DEBUG_ASSERT(controller != nullptr);
  ZX_DEBUG_ASSERT(coordinator_server_end.is_valid());
  ZX_DEBUG_ASSERT(coordinator_listener_client_end.is_valid());

  PrintChannelKoids(client_priority, coordinator_server_end.channel());

  if ((client_priority == ClientPriority::kVirtcon && virtcon_client_ != nullptr) ||
      (client_priority == ClientPriority::kPrimary && primary_client_ != nullptr)) {
    fdf::debug("Client already bound at priority {}",
               DebugStringFromClientPriority(client_priority));
    return zx::error(ZX_ERR_ALREADY_BOUND);
  }

  ClientId client_id = next_client_id_;
  ++next_client_id_;
  auto client = std::make_unique<Client>(controller, client_priority, client_id);

  inspect::Node client_inspect_node =
      root_node_.CreateChild(fbl::StringPrintf("client-%" PRIu64, client_id.value()));
  zx_status_t status =
      client->Bind(std::move(client_inspect_node), std::move(coordinator_server_end),
                   std::move(coordinator_listener_client_end));
  if (status != ZX_OK) {
    fdf::warn("Failed to initialize client: {}", status);
    return zx::error(status);
  }

  Client* client_ptr = client.get();
  clients_.push_back(std::move(client));

  fdf::info("Client connected at priority {} with ID {}",
            DebugStringFromClientPriority(client_priority), client_ptr->id().value());

  switch (client_priority) {
    case ClientPriority::kVirtcon:
      ZX_DEBUG_ASSERT(virtcon_client_ == nullptr);
      ZX_DEBUG_ASSERT(!virtcon_client_ready_);
      virtcon_client_ = client_ptr;
      break;
    case ClientPriority::kPrimary:
      ZX_DEBUG_ASSERT(primary_client_ == nullptr);
      ZX_DEBUG_ASSERT(!primary_client_ready_);
      primary_client_ = client_ptr;
  }
  HandleClientOwnershipChanges();

  return zx::ok(client_id);
}

void ClientSet::SendInitialState(ClientId client_id,
                                 std::span<const display::DisplayId> current_display_ids) {
  Client* client;
  if (virtcon_client_ != nullptr && virtcon_client_->id() == client_id) {
    client = virtcon_client_;
  } else if (primary_client_ != nullptr && primary_client_->id() == client_id) {
    client = primary_client_;
  } else {
    return;
  }

  std::span<const display::DisplayId> removed_display_ids = {};
  client->OnDisplaysChanged(current_display_ids, removed_display_ids);

  if (virtcon_client_ == client) {
    ZX_DEBUG_ASSERT(!virtcon_client_ready_);
    virtcon_client_ready_ = true;
  } else {
    ZX_DEBUG_ASSERT(primary_client_ == client);
    ZX_DEBUG_ASSERT(!primary_client_ready_);
    primary_client_ready_ = true;
  }
}

std::optional<ClientPriority> ClientSet::FindConfigStampSource(
    display::DriverConfigStamp driver_config_stamp) {
  for (const std::unique_ptr<Client>& client : clients_) {
    const std::list<Client::ConfigStampPair>& pending_stamps =
        client->pending_displayed_config_stamps();
    auto pending_stamps_it =
        std::ranges::find_if(pending_stamps, [&](const Client::ConfigStampPair& pending_stamp) {
          return pending_stamp.driver_stamp >= driver_config_stamp;
        });
    if (pending_stamps_it == pending_stamps.end()) {
      continue;
    }
    if (pending_stamps_it->driver_stamp == driver_config_stamp) {
      return std::make_optional(client->priority());
    }
  }
  return std::nullopt;
}

void ClientSet::OnClientDisconnected(Client* client) {
  ZX_DEBUG_ASSERT(client != nullptr);

  fdf::info("Client at priority {} with ID {} disconnected",
            DebugStringFromClientPriority(client->priority()), client->id().value());

  if (client == virtcon_client_) {
    virtcon_client_ = nullptr;
    virtcon_mode_ = fuchsia_hardware_display::wire::VirtconMode::kFallback;
    virtcon_client_ready_ = false;
  } else if (client == primary_client_) {
    primary_client_ = nullptr;
    primary_client_ready_ = false;
  } else {
    ZX_DEBUG_ASSERT_MSG(false, "Dead client is neither Virtcon nor Primary");
  }
  if (client == client_owning_displays_) {
    client_owning_displays_ = nullptr;
  }

  clients_.remove_if(
      [client](std::unique_ptr<Client>& list_client) { return list_client.get() == client; });

  HandleClientOwnershipChanges();
}

void ClientSet::HandleClientOwnershipChanges() {
  Client* new_client_owning_displays;
  if (virtcon_mode_ == fuchsia_hardware_display::wire::VirtconMode::kForced ||
      (virtcon_mode_ == fuchsia_hardware_display::wire::VirtconMode::kFallback &&
       primary_client_ == nullptr)) {
    new_client_owning_displays = virtcon_client_;
  } else {
    new_client_owning_displays = primary_client_;
  }

  if (new_client_owning_displays != client_owning_displays_) {
    if (client_owning_displays_) {
      client_owning_displays_->SetOwnership(false);
    }
    if (new_client_owning_displays) {
      new_client_owning_displays->SetOwnership(true);
    }
    client_owning_displays_ = new_client_owning_displays;
  }
}

void ClientSet::CloseAll() {
  for (const std::unique_ptr<Client>& client : clients_) {
    client->TearDown(ZX_ERR_CONNECTION_ABORTED);
  }

  // TODO(costan): Find a better workaround.
  //
  // We do not clear `clients_` here. `Client::TearDown()` will trigger
  // `OnClientDead()` which will call `OnClientDisconnected()` to remove the
  // client from the list.
  client_owning_displays_ = nullptr;
  primary_client_ = nullptr;
  primary_client_ready_ = false;
  virtcon_client_ = nullptr;
  virtcon_client_ready_ = false;
}

Client* ClientSet::GetClientOwningDisplays() const { return client_owning_displays_; }

}  // namespace display_coordinator
