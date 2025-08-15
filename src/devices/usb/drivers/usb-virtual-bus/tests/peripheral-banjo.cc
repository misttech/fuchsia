// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/usb-virtual-bus/tests/peripheral-banjo.h"

#include <lib/driver/compat/cpp/compat.h>
#include <lib/driver/component/cpp/driver_export.h>

#include <usb/request-cpp.h>

namespace virtualbus {

zx::result<> BanjoTestFunction::Start() {
  auto result = TestFunction::Start();
  if (result.is_error()) {
    return result.take_error();
  }

  parent_req_size_ = function_.GetRequestSize();
  return zx::ok();
}

void BanjoTestFunction::QueueOut() {
  usb_request_complete_callback_t complete = {
      .callback =
          [](void* ctx, usb_request_t* req) {
            usb::Request<> request(req, static_cast<BanjoTestFunction*>(ctx)->parent_req_size_);
            std::vector<uint8_t> data(request.request()->response.actual);
            size_t actual = request.CopyFrom(data.data(), data.size(), 0);
            if (actual != data.size()) {
              static_cast<BanjoTestFunction*>(ctx)->expect_out_->Reply(zx::error(ZX_ERR_BAD_STATE));
              static_cast<BanjoTestFunction*>(ctx)->expect_out_.reset();
              return;
            }

            static_cast<BanjoTestFunction*>(ctx)->expect_out_->Reply(zx::ok(std::move(data)));
            static_cast<BanjoTestFunction*>(ctx)->expect_out_.reset();
          },
      .ctx = this,
  };
  std::optional<usb::Request<>> data_out_req;
  usb::Request<>::Alloc(&data_out_req, kMaxPacketSize, descriptor_.bulk_out.b_endpoint_address,
                        parent_req_size_);
  function_.RequestQueue(data_out_req->take(), &complete);
}

void BanjoTestFunction::QueueIn(std::vector<uint8_t> data) {
  usb_request_complete_callback_t complete = {
      .callback =
          [](void* ctx, usb_request_t* req) {
            usb::Request<> request(req, static_cast<BanjoTestFunction*>(ctx)->parent_req_size_);
            static_cast<BanjoTestFunction*>(ctx)->expect_in_->Reply(
                zx::ok(request.request()->response.actual));
            static_cast<BanjoTestFunction*>(ctx)->expect_in_.reset();
          },
      .ctx = this,
  };

  std::optional<usb::Request<>> data_in_req;
  usb::Request<>::Alloc(&data_in_req, data.size(), descriptor_.bulk_in.b_endpoint_address,
                        parent_req_size_);
  size_t actual = data_in_req->CopyTo(data.data(), data.size(), 0);
  if (actual != data.size()) {
    expect_in_->Reply(zx::error(ZX_ERR_BAD_STATE));
    expect_in_.reset();
    return;
  }
  function_.RequestQueue(data_in_req->take(), &complete);
}

}  // namespace virtualbus

FUCHSIA_DRIVER_EXPORT(virtualbus::BanjoTestFunction);
