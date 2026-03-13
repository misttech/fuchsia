// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_GRAPHICS_DISPLAY_DRIVERS_COORDINATOR_CLIENT_SET_H_
#define SRC_GRAPHICS_DISPLAY_DRIVERS_COORDINATOR_CLIENT_SET_H_

#include <fidl/fuchsia.hardware.display/cpp/wire.h>
#include <lib/inspect/cpp/vmo/types.h>
#include <lib/zx/result.h>
#include <zircon/time.h>

#include <list>
#include <memory>
#include <span>

#include "src/graphics/display/drivers/coordinator/client-id.h"
#include "src/graphics/display/drivers/coordinator/client-priority.h"
#include "src/graphics/display/lib/api-types/cpp/display-id.h"
#include "src/graphics/display/lib/api-types/cpp/driver-config-stamp.h"

namespace display_coordinator {

class Client;
class Controller;

// Manages all of a Display Coordinator's client connections.
//
// Instances are not thread-safe.
class ClientSet {
 public:
  // Creates an empty set.
  //
  // `root_node` will be populated with one sub-node per connected client.
  explicit ClientSet(inspect::Node root_node);

  ClientSet(const ClientSet&) = delete;
  ClientSet(ClientSet&&) = delete;
  ClientSet& operator=(const ClientSet&) = delete;
  ClientSet& operator=(ClientSet&&) = delete;

  ~ClientSet();

  // Dispatches the changes to all clients.
  void DispatchOnDisplaysChanged(std::span<const display::DisplayId> added_display_ids,
                                 std::span<const display::DisplayId> removed_display_ids);

  // Dispatches the VSync to the client that submitted the configuration.
  void DispatchOnDisplayVsync(display::DisplayId display_id, zx::time_monotonic timestamp,
                              display::DriverConfigStamp vsync_config_stamp,
                              ClientPriority client_priority);

  // Dispatches the event to all clients.
  void DispatchOnCaptureComplete();

  // May change the client that owns the displays.
  void SetVirtconMode(fuchsia_hardware_display::wire::VirtconMode virtcon_mode);

  // `controller` must be null and must outlive the ClientSet.
  zx::result<ClientId> ConnectClient(
      Controller* controller, ClientPriority client_priority,
      fidl::ServerEnd<fuchsia_hardware_display::Coordinator> coordinator_server_end,
      fidl::ClientEnd<fuchsia_hardware_display::CoordinatorListener>
          coordinator_listener_client_end);

  // Transmits the initial set of connected displays to a client.
  //
  // After this method completes, the client will receive an OnDisplaysChanged
  // event that describes all the currently connected displays.
  //
  // This method is a no-op if there is no client with the given `client_id`.
  // This simplifies handling clients who disconnect before receiving the
  // initial set of displays.
  void SendInitialState(ClientId client_id,
                        std::span<const display::DisplayId> current_display_ids);

  // `client` must point to a proxy associated with a client in this set.
  //
  // This method must be called at most once for a client.
  void OnClientDisconnected(Client* client);

  // Returns the priority of the client that applied the display configuration.
  //
  // Returns nullopt if the applied configuration does not belong to any of the
  // current clients. This happens if the client that applied the configuration
  // has disconnected.
  std::optional<ClientPriority> FindConfigStampSource(
      display::DriverConfigStamp driver_config_stamp);

  // Closes all the client connections.
  //
  // The ClientSet will be cleared asynchronously. Each client's FIDL
  // disconnection handler will remove the client from the set.
  void CloseAll();

  // Returns null if no client owns the displays.
  Client* GetClientOwningDisplays() const;

 private:
  void HandleClientOwnershipChanges();

  std::list<std::unique_ptr<Client>> clients_;

  ClientId next_client_id_ = ClientId(1);

  // The inspect node that lists all client connections.
  inspect::Node root_node_;

  // Pointers to instances owned by `clients_`.
  Client* client_owning_displays_ = nullptr;
  Client* virtcon_client_ = nullptr;
  Client* primary_client_ = nullptr;

  // True iff the corresponding client can dispatch FIDL events.
  bool virtcon_client_ready_ = false;
  bool primary_client_ready_ = false;

  fuchsia_hardware_display::wire::VirtconMode virtcon_mode_ =
      fuchsia_hardware_display::wire::VirtconMode::kFallback;
};

}  // namespace display_coordinator

#endif  // SRC_GRAPHICS_DISPLAY_DRIVERS_COORDINATOR_CLIENT_SET_H_
