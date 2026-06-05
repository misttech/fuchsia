// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/lib/usb-endpoint/include/usb-endpoint/usb-endpoint-server.h"

#include <lib/fit/defer.h>
#include <lib/io-buffer/phys-iter.h>

#include <mutex>
#include <variant>

namespace usb {

namespace {

io_buffer::PhysIter phys_iter(uint64_t* phys_list, size_t phys_count, zx_off_t length,
                              size_t max_length) {
  static_assert(sizeof(phys_iter_sg_entry_t) == sizeof(sg_entry_t) &&
                offsetof(phys_iter_sg_entry_t, length) == offsetof(sg_entry_t, length) &&
                offsetof(phys_iter_sg_entry_t, offset) == offsetof(sg_entry_t, offset));
  phys_iter_buffer_t buf = {.phys = phys_list,
                            .phys_count = phys_count,
                            .length = length,
                            .vmo_offset = 0,
                            .sg_list = nullptr,
                            .sg_count = 0};
  return io_buffer::PhysIter(buf, max_length);
}

}  // namespace

zx::result<std::vector<io_buffer::PhysIter>> EndpointServer::get_iter(RequestVariant& req,
                                                                      size_t max_length) const {
  std::vector<io_buffer::PhysIter> iters;
  if (std::holds_alternative<usb::BorrowedRequest<void>>(req)) {
    iters.push_back(std::get<usb::BorrowedRequest<void>>(req).phys_iter(max_length));
  } else {
    const auto& fidl_request = std::get<usb::FidlRequest>(req);
    size_t i = 0;
    std::lock_guard<std::mutex> lock(lock_);
    for (const auto& d : *fidl_request->data()) {
      switch (d.buffer()->Which()) {
        case fuchsia_hardware_usb_request::Buffer::Tag::kVmoId:
          iters.push_back(phys_iter(registered_vmos_.at(d.buffer()->vmo_id().value()).phys_list,
                                    registered_vmos_.at(d.buffer()->vmo_id().value()).phys_count,
                                    *d.size(), max_length));
          break;
        case fuchsia_hardware_usb_request::Buffer::Tag::kData:
          iters.push_back(fidl_request.phys_iter(i, max_length));
          break;
        default:
          zxlogf(ERROR, "Not supported buffer type");
          return zx::error(ZX_ERR_NOT_SUPPORTED);
      }
      i++;
    }
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
      zxlogf(ERROR, "Error sending event: %s", status.error_value().status_string());
    }
  }

  // Unregister VMOs
  for (auto& [id, vmo] : registered_vmos) {
    zx_status_t status = zx_pmt_unpin(vmo.pmt);
    ZX_DEBUG_ASSERT(status == ZX_OK);
    free(vmo.phys_list);
  }

  if (info.is_user_initiated()) {
    return;
  }

  if (info.is_peer_closed()) {
    zxlogf(INFO, "Client disconnected");
  } else {
    zxlogf(ERROR, "Server error: %s", info.ToError().status_string());
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
        zxlogf(ERROR, "VMO ID %lu already registered", id);
        continue;
      }

      zx::vmo vmo;
      auto status = zx::vmo::create(size, 0, &vmo);
      if (status != ZX_OK) {
        zxlogf(ERROR, "Failed to pin registered VMO %d", status);
        continue;
      }

      // Pin VMO. Abusing usb_request_physmap
      usb_request_t req = {
          .vmo_handle = vmo.get(),
          .size = size,
          .offset = 0,
          .pmt = ZX_HANDLE_INVALID,
          .phys_list = nullptr,
          .phys_count = 0,
      };
      status = usb_request_physmap(&req, bti_.get());
      if (status != ZX_OK) {
        zxlogf(ERROR, "Failed to pin registered VMO %d", status);
        continue;
      }

      // Save
      vmos.emplace_back(
          std::move(fuchsia_hardware_usb_endpoint::VmoHandle().id(id).vmo(std::move(vmo))));
      registered_vmos_[id] = {
          .pmt = req.pmt, .phys_list = req.phys_list, .phys_count = req.phys_count, .size = size};
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
      zxlogf(ERROR, "Failed to unpin registered VMO %d", status);
      failed_vmo_ids.emplace_back(info.id);
      errors.emplace_back(status);
      continue;
    }
    free(info.phys_list);
  }
  completer.Reply({std::move(failed_vmo_ids), std::move(errors)});
}

void EndpointServer::RequestComplete(zx_status_t status, size_t actual, RequestVariant request,
                                     std::optional<zx::eventpair> wake_lease) {
  if (std::holds_alternative<usb::BorrowedRequest<void>>(request)) {
    std::get<usb::BorrowedRequest<void>>(request).Complete(status, actual);
    return;
  }
  auto& req = std::get<usb::FidlRequest>(request);

  auto defer_completion = *req->defer_completion();

  std::vector<fuchsia_hardware_usb_endpoint::Completion> completions;
  std::optional<fidl::ServerBindingRef<fuchsia_hardware_usb_endpoint::Endpoint>> binding;

  {
    std::lock_guard<std::mutex> lock(lock_);
    completions_.emplace_back(std::move(fuchsia_hardware_usb_endpoint::Completion()
                                            .request(req.take_request())
                                            .status(status)
                                            .transfer_size(actual)
                                            .wake_lease(std::move(wake_lease))));
    if (defer_completion && status == ZX_OK) {
      return;
    }

    if (binding_ref_) {
      completions.swap(completions_);
      binding = *binding_ref_;
    }
  }

  if (binding) {
    auto status = fidl::SendEvent(*binding)->OnCompletion(std::move(completions));
    if (status.is_error()) {
      zxlogf(ERROR, "Error sending event: %s", status.error_value().status_string());
    }
  }
}

}  // namespace usb
