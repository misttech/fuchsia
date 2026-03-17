// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/block/drivers/ramdisk/v2/ramdisk-controller.h"

#include <fidl/fuchsia.driver.framework/cpp/markers.h>
#include <fidl/fuchsia.driver.framework/cpp/wire_types.h>
#include <fidl/fuchsia.hardware.ramdisk/cpp/wire_messaging.h>
#include <fidl/fuchsia.io/cpp/wire.h>
#include <lib/async/cpp/wait.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/driver/component/cpp/driver_base.h>
#include <lib/driver/component/cpp/driver_export.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/fdf/cpp/dispatcher.h>
#include <lib/fidl/cpp/wire/arena.h>
#include <lib/fidl/cpp/wire/channel.h>
#include <lib/fidl/cpp/wire/status.h>
#include <lib/zircon-assert/zircon/assert.h>
#include <lib/zx/eventpair.h>
#include <lib/zx/result.h>
#include <lib/zx/vmo.h>
#include <zircon/system/public/zircon/errors.h>
#include <zircon/system/public/zircon/syscalls.h>
#include <zircon/system/public/zircon/syscalls/port.h>
#include <zircon/types.h>

#include <atomic>
#include <cstdint>
#include <cstring>
#include <memory>
#include <string>
#include <utility>

#include <safemath/checked_math.h>

#include "src/devices/block/drivers/ramdisk/v2/ramdisk.h"
#include "src/storage/lib/block_server/block_server.h"

namespace ramdisk_v2 {

namespace fio = fuchsia_io;

RamdiskController::RamdiskController(fdf::DriverStartArgs start_args,
                                     fdf::UnownedSynchronizedDispatcher driver_dispatcher)
    : DriverBase("ramctl-v2", std::move(start_args), std::move(driver_dispatcher)) {}

zx::result<> RamdiskController::Start() {
  fuchsia_hardware_ramdisk::Service::InstanceHandler handler(
      {.controller = bind_handler(dispatcher())});
  if (zx::result result =
          outgoing()->AddService<fuchsia_hardware_ramdisk::Service>(std::move(handler));
      result.is_error()) {
    return result;
  }
  inspector().Health().Ok();
  node_client_.Bind(std::move(node()));

  return zx::ok();
}

void RamdiskController::Create(CreateRequestView request, CreateCompleter::Sync& completer) {
  uint32_t block_size =
      request->has_block_size() ? request->block_size() : zx_system_get_page_size();
  if (block_size == 0) {
    completer.Reply(zx::error(ZX_ERR_INVALID_ARGS));
    return;
  }

  uint64_t block_count;
  zx::vmo vmo;
  if (request->has_vmo()) {
    vmo = std::move(request->vmo());
    if (request->has_block_count()) {
      block_count = request->block_count();

      if (!safemath::CheckMul<uint64_t>(block_size, block_count).IsValid()) {
        completer.Reply(zx::error(ZX_ERR_INVALID_ARGS));
        return;
      }
    } else {
      uint64_t vmo_size;
      if (zx_status_t status = vmo.get_size(&vmo_size); status != ZX_OK) {
        completer.Reply(zx::error(status));
        return;
      }
      block_count = vmo_size / block_size;
    }
  } else {
    if (!request->has_block_count()) {
      completer.Reply(zx::error(ZX_ERR_INVALID_ARGS));
      return;
    }
    block_count = request->block_count();

    uint64_t size;
    if (!safemath::CheckMul<uint64_t>(block_size, block_count).AssignIfValid(&size)) {
      completer.Reply(zx::error(ZX_ERR_INVALID_ARGS));
      return;
    }

    if (zx_status_t status = zx::vmo::create(size, 0, &vmo); status != ZX_OK) {
      completer.Reply(zx::error(status));
      return;
    }
  }
  component::OutgoingDirectory outgoing(dispatcher());

  fidl::ClientEnd<fio::Directory> client;
  zx::result server = fidl::CreateEndpoints(&client);
  if (server.is_error()) {
    completer.Reply(server.take_error());
    return;
  }
  if (zx::result result = outgoing.Serve(*std::move(server)); result.is_error()) {
    completer.Reply(result.take_error());
    return;
  }

  block_server::PartitionInfo partition_info = {
      .start_block = 0,
      .block_count = block_count,
      .block_size = block_size,
  };
  if (request->has_max_transfer_blocks()) {
    partition_info.max_transfer_size = request->max_transfer_blocks() * block_size;
  }
  if (request->has_device_flags()) {
    partition_info.device_flags = static_cast<uint32_t>(request->device_flags());
  }

  static std::atomic<int> counter = 0;

  int id = counter.fetch_add(1);
  if (request->has_type_guid())
    memcpy(partition_info.type_guid, request->type_guid().value.data(), 16);

  zx::eventpair endpoint0, endpoint1;
  if (zx_status_t status = zx::eventpair::create(0, &endpoint0, &endpoint1); status != ZX_OK) {
    completer.Reply(zx::error(status));
    return;
  }

  zx_handle_t handle = endpoint1.get();
  auto waiter = std::make_unique<async::WaitOnce>(handle, ZX_EVENTPAIR_PEER_CLOSED, 0);

  waiter->Begin(dispatcher(),
                [this, id, endpoint1 = std::move(endpoint1)](
                    async_dispatcher_t*, async::WaitOnce*, zx_status_t, const zx_packet_signal_t*) {
                  auto controller = ramdisk_controllers_.extract(id);
                  ZX_ASSERT(!controller.empty());
                  const fidl::OneWayStatus result = fidl::WireCall(controller.mapped())->Remove();
                  if (!result.ok()) {
                    FDF_LOGL(WARNING, logger(), "Failed to remove child ramdisk %d: %s", id,
                             result.status_string());
                  }
                  ZX_ASSERT(ramdisks_.erase(id) == 1);
                });

  fidl::Arena arena;
  const auto args = fuchsia_driver_framework::wire::NodeAddArgs::Builder(arena)
                        .name(arena, "ramdisk-" + std::to_string(id))
                        .Build();
  auto [controller_client_end, controller_server_end] =
      fidl::Endpoints<fuchsia_driver_framework::NodeController>::Create();
  auto [node_client_end, node_server_end] =
      fidl::Endpoints<fuchsia_driver_framework::Node>::Create();

  auto result =
      node_client_->AddChild(args, std::move(controller_server_end), std::move(node_server_end));
  if (!result.ok()) {
    FDF_LOGL(ERROR, logger(), "Failed to add child ramdisk: %s", result.status_string());
    completer.Reply(zx::error(result.status()));
    return;
  }

  if (zx::result ramdisk = Ramdisk::Create(this, dispatcher(), std::move(node_client_end),
                                           std::move(vmo), partition_info, std::move(outgoing), id,
                                           request->has_publish() ? request->publish() : false);
      ramdisk.is_error()) {
    completer.Reply(ramdisk.take_error());
  } else {
    ramdisks_.emplace(id, std::make_pair(*std::move(ramdisk), std::move(waiter)));
    ramdisk_controllers_.emplace(id, std::move(controller_client_end));
    completer.ReplySuccess(std::move(client), std::move(endpoint0));
  }
}

}  // namespace ramdisk_v2

FUCHSIA_DRIVER_EXPORT(ramdisk_v2::RamdiskController);
