// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/usb-virtual-bus/tests/virtual-bus-tester-function/peripheral-fidl.h"

#include <fidl/fuchsia.hardware.usb.function/cpp/fidl.h>
#include <lib/driver/compat/cpp/compat.h>
#include <lib/driver/component/cpp/driver_export.h>

namespace virtualbus {

namespace fendpoint = fuchsia_hardware_usb_endpoint;

zx::result<> FidlTestFunction::Start() {
  zx::result result = TestFunction::Start();
  if (result.is_error()) {
    FDF_LOG(ERROR, "Failed to start %s", result.status_string());
    return result.take_error();
  }

  zx::result dispatcher =
      fdf::SynchronizedDispatcher::Create({}, "ep-dispatcher", [](fdf_dispatcher_t*) {}, "");
  if (dispatcher.is_error()) {
    FDF_LOG(ERROR, "Failed to create dispatcher %s", dispatcher.status_string());
    return dispatcher.take_error();
  }
  dispatcher_ = std::move(*dispatcher);

  auto client = incoming()->Connect<fuchsia_hardware_usb_function::UsbFunctionService::Device>();
  if (client.is_error()) {
    FDF_LOG(ERROR, "Failed to connect fidl protocol");
    return client.take_error();
  }

  zx_status_t status = bulk_out_ep_.Init(descriptor_.bulk_out.b_endpoint_address, *client,
                                         dispatcher_.async_dispatcher());
  if (status != ZX_OK) {
    FDF_LOG(ERROR, "Failed to init UsbEndpoint %s", zx_status_get_string(status));
    return zx::error(status);
  }

  status = bulk_in_ep_.Init(descriptor_.bulk_in.b_endpoint_address, *client,
                            dispatcher_.async_dispatcher());
  if (status != ZX_OK) {
    FDF_LOG(ERROR, "Failed to init UsbEndpoint %s", zx_status_get_string(status));
    return zx::error(status);
  }

  return zx::ok();
}

void FidlTestFunction::QueueOut() {
  std::vector<fuchsia_hardware_usb_request::Request> requests;
  requests.emplace_back(usb::FidlRequest(usb::EndpointType::BULK)
                            .add_data(std::vector<uint8_t>(kMaxPacketSize), kMaxPacketSize)
                            .take_request());
  auto result = bulk_out_ep_->QueueRequests(std::move(requests));
  if (result.is_error()) {
    FDF_LOG(ERROR, "Failed to QueueRequests %s", result.error_value().FormatDescription().c_str());
    expect_out_->Reply(zx::error(ZX_ERR_INTERNAL));
    expect_out_.reset();
    return;
  }
}

void FidlTestFunction::QueueIn(std::vector<uint8_t> data) {
  size_t size = data.size();
  std::vector<fuchsia_hardware_usb_request::Request> requests;
  requests.emplace_back(
      usb::FidlRequest(usb::EndpointType::BULK).add_data(std::move(data), size).take_request());
  auto result = bulk_in_ep_->QueueRequests(std::move(requests));
  if (result.is_error()) {
    FDF_LOG(ERROR, "Failed to QueueRequests %s", result.error_value().FormatDescription().c_str());
    expect_in_->Reply(zx::error(ZX_ERR_INTERNAL));
    expect_in_.reset();
    return;
  }
}

void FidlTestFunction::OutComplete(fendpoint::Completion completion) {
  if (!expect_out_) {
    return;
  }

  if (*completion.status() != ZX_OK) {
    expect_out_->Reply(zx::error(*completion.status()));
    expect_out_.reset();
    return;
  }

  auto req = usb::FidlRequest(std::move(completion.request().value()));
  std::vector<uint8_t> data = std::move((*req->data())[0].buffer()->data().value());
  data.resize(*completion.transfer_size());
  expect_out_->Reply(zx::ok(std::move(data)));
  expect_out_.reset();
}

void FidlTestFunction::InComplete(fendpoint::Completion completion) {
  if (!expect_in_) {
    return;
  }

  *completion.status() == ZX_OK ? expect_in_->Reply(zx::ok(*completion.transfer_size()))
                                : expect_in_->Reply(zx::error(*completion.status()));
  expect_in_.reset();
}

}  // namespace virtualbus

FUCHSIA_DRIVER_EXPORT(virtualbus::FidlTestFunction);
