// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/usb-virtual-bus/tests/host-banjo.h"

#include <lib/driver/component/cpp/driver_export.h>

#include <usb/request-cpp.h>

namespace virtualbus {

void BanjoDevice::PrepareStop(fdf::PrepareStopCompleter completer) {
  usb_client_.CancelAll(bulk_out_addr_);
  usb_client_.CancelAll(bulk_in_addr_);
  completer(zx::ok());
}

void BanjoDevice::QueueOut(std::vector<uint8_t> data) {
  std::optional<usb::Request<>> req;
  zx_status_t status = usb::Request<>::Alloc(&req, data.size(), bulk_out_addr_, parent_req_size_);
  if (status != ZX_OK) {
    out_completer_->Reply(zx::error(status));
    out_completer_.reset();
    return;
  }
  size_t copy_result = req->CopyTo(data.data(), data.size(), 0);
  ZX_ASSERT(copy_result == data.size());

  usb_request_complete_callback_t complete = {
      .callback =
          [](void* ctx, usb_request_t* request) {
            usb::Request<> req(request, static_cast<BanjoDevice*>(ctx)->parent_req_size_);
            static_cast<BanjoDevice*>(ctx)->out_completer_->Reply(
                zx::ok(req.request()->response.actual));
            static_cast<BanjoDevice*>(ctx)->out_completer_.reset();
          },
      .ctx = this,
  };
  usb_client_.RequestQueue(req->take(), &complete);
}

void BanjoDevice::QueueIn(size_t size) {
  std::optional<usb::Request<>> req;
  zx_status_t status = usb::Request<>::Alloc(&req, size, bulk_in_addr_, parent_req_size_);
  if (status != ZX_OK) {
    in_completer_->Reply(zx::error(status));
    in_completer_.reset();
    return;
  }

  usb_request_complete_callback_t complete = {
      .callback =
          [](void* ctx, usb_request_t* request) {
            usb::Request<> req(request, static_cast<BanjoDevice*>(ctx)->parent_req_size_);

            std::vector<uint8_t> data(req.request()->response.actual);
            size_t actual = req.CopyFrom(data.data(), data.size(), 0);
            if (actual != req.request()->response.actual) {
              static_cast<BanjoDevice*>(ctx)->in_completer_->Reply(zx::error(ZX_ERR_BAD_STATE));
              static_cast<BanjoDevice*>(ctx)->in_completer_.reset();
              return;
            }

            static_cast<BanjoDevice*>(ctx)->in_completer_->Reply(zx::ok(std::move(data)));
            static_cast<BanjoDevice*>(ctx)->in_completer_.reset();
          },
      .ctx = this,
  };
  usb_client_.RequestQueue(req->take(), &complete);
}

zx::result<> BanjoDevice::Start() {
  zx::result result = Device::Start();
  if (result.is_error()) {
    FDF_LOG(ERROR, "Failed to start %s", result.status_string());
    return result.take_error();
  }

  parent_req_size_ = usb_client_.GetRequestSize();
  return zx::ok();
}

}  // namespace virtualbus

FUCHSIA_DRIVER_EXPORT(virtualbus::BanjoDevice);
