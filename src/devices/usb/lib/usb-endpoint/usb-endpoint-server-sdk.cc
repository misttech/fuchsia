// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async/dispatcher.h>
#include <lib/dma-buffer/phys-iter.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/fit/defer.h>
#include <zircon/assert.h>
#include <zircon/status.h>

#include <memory>
#include <mutex>
#include <vector>

#include <usb/sdk/request-fidl.h>

#include "src/devices/usb/lib/usb-endpoint/include/usb-endpoint/sdk/usb-endpoint-server.h"

namespace usb {

static const size_t kPageSize = zx_system_get_page_size();

zx::result<std::vector<dma_buffer::PhysIter>> EndpointServer::get_iter(RequestVariant& req,
                                                                       size_t max_length) const {
  std::vector<dma_buffer::PhysIter> iters;
  const auto& fidl_request = std::get<usb::FidlRequest>(req);
  size_t i = 0;
  std::lock_guard<std::mutex> lock(lock_);
  for (const auto& d : *fidl_request->data()) {
    switch (d.buffer()->Which()) {
      case fuchsia_hardware_usb_request::Buffer::Tag::kVmoId:
        iters.push_back(
            dma_buffer::PhysIter{registered_vmos_.at(d.buffer()->vmo_id().value()).phys_list,
                                 registered_vmos_.at(d.buffer()->vmo_id().value()).phys_count, 0,
                                 *d.size(), max_length});
        break;
      case fuchsia_hardware_usb_request::Buffer::Tag::kData:
        iters.push_back(fidl_request.phys_iter(i, max_length));
        break;
      default:
        fdf::error("Not supported buffer type");
        return zx::error(ZX_ERR_NOT_SUPPORTED);
    }
    i++;
  }
  return zx::success(std::move(iters));
}

void EndpointServer::Connect(async_dispatcher_t* dispatcher,
                             fidl::ServerEnd<fuchsia_hardware_usb_endpoint::Endpoint> server_end) {
  std::lock_guard<std::mutex> lock(lock_);
  binding_ref_.emplace(fidl::BindServer(dispatcher, std::move(server_end), this,
                                        std::mem_fn(&EndpointServer::OnUnbound)));
}

void EndpointServer::OnUnbound(
    fidl::UnbindInfo info, fidl::ServerEnd<fuchsia_hardware_usb_endpoint::Endpoint> server_end) {
  std::vector<fuchsia_hardware_usb_endpoint::Completion> completions;
  std::map<uint64_t, RegisteredVmo> registered_vmos;
  {
    std::lock_guard<std::mutex> lock(lock_);
    completions = std::move(completions_);
    registered_vmos = std::move(registered_vmos_);
    binding_ref_.reset();
  }

  if (!completions.empty()) {
    // Return all already completed events.
    auto status = fidl::SendEvent(server_end)->OnCompletion(std::move(completions));
    if (status.is_error()) {
      fdf::error("Error sending event: {}", status.error_value().status_string());
    }
  }

  // Unregister VMOs
  for (auto& [id, vmo] : registered_vmos) {
    zx_status_t status = zx_pmt_unpin(vmo.pmt);
    ZX_DEBUG_ASSERT(status == ZX_OK);
    delete[] vmo.phys_list;
  }

  if (info.is_user_initiated()) {
    return;
  }

  if (info.is_peer_closed()) {
    fdf::info("Client disconnected");
  } else {
    fdf::error("Server error: {}", info.ToError().status_string());
  }
}

void EndpointServer::RegisterVmos(RegisterVmosRequest& request,
                                  RegisterVmosCompleter::Sync& completer) {
  std::vector<fuchsia_hardware_usb_endpoint::VmoHandle> vmos;
  {
    std::lock_guard<std::mutex> lock(lock_);
    for (const auto& info : request.vmo_ids()) {
      ZX_ASSERT(info.id());
      ZX_ASSERT(info.size());
      auto id = *info.id();
      auto size = *info.size();

      if (registered_vmos_.find(id) != registered_vmos_.end()) {
        fdf::error("VMO ID {} already registered", id);
        continue;
      }

      zx::vmo vmo;
      auto status = zx::vmo::create(size, 0, &vmo);
      if (status != ZX_OK) {
        fdf::error("Failed to pin registered VMO {}", zx_status_get_string(status));
        continue;
      }

      zx_handle_t pmt;
      size_t num_addrs = USB_ROUNDUP(size, kPageSize) / kPageSize;

      std::unique_ptr<zx_paddr_t[]> paddrs{new zx_paddr_t[num_addrs]};

      uint64_t vmo_size;
      vmo.get_size(&vmo_size);

      status = zx_bti_pin(bti_.get(), ZX_BTI_PERM_READ | ZX_BTI_PERM_WRITE, vmo.get(), 0, vmo_size,
                          paddrs.get(), num_addrs, &pmt);

      if (status != ZX_OK) {
        fdf::error("zx_bti_pin(): {}", zx_status_get_string(status));
        continue;
      }

      // Save
      vmos.emplace_back(
          std::move(fuchsia_hardware_usb_endpoint::VmoHandle().id(id).vmo(std::move(vmo))));
      registered_vmos_[id] = {
          .pmt = pmt, .phys_list = paddrs.release(), .phys_count = num_addrs, .size = size};
    }
  }

  completer.Reply({std::move(vmos)});
}

void EndpointServer::UnregisterVmos(UnregisterVmosRequest& request,
                                    UnregisterVmosCompleter::Sync& completer) {
  std::vector<zx_status_t> errors;
  std::vector<uint64_t> failed_vmo_ids;

  struct UnmapInfo {
    uint64_t id;
    zx_handle_t pmt;
    uint64_t* phys_list;
  };
  std::vector<UnmapInfo> vmos_to_unmap;

  {
    std::lock_guard<std::mutex> lock(lock_);
    for (const auto& id : request.vmo_ids()) {
      auto registered_vmo = registered_vmos_.extract(id);
      if (registered_vmo.empty()) {
        failed_vmo_ids.emplace_back(id);
        errors.emplace_back(ZX_ERR_NOT_FOUND);
        continue;
      }
      vmos_to_unmap.push_back({id, registered_vmo.mapped().pmt, registered_vmo.mapped().phys_list});
    }
  }

  for (const auto& info : vmos_to_unmap) {
    zx_status_t status = zx_pmt_unpin(info.pmt);
    if (status != ZX_OK) {
      fdf::error("Failed to unpin registered VMO {}", zx_status_get_string(status));
      failed_vmo_ids.emplace_back(info.id);
      errors.emplace_back(status);
      continue;
    }
    delete[] info.phys_list;
  }
  completer.Reply({std::move(failed_vmo_ids), std::move(errors)});
}

void EndpointServer::RequestComplete(zx_status_t status, size_t actual, RequestVariant request,
                                     bool send_now) {
  auto& req = std::get<usb::FidlRequest>(request);

  auto defer_completion = *req->defer_completion();

  std::vector<fuchsia_hardware_usb_endpoint::Completion> completions;
  std::optional<fidl::ServerBindingRef<fuchsia_hardware_usb_endpoint::Endpoint>> binding;

  {
    std::lock_guard<std::mutex> lock(lock_);
    completions_.emplace_back(std::move(fuchsia_hardware_usb_endpoint::Completion()
                                            .request(req.take_request())
                                            .status(status)
                                            .transfer_size(actual)));
    if ((defer_completion && status == ZX_OK) || !send_now || !binding_ref_) {
      return;
    }

    completions.swap(completions_);
    binding = *binding_ref_;
  }

  auto fidl_status = fidl::SendEvent(*binding)->OnCompletion(std::move(completions));
  if (fidl_status.is_error()) {
    fdf::error("Error sending event: {}", fidl_status.error_value().status_string());
  }
}

void EndpointServer::SendCompletions() {
  std::vector<fuchsia_hardware_usb_endpoint::Completion> completions;
  std::optional<fidl::ServerBindingRef<fuchsia_hardware_usb_endpoint::Endpoint>> binding;
  {
    std::lock_guard lock(lock_);
    if (!binding_ref_ || completions_.empty()) {
      return;
    }
    completions.swap(completions_);
    binding = *binding_ref_;
  }

  auto status = fidl::SendEvent(*binding)->OnCompletion(std::move(completions));
  if (status.is_error()) {
    fdf::error("Error sending event: {}", status.error_value().status_string());
  }
}

}  // namespace usb
